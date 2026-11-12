use crate::{
    agent::{AgentConfig, Process, cursor_command},
    chat::ExecutionMode,
    error::ApiError,
};
use agent_client_protocol::{
    Agent, Client, Lines, RequestCancellationHandle, UntypedMessage, v1::ConnectionTo,
};
use futures::{StreamExt, sink};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{Mutex, mpsc, oneshot},
    task::JoinHandle,
};
use tokio_util::{
    codec::{FramedRead, LinesCodec},
    sync::CancellationToken,
};

#[derive(Debug)]
pub enum Event {
    Notification(Value),
    Finished(Result<Value, ApiError>),
    Exited(ApiError),
}

#[derive(Default)]
struct Permissions {
    session: String,
    server: String,
    tools: HashSet<String>,
    calls: HashMap<(String, String), String>,
}

pub struct Connection {
    peer: ConnectionTo<Agent>,
    events: mpsc::Receiver<(Event, usize)>,
    events_tx: mpsc::Sender<(Event, usize)>,
    queued_bytes: Arc<AtomicUsize>,
    stop: CancellationToken,
    worker: Option<JoinHandle<()>>,
    permissions: Arc<Mutex<Permissions>>,
    approval_cancelled: Arc<AtomicBool>,
    tool_activity: Arc<AtomicUsize>,
    timeout: Duration,
    pub capabilities: Value,
}

fn protocol_error(error: agent_client_protocol::Error) -> ApiError {
    ApiError::from_agent(&error.message)
}

struct PendingRequest(Option<RequestCancellationHandle>);

pub struct OrderedResponse {
    value: Value,
    applied: Option<oneshot::Sender<()>>,
}

impl OrderedResponse {
    pub fn value(&self) -> &Value {
        &self.value
    }

    pub fn apply(mut self) -> Value {
        if let Some(applied) = self.applied.take()
            && applied.send(()).is_err()
        {
            tracing::debug!("ACP response application owner already closed");
        }
        std::mem::take(&mut self.value)
    }
}

impl Drop for PendingRequest {
    fn drop(&mut self) {
        if let Some(request) = self.0.take()
            && let Err(error) = request.cancel()
        {
            tracing::debug!(%error, "ACP request owner already closed");
        }
    }
}

impl Connection {
    pub async fn open(
        config: &AgentConfig,
        workspace: &Path,
        model: &str,
    ) -> Result<Self, ApiError> {
        let mut command = cursor_command(&config.binary)?;
        command
            .arg("--model")
            .arg(model)
            .arg("--workspace")
            .arg(workspace);
        if config.trusted_workspace.as_deref() == Some(workspace) {
            command.arg("--trust");
        }
        command
            .arg("acp")
            .current_dir(workspace)
            .env_remove("CURSOR_API_KEY")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut process = Process::spawn(&mut command)?;
        let stdin = process
            .child
            .stdin
            .take()
            .ok_or_else(|| ApiError::upstream("Missing ACP stdin"))?;
        let stdout = process
            .child
            .stdout
            .take()
            .ok_or_else(|| ApiError::upstream("Missing ACP stdout"))?;
        let mut stderr = process
            .child
            .stderr
            .take()
            .ok_or_else(|| ApiError::upstream("Missing ACP stderr"))?;
        let tail = Arc::new(Mutex::new(Vec::new()));
        let tail_writer = tail.clone();
        let stderr_task = tokio::spawn(async move {
            let mut buffer = [0; 4096];
            loop {
                match stderr.read(&mut buffer).await {
                    Ok(0) => break,
                    Ok(size) => {
                        let mut tail = tail_writer.lock().await;
                        tail.extend_from_slice(&buffer[..size]);
                        let excess = tail.len().saturating_sub(16384);
                        tail.drain(..excess);
                    }
                    Err(error) => {
                        tracing::debug!(%error, "ACP stderr closed");
                        break;
                    }
                }
            }
        });
        let incoming = FramedRead::new(stdout, LinesCodec::new_with_max_length(8 * 1024 * 1024))
            .map(|line| line.map_err(std::io::Error::other));
        let outgoing = sink::unfold(stdin, |mut writer, line: String| async move {
            writer.write_all(line.as_bytes()).await?;
            writer.write_all(b"\n").await?;
            writer.flush().await?;
            Ok::<_, std::io::Error>(writer)
        });
        let transport = Lines::new(Box::pin(outgoing), Box::pin(incoming));
        let (sender, events) = mpsc::channel(1024);
        let queued_bytes = Arc::new(AtomicUsize::new(0));
        let pending_bytes = queued_bytes.clone();
        let stop = CancellationToken::new();
        let startup_guard = stop.clone().drop_guard();
        let events_tx = sender.clone();
        let permissions = Arc::new(Mutex::new(Permissions::default()));
        let approval_cancelled = Arc::new(AtomicBool::new(false));
        let request_cancelled = approval_cancelled.clone();
        let tool_activity = Arc::new(AtomicUsize::new(0));
        let notification_activity = tool_activity.clone();
        let request_activity = tool_activity.clone();
        let notifications = sender.clone();
        let permission_updates = permissions.clone();
        let overflow = stop.clone();
        let request_permissions = permissions.clone();
        let (ready, received) = oneshot::channel();
        let stopped = stop.clone();
        let worker = tokio::spawn(async move {
            let builder = Client.v1()
                .on_receive_notification(async move |message: UntypedMessage, _peer| {
                    if message.method() == "session/update" {
                        let params = message.params().clone();
                        let update = &params["update"];
                        if matches!(update["sessionUpdate"].as_str(), Some("tool_call" | "tool_call_update")) {
                            notification_activity.fetch_add(1, Ordering::Relaxed);
                        }
                        if let (Some(id), Some(server), Some(tool)) = (
                            update["toolCallId"].as_str(), update["rawInput"]["providerIdentifier"].as_str(),
                            update["rawInput"]["toolName"].as_str(),
                        ) {
                            let mut state = permission_updates.lock().await;
                            if params["sessionId"].as_str() == Some(state.session.as_str()) && state.server == server && state.tools.contains(tool) {
                                let key = (state.session.clone(), id.to_owned());
                                if id.len() > 256 || (state.calls.len() >= 1024 && !state.calls.contains_key(&key)) {
                                    state.calls.clear();
                                    overflow.cancel();
                                    return Err(agent_client_protocol::Error::internal_error().data("ACP pending permission limit exceeded"));
                                }
                                state.calls.insert(key, tool.to_owned());
                            }
                        }
                        if update["status"] == "completed" || update["status"] == "failed" {
                            if let Some(id) = update["toolCallId"].as_str() {
                                if let Some(session) = params["sessionId"].as_str() {
                                    permission_updates.lock().await.calls.remove(&(session.to_owned(), id.to_owned()));
                                }
                            }
                        }
                        let size = serde_json::to_vec(&params)?.len();
                        let prior = pending_bytes.fetch_add(size, Ordering::Relaxed);
                        if prior.saturating_add(size) > 16 * 1024 * 1024 {
                            pending_bytes.fetch_sub(size, Ordering::Relaxed);
                            overflow.cancel();
                            return Err(agent_client_protocol::Error::internal_error().data("ACP pending notification bytes exceeded 16 MiB"));
                        }
                        if let Err(error) = notifications.try_send((Event::Notification(params), size)) {
                            pending_bytes.fetch_sub(size, Ordering::Relaxed);
                            overflow.cancel();
                            return Err(agent_client_protocol::Error::internal_error().data(format!("ACP event queue unavailable: {error}")));
                        }
                    } else if matches!(message.method(), "cursor/task" | "cursor/generate_image") {
                        notification_activity.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(())
                }, agent_client_protocol::on_receive_notification!())
                .on_receive_request(async move |message: UntypedMessage, responder, _peer| {
                    let params = message.params();
                    request_activity.fetch_add(1, Ordering::Relaxed);
                    let result = match message.method() {
                        "session/request_permission" => {
                            if request_cancelled.load(Ordering::Relaxed) {
                                return responder.respond(json!({"outcome":{"outcome":"cancelled"}}));
                            }
                            let mut state = request_permissions.lock().await;
                            let session = state.session.clone();
                            let permitted = if params["sessionId"].as_str() == Some(state.session.as_str()) {
                                params["toolCall"]["toolCallId"].as_str()
                                    .and_then(|id| state.calls.remove(&(session, id.to_owned())))
                                    .is_some_and(|tool| state.tools.contains(&tool))
                            } else { false };
                            let kind = if permitted { "allow_once" } else { "reject_once" };
                            let option = params["options"].as_array().and_then(|options| options.iter().find(|option| option["kind"] == kind));
                            match option.and_then(|option| option["optionId"].as_str()) {
                                Some(id) => json!({"outcome":{"outcome":"selected","optionId":id}}),
                                None => json!({"outcome":{"outcome":"cancelled"}}),
                            }
                        }
                        "cursor/ask_question" => json!({"outcome":{"outcome":"skipped","reason":"No interactive Cursor UI in this proxy"}}),
                        "cursor/create_plan" => json!({"outcome":{"outcome":"rejected","reason":"Plan approval remains with the client"}}),
                        _ => return responder.respond_with_error(agent_client_protocol::Error::method_not_found()),
                    };
                    responder.respond(result)
                }, agent_client_protocol::on_receive_request!());
            let control = stopped.clone();
            let io = builder.connect_with(transport, async move |peer: ConnectionTo<Agent>| {
                ready.send(peer.clone()).map_err(|_| {
                    agent_client_protocol::Error::internal_error()
                        .data("ACP owner dropped during startup")
                })?;
                tokio::select! { _ = control.cancelled() => (), _ = peer.incoming_closed() => () }
                Ok(())
            });
            tokio::pin!(io);
            let failure = tokio::select! {
                result = &mut io => result.err().map(protocol_error),
                _ = stopped.cancelled() => {
                    match tokio::time::timeout(Duration::from_millis(250), &mut io).await {
                        Ok(result) => result.err().map(protocol_error),
                        Err(_) => Some(ApiError::upstream("ACP shutdown write drain timed out")),
                    }
                }
                status = process.child.wait() => {
                    if tokio::time::timeout(Duration::from_millis(250), &mut io).await.is_err() {
                        stopped.cancel();
                    }
                    Some(match status {
                        Ok(status) => ApiError::upstream(format!("ACP process exited with {status}")),
                        Err(error) => ApiError::upstream(format!("Cannot wait for ACP process: {error}")),
                    })
                }
            };
            process.terminate();
            if let Err(error) = process.child.wait().await {
                tracing::debug!(%error, "ACP process reap failed");
            }
            let mut stderr_task = stderr_task;
            if tokio::time::timeout(Duration::from_millis(250), &mut stderr_task)
                .await
                .is_err()
            {
                stderr_task.abort();
            }
            let description = String::from_utf8_lossy(&tail.lock().await).into_owned();
            let error = if description.trim().is_empty() {
                failure.unwrap_or_else(|| ApiError::upstream("ACP connection closed"))
            } else {
                ApiError::from_agent(&description)
            };
            if sender.send((Event::Exited(error), 0)).await.is_err() {
                tracing::debug!("ACP event receiver already closed");
            }
        });
        let peer = tokio::time::timeout(config.timeout, received)
            .await
            .map_err(|_| ApiError::upstream("ACP startup timed out"))?
            .map_err(|_| ApiError::upstream("ACP connection failed during startup"))?;
        let mut connection = Self {
            peer,
            events,
            events_tx,
            queued_bytes,
            stop,
            worker: Some(worker),
            permissions,
            approval_cancelled,
            tool_activity,
            timeout: config.timeout,
            capabilities: Value::Null,
        };
        let _owned_stop = startup_guard.disarm();
        let initialized = tokio::time::timeout(config.timeout, connection.call("initialize", json!({
            "protocolVersion":1,
            "clientInfo":{"name":"cursor-proxy","version":env!("CARGO_PKG_VERSION")},
            "clientCapabilities":{"fs":{"readTextFile":false,"writeTextFile":false},"terminal":false},
        }))).await.map_err(|_| ApiError::upstream("ACP initialization timed out"))??;
        if initialized["protocolVersion"] != 1 {
            return Err(ApiError::upstream(
                "Cursor ACP protocol version is not supported",
            ));
        }
        connection.capabilities = initialized["agentCapabilities"].clone();
        Ok(connection)
    }

    pub async fn call(&self, method: &str, params: Value) -> Result<Value, ApiError> {
        Ok(self.call_ordered(method, params).await?.apply())
    }

    pub async fn call_ordered(
        &self,
        method: &str,
        params: Value,
    ) -> Result<OrderedResponse, ApiError> {
        let request = self
            .peer
            .prepare_request(UntypedMessage::new(method, params).map_err(protocol_error)?);
        let mut pending = PendingRequest(Some(request.cancellation_handle()));
        let (sender, received) = oneshot::channel();
        let (applied, application) = oneshot::channel();
        let stop = self.stop.clone();
        let timeout = self.timeout;
        request
            .on_receiving_result(async move |result| {
                if sender.send((result, applied)).is_err() {
                    tracing::debug!("ACP response owner already closed");
                }
                match tokio::time::timeout(timeout, application).await {
                    Ok(Ok(())) => (),
                    Ok(Err(_)) => tracing::debug!("ACP response application abandoned"),
                    Err(_) => {
                        stop.cancel();
                        return Err(agent_client_protocol::Error::internal_error()
                            .data("ACP response application timed out"));
                    }
                }
                Ok(())
            })
            .map_err(protocol_error)?;
        let (result, applied) = tokio::time::timeout(self.timeout, received)
            .await
            .map_err(|_| ApiError::upstream("ACP request timed out"))?
            .map_err(|_| ApiError::upstream("ACP response callback was dropped"))?;
        pending.0 = None;
        if self.stop.is_cancelled() {
            return Err(ApiError::upstream("ACP event dispatch stopped"));
        }
        Ok(OrderedResponse {
            value: result.map_err(protocol_error)?,
            applied: Some(applied),
        })
    }

    pub fn prompt(&self, session: &str, text: &str) -> Result<(), ApiError> {
        let events = self.events_tx.clone();
        let stopped = self.stop.clone();
        self.peer
            .prepare_request(
                UntypedMessage::new(
                    "session/prompt",
                    json!({
                        "sessionId":session,"prompt":[{"type":"text","text":text}],
                    }),
                )
                .map_err(protocol_error)?,
            )
            .on_receiving_result(async move |result| {
                let result = if stopped.is_cancelled() {
                    Err(ApiError::upstream("ACP event dispatch stopped"))
                } else {
                    result.map_err(protocol_error)
                };
                events
                    .try_send((Event::Finished(result), 0))
                    .map_err(|error| {
                        agent_client_protocol::Error::internal_error()
                            .data(format!("ACP result queue unavailable: {error}"))
                    })
            })
            .map_err(protocol_error)
    }

    pub fn cancel(&self, session: &str) -> Result<(), ApiError> {
        self.approval_cancelled.store(true, Ordering::Relaxed);
        self.peer
            .send_notification(
                UntypedMessage::new("session/cancel", json!({"sessionId":session}))
                    .map_err(protocol_error)?,
            )
            .map_err(protocol_error)
    }

    pub async fn allow_tools(
        &self,
        session: &str,
        server: &str,
        tools: impl IntoIterator<Item = String>,
        mode: ExecutionMode,
    ) {
        let mut state = self.permissions.lock().await;
        state.calls.clear();
        state.session = session.to_owned();
        state.server = server.to_owned();
        state.tools = tools
            .into_iter()
            .filter(|name| mode.allows_tool(name))
            .collect();
        self.approval_cancelled.store(false, Ordering::Relaxed);
    }

    pub async fn next(&mut self) -> Option<Event> {
        let (event, size) = self.events.recv().await?;
        self.queued_bytes.fetch_sub(size, Ordering::Relaxed);
        Some(event)
    }

    pub fn tool_activity(&self) -> usize {
        self.tool_activity.load(Ordering::Relaxed)
    }

    pub fn closed(&self) -> impl std::future::Future<Output = ()> + Send + 'static {
        let peer = self.peer.clone();
        async move {
            peer.terminated().await;
        }
    }

    pub fn take_pending(&mut self) -> Result<Vec<Event>, ApiError> {
        let mut events = Vec::new();
        loop {
            match self.events.try_recv() {
                Ok((event, size)) => {
                    self.queued_bytes.fetch_sub(size, Ordering::Relaxed);
                    match event {
                        Event::Exited(error) | Event::Finished(Err(error)) => return Err(error),
                        event => events.push(event),
                    }
                }
                Err(mpsc::error::TryRecvError::Empty) => return Ok(events),
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    return Err(ApiError::upstream("ACP event stream closed"));
                }
            }
        }
    }

    pub async fn close(mut self) {
        self.stop.cancel();
        self.events.close();
        if let Some(mut worker) = self.worker.take()
            && tokio::time::timeout(Duration::from_secs(5), &mut worker)
                .await
                .is_err()
        {
            worker.abort();
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

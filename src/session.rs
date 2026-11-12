use crate::{
    acp::{Connection, Event, OrderedResponse},
    agent::{AgentConfig, Update},
    chat::{ChatRequest, ExecutionMode, Message, prompt},
    error::ApiError,
    mcp::{Bridge, HostCall, SERVER_NAME},
    stream::Usage,
    tools::{ToolCall, check_loop, render_history},
    usage,
};
use axum::http::StatusCode;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Instant};
use tokio::sync::{Mutex, mpsc, watch};
use uuid::Uuid;

const FINAL_ANSWER_PROMPT: &str = "Your previous turn ended without a final answer. Provide the final answer to the existing request now. Do not repeat the task, repeat your reasoning, or call any tools.";

#[derive(Default)]
pub struct SessionStore {
    entries: Mutex<HashMap<String, Record>>,
    configuration: Option<PathBuf>,
    version: crate::opencode::Version,
}

#[derive(PartialEq)]
enum State {
    Running,
    Tools,
}

struct Record {
    sender: mpsc::Sender<Attachment>,
    fingerprint: String,
    state: State,
    history_hash: String,
    history_len: usize,
    pending: Vec<String>,
}

struct Attachment {
    request: ChatRequest,
    output: mpsc::Sender<Update>,
}

fn hash(bytes: impl AsRef<[u8]>) -> String {
    format!("{:x}", Sha256::digest(bytes.as_ref()))
}

fn history_hash(messages: &[Message]) -> String {
    hash(json!(messages.iter().map(|message| json!({
        "role":message.role,"text":message.text(),"name":message.name,"tool_call_id":message.tool_call_id,
        "calls":message.tool_calls.iter().map(|call| json!({"id":call.id,"name":call.function.name,
            "arguments":serde_json::from_str::<Value>(&call.function.arguments).unwrap_or_else(|_| json!(call.function.arguments))})).collect::<Vec<_>>()
    })).collect::<Vec<_>>()).to_string())
}

fn fingerprint(request: &ChatRequest, workspace: &std::path::Path) -> String {
    hash(
        json!({"workspace":workspace,"model":request.model(),"tools":request.tools,
        "choice":request.tool_choice,"mode":request.execution_mode().as_str()})
        .to_string(),
    )
}

fn assistant(text: &str, calls: Vec<ToolCall>) -> Message {
    Message {
        role: "assistant".to_owned(),
        content: if text.is_empty() && !calls.is_empty() {
            Value::Null
        } else {
            json!(text)
        },
        name: None,
        tool_call_id: None,
        tool_calls: calls,
    }
}

fn timeout_error() -> ApiError {
    ApiError::new(
        StatusCode::GATEWAY_TIMEOUT,
        "cursor_agent_timeout",
        "Cursor ACP conversation timed out",
    )
}

async fn send(
    output: &mpsc::Sender<Update>,
    update: Update,
    deadline: tokio::time::Instant,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<(), ApiError> {
    tokio::select! {
        _ = shutdown.changed() => Err(ApiError::upstream("Proxy is shutting down")),
        _ = tokio::time::sleep_until(deadline) => Err(timeout_error()),
        sent = output.send(update) => sent.map_err(|_| ApiError::upstream("Client disconnected")),
    }
}

async fn ordered(
    connection: &Connection,
    method: &str,
    params: Value,
    output: Option<&mpsc::Sender<Update>>,
    deadline: tokio::time::Instant,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<OrderedResponse, ApiError> {
    tokio::select! {
        _ = shutdown.changed() => Err(ApiError::upstream("Proxy is shutting down")),
        _ = tokio::time::sleep_until(deadline) => Err(timeout_error()),
        _ = async { if let Some(output) = output { output.closed().await } else { std::future::pending::<()>().await } } => Err(ApiError::upstream("Client disconnected during ACP ordering barrier")),
        response = connection.call_ordered(method, params) => response,
    }
}

impl SessionStore {
    pub fn with_configuration(path: PathBuf, version: crate::opencode::Version) -> Self {
        Self {
            configuration: Some(path),
            version,
            ..Self::default()
        }
    }

    pub fn start(
        self: Arc<Self>,
        config: AgentConfig,
        mut request: ChatRequest,
        workspace: PathBuf,
        mut shutdown: watch::Receiver<bool>,
        request_id: String,
    ) -> mpsc::Receiver<Update> {
        let (output, received) = mpsc::channel(16);
        tokio::spawn(async move {
            let started = Instant::now();
            let timeout = config.timeout;
            let model = request.model();
            tracing::info!(%request_id, model = ?model, stream = request.stream, messages = request.messages.len(), tools = request.tools.len(), "chat request");
            let result = tokio::time::timeout(timeout, async {
                if *shutdown.borrow() { return Err(ApiError::upstream("Proxy is shutting down")); }
                request.validate()?;
                if config.load_skills { prompt::load_workspace_instructions(&mut request, &workspace).await?; }
                let mut events = self.accept(config, request, workspace, shutdown.clone()).await?;
                loop {
                    let update = tokio::select! {
                        _ = output.closed() => return Err(ApiError::upstream("Client disconnected")),
                        _ = shutdown.changed() => return Err(ApiError::upstream("Proxy is shutting down")),
                        update = events.recv() => update.ok_or_else(|| ApiError::upstream("ACP reply ended without completion"))?,
                    };
                    match update {
                        Update::Error(error) => return Err(error),
                        Update::Done(usage) => {
                            if let Some(capacity) = usage.as_ref().and_then(|usage| usage.context_window)
                                && let Some(path) = self.configuration.clone()
                            {
                                tokio::select! {
                                        _ = shutdown.changed() => return Err(ApiError::upstream("Proxy is shutting down")),
                                        _ = output.closed() => return Err(ApiError::upstream("Client disconnected")),
                                        _ = usage::publish_capacity(self.version, path, model.clone(), capacity) => (),
                                }
                            }
                            output.send(Update::Done(usage)).await.map_err(|_| ApiError::upstream("Client disconnected"))?;
                            return Ok(());
                        }
                        update => tokio::select! {
                            _ = shutdown.changed() => return Err(ApiError::upstream("Proxy is shutting down")),
                            sent = output.send(update) => sent.map_err(|_| ApiError::upstream("Client disconnected"))?,
                        },
                    }
                }
            }).await.unwrap_or_else(|_| Err(timeout_error()));
            let outcome = result
                .as_ref()
                .map(|_| "completed")
                .unwrap_or_else(|error| error.code);
            tracing::info!(%request_id, model = ?model, outcome, elapsed_ms = %started.elapsed().as_millis(), "chat reply");
            if let Err(error) = result
                && output.try_send(Update::Error(error)).is_err()
            {
                tracing::debug!("chat error receiver closed");
            }
        });
        received
    }

    async fn accept(
        self: &Arc<Self>,
        config: AgentConfig,
        request: ChatRequest,
        workspace: PathBuf,
        shutdown: watch::Receiver<bool>,
    ) -> Result<mpsc::Receiver<Update>, ApiError> {
        let signature = fingerprint(&request, &workspace);
        let tail = request
            .messages
            .iter()
            .rev()
            .take_while(|message| message.role == "tool")
            .count();
        let (output, received) = mpsc::channel(16);
        let mut entries = self.entries.lock().await;
        if tail > 0 {
            let results = &request.messages[request.messages.len() - tail..];
            let ids: Vec<_> = results
                .iter()
                .filter_map(|message| message.tool_call_id.clone())
                .collect();
            let record = entries
                .values_mut()
                .find(|entry| entry.pending.iter().any(|id| ids.contains(id)))
                .ok_or_else(|| ApiError::bad_request("Unknown or expired tool_call_id"))?;
            let prefix = &request.messages[..request.messages.len() - tail];
            if record.state != State::Tools
                || signature != record.fingerprint
                || ids.len() != record.pending.len()
                || !record
                    .pending
                    .iter()
                    .all(|id| ids.iter().filter(|value| *value == id).count() == 1)
                || prefix.len() != record.history_len
                || history_hash(prefix) != record.history_hash
            {
                return Err(ApiError::bad_request(
                    "Tool result does not match its ACP conversation",
                ));
            }
            record
                .sender
                .try_send(Attachment { request, output })
                .map_err(|_| {
                    ApiError::upstream("ACP conversation is no longer accepting tool results")
                })?;
            record.pending.clear();
            record.state = State::Running;
            return Ok(received);
        }
        if entries.len() >= 64 {
            return Err(ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "too_many_conversations",
                "Too many active ACP conversations",
            ));
        }
        let id = Uuid::new_v4().simple().to_string();
        let (sender, controls) = mpsc::channel(1);
        entries.insert(
            id.clone(),
            Record {
                sender,
                fingerprint: signature,
                state: State::Running,
                history_hash: String::new(),
                history_len: 0,
                pending: Vec::new(),
            },
        );
        drop(entries);
        let store = self.clone();
        tokio::spawn(async move {
            let mut outlet = Some(output);
            let result = store
                .run(
                    &id,
                    config,
                    request,
                    workspace,
                    shutdown,
                    controls,
                    &mut outlet,
                )
                .await;
            store.entries.lock().await.remove(&id);
            if let Err(error) = result
                && let Some(outlet) = outlet
                && outlet.try_send(Update::Error(error)).is_err()
            {
                tracing::debug!("ACP conversation receiver closed");
            }
        });
        Ok(received)
    }

    async fn publish(&self, id: &str, request: &ChatRequest, text: &str, calls: Vec<ToolCall>) {
        let mut history = request.messages.clone();
        let pending = calls.iter().map(|call| call.id.clone()).collect();
        history.push(assistant(text, calls));
        if let Some(record) = self.entries.lock().await.get_mut(id) {
            record.history_hash = history_hash(&history);
            record.history_len = history.len();
            record.pending = pending;
            record.state = State::Tools;
        }
    }

    async fn run(
        self: &Arc<Self>,
        id: &str,
        config: AgentConfig,
        mut request: ChatRequest,
        workspace: PathBuf,
        mut shutdown: watch::Receiver<bool>,
        mut controls: mpsc::Receiver<Attachment>,
        outlet: &mut Option<mpsc::Sender<Update>>,
    ) -> Result<(), ApiError> {
        let catalog = request.catalog()?;
        let (calls, mut host_calls) = mpsc::channel(16);
        let setup = async {
            let connection = Connection::open(&config, &workspace, &request.model()).await?;
            let bridge = if catalog.tools.is_empty() {
                None
            } else {
                if connection.capabilities["mcpCapabilities"]["http"] != true {
                    return Err(ApiError::upstream("Cursor ACP does not support HTTP MCP"));
                }
                Some(Bridge::open(catalog.clone(), calls, request.execution_mode()).await?)
            };
            let servers: Vec<_> = bridge
                .as_ref()
                .map(Bridge::configuration)
                .into_iter()
                .collect();
            let response = connection
                .call_ordered("session/new", json!({"cwd":workspace,"mcpServers":servers}))
                .await?;
            let session = response.value()["sessionId"]
                .as_str()
                .filter(|id| !id.is_empty() && id.len() <= 256)
                .ok_or_else(|| ApiError::upstream("Cursor ACP returned an invalid session ID"))?
                .to_owned();
            connection
                .allow_tools(
                    &session,
                    SERVER_NAME,
                    catalog.tools.iter().map(|tool| tool.name.clone()),
                    request.execution_mode(),
                )
                .await;
            let created = response.apply();
            let mode = if request.execution_mode() == ExecutionMode::Plan {
                "plan"
            } else {
                "agent"
            };
            let barrier = if let Some(option) = created["configOptions"]
                .as_array()
                .and_then(|options| options.iter().find(|option| option["id"] == "mode"))
            {
                if option["type"] != "select"
                    || !option["options"].as_array().is_some_and(|options| {
                        options.iter().any(|option| {
                            option["value"] == mode
                                || option["options"].as_array().is_some_and(|group| {
                                    group.iter().any(|option| option["value"] == mode)
                                })
                        })
                    })
                {
                    return Err(ApiError::upstream(format!(
                        "Cursor ACP did not advertise requested mode: {mode}"
                    )));
                }
                Some((
                    "session/set_config_option",
                    json!({"sessionId":session,"configId":"mode","value":mode}),
                ))
            } else if let Some(modes) = created["modes"]["availableModes"].as_array() {
                if !modes.iter().any(|option| option["id"] == mode) {
                    return Err(ApiError::upstream(format!(
                        "Cursor ACP did not advertise requested mode: {mode}"
                    )));
                }
                Some((
                    "session/set_mode",
                    json!({"sessionId":session,"modeId":mode}),
                ))
            } else if mode == "plan" {
                return Err(ApiError::upstream("Cursor ACP did not advertise Plan mode"));
            } else {
                None
            };
            if let Some((method, params)) = &barrier {
                connection.call(method, params.clone()).await?;
            }
            let mut text = render_history(&request.messages);
            if !catalog.tools.is_empty() {
                text.push_str(
                    "\nUse the cursor_proxy MCP server for client-provided tools when applicable.",
                );
            }
            let activity = connection.tool_activity();
            let mcp_activity = bridge.as_ref().map_or(0, Bridge::tool_activity);
            let checkpoint_root = usage::baseline(&session).await;
            connection.prompt(&session, &text)?;
            Ok::<_, ApiError>((
                connection,
                bridge,
                session,
                barrier,
                activity,
                mcp_activity,
                checkpoint_root,
            ))
        };
        let (mut connection, bridge, session, barrier, activity, mcp_activity, checkpoint_root) = tokio::select! {
            _ = shutdown.changed() => return Err(ApiError::upstream("Proxy is shutting down")),
            _ = async { if let Some(output) = outlet.as_ref() { output.closed().await } else { std::future::pending::<()>().await } } => return Err(ApiError::upstream("Client disconnected during ACP setup")),
            result = tokio::time::timeout(config.timeout, setup) => result.map_err(|_| timeout_error())??,
        };
        let mut text = String::new();
        let mut pending = Vec::new();
        let mut called = false;
        let mut saw_thought = false;
        let mut repaired = false;
        let mut context_usage = None;
        let mut reported_usage: Option<Usage> = None;
        let mut bytes = 0usize;
        let mut deadline = tokio::time::Instant::now() + config.timeout;
        let result = async {
            loop {
                enum Input { Shutdown, Timeout, Disconnected, Closed, ToolCancelled, Attach(Option<Attachment>), Call(Option<HostCall>), Acp(Option<Event>) }
                let closed = connection.closed();
                let input = tokio::select! {
                    _ = shutdown.changed() => Input::Shutdown,
                    _ = tokio::time::sleep_until(deadline) => Input::Timeout,
                    _ = async { if let Some(output) = outlet.as_ref() { output.closed().await } else { std::future::pending::<()>().await } } => Input::Disconnected,
                    attachment = controls.recv() => Input::Attach(attachment),
                    _ = closed, if outlet.is_none() => Input::Closed,
                    _ = async { if let Some(call) = pending.first_mut() { let call: &mut HostCall = call; call.reply.closed().await } else { std::future::pending::<()>().await } } => Input::ToolCancelled,
                    call = host_calls.recv(), if bridge.is_some() && outlet.is_some() => Input::Call(call),
                    event = connection.next() => Input::Acp(event),
                };
                match input {
                    Input::Shutdown => return Err(ApiError::upstream("Proxy is shutting down")),
                    Input::Timeout => return Err(timeout_error()),
                    Input::Disconnected => return Err(ApiError::upstream("Client disconnected")),
                    Input::Closed => return Err(ApiError::upstream("Cursor ACP closed while awaiting a host result")),
                    Input::ToolCancelled => return Err(ApiError::upstream("MCP tool call was cancelled before its host result arrived")),
                    Input::Attach(None) => return Ok(()),
                    Input::Attach(Some(attachment)) => {
                        if outlet.is_some() { return Err(ApiError::upstream("ACP reply attachment overlapped an active reply")); }
                        *outlet = Some(attachment.output);
                        deadline = tokio::time::Instant::now() + config.timeout;
                        if let Some((method, params)) = &barrier {
                            let response = ordered(&connection, method, params.clone(), outlet.as_ref(), deadline, &mut shutdown).await?;
                            for event in connection.take_pending()? {
                                if let Event::Notification(value) = event
                                    && value["sessionId"] == session
                                {
                                    if let Some(usage) = Usage::context_update(&value["update"]) { context_usage = Some(usage); }
                                    if matches!(value["update"]["sessionUpdate"].as_str(), Some("agent_message_chunk" | "agent_thought_chunk")) {
                                        return Err(ApiError::upstream("ACP generated unassignable content before the host tool result"));
                                    }
                                }
                            }
                            response.apply();
                        }
                        request = attachment.request;
                        text.clear();
                        bytes = 0;
                        for call in pending.drain(..) {
                            let call: HostCall = call;
                            let result = request.messages.iter().rev().find(|message| message.role == "tool" && message.tool_call_id.as_deref() == Some(call.call.id.as_str()))
                                .ok_or_else(|| ApiError::bad_request("Missing pending tool result"))?;
                            call.reply.send(result.text()).map_err(|_| ApiError::upstream("MCP call expired before its host result arrived"))?;
                        }
                    }
                    Input::Call(None) => return Err(ApiError::upstream("Internal MCP call channel closed")),
                    Input::Call(Some(call)) => {
                        if repaired { return Err(ApiError::upstream("Cursor requested a host tool during final-answer recovery")); }
                        if call.reply.is_closed() { continue; }
                        let ordered = if let Some((method, params)) = &barrier {
                            Some(ordered(&connection, method, params.clone(), outlet.as_ref(), deadline, &mut shutdown).await?)
                        } else {
                            return Err(ApiError::upstream("Cursor ACP cannot establish a tool handoff ordering barrier"));
                        };
                        for event in connection.take_pending()? {
                            match event {
                                 Event::Notification(value) if value["sessionId"] == session => {
                                     if let Some(usage) = Usage::context_update(&value["update"]) { context_usage = Some(usage); }
                                    let kind = value["update"]["sessionUpdate"].as_str().unwrap_or("");
                                     if !matches!(kind, "agent_message_chunk" | "agent_thought_chunk") { continue; }
                                     let value = value["update"]["content"]["text"].as_str().ok_or_else(|| ApiError::upstream("Non-text ACP reply is not supported"))?.to_owned();
                                     if kind == "agent_thought_chunk" && !value.trim().is_empty() { saw_thought = true; }
                                    bytes = bytes.saturating_add(value.len());
                                    if bytes > 16 * 1024 * 1024 { return Err(ApiError::upstream("ACP response exceeds 16 MiB")); }
                                    if let Some(output) = outlet.as_ref() {
                                        if kind == "agent_message_chunk" { text.push_str(&value); }
                                        send(output, if kind == "agent_message_chunk" { Update::Text(value) } else { Update::Thinking(value) }, deadline, &mut shutdown).await?;
                                    }
                                }
                                Event::Finished(_) => return Err(ApiError::upstream("ACP prompt ended before its pending MCP call")),
                                _ => (),
                            }
                        }
                        if let Some(ordered) = ordered { ordered.apply(); }
                        check_loop(&request.messages, &call.call)?;
                        request.check_tool_choice(std::slice::from_ref(&call.call))?;
                        self.publish(id, &request, &text, vec![call.call.clone()]).await;
                        pending.push(call);
                        called = true;
                        if let Some(output) = outlet.take() {
                            send(&output, Update::Tools(vec![pending[0].call.clone()]), deadline, &mut shutdown).await?;
                            let usage = usage::resolve(reported_usage.clone(), context_usage.clone(), &session, &checkpoint_root).await;
                            send(&output, Update::Done(usage), deadline, &mut shutdown).await?;
                        }
                        deadline = tokio::time::Instant::now() + config.timeout;
                    }
                    Input::Acp(None) => return Err(ApiError::upstream("ACP event stream closed")),
                    Input::Acp(Some(Event::Exited(error))) => return Err(error),
                    Input::Acp(Some(Event::Notification(value))) => {
                        if value["sessionId"] != session { continue; }
                        if repaired && (connection.tool_activity() != activity || bridge.as_ref().is_some_and(|bridge| bridge.tool_activity() != mcp_activity)) { return Err(ApiError::upstream("Cursor reported tool activity during final-answer recovery")); }
                        let update = &value["update"];
                        if let Some(usage) = Usage::context_update(update) { context_usage = Some(usage); }
                        let kind = update["sessionUpdate"].as_str().unwrap_or("");
                        if !matches!(kind, "agent_message_chunk" | "agent_thought_chunk") { continue; }
                        let value = update["content"]["text"].as_str().ok_or_else(|| ApiError::upstream("Non-text ACP reply is not supported"))?.to_owned();
                        if kind == "agent_thought_chunk" && !value.trim().is_empty() { saw_thought = true; }
                        bytes = bytes.saturating_add(value.len());
                        if bytes > 16 * 1024 * 1024 { return Err(ApiError::upstream("ACP response exceeds 16 MiB")); }
                        if let Some(output) = outlet.as_ref() {
                            if kind == "agent_message_chunk" { text.push_str(&value); }
                            send(output, if kind == "agent_message_chunk" { Update::Text(value) } else { Update::Thinking(value) }, deadline, &mut shutdown).await?;
                        } else { return Err(ApiError::upstream("ACP generated reply content while awaiting a host tool result")); }
                    }
                    Input::Acp(Some(Event::Finished(result))) => {
                        let result = result?;
                        if result["stopReason"] != "end_turn" { return Err(ApiError::upstream(format!("Cursor ACP stopped: {}", result["stopReason"]))); }
                        if let Some(current) = Usage::reported(&result["usage"]) {
                            if let Some(previous) = reported_usage.as_mut() { previous.accumulate_reported(&current); }
                            else { reported_usage = Some(current); }
                        }
                        if !called { request.check_tool_choice(&[])?; }
                        if !pending.is_empty() { return Err(ApiError::upstream("ACP prompt ended with pending host tools")); }
                        if repaired && (connection.tool_activity() != activity || bridge.as_ref().is_some_and(|bridge| bridge.tool_activity() != mcp_activity)) { return Err(ApiError::upstream("Cursor reported tool activity during final-answer recovery")); }
                        if text.is_empty() && !called {
                            if saw_thought && !repaired && host_calls.is_empty() && connection.tool_activity() == activity && bridge.as_ref().is_none_or(|bridge| bridge.tool_activity() == mcp_activity) {
                                repaired = true;
                                connection.allow_tools(&session, SERVER_NAME, [], request.execution_mode()).await;
                                if connection.tool_activity() != activity || bridge.as_ref().is_some_and(|bridge| bridge.tool_activity() != mcp_activity) || !host_calls.is_empty() {
                                    return Err(ApiError::upstream("Tool activity arrived before final-answer recovery"));
                                }
                                if *shutdown.borrow() || outlet.as_ref().is_none_or(|output| output.is_closed()) || tokio::time::Instant::now() >= deadline {
                                    return Err(ApiError::upstream("Final-answer recovery was cancelled or timed out"));
                                }
                                tracing::warn!(conversation_id = id, "Cursor returned reasoning without text; requesting one final answer");
                                connection.prompt(&session, FINAL_ANSWER_PROMPT)?;
                                continue;
                            }
                            return Err(ApiError::upstream("Cursor ACP ended the turn without reply text"));
                        }
                        let usage = usage::resolve(reported_usage.clone(), context_usage.clone(), &session, &checkpoint_root).await;
                        if let Some(output) = outlet.take() { send(&output, Update::Done(usage), deadline, &mut shutdown).await?; }
                        return Ok(());
                    }
                }
            }
        }.await;
        if result.is_err()
            && let Err(error) = connection.cancel(&session)
        {
            tracing::debug!(%error, "ACP cancellation could not be sent");
        }
        connection.close().await;
        if let Some(bridge) = bridge {
            bridge.close().await;
        }
        result
    }
}

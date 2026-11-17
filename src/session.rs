mod database;
mod runtime;

use crate::{
    acp::{Connection, Event, OrderedResponse},
    agent::{AgentConfig, Update},
    chat::{ChatRequest, Message, prompt},
    error::ApiError,
    mcp::HostCall,
    stream::Usage,
    tools::{ToolCall, render_history},
    usage,
};
use axum::http::StatusCode;
use database::{Database, Mapping, now};
use runtime::Runtime;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Instant};
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, Span};
use uuid::Uuid;

const FINAL_ANSWER_PROMPT: &str = "Your previous turn ended without a final answer. Provide the final answer to the existing request now. Do not repeat the task, repeat your reasoning, or call any tools.";
const MAX_LIVE: usize = 64;

pub struct SessionStore {
    entries: Mutex<HashMap<String, Record>>,
    configuration: Option<PathBuf>,
    version: crate::opencode::Version,
    database: Database,
}

#[derive(PartialEq)]
enum State {
    Running,
    Tools,
    Idle,
}

struct Record {
    mapping: Mapping,
    stop: CancellationToken,
    last_used: Instant,
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
    skip: usize,
    deadline: tokio::time::Instant,
    accepted: Option<oneshot::Sender<()>>,
    span: Span,
}

fn logical_history(messages: &[Message]) -> Vec<Message> {
    messages
        .iter()
        .filter(|message| {
            !matches!(message.role.as_str(), "system" | "developer")
                && message.mode_reminder().is_none()
        })
        .cloned()
        .collect()
}

fn instruction_hash(messages: &[Message]) -> String {
    history_hash(
        &messages
            .iter()
            .filter(|message| matches!(message.role.as_str(), "system" | "developer"))
            .cloned()
            .collect::<Vec<_>>(),
    )
}

fn continuation(mapping: &Mapping, boundaries: &[String]) -> bool {
    mapping.history_len > 0
        && mapping.history_len <= boundaries.len()
        && boundaries[mapping.history_len - 1] == mapping.history_hash
        && (!mapping.ready || mapping.history_len < boundaries.len())
}

fn new_prompt(request: &ChatRequest, skip: usize, previous_instructions: &str) -> String {
    let mut text = if skip == 0 {
        render_history(&request.messages)
    } else {
        let mut messages = Vec::new();
        if instruction_hash(&request.messages) != previous_instructions {
            messages.extend(
                request
                    .messages
                    .iter()
                    .filter(|message| matches!(message.role.as_str(), "system" | "developer"))
                    .cloned(),
            );
        }
        if let Some(reminder) = request
            .messages
            .iter()
            .rev()
            .find(|message| message.mode_reminder().is_some())
        {
            messages.push(reminder.clone());
        }
        messages.extend(logical_history(&request.messages).into_iter().skip(skip));
        render_history(&messages)
    };
    if !request.tools.is_empty() {
        text.push_str("\nUse the cursor_proxy MCP server for client-provided tools, including command execution when available. OpenCode checks their permissions in both Plan and Build modes.");
    }
    text
}

fn hash(bytes: impl AsRef<[u8]>) -> String {
    format!("{:x}", Sha256::digest(bytes.as_ref()))
}

fn history_item(message: &Message) -> Value {
    json!({
        "role":message.role,"text":message.text(),"name":message.name,"tool_call_id":message.tool_call_id,
        "calls":message.tool_calls.iter().map(|call| json!({"id":call.id,"name":call.function.name,
            "arguments":serde_json::from_str::<Value>(&call.function.arguments).unwrap_or_else(|_| json!(call.function.arguments))})).collect::<Vec<_>>()
    })
}

fn history_hash(messages: &[Message]) -> String {
    hash(json!(messages.iter().map(history_item).collect::<Vec<_>>()).to_string())
}

fn history_boundaries(messages: &[Message]) -> Vec<String> {
    let mut digest = Sha256::new();
    digest.update(b"[");
    messages
        .iter()
        .enumerate()
        .map(|(index, message)| {
            if index > 0 {
                digest.update(b",");
            }
            digest.update(history_item(message).to_string());
            let mut boundary = digest.clone();
            boundary.update(b"]");
            format!("{:x}", boundary.finalize())
        })
        .collect()
}

fn fingerprint(request: &ChatRequest, workspace: &std::path::Path) -> String {
    hash(
        json!({"owner":request.session_id,"workspace":workspace,"model":request.model(),"tools":request.tools,
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
    pub async fn open(path: PathBuf, version: crate::opencode::Version) -> Result<Self, ApiError> {
        let database = Database::open(
            path.parent()
                .ok_or_else(|| ApiError::bad_request("Configuration directory missing"))?
                .join("cursor-proxy/sessions.db"),
        )
        .await?;
        database.cleanup(Default::default()).await?;
        Ok(Self {
            entries: Mutex::new(HashMap::new()),
            configuration: Some(path),
            version,
            database,
        })
    }

    pub async fn maintain(self: Arc<Self>, mut shutdown: watch::Receiver<bool>) {
        let mut timer = tokio::time::interval(std::time::Duration::from_secs(24 * 60 * 60));
        loop {
            tokio::select! {
                _ = shutdown.changed() => break,
                _ = timer.tick() => {
                    let mut entries = self.entries.lock().await;
                    let active = entries.iter().filter(|(_, entry)| entry.state != State::Idle).map(|(id, _)| id.clone()).collect();
                    match self.database.cleanup(active).await {
                        Ok(expired) => for id in expired { if let Some(entry) = entries.remove(&id) { entry.stop.cancel(); } },
                        Err(error) => tracing::warn!(%error, "Session expiry cleanup failed"),
                    }
                }
            }
        }
        let senders: Vec<_> = {
            let entries = self.entries.lock().await;
            entries
                .values()
                .map(|entry| {
                    entry.stop.cancel();
                    entry.sender.clone()
                })
                .collect()
        };
        if tokio::time::timeout(
            std::time::Duration::from_secs(8),
            futures::future::join_all(senders.iter().map(|sender| sender.closed())),
        )
        .await
        .is_err()
        {
            tracing::warn!("Cursor session shutdown deadline exceeded");
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
        let model = request.model();
        // Keep correlation fields available with warning-only filters.
        let span = tracing::error_span!("chat", req = %request_id,
            oc = request.session_id.as_deref().unwrap_or("-"), model = ?model,
            cursor = tracing::field::Empty);
        tokio::spawn(async move {
            let started = Instant::now();
            let timeout = config.timeout;
            let deadline = tokio::time::Instant::now() + timeout;
            tracing::info!("request");
            tracing::debug!(stream = request.stream, messages = request.messages.len(), tools = request.tools.len(), "input");
            let result = tokio::time::timeout(timeout, async {
                if *shutdown.borrow() { return Err(ApiError::upstream("Proxy is shutting down")); }
                request.validate()?;
                if config.load_skills { prompt::load_workspace_instructions(&mut request, &workspace).await?; }
                let mut events = self.accept(config, request, workspace, shutdown.clone(), deadline).await?;
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
            match &result {
                Ok(()) => tracing::info!(ms = %started.elapsed().as_millis(), "reply"),
                Err(error) => tracing::warn!(code = %error.code, error = ?error.message, ms = %started.elapsed().as_millis(), "reply"),
            }
            if let Err(error) = result
                && output.try_send(Update::Error(error)).is_err()
            {
                tracing::debug!("reply receiver closed");
            }
        }.instrument(span));
        received
    }

    async fn accept(
        self: &Arc<Self>,
        config: AgentConfig,
        mut request: ChatRequest,
        workspace: PathBuf,
        shutdown: watch::Receiver<bool>,
        deadline: tokio::time::Instant,
    ) -> Result<mpsc::Receiver<Update>, ApiError> {
        let signature = fingerprint(&request, &workspace);
        let tail = request
            .messages
            .iter()
            .rev()
            .take_while(|message| message.role == "tool")
            .count();
        let (mut output, received) = mpsc::channel(16);
        let history = logical_history(&request.messages);
        let boundaries = history_boundaries(&history);
        let prefix_hash = history_hash(&history[..history.len() - tail]);
        let instructions = instruction_hash(&request.messages);
        let mut entries = self.entries.lock().await;
        if tail > 0 {
            let results = &request.messages[request.messages.len() - tail..];
            let ids: Vec<_> = results
                .iter()
                .filter_map(|message| message.tool_call_id.clone())
                .collect();
            if let Some(record) = entries
                .values_mut()
                .find(|entry| entry.pending.iter().any(|id| ids.contains(id)))
            {
                if record.state != State::Tools
                    || signature != record.fingerprint
                    || ids.len() != record.pending.len()
                    || !record
                        .pending
                        .iter()
                        .all(|id| ids.iter().filter(|value| *value == id).count() == 1)
                    || history.len() - tail != record.history_len
                    || prefix_hash != record.history_hash
                {
                    return Err(ApiError::bad_request(
                        "Tool result does not match its ACP conversation",
                    ));
                }
                let (accepted, confirmed) = oneshot::channel();
                let (attachment_output, attachment_events) = mpsc::channel(16);
                let id = record.mapping.id.clone();
                let sender = record.sender.clone();
                if record
                    .sender
                    .try_send(Attachment {
                        request: request.clone(),
                        output: attachment_output,
                        skip: 0,
                        deadline,
                        accepted: Some(accepted),
                        span: Span::current(),
                    })
                    .is_ok()
                {
                    record.pending.clear();
                    record.state = State::Running;
                    record.last_used = Instant::now();
                    drop(entries);
                    if confirmed.await.is_ok() {
                        return Ok(attachment_events);
                    }
                    entries = self.entries.lock().await;
                    if entries
                        .get(&id)
                        .is_some_and(|entry| entry.sender.same_channel(&sender))
                        && let Some(entry) = entries.remove(&id)
                    {
                        entry.stop.cancel();
                    }
                }
            }
        }
        drop(entries);
        loop {
            let mut candidates = self
                .database
                .candidates(
                    &request.session_id,
                    &workspace,
                    &request.model(),
                    boundaries.clone(),
                )
                .await?;
            let mut entries = self.entries.lock().await;
            entries.retain(|_, record| !record.sender.is_closed());
            candidates.retain(|mapping| !entries.contains_key(&mapping.id));
            if request.session_id.is_some() {
                candidates.extend(
                    entries
                        .values()
                        .filter(|entry| {
                            entry.mapping.owner == request.session_id
                                && entry.mapping.workspace == workspace
                                && entry.mapping.model == request.model()
                                && entry.mapping.last_used_at >= now() - database::RETENTION
                        })
                        .map(|entry| entry.mapping.clone()),
                );
            }
            candidates.sort_by_key(|mapping| std::cmp::Reverse(mapping.history_len));
            let mut mapping = candidates
                .into_iter()
                .find(|mapping| {
                    continuation(mapping, &boundaries)
                        && entries.get(&mapping.id).is_none_or(|record| {
                            record.state == State::Idle || record.sender.is_closed()
                        })
                })
                .unwrap_or_else(|| Mapping {
                    id: Uuid::new_v4().simple().to_string(),
                    owner: request.session_id.clone(),
                    workspace: workspace.clone(),
                    model: request.model(),
                    cursor_id: String::new(),
                    history_hash: boundaries.last().cloned().unwrap_or_else(|| hash("[]")),
                    history_len: history.len(),
                    instructions_hash: instructions.clone(),
                    ready: false,
                    created_at: now(),
                    last_used_at: now(),
                });
            let skip = if mapping.ready {
                mapping.history_len
            } else {
                0
            };
            if let Some(record) = entries.get_mut(&mapping.id) {
                if !record.sender.is_closed() {
                    match record.sender.try_send(Attachment {
                        request,
                        output,
                        skip,
                        deadline,
                        accepted: None,
                        span: Span::current(),
                    }) {
                        Ok(()) => {
                            record.state = State::Running;
                            record.fingerprint = signature;
                            record.last_used = Instant::now();
                            return Ok(received);
                        }
                        Err(error) => {
                            let attachment = error.into_inner();
                            request = attachment.request;
                            output = attachment.output;
                        }
                    }
                }
                entries.remove(&mapping.id);
            }
            if entries.len() >= MAX_LIVE {
                if let Some(id) = entries
                    .iter()
                    .filter(|(_, entry)| entry.state == State::Idle)
                    .min_by_key(|(_, entry)| entry.last_used)
                    .map(|(id, _)| id.clone())
                {
                    if let Some(entry) = entries.remove(&id) {
                        entry.stop.cancel();
                    }
                } else {
                    return Err(ApiError::new(
                        StatusCode::TOO_MANY_REQUESTS,
                        "too_many_conversations",
                        "Too many active ACP conversations",
                    ));
                }
            }
            let id = mapping.id.clone();
            let (sender, controls) = mpsc::channel(1);
            let stop = CancellationToken::new();
            mapping.last_used_at = now();
            entries.insert(
                id.clone(),
                Record {
                    sender: sender.clone(),
                    mapping: mapping.clone(),
                    stop: stop.clone(),
                    last_used: Instant::now(),
                    fingerprint: signature.clone(),
                    state: State::Running,
                    history_hash: String::new(),
                    history_len: 0,
                    pending: Vec::new(),
                },
            );
            drop(entries);
            if !mapping.cursor_id.is_empty() {
                match self.database.refresh(&mapping).await {
                    Ok(Some(current)) if continuation(&current, &boundaries) => mapping = current,
                    result => {
                        self.entries.lock().await.remove(&id);
                        result?;
                        continue;
                    }
                }
            }
            let store = self.clone();
            let span = Span::current();
            tokio::spawn(async move {
                let mut outlet = Some(output);
                let result = store
                    .run(
                        config,
                        request,
                        mapping,
                        shutdown,
                        (stop, controls, deadline, span),
                        &mut outlet,
                    )
                    .await;
                {
                    let mut entries = store.entries.lock().await;
                    if entries
                        .get(&id)
                        .is_some_and(|entry| entry.sender.same_channel(&sender))
                    {
                        entries.remove(&id);
                    }
                }
                if let Err(error) = result
                    && let Some(outlet) = outlet
                    && outlet.try_send(Update::Error(error)).is_err()
                {
                    tracing::debug!("ACP conversation receiver closed");
                }
            });
            return Ok(received);
        }
    }

    async fn remember(
        &self,
        mapping: &mut Mapping,
        request: &ChatRequest,
        completed: Option<&str>,
    ) -> Result<(), ApiError> {
        let mut history = logical_history(&request.messages);
        if let Some(text) = completed {
            history.push(assistant(text, Vec::new()));
        }
        mapping.history_hash = history_hash(&history);
        mapping.history_len = history.len();
        mapping.ready = completed.is_some();
        mapping.last_used_at = now();
        self.database.save(mapping).await?;
        if let Some(record) = self.entries.lock().await.get_mut(&mapping.id) {
            record.mapping = mapping.clone();
            record.last_used = Instant::now();
            if completed.is_some() {
                record.state = State::Idle;
            }
        }
        Ok(())
    }

    async fn publish(
        &self,
        mapping: &mut Mapping,
        request: &ChatRequest,
        text: &str,
        calls: Vec<ToolCall>,
    ) -> Result<(), ApiError> {
        let mut history = request.messages.clone();
        let pending = calls.iter().map(|call| call.id.clone()).collect();
        history.push(assistant(text, calls));
        let logical = logical_history(&history);
        mapping.history_hash = history_hash(&logical);
        mapping.history_len = logical.len();
        mapping.last_used_at = now();
        mapping.ready = false;
        self.database.save(mapping).await?;
        if let Some(record) = self.entries.lock().await.get_mut(&mapping.id) {
            record.mapping = mapping.clone();
            record.history_hash = mapping.history_hash.clone();
            record.history_len = logical.len();
            record.pending = pending;
            record.state = State::Tools;
        }
        Ok(())
    }

    async fn run(
        self: &Arc<Self>,
        config: AgentConfig,
        mut request: ChatRequest,
        mut mapping: Mapping,
        mut shutdown: watch::Receiver<bool>,
        control: (
            CancellationToken,
            mpsc::Receiver<Attachment>,
            tokio::time::Instant,
            Span,
        ),
        outlet: &mut Option<mpsc::Sender<Update>>,
    ) -> Result<(), ApiError> {
        let (stop, mut controls, mut deadline, mut span) = control;
        let skip = if mapping.ready {
            mapping.history_len
        } else {
            0
        };
        let (mut runtime, restored) = tokio::select! {
            _ = shutdown.changed() => return Err(ApiError::upstream("Proxy is shutting down")),
            _ = stop.cancelled() => return Ok(()),
            _ = async { if let Some(output) = outlet.as_ref() { output.closed().await } else { std::future::pending::<()>().await } } => return Err(ApiError::upstream("Client disconnected during ACP setup")),
            result = tokio::time::timeout_at(deadline, Runtime::open(&config, &mut mapping, &request, deadline).instrument(span.clone())) => result.map_err(|_| timeout_error())??,
        };
        span.record("cursor", runtime.session.as_str());
        let prompt = new_prompt(
            &request,
            if restored { skip } else { 0 },
            &mapping.instructions_hash,
        );
        mapping.instructions_hash = instruction_hash(&request.messages);
        self.remember(&mut mapping, &request, None).await?;
        let mut activity = runtime.connection.tool_activity();
        let mut mcp_activity = runtime
            .bridge
            .as_ref()
            .map_or(0, |bridge| bridge.tool_activity());
        let mut checkpoint_root = usage::baseline(&runtime.session).await;
        if *shutdown.borrow()
            || stop.is_cancelled()
            || outlet.as_ref().is_none_or(|output| output.is_closed())
        {
            runtime.close().await;
            return Err(ApiError::upstream("Request cancelled before prompting"));
        }
        if let Some(bridge) = &runtime.bridge {
            bridge.begin_turn()?;
        }
        runtime.connection.prompt(&runtime.session, &prompt)?;
        drop(prompt);
        let mut idle = false;
        let mut text = String::new();
        let mut pending = Vec::new();
        let mut called = false;
        let mut saw_thought = false;
        let mut repaired = false;
        let mut context_usage = None;
        let mut reported_usage: Option<Usage> = None;
        let mut bytes = 0usize;
        let result = async {
            loop {
                enum Input { Shutdown, Timeout, Disconnected, Closed, ToolCancelled, Attach(Option<Attachment>), Call(Option<HostCall>), Acp(Option<Event>) }
                let closed = runtime.connection.closed();
                let input = tokio::select! {
                    _ = shutdown.changed() => Input::Shutdown,
                    _ = stop.cancelled() => Input::Shutdown,
                    _ = tokio::time::sleep_until(deadline), if !idle => Input::Timeout,
                    _ = async { if let Some(output) = outlet.as_ref() { output.closed().await } else { std::future::pending::<()>().await } } => Input::Disconnected,
                    attachment = controls.recv() => Input::Attach(attachment),
                    _ = closed, if outlet.is_none() => Input::Closed,
                    _ = async { if let Some(call) = pending.first_mut() { let call: &mut HostCall = call; call.closed().await } else { std::future::pending::<()>().await } } => Input::ToolCancelled,
                    call = runtime.host_calls.recv(), if runtime.bridge.is_some() && outlet.is_some() => Input::Call(call),
                    event = runtime.connection.next() => Input::Acp(event),
                };
                match input {
                    Input::Shutdown => return Err(ApiError::upstream("Proxy is shutting down")),
                    Input::Timeout => return Err(timeout_error()),
                    Input::Disconnected => return Err(ApiError::upstream("Client disconnected")),
                    Input::Closed => return Err(ApiError::upstream("Cursor ACP connection closed")),
                    Input::ToolCancelled => return Err(ApiError::upstream("MCP tool call was cancelled before its host result arrived")),
                    Input::Attach(None) => return Ok(()),
                    Input::Attach(Some(attachment)) => {
                        if outlet.is_some() { return Err(ApiError::upstream("ACP reply attachment overlapped an active reply")); }
                        *outlet = Some(attachment.output);
                        span = attachment.span;
                        deadline = attachment.deadline;
                        if idle {
                            request = attachment.request;
                            let prepared = tokio::select! {
                                _ = shutdown.changed() => return Err(ApiError::upstream("Proxy is shutting down")),
                                _ = stop.cancelled() => return Err(ApiError::upstream("Session released")),
                                _ = tokio::time::sleep_until(deadline) => return Err(timeout_error()),
                                _ = async { if let Some(output) = outlet.as_ref() { output.closed().await } } => return Err(ApiError::upstream("Client disconnected")),
                                result = runtime.prepare(&config, &mut mapping, &request, attachment.skip, deadline).instrument(span.clone()) => result,
                            };
                            span.record("cursor", runtime.session.as_str());
                            let skip = prepared?;
                            let prompt = new_prompt(&request, skip, &mapping.instructions_hash);
                            mapping.instructions_hash = instruction_hash(&request.messages);
                            self.remember(&mut mapping, &request, None).await?;
                            activity = runtime.connection.tool_activity();
                            mcp_activity = runtime.bridge.as_ref().map_or(0, |bridge| bridge.tool_activity());
                            checkpoint_root = usage::baseline(&runtime.session).await;
                            if *shutdown.borrow() || stop.is_cancelled() || outlet.as_ref().is_none_or(|output| output.is_closed()) {
                                return Err(ApiError::upstream("Request cancelled before prompting"));
                            }
                            text.clear();
                            bytes = 0;
                            called = false;
                            saw_thought = false;
                            repaired = false;
                            context_usage = None;
                            reported_usage = None;
                            idle = false;
                            if let Some(bridge) = &runtime.bridge { bridge.begin_turn()?; }
                            runtime.connection.prompt(&runtime.session, &prompt)?;
                            continue;
                        }
                        if let Some((method, params)) = &runtime.barrier {
                            let response = ordered(&runtime.connection, method, params.clone(), outlet.as_ref(), deadline, &mut shutdown).await?;
                            for event in runtime.connection.take_pending()? {
                                if let Event::Notification(value) = event
                                    && value["sessionId"] == runtime.session
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
                        self.remember(&mut mapping, &request, None).await?;
                        text.clear();
                        bytes = 0;
                        for call in pending.drain(..) {
                            let call: HostCall = call;
                            let result = request.messages.iter().rev().find(|message| message.role == "tool" && message.tool_call_id.as_deref() == Some(call.call.id.as_str()))
                                .ok_or_else(|| ApiError::bad_request("Missing pending tool result"))?;
                            call.reply.send(result.text()).map_err(|_| ApiError::upstream("MCP call expired before its host result arrived"))?;
                        }
                        span.record("cursor", runtime.session.as_str());
                        if let Some(accepted) = attachment.accepted { let _delivered = accepted.send(()); }
                    }
                    Input::Call(None) => return Err(ApiError::upstream("Internal MCP call channel closed")),
                    Input::Call(Some(call)) => {
                        if repaired { return Err(ApiError::upstream("Cursor requested a host tool during final-answer recovery")); }
                        if call.is_closed() { continue; }
                        if !request.catalog()?.tools.iter().any(|tool| tool.name == call.call.function.name) {
                            continue;
                        }
                        let ordered = if let Some((method, params)) = &runtime.barrier {
                            Some(ordered(&runtime.connection, method, params.clone(), outlet.as_ref(), deadline, &mut shutdown).await?)
                        } else {
                            return Err(ApiError::upstream("Cursor ACP cannot establish a tool handoff ordering barrier"));
                        };
                        for event in runtime.connection.take_pending()? {
                            match event {
                                  Event::Notification(value) if value["sessionId"] == runtime.session => {
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
                        request.check_tool_choice(std::slice::from_ref(&call.call))?;
                        self.publish(&mut mapping, &request, &text, vec![call.call.clone()]).await?;
                        pending.push(call);
                        called = true;
                        if let Some(output) = outlet.take() {
                            send(&output, Update::Tools(vec![pending[0].call.clone()]), deadline, &mut shutdown).await?;
                            let usage = usage::resolve(reported_usage.clone(), context_usage.clone(), &runtime.session, &checkpoint_root).await;
                            send(&output, Update::Done(usage), deadline, &mut shutdown).await?;
                        }
                        deadline = tokio::time::Instant::now() + config.timeout;
                    }
                    Input::Acp(None) => return Err(ApiError::upstream("ACP event stream closed")),
                    Input::Acp(Some(Event::Exited(error))) => return Err(error),
                    Input::Acp(Some(Event::Notification(value))) => {
                        if value["sessionId"] != runtime.session || idle { continue; }
                        if repaired && (runtime.connection.tool_activity() != activity || runtime.bridge.as_ref().is_some_and(|bridge| bridge.tool_activity() != mcp_activity)) { return Err(ApiError::upstream("Cursor reported tool activity during final-answer recovery")); }
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
                        if idle { continue; }
                        let result = result?;
                        if result["stopReason"] != "end_turn" { return Err(ApiError::upstream(format!("Cursor ACP stopped: {}", result["stopReason"]))); }
                        if let Some(current) = Usage::reported(&result["usage"]) {
                            if let Some(previous) = reported_usage.as_mut() { previous.accumulate_reported(&current); }
                            else { reported_usage = Some(current); }
                        }
                        if !called { request.check_tool_choice(&[])?; }
                        if !pending.is_empty() { return Err(ApiError::upstream("ACP prompt ended with pending host tools")); }
                        if repaired && (runtime.connection.tool_activity() != activity || runtime.bridge.as_ref().is_some_and(|bridge| bridge.tool_activity() != mcp_activity)) { return Err(ApiError::upstream("Cursor reported tool activity during final-answer recovery")); }
                        if text.is_empty() && !called {
                            if saw_thought && !repaired && runtime.host_calls.is_empty() && runtime.connection.tool_activity() == activity && runtime.bridge.as_ref().is_none_or(|bridge| bridge.tool_activity() == mcp_activity) {
                                repaired = true;
                                runtime.connection.revoke_tools().await;
                                if let Some(bridge) = &runtime.bridge { bridge.end_turn()?; }
                                if runtime.connection.tool_activity() != activity || runtime.bridge.as_ref().is_some_and(|bridge| bridge.tool_activity() != mcp_activity) || !runtime.host_calls.is_empty() {
                                    return Err(ApiError::upstream("Tool activity arrived before final-answer recovery"));
                                }
                                if *shutdown.borrow() || outlet.as_ref().is_none_or(|output| output.is_closed()) || tokio::time::Instant::now() >= deadline {
                                    return Err(ApiError::upstream("Final-answer recovery was cancelled or timed out"));
                                }
                                tracing::warn!(parent: &span, "empty reply; retry");
                                runtime.connection.prompt(&runtime.session, FINAL_ANSWER_PROMPT)?;
                                continue;
                            }
                            return Err(ApiError::upstream("Cursor ACP ended the turn without reply text"));
                        }
                        let usage = usage::resolve(reported_usage.clone(), context_usage.clone(), &runtime.session, &checkpoint_root).await;
                        runtime.connection.revoke_tools().await;
                        if let Some(bridge) = &runtime.bridge { bridge.end_turn()?; }
                        self.remember(&mut mapping, &request, Some(&text)).await?;
                        if let Some(output) = outlet.take() { send(&output, Update::Done(usage), deadline, &mut shutdown).await?; }
                        if mapping.owner.is_none() { return Ok(()); }
                        request.messages = Vec::new();
                        request.tools = Vec::new();
                        request.tool_choice = None;
                        text = String::new();
                        span = Span::none();
                        idle = true;
                    }
                }
            }
        }.await;
        drop(controls);
        if result.is_err()
            && let Err(error) = runtime.connection.cancel(&runtime.session)
        {
            tracing::debug!(parent: &span, %error, "ACP cancel failed");
        }
        runtime.close().await;
        result
    }
}

use crate::{
    error::ApiError,
    tools::{FunctionCall, ToolCall, ToolCatalog},
};
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
};
use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
    },
    service::RequestContext,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub const SERVER_NAME: &str = "opencode";

pub struct HostCall {
    pub call: ToolCall,
    pub reply: oneshot::Sender<String>,
    turn: CancellationToken,
    cancelled: CancellationToken,
}

impl HostCall {
    pub fn is_closed(&self) -> bool {
        self.reply.is_closed() || self.turn.is_cancelled() || self.cancelled.is_cancelled()
    }

    pub async fn closed(&mut self) {
        tokio::select! {
            _ = self.reply.closed() => (),
            _ = self.turn.cancelled() => (),
            _ = self.cancelled.cancelled() => (),
        }
    }
}

#[derive(Clone)]
struct Tools {
    published: Arc<RwLock<Published>>,
    calls: mpsc::Sender<HostCall>,
    stop: CancellationToken,
    pending: Arc<Mutex<HashMap<String, CancellationToken>>>,
}

struct Published {
    definitions: Vec<Tool>,
    turn: Option<CancellationToken>,
}

impl Published {
    fn new(catalog: ToolCatalog) -> Result<Self, ApiError> {
        let mut definitions = Vec::new();
        for spec in catalog.tools {
            if spec.name.is_empty()
                || spec.name.len() > 128
                || !spec
                    .name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
            {
                return Err(ApiError::bad_request(
                    "Host tool name is not supported by MCP",
                ));
            }
            let Value::Object(schema) = spec.parameters else {
                return Err(ApiError::bad_request("MCP inputSchema must be an object"));
            };
            definitions.push(Tool::new(spec.name, spec.description, schema));
        }
        Ok(Self {
            definitions,
            turn: None,
        })
    }
}

impl ServerHandler for Tools {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult {
            tools: self
                .published
                .read()
                .map_err(|_| ErrorData::internal_error("MCP catalog lock poisoned", None))?
                .definitions
                .clone(),
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let id = serde_json::to_string(&context.id)
            .map_err(|error| ErrorData::internal_error(error.to_string(), None))?;
        let cancelled = self
            .pending
            .lock()
            .map_err(|_| ErrorData::internal_error("MCP pending call lock poisoned", None))?
            .get(&id)
            .cloned()
            .ok_or_else(|| ErrorData::internal_error("MCP request admission missing", None))?;
        let (name, turn) = {
            let published = self
                .published
                .read()
                .map_err(|_| ErrorData::internal_error("MCP catalog lock poisoned", None))?;
            let turn = published
                .turn
                .clone()
                .filter(|turn| !turn.is_cancelled())
                .ok_or_else(|| ErrorData::invalid_request("No active host turn", None))?;
            let name = published
                .definitions
                .iter()
                .find(|spec| spec.name == request.name)
                .map(|spec| spec.name.to_string())
                .ok_or_else(|| ErrorData::invalid_params("Unknown host tool", None))?;
            (name, turn)
        };
        let arguments = Value::Object(request.arguments.unwrap_or_default());
        let (reply, result) = oneshot::channel();
        let call = HostCall {
            call: ToolCall {
                id: format!("call_{}", Uuid::new_v4().simple()),
                kind: "function".to_owned(),
                function: FunctionCall {
                    name,
                    arguments: arguments.to_string(),
                },
            },
            reply,
            turn: turn.clone(),
            cancelled: cancelled.clone(),
        };
        tokio::select! {
            _ = turn.cancelled() => return Err(ErrorData::internal_error("Host turn ended", None)),
            _ = cancelled.cancelled() => return Err(ErrorData::internal_error("MCP call cancelled", None)),
            _ = context.ct.cancelled() => return Err(ErrorData::internal_error("MCP call cancelled", None)),
            _ = self.stop.cancelled() => return Err(ErrorData::internal_error("MCP bridge closed", None)),
            sent = self.calls.send(call) => sent.map_err(|_| ErrorData::internal_error("Host conversation closed", None))?,
        }
        tokio::select! {
            _ = turn.cancelled() => Err(ErrorData::internal_error("Host turn ended", None)),
            _ = cancelled.cancelled() => Err(ErrorData::internal_error("MCP call cancelled", None)),
            _ = context.ct.cancelled() => Err(ErrorData::internal_error("MCP call cancelled", None)),
            _ = self.stop.cancelled() => Err(ErrorData::internal_error("MCP bridge closed", None)),
            result = result => match result {
                Ok(text) => Ok(CallToolResult::success(vec![ContentBlock::text(text)]).into()),
                Err(_) => Ok(CallToolResult::error(vec![ContentBlock::text("Host tool result was cancelled")]).into()),
            }
        }
    }
}

pub struct Bridge {
    published: Arc<RwLock<Published>>,
    url: String,
    authorization: String,
    stop: CancellationToken,
    worker: Option<JoinHandle<()>>,
    tool_activity: Arc<AtomicUsize>,
}

#[derive(Clone)]
struct Access {
    expected: Arc<String>,
    pending: Arc<Mutex<HashMap<String, CancellationToken>>>,
    tool_activity: Arc<AtomicUsize>,
}

struct PendingCall {
    access: Access,
    id: String,
}

impl Drop for PendingCall {
    fn drop(&mut self) {
        if let Ok(mut pending) = self.access.pending.lock()
            && let Some(token) = pending.remove(&self.id)
        {
            token.cancel();
        }
    }
}

async fn authorize(State(access): State<Access>, request: Request, next: Next) -> Response {
    let expected = &access.expected;
    if request.headers().contains_key("Origin") {
        return StatusCode::FORBIDDEN.into_response();
    }
    let actual = request
        .headers()
        .get("Authorization")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if actual.len() != expected.len()
        || !actual
            .bytes()
            .zip(expected.bytes())
            .fold(0u8, |different, (a, b)| different | (a ^ b))
            .eq(&0)
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let (parts, body) = request.into_parts();
    let bytes = match to_bytes(body, 8 * 1024 * 1024).await {
        Ok(bytes) => bytes,
        Err(_) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
    };
    let value = serde_json::from_slice::<Value>(&bytes).ok();
    let mut admitted = None;
    if let Some(value) = &value {
        if value["method"] == "notifications/cancelled" {
            let id = value["params"]["requestId"].to_string();
            if let Ok(pending) = access.pending.lock()
                && let Some(token) = pending.get(&id)
            {
                token.cancel();
            }
            return StatusCode::ACCEPTED.into_response();
        }
        if value["method"] == "tools/call" {
            access.tool_activity.fetch_add(1, Ordering::Relaxed);
            let id = value["id"].to_string();
            let Ok(mut pending) = access.pending.lock() else {
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            };
            if pending.len() >= 16 {
                return StatusCode::TOO_MANY_REQUESTS.into_response();
            }
            if pending.contains_key(&id) {
                return StatusCode::CONFLICT.into_response();
            }
            pending.insert(id.clone(), CancellationToken::new());
            admitted = Some(PendingCall {
                access: access.clone(),
                id,
            });
        }
    }
    let response = next
        .run(Request::from_parts(parts, Body::from(bytes)))
        .await;
    drop(admitted);
    response
}

impl Bridge {
    pub async fn open(
        catalog: ToolCatalog,
        calls: mpsc::Sender<HostCall>,
    ) -> Result<Self, ApiError> {
        let published = Arc::new(RwLock::new(Published::new(catalog)?));
        let stop = CancellationToken::new();
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let tools = Tools {
            published: published.clone(),
            calls,
            stop: stop.clone(),
            pending: pending.clone(),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|error| ApiError::upstream(format!("Cannot bind internal MCP: {error}")))?;
        let address = listener
            .local_addr()
            .map_err(|error| ApiError::upstream(format!("Cannot read MCP address: {error}")))?;
        let authorization = format!("Bearer {}", Uuid::new_v4().simple());
        let tool_activity = Arc::new(AtomicUsize::new(0));
        let service = StreamableHttpService::new(
            move || Ok(tools.clone()),
            Arc::new(LocalSessionManager::default()),
            StreamableHttpServerConfig::default()
                .with_legacy_session_mode(false)
                .with_json_response(true)
                .with_cancellation_token(stop.clone())
                .with_max_request_body_bytes(8 * 1024 * 1024)
                .enforce_origin_validation(),
        );
        let router =
            Router::new()
                .nest_service("/mcp", service)
                .layer(middleware::from_fn_with_state(
                    Access {
                        expected: Arc::new(authorization.clone()),
                        pending,
                        tool_activity: tool_activity.clone(),
                    },
                    authorize,
                ));
        let shutdown = stop.clone();
        let worker = tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, router)
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await
            {
                tracing::warn!(%error, "internal MCP server stopped");
            }
        });
        Ok(Self {
            published,
            url: format!("http://{address}/mcp"),
            authorization,
            stop,
            worker: Some(worker),
            tool_activity,
        })
    }

    pub fn configuration(&self) -> Value {
        json!({"type":"http","name":SERVER_NAME,"url":self.url,"headers":[{"name":"Authorization","value":self.authorization}]})
    }

    pub fn replace_catalog(&self, catalog: ToolCatalog) -> Result<(), ApiError> {
        let replacement = Published::new(catalog)?;
        let mut published = self
            .published
            .write()
            .map_err(|_| ApiError::upstream("MCP catalog lock poisoned"))?;
        if let Some(turn) = published.turn.take() {
            turn.cancel();
        }
        *published = replacement;
        Ok(())
    }

    pub fn begin_turn(&self) -> Result<(), ApiError> {
        let mut published = self
            .published
            .write()
            .map_err(|_| ApiError::upstream("MCP catalog lock poisoned"))?;
        if let Some(turn) = published.turn.replace(CancellationToken::new()) {
            turn.cancel();
        }
        Ok(())
    }

    pub fn end_turn(&self) -> Result<(), ApiError> {
        if let Some(turn) = self
            .published
            .write()
            .map_err(|_| ApiError::upstream("MCP catalog lock poisoned"))?
            .turn
            .take()
        {
            turn.cancel();
        }
        Ok(())
    }

    pub fn tool_activity(&self) -> usize {
        self.tool_activity.load(Ordering::Relaxed)
    }

    pub async fn close(mut self) {
        self.stop.cancel();
        if let Some(mut worker) = self.worker.take()
            && tokio::time::timeout(Duration::from_secs(2), &mut worker)
                .await
                .is_err()
        {
            worker.abort();
        }
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

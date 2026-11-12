use crate::{
    chat::ExecutionMode,
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
        Arc, Mutex,
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

pub const SERVER_NAME: &str = "cursor_proxy";

pub struct HostCall {
    pub call: ToolCall,
    pub reply: oneshot::Sender<String>,
}

#[derive(Clone)]
struct Tools {
    mode: ExecutionMode,
    catalog: Arc<ToolCatalog>,
    definitions: Arc<Vec<Tool>>,
    calls: mpsc::Sender<HostCall>,
    stop: CancellationToken,
    pending: Arc<Mutex<HashMap<String, CancellationToken>>>,
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
            tools: self.definitions.as_ref().clone(),
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        if !self.mode.allows_tool(&request.name) {
            return Err(ErrorData::invalid_params(
                format!("Plan mode forbids host tool: {}", request.name),
                None,
            ));
        }
        let id = serde_json::to_string(&context.id)
            .map_err(|error| ErrorData::internal_error(error.to_string(), None))?;
        let cancelled = self
            .pending
            .lock()
            .map_err(|_| ErrorData::internal_error("MCP pending call lock poisoned", None))?
            .get(&id)
            .cloned()
            .ok_or_else(|| ErrorData::internal_error("MCP request admission missing", None))?;
        let spec = self
            .catalog
            .tools
            .iter()
            .find(|spec| spec.name == request.name)
            .ok_or_else(|| ErrorData::invalid_params("Unknown host tool", None))?;
        let arguments = Value::Object(request.arguments.unwrap_or_default());
        let validator = jsonschema::validator_for(&spec.parameters).map_err(|error| {
            ErrorData::internal_error(format!("Invalid host tool schema: {error}"), None)
        })?;
        if let Err(error) = validator.validate(&arguments) {
            return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "Invalid tool arguments: {error}"
            ))])
            .into());
        }
        let (reply, result) = oneshot::channel();
        let call = HostCall {
            call: ToolCall {
                id: format!("call_{}", Uuid::new_v4().simple()),
                kind: "function".to_owned(),
                function: FunctionCall {
                    name: spec.name.clone(),
                    arguments: arguments.to_string(),
                },
            },
            reply,
        };
        tokio::select! {
            _ = cancelled.cancelled() => return Err(ErrorData::internal_error("MCP call cancelled", None)),
            _ = context.ct.cancelled() => return Err(ErrorData::internal_error("MCP call cancelled", None)),
            _ = self.stop.cancelled() => return Err(ErrorData::internal_error("MCP bridge closed", None)),
            sent = self.calls.send(call) => sent.map_err(|_| ErrorData::internal_error("Host conversation closed", None))?,
        }
        tokio::select! {
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
        mut catalog: ToolCatalog,
        calls: mpsc::Sender<HostCall>,
        mode: ExecutionMode,
    ) -> Result<Self, ApiError> {
        catalog.tools.retain(|tool| mode.allows_tool(&tool.name));
        let mut definitions = Vec::new();
        for spec in &catalog.tools {
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
            let schema = spec
                .parameters
                .as_object()
                .ok_or_else(|| ApiError::bad_request("MCP inputSchema must be an object"))?;
            definitions.push(Tool::new(
                spec.name.clone(),
                spec.description.clone(),
                schema.clone(),
            ));
        }
        let stop = CancellationToken::new();
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let tools = Tools {
            mode,
            catalog: Arc::new(catalog),
            definitions: Arc::new(definitions),
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

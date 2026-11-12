use crate::{
    agent::AgentConfig,
    chat::{ChatRequest, sse},
    error::ApiError,
    models::ModelCache,
    session::SessionStore,
};
use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, State, rejection::JsonRejection},
    http::{HeaderMap, Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc};
use tokio::sync::watch;

pub struct AppState {
    pub workspace: PathBuf,
    pub agent: AgentConfig,
    pub token: Option<String>,
    pub models: ModelCache,
    pub sessions: Arc<SessionStore>,
    pub shutdown: watch::Receiver<bool>,
}

pub fn router(state: Arc<AppState>) -> Router {
    let api = Router::new()
        .route("/v1/models", get(models))
        .route("/models", get(models))
        .route("/v1/chat/completions", post(completions))
        .route("/chat/completions", post(completions))
        .fallback(|| async {
            ApiError::new(StatusCode::NOT_FOUND, "not_found", "Unsupported endpoint")
        })
        .layer(middleware::from_fn_with_state(state.clone(), authorize));
    Router::new()
        .route("/health", get(health))
        .merge(api)
        .layer(DefaultBodyLimit::max(8 * 1024 * 1024))
        .with_state(state)
}

async fn authorize(
    State(state): State<Arc<AppState>>,
    request: Request<Body>,
    next: Next,
) -> Response {
    if let Some(token) = &state.token {
        let expected = format!("Bearer {token}");
        let actual = request
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if !same_token(actual.as_bytes(), expected.as_bytes()) {
            return ApiError::new(
                StatusCode::UNAUTHORIZED,
                "invalid_proxy_token",
                "Missing or invalid local proxy token",
            )
            .into_response();
        }
    }
    next.run(request).await
}

fn same_token(actual: &[u8], expected: &[u8]) -> bool {
    let mut difference = actual.len() ^ expected.len();
    for (index, byte) in expected.iter().enumerate() {
        difference |= usize::from(*byte ^ actual.get(index).copied().unwrap_or(0));
    }
    difference == 0
}

async fn health(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(json!({"ok":true, "workspace":state.workspace}))
}

async fn models(State(state): State<Arc<AppState>>) -> Result<Json<Value>, ApiError> {
    let mut shutdown = state.shutdown.clone();
    if *shutdown.borrow() {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "shutting_down",
            "Proxy is shutting down",
        ));
    }
    tokio::select! {
        _ = shutdown.changed() => Err(ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "shutting_down", "Proxy is shutting down")),
        models = state.models.list() => Ok(Json(crate::models::openai(&models))),
    }
}

async fn completions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    payload: Result<Json<ChatRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    if *state.shutdown.borrow() {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "shutting_down",
            "Proxy is shutting down",
        ));
    }
    let Json(request) =
        payload.map_err(|e| ApiError::new(e.status(), "invalid_request_error", e.body_text()))?;
    request.validate()?;
    request.catalog()?;
    let workspace = resolve_workspace(&headers, &state.workspace).await?;
    let model = request.model();
    let stream = request.stream;
    let id = sse::new_id();
    let rx = state.sessions.clone().start(
        state.agent.clone(),
        request,
        workspace,
        state.shutdown.clone(),
        id.clone(),
    );
    sse::respond(rx, stream, model, id).await
}

async fn resolve_workspace(
    headers: &HeaderMap,
    fallback: &std::path::Path,
) -> Result<PathBuf, ApiError> {
    let workspace = if let Some(value) = headers.get("x-opencode-directory") {
        let raw = value
            .to_str()
            .map_err(|_| ApiError::bad_request("x-opencode-directory must be UTF-8"))?;
        let decoded = percent_encoding::percent_decode_str(raw)
            .decode_utf8()
            .map_err(|_| ApiError::bad_request("x-opencode-directory contains invalid UTF-8"))?;
        if decoded.is_empty() || decoded.contains('\0') {
            return Err(ApiError::bad_request(
                "x-opencode-directory must name a directory",
            ));
        }
        let path = PathBuf::from(decoded.as_ref());
        if !path.is_absolute() {
            return Err(ApiError::bad_request(
                "x-opencode-directory must be absolute",
            ));
        }
        path
    } else {
        fallback.to_owned()
    };
    let workspace = tokio::fs::canonicalize(workspace)
        .await
        .map_err(|e| ApiError::bad_request(format!("Cannot resolve workspace: {e}")))?;
    let metadata = tokio::fs::metadata(&workspace)
        .await
        .map_err(|e| ApiError::bad_request(format!("Cannot read workspace: {e}")))?;
    if !metadata.is_dir() {
        return Err(ApiError::bad_request("Workspace must be a directory"));
    }
    Ok(workspace)
}

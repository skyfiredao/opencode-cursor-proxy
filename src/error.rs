use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};

#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_request_error", message)
    }

    pub fn upstream(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, "cursor_agent_error", message)
    }

    pub fn from_agent(message: &str) -> Self {
        let lower = message.to_lowercase();
        if [
            "not logged in",
            "unauthenticated",
            "authentication required",
            "authentication failed",
            "invalid api key",
            "unauthorized",
            "login required",
            "please log in",
            "please login",
            "token expired",
            "token has expired",
            "cursor-agent login",
        ]
        .iter()
        .any(|needle| lower.contains(needle))
        {
            Self::new(
                StatusCode::UNAUTHORIZED,
                "cursor_auth_required",
                "cursor-agent authentication failed. Run cursor-agent login as the proxy user.",
            )
        } else if lower.contains("cannot use this model") || lower.contains("model not found") {
            Self::bad_request(message.chars().take(2000).collect::<String>())
        } else {
            Self::upstream(message.chars().take(2000).collect::<String>())
        }
    }

    pub fn body(&self) -> Value {
        json!({"error": {"message": self.message, "type": self.code, "code": self.code}})
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ApiError {}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(self.body())).into_response()
    }
}

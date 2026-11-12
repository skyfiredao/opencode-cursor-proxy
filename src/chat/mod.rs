mod mode;
pub mod prompt;
pub mod sse;

pub use mode::ExecutionMode;

use crate::{
    error::ApiError,
    tools::{ToolCall, ToolCatalog},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Deserialize)]
pub struct ChatRequest {
    pub model: Option<String>,
    #[serde(rename = "cursorModel")]
    pub cursor_model: Option<String>,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub tools: Vec<Value>,
    pub tool_choice: Option<Value>,
    #[serde(default)]
    pub stream: bool,
    pub n: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Message {
    pub role: String,
    #[serde(default)]
    pub content: Value,
    pub name: Option<String>,
    pub tool_call_id: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
}

impl Message {
    pub fn text(&self) -> String {
        match &self.content {
            Value::String(text) => text.clone(),
            Value::Array(parts) => parts
                .iter()
                .filter_map(|p| {
                    (p.get("type").and_then(Value::as_str) == Some("text"))
                        .then(|| p.get("text").and_then(Value::as_str))
                        .flatten()
                })
                .collect::<Vec<_>>()
                .join("\n"),
            Value::Null => String::new(),
            other => other.to_string(),
        }
    }
}

impl ChatRequest {
    pub fn model(&self) -> String {
        let name = self
            .cursor_model
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .or(self.model.as_deref())
            .unwrap_or("auto")
            .trim();
        let name = name.strip_prefix("cursor-acp/").unwrap_or(name);
        if name.is_empty() {
            "auto".to_owned()
        } else {
            name.to_owned()
        }
    }

    pub fn catalog(&self) -> Result<ToolCatalog, ApiError> {
        let mut catalog = ToolCatalog::parse(&self.tools)?;
        if self.tool_choice.as_ref().and_then(Value::as_str) == Some("none") {
            return Ok(ToolCatalog::default());
        }
        let mode = self.execution_mode();
        if let Some(name) = self
            .tool_choice
            .as_ref()
            .and_then(|choice| choice.pointer("/function/name"))
            .and_then(Value::as_str)
        {
            if !catalog.tools.iter().any(|tool| tool.name == name) {
                return Err(ApiError::bad_request(
                    "tool_choice must name an advertised tool",
                ));
            }
            if !mode.allows_tool(name) {
                return Err(ApiError::bad_request(format!(
                    "Plan mode forbids tool_choice for {name}"
                )));
            }
            catalog.tools.retain(|tool| tool.name == name);
        }
        catalog.tools.retain(|tool| mode.allows_tool(&tool.name));
        if mode == ExecutionMode::Plan
            && catalog.tools.is_empty()
            && self.tool_choice.as_ref().and_then(Value::as_str) == Some("required")
        {
            return Err(ApiError::bad_request(
                "Plan mode has no permitted tools for tool_choice=required",
            ));
        }
        Ok(catalog)
    }

    pub fn validate(&self) -> Result<(), ApiError> {
        if self.messages.is_empty() {
            return Err(ApiError::bad_request("messages must not be empty"));
        }
        if self.n.is_some_and(|n| n != 1) {
            return Err(ApiError::bad_request("Only n=1 is supported"));
        }
        for message in &self.messages {
            if !matches!(
                message.role.as_str(),
                "system" | "developer" | "user" | "assistant" | "tool"
            ) {
                return Err(ApiError::bad_request(format!(
                    "Unsupported role: {}",
                    message.role
                )));
            }
            match &message.content {
                Value::Null | Value::String(_) => (),
                Value::Array(parts)
                    if parts.iter().all(|p| {
                        p.get("type").and_then(Value::as_str) == Some("text")
                            && p.get("text").is_some_and(Value::is_string)
                    }) => {}
                _ => {
                    return Err(ApiError::bad_request(
                        "Only text message content is supported; multimodal parts are not silently discarded",
                    ));
                }
            }
            if message.role == "tool" && message.tool_call_id.as_deref().is_none_or(str::is_empty) {
                return Err(ApiError::bad_request("Tool results require tool_call_id"));
            }
        }
        if let Some(choice) = &self.tool_choice {
            let valid = matches!(choice.as_str(), Some("none" | "auto" | "required"))
                || choice
                    .pointer("/function/name")
                    .is_some_and(Value::is_string);
            if !valid {
                return Err(ApiError::bad_request("Unsupported tool_choice"));
            }
        }
        Ok(())
    }

    pub fn check_tool_choice(&self, calls: &[ToolCall]) -> Result<(), ApiError> {
        let Some(choice) = &self.tool_choice else {
            return Ok(());
        };
        if choice.as_str() == Some("required") && calls.is_empty() {
            return Err(ApiError::upstream(
                "cursor-agent did not honor tool_choice=required",
            ));
        }
        if let Some(name) = choice.pointer("/function/name").and_then(Value::as_str)
            && (calls.is_empty() || calls.iter().any(|c| c.function.name != name))
        {
            return Err(ApiError::upstream(format!(
                "cursor-agent did not honor tool_choice for {name}"
            )));
        }
        Ok(())
    }
}

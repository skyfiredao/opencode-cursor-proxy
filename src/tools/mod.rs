use crate::{chat::Message, error::ApiError};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type", default = "function_type")]
    pub kind: String,
    pub function: FunctionCall,
}

fn function_type() -> String {
    "function".to_owned()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug, Clone, Default)]
pub struct ToolCatalog {
    pub tools: Vec<ToolSpec>,
}

impl ToolCatalog {
    pub fn parse(raw: &[Value]) -> Result<Self, ApiError> {
        let mut tools: Vec<ToolSpec> = Vec::new();
        for value in raw {
            let function = value.get("function").unwrap_or(value);
            let name = function
                .get("name")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| {
                    ApiError::bad_request("Every tool needs a nonempty function.name")
                })?;
            if tools.iter().any(|t| t.name == name) {
                return Err(ApiError::bad_request(format!("Duplicate tool: {name}")));
            }
            let parameters = function
                .get("parameters")
                .cloned()
                .unwrap_or_else(|| json!({"type":"object"}));
            if !parameters.is_object() {
                return Err(ApiError::bad_request(format!(
                    "Tool parameters for {name} must be an object"
                )));
            }
            tools.push(ToolSpec {
                name: name.to_owned(),
                description: function
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                parameters,
            });
        }
        Ok(Self { tools })
    }
}

pub fn render_history(messages: &[Message]) -> String {
    let mut names = std::collections::HashMap::new();
    let mut lines = Vec::new();
    for message in messages {
        if message.role == "tool" {
            let id = message.tool_call_id.as_deref().unwrap_or("unknown");
            let name = message.name.as_deref().or_else(|| names.get(id).copied());
            let body = message
                .content
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| message.content.to_string());
            let label = name.map(|n| format!("name: {n}, ")).unwrap_or_default();
            lines.push(format!("TOOL_RESULT ({label}call_id: {id}): {body}"));
            continue;
        }
        let mut text = message.text();
        for call in &message.tool_calls {
            names.insert(call.id.as_str(), call.function.name.as_str());
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&format!(
                "tool_call(id: {}, name: {}, args: {})",
                call.id, call.function.name, call.function.arguments
            ));
        }
        if !text.is_empty() {
            lines.push(format!("{}: {text}", message.role.to_uppercase()));
        }
    }
    if messages.last().is_some_and(|m| m.role == "tool") {
        lines.push("The above tool calls have been executed. Continue your response based on these results.".to_owned());
    }
    lines.join("\n\n")
}

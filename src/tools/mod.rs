mod alias;
mod bridge_json;
mod loop_guard;
mod schema_compat;

use crate::{chat::Message, error::ApiError};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub use bridge_json::BridgeBuffer;
pub use loop_guard::check_loop;

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

#[derive(Debug, Clone)]
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
            jsonschema::validator_for(&parameters)
                .map_err(|e| ApiError::bad_request(format!("Invalid schema for {name}: {e}")))?;
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

    pub fn resolve(&self, raw: &str) -> Option<&ToolSpec> {
        let base = raw.strip_suffix("ToolCall").unwrap_or(raw);
        self.tools
            .iter()
            .find(|t| t.name == base)
            .or_else(|| {
                self.tools
                    .iter()
                    .find(|t| alias::key(&t.name) == alias::key(base))
            })
            .or_else(|| {
                let remapped = base.strip_prefix("mcp__")?.replacen("__", "_", 1);
                self.tools
                    .iter()
                    .find(|t| alias::key(&t.name) == alias::key(&remapped))
            })
            .or_else(|| {
                self.tools
                    .iter()
                    .find(|t| alias::family(&t.name) == alias::family(base))
            })
    }

    pub fn normalize(&self, id: &str, name: &str, raw_args: Value) -> Result<ToolCall, ApiError> {
        let mut spec = self.resolve(name).ok_or_else(|| {
            ApiError::upstream(format!("Cursor requested unadvertised tool: {name}"))
        })?;
        let args = if let Value::String(text) = raw_args {
            serde_json::from_str(&text).map_err(|_| {
                ApiError::upstream(format!("Invalid JSON arguments for {}", spec.name))
            })?
        } else {
            raw_args
        };

        if alias::family(&spec.name) == "edit"
            && let Some(full_file) = schema_compat::full_file_write(&args)
            && let Some(write) = self.resolve("write")
        {
            spec = write;
            return self.normalize(id, &spec.name, full_file);
        }
        let args = schema_compat::normalize(&spec.name, args, &spec.parameters)?;
        let validator = jsonschema::validator_for(&spec.parameters)
            .map_err(|e| ApiError::bad_request(format!("Invalid schema for {}: {e}", spec.name)))?;
        if let Err(error) = validator.validate(&args) {
            return Err(ApiError::upstream(format!(
                "Invalid arguments for {}: {error}",
                spec.name
            )));
        }
        Ok(ToolCall {
            id: id.to_owned(),
            kind: function_type(),
            function: FunctionCall {
                name: spec.name.clone(),
                arguments: args.to_string(),
            },
        })
    }

    pub fn intercept(
        &self,
        payload: &Value,
        name: Option<&str>,
        id: &str,
    ) -> Result<Option<ToolCall>, ApiError> {
        let Some(map) = payload.as_object() else {
            return Err(ApiError::upstream(
                "Malformed cursor-agent tool_call payload",
            ));
        };
        let entry = map.iter().find(|(k, v)| {
            v.is_object()
                && (k.ends_with("ToolCall") || v.get("args").is_some() || v.get("input").is_some())
        });
        let Some((raw_name, body)) = entry else {
            return Ok(None);
        };
        let args = body.get("args").or_else(|| body.get("input"));
        let Some(args) = args else {
            return Ok(None);
        };
        self.normalize(id, name.unwrap_or(raw_name), args.clone())
            .map(Some)
    }

    pub fn prompt(&self) -> String {
        if self.tools.is_empty() {
            return String::new();
        }
        let mut prompt = String::from(
            "SYSTEM: OpenCode owns the tools below. Do not execute Cursor's native tools. \
             To call a host tool, respond with exactly one JSON object and no prose: \
             {\"name\":\"advertised tool name\",\"arguments\":{...}}. \
             You may return an array of these envelopes for parallel calls. \
             Use the exact parameter schema below. Tool results in this conversation \
             were already executed by OpenCode; continue from their results.\n\nAvailable tools:\n",
        );
        for spec in &self.tools {
            prompt.push_str(&format!(
                "- {}: {}\n  Parameters: {}\n",
                spec.name, spec.description, spec.parameters
            ));
        }
        if let Some(write) = self.resolve("write") {
            prompt.push_str(&format!(
                "\nFor a complete-file write, use the {} envelope with the complete file content. \
                 Do not call Cursor's native edit or write tools. Never invent missing replacement text.", write.name));
        }
        prompt
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

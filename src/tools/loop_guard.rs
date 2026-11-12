use super::{ToolCall, alias::family};
use crate::{chat::Message, error::ApiError};
use serde_json::Value;
use std::collections::HashMap;

fn arguments(call: &ToolCall) -> Value {
    serde_json::from_str(&call.function.arguments)
        .unwrap_or_else(|_| Value::String(call.function.arguments.clone()))
}

fn error_class(text: &str) -> &'static str {
    let text = text.to_lowercase();
    if text.contains("permission denied") || text.contains("not allowed") {
        "permission"
    } else if text.contains("not found") || text.contains("no such file") {
        "not_found"
    } else if text.contains("validation")
        || text.contains("invalid argument")
        || text.contains("required field")
    {
        "validation"
    } else if text.contains("timeout") || text.contains("timed out") {
        "timeout"
    } else if text.contains("error") || text.contains("failed") {
        "tool_error"
    } else {
        "success"
    }
}

pub fn check_loop(messages: &[Message], candidate: &ToolCall) -> Result<(), ApiError> {
    let mut by_id = HashMap::new();
    let mut failures = 0;
    let mut same_successes = 0;
    let mut coarse_failures = 0;
    for message in messages {
        for call in &message.tool_calls {
            by_id.insert(call.id.as_str(), call);
        }
        if message.role != "tool" {
            continue;
        }
        let Some(call) = message.tool_call_id.as_deref().and_then(|id| by_id.get(id)) else {
            continue;
        };
        if call.function.name != candidate.function.name {
            continue;
        }
        let class = error_class(&message.text());
        let same = arguments(call) == arguments(candidate);
        if class == "success" {
            failures = 0;
            coarse_failures = 0;
            if same {
                same_successes += 1;
            } else {
                same_successes = 0;
            }
        } else {
            coarse_failures += 1;
            if same {
                failures += 1;
            }
        }
    }
    let exploratory = matches!(
        family(&candidate.function.name).as_str(),
        "read" | "grep" | "glob" | "ls" | "stat" | "bash" | "webfetch" | "task"
    );
    let success_limit = if exploratory { 10 } else { 2 };
    if failures >= 2 || same_successes >= success_limit || (!exploratory && coarse_failures >= 6) {
        return Err(ApiError::upstream(format!(
            "Stopped repetitive tool loop for {}",
            candidate.function.name
        )));
    }
    Ok(())
}

use super::{ToolCall, ToolCatalog};
use crate::error::ApiError;
use serde_json::Value;

#[derive(Default)]
pub struct BridgeBuffer {
    pending: String,
    passthrough: bool,
}

impl BridgeBuffer {
    pub fn push(&mut self, text: &str) -> Option<String> {
        if self.passthrough {
            return Some(text.to_owned());
        }
        self.pending.push_str(text);
        let trimmed = self.pending.trim_start();
        if !trimmed.is_empty() && !trimmed.starts_with(['{', '[', '`']) {
            self.passthrough = true;
            return Some(std::mem::take(&mut self.pending));
        }
        None
    }

    pub fn finish(
        &mut self,
        catalog: &ToolCatalog,
        id_prefix: &str,
    ) -> Result<(String, Vec<ToolCall>), ApiError> {
        let text = std::mem::take(&mut self.pending);
        if self.passthrough {
            return Ok((text, Vec::new()));
        }
        let trimmed = text.trim();
        let candidate = if let Some(fenced) = trimmed.strip_prefix("```") {
            match fenced.split_once('\n') {
                Some((info, body))
                    if info.trim().is_empty() || info.trim().eq_ignore_ascii_case("json") =>
                {
                    body.trim().strip_suffix("```").map(str::trim)
                }
                _ => None,
            }
        } else {
            Some(trimmed)
        };
        let value = candidate.and_then(|candidate| serde_json::from_str::<Value>(candidate).ok());
        let Some(value) = value else {
            return Ok((text, Vec::new()));
        };
        let envelopes: Vec<&Value> = if let Some(array) = value.as_array() {
            array.iter().collect()
        } else {
            vec![&value]
        };
        if envelopes.is_empty()
            || !envelopes.iter().all(|e| {
                e.as_object().is_some_and(|map| {
                    map.len() == 2 && map.contains_key("name") && map.contains_key("arguments")
                }) && e.get("name").is_some_and(Value::is_string)
                    && e.get("arguments").is_some_and(Value::is_object)
            })
        {
            return Ok((text, Vec::new()));
        }
        let mut calls = Vec::new();
        for (index, envelope) in envelopes.into_iter().enumerate() {
            let Some(name) = envelope.get("name").and_then(Value::as_str) else {
                continue;
            };
            calls.push(catalog.normalize(
                &format!("call_{id_prefix}_{index}"),
                name,
                envelope["arguments"].clone(),
            )?);
        }
        Ok((String::new(), calls))
    }
}

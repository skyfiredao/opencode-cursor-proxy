use super::{ChatRequest, Message};
use crate::{
    error::ApiError,
    tools::{ToolCatalog, render_history},
};
use std::{io::ErrorKind, path::Path};
use tokio::io::AsyncReadExt;

pub(crate) async fn load_workspace_instructions(
    request: &mut ChatRequest,
    workspace: &Path,
) -> Result<Option<String>, ApiError> {
    const LIMIT: u64 = 1024 * 1024;
    let path = workspace.join("AGENTS.md");
    let metadata = match tokio::fs::metadata(&path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(ApiError::bad_request(format!(
                "Cannot inspect workspace AGENTS.md: {error}"
            )));
        }
    };
    if !metadata.is_file() {
        return Err(ApiError::bad_request(
            "Workspace AGENTS.md must be a regular file",
        ));
    }
    let file = tokio::fs::File::open(&path).await.map_err(|error| {
        ApiError::bad_request(format!("Cannot open workspace AGENTS.md: {error}"))
    })?;
    let mut bytes = Vec::new();
    file.take(LIMIT + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| {
            ApiError::bad_request(format!("Cannot read workspace AGENTS.md: {error}"))
        })?;
    if bytes.len() as u64 > LIMIT {
        return Err(ApiError::bad_request("Workspace AGENTS.md exceeds 1 MiB"));
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| ApiError::bad_request("Workspace AGENTS.md must be UTF-8"))?;
    if text.trim().is_empty() {
        return Ok(None);
    }
    let instructions = format!("Workspace instructions from {}:\n\n{text}", path.display());
    request.messages.insert(
        0,
        Message {
            role: "developer".to_owned(),
            content: serde_json::Value::String(instructions.clone()),
            name: None,
            tool_call_id: None,
            tool_calls: Vec::new(),
        },
    );
    Ok(Some(instructions))
}

pub fn full(request: &ChatRequest, catalog: &ToolCatalog) -> String {
    let mut parts = Vec::new();
    let tools = catalog.prompt();
    if !tools.is_empty() {
        parts.push(tools);
    }
    if let Some(choice) = &request.tool_choice {
        if choice.as_str() == Some("required") {
            parts.push(
                "SYSTEM: You must call at least one of the advertised host tools.".to_owned(),
            );
        }
        if let Some(name) = choice
            .pointer("/function/name")
            .and_then(serde_json::Value::as_str)
        {
            parts.push(format!(
                "SYSTEM: You must call the advertised host tool {name}."
            ));
        }
    }
    parts.push(render_history(&request.messages));
    parts.join("\n\n")
}

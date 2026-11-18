use crate::{
    agent::{Process, cursor_command, output_preview},
    chat::sse::unix_time,
    error::ApiError,
};
use axum::http::StatusCode;
use serde_json::{Value, json};
use std::{
    path::Path,
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{
    io::AsyncReadExt,
    sync::{RwLock, watch},
};

const REFRESH_INTERVAL: Duration = Duration::from_secs(3600);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Model {
    pub id: String,
    pub name: String,
}

#[derive(Default)]
pub struct ModelCache {
    cached: RwLock<Option<Vec<Model>>>,
    version: crate::opencode::Version,
}

pub struct RefreshResult {
    pub models: usize,
    pub config_changed: bool,
}

impl ModelCache {
    pub fn new(version: crate::opencode::Version) -> Self {
        Self {
            version,
            ..Self::default()
        }
    }

    pub async fn list(&self) -> Vec<Model> {
        self.cached.read().await.clone().unwrap_or_else(fallback)
    }

    pub async fn refresh(
        &self,
        binary: &Path,
        config: &Path,
        base_url: &str,
        token: Option<&str>,
    ) -> Result<RefreshResult, ApiError> {
        let mut models = discover(binary).await?;
        models.sort_by(|left, right| left.id.cmp(&right.id));
        if self.cached.read().await.as_ref() == Some(&models) {
            return Ok(RefreshResult {
                models: models.len(),
                config_changed: false,
            });
        }
        let path = config.to_owned();
        let base_url = base_url.to_owned();
        let token = token.map(str::to_owned);
        let version = self.version;
        let (models, changed) = tokio::task::spawn_blocking(move || {
            crate::config::configure_for(version, &path, Some(&base_url), token.as_deref(), &models)
                .map(|configured| (models, configured.changed))
                .map_err(|error| error.to_string())
        })
        .await
        .map_err(|error| ApiError::upstream(format!("Model publication task failed: {error}")))?
        .map_err(|error| {
            ApiError::upstream(format!("Cannot synchronize OpenCode models: {error}"))
        })?;
        let count = models.len();
        *self.cached.write().await = Some(models);
        Ok(RefreshResult {
            models: count,
            config_changed: changed,
        })
    }

    pub async fn maintain(
        &self,
        binary: &Path,
        config: &Path,
        base_url: &str,
        token: Option<&str>,
        mut shutdown: watch::Receiver<bool>,
    ) {
        if *shutdown.borrow() {
            return;
        }
        let mut timer = tokio::time::interval(REFRESH_INTERVAL);
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = shutdown.changed() => break,
                _ = timer.tick() => (),
            }
            let started = Instant::now();
            tracing::debug!("models refresh");
            let result = tokio::select! {
                _ = shutdown.changed() => break,
                result = self.refresh(binary, config, base_url, token) => result,
            };
            match result {
                Ok(result) => {
                    if result.config_changed {
                        tracing::info!(result = "pass", count = result.models, ms = %started.elapsed().as_millis(), config = "updated", "models");
                    } else {
                        tracing::info!(result = "pass", count = result.models, ms = %started.elapsed().as_millis(), "models");
                    }
                }
                Err(error) => {
                    let result = match error.code {
                        "cursor_cli_unavailable" => "no-agent",
                        "cursor_auth_required" => "auth",
                        "cursor_model_catalog_error" => "catalog",
                        _ => "error",
                    };
                    tracing::warn!(result, error = ?error.message, ms = %started.elapsed().as_millis(), "models")
                }
            }
        }
    }
}

pub fn parse(output: &str) -> Vec<Model> {
    output
        .lines()
        .filter_map(|line| {
            let (id, name) = line.trim().split_once(" - ")?;
            if id.is_empty() || id.chars().any(|c| c.is_whitespace() || c.is_control()) {
                return None;
            }
            Some(Model {
                id: id.to_owned(),
                name: name.trim().trim_end_matches(" (default)").to_owned(),
            })
        })
        .collect()
}

pub async fn discover(binary: &Path) -> Result<Vec<Model>, ApiError> {
    let operation = async {
        tracing::debug!(home = ?std::env::var_os("HOME"), userprofile = ?std::env::var_os("USERPROFILE"), cursor_config_dir = ?std::env::var_os("CURSOR_CONFIG_DIR"), xdg_config_home = ?std::env::var_os("XDG_CONFIG_HOME"), appdata = ?std::env::var_os("APPDATA"), localappdata = ?std::env::var_os("LOCALAPPDATA"), force_color = ?std::env::var_os("FORCE_COLOR"), no_color = ?std::env::var_os("NO_COLOR"), api_key_present = std::env::var_os("CURSOR_API_KEY").is_some(), api_key_removed = true, "models environment");
        tracing::debug!(path = ?std::env::var_os("PATH").map(|value| output_preview(value.to_string_lossy().as_bytes())), pathext = ?std::env::var_os("PATHEXT"), comspec = ?std::env::var_os("ComSpec"), systemroot = ?std::env::var_os("SystemRoot"), term = ?std::env::var_os("TERM"), term_program = ?std::env::var_os("TERM_PROGRAM"), "models search environment");
        let mut command = cursor_command(binary).map_err(|mut error| {
            error.code = "cursor_cli_unavailable";
            error
        })?;
        command
            .arg("--list-models")
            .env_remove("CURSOR_API_KEY")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        tracing::debug!(agent = %binary.display(), program = ?command.as_std().get_program(), args = ?command.as_std().get_args().collect::<Vec<_>>(), cwd = ?std::env::current_dir(), "models command");
        let mut process = Process::spawn(&mut command).map_err(|mut error| {
            error.code = "cursor_cli_unavailable";
            error
        })?;
        let stdout = process
            .child
            .stdout
            .take()
            .ok_or_else(|| ApiError::upstream("Missing model discovery stdout"))?;
        let stderr = process
            .child
            .stderr
            .take()
            .ok_or_else(|| ApiError::upstream("Missing model discovery stderr"))?;
        let read = |stream: &'static str, reader: Box<dyn tokio::io::AsyncRead + Unpin + Send>| async move {
            let mut bytes = Vec::new();
            reader
                .take(1024 * 1024 + 1)
                .read_to_end(&mut bytes)
                .await
                .map_err(|e| ApiError::upstream(format!("Cannot read model catalog: {e}")))?;
            tracing::debug!(stream, bytes = bytes.len(), utf8_valid = std::str::from_utf8(&bytes).is_ok(), ansi = bytes.contains(&0x1b), nul = bytes.contains(&0), truncated = bytes.len() > 4096, preview = ?output_preview(&bytes), "models output");
            if bytes.len() > 1024 * 1024 {
                return Err(ApiError::upstream("Model catalog output exceeds 1 MiB"));
            }
            String::from_utf8(bytes)
                .map_err(|_| ApiError::upstream("Model catalog is not valid UTF-8"))
        };
        let (output, errors) = tokio::try_join!(
            read("stdout", Box::new(stdout)),
            read("stderr", Box::new(stderr))
        )?;
        let status = process
            .child
            .wait()
            .await
            .map_err(|e| ApiError::upstream(format!("Cannot wait for model catalog: {e}")))?;
        tracing::debug!(exit_code = ?status.code(), status = %status, "models exit");
        if !status.success() {
            return Err(ApiError::from_agent(&errors));
        }
        let mut models = parse(&output);
        let stdout_count = models.len();
        if models.is_empty() {
            models = parse(&errors);
        }
        tracing::debug!(
            count = models.len(),
            stdout_count,
            stderr_used = stdout_count == 0,
            "models parsed"
        );
        if models.is_empty() {
            return Err(ApiError::new(
                StatusCode::BAD_GATEWAY,
                "cursor_model_catalog_error",
                "cursor-agent returned an empty or unrecognized model catalog",
            ));
        }
        Ok(models)
    };
    tokio::time::timeout(Duration::from_secs(20), operation)
        .await
        .map_err(|_| ApiError::upstream("cursor-agent model discovery timed out"))?
}

pub fn fallback() -> Vec<Model> {
    vec![
        Model {
            id: "auto".to_owned(),
            name: "Auto".to_owned(),
        },
        Model {
            id: "composer-2.5".to_owned(),
            name: "Composer 2.5".to_owned(),
        },
    ]
}

pub fn openai(models: &[Model]) -> Value {
    json!({"object":"list", "data":models.iter().map(|m| {
        json!({"id":m.id,"object":"model","created":unix_time(),"owned_by":"cursor"})
    }).collect::<Vec<_>>()})
}

pub fn provider_config(
    models: &[Model],
    base_url: &str,
    token: Option<&str>,
    provider: &str,
) -> Value {
    provider_config_for(
        crate::opencode::Version::V2,
        models,
        base_url,
        token,
        provider,
    )
}

pub fn provider_config_for(
    version: crate::opencode::Version,
    models: &[Model],
    base_url: &str,
    token: Option<&str>,
    provider: &str,
) -> Value {
    let inventory: serde_json::Map<String, Value> = models
        .iter()
        .map(|m| {
            (
                m.id.clone(),
                match version {
                    crate::opencode::Version::V1 => json!({"name":m.name,"tool_call":true,"reasoning":true,
                        "interleaved":{"field":"reasoning_content"},"modalities":{"input":["text"],"output":["text"]}}),
                    crate::opencode::Version::V2 => json!({"name":m.name,
                        "capabilities":{"tools":true,"input":["text"],"output":["text"]}}),
                },
            )
        })
        .collect();
    let mut providers = serde_json::Map::new();
    let definition = match version {
        crate::opencode::Version::V1 => {
            json!({"npm":"@ai-sdk/openai-compatible","name":"Cursor Proxy",
            "options":{"baseURL":base_url,"apiKey":token.unwrap_or("cursor-agent")},"models":inventory})
        }
        crate::opencode::Version::V2 => json!({
        "package":"@opencode/ai/providers/openai-compatible", "name":"Cursor Proxy",
        "compatibility":{"reasoningField":"reasoning_content"},
        "settings":{"baseURL":base_url,"apiKey":token.unwrap_or("cursor-agent")}, "models":inventory
        }),
    };
    providers.insert(provider.to_owned(), definition);
    json!({"$schema":"https://opencode.ai/config.json", (version.providers()):providers})
}

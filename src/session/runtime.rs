use super::{
    database::{Mapping, now},
    hash,
};
use crate::{
    acp::Connection,
    agent::AgentConfig,
    chat::{ChatRequest, ExecutionMode},
    error::ApiError,
    mcp::{Bridge, HostCall, SERVER_NAME},
    tools::ToolCatalog,
};
use serde_json::{Value, json};
use tokio::sync::mpsc;

pub(super) struct Runtime {
    pub connection: Connection,
    pub bridge: Option<Bridge>,
    pub host_calls: mpsc::Receiver<HostCall>,
    pub session: String,
    pub barrier: Option<(&'static str, Value)>,
    calls: mpsc::Sender<HostCall>,
    metadata: Value,
    catalog: String,
}

fn control(
    metadata: &Value,
    session: &str,
    mode: ExecutionMode,
) -> Result<Option<(&'static str, Value)>, ApiError> {
    let mode = if mode == ExecutionMode::Plan {
        "plan"
    } else {
        "agent"
    };
    if let Some(option) = metadata["configOptions"]
        .as_array()
        .and_then(|options| options.iter().find(|option| option["id"] == "mode"))
    {
        if option["type"] != "select"
            || !option["options"].as_array().is_some_and(|options| {
                options.iter().any(|option| {
                    option["value"] == mode
                        || option["options"]
                            .as_array()
                            .is_some_and(|group| group.iter().any(|option| option["value"] == mode))
                })
            })
        {
            return Err(ApiError::upstream(format!(
                "Cursor ACP did not advertise requested mode: {mode}"
            )));
        }
        Ok(Some((
            "session/set_config_option",
            json!({"sessionId":session,"configId":"mode","value":mode}),
        )))
    } else if let Some(modes) = metadata["modes"]["availableModes"].as_array() {
        if !modes.iter().any(|option| option["id"] == mode) {
            return Err(ApiError::upstream(format!(
                "Cursor ACP did not advertise requested mode: {mode}"
            )));
        }
        Ok(Some((
            "session/set_mode",
            json!({"sessionId":session,"modeId":mode}),
        )))
    } else if mode == "plan" {
        Err(ApiError::upstream("Cursor ACP did not advertise Plan mode"))
    } else {
        Ok(None)
    }
}

impl Runtime {
    pub async fn open(
        config: &AgentConfig,
        mapping: &mut Mapping,
        request: &ChatRequest,
        deadline: tokio::time::Instant,
    ) -> Result<(Self, bool), ApiError> {
        let connection = Connection::open(config, &mapping.workspace, &mapping.model).await?;
        let (calls, host_calls) = mpsc::channel(16);
        let mut runtime = Self {
            connection,
            bridge: None,
            host_calls,
            session: String::new(),
            barrier: None,
            calls,
            metadata: Value::Null,
            catalog: String::new(),
        };
        runtime.catalog(request.catalog()?).await?;
        let restored = runtime
            .establish(config, mapping, request, deadline)
            .await?;
        Ok((runtime, restored))
    }

    async fn catalog(&mut self, catalog: ToolCatalog) -> Result<(), ApiError> {
        let key = hash(json!(catalog.tools).to_string());
        if let Some(bridge) = &self.bridge {
            bridge.replace_catalog(catalog)?;
        } else if !catalog.tools.is_empty() {
            if self.connection.capabilities["mcpCapabilities"]["http"] != true {
                return Err(ApiError::upstream("Cursor ACP does not support HTTP MCP"));
            }
            self.bridge = Some(Bridge::open(catalog, self.calls.clone()).await?);
        }
        self.catalog = key;
        Ok(())
    }

    fn servers(&self) -> Vec<Value> {
        self.bridge
            .as_ref()
            .map(Bridge::configuration)
            .into_iter()
            .collect()
    }

    async fn configure(&mut self, request: &ChatRequest) -> Result<(), ApiError> {
        let catalog = request.catalog()?;
        self.connection
            .allow_tools(
                &self.session,
                SERVER_NAME,
                catalog.tools.into_iter().map(|tool| tool.name),
                request.execution_mode(),
            )
            .await;
        self.barrier = control(&self.metadata, &self.session, request.execution_mode())?;
        if let Some((method, params)) = &self.barrier {
            self.connection.call(method, params.clone()).await?;
        }
        // Loading replays history. The mode response is an ordered boundary before the next prompt.
        self.connection.take_pending()?;
        Ok(())
    }

    async fn replace_connection(
        &mut self,
        config: &AgentConfig,
        mapping: &Mapping,
    ) -> Result<(), ApiError> {
        let replacement = Connection::open(config, &mapping.workspace, &mapping.model).await?;
        let old = std::mem::replace(&mut self.connection, replacement);
        old.close().await;
        Ok(())
    }

    async fn establish(
        &mut self,
        config: &AgentConfig,
        mapping: &mut Mapping,
        request: &ChatRequest,
        deadline: tokio::time::Instant,
    ) -> Result<bool, ApiError> {
        if !mapping.cursor_id.is_empty() && self.connection.capabilities["loadSession"] == true {
            let budget = deadline.saturating_duration_since(tokio::time::Instant::now()) / 3;
            let result = tokio::time::timeout(budget, async {
                self.metadata = self.connection.load_session(json!({"sessionId":mapping.cursor_id,"cwd":mapping.workspace,"mcpServers":self.servers()})).await?;
                self.session = mapping.cursor_id.clone();
                self.configure(request).await
            }).await.unwrap_or_else(|_| Err(ApiError::upstream("Cursor session restoration timed out")));
            match result {
                Ok(()) => {
                    tracing::info!(cursor = %self.session, "load");
                    return Ok(true);
                }
                Err(error) => {
                    tracing::warn!(cursor = %mapping.cursor_id, error = ?error.message, "load failed; new")
                }
            }
            self.replace_connection(config, mapping).await?;
        }
        let response = self
            .connection
            .call_ordered(
                "session/new",
                json!({"cwd":mapping.workspace,"mcpServers":self.servers()}),
            )
            .await?;
        self.session = response.value()["sessionId"]
            .as_str()
            .filter(|id| !id.is_empty() && id.len() <= 256)
            .ok_or_else(|| ApiError::upstream("Cursor ACP returned an invalid session ID"))?
            .to_owned();
        self.connection
            .allow_tools(
                &self.session,
                SERVER_NAME,
                request.catalog()?.tools.into_iter().map(|tool| tool.name),
                request.execution_mode(),
            )
            .await;
        self.metadata = response.apply();
        self.configure(request).await?;
        mapping.cursor_id = self.session.clone();
        mapping.created_at = now();
        mapping.ready = false;
        tracing::info!(cursor = %self.session, "new");
        Ok(false)
    }

    pub async fn prepare(
        &mut self,
        config: &AgentConfig,
        mapping: &mut Mapping,
        request: &ChatRequest,
        skip: usize,
        deadline: tokio::time::Instant,
    ) -> Result<usize, ApiError> {
        let catalog = request.catalog()?;
        let changed = self.catalog != hash(json!(catalog.tools).to_string());
        if changed {
            self.catalog(catalog).await?;
        }
        if self.connection.take_pending().is_err() {
            self.replace_connection(config, mapping).await?;
            return Ok(
                if self.establish(config, mapping, request, deadline).await? {
                    skip
                } else {
                    0
                },
            );
        }
        if changed {
            let budget = deadline.saturating_duration_since(tokio::time::Instant::now()) / 3;
            let loaded = if self.connection.capabilities["loadSession"] == true {
                tokio::time::timeout(budget, self.connection.load_session(json!({"sessionId":self.session,"cwd":mapping.workspace,"mcpServers":self.servers()}))).await
                    .unwrap_or_else(|_| Err(ApiError::upstream("Cursor tool-session reload timed out")))
            } else {
                Err(ApiError::upstream(
                    "Cursor cannot refresh the existing MCP session",
                ))
            };
            match loaded {
                Ok(metadata) => {
                    if metadata.get("configOptions").is_some() || metadata.get("modes").is_some() {
                        self.metadata = metadata;
                    }
                }
                Err(error) => {
                    tracing::warn!(cursor = %self.session, error = ?error.message, "reload failed; new");
                    mapping.ready = false;
                    mapping.cursor_id.clear();
                    self.replace_connection(config, mapping).await?;
                    self.establish(config, mapping, request, deadline).await?;
                    return Ok(0);
                }
            }
        }
        if let Err(error) = self.configure(request).await {
            tracing::warn!(cursor = %self.session, error = ?error.message, "reconnect");
            self.replace_connection(config, mapping).await?;
            return Ok(
                if self.establish(config, mapping, request, deadline).await? {
                    skip
                } else {
                    0
                },
            );
        }
        Ok(skip)
    }

    pub async fn close(self) {
        self.connection.close().await;
        if let Some(bridge) = self.bridge {
            bridge.close().await;
        }
    }
}

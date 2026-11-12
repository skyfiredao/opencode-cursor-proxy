use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, Deserialize)]
pub struct AgentEvent {
    #[serde(flatten)]
    pub kind: EventKind,
    pub session_id: Option<String>,
    pub timestamp_ms: Option<u64>,
    pub model_call_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum EventKind {
    #[serde(rename = "system")]
    System {},
    #[serde(rename = "user")]
    User {},
    #[serde(rename = "assistant")]
    Assistant { message: Value },
    #[serde(rename = "thinking")]
    Thinking {
        subtype: Option<String>,
        text: Option<String>,
    },
    #[serde(rename = "tool_call")]
    ToolCall {
        subtype: Option<String>,
        call_id: Option<String>,
        tool_call: Value,
        name: Option<String>,
    },
    #[serde(rename = "result")]
    Result {
        #[serde(default)]
        is_error: bool,
        subtype: Option<String>,
        result: Option<String>,
        usage: Option<Usage>,
    },
    #[serde(rename = "error")]
    Error {
        message: Option<String>,
        error: Option<Value>,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    #[serde(default, alias = "input_tokens")]
    pub input_tokens: u64,
    #[serde(default, alias = "output_tokens")]
    pub output_tokens: u64,
    #[serde(default, alias = "cache_read_tokens")]
    pub cache_read_tokens: u64,
    #[serde(default, alias = "cache_write_tokens")]
    pub cache_write_tokens: u64,
    #[serde(default, alias = "reasoning_tokens")]
    pub reasoning_tokens: u64,
    #[serde(skip)]
    pub context_window: Option<u64>,
    #[serde(skip)]
    pub context_snapshot: bool,
}

impl Usage {
    pub fn accumulate_reported(&mut self, other: &Self) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.reasoning_tokens = self.reasoning_tokens.saturating_add(other.reasoning_tokens);
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(other.cache_read_tokens);
        self.cache_write_tokens = self
            .cache_write_tokens
            .saturating_add(other.cache_write_tokens);
    }

    pub fn from_context(used: u64, size: u64) -> Option<Self> {
        (size > 0 && used <= 9_007_199_254_740_991 && size <= 9_007_199_254_740_991).then_some(
            Self {
                input_tokens: used,
                context_window: Some(size),
                context_snapshot: true,
                ..Self::default()
            },
        )
    }

    pub fn context_update(update: &Value) -> Option<Self> {
        if update["sessionUpdate"] != "usage_update" {
            return None;
        }
        Self::from_context(update["used"].as_u64()?, update["size"].as_u64()?)
    }

    pub fn reported(value: &Value) -> Option<Self> {
        let fields = value.as_object()?;
        fields
            .keys()
            .any(|key| {
                matches!(
                    key.as_str(),
                    "inputTokens"
                        | "input_tokens"
                        | "outputTokens"
                        | "output_tokens"
                        | "cacheReadTokens"
                        | "cache_read_tokens"
                        | "cacheWriteTokens"
                        | "cache_write_tokens"
                        | "reasoningTokens"
                        | "reasoning_tokens"
                )
            })
            .then(|| serde_json::from_value(value.clone()).ok())
            .flatten()
    }

    pub fn openai(&self) -> Value {
        let input = self
            .input_tokens
            .saturating_add(self.cache_read_tokens)
            .saturating_add(self.cache_write_tokens);
        let mut usage = json!({
            "prompt_tokens": input,
            "completion_tokens": self.output_tokens,
            "total_tokens": input.saturating_add(self.output_tokens),
            "prompt_tokens_details": {
                "cached_tokens": self.cache_read_tokens,
                "cache_write_tokens": self.cache_write_tokens
            },
            "completion_tokens_details": {"reasoning_tokens": self.reasoning_tokens}
        });
        if self.context_snapshot {
            usage["cursor_proxy"] =
                json!({"source":"context_snapshot", "context_window":self.context_window});
        }
        usage
    }
}

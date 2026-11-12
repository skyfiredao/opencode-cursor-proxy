use super::{ChatRequest, Message};

const PLAN_PREFIX: &str = concat!(
    "You are in Plan mode. Discuss the plan with the user directly in the conversation. ",
    "Do not create or update plan files unless the user explicitly asks you to; ",
    "when they do, write them only in:\n"
);
const PLAN_SUFFIX: &str = concat!(
    "\n\nDo not modify any other files or ask a subagent to do so.\n\n",
    "You remain in Plan mode until the user switches agents. ",
    "If the user asks you to implement changes, do not do so. ",
    "Tell them they need to switch agents."
);
const BUILD_REMINDER: &str = concat!(
    "You are NO LONGER in Plan mode. The previous Plan restrictions no longer apply. ",
    "Any Plan mode instructions from earlier in this conversation are no longer active."
);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionMode {
    Build,
    Plan,
}

impl ExecutionMode {
    pub fn allows_tool(self, name: &str) -> bool {
        self == Self::Build || matches!(name, "read" | "glob" | "grep" | "question")
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Build => "build",
            Self::Plan => "plan",
        }
    }
}

impl Message {
    pub(crate) fn mode_reminder(&self) -> Option<ExecutionMode> {
        if self.role != "user" {
            return None;
        }
        let text = self.text().replace("\r\n", "\n");
        let body = text
            .trim()
            .strip_prefix("<system-reminder>")?
            .strip_suffix("</system-reminder>")?
            .trim();
        if body == BUILD_REMINDER {
            return Some(ExecutionMode::Build);
        }
        let directory = body
            .strip_prefix(PLAN_PREFIX)?
            .strip_suffix(PLAN_SUFFIX)?
            .trim();
        if directory.is_empty() || directory.contains('\n') {
            return None;
        }
        Some(ExecutionMode::Plan)
    }
}

impl ChatRequest {
    pub fn execution_mode(&self) -> ExecutionMode {
        self.messages
            .iter()
            .rev()
            .find_map(Message::mode_reminder)
            .unwrap_or(ExecutionMode::Build)
    }
}

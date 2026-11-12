use crate::{error::ApiError, stream::Usage, tools::ToolCall};
#[cfg(windows)]
use std::process::Stdio;
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::process::{Child, Command};

#[derive(Debug)]
pub enum Update {
    Text(String),
    Thinking(String),
    Tools(Vec<ToolCall>),
    Done(Option<Usage>),
    Error(ApiError),
}

#[derive(Clone)]
pub struct AgentConfig {
    pub binary: PathBuf,
    pub timeout: Duration,
    pub trusted_workspace: Option<PathBuf>,
    pub load_skills: bool,
}

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[cfg(windows)]
fn windows_system_tool(relative: &str) -> PathBuf {
    std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
        .join("System32")
        .join(relative)
}

#[cfg(windows)]
fn windows_cli_path(binary: &Path, name: &str) -> Result<PathBuf, ApiError> {
    let mut candidates = Vec::new();
    if binary.components().count() > 1 || binary.is_absolute() {
        candidates.push(binary.to_owned());
    } else {
        if let Some(paths) = std::env::var_os("PATH") {
            for directory in std::env::split_paths(&paths) {
                if binary.extension().is_some() {
                    candidates.push(directory.join(binary));
                } else {
                    for extension in ["exe", "ps1", "cmd"] {
                        candidates.push(directory.join(binary).with_extension(extension));
                    }
                }
            }
        }
        if binary == Path::new("cursor-agent")
            && let Some(local) = std::env::var_os("LOCALAPPDATA")
        {
            let directory = PathBuf::from(local).join("cursor-agent");
            for extension in ["exe", "ps1", "cmd"] {
                candidates.push(directory.join("cursor-agent").with_extension(extension));
            }
        }
    }
    let selected = candidates
        .into_iter()
        .find(|path| path.is_file())
        .ok_or_else(|| {
            ApiError::upstream(format!("{name} CLI not found; install it or check PATH"))
        })?;
    std::path::absolute(selected)
        .map_err(|error| ApiError::upstream(format!("Cannot resolve {name} CLI path: {error}")))
}

pub(crate) fn cursor_command(binary: &Path) -> Result<Command, ApiError> {
    cli_command(binary, "Cursor")
}

pub(crate) fn cli_command(binary: &Path, _name: &str) -> Result<Command, ApiError> {
    #[cfg(windows)]
    {
        let launcher = windows_cli_path(binary, _name)?;
        let extension = launcher
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("");
        let script = if extension.eq_ignore_ascii_case("cmd") {
            let script = launcher.with_extension("ps1");
            if !script.is_file() {
                return Err(ApiError::upstream(format!(
                    "{_name} .cmd launcher needs its sibling .ps1 script; use a native .exe or reinstall the CLI"
                )));
            }
            Some(script)
        } else if extension.eq_ignore_ascii_case("ps1") {
            Some(launcher.clone())
        } else if extension.eq_ignore_ascii_case("exe") || extension.eq_ignore_ascii_case("com") {
            None
        } else {
            return Err(ApiError::upstream(format!(
                "Unsupported Windows {_name} launcher; use a .exe, .ps1, or .cmd with a sibling .ps1"
            )));
        };
        if let Some(script) = script {
            let mut command = Command::new(windows_system_tool(
                r"WindowsPowerShell\v1.0\powershell.exe",
            ));
            command
                .args([
                    "-NoLogo",
                    "-NoProfile",
                    "-NonInteractive",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-File",
                ])
                .arg(script);
            Ok(command)
        } else {
            Ok(Command::new(launcher))
        }
    }
    #[cfg(not(windows))]
    {
        Ok(Command::new(binary))
    }
}

pub struct Process {
    pub child: Child,
    pid: Option<u32>,
}

impl Process {
    pub fn spawn(command: &mut Command) -> Result<Self, ApiError> {
        #[cfg(unix)]
        command.process_group(0);
        #[cfg(windows)]
        command.creation_flags(CREATE_NO_WINDOW);
        let child = command.kill_on_drop(true).spawn().map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                ApiError::upstream(
                    "cursor-agent binary not found; set --cursor-agent to its installed path",
                )
            } else {
                ApiError::upstream(format!("Cannot start cursor-agent: {error}"))
            }
        })?;
        let pid = child.id();
        Ok(Self { child, pid })
    }

    pub fn terminate(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.pid.take().and_then(|pid| i32::try_from(pid).ok()) {
            use nix::{
                sys::signal::{Signal, killpg},
                unistd::Pid,
            };
            if let Err(error) = killpg(Pid::from_raw(pid), Signal::SIGKILL)
                && error != nix::errno::Errno::ESRCH
            {
                tracing::warn!(%error, pid, "could not kill cursor-agent process group");
            }
        }
        #[cfg(windows)]
        if let Some(pid) = self.pid.take()
            && self.child.id().is_some()
        {
            use std::os::windows::process::CommandExt;
            let result = std::process::Command::new(windows_system_tool("taskkill.exe"))
                .args(["/PID", &pid.to_string(), "/T", "/F"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .creation_flags(CREATE_NO_WINDOW)
                .status();
            match result {
                Ok(status) if status.success() => (),
                Ok(status) => {
                    tracing::debug!(%status, pid, "taskkill failed or cursor-agent already exited")
                }
                Err(error) => {
                    tracing::warn!(%error, pid, "could not terminate cursor-agent process tree")
                }
            }
        }
        if let Err(error) = self.child.start_kill() {
            tracing::debug!(%error, "cursor-agent already exited or could not be killed");
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.terminate();
    }
}

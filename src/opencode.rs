#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Version {
    V1,
    #[default]
    V2,
}

impl Version {
    pub fn parse(output: &str) -> Result<Self, String> {
        let value = output.trim();
        let value = value.strip_prefix("opencode ").unwrap_or(value);
        let value = value.strip_prefix('v').unwrap_or(value);
        let mut sections = value.splitn(2, ['-', '+']);
        let core = sections.next().unwrap_or_default();
        if let Some(suffix) = sections.next()
            && (suffix.is_empty()
                || !suffix
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+')))
        {
            return Err("Unrecognized OpenCode version suffix".to_owned());
        }
        let components: Vec<_> = core.split('.').collect();
        if components.len() != 3
            || components
                .iter()
                .any(|part| part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()))
        {
            return Err("Unrecognized OpenCode version".to_owned());
        }
        match components[0] {
            "1" => Ok(Self::V1),
            "2" => Ok(Self::V2),
            _ => Err("Unsupported OpenCode major version".to_owned()),
        }
    }

    pub fn providers(self) -> &'static str {
        match self {
            Self::V1 => "provider",
            Self::V2 => "providers",
        }
    }
    pub fn settings(self) -> &'static str {
        match self {
            Self::V1 => "options",
            Self::V2 => "settings",
        }
    }
    pub fn other(self) -> Self {
        match self {
            Self::V1 => Self::V2,
            Self::V2 => Self::V1,
        }
    }
}

pub async fn detect(binary: &std::path::Path) -> Result<Version, String> {
    use crate::agent::{Process, cli_command};
    use std::{process::Stdio, time::Duration};
    use tokio::io::AsyncReadExt;
    let operation = async {
        let mut command = cli_command(binary, "OpenCode")
            .map_err(|_| "Cannot resolve OpenCode CLI".to_owned())?;
        command
            .arg("--version")
            .env_remove("CURSOR_API_KEY")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut process = Process::spawn(&mut command)
            .map_err(|_| "Cannot start OpenCode version command".to_owned())?;
        let stdout = process
            .child
            .stdout
            .take()
            .ok_or("Missing OpenCode version stdout")?;
        let stderr = process
            .child
            .stderr
            .take()
            .ok_or("Missing OpenCode version stderr")?;
        let read = |reader: Box<dyn tokio::io::AsyncRead + Unpin + Send>| async move {
            let mut output = Vec::new();
            reader
                .take(8193)
                .read_to_end(&mut output)
                .await
                .map_err(|_| "Cannot read OpenCode version output".to_owned())?;
            if output.len() > 8192 {
                return Err("OpenCode version output exceeds 8 KiB".to_owned());
            }
            Ok(output)
        };
        let (output, _) = tokio::try_join!(read(Box::new(stdout)), read(Box::new(stderr)))?;
        let status = process
            .child
            .wait()
            .await
            .map_err(|_| "Cannot wait for OpenCode version command".to_owned())?;
        if !status.success() {
            return Err("OpenCode version command failed".to_owned());
        }
        Version::parse(
            std::str::from_utf8(&output).map_err(|_| "OpenCode version is not UTF-8".to_owned())?,
        )
    };
    tokio::time::timeout(Duration::from_secs(2), operation)
        .await
        .map_err(|_| "OpenCode version command timed out".to_owned())?
}

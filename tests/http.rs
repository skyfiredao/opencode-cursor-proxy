#![cfg(unix)]

use cursor_proxy::{
    agent::{AgentConfig, Update},
    chat::ChatRequest,
    models::ModelCache,
    server::{self, AppState},
};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
    sync::watch,
};

static SEQUENCE: AtomicU64 = AtomicU64::new(1);

struct MockAgent {
    directory: PathBuf,
    binary: PathBuf,
}

impl MockAgent {
    fn new(body: &str) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let directory = std::env::temp_dir().join(format!(
            "cursor-proxy-test-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&directory).expect("new test directory");
        let binary = directory.join("agent");
        let legacy = directory.join("legacy-agent");
        std::fs::write(&legacy, format!("#!/bin/sh\n{body}\n")).expect("mock script");
        std::fs::set_permissions(&legacy, std::fs::Permissions::from_mode(0o700))
            .expect("legacy permissions");
        let adapter = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/acp-agent.py");
        std::os::unix::fs::symlink(adapter, &binary).expect("stable mock launcher");
        let opencode = directory.join("opencode");
        std::fs::write(&opencode, "#!/bin/sh\necho 2.2.7\n").expect("OpenCode version fixture");
        std::fs::set_permissions(&opencode, std::fs::Permissions::from_mode(0o700))
            .expect("OpenCode fixture mode");
        Self { directory, binary }
    }
}

impl Drop for MockAgent {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.directory) {
            eprintln!("mock directory cleanup: {error}");
        }
    }
}

async fn request(
    mock: &MockAgent,
    path: &str,
    body: Option<Value>,
    token: Option<&str>,
    authorization: Option<&str>,
) -> String {
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let models = ModelCache::default();
    if matches!(path, "/v1/models" | "/models") {
        if let Err(error) = models
            .refresh(
                &mock.binary,
                &mock.directory.join("model-config.jsonc"),
                "http://127.0.0.1:32124/v1",
                token,
            )
            .await
        {
            eprintln!("mock model discovery: {error}");
        }
    }
    let state = Arc::new(AppState {
        workspace: mock.directory.clone(),
        agent: AgentConfig {
            binary: mock.binary.clone(),
            timeout: Duration::from_secs(3),
            trusted_workspace: None,
            load_skills: true,
        },
        token: token.map(str::to_owned),
        models,
        sessions: Arc::new(cursor_proxy::session::SessionStore::default()),
        shutdown: shutdown_rx,
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move { axum::serve(listener, server::router(state)).await });
    let mut socket = TcpStream::connect(address).await.expect("connect");
    let method = if body.is_some() { "POST" } else { "GET" };
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    let auth = authorization
        .map(|s| format!("Authorization: {s}\r\n"))
        .unwrap_or_default();
    let wire = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n{auth}\r\n{body}",
        body.len()
    );
    socket.write_all(wire.as_bytes()).await.expect("request");
    let mut output = Vec::new();
    tokio::time::timeout(Duration::from_secs(8), socket.read_to_end(&mut output))
        .await
        .expect("response timeout")
        .expect("read response");
    shutdown_tx.send(true).expect("shutdown");
    server.abort();
    let _stopped = server.await;
    String::from_utf8(output).expect("response UTF-8")
}

fn chat(stream: bool) -> Value {
    json!({"model":"composer-2.5", "stream":stream,"messages":[{"role":"user","content":"Reply with exactly: PONG"}]})
}

fn fixture_script(fixture: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(fixture);
    format!("cat >/dev/null\ncat '{}'", path.display())
}

async fn cli_chat(
    mock: &MockAgent,
    explicit_workspace: bool,
    directory_header: Option<&str>,
    load_skills: bool,
) -> String {
    cli_chat_logged(mock, explicit_workspace, directory_header, load_skills)
        .await
        .0
}

async fn cli_chat_logged(
    mock: &MockAgent,
    explicit_workspace: bool,
    directory_header: Option<&str>,
    load_skills: bool,
) -> (String, String) {
    let reservation = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("reserve port");
    let address = reservation.local_addr().expect("address");
    drop(reservation);
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_cursor-proxy"));
    command
        .args(["--host", "127.0.0.1", "--port"])
        .arg(address.port().to_string())
        .arg("--cursor-agent")
        .arg(&mock.binary)
        .current_dir(&mock.directory)
        .env("RUST_LOG", "info")
        .env(
            "PATH",
            std::env::join_paths(std::iter::once(mock.directory.clone()).chain(
                std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
            ))
            .expect("fixture PATH"),
        )
        .env(
            "CURSOR_PROXY_FIXTURE_LEGACY",
            mock.directory.join("legacy-agent"),
        )
        .env("XDG_CONFIG_HOME", mock.directory.join("config"))
        .env_remove("OPENCODE_CONFIG_DIR")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    if explicit_workspace {
        command.arg("--workspace").arg(&mock.directory);
    }
    if load_skills {
        command.arg("--load-skills");
    }
    let mut child = command.spawn().expect("start proxy CLI");
    let mut stderr = BufReader::new(child.stderr.take().expect("proxy stderr"));
    let mut socket = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            assert!(
                child.try_wait().expect("proxy status").is_none(),
                "proxy exited during startup"
            );
            match TcpStream::connect(address).await {
                Ok(socket) => break socket,
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(error) => panic!("connect proxy: {error}"),
            }
        }
    })
    .await
    .expect("proxy readiness");
    let mut logs = String::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let mut line = String::new();
            if stderr.read_line(&mut line).await.expect("startup log line") == 0 {
                break;
            }
            let refreshed =
                line.contains("model catalog refreshed") || line.contains("model refresh failed");
            logs.push_str(&line);
            if refreshed {
                break;
            }
        }
    })
    .await
    .expect("startup refresh deadline");
    let body = chat(false).to_string();
    let directory = directory_header
        .map(|path| format!("x-opencode-directory: {path}\r\n"))
        .unwrap_or_default();
    let wire = format!(
        "POST /v1/chat/completions HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n{directory}\r\n{body}",
        body.len()
    );
    socket.write_all(wire.as_bytes()).await.expect("send chat");
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(8), socket.read_to_end(&mut response))
        .await
        .expect("chat deadline")
        .expect("read response");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let mut line = String::new();
            if stderr.read_line(&mut line).await.expect("proxy log line") == 0 {
                break;
            }
            let replied = line.contains("chat reply");
            logs.push_str(&line);
            if replied {
                break;
            }
        }
    })
    .await
    .expect("reply log deadline");
    child.start_kill().expect("stop proxy");
    child.wait().await.expect("reap proxy");
    stderr
        .read_to_string(&mut logs)
        .await
        .expect("remaining proxy logs");
    (String::from_utf8(response).expect("HTTP response"), logs)
}

#[tokio::test]
async fn cli_automatically_selects_and_updates_the_opencode_configuration() {
    for (json, jsonc, selected) in [
        (false, false, "opencode.jsonc"),
        (true, false, "opencode.json"),
        (false, true, "opencode.jsonc"),
        (true, true, "opencode.jsonc"),
    ] {
        let mock = MockAgent::new(&format!(
            "if [ \"$1\" = --list-models ]; then\n  printf 'composer-2.5 - Composer 2.5\\nother-model - Other Model\\n'\n  exit 0\nfi\n{}",
            fixture_script("text.ndjson")
        ));
        let directory = mock.directory.join("config/opencode");
        std::fs::create_dir_all(&directory).expect("configuration directory");
        let original_json = "{\"model\":\"other/current\"}\n";
        let original_jsonc = "{\n// preserve user settings\n\"model\":\"other/current\"\n}\n";
        if json {
            std::fs::write(directory.join("opencode.json"), original_json).expect("JSON config");
        }
        if jsonc {
            std::fs::write(directory.join("opencode.jsonc"), original_jsonc).expect("JSONC config");
        }
        let response = cli_chat(&mock, true, None, false).await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        let output =
            std::fs::read_to_string(directory.join(selected)).expect("automatic configuration");
        let value = cursor_proxy::config::parse(&output).expect("configured JSONC");
        assert_eq!(
            value["providers"]["cursor-proxy"]["models"]
                .as_object()
                .expect("model inventory")
                .len(),
            2
        );
        assert_eq!(
            value["providers"]["cursor-proxy"]["models"]["other-model"]["capabilities"]["tools"],
            true
        );
        if json || jsonc {
            assert_eq!(value["model"], "other/current");
        }
        if jsonc {
            assert!(output.contains("// preserve user settings"));
        }
        if json && jsonc {
            assert_eq!(
                std::fs::read_to_string(directory.join("opencode.json"))
                    .expect("lower priority configuration"),
                original_json
            );
        }
    }
}

#[tokio::test]
async fn cli_logs_timestamped_request_and_reply_with_the_same_id() {
    for (script, outcome) in [
        (fixture_script("text.ndjson"), "outcome=\"completed\""),
        (
            "cat >/dev/null\necho 'not logged in' >&2\nexit 1".to_owned(),
            "outcome=\"cursor_auth_required\"",
        ),
    ] {
        let mock = MockAgent::new(&script);
        let (_, logs) = cli_chat_logged(&mock, true, None, false).await;
        let requests: Vec<_> = logs
            .lines()
            .filter(|line| line.contains("chat request"))
            .collect();
        let replies: Vec<_> = logs
            .lines()
            .filter(|line| line.contains("chat reply"))
            .collect();
        assert_eq!(requests.len(), 1, "{logs}");
        assert_eq!(replies.len(), 1, "{logs}");
        for line in [requests[0], replies[0]] {
            let timestamp = line.split_whitespace().next().expect("timestamp");
            assert!(
                timestamp.contains('T') && timestamp.ends_with('Z'),
                "{line}"
            );
        }
        let request_id = requests[0]
            .split_whitespace()
            .find(|field| field.starts_with("request_id="))
            .expect("request ID");
        assert!(replies[0].contains(request_id), "{logs}");
        assert!(replies[0].contains(outcome), "{logs}");
        assert!(replies[0].contains("elapsed_ms="), "{logs}");
        assert!(!logs.contains("Reply with exactly: PONG"), "{logs}");
    }
}

#[tokio::test]
async fn cli_workspace_is_implicitly_trusted_without_force_or_yolo() {
    let mock = MockAgent::new(&format!(
        r#"trusted=false
for argument in "$@"; do
  case "$argument" in
    --trust) trusted=true ;;
    --force|--yolo|-f) echo 'unexpected permission bypass' >&2; exit 1 ;;
  esac
done
if [ "$trusted" != true ]; then
  echo 'Workspace Trust Required: --trust missing' >&2
  exit 1
fi
{}"#,
        fixture_script("text.ndjson")
    ));
    for explicit_workspace in [true, false] {
        let response = cli_chat(&mock, explicit_workspace, None, false).await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.contains("\"content\":\"PONG\""), "{response}");
    }
    let same_directory = mock.directory.join(".");
    let response = cli_chat(&mock, true, same_directory.to_str(), false).await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
}

#[tokio::test]
async fn cli_workspace_trust_does_not_extend_to_header_override() {
    let mock = MockAgent::new(&format!(
        r#"for argument in "$@"; do
  case "$argument" in
    --trust|--force|--yolo|-f) echo 'unexpected implicit trust' >&2; exit 1 ;;
  esac
done
{}"#,
        fixture_script("text.ndjson")
    ));
    let other_workspace = MockAgent::new("exit 9");
    for explicit_workspace in [true, false] {
        let response = cli_chat(
            &mock,
            explicit_workspace,
            other_workspace.directory.to_str(),
            false,
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    }
}

#[tokio::test]
async fn cli_does_not_load_workspace_agents_by_default() {
    let mock = MockAgent::new(&format!(
        r#"prompt=$(cat)
case "$prompt" in
  *'WORKSPACE_RULE_ALPHA'*|*'Workspace instructions from '*)
    echo 'workspace instructions loaded without --load-skills' >&2; exit 1 ;;
esac
{}"#,
        fixture_script("text.ndjson")
    ));
    let instructions = mock.directory.join("AGENTS.md");
    for contents in [b"WORKSPACE_RULE_ALPHA".to_vec(), vec![0xff]] {
        std::fs::write(&instructions, contents).expect("workspace instructions");
        let response = cli_chat(&mock, true, None, false).await;
        std::fs::remove_file(&instructions).expect("remove workspace instructions");
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    }
}

#[tokio::test]
async fn workspace_agents_are_injected_and_reloaded_without_cli_file_reads() {
    let mock = MockAgent::new(&format!(
        r#"prompt=$(cat)
case "$prompt" in
  *'DEVELOPER: Workspace instructions from '*'/AGENTS.md:'*) ;;
  *) echo 'missing workspace instructions in stdin' >&2; exit 1 ;;
esac
case "$prompt" in
  *'WORKSPACE_RULE_ALPHA'*|*'WORKSPACE_RULE_BETA'*) ;;
  *) echo 'missing workspace rule in stdin' >&2; exit 1 ;;
esac
printf '%s\n' "$prompt" > received-prompt
{}"#,
        fixture_script("text.ndjson")
    ));
    let instructions = mock.directory.join("AGENTS.md");
    let received = mock.directory.join("received-prompt");
    for (rule, old_rule, stream) in [
        ("WORKSPACE_RULE_ALPHA", "WORKSPACE_RULE_BETA", false),
        ("WORKSPACE_RULE_BETA", "WORKSPACE_RULE_ALPHA", true),
    ] {
        std::fs::write(&instructions, rule).expect("workspace instructions");
        let response = request(
            &mock,
            "/v1/chat/completions",
            Some(chat(stream)),
            None,
            None,
        )
        .await;
        let prompt = std::fs::read_to_string(&received).expect("received prompt");
        std::fs::remove_file(&received).expect("remove received prompt");
        std::fs::remove_file(&instructions).expect("remove workspace instructions");
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(prompt.contains(rule), "{prompt}");
        assert!(!prompt.contains(old_rule), "{prompt}");
        assert!(prompt.contains("Reply with exactly: PONG"), "{prompt}");
    }
}

#[tokio::test]
async fn workspace_agents_are_reloaded_for_each_new_conversation() {
    let mock = MockAgent::new(&format!(
        "cat > received-prompt\nprintf '%s\\n' \"$@\" > received-args\n{}",
        fixture_script("text.ndjson")
    ));
    let instructions = mock.directory.join("AGENTS.md");
    let received_prompt = mock.directory.join("received-prompt");
    let received_args = mock.directory.join("received-args");
    let sessions = Arc::new(cursor_proxy::session::SessionStore::default());
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);
    let mut body = chat(false);
    let mut previous_session = None;
    for rule in [
        Some("WORKSPACE_RULE_ALPHA"),
        Some("WORKSPACE_RULE_ALPHA"),
        Some("WORKSPACE_RULE_BETA"),
        None,
    ] {
        if let Some(rule) = rule {
            std::fs::write(&instructions, rule).expect("workspace instructions");
        } else {
            std::fs::remove_file(&instructions).expect("remove workspace instructions");
        }
        let request: ChatRequest = serde_json::from_value(body.clone()).expect("chat request");
        let mut updates = sessions.clone().start(
            AgentConfig {
                binary: mock.binary.clone(),
                timeout: Duration::from_secs(3),
                trusted_workspace: None,
                load_skills: true,
            },
            request,
            mock.directory.clone(),
            shutdown_rx.clone(),
            "instruction-reload-test".to_owned(),
        );
        let mut completed = false;
        while let Some(update) = updates.recv().await {
            match update {
                Update::Error(error) => panic!("chat failed: {error}"),
                Update::Done(_) => completed = true,
                _ => (),
            }
        }
        let prompt = std::fs::read_to_string(&received_prompt).expect("received prompt");
        let arguments = std::fs::read_to_string(&received_args).expect("received arguments");
        std::fs::remove_file(&received_prompt).expect("remove received prompt");
        std::fs::remove_file(&received_args).expect("remove received arguments");
        assert!(completed);
        assert!(!arguments.lines().any(|arg| arg == "--continue"));
        let session = std::fs::read_to_string(mock.directory.join("received-session"))
            .expect("session identity");
        assert_ne!(previous_session.as_ref(), Some(&session));
        previous_session = Some(session);
        match rule {
            Some(rule) => assert!(prompt.contains(rule), "{prompt}"),
            None => {
                assert!(!prompt.contains("WORKSPACE_RULE_ALPHA"), "{prompt}");
                assert!(!prompt.contains("WORKSPACE_RULE_BETA"), "{prompt}");
            }
        }
        let messages = body["messages"].as_array_mut().expect("messages");
        messages.push(json!({"role":"assistant","content":"PONG"}));
        messages.push(json!({"role":"user","content":"Reply with exactly: PONG"}));
    }
}

#[tokio::test]
async fn workspace_agents_follow_the_request_workspace() {
    let mock = MockAgent::new(&format!(
        r#"prompt=$(cat)
case "$prompt" in
  *'OTHER_WORKSPACE_RULE'*) ;;
  *) echo 'missing request workspace rule' >&2; exit 1 ;;
esac
case "$prompt" in
  *'STARTUP_WORKSPACE_RULE'*) echo 'leaked startup workspace rule' >&2; exit 1 ;;
esac
{}"#,
        fixture_script("text.ndjson")
    ));
    let other = MockAgent::new("exit 9");
    let startup_instructions = mock.directory.join("AGENTS.md");
    let other_instructions = other.directory.join("AGENTS.md");
    std::fs::write(&startup_instructions, "STARTUP_WORKSPACE_RULE").expect("startup rule");
    std::fs::write(&other_instructions, "OTHER_WORKSPACE_RULE").expect("request rule");
    let response = cli_chat(&mock, true, other.directory.to_str(), true).await;
    std::fs::remove_file(&startup_instructions).expect("remove startup rule");
    std::fs::remove_file(&other_instructions).expect("remove request rule");
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
}

#[tokio::test]
async fn invalid_workspace_agents_fail_instead_of_being_ignored() {
    let mock = MockAgent::new("exit 9");
    let instructions = mock.directory.join("AGENTS.md");
    for contents in [vec![0xff], vec![b'x'; 1024 * 1024 + 1]] {
        std::fs::write(&instructions, contents).expect("invalid workspace instructions");
        let response = request(&mock, "/v1/chat/completions", Some(chat(false)), None, None).await;
        std::fs::remove_file(&instructions).expect("remove invalid instructions");
        assert!(response.starts_with("HTTP/1.1 400"), "{response}");
        assert!(response.contains("AGENTS.md"), "{response}");
    }
}

#[tokio::test]
async fn reported_usage_survives_recovery_and_sums_separate_reported_attempts() {
    for (first, second, input, output) in [
        (json!({"inputTokens":321,"outputTokens":5}), None, 321, 5),
        (json!({"inputTokens":0,"outputTokens":0}), None, 0, 0),
        (
            json!({"inputTokens":321,"outputTokens":5}),
            Some(json!({"inputTokens":12,"outputTokens":2})),
            333,
            7,
        ),
    ] {
        for stream in [false, true] {
            let mock = MockAgent::new("exit 1");
            std::fs::write(mock.directory.join("acp-scenario"), "repair-reported")
                .expect("scenario");
            std::fs::write(mock.directory.join("usage-report.json"), first.to_string())
                .expect("usage");
            if let Some(second) = &second {
                std::fs::write(
                    mock.directory.join("recovery-usage.json"),
                    second.to_string(),
                )
                .expect("recovery usage");
            }
            let response = request(
                &mock,
                "/v1/chat/completions",
                Some(chat(stream)),
                None,
                None,
            )
            .await;
            assert!(response.starts_with("HTTP/1.1 200"), "{response}");
            let bodies: Vec<Value> = if stream {
                response
                    .lines()
                    .filter_map(|line| line.strip_prefix("data: "))
                    .filter(|data| *data != "[DONE]")
                    .map(|data| serde_json::from_str(data).expect("SSE JSON"))
                    .collect()
            } else {
                vec![
                    serde_json::from_str(response.split_once("\r\n\r\n").expect("body").1)
                        .expect("JSON"),
                ]
            };
            let usages: Vec<_> = bodies.iter().filter_map(|body| body.get("usage")).collect();
            assert_eq!(usages.len(), 1, "{response}");
            assert_eq!(usages[0]["prompt_tokens"], input, "{response}");
            assert_eq!(usages[0]["completion_tokens"], output, "{response}");
            assert!(
                usages[0].get("cursor_proxy").is_none(),
                "reported stats must win"
            );
        }
    }
}

#[tokio::test]
async fn context_update_falls_back_to_standard_usage_without_cross_session_counts() {
    for stream in [false, true] {
        let mock = MockAgent::new("exit 1");
        std::fs::write(mock.directory.join("acp-scenario"), "usage-context").expect("scenario");
        let response = request(
            &mock,
            "/v1/chat/completions",
            Some(chat(stream)),
            None,
            None,
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        let bodies: Vec<Value> = if stream {
            response
                .lines()
                .filter_map(|line| line.strip_prefix("data: "))
                .filter(|data| *data != "[DONE]")
                .map(|data| serde_json::from_str(data).expect("SSE JSON"))
                .collect()
        } else {
            vec![
                serde_json::from_str(response.split_once("\r\n\r\n").expect("body").1)
                    .expect("JSON"),
            ]
        };
        let usages: Vec<_> = bodies.iter().filter_map(|body| body.get("usage")).collect();
        assert_eq!(usages.len(), 1, "{response}");
        assert_eq!(usages[0]["prompt_tokens"], 12167);
        assert_eq!(usages[0]["completion_tokens"], 0);
        assert_eq!(usages[0]["total_tokens"], 12167);
        assert_eq!(usages[0]["cursor_proxy"]["source"], "context_snapshot");
        assert_eq!(usages[0]["cursor_proxy"]["context_window"], 200000);
        assert!(usages[0].get("cost").is_none());
    }
}

#[tokio::test]
async fn reported_usage_is_returned_once_in_json_and_sse_including_zero() {
    for (reported, expected) in [
        (
            json!({"inputTokens":10,"outputTokens":5,"cacheReadTokens":6,"cacheWriteTokens":2,"reasoningTokens":1}),
            json!({"prompt_tokens":18,"completion_tokens":5,"total_tokens":23}),
        ),
        (
            json!({"inputTokens":0,"outputTokens":0}),
            json!({"prompt_tokens":0,"completion_tokens":0,"total_tokens":0}),
        ),
    ] {
        for stream in [false, true] {
            let mock = MockAgent::new("exit 1");
            std::fs::write(mock.directory.join("acp-scenario"), "usage-reported")
                .expect("scenario");
            std::fs::write(
                mock.directory.join("usage-report.json"),
                reported.to_string(),
            )
            .expect("usage");
            let response = request(
                &mock,
                "/v1/chat/completions",
                Some(chat(stream)),
                None,
                None,
            )
            .await;
            assert!(response.starts_with("HTTP/1.1 200"), "{response}");
            let bodies: Vec<Value> = if stream {
                response
                    .lines()
                    .filter_map(|line| line.strip_prefix("data: "))
                    .filter(|data| *data != "[DONE]")
                    .map(|data| serde_json::from_str(data).expect("SSE JSON"))
                    .collect()
            } else {
                vec![
                    serde_json::from_str(response.split_once("\r\n\r\n").expect("body").1)
                        .expect("JSON"),
                ]
            };
            let usages: Vec<_> = bodies.iter().filter_map(|body| body.get("usage")).collect();
            assert_eq!(usages.len(), 1, "{response}");
            for key in ["prompt_tokens", "completion_tokens", "total_tokens"] {
                assert_eq!(usages[0][key], expected[key], "{response}");
            }
            if stream {
                assert_eq!(response.matches("[DONE]").count(), 1);
            }
        }
    }
}

#[tokio::test]
async fn sse_uses_the_same_plan_catalog_and_rejects_forbidden_named_choices() {
    for plan in [false, true] {
        let mock = MockAgent::new("exit 1");
        std::fs::write(mock.directory.join("acp-scenario"), "policy-catalog").expect("scenario");
        let mut body = chat(true);
        body["messages"] = json!([{"role":"user","content":if plan { include_str!("fixtures/opencode-plan.txt") } else { "hello" }}]);
        body["tools"] = json!([{"function":{"name":"read","parameters":{"type":"object"}}},{"function":{"name":"write","parameters":{"type":"object"}}}]);
        let response = request(
            &mock,
            "/v1/chat/completions",
            Some(body.clone()),
            None,
            None,
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        let chunks: Vec<Value> = response
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter(|data| *data != "[DONE]")
            .map(|data| serde_json::from_str(data).expect("chunk"))
            .collect();
        assert!(
            !chunks.iter().any(|chunk| chunk.get("error").is_some()),
            "{response}"
        );
        let text: String = chunks
            .iter()
            .filter_map(|chunk| chunk["choices"][0]["delta"]["content"].as_str())
            .collect();
        assert_eq!(
            serde_json::from_str::<Value>(&text).expect("catalog"),
            if plan {
                json!(["read"])
            } else {
                json!(["read", "write"])
            }
        );
        assert_eq!(response.matches("[DONE]").count(), 1);
        if plan {
            body["tool_choice"] = json!({"type":"function","function":{"name":"write"}});
            let response = request(&mock, "/v1/chat/completions", Some(body), None, None).await;
            assert!(response.starts_with("HTTP/1.1 400"), "{response}");
        }
    }
}

#[tokio::test]
async fn http_admission_exposes_the_effective_plan_catalog_and_preserves_build() {
    for (plan, choice, names, expected, call_read) in [
        (
            true,
            json!("auto"),
            vec!["read", "write"],
            vec!["read"],
            false,
        ),
        (
            false,
            json!("auto"),
            vec!["read", "write"],
            vec!["read", "write"],
            false,
        ),
        (
            true,
            json!({"type":"function","function":{"name":"read"}}),
            vec!["read", "write"],
            vec!["read"],
            true,
        ),
        (true, json!("none"), vec!["read", "write"], vec![], false),
        (false, json!("none"), vec!["read", "write"], vec![], false),
        (true, json!("auto"), vec!["write"], vec![], false),
        (
            true,
            json!("required"),
            vec!["read", "write"],
            vec!["read"],
            true,
        ),
    ] {
        let mock = MockAgent::new("exit 1");
        std::fs::write(mock.directory.join("acp-scenario"), "policy-catalog").expect("scenario");
        let mut body = chat(false);
        body["messages"] = json!([
            {"role":"user","content":if plan { include_str!("fixtures/opencode-plan.txt") } else { "hello" }},
            {"role":"user","content":if call_read { "CALL_READ" } else { "List tools" }}
        ]);
        body["tools"] = json!(
            names
                .into_iter()
                .map(|name| json!({"type":"function","function":{
                    "name":name,"parameters":{"type":"object"}
                }}))
                .collect::<Vec<_>>()
        );
        body["tool_choice"] = choice;
        let response = request(&mock, "/v1/chat/completions", Some(body), None, None).await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        let reply: Value =
            serde_json::from_str(response.split_once("\r\n\r\n").expect("body").1).expect("JSON");
        let listed: Vec<String> = serde_json::from_str(
            reply["choices"][0]["message"]["content"]
                .as_str()
                .expect("catalog text"),
        )
        .expect("names");
        assert_eq!(listed, expected);
        if call_read {
            assert_eq!(reply["choices"][0]["finish_reason"], "tool_calls");
            assert_eq!(
                reply["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
                "read"
            );
        }
    }
    for plan in [false, true] {
        let mock = MockAgent::new("exit 1");
        let mut body = chat(false);
        if plan {
            body["messages"] =
                json!([{"role":"user","content":include_str!("fixtures/opencode-plan.txt")}]);
        }
        body["tool_choice"] = json!("none");
        body["tools"] =
            json!([{"function":{"name":"broken","parameters":{"type":"not-a-json-type"}}}]);
        let response = request(&mock, "/v1/chat/completions", Some(body), None, None).await;
        assert!(response.starts_with("HTTP/1.1 400"), "{response}");
        assert!(!mock.directory.join("acp-pid").exists());
    }
}

#[tokio::test]
async fn plan_rejects_forbidden_tool_choices_before_starting_cursor() {
    for (tools, choice) in [
        (
            vec!["read", "write"],
            json!({"type":"function","function":{"name":"write"}}),
        ),
        (vec!["write"], json!("required")),
        (
            vec!["read"],
            json!({"type":"function","function":{"name":"missing"}}),
        ),
    ] {
        let mock = MockAgent::new("printf 'unexpected execution' >&2; exit 1");
        let mut body = chat(false);
        body["messages"] = json!([
            {"role":"user","content":include_str!("fixtures/opencode-plan.txt")},
            {"role":"user","content":"Read only"}
        ]);
        body["tools"] = json!(
            tools
                .into_iter()
                .map(|name| json!({"type":"function","function":{
                    "name":name,"parameters":{"type":"object"}
                }}))
                .collect::<Vec<_>>()
        );
        body["tool_choice"] = choice;
        let response = request(&mock, "/v1/chat/completions", Some(body), None, None).await;
        assert!(response.starts_with("HTTP/1.1 400"), "{response}");
        assert!(
            !mock.directory.join("acp-pid").exists(),
            "denial must precede subprocess startup"
        );
    }
}

#[tokio::test]
async fn opencode_plan_reminder_passes_plan_mode_to_cursor() {
    let mock = MockAgent::new(&format!(
        r#"mode=
while [ "$#" -gt 0 ]; do
  if [ "$1" = --mode ]; then
    shift
    mode="$1"
  fi
  shift
done
if [ "$mode" != plan ]; then
  echo 'expected --mode plan' >&2
  exit 1
fi
{}"#,
        fixture_script("text.ndjson")
    ));
    let mut body = chat(false);
    body["messages"] = json!([
        {"role":"system","content":"You are a coding agent."},
        {"role":"user","content":include_str!("fixtures/opencode-plan.txt")},
        {"role":"user","content":"Reply with exactly: PONG"}
    ]);
    let response = request(&mock, "/v1/chat/completions", Some(body), None, None).await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.contains("\"content\":\"PONG\""), "{response}");
}

#[tokio::test]
async fn opencode_build_and_ordinary_plan_text_do_not_pass_cli_plan_mode() {
    let mock = MockAgent::new(&format!(
        r#"for argument in "$@"; do
  if [ "$argument" = --mode ] || [ "$argument" = --plan ]; then
    echo 'Build must not receive Cursor Plan mode' >&2
    exit 1
  fi
done
{}"#,
        fixture_script("text.ndjson")
    ));
    for messages in [
        json!([{"role":"user","content":"Explain --mode plan without changing execution mode."}]),
        json!([
            {"role":"user","content":include_str!("fixtures/opencode-plan.txt")},
            {"role":"user","content":"a plan"},
            {"role":"assistant","content":"PONG"},
            {"role":"user","content":include_str!("fixtures/opencode-build.txt")},
            {"role":"user","content":"implement"}
        ]),
    ] {
        let mut body = chat(false);
        body["messages"] = messages;
        let response = request(&mock, "/v1/chat/completions", Some(body), None, None).await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    }
}

#[tokio::test]
async fn health_is_public_and_api_checks_the_local_token() {
    let mock = MockAgent::new("exit 9");
    let response = request(&mock, "/health", None, Some("secret"), None).await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    let response = request(&mock, "/v1/models", None, Some("secret"), None).await;
    assert!(response.starts_with("HTTP/1.1 401"), "{response}");
    let mock = MockAgent::new("printf 'Available models\\nauto - Auto (default)\\n'");
    let response = request(
        &mock,
        "/v1/models",
        None,
        Some("secret"),
        Some("Bearer secret"),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.contains("\"id\":\"auto\""), "{response}");
}

#[tokio::test]
async fn model_refresh_publishes_changes_and_retains_the_last_success_on_failure() {
    let mock = MockAgent::new(
        "cd \"$(dirname \"$0\")\" || exit 1\nprintf 'refresh\\n' >> model-calls\ncat catalog",
    );
    let catalog = mock.directory.join("catalog");
    let path = mock.directory.join("opencode.jsonc");
    let cache = ModelCache::default();
    std::fs::write(&catalog, "first-model - First Model\n").expect("first catalog");
    assert!(
        cache
            .refresh(&mock.binary, &path, "http://127.0.0.1:32124/v1", None)
            .await
            .expect("first refresh")
            .config_changed
    );
    assert_eq!(cache.list().await[0].id, "first-model");
    let first_config = std::fs::read_to_string(&path).expect("first configuration");
    assert!(
        !cache
            .refresh(&mock.binary, &path, "http://127.0.0.1:32124/v1", None)
            .await
            .expect("unchanged refresh")
            .config_changed
    );
    assert_eq!(
        std::fs::read_to_string(&path).expect("unchanged configuration"),
        first_config
    );
    std::fs::write(&catalog, "second-model - Second Model\n").expect("changed catalog");
    assert!(
        cache
            .refresh(
                &mock.binary,
                &mock.directory,
                "http://127.0.0.1:32124/v1",
                None
            )
            .await
            .is_err()
    );
    assert_eq!(cache.list().await[0].id, "first-model");
    assert_eq!(
        std::fs::read_to_string(&path).expect("retained configuration"),
        first_config
    );
    cache
        .refresh(&mock.binary, &path, "http://127.0.0.1:32124/v1", None)
        .await
        .expect("changed refresh");
    assert_eq!(cache.list().await[0].id, "second-model");
    let second_config = std::fs::read_to_string(&path).expect("second configuration");
    assert!(second_config.contains("second-model"));
    std::fs::write(&catalog, "unrecognized catalog").expect("invalid catalog");
    assert!(
        cache
            .refresh(&mock.binary, &path, "http://127.0.0.1:32124/v1", None)
            .await
            .is_err()
    );
    assert_eq!(cache.list().await[0].id, "second-model");
    assert_eq!(
        std::fs::read_to_string(&path).expect("retained second configuration"),
        second_config
    );
    let calls = std::fs::read_to_string(mock.directory.join("model-calls")).expect("refresh calls");
    assert_eq!(calls.lines().count(), 5);
    for _ in 0..3 {
        assert_eq!(cache.list().await[0].id, "second-model");
    }
    assert_eq!(
        std::fs::read_to_string(mock.directory.join("model-calls"))
            .expect("no request-triggered discovery"),
        calls
    );
}

#[tokio::test]
async fn model_refresh_loop_runs_on_startup_and_stops_with_the_server() {
    let mock = MockAgent::new(
        "printf 'refresh\\n' >> \"$(dirname \"$0\")/model-calls\"\nprintf 'startup-model - Startup Model\\n'",
    );
    let cache = Arc::new(ModelCache::default());
    let worker_cache = cache.clone();
    let binary = mock.binary.clone();
    let path = mock.directory.join("opencode.jsonc");
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let worker = tokio::spawn(async move {
        worker_cache
            .maintain(
                &binary,
                &path,
                "http://127.0.0.1:32124/v1",
                None,
                shutdown_rx,
            )
            .await;
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if cache.list().await[0].id == "startup-model" {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("startup publication");
    shutdown_tx.send(true).expect("stop refresh loop");
    tokio::time::timeout(Duration::from_secs(2), worker)
        .await
        .expect("refresh loop stopped")
        .expect("refresh task");
    assert_eq!(
        std::fs::read_to_string(mock.directory.join("model-calls"))
            .expect("discovery calls")
            .lines()
            .count(),
        1
    );
}

#[tokio::test]
async fn model_catalog_keeps_cursor_ownership_and_checks_authentication() {
    let mock = MockAgent::new(
        "printf 'Available models\\ncomposer-2.5 - Composer 2.5\\nother-model - Other Model\\n'",
    );
    let response = request(&mock, "/v1/models", None, None, None).await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    let body: Value = serde_json::from_str(response.split_once("\r\n\r\n").expect("HTTP body").1)
        .expect("JSON body");
    assert_eq!(body["data"].as_array().expect("models").len(), 2);
    assert_eq!(body["data"][0]["id"], "composer-2.5");
    assert_eq!(body["data"][1]["id"], "other-model");
    for model in body["data"].as_array().expect("models") {
        assert_eq!(model["object"], "model");
        assert_eq!(model["owned_by"], "cursor");
    }
    let response = request(&mock, "/v1/models", None, Some("secret"), None).await;
    assert!(response.starts_with("HTTP/1.1 401"), "{response}");
    let response = request(
        &mock,
        "/v1/models",
        None,
        Some("secret"),
        Some("Bearer secret"),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
}

#[tokio::test]
async fn reasoning_only_recovery_preserves_one_http_completion_for_json_and_sse() {
    let mock = MockAgent::new(
        r#"prompt=$(cat)
case "$prompt" in
  *ORIGINAL_TASK_761*)
    printf '%s\n' '{"type":"thinking","subtype":"delta","text":"THOUGHT_ONLY"}'
    ;;
  *)
    printf '%s\n' '{"type":"assistant","timestamp_ms":1,"message":{"content":[{"type":"text","text":"RECOVERED"}]}}'
    ;;
esac
printf '%s\n' '{"type":"result","subtype":"success","is_error":false}'
"#,
    );
    for stream in [false, true] {
        let body = json!({"model":"composer-2.5","stream":stream,"messages":[{"role":"user","content":"ORIGINAL_TASK_761"}]});
        let response = request(&mock, "/v1/chat/completions", Some(body), None, None).await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        if stream {
            let chunks: Vec<Value> = response
                .lines()
                .filter_map(|line| line.strip_prefix("data: "))
                .filter(|data| *data != "[DONE]")
                .map(|data| serde_json::from_str(data).expect("SSE chunk"))
                .collect();
            let text: String = chunks
                .iter()
                .filter_map(|chunk| chunk["choices"][0]["delta"]["content"].as_str())
                .collect();
            assert_eq!(text, "RECOVERED");
            let id = chunks[0]["id"].as_str().expect("completion ID");
            assert!(chunks.iter().all(|chunk| chunk["id"] == id));
            assert_eq!(response.matches("[DONE]").count(), 1);
            assert_eq!(
                chunks
                    .iter()
                    .filter(|chunk| chunk["choices"][0]["finish_reason"] == "stop")
                    .count(),
                1
            );
        } else {
            let body: Value =
                serde_json::from_str(response.split_once("\r\n\r\n").expect("HTTP body").1)
                    .expect("JSON response");
            assert_eq!(body["choices"][0]["message"]["content"], "RECOVERED");
            assert_eq!(
                body["choices"][0]["message"]["reasoning_content"],
                "THOUGHT_ONLY"
            );
        }
    }
}

#[tokio::test]
async fn recorded_text_roundtrips_as_nonstream_and_sse_without_duplicates() {
    let mock = MockAgent::new(&fixture_script("text.ndjson"));
    let response = request(&mock, "/v1/chat/completions", Some(chat(false)), None, None).await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    let body: Value = serde_json::from_str(response.split_once("\r\n\r\n").expect("HTTP body").1)
        .expect("JSON body");
    assert_eq!(body["choices"][0]["message"]["content"], "PONG");
    assert!(
        body.get("usage").is_none(),
        "ACP must not invent per-turn token usage"
    );
    let response = request(&mock, "/v1/chat/completions", Some(chat(true)), None, None).await;
    assert!(
        response
            .to_lowercase()
            .contains("content-type: text/event-stream"),
        "{response}"
    );
    assert!(response.contains("[DONE]"), "{response}");
    assert!(response.contains("reasoning_content"), "{response}");
    assert!(response.contains("\"content\":\"P\""), "{response}");
    assert!(response.contains("\"content\":\"ONG\""), "{response}");
    assert!(!response.contains("\"content\":\"PONG\""), "{response}");
}

#[tokio::test]
async fn assistant_deltas_skip_pre_tool_and_final_flushes() {
    let mock = MockAgent::new(
        r#"cat >/dev/null
cat <<'EOF'
{"type":"assistant","timestamp_ms":1,"message":{"content":[{"type":"text","text":"PO"}]}}
{"type":"assistant","timestamp_ms":2,"model_call_id":"first-call","message":{"content":[{"type":"text","text":"PO"}]}}
{"type":"assistant","timestamp_ms":3,"message":{"content":[{"type":"text","text":"N"},{"type":"text","text":"G"}]}}
{"type":"assistant","timestamp_ms":4,"model_call_id":"second-call","message":{"content":[{"type":"text","text":"NG"}]}}
{"type":"assistant","message":{"content":[{"type":"text","text":"PONG"}]}}
{"type":"assistant","message":{"content":[{"type":"text","text":"IGNORED_FINAL_FLUSH"}]}}
{"type":"result","subtype":"success","is_error":false,"result":"PONG"}
EOF"#,
    );
    for stream in [false, true] {
        let response = request(
            &mock,
            "/v1/chat/completions",
            Some(chat(stream)),
            None,
            None,
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        if stream {
            let chunks: Vec<Value> = response
                .lines()
                .filter_map(|line| line.strip_prefix("data: "))
                .filter(|data| *data != "[DONE]")
                .map(|data| serde_json::from_str(data).expect("SSE chunk"))
                .collect();
            let deltas: Vec<&str> = chunks
                .iter()
                .filter_map(|chunk| chunk["choices"][0]["delta"]["content"].as_str())
                .collect();
            assert_eq!(deltas, ["PO", "NG"], "{response}");
            assert_eq!(response.matches("[DONE]").count(), 1, "{response}");
            assert_eq!(
                chunks
                    .iter()
                    .filter(|chunk| chunk["choices"][0]["finish_reason"] == "stop")
                    .count(),
                1,
                "{response}"
            );
        } else {
            let body: Value =
                serde_json::from_str(response.split_once("\r\n\r\n").expect("HTTP body").1)
                    .expect("JSON body");
            assert_eq!(body["choices"][0]["message"]["content"], "PONG");
        }
    }
}

#[tokio::test]
async fn terminal_result_supplies_text_when_only_assistant_flushes_arrive() {
    let mock = MockAgent::new(
        r#"cat >/dev/null
cat <<'EOF'
{"type":"assistant","timestamp_ms":1,"model_call_id":"first-call","message":{"content":[{"type":"text","text":"IGNORED_PRE_TOOL_FLUSH"}]}}
{"type":"assistant","message":{"content":[{"type":"text","text":"IGNORED_FINAL_FLUSH"}]}}
{"type":"result","subtype":"success","is_error":false,"result":"PONG"}
EOF"#,
    );
    for stream in [false, true] {
        let response = request(
            &mock,
            "/v1/chat/completions",
            Some(chat(stream)),
            None,
            None,
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(!response.contains("IGNORED_"), "{response}");
        if stream {
            assert_eq!(
                response.matches("\"content\":\"PONG\"").count(),
                1,
                "{response}"
            );
            assert_eq!(response.matches("[DONE]").count(), 1, "{response}");
        } else {
            let body: Value =
                serde_json::from_str(response.split_once("\r\n\r\n").expect("HTTP body").1)
                    .expect("JSON body");
            assert_eq!(body["choices"][0]["message"]["content"], "PONG");
        }
    }
}

#[tokio::test]
async fn native_tool_call_returns_to_host_instead_of_waiting_for_completion() {
    let mock = MockAgent::new(&fixture_script("tool_call.ndjson"));
    let mut body = chat(false);
    body["tools"] = json!([{"type":"function","function":{"name":"read","parameters":{
        "type":"object","properties":{"filePath":{"type":"string"}},"required":["filePath"]}}}]);
    let response = request(&mock, "/v1/chat/completions", Some(body), None, None).await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    let body: Value = serde_json::from_str(response.split_once("\r\n\r\n").expect("HTTP body").1)
        .expect("JSON body");
    assert_eq!(body["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(
        body["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
        "read"
    );
}

#[tokio::test]
async fn historical_tool_results_cannot_attach_to_an_unknown_acp_thread() {
    let script = r#"prompt=$(cat)
case "$prompt" in
  *'TOOL_RESULT (name: read, call_id: read1): contents'*) ;;
  *) echo 'missing host tool result' >&2; exit 1 ;;
esac
printf '%s\n' '{"type":"assistant","timestamp_ms":1,"message":{"content":[{"type":"text","text":"{\"name\":\"write\",\"arguments\":{\"path\":\"a\",\"content\":\"contents\"}}"}]}}'
printf '%s\n' '{"type":"result","subtype":"success","is_error":false}'
"#;
    let mock = MockAgent::new(script);
    let body = json!({"model":"auto","messages":[
        {"role":"user","content":"copy a"},
        {"role":"assistant","tool_calls":[{"id":"read1","type":"function","function":{"name":"read","arguments":"{}"}}]},
        {"role":"tool","tool_call_id":"read1","content":"contents"}
    ],"tools":[{"type":"function","function":{"name":"write","parameters":{
        "type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]}}}]});
    let response = request(&mock, "/v1/chat/completions", Some(body), None, None).await;
    assert!(response.starts_with("HTTP/1.1 400"), "{response}");
    assert!(
        response.contains("Unknown or expired tool_call_id"),
        "{response}"
    );
}

#[tokio::test]
async fn process_errors_preserve_auth_status_and_invalid_model_errors() {
    let mock = MockAgent::new("cat >/dev/null\necho 'not logged in' >&2\nexit 1");
    let response = request(&mock, "/v1/chat/completions", Some(chat(false)), None, None).await;
    assert!(response.starts_with("HTTP/1.1 401"), "{response}");
    assert!(response.contains("cursor-agent login"), "{response}");
    let mock = MockAgent::new("cat >/dev/null\necho 'Cannot use this model: wrong' >&2\nexit 1");
    let response = request(&mock, "/v1/chat/completions", Some(chat(false)), None, None).await;
    assert!(response.starts_with("HTTP/1.1 400"), "{response}");
}

#[tokio::test]
async fn requests_reject_multimodal_and_invalid_tool_arguments_explicitly() {
    let mock = MockAgent::new("exit 9");
    let body = json!({"messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"https://invalid/image.png"}}]}]});
    let response = request(&mock, "/v1/chat/completions", Some(body), None, None).await;
    assert!(response.starts_with("HTTP/1.1 400"), "{response}");
    let mock = MockAgent::new(
        r#"cat >/dev/null
printf '%s\n' '{"type":"tool_call","subtype":"started","call_id":"x","tool_call":{"readToolCall":{"args":{}}}}'"#,
    );
    let mut body = chat(false);
    body["tools"] = json!([{"type":"function","function":{"name":"read","parameters":{
        "type":"object","properties":{"path":{"type":"string"}},"required":["path"]}}}]);
    let response = request(&mock, "/v1/chat/completions", Some(body), None, None).await;
    assert!(response.starts_with("HTTP/1.1 502"), "{response}");
}

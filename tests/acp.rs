#![cfg(unix)]

use cursor_proxy::{
    acp::{Connection, Event},
    agent::AgentConfig,
    chat::ExecutionMode,
};
use serde_json::json;
use std::{path::PathBuf, time::Duration};

struct Fixture(PathBuf);

impl Fixture {
    fn new(scenario: &str) -> Self {
        let directory =
            std::env::temp_dir().join(format!("cursor-acp-test-{}-{scenario}", std::process::id()));
        std::fs::create_dir(&directory).expect("test directory");
        let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/acp-agent.py");
        std::fs::write(directory.join("acp-scenario"), scenario).expect("scenario");
        std::os::unix::fs::symlink(script, directory.join("agent")).expect("stable launcher");
        Self(directory)
    }

    fn config(&self) -> AgentConfig {
        AgentConfig {
            binary: self.0.join("agent"),
            timeout: Duration::from_secs(3),
            trusted_workspace: None,
            load_skills: false,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.0) {
            eprintln!("ACP fixture cleanup: {error}");
        }
    }
}

#[tokio::test]
async fn streams_ordered_text_and_thoughts_without_prefix_deduplication() {
    let fixture = Fixture::new("text");
    let mut connection = Connection::open(&fixture.config(), &fixture.0, "composer-2.5")
        .await
        .expect("ACP connection");
    let created = connection
        .call("session/new", json!({"cwd":fixture.0,"mcpServers":[]}))
        .await
        .expect("session");
    let session = created["sessionId"].as_str().expect("session ID");
    connection.prompt(session, "hello").expect("prompt");
    let mut text = String::new();
    let mut thought = String::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(5), connection.next())
            .await
            .expect("event deadline")
            .expect("event")
        {
            Event::Notification(value) => {
                let update = &value["update"];
                match update["sessionUpdate"].as_str() {
                    Some("agent_message_chunk") => {
                        text.push_str(update["content"]["text"].as_str().expect("text"))
                    }
                    Some("agent_thought_chunk") => {
                        thought.push_str(update["content"]["text"].as_str().expect("thought"))
                    }
                    _ => (),
                }
            }
            Event::Finished(result) => {
                assert_eq!(result.expect("prompt result")["stopReason"], "end_turn");
                break;
            }
            Event::Exited(error) => panic!("early exit: {error}"),
        }
    }
    assert_eq!(text, "haha你好");
    assert_eq!(thought, "THOUGHT");
    connection.close().await;
}

#[tokio::test]
async fn notifications_before_session_response_are_retained() {
    let fixture = Fixture::new("early");
    let mut connection = Connection::open(&fixture.config(), &fixture.0, "composer-2.5")
        .await
        .expect("ACP connection");
    connection
        .call("session/new", json!({"cwd":fixture.0,"mcpServers":[]}))
        .await
        .expect("session");
    match tokio::time::timeout(Duration::from_secs(2), connection.next())
        .await
        .expect("deadline")
        .expect("early event")
    {
        Event::Notification(value) => assert_eq!(value["update"]["content"]["text"], "EARLY"),
        event => panic!("lost pre-registration notification: {event:?}"),
    }
    connection.close().await;
}

#[tokio::test]
async fn process_exit_drains_last_output_and_stderr_without_waiting_for_descendants() {
    let fixture = Fixture::new("exit");
    let mut connection = Connection::open(&fixture.config(), &fixture.0, "composer-2.5")
        .await
        .expect("ACP connection");
    let created = connection
        .call("session/new", json!({"cwd":fixture.0,"mcpServers":[]}))
        .await
        .expect("session");
    connection
        .prompt(created["sessionId"].as_str().expect("session"), "exit")
        .expect("prompt");
    let mut text = String::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(2), connection.next())
            .await
            .expect("bounded drain")
            .expect("event")
        {
            Event::Notification(value) => {
                text.push_str(value["update"]["content"]["text"].as_str().expect("text"))
            }
            Event::Exited(error) => {
                assert!(error.message.contains("controlled ACP exit"), "{error}");
                break;
            }
            Event::Finished(result) => panic!("unexpected prompt response: {result:?}"),
        }
    }
    assert_eq!(text, "LAST");
    connection.close().await;
}

#[tokio::test]
async fn cancellation_returns_the_cancelled_stop_reason() {
    let fixture = Fixture::new("cancel");
    let mut connection = Connection::open(&fixture.config(), &fixture.0, "composer-2.5")
        .await
        .expect("ACP connection");
    let created = connection
        .call("session/new", json!({"cwd":fixture.0,"mcpServers":[]}))
        .await
        .expect("session");
    let session = created["sessionId"].as_str().expect("session");
    connection.prompt(session, "wait").expect("prompt");
    connection.cancel(session).expect("cancel");
    match tokio::time::timeout(Duration::from_secs(2), connection.next())
        .await
        .expect("cancel deadline")
        .expect("cancel result")
    {
        Event::Finished(result) => assert_eq!(result.expect("response")["stopReason"], "cancelled"),
        event => panic!("unexpected event: {event:?}"),
    }
    connection.close().await;
}

#[tokio::test]
async fn blocking_cursor_extensions_are_answered_without_stalling_dispatch() {
    let fixture = Fixture::new("question");
    let mut connection = Connection::open(&fixture.config(), &fixture.0, "composer-2.5")
        .await
        .expect("ACP connection");
    let created = connection
        .call("session/new", json!({"cwd":fixture.0,"mcpServers":[]}))
        .await
        .expect("session");
    connection
        .prompt(created["sessionId"].as_str().expect("session"), "question")
        .expect("prompt");
    let mut text = String::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(2), connection.next())
            .await
            .expect("extension deadline")
            .expect("event")
        {
            Event::Notification(value) => {
                text.push_str(value["update"]["content"]["text"].as_str().expect("text"))
            }
            Event::Finished(result) => {
                result.expect("reply after extension");
                break;
            }
            Event::Exited(error) => panic!("extension failed: {error}"),
        }
    }
    assert_eq!(text, "SKIPPED");
    connection.close().await;
}

#[tokio::test]
async fn startup_timeout_kills_the_owned_process() {
    let fixture = Fixture::new("hang-init");
    let mut config = fixture.config();
    config.timeout = Duration::from_millis(100);
    assert!(
        Connection::open(&config, &fixture.0, "composer-2.5")
            .await
            .is_err()
    );
    let pid: i32 = std::fs::read_to_string(fixture.0.join("acp-pid"))
        .expect("pid")
        .parse()
        .expect("pid number");
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None)
                == Err(nix::errno::Errno::ESRCH)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("ACP owner drop must reap child");
}

#[tokio::test]
async fn pre_registration_notifications_have_a_byte_budget() {
    let fixture = Fixture::new("overflow");
    let connection = Connection::open(&fixture.config(), &fixture.0, "composer-2.5")
        .await
        .expect("ACP connection");
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        connection.call("session/new", json!({"cwd":fixture.0,"mcpServers":[]})),
    )
    .await
    .expect("bounded overflow handling");
    assert!(
        result.is_err(),
        "oversized pending session notifications must fail session creation"
    );
    connection.close().await;
}

#[tokio::test]
async fn dropping_a_connection_kills_a_peer_that_stops_reading_stdin() {
    let fixture = Fixture::new("blocked-input");
    let connection = Connection::open(&fixture.config(), &fixture.0, "composer-2.5")
        .await
        .expect("ACP connection");
    let created = connection
        .call("session/new", json!({"cwd":fixture.0,"mcpServers":[]}))
        .await
        .expect("session");
    connection
        .prompt(
            created["sessionId"].as_str().expect("session"),
            &"x".repeat(2 * 1024 * 1024),
        )
        .expect("large prompt");
    let pid: i32 = std::fs::read_to_string(fixture.0.join("acp-pid"))
        .expect("pid")
        .parse()
        .expect("pid number");
    drop(connection);
    let result = tokio::time::timeout(Duration::from_secs(2), async {
        while nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None)
            != Err(nix::errno::Errno::ESRCH)
        {
            tokio::task::yield_now().await;
        }
    })
    .await;
    if result.is_err() {
        nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGKILL,
        )
        .expect("clean up owned stalled peer");
    }
    assert!(
        result.is_ok(),
        "owner drop must not wait for the SDK write drain"
    );
}

#[tokio::test]
async fn plan_permissions_filter_tools_and_do_not_reuse_build_grants() {
    let fixture = Fixture::new("plan-approvals");
    let mut connection = Connection::open(&fixture.config(), &fixture.0, "composer-2.5")
        .await
        .expect("connection");
    let created = connection
        .call("session/new", json!({"cwd":fixture.0,"mcpServers":[]}))
        .await
        .expect("session");
    let session = created["sessionId"].as_str().expect("session");
    let names = [
        "read",
        "glob",
        "grep",
        "question",
        "write",
        "edit",
        "patch",
        "bash",
        "shell",
        "task",
        "execute",
        "custom_read",
        "Read",
    ];
    for mode in [
        ExecutionMode::Build,
        ExecutionMode::Plan,
        ExecutionMode::Build,
    ] {
        for (index, name) in names.iter().enumerate() {
            connection
                .allow_tools(
                    session,
                    "bridge",
                    names.iter().map(|name| (*name).to_owned()),
                    mode,
                )
                .await;
            connection
                .call("probe/grant", json!({"toolName":name}))
                .await
                .expect("tool notification");
            assert!(matches!(
                connection.next().await,
                Some(Event::Notification(_))
            ));
            let outcome = connection
                .call("probe/permission", json!({"sessionId":session}))
                .await
                .expect("decision");
            assert_eq!(
                outcome["outcome"]["optionId"],
                if mode == ExecutionMode::Build || index < 4 {
                    "allow-7"
                } else {
                    "deny-7"
                },
                "{mode:?} {name}"
            );
            let repeated = connection
                .call("probe/permission", json!({"sessionId":session}))
                .await
                .expect("repeated decision");
            assert_eq!(repeated["outcome"]["optionId"], "deny-7");
        }
    }
    connection
        .allow_tools(
            session,
            "bridge",
            ["write".to_owned()],
            ExecutionMode::Build,
        )
        .await;
    connection
        .call("probe/grant", json!({"toolName":"write"}))
        .await
        .expect("build grant");
    assert!(matches!(
        connection.next().await,
        Some(Event::Notification(_))
    ));
    connection
        .allow_tools(
            session,
            "bridge",
            names.iter().map(|name| (*name).to_owned()),
            ExecutionMode::Plan,
        )
        .await;
    let outcome = connection
        .call("probe/permission", json!({"sessionId":session}))
        .await
        .expect("stale decision");
    assert_eq!(outcome["outcome"]["optionId"], "deny-7");
    connection.close().await;
}

#[tokio::test]
async fn session_response_application_precedes_subsequent_permission_requests() {
    let fixture = Fixture::new("barrier");
    let connection = Connection::open(&fixture.config(), &fixture.0, "composer-2.5")
        .await
        .expect("ACP connection");
    let response = connection
        .call_ordered("session/new", json!({"cwd":fixture.0,"mcpServers":[]}))
        .await
        .expect("session response");
    let session = response.value()["sessionId"]
        .as_str()
        .expect("session")
        .to_owned();
    assert!(!fixture.0.join("permission-result").exists());
    connection
        .allow_tools(
            &session,
            "bridge",
            ["probe".to_owned()],
            ExecutionMode::Build,
        )
        .await;
    response.apply();
    tokio::time::timeout(Duration::from_secs(2), async {
        while !fixture.0.join("permission-result").exists() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("permission response");
    let outcome: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(fixture.0.join("permission-result")).expect("permission result"),
    )
    .expect("JSON");
    assert_eq!(outcome["outcome"]["optionId"], "allow-7");
    connection.close().await;
}

#[tokio::test]
async fn permissions_are_session_bound_single_use_and_invalidated_by_policy_changes() {
    for scenario in [
        "policy-change",
        "wrong-session",
        "valid-once",
        "policy-cancel",
    ] {
        let fixture = Fixture::new(scenario);
        let mut connection = Connection::open(&fixture.config(), &fixture.0, "composer-2.5")
            .await
            .expect("ACP connection");
        let created = connection
            .call("session/new", json!({"cwd":fixture.0,"mcpServers":[]}))
            .await
            .expect("session");
        let session = created["sessionId"].as_str().expect("session");
        connection
            .allow_tools(
                session,
                "bridge",
                ["probe".to_owned()],
                ExecutionMode::Build,
            )
            .await;
        connection
            .call("probe/grant", json!({}))
            .await
            .expect("grant update");
        assert!(matches!(
            connection.next().await,
            Some(Event::Notification(_))
        ));
        if scenario == "policy-change" {
            connection
                .allow_tools(session, "different", [], ExecutionMode::Build)
                .await;
        }
        if scenario == "policy-cancel" {
            connection.cancel(session).expect("cancel");
        }
        let target = if scenario == "wrong-session" {
            "unrelated-session"
        } else {
            session
        };
        let outcome = connection
            .call("probe/permission", json!({"sessionId":target}))
            .await
            .expect("decision");
        if scenario == "policy-cancel" {
            assert_eq!(outcome["outcome"]["outcome"], "cancelled");
        } else {
            assert_eq!(
                outcome["outcome"]["optionId"],
                if scenario == "valid-once" {
                    "allow-7"
                } else {
                    "deny-7"
                }
            );
        }
        if scenario == "valid-once" {
            let repeated = connection
                .call("probe/permission", json!({"sessionId":session}))
                .await
                .expect("repeat decision");
            assert_eq!(repeated["outcome"]["optionId"], "deny-7");
        }
        connection.close().await;
    }
}

#[tokio::test]
async fn retained_permission_grants_are_bounded_even_when_notifications_are_drained() {
    let fixture = Fixture::new("grant-budget");
    let mut connection = Connection::open(&fixture.config(), &fixture.0, "composer-2.5")
        .await
        .expect("ACP connection");
    let created = connection
        .call("session/new", json!({"cwd":fixture.0,"mcpServers":[]}))
        .await
        .expect("session");
    connection
        .allow_tools(
            created["sessionId"].as_str().expect("session"),
            "bridge",
            ["probe".to_owned()],
            ExecutionMode::Build,
        )
        .await;
    for index in 0..1024 {
        connection
            .call("probe/grant", json!({"toolId":format!("grant-{index}")}))
            .await
            .expect("bounded grant");
        assert!(matches!(
            connection.next().await,
            Some(Event::Notification(_))
        ));
    }
    assert!(
        connection
            .call("probe/grant", json!({"toolId":"overflow"}))
            .await
            .is_err()
    );
    connection.close().await;
}

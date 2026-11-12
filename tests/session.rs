#![cfg(unix)]

use cursor_proxy::{
    agent::{AgentConfig, Update},
    chat::ChatRequest,
    session::SessionStore,
    tools::ToolCall,
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
use tokio::sync::{mpsc, watch};

struct Fixture(PathBuf);
static SEQUENCE: AtomicU64 = AtomicU64::new(1);

impl Fixture {
    fn new(scenario: &str) -> Self {
        let directory = std::env::temp_dir().join(format!(
            "cursor-session-test-{}-{scenario}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
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
            eprintln!("session fixture cleanup: {error}");
        }
    }
}

async fn collect(mut updates: mpsc::Receiver<Update>) -> (String, Vec<ToolCall>) {
    let mut text = String::new();
    let mut calls = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(5), updates.recv())
            .await
            .expect("reply deadline")
            .expect("reply event")
        {
            Update::Text(value) => text.push_str(&value),
            Update::Tools(value) => calls.extend(value),
            Update::Done(_) => return (text, calls),
            Update::Error(error) => panic!("reply failed: {error}"),
            Update::Thinking(_) => (),
        }
    }
}

#[tokio::test]
async fn reported_and_context_usage_do_not_leak_into_the_next_user_turn() {
    for scenario in ["usage-reported-once", "usage-context-once"] {
        let fixture = Fixture::new(scenario);
        std::fs::write(
            fixture.0.join("usage-report.json"),
            "{\"inputTokens\":321,\"outputTokens\":5}",
        )
        .expect("usage");
        let store = Arc::new(SessionStore::default());
        let (shutdown, receiver) = watch::channel(false);
        let mut body =
            json!({"model":"composer-2.5","messages":[{"role":"user","content":"PONG"}]});
        for turn in 0..2 {
            let mut updates = store.clone().start(
                fixture.config(),
                serde_json::from_value(body.clone()).expect("request"),
                fixture.0.clone(),
                receiver.clone(),
                format!("usage-{turn}"),
            );
            let mut text = String::new();
            loop {
                match tokio::time::timeout(Duration::from_secs(5), updates.recv())
                    .await
                    .expect("deadline")
                    .expect("update")
                {
                    Update::Text(value) => text.push_str(&value),
                    Update::Done(usage) => {
                        assert_eq!(usage.is_some(), turn == 0, "{scenario}");
                        break;
                    }
                    Update::Thinking(_) => (),
                    update => panic!("unexpected update: {update:?}"),
                }
            }
            assert_eq!(text, "PONG");
            body["messages"].as_array_mut().expect("history").extend([
                json!({"role":"assistant","content":text}),
                json!({"role":"user","content":"PONG_AGAIN"}),
            ]);
        }
        shutdown.send(true).expect("shutdown");
    }
}

#[tokio::test]
async fn context_usage_follows_tool_handoff_ordering_and_is_emitted_once_per_attachment() {
    async fn receive(
        mut updates: mpsc::Receiver<Update>,
    ) -> (String, Vec<ToolCall>, Option<cursor_proxy::stream::Usage>) {
        let mut text = String::new();
        let mut calls = Vec::new();
        loop {
            match tokio::time::timeout(Duration::from_secs(5), updates.recv())
                .await
                .expect("deadline")
                .expect("update")
            {
                Update::Text(value) => text.push_str(&value),
                Update::Tools(value) => calls.extend(value),
                Update::Done(usage) => return (text, calls, usage),
                Update::Thinking(_) => (),
                Update::Error(error) => panic!("reply failed: {error}"),
            }
        }
    }
    let fixture = Fixture::new("usage-host-tool");
    let store = Arc::new(SessionStore::default());
    let (_shutdown, receiver) = watch::channel(false);
    let mut body = json!({"model":"composer-2.5","messages":[{"role":"user","content":"Read"}],"tools":[{"function":{"name":"read","parameters":{"type":"object","properties":{"text":{"type":"string"}}}}}]});
    let (text, calls, usage) = receive(store.clone().start(
        fixture.config(),
        serde_json::from_value(body.clone()).expect("request"),
        fixture.0.clone(),
        receiver.clone(),
        "first".into(),
    ))
    .await;
    assert_eq!(calls.len(), 1);
    assert_eq!(usage.expect("handoff usage").input_tokens, 12168);
    body["messages"].as_array_mut().expect("history").extend([
        json!({"role":"assistant","content":text,"tool_calls":calls}),
        json!({"role":"tool","tool_call_id":calls[0].id,"content":"HOST_RESULT"}),
    ]);
    let (text, calls, usage) = receive(store.start(
        fixture.config(),
        serde_json::from_value(body).expect("result"),
        fixture.0.clone(),
        receiver,
        "second".into(),
    ))
    .await;
    assert_eq!(text, "HOST_RESULT");
    assert!(calls.is_empty());
    assert_eq!(
        usage.expect("post-result barrier usage").input_tokens,
        16000
    );
}

#[tokio::test]
async fn switching_build_plan_build_uses_independent_sessions() {
    let fixture = Fixture::new("mode-config");
    let store = Arc::new(SessionStore::default());
    let (shutdown, receiver) = watch::channel(false);
    let mut body = json!({"model":"composer-2.5","messages":[]});
    for (index, plan) in [false, true, false].into_iter().enumerate() {
        body["messages"].as_array_mut().expect("history").extend([
            json!({"role":"user","content":if plan { include_str!("fixtures/opencode-plan.txt") } else { include_str!("fixtures/opencode-build.txt") }}),
            json!({"role":"user","content":"Confirm mode"}),
        ]);
        let (text, calls) = collect(store.clone().start(
            fixture.config(),
            serde_json::from_value(body.clone()).expect("request"),
            fixture.0.clone(),
            receiver.clone(),
            format!("mode-{index}"),
        ))
        .await;
        assert_eq!(text, "MODE_OK");
        assert!(calls.is_empty());
        body["messages"]
            .as_array_mut()
            .expect("history")
            .push(json!({"role":"assistant","content":text}));
    }
    let trace: Vec<Value> = std::fs::read_to_string(fixture.0.join("mode-trace.ndjson"))
        .expect("trace")
        .lines()
        .map(|line| serde_json::from_str(line).expect("JSON"))
        .collect();
    let sessions: Vec<_> = trace
        .iter()
        .filter(|entry| entry["method"] == "session/prompt")
        .map(|entry| entry["params"]["sessionId"].as_str().expect("session"))
        .collect();
    assert_eq!(sessions.len(), 3);
    assert_ne!(sessions[0], sessions[1]);
    assert_ne!(sessions[1], sessions[2]);
    assert_ne!(sessions[0], sessions[2]);
    let modes: Vec<_> = trace
        .iter()
        .filter(|entry| entry["method"] == "session/set_config_option")
        .map(|entry| entry["params"]["value"].as_str().expect("mode"))
        .collect();
    assert_eq!(modes, ["agent", "plan", "agent"]);
    shutdown.send(true).expect("shutdown");
}

#[tokio::test]
async fn concurrent_plan_and_build_tools_keep_their_own_policy_and_result() {
    let fixture = Fixture::new("policy-catalog");
    let store = Arc::new(SessionStore::default());
    let (shutdown, receiver) = watch::channel(false);
    let mut bodies: Vec<Value> = [false, true].into_iter().map(|plan| json!({
        "model":"composer-2.5", "messages":[
            {"role":"user","content":if plan { include_str!("fixtures/opencode-plan.txt") } else { "hello" }},
            {"role":"user","content":if plan { "CALL_READ" } else { "CALL_WRITE" }}
        ], "tools":[{"function":{"name":"read","parameters":{"type":"object"}}},{"function":{"name":"write","parameters":{"type":"object"}}}]
    })).collect();
    let first = store.clone().start(
        fixture.config(),
        serde_json::from_value(bodies[0].clone()).expect("build request"),
        fixture.0.clone(),
        receiver.clone(),
        "build".into(),
    );
    let second = store.clone().start(
        fixture.config(),
        serde_json::from_value(bodies[1].clone()).expect("plan request"),
        fixture.0.clone(),
        receiver.clone(),
        "plan".into(),
    );
    let replies = tokio::join!(collect(first), collect(second));
    for (index, (text, calls)) in [replies.0, replies.1].into_iter().enumerate() {
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].function.name,
            if index == 0 { "write" } else { "read" }
        );
        assert_eq!(
            serde_json::from_str::<Value>(&text).expect("catalog"),
            if index == 0 {
                json!(["read", "write"])
            } else {
                json!(["read"])
            }
        );
        let marker = if index == 0 {
            "BUILD_RESULT"
        } else {
            "PLAN_RESULT"
        };
        bodies[index]["messages"]
            .as_array_mut()
            .expect("history")
            .extend([
                json!({"role":"assistant","content":text,"tool_calls":calls}),
                json!({"role":"tool","tool_call_id":calls[0].id,"content":marker}),
            ]);
        let mut wrong_mode = bodies[index].clone();
        wrong_mode["messages"][0]["content"] = json!(if index == 0 {
            include_str!("fixtures/opencode-plan.txt")
        } else {
            include_str!("fixtures/opencode-build.txt")
        });
        let mut rejected = store.clone().start(
            fixture.config(),
            serde_json::from_value(wrong_mode).expect("changed mode"),
            fixture.0.clone(),
            receiver.clone(),
            "cross-mode".into(),
        );
        match rejected.recv().await.expect("rejection") {
            Update::Error(error) => assert_eq!(error.status, axum::http::StatusCode::BAD_REQUEST),
            update => panic!("mode-changing continuation was accepted: {update:?}"),
        }
        let (text, calls) = collect(store.clone().start(
            fixture.config(),
            serde_json::from_value(bodies[index].clone()).expect("result"),
            fixture.0.clone(),
            receiver.clone(),
            marker.into(),
        ))
        .await;
        assert_eq!(text, marker);
        assert!(calls.is_empty());
    }
    shutdown.send(true).expect("shutdown");
}

#[tokio::test]
async fn advertised_modes_are_selected_before_prompt_and_missing_values_fail_closed() {
    for (scenario, plan, succeeds) in [
        ("mode-config", true, true),
        ("mode-config", false, true),
        ("mode-config-grouped", true, true),
        ("mode-config-grouped", false, true),
        ("mode-config-grouped-missing-plan", true, false),
        ("mode-legacy", true, true),
        ("mode-legacy", false, true),
        ("mode-config-missing-plan", true, false),
        ("mode-config-missing-agent", false, false),
        ("mode-legacy-missing-plan", true, false),
        ("mode-legacy-missing-agent", false, false),
        ("mode-config-rejected", true, false),
        ("mode-legacy-rejected", true, false),
        ("mode-none", true, false),
        ("mode-none", false, true),
    ] {
        let fixture = Fixture::new(scenario);
        let store = Arc::new(SessionStore::default());
        let (_shutdown, receiver) = watch::channel(false);
        let body = json!({"model":"composer-2.5","messages":[{"role":"user","content":if plan {
            include_str!("fixtures/opencode-plan.txt")
        } else { "hello" }}]});
        let mut updates = store.start(
            fixture.config(),
            serde_json::from_value(body).expect("request"),
            fixture.0.clone(),
            receiver,
            scenario.into(),
        );
        let mut text = String::new();
        loop {
            match tokio::time::timeout(Duration::from_secs(5), updates.recv())
                .await
                .expect("deadline")
                .expect("terminal event")
            {
                Update::Text(value) => text.push_str(&value),
                Update::Done(_) => {
                    assert!(succeeds, "{scenario} accepted unsupported mode");
                    break;
                }
                Update::Error(error) => {
                    assert!(!succeeds, "{scenario}: {error}");
                    break;
                }
                update => panic!("{scenario}: unexpected {update:?}"),
            }
        }
        let trace: Vec<Value> = std::fs::read_to_string(fixture.0.join("mode-trace.ndjson"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).expect("trace JSON"))
            .collect();
        if succeeds {
            assert_eq!(text, "MODE_OK");
            assert_eq!(trace.last().expect("prompt")["method"], "session/prompt");
            if scenario != "mode-none" {
                assert_eq!(trace.len(), 2);
                let expected = if plan { "plan" } else { "agent" };
                assert_eq!(
                    trace[0]["params"][if scenario.contains("legacy") {
                        "modeId"
                    } else {
                        "value"
                    }],
                    expected
                );
            }
        } else {
            assert!(text.is_empty());
            assert!(
                !trace
                    .iter()
                    .any(|entry| entry["method"] == "session/prompt"),
                "{scenario}: {trace:?}"
            );
        }
    }
}

#[tokio::test]
async fn reasoning_only_turn_gets_one_final_answer_request_in_the_same_session() {
    let fixture = Fixture::new("repair-success");
    let store = Arc::new(SessionStore::default());
    let (_shutdown, receiver) = watch::channel(false);
    let body =
        json!({"model":"composer-2.5","messages":[{"role":"user","content":"ORIGINAL_TASK_761"}]});
    let (text, calls) = collect(store.start(
        fixture.config(),
        serde_json::from_value(body).expect("request"),
        fixture.0.clone(),
        receiver,
        "repair".into(),
    ))
    .await;
    assert_eq!(text, "RECOVERED");
    assert!(calls.is_empty());
    let prompts: Vec<Value> = std::fs::read_to_string(fixture.0.join("repair-prompts.ndjson"))
        .expect("prompt trace")
        .lines()
        .map(|line| serde_json::from_str(line).expect("prompt JSON"))
        .collect();
    assert_eq!(prompts.len(), 2);
    assert_eq!(prompts[0]["sessionId"], prompts[1]["sessionId"]);
    assert!(
        prompts[0]["text"]
            .as_str()
            .expect("original")
            .contains("ORIGINAL_TASK_761")
    );
    assert!(
        !prompts[1]["text"]
            .as_str()
            .expect("repair")
            .contains("ORIGINAL_TASK_761"),
        "repair must not replay the original task"
    );
}

#[tokio::test]
async fn empty_recovery_never_replays_tools_failures_or_more_than_one_prompt() {
    for (scenario, expected_prompts) in [
        ("repair-repeated", 2),
        ("repair-native", 1),
        ("repair-permission", 1),
        ("repair-task", 1),
        ("repair-empty", 1),
        ("repair-error", 1),
        ("repair-cancelled", 1),
        ("repair-second-native", 2),
        ("repair-mcp-invalid", 1),
        ("repair-second-mcp-invalid", 2),
    ] {
        let fixture = Fixture::new(scenario);
        let store = Arc::new(SessionStore::default());
        let (_shutdown, receiver) = watch::channel(false);
        let body = json!({"model":"composer-2.5","messages":[{"role":"user","content":"original"}],"tools":[{"type":"function","function":{
            "name":"host_echo","parameters":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}
        }}]});
        let mut updates = store.start(
            fixture.config(),
            serde_json::from_value(body).expect("request"),
            fixture.0.clone(),
            receiver,
            scenario.into(),
        );
        loop {
            match tokio::time::timeout(Duration::from_secs(5), updates.recv())
                .await
                .expect("deadline")
                .expect("explicit terminal event")
            {
                Update::Error(_) => break,
                Update::Thinking(_) => (),
                update => panic!("{scenario}: unexpected success or tool replay: {update:?}"),
            }
        }
        let prompts =
            std::fs::read_to_string(fixture.0.join("repair-prompts.ndjson")).expect("prompt trace");
        assert_eq!(prompts.lines().count(), expected_prompts, "{scenario}");
    }
}

#[tokio::test]
async fn final_answer_recovery_uses_the_original_request_deadline() {
    for (scenario, expected_prompts) in [("repair-timeout", 1), ("repair-second-timeout", 2)] {
        let fixture = Fixture::new(scenario);
        let store = Arc::new(SessionStore::default());
        let (_shutdown, receiver) = watch::channel(false);
        let mut config = fixture.config();
        config.timeout = Duration::from_millis(300);
        let body =
            json!({"model":"composer-2.5","messages":[{"role":"user","content":"original"}]});
        let mut updates = store.start(
            config,
            serde_json::from_value(body).expect("request"),
            fixture.0.clone(),
            receiver,
            scenario.into(),
        );
        loop {
            match tokio::time::timeout(Duration::from_secs(1), updates.recv())
                .await
                .expect("original deadline")
                .expect("terminal event")
            {
                Update::Error(error) => {
                    assert_eq!(error.status, axum::http::StatusCode::GATEWAY_TIMEOUT);
                    break;
                }
                Update::Thinking(_) => (),
                update => panic!("{scenario}: unexpected update: {update:?}"),
            }
        }
        let prompts =
            std::fs::read_to_string(fixture.0.join("repair-prompts.ndjson")).expect("prompt trace");
        assert_eq!(prompts.lines().count(), expected_prompts, "{scenario}");
    }
}

#[tokio::test]
async fn slow_initial_reasoning_does_not_grant_recovery_a_fresh_timeout() {
    let fixture = Fixture::new("repair-delayed-second-timeout");
    let store = Arc::new(SessionStore::default());
    let (_shutdown, receiver) = watch::channel(false);
    let mut config = fixture.config();
    config.timeout = Duration::from_millis(650);
    let body = json!({"model":"composer-2.5","messages":[{"role":"user","content":"original"}]});
    let started = std::time::Instant::now();
    let mut updates = store.start(
        config,
        serde_json::from_value(body).expect("request"),
        fixture.0.clone(),
        receiver,
        "slow-repair".into(),
    );
    loop {
        match tokio::time::timeout(
            Duration::from_millis(850).saturating_sub(started.elapsed()),
            updates.recv(),
        )
        .await
        .expect("recovery must use the remaining budget")
        .expect("terminal event")
        {
            Update::Thinking(_) => (),
            Update::Error(error) => {
                assert_eq!(error.status, axum::http::StatusCode::GATEWAY_TIMEOUT);
                break;
            }
            update => panic!("unexpected update: {update:?}"),
        }
    }
    let prompts =
        std::fs::read_to_string(fixture.0.join("repair-prompts.ndjson")).expect("prompt trace");
    assert_eq!(
        prompts.lines().count(),
        2,
        "the original turn must leave time for one recovery prompt"
    );
}

#[tokio::test]
async fn each_user_turn_has_one_recovery_in_its_own_session() {
    let fixture = Fixture::new("repair-per-turn");
    let store = Arc::new(SessionStore::default());
    let (shutdown, receiver) = watch::channel(false);
    let mut body =
        json!({"model":"composer-2.5","messages":[{"role":"user","content":"first task"}]});
    for turn in 0..2 {
        let (text, calls) = collect(store.clone().start(
            fixture.config(),
            serde_json::from_value(body.clone()).expect("request"),
            fixture.0.clone(),
            receiver.clone(),
            format!("repair-turn-{turn}"),
        ))
        .await;
        assert_eq!(text, "RECOVERED");
        assert!(calls.is_empty());
        body["messages"].as_array_mut().expect("messages").extend([
            json!({"role":"assistant","content":text}),
            json!({"role":"user","content":"next task"}),
        ]);
    }
    let prompts: Vec<Value> = std::fs::read_to_string(fixture.0.join("repair-prompts.ndjson"))
        .expect("prompt trace")
        .lines()
        .map(|line| serde_json::from_str(line).expect("JSON"))
        .collect();
    assert_eq!(prompts.len(), 4);
    assert_eq!(prompts[0]["sessionId"], prompts[1]["sessionId"]);
    assert_eq!(prompts[2]["sessionId"], prompts[3]["sessionId"]);
    assert_ne!(prompts[0]["sessionId"], prompts[2]["sessionId"]);
    shutdown.send(true).expect("shutdown");
}

#[tokio::test]
async fn tool_result_continues_the_same_acp_prompt_across_host_requests() {
    let fixture = Fixture::new("host-tool");
    let store = Arc::new(SessionStore::default());
    let (_shutdown, receiver) = watch::channel(false);
    let mut body = json!({"model":"composer-2.5","messages":[{"role":"user","content":"Use host_echo"}],"tools":[{"type":"function","function":{
        "name":"host_echo","parameters":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}
    }}]});
    let request: ChatRequest = serde_json::from_value(body.clone()).expect("request");
    let (text, calls) = collect(store.clone().start(
        fixture.config(),
        request,
        fixture.0.clone(),
        receiver.clone(),
        "first".to_owned(),
    ))
    .await;
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].function.name, "host_echo");
    let messages = body["messages"].as_array_mut().expect("messages");
    messages.push(json!({"role":"assistant","content":if text.is_empty() {Value::Null} else {json!(text)},"tool_calls":calls}));
    messages.push(json!({"role":"tool","tool_call_id":calls[0].id,"content":"HOST_RESULT"}));
    let request: ChatRequest = serde_json::from_value(body).expect("continuation");
    let (text, calls) = collect(store.start(
        fixture.config(),
        request,
        fixture.0.clone(),
        receiver,
        "second".to_owned(),
    ))
    .await;
    assert_eq!(text, "HOST_RESULT");
    assert!(
        calls.is_empty(),
        "ACP tool status must not duplicate the host call"
    );
}

#[tokio::test]
async fn parallel_mcp_calls_continue_without_overlapping_or_replaying_prompts() {
    let fixture = Fixture::new("parallel-tools");
    for _ in 0..16 {
        let store = Arc::new(SessionStore::default());
        let (_shutdown, receiver) = watch::channel(false);
        let mut body = json!({"model":"composer-2.5","messages":[{"role":"user","content":"Use both tools"}],"tools":[{"type":"function","function":{
            "name":"host_echo","parameters":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}
        }}]});
        let mut ids = Vec::new();
        for turn in 0..3 {
            let request = serde_json::from_value(body.clone()).expect("request");
            let (text, calls) = collect(store.clone().start(
                fixture.config(),
                request,
                fixture.0.clone(),
                receiver.clone(),
                format!("parallel-{turn}"),
            ))
            .await;
            if turn == 2 {
                assert!(calls.is_empty());
                assert_eq!(text, "result-hello-1|result-hello-2");
                break;
            }
            assert_eq!(calls.len(), 1);
            assert!(!ids.contains(&calls[0].id));
            ids.push(calls[0].id.clone());
            let arguments: Value =
                serde_json::from_str(&calls[0].function.arguments).expect("args");
            let result = format!("result-{}", arguments["text"].as_str().expect("tool input"));
            let messages = body["messages"].as_array_mut().expect("messages");
            messages.push(json!({"role":"assistant","content":if text.is_empty() {Value::Null} else {json!(text)},"tool_calls":calls}));
            messages.push(json!({"role":"tool","tool_call_id":calls[0].id,"content":result}));
        }
    }
}

#[tokio::test]
async fn completed_turns_release_the_agent_and_start_new_conversations() {
    let fixture = Fixture::new("memory");
    let store = Arc::new(SessionStore::default());
    let (shutdown, receiver) = watch::channel(false);
    let mut body =
        json!({"model":"composer-2.5","messages":[{"role":"user","content":"REMEMBER:ALPHA"}]});
    let (first, _) = collect(store.clone().start(
        fixture.config(),
        serde_json::from_value(body.clone()).expect("first"),
        fixture.0.clone(),
        receiver.clone(),
        "memory-first".into(),
    ))
    .await;
    let pid: i32 = std::fs::read_to_string(fixture.0.join("acp-pid"))
        .expect("agent PID")
        .parse()
        .expect("numeric PID");
    tokio::time::timeout(Duration::from_secs(1), async {
        while nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("completed ACP agent must exit without an idle retention period");
    body["messages"].as_array_mut().expect("messages").extend([
        json!({"role":"assistant","content":first}),
        json!({"role":"user","content":"RECALL"}),
    ]);
    let (next, calls) = collect(store.clone().start(
        fixture.config(),
        serde_json::from_value(body.clone()).expect("next"),
        fixture.0.clone(),
        receiver.clone(),
        "memory-next".into(),
    ))
    .await;
    assert_ne!(next.split(':').next(), first.split(':').next());
    assert!(next.ends_with(":ALPHA"));
    assert!(calls.is_empty());
    body["messages"][0]["content"] = json!("REMEMBER:BETA");
    let (changed, _) = collect(store.start(
        fixture.config(),
        serde_json::from_value(body).expect("changed history"),
        fixture.0.clone(),
        receiver,
        "memory-other".into(),
    ))
    .await;
    assert!(changed.ends_with(":BETA"));
    assert_ne!(changed.split(':').next(), first.split(':').next());
    shutdown.send(true).expect("shutdown threads");
}

#[tokio::test]
async fn identical_initial_requests_do_not_share_an_active_thread() {
    let fixture = Fixture::new("memory");
    let store = Arc::new(SessionStore::default());
    let (shutdown, receiver) = watch::channel(false);
    let body =
        json!({"model":"composer-2.5","messages":[{"role":"user","content":"REMEMBER:SAME"}]});
    let first = store.clone().start(
        fixture.config(),
        serde_json::from_value(body.clone()).expect("first"),
        fixture.0.clone(),
        receiver.clone(),
        "same-first".into(),
    );
    let second = store.start(
        fixture.config(),
        serde_json::from_value(body).expect("second"),
        fixture.0.clone(),
        receiver,
        "same-second".into(),
    );
    let ((a, _), (b, _)) = tokio::join!(collect(first), collect(second));
    assert_ne!(a, b);
    assert!(a.ends_with(":SAME") && b.ends_with(":SAME"));
    shutdown.send(true).expect("shutdown threads");
}

#[tokio::test]
async fn native_tool_status_notifications_are_not_host_execution_requests() {
    let fixture = Fixture::new("native-status");
    let store = Arc::new(SessionStore::default());
    let (_shutdown, receiver) = watch::channel(false);
    let body =
        json!({"model":"composer-2.5","messages":[{"role":"user","content":"REMEMBER:NATIVE"}]});
    let (text, calls) = collect(store.start(
        fixture.config(),
        serde_json::from_value(body).expect("request"),
        fixture.0.clone(),
        receiver,
        "native".into(),
    ))
    .await;
    assert!(calls.is_empty());
    assert!(text.ends_with(":NATIVE"));
}

#[tokio::test]
async fn reply_backpressure_cannot_keep_a_timed_out_acp_process_alive() {
    let fixture = Fixture::new("flood");
    let store = Arc::new(SessionStore::default());
    let (_shutdown, receiver) = watch::channel(false);
    let mut config = fixture.config();
    config.timeout = Duration::from_millis(150);
    let request = serde_json::from_value(
        json!({"model":"composer-2.5","messages":[{"role":"user","content":"flood"}]}),
    )
    .expect("request");
    let updates = store.start(
        config,
        request,
        fixture.0.clone(),
        receiver,
        "backpressure".into(),
    );
    tokio::time::timeout(Duration::from_secs(2), async {
        while !fixture.0.join("acp-pid").exists() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("process started");
    let pid: i32 = std::fs::read_to_string(fixture.0.join("acp-pid"))
        .expect("pid")
        .parse()
        .expect("PID");
    tokio::time::timeout(Duration::from_secs(2), async {
        while nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None)
            != Err(nix::errno::Errno::ESRCH)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("timeout must release peer even while consumer stays connected without reading");
    drop(updates);
}

#[tokio::test]
async fn pre_tool_text_stays_before_the_host_result_handoff() {
    let fixture = Fixture::new("ordered-tools");
    for iteration in 0..10 {
        let store = Arc::new(SessionStore::default());
        let (_shutdown, receiver) = watch::channel(false);
        let mut body = json!({"model":"composer-2.5","messages":[{"role":"user","content":"Use host_echo"}],"tools":[{"type":"function","function":{
            "name":"host_echo","parameters":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}
        }}]});
        let first = store.clone().start(
            fixture.config(),
            serde_json::from_value(body.clone()).expect("first"),
            fixture.0.clone(),
            receiver.clone(),
            format!("ordered-{iteration}"),
        );
        let (text, calls) = collect(first).await;
        assert_eq!(
            text,
            "x".repeat(120),
            "pre-tool text must not leak into continuation"
        );
        body["messages"].as_array_mut().expect("messages").extend([
            json!({"role":"assistant","content":text,"tool_calls":calls}),
            json!({"role":"tool","tool_call_id":calls[0].id,"content":"RESULT"}),
        ]);
        let (text, calls) = collect(store.start(
            fixture.config(),
            serde_json::from_value(body).expect("continuation"),
            fixture.0.clone(),
            receiver,
            "ordered-result".into(),
        ))
        .await;
        assert_eq!(text, "RESULT");
        assert!(calls.is_empty());
    }
}

#[tokio::test]
async fn disconnect_during_initialization_releases_the_peer_before_setup_timeout() {
    let fixture = Fixture::new("hang-init");
    let store = Arc::new(SessionStore::default());
    let (_shutdown, receiver) = watch::channel(false);
    let request = serde_json::from_value(
        json!({"model":"composer-2.5","messages":[{"role":"user","content":"hello"}]}),
    )
    .expect("request");
    let updates = store.start(
        fixture.config(),
        request,
        fixture.0.clone(),
        receiver,
        "setup-disconnect".into(),
    );
    tokio::time::timeout(Duration::from_secs(2), async {
        while !fixture.0.join("acp-pid").exists() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("peer started");
    let pid: i32 = std::fs::read_to_string(fixture.0.join("acp-pid"))
        .expect("pid")
        .parse()
        .expect("PID");
    drop(updates);
    tokio::time::timeout(Duration::from_secs(1), async {
        while nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None)
            != Err(nix::errno::Errno::ESRCH)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("disconnected setup must kill its peer promptly");
}

#[tokio::test]
async fn pre_result_text_after_a_tool_handoff_is_rejected_not_reassigned() {
    let fixture = Fixture::new("late-tool-text");
    let store = Arc::new(SessionStore::default());
    let (_shutdown, receiver) = watch::channel(false);
    let mut body = json!({"model":"composer-2.5","messages":[{"role":"user","content":"Use host_echo"}],"tools":[{"type":"function","function":{
        "name":"host_echo","parameters":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}
    }}]});
    let (_, calls) = collect(store.clone().start(
        fixture.config(),
        serde_json::from_value(body.clone()).expect("first"),
        fixture.0.clone(),
        receiver.clone(),
        "late-first".into(),
    ))
    .await;
    body["messages"].as_array_mut().expect("messages").extend([
        json!({"role":"assistant","content":null,"tool_calls":calls}),
        json!({"role":"tool","tool_call_id":calls[0].id,"content":"RESULT"}),
    ]);
    let mut updates = store.start(
        fixture.config(),
        serde_json::from_value(body).expect("continuation"),
        fixture.0.clone(),
        receiver,
        "late-result".into(),
    );
    while let Some(update) = tokio::time::timeout(Duration::from_secs(5), updates.recv())
        .await
        .expect("deadline")
    {
        match update {
            Update::Error(_) => return,
            Update::Text(value) => assert!(
                !value.contains("LATE_PRE_TOOL"),
                "late text crossed the tool boundary"
            ),
            Update::Done(_) => {
                panic!("unassignable pre-result text must not produce a successful reply")
            }
            _ => (),
        }
    }
    panic!("missing explicit protocol error");
}

#[tokio::test]
async fn disconnect_cancels_a_stalled_mcp_handoff_barrier() {
    let fixture = Fixture::new("blocked-barrier");
    let store = Arc::new(SessionStore::default());
    let (_shutdown, receiver) = watch::channel(false);
    let body = json!({"model":"composer-2.5","messages":[{"role":"user","content":"Use host_echo"}],"tools":[{"type":"function","function":{
        "name":"host_echo","parameters":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}
    }}]});
    let updates = store.start(
        fixture.config(),
        serde_json::from_value(body).expect("request"),
        fixture.0.clone(),
        receiver,
        "barrier-disconnect".into(),
    );
    tokio::time::timeout(Duration::from_secs(2), async {
        while !fixture.0.join("barrier-reached").exists() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("barrier started");
    let pid: i32 = std::fs::read_to_string(fixture.0.join("acp-pid"))
        .expect("pid")
        .parse()
        .expect("PID");
    drop(updates);
    tokio::time::timeout(Duration::from_secs(1), async {
        while nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None)
            != Err(nix::errno::Errno::ESRCH)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("stalled ordering barrier must remain cancellable");
}

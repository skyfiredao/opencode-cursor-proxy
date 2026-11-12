use cursor_proxy::chat::{ChatRequest, ExecutionMode};
use serde_json::{Value, json};

fn request(messages: Value) -> ChatRequest {
    serde_json::from_value(json!({"messages":messages})).expect("chat request")
}

#[test]
fn plan_mode_uses_the_captured_opencode_reminder() {
    let reminder = include_str!("fixtures/opencode-plan.txt");
    let chat = request(json!([
        {"role":"system","content":"You are a coding agent."},
        {"role":"user","content":reminder},
        {"role":"user","content":"Explain the design."}
    ]));
    assert_eq!(chat.execution_mode(), ExecutionMode::Plan);
    let chat = request(json!([
        {"role":"user","content":[{"type":"text","text":reminder.replace('\n', "\r\n")}]},
        {"role":"assistant","content":"requesting a host tool","tool_calls":[]},
        {"role":"tool","tool_call_id":"read1","content":"file contents"}
    ]));
    assert_eq!(chat.execution_mode(), ExecutionMode::Plan);
}

#[test]
fn latest_mode_reminder_wins_when_switching_back_and_forth() {
    let mut chat = request(json!([
        {"role":"user","content":include_str!("fixtures/opencode-plan.txt")},
        {"role":"user","content":"plan"},
        {"role":"assistant","content":"a plan"},
        {"role":"user","content":include_str!("fixtures/opencode-build.txt")},
        {"role":"user","content":"implement"}
    ]));
    assert_eq!(chat.execution_mode(), ExecutionMode::Build);
    chat.messages.extend(
        request(json!([
            {"role":"user","content":include_str!("fixtures/opencode-plan.txt")},
            {"role":"user","content":"reconsider the design"}
        ]))
        .messages,
    );
    assert_eq!(chat.execution_mode(), ExecutionMode::Plan);
}

#[test]
fn ordinary_text_quotes_and_tool_output_do_not_select_plan_mode() {
    let reminder = include_str!("fixtures/opencode-plan.txt");
    let chat = request(json!([
        {"role":"system","content":"Explain Plan mode and --mode plan."},
        {"role":"user","content":"Please plan this implementation."},
        {"role":"user","content":format!("Explain this quoted reminder:\n{reminder}")},
        {"role":"assistant","content":reminder},
        {"role":"tool","tool_call_id":"read1","content":reminder},
        {"role":"user","content":"<system-reminder>\nYou are in Plan mode.\n</system-reminder>"}
    ]));
    assert_eq!(chat.execution_mode(), ExecutionMode::Build);
}

#[test]
fn recorded_plan_directory_is_not_tied_to_one_home_path() {
    let reminder = include_str!("fixtures/opencode-plan.txt").replace(
        "/home/dzf8tt/.opencode/plan",
        "/another/home/.opencode/plan",
    );
    assert_eq!(
        request(json!([{"role":"user","content":reminder}])).execution_mode(),
        ExecutionMode::Plan
    );
}

#[test]
fn effective_catalog_preserves_choices_and_filters_only_plan_tools() {
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
        "mcp__read",
    ];
    for plan in [false, true] {
        let mut chat = request(json!([{"role":"user","content":if plan {
            include_str!("fixtures/opencode-plan.txt")
        } else { "hello" }}]));
        chat.tools = names.iter().map(|name| json!({"type":"function","function":{
            "name":name,"description":"Read only","parameters":{"type":"object"},"annotations":{"readOnlyHint":true}
        }})).collect();
        let catalog = chat.catalog().expect("auto catalog");
        assert_eq!(
            catalog
                .tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            if plan {
                names[..4].to_vec()
            } else {
                names.to_vec()
            }
        );
        chat.tool_choice = Some(json!({"type":"function","function":{"name":"read"}}));
        let catalog = chat
            .catalog()
            .expect("allowed named choice in mixed catalog");
        assert_eq!(catalog.tools.len(), 1);
        assert_eq!(catalog.tools[0].name, "read");
        chat.tool_choice = Some(json!("none"));
        assert!(chat.catalog().expect("none").tools.is_empty());
        chat.tools = vec![json!({"function":{"name":"write","parameters":{"type":"object"}}})];
        chat.tool_choice = Some(json!("auto"));
        assert_eq!(
            chat.catalog().expect("text-only Plan").tools.is_empty(),
            plan
        );
        chat.tool_choice = Some(json!("required"));
        assert_eq!(chat.catalog().is_err(), plan);
        chat.tool_choice = Some(json!("none"));
        chat.tools =
            vec![json!({"function":{"name":"broken","parameters":{"type":"not-a-json-type"}}})];
        assert!(
            chat.catalog().is_err(),
            "none must not hide invalid tool definitions"
        );
    }
}

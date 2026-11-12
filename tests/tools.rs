use cursor_proxy::{
    chat::ChatRequest,
    stream::{AgentEvent, EventKind},
    tools::{BridgeBuffer, ToolCatalog, check_loop, render_history},
};
use serde_json::{Value, json};

fn catalog() -> ToolCatalog {
    ToolCatalog::parse(&[
        json!({"type":"function","function":{"name":"read","parameters":{
            "type":"object","properties":{"filePath":{"type":"string"}},"required":["filePath"],"additionalProperties":false}}}),
        json!({"type":"function","function":{"name":"shell","parameters":{
            "type":"object","properties":{"command":{"type":"string"},"workdir":{"type":"string"}},"required":["command"]}}}),
        json!({"type":"function","function":{"name":"write","parameters":{
            "type":"object","properties":{"filePath":{"type":"string"},"content":{"type":"string"}},"required":["filePath","content"]}}}),
        json!({"type":"function","function":{"name":"edit","parameters":{
            "type":"object","properties":{"filePath":{"type":"string"},"oldString":{"type":"string"},"newString":{"type":"string"}},
            "required":["filePath","oldString","newString"]}}}),
    ]).expect("tool definitions")
}

#[test]
fn recorded_native_read_becomes_a_host_read() {
    for line in include_str!("fixtures/tool_call.ndjson").lines() {
        let event: AgentEvent = serde_json::from_str(line).expect("event");
        if let EventKind::ToolCall {
            subtype,
            call_id,
            tool_call,
            name,
        } = event.kind
        {
            if subtype.as_deref() != Some("started") {
                continue;
            }
            let call = catalog()
                .intercept(
                    &tool_call,
                    name.as_deref(),
                    call_id.as_deref().expect("call id"),
                )
                .expect("intercept")
                .expect("read call");
            assert_eq!(call.function.name, "read");
            assert_eq!(
                serde_json::from_str::<Value>(&call.function.arguments).expect("arguments"),
                json!({"filePath":"/tmp/opencode/cp-ws/note.txt"})
            );
            return;
        }
    }
    panic!("fixture has no started read call");
}

#[test]
fn aliases_follow_advertised_names_and_schemas() {
    let call = catalog()
        .normalize("x", "runCommandToolCall", json!({"cmd":"pwd","cwd":"/tmp"}))
        .expect("shell alias");
    assert_eq!(call.function.name, "shell");
    assert_eq!(
        serde_json::from_str::<Value>(&call.function.arguments).expect("arguments"),
        json!({"command":"pwd","workdir":"/tmp"})
    );
    assert!(catalog().normalize("x", "unknown", json!({})).is_err());
    assert!(catalog().normalize("x", "read", json!({})).is_err());
    assert!(
        catalog()
            .normalize("x", "read", json!({"path":"a","filePath":"b"}))
            .is_err()
    );
    assert!(catalog().normalize("x", "read", json!({"path":4})).is_err());
}

#[test]
fn full_file_edit_can_reroute_but_bodyless_edit_cannot() {
    let call = catalog()
        .normalize(
            "x",
            "editToolCall",
            json!({"path":"a","streamContent":"all contents"}),
        )
        .expect("write reroute");
    assert_eq!(call.function.name, "write");
    assert_eq!(
        serde_json::from_str::<Value>(&call.function.arguments).expect("args"),
        json!({"filePath":"a","content":"all contents"})
    );
    assert!(
        catalog()
            .normalize("x", "edit", json!({"path":"a"}))
            .is_err()
    );
}

#[test]
fn bridge_holds_envelopes_until_complete_and_releases_plain_text() {
    let mut buffer = BridgeBuffer::default();
    assert!(buffer.push("```json\n{\"name\":\"write\",").is_none());
    assert!(
        buffer
            .push("\"arguments\":{\"path\":\"a\",\"content\":\"contents\"}}\n```")
            .is_none()
    );
    let (text, calls) = buffer.finish(&catalog(), "test").expect("bridge");
    assert!(text.is_empty());
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].function.name, "write");
    let mut plain = BridgeBuffer::default();
    assert_eq!(plain.push("Hello"), Some("Hello".to_owned()));
    assert_eq!(plain.push(" world"), Some(" world".to_owned()));
    let mut json_answer = BridgeBuffer::default();
    json_answer.push("{\"answer\":42}");
    assert_eq!(
        json_answer.finish(&catalog(), "x").expect("plain JSON").0,
        "{\"answer\":42}"
    );
}

#[test]
fn history_preserves_tool_identity_and_guards_repeated_failures() {
    let request: ChatRequest = serde_json::from_value(json!({"messages":[
        {"role":"user","content":"read a"},
        {"role":"assistant","tool_calls":[{"id":"a","type":"function","function":{"name":"read","arguments":"{\"filePath\":\"a\"}"}}]},
        {"role":"tool","tool_call_id":"a","content":"Error: not found"},
        {"role":"assistant","tool_calls":[{"id":"b","type":"function","function":{"name":"read","arguments":"{\"filePath\":\"a\"}"}}]},
        {"role":"tool","tool_call_id":"b","content":"Error: not found"}
    ]})).expect("request");
    let rendered = render_history(&request.messages);
    assert!(rendered.contains("TOOL_RESULT (name: read, call_id: a): Error: not found"));
    assert!(rendered.ends_with("Continue your response based on these results."));
    let same = catalog()
        .normalize("c", "read", json!({"filePath":"a"}))
        .expect("candidate");
    assert!(check_loop(&request.messages, &same).is_err());
    let different = catalog()
        .normalize("d", "read", json!({"filePath":"b"}))
        .expect("candidate");
    assert!(check_loop(&request.messages, &different).is_ok());
}

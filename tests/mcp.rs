use cursor_proxy::{chat::ExecutionMode, mcp::Bridge, tools::ToolCatalog};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::sync::mpsc;

#[tokio::test]
async fn plan_mcp_filters_discovery_and_rejects_direct_mutations() {
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
    for mode in [ExecutionMode::Plan, ExecutionMode::Build] {
        let catalog = ToolCatalog::parse(&names.iter().map(|name| json!({"function":{
            "name":name,"description":"Read only","parameters":{"type":"object"},"annotations":{"readOnlyHint":true}
        }})).collect::<Vec<_>>()).expect("catalog");
        let (sender, mut calls) = mpsc::channel(16);
        let bridge = Bridge::open(catalog, sender, mode).await.expect("bridge");
        let config = bridge.configuration();
        let url = config["url"].as_str().expect("url").to_owned();
        let auth = config["headers"][0]["value"]
            .as_str()
            .expect("credential")
            .to_owned();
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(3))
            .build()
            .expect("client");
        let rpc = |id, method: &'static str, params: Value| {
            let client = client.clone();
            let url = url.clone();
            let auth = auth.clone();
            async move {
                client
                    .post(url)
                    .header("Authorization", auth)
                    .header("Accept", "application/json, text/event-stream")
                    .header("MCP-Protocol-Version", "2025-11-25")
                    .json(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
                    .send()
                    .await
                    .expect("HTTP response")
                    .error_for_status()
                    .expect("HTTP status")
                    .json::<Value>()
                    .await
                    .expect("JSON")
            }
        };
        rpc(1, "initialize", json!({"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"test","version":"1"}})).await;
        let listed = rpc(2, "tools/list", json!({})).await;
        let expected = if mode == ExecutionMode::Plan {
            &names[..4]
        } else {
            &names[..]
        };
        let listed: Vec<_> = listed["result"]["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .map(|tool| tool["name"].as_str().expect("name"))
            .collect();
        assert_eq!(listed, expected);
        for (index, name) in names.iter().enumerate() {
            let response = tokio::spawn(rpc(
                index + 3,
                "tools/call",
                json!({"name":name,"arguments":{"filePath":"/home/test/.opencode/plan/plan.md"}}),
            ));
            if mode == ExecutionMode::Plan && index >= 4 {
                let denied = response.await.expect("denial");
                assert!(denied.get("error").is_some(), "{name}: {denied}");
                assert!(calls.try_recv().is_err(), "{name} reached host");
            } else {
                let call = tokio::time::timeout(Duration::from_secs(2), calls.recv())
                    .await
                    .expect("handoff deadline")
                    .expect("call");
                assert_eq!(call.call.function.name, *name);
                call.reply.send("HOST_OK".into()).expect("result");
                assert_eq!(
                    response.await.expect("response")["result"]["content"][0]["text"],
                    "HOST_OK"
                );
            }
        }
        bridge.close().await;
    }
}

#[tokio::test]
async fn mcp_tool_calls_wait_for_the_original_host_result() {
    let catalog = ToolCatalog::parse(&[json!({"type":"function","function":{
        "name":"host_echo","description":"Echo input","parameters":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}
    }})]).expect("catalog");
    let (sender, mut calls) = mpsc::channel(8);
    let bridge = Bridge::open(catalog, sender, ExecutionMode::Build)
        .await
        .expect("MCP server");
    let configuration = bridge.configuration();
    let url = configuration["url"].as_str().expect("URL").to_owned();
    let authorization = configuration["headers"][0]["value"]
        .as_str()
        .expect("internal credential")
        .to_owned();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .expect("HTTP client");
    let request = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
        "protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"test","version":"1"}
    }});
    let denied = client
        .post(&url)
        .json(&request)
        .send()
        .await
        .expect("unauthenticated response");
    assert_eq!(denied.status(), 401);
    let denied = client
        .post(&url)
        .header("Authorization", &authorization)
        .header("Origin", "https://untrusted.invalid")
        .json(&request)
        .send()
        .await
        .expect("origin rejection");
    assert_eq!(denied.status(), 403);
    let response: Value = client
        .post(&url)
        .header("Authorization", &authorization)
        .header("Accept", "application/json, text/event-stream")
        .json(&request)
        .send()
        .await
        .expect("initialize")
        .error_for_status()
        .expect("status")
        .json()
        .await
        .expect("initialize JSON");
    assert_eq!(response["result"]["protocolVersion"], "2025-11-25");
    let listed: Value = client
        .post(&url)
        .header("Authorization", &authorization)
        .header("Accept", "application/json, text/event-stream")
        .header("MCP-Protocol-Version", "2025-11-25")
        .json(&json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}))
        .send()
        .await
        .expect("tool list")
        .error_for_status()
        .expect("list status")
        .json()
        .await
        .expect("tools JSON");
    assert_eq!(listed["result"]["tools"][0]["name"], "host_echo");
    let http = tokio::spawn(async move {
        client.post(&url).header("Authorization", &authorization).header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", "2025-11-25")
            .json(&json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"host_echo","arguments":{"text":"hello"}}}))
            .send().await.expect("call response").error_for_status().expect("call status").json::<Value>().await.expect("result JSON")
    });
    let call = tokio::time::timeout(Duration::from_secs(2), calls.recv())
        .await
        .expect("call deadline")
        .expect("host call");
    assert_eq!(call.call.function.name, "host_echo");
    assert_eq!(
        serde_json::from_str::<Value>(&call.call.function.arguments).expect("args"),
        json!({"text":"hello"})
    );
    assert!(
        !http.is_finished(),
        "MCP must wait for OpenCode's tool result"
    );
    call.reply
        .send("HOST_RESULT".to_owned())
        .expect("deliver result");
    let response = http.await.expect("HTTP task");
    assert_eq!(response["result"]["content"][0]["text"], "HOST_RESULT");
    let configuration = bridge.configuration();
    let url = configuration["url"].as_str().expect("URL").to_owned();
    let authorization = configuration["headers"][0]["value"]
        .as_str()
        .expect("credential")
        .to_owned();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .expect("client");
    let request_url = url.clone();
    let request_auth = authorization.clone();
    let http = tokio::spawn(async move {
        reqwest::Client::new().post(request_url).header("Authorization", request_auth).header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", "2025-11-25")
            .json(&json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"host_echo","arguments":{"text":"cancel"}}}))
            .send().await.expect("cancelled call response")
    });
    let mut call = tokio::time::timeout(Duration::from_secs(2), calls.recv())
        .await
        .expect("pending cancellation")
        .expect("call");
    let response = client.post(&url).header("Authorization", &authorization).json(&json!({
        "jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":4,"reason":"test"}
    })).send().await.expect("cancel notification");
    assert_eq!(response.status(), 202);
    tokio::time::timeout(Duration::from_secs(2), call.reply.closed())
        .await
        .expect("host result receiver must close on cancellation");
    let response: Value = http
        .await
        .expect("HTTP task")
        .json()
        .await
        .expect("JSON error");
    assert!(response.get("error").is_some(), "{response}");
    bridge.close().await;
}

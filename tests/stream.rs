use cursor_proxy::stream::{AgentEvent, DeltaTracker, EventKind, LineBuffer, Usage};

#[test]
fn stream_is_independent_of_chunk_boundaries() {
    let fixture = include_bytes!("fixtures/text.ndjson");
    for boundary in 0..=fixture.len() {
        let mut lines = LineBuffer::default();
        let mut output = lines.push(&fixture[..boundary]).expect("first chunk");
        output.extend(lines.push(&fixture[boundary..]).expect("second chunk"));
        output.extend(lines.finish().expect("last line"));
        assert_eq!(output.len(), 11, "split at {boundary}");
        let mut tracker = DeltaTracker::default();
        let mut text = String::new();
        let mut thinking = String::new();
        for line in output {
            let event: AgentEvent = serde_json::from_str(&line).expect("recorded event");
            match event.kind {
                EventKind::Assistant { message } => {
                    if event.timestamp_ms.is_none() || event.model_call_id.is_some() {
                        continue;
                    }
                    let value = message["content"][0]["text"].as_str().expect("text part");
                    text.push_str(&tracker.text(value, true));
                }
                EventKind::Thinking { subtype, text } => {
                    thinking.push_str(&tracker.thinking(
                        text.as_deref().unwrap_or(""),
                        subtype.as_deref() == Some("delta"),
                    ));
                }
                _ => (),
            }
        }
        assert_eq!(text, "PONG");
        assert_eq!(thinking, "The user wants a reply of exactly \"PONG\".");
    }
}

#[test]
fn stream_preserves_utf8_split_across_chunks_and_final_unterminated_line() {
    let source = "你好\r\nworld".as_bytes();
    let mut buffer = LineBuffer::default();
    let mut lines = Vec::new();
    for byte in source {
        lines.extend(buffer.push(&[*byte]).expect("UTF-8 fragment"));
    }
    lines.extend(buffer.finish().expect("final line"));
    assert_eq!(lines, ["你好", "world"]);
}

#[test]
fn stream_rejects_oversized_lines_and_invalid_utf8() {
    let mut buffer = LineBuffer::default();
    assert!(buffer.push(&vec![b'a'; 8 * 1024 * 1024 + 1]).is_err());
    let mut buffer = LineBuffer::default();
    assert!(buffer.push(&[0xff, b'\n']).is_err());
}

#[test]
fn stream_unknown_event_is_ignored_and_usage_is_mapped() {
    let event: AgentEvent = serde_json::from_str(r#"{"type":"future_event","session_id":"test"}"#)
        .expect("unknown event");
    assert!(matches!(event.kind, EventKind::Unknown));
    let usage: Usage = serde_json::from_str(
        r#"{"inputTokens":5442,"outputTokens":27,"cacheReadTokens":4160,"cacheWriteTokens":0}"#,
    )
    .expect("usage");
    assert_eq!(usage.openai()["prompt_tokens"], 9602);
    assert_eq!(usage.openai()["total_tokens"], 9629);
}

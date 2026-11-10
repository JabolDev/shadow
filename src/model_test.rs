use super::*;

#[test]
fn sse_events_split_anywhere() {
    let stream =
        "event: a\ndata: {\"type\":\"ping\"}\n\n: comment\r\ndata: {\"x\":\ndata: 1}\r\n\r\n";
    for cut in 0..stream.len() {
        let mut sse = Sse::default();
        let mut out = Vec::new();
        sse.feed(&stream.as_bytes()[..cut], &mut out);
        sse.feed(&stream.as_bytes()[cut..], &mut out);
        assert_eq!(
            out,
            vec!["{\"type\":\"ping\"}".to_string(), "{\"x\":\n1}".to_string()],
            "cut at {cut}"
        );
    }
}

fn feed(a: &mut Assembler, events: &[Value]) {
    for e in events {
        a.on_event(e).unwrap();
    }
}

#[test]
fn assembles_text_tools_and_usage() {
    let mut a = Assembler::default();
    feed(
        &mut a,
        &[
            json!({ "type": "message_start", "message": { "usage": { "input_tokens": 7 } } }),
            json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } }),
            json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "Hel" } }),
            json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "lo" } }),
            json!({ "type": "content_block_stop", "index": 0 }),
            json!({ "type": "content_block_start", "index": 1, "content_block": { "type": "tool_use", "id": "t", "name": "Read", "input": {} } }),
            json!({ "type": "content_block_delta", "index": 1, "delta": { "type": "input_json_delta", "partial_json": "{\"file_pa" } }),
            json!({ "type": "content_block_delta", "index": 1, "delta": { "type": "input_json_delta", "partial_json": "th\":\"a\"}" } }),
        ],
    );
    // stopped here: only the complete block is kept
    assert_eq!(
        a.message().unwrap(),
        json!({ "role": "assistant", "content": [{ "type": "text", "text": "Hello" }] })
    );
    feed(
        &mut a,
        &[
            json!({ "type": "content_block_stop", "index": 1 }),
            json!({ "type": "message_delta", "delta": { "stop_reason": "tool_use" }, "usage": { "output_tokens": 9 } }),
            json!({ "type": "message_stop" }),
        ],
    );
    assert!(a.finished);
    let r = a.reply();
    assert_eq!(
        r.message["content"][1]["input"],
        json!({ "file_path": "a" })
    );
    assert_eq!(r.stop_reason, "tool_use");
    assert_eq!(r.usage, json!({ "input_tokens": 7, "output_tokens": 9 }));
    assert!(
        a.on_event(&json!({ "type": "error", "error": { "type": "overloaded_error" } }))
            .is_err()
    );
}

#[test]
fn body_splices_messages_and_marks_the_ends() {
    let m1 = r#"{"role":"user","content":[{"type":"text","text":"a"}]}"#.to_string();
    let m2 = r#"{"role":"assistant","content":[{"type":"text","text":"b"}]}"#.to_string();
    let m3 = r#"{"role":"user","content":[{"type":"text","text":"c"},{"type":"text","text":"d"}]}"#
        .to_string();
    let tools = vec![json!({ "name": "A" }), json!({ "name": "B" })];
    let system = json!([{ "type": "text", "text": "s1" }, { "type": "text", "text": "s2" }]);
    let mut req = Map::new();
    req.insert("max_tokens".into(), json!(5));
    let cache = json!({ "type": "ephemeral" });
    let body = String::from_utf8(super::body(
        "m",
        Some(&system),
        &tools,
        &[m1.clone(), m2.clone(), m3],
        &req,
        Some(&cache),
    ))
    .unwrap();
    let v: Value = serde_json::from_str(&body).unwrap();
    assert!(
        body.contains(&format!("[{m1},{m2},")),
        "earlier messages are spliced as they are"
    );
    assert_eq!(v["system"][0].get("cache_control"), None);
    assert_eq!(v["system"][1]["cache_control"], cache);
    assert_eq!(v["tools"][0].get("cache_control"), None);
    assert_eq!(v["tools"][1]["cache_control"], cache);
    assert_eq!(v["messages"][2]["content"][0].get("cache_control"), None);
    assert_eq!(v["messages"][2]["content"][1]["cache_control"], cache);
    assert_eq!(v["stream"], true);
    assert_eq!(v["max_tokens"], 5);
    let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        [
            "model",
            "system",
            "tools",
            "messages",
            "stream",
            "max_tokens"
        ]
    );
    // no marks when off; a thinking block or string content is not marked
    let plain = String::from_utf8(super::body(
        "m",
        None,
        &[],
        std::slice::from_ref(&m2),
        &Map::new(),
        None,
    ))
    .unwrap();
    assert!(!plain.contains("cache_control") && !plain.contains("\"system\""));
    let thinking =
        r#"{"role":"assistant","content":[{"type":"thinking","thinking":"x","signature":"s"}]}"#
            .to_string();
    let b = String::from_utf8(super::body(
        "m",
        None,
        &[],
        &[thinking],
        &Map::new(),
        Some(&cache),
    ))
    .unwrap();
    assert!(!b.contains("cache_control"));
    let s = r#"{"role":"user","content":"plain"}"#.to_string();
    let b =
        String::from_utf8(super::body("m", None, &[], &[s], &Map::new(), Some(&cache))).unwrap();
    assert!(!b.contains("cache_control"));
}

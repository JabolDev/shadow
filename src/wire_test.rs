use super::*;

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("shadow-unit-wire-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d.join("wire.jsonl")
}

#[test]
fn messages_come_back_byte_for_byte() {
    let p = tmp("bytes");
    let mut w = Wire::load(&p).unwrap();
    assert!(!w.exists);
    let user = json!({ "role": "user", "content": [{ "type": "text", "text": "héllo \"q\"" }] });
    let asst = json!({ "role": "assistant", "content": [{ "type": "tool_use", "id": "t1", "name": "Read", "input": { "z": 1.5, "a": [1, 2] } }] });
    w.push_cursor("c1");
    w.push_message(&user);
    w.push_model_call(42, None);
    w.push_message(&asst);
    w.commit().unwrap();
    let texts = w.messages.clone();
    let w2 = Wire::load(&p).unwrap();
    assert!(w2.exists);
    assert_eq!(w2.cursor.as_deref(), Some("c1"));
    assert_eq!(w2.last_model_call_at, Some(42));
    assert_eq!(w2.assistant_count, 1);
    // the unanswered call gets an Interrupted result (in memory until the next commit)
    assert_eq!(w2.messages.len(), 3);
    assert_eq!(&w2.messages[..2], &texts[..]);
    assert!(
        w2.messages[2].contains(INTERRUPTED) && w2.messages[2].contains("\"tool_use_id\":\"t1\"")
    );
    // key order is kept as written
    assert!(texts[1].find("\"z\"").unwrap() < texts[1].find("\"a\"").unwrap());
}

#[test]
fn torn_last_line_is_cut_at_the_next_write() {
    let p = tmp("torn");
    let mut w = Wire::load(&p).unwrap();
    w.push_message(&json!({ "role": "user", "content": "one" }));
    w.commit().unwrap();
    let mut f = OpenOptions::new().append(true).open(&p).unwrap();
    f.write_all(b"{\"kind\":\"message\",\"message\":{\"role\":\"assi")
        .unwrap();
    let mut w = Wire::load(&p).unwrap();
    assert_eq!(w.messages.len(), 1);
    w.push_message(&json!({ "role": "user", "content": "two" }));
    w.commit().unwrap();
    let text = std::fs::read_to_string(&p).unwrap();
    for line in text.lines() {
        serde_json::from_str::<Value>(line).unwrap();
    }
    assert_eq!(text.lines().count(), 2);
    assert!(text.ends_with('\n'));
}

#[test]
fn compaction_restarts_the_context() {
    let p = tmp("compact");
    let mut w = Wire::load(&p).unwrap();
    w.push_message(&json!({ "role": "user", "content": "a" }));
    w.push_message(&json!({ "role": "assistant", "content": [{ "type": "text", "text": "b" }] }));
    w.push_compaction(
        &json!({ "role": "user", "content": [{ "type": "text", "text": "summary" }] }),
        None,
    );
    w.push_message(&json!({ "role": "user", "content": "c" }));
    w.commit().unwrap();
    let w = Wire::load(&p).unwrap();
    assert_eq!(w.messages.len(), 2);
    assert!(w.messages[0].contains("summary"));
    assert_eq!(w.assistant_count, 0);
}

#[test]
fn a_bad_line_in_the_middle_is_an_error() {
    let p = tmp("bad");
    std::fs::write(&p, "{\"kind\":\"cursor\",\"cursor\":\"x\"}\nnot json\n{\"kind\":\"cursor\",\"cursor\":\"y\"}\n").unwrap();
    let err = Wire::load(&p).err().unwrap();
    assert!(err.to_string().contains("line 2"), "{err}");
}

#[test]
fn answered_calls_need_no_repair() {
    assert_eq!(
        unanswered_calls(r#"{"role":"assistant","content":[{"type":"text","text":"hi"}]}"#),
        None
    );
    assert_eq!(unanswered_calls(r#"{"role":"user","content":"x"}"#), None);
    assert_eq!(
        unanswered_calls(
            r#"{"role":"assistant","content":[{"type":"tool_use","id":"a","name":"Read","input":{}}]}"#
        ),
        Some(vec!["a".to_string()])
    );
}

#[test]
fn a_fade_marker_rebuilds_the_same_bytes_after_a_load() {
    let p = tmp("fade");
    let mut w = Wire::load(&p).unwrap();
    w.push_message(&json!({ "role": "user", "content": [{ "type": "text", "text": "go" }] }));
    w.push_message(&json!({ "role": "assistant", "content": [{ "type": "tool_use", "id": "a", "name": "Read", "input": {} }, { "type": "tool_use", "id": "b", "name": "Read", "input": {} }] }));
    let big = "x".repeat(4000);
    w.push_message(&json!({ "role": "user", "content": [
        { "type": "tool_result", "tool_use_id": "a", "content": [{ "type": "text", "text": big }] },
        { "type": "tool_result", "tool_use_id": "b", "content": [{ "type": "text", "text": "small" }], "is_error": true },
    ] }));
    w.push_model_call(7, Some(2000));
    let before = w.size();
    assert_eq!(before, 2000);
    let untouched = w.messages[..2].to_vec();
    let mut stubs = Map::new();
    stubs.insert("a".into(), "[stub of a]".into());
    w.push_fade(stubs);
    w.commit().unwrap();
    assert_eq!(
        &w.messages[..2],
        &untouched[..],
        "only the message holding the result changes"
    );
    assert!(w.messages[2].contains("[stub of a]") && !w.messages[2].contains(&big));
    assert!(w.messages[2].contains("\"is_error\":true") && w.messages[2].contains("small"));
    assert_eq!(before - w.size(), (4000 - "[stub of a]".len() as u64) / 4);
    assert!(w.faded.contains("a"));
    let w2 = Wire::load(&p).unwrap();
    assert_eq!(w2.messages, w.messages, "the same bytes after a restart");
    assert_eq!(w2.size(), w.size());
    // the log itself is append-only: the result is still in it
    assert!(std::fs::read_to_string(&p).unwrap().contains(&big));
}

#[test]
fn a_compaction_keeps_its_ledger_and_forgets_fades() {
    let p = tmp("ledger");
    let mut w = Wire::load(&p).unwrap();
    w.push_message(&json!({ "role": "user", "content": "a" }));
    let mut stubs = Map::new();
    stubs.insert("z".into(), "s".into());
    w.push_fade(stubs);
    let ledger = json!({ "files": ["/w/a.txt"], "latest": [] });
    w.push_compaction(
        &json!({ "role": "user", "content": "summary" }),
        Some(ledger.clone()),
    );
    w.commit().unwrap();
    let w2 = Wire::load(&p).unwrap();
    assert_eq!(w2.ledger, Some(ledger));
    assert!(w2.faded.is_empty());
    assert_eq!(
        w2.size(),
        ("{\"role\":\"user\",\"content\":\"summary\"}".len() / 4) as u64
    );
}

//! The wire log: resuming after kill -9 (P6), prefix stability (P7),
//! compaction (P8).

mod common;
use common::*;
use serde_json::{Value, json};
use std::time::Duration;

#[test]
fn kill_9_mid_turn_then_resume() {
    let script = vec![
        json!({ "text": "step one", "tool_calls": [{ "id": "g1", "name": "Glob", "input": { "pattern": "*" } }] }),
        json!({ "tool_calls": [{ "id": "b1", "name": "Bash", "input": { "command": "sleep 30" } }, { "id": "b2", "name": "Bash", "input": { "command": "echo second" } }] }),
    ];
    let mut e = Env::new("kill9", script, vec![]);
    e.start("k", "first input", json!({}));
    e.host
        .wait(
            &json!({ "method": "tool.start", "params": { "call": { "id": "b1" } } }),
            LONG,
        )
        .unwrap();
    e.host.kill();
    // no broken line
    let wire = e.wire("k");
    assert!(wire.ends_with('\n'));
    for l in wire.lines() {
        serde_json::from_str::<Value>(l).unwrap();
    }
    e.restart();
    let info = e.host.call("session.info", json!({ "session": "k" }));
    assert_eq!(info["result"]["exists"], true);
    assert_eq!(info["result"]["running"], false);
    e.fake.push([json!({ "text": "resumed" })]);
    let t = e.start("k", "second input", json!({}));
    assert_eq!(e.end(&t)["reason"], "done");
    let body = e.fake.bodies().last().unwrap().clone();
    let msgs = body["messages"].as_array().unwrap();
    let roles: Vec<&str> = msgs.iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(
        roles,
        ["user", "assistant", "user", "assistant", "user", "user"]
    );
    assert_eq!(msgs[0]["content"][0]["text"], "first input");
    assert_eq!(msgs[2]["content"][0]["tool_use_id"], "g1");
    let repaired = msgs[4]["content"].as_array().unwrap();
    assert_eq!(repaired.len(), 2);
    for (r, id) in repaired.iter().zip(["b1", "b2"]) {
        assert_eq!(r["tool_use_id"], id);
        assert_eq!(r["is_error"], true);
        assert_eq!(text(r), "Interrupted: the harness restarted.");
    }
    assert_eq!(msgs[5]["content"][0]["text"], "second input");
    for l in e.wire("k").lines() {
        serde_json::from_str::<Value>(l).unwrap();
    }
}

#[test]
fn prefix_is_stable_over_ten_steps_and_a_restart() {
    let mut e = Env::new("prefix", vec![], vec![]);
    write(&e.work.join("a.txt"), "alpha\n");
    let steps: Vec<Value> = vec![
        json!({ "text": "Looking.", "tool_calls": [{ "name": "Read", "input": { "file_path": "a.txt" } }, { "name": "Glob", "input": { "pattern": "*.txt" } }] }),
        json!({ "tool_calls": [{ "name": "Bash", "input": { "command": "printf 'x%.0s' $(seq 1 50)" } }] }),
        json!({ "thinking": "plan", "tool_calls": [{ "name": "Write", "input": { "file_path": "b.txt", "content": "new file\n" } }] }),
        json!({ "tool_calls": [{ "name": "Edit", "input": { "file_path": "b.txt", "old_string": "new", "new_string": "edited" } }] }),
        json!({ "tool_calls": [{ "name": "Grep", "input": { "pattern": "edited", "output_mode": "content", "-n": true } }] }),
        json!({ "tool_calls": [{ "name": "Read", "input": { "file_path": "a.txt" } }] }),
        json!({ "tool_calls": [{ "name": "Bash", "input": { "command": "exit 4" } }] }),
        json!({ "tool_calls": [{ "name": "Nope", "input": { "q": [1, 2.5, "ü"] } }] }),
        json!({ "tool_calls": [{ "name": "Glob", "input": { "pattern": "**/*" } }] }),
        json!({ "text": "Ten steps done." }),
    ];
    e.fake.push(steps);
    let t = e.start("p", "do ten things", json!({}));
    let end = e.end(&t);
    assert_eq!(
        (end["reason"].as_str(), end["steps"].as_u64()),
        (Some("done"), Some(10))
    );
    e.restart();
    e.fake.push([
        json!({ "tool_calls": [{ "name": "Read", "input": { "file_path": "b.txt" } }] }),
        json!({ "text": "after restart" }),
    ]);
    let t = e.start("p", "one more", json!({}));
    assert_eq!(e.end(&t)["reason"], "done");
    let raw = e.fake.requests();
    assert_eq!(raw.len(), 12);
    for i in 1..raw.len() {
        let prev = raw_messages(&raw[i - 1].body);
        let cur = raw_messages(&raw[i].body);
        let prev_open = &prev[..prev.len() - 1];
        assert!(
            cur.starts_with(prev_open),
            "request {} does not begin with request {}'s messages",
            i + 1,
            i
        );
        let (p, c) = (
            plain_messages(&serde_json::from_str(&raw[i - 1].body).unwrap()),
            plain_messages(&serde_json::from_str(&raw[i].body).unwrap()),
        );
        assert_eq!(&c[..p.len()], &p[..]);
    }
    // the system prompt and tools are the same bytes too
    let head = |b: &str| b[..b.find("\"messages\":").unwrap()].to_string();
    assert!(raw.iter().all(|r| head(&r.body) == head(&raw[0].body)));
}

#[test]
fn compaction_replaces_the_context() {
    let mut e = Env::new(
        "compact",
        vec![
            json!({ "text": "answer one" }),
            json!({ "text": "answer two" }),
        ],
        vec![],
    );
    for q in ["question one", "question two"] {
        let t = e.start("c", q, json!({}));
        e.end(&t);
    }
    // a failed summary call leaves the log unchanged
    let before = e.wire("c");
    e.fake.push([json!({ "status": 400, "body": { "type": "error", "error": { "type": "invalid_request_error", "message": "nope" } } })]);
    let r = e.host.call(
        "session.compact",
        json!({ "session": "c", "model": "fake-model", "endpoint": { "url": e.fake.url() } }),
    );
    assert_eq!(r["error"]["code"], "compaction_failed", "{r}");
    assert_eq!(e.wire("c"), before);
    e.host
        .wait_any(
            &json!({ "method": "compaction", "params": { "session": "c", "state": "failed" } }),
            LONG,
        )
        .unwrap();
    // a good one
    e.fake.push([json!({ "match": "Write a summary", "text": "The user asked two questions; both answered." })]);
    let r = e.host.call("session.compact", json!({ "session": "c", "model": "fake-model", "endpoint": { "url": e.fake.url() }, "system": [{ "type": "text", "text": "Be brief." }], "request": { "max_tokens": 500 } }));
    assert_eq!(
        r["result"],
        json!({ "summary": "The user asked two questions; both answered.", "messages_before": 4, "messages_after": 1 })
    );
    let ask = e.fake.bodies().last().unwrap().clone();
    assert_eq!(
        ask["messages"].as_array().unwrap().len(),
        5,
        "the chat plus the summary prompt"
    );
    assert_eq!(ask["max_tokens"], 500);
    e.fake
        .push([json!({ "text": "after" }), json!({ "text": "and again" })]);
    for q in ["question three", "question four"] {
        let t = e.start("c", q, json!({}));
        e.end(&t);
    }
    let bodies = e.fake.bodies();
    let third = &bodies[bodies.len() - 2];
    let msgs = third["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 2);
    assert!(
        msgs[0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .ends_with("The user asked two questions; both answered.")
    );
    assert_eq!(msgs[1]["content"][0]["text"], "question three");
    assert_eq!(third["system"][0]["text"], "Be brief.");
    let fourth = plain_messages(bodies.last().unwrap());
    assert_eq!(fourth.len(), 4);
    assert_eq!(&fourth[..2], &plain_messages(third)[..]);
    let info = e.host.call("session.info", json!({ "session": "c" }));
    assert_eq!(
        info["result"]["messages"], 5,
        "summary + two questions and answers"
    );
}

#[test]
fn compaction_waits_for_the_turn() {
    let mut e = Env::new(
        "compactbusy",
        vec![json!({ "text": "slow", "delay_ms": 200 })],
        vec![],
    );
    let t = e.start("cb", "q", json!({}));
    let r = e.host.call(
        "session.compact",
        json!({ "session": "cb", "model": "m", "endpoint": { "url": e.fake.url() } }),
    );
    assert_eq!(r["error"]["code"], "busy");
    e.end(&t);
    let empty = e.host.call(
        "session.compact",
        json!({ "session": "never-used", "model": "m", "endpoint": { "url": e.fake.url() } }),
    );
    assert_eq!(empty["error"]["code"], "compaction_failed");
    std::thread::sleep(Duration::from_millis(10));
}

#[test]
fn catchup_and_cursor_are_logged() {
    let mut e = Env::new("catchup", vec![json!({ "text": "ok" })], vec![]);
    let catchup = json!([
        { "role": "user", "content": [{ "type": "text", "text": "earlier question" }] },
        { "role": "assistant", "content": [{ "type": "text", "text": "earlier answer" }] },
    ]);
    let t = e.start(
        "cu",
        "now",
        json!({ "catchup": catchup, "cursor": "rec-42" }),
    );
    assert_eq!(e.end(&t)["cursor"], "rec-42");
    let msgs = e.fake.bodies()[0]["messages"].as_array().unwrap().clone();
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[0]["content"][0]["text"], "earlier question");
    assert_eq!(msgs[2]["content"][0]["text"], "now");
    let info = e.host.call("session.info", json!({ "session": "cu" }));
    assert_eq!(info["result"]["cursor"], "rec-42");
}

//! P9 (memory, latency) and P10 (35 chats at once). The targets are checked
//! on a release build (`cargo test --release --test load`); a debug build
//! only prints the numbers.

mod common;
use common::*;
use serde_json::json;
use std::time::{Duration, Instant};

const RELEASE: bool = !cfg!(debug_assertions);

/// Writes a wire log of about `bytes` bytes: turns with tool calls and results.
fn big_wire(path: &std::path::Path, bytes: usize) -> usize {
    let mut out = String::new();
    let mut i = 0;
    while out.len() < bytes {
        let user = json!({ "role": "user", "content": [{ "type": "text", "text": format!("question {i}: what is in file {i}?") }] });
        let asst = json!({ "role": "assistant", "content": [{ "type": "text", "text": "Reading it." }, { "type": "tool_use", "id": format!("t{i}"), "name": "Read", "input": { "file_path": format!("/src/file{i}.rs") } }] });
        let body: String = (1..=60)
            .map(|n| format!("{n:>6}\tlet value_{n} = compute({n}, \"text\");\n"))
            .collect();
        let result = json!({ "role": "user", "content": [{ "type": "tool_result", "tool_use_id": format!("t{i}"), "content": [{ "type": "text", "text": body }] }] });
        let answer = json!({ "role": "assistant", "content": [{ "type": "text", "text": "It defines sixty values." }] });
        for m in [user, asst, result, answer] {
            out.push_str(&format!("{{\"kind\":\"message\",\"message\":{m}}}\n"));
        }
        i += 1;
    }
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, &out).unwrap();
    out.len()
}

#[test]
fn memory_and_latency() {
    let mut e = Env::new("p9", vec![], vec![]);
    let pid = e.host.pid().unwrap();
    // warm: one small turn
    e.fake.push([json!({ "text": "warm" })]);
    let t = e.start("warm", "hi", json!({}));
    e.end(&t);
    std::thread::sleep(Duration::from_millis(200));
    let idle = rss_kb(pid);
    let size = big_wire(&e.data.join("sessions/big/wire.jsonl"), 800 * 1024);
    // a turn on the big chat that stays running for a while
    e.fake
        .push([json!({ "text": "slow answer", "delay_ms": 150 })]);
    let sent = Instant::now();
    let t = e.start("big", "one more question", json!({}));
    std::thread::sleep(Duration::from_millis(400));
    let running = rss_kb(pid);
    assert_eq!(e.end(&t)["reason"], "done");
    let req = e.fake.requests().last().unwrap().clone();
    let logged = std::fs::read_to_string(e.data.join("sessions/big/wire.jsonl"))
        .unwrap()
        .lines()
        .filter(|l| l.contains("\"kind\":\"message\""))
        .count();
    let body: serde_json::Value = serde_json::from_str(&req.body).unwrap();
    assert_eq!(
        body["messages"].as_array().unwrap().len(),
        logged - 1,
        "the request holds the whole chat (the last logged message is the reply)"
    );
    let latency = req.at.duration_since(sent);
    eprintln!(
        "P9: idle RSS {idle} kB; wire log {size} bytes; RSS with that chat running {running} kB (+{} kB); turn.start → request head {latency:?}",
        running.saturating_sub(idle)
    );
    if RELEASE {
        assert!(idle <= 30 * 1024, "idle {idle} kB");
        assert!(
            running.saturating_sub(idle) <= 20 * 1024,
            "running chat added {} kB",
            running - idle
        );
        assert!(latency <= Duration::from_millis(50), "{latency:?}");
    }
}

#[test]
fn thirty_five_chats_at_once() {
    let call = json!({ "repeat": true, "match": "\"text\"", "tool_calls": [{ "name": "Bash", "input": { "command": "sleep 0.5; echo done" } }], "delay_ms": 5 });
    let answer =
        json!({ "repeat": true, "match": "tool_result", "text": "finished", "delay_ms": 5 });
    let mut e = Env::new("p10", vec![answer, call], vec![]);
    let pid = e.host.pid().unwrap();
    let before = rss_kb(pid);
    let started = Instant::now();
    let turns: Vec<String> = (0..35)
        .map(|i| e.start(&format!("chat{i}"), &format!("task {i}"), json!({})))
        .collect();
    let mut peak = 0;
    while e
        .host
        .lines()
        .iter()
        .filter(|l| l["method"] == "turn.end")
        .count()
        < 35
    {
        peak = peak.max(rss_kb(pid));
        assert!(started.elapsed() < Duration::from_secs(30));
        std::thread::sleep(Duration::from_millis(20));
    }
    for t in &turns {
        let end = e.end(t);
        assert_eq!(end["reason"], "done", "{end}");
        assert_eq!(end["steps"], 2);
    }
    let after = rss_kb(pid);
    eprintln!(
        "P10: 35 chats done in {:?}; RSS before {before} kB, peak {peak} kB, after {after} kB",
        started.elapsed()
    );
    if RELEASE {
        assert!(peak <= 30 * 1024 + 35 * 20 * 1024);
    }
}

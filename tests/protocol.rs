//! Protocol v1: requests, the order of a full turn (P2), errors, retries,
//! cache marks and the tool list.

mod common;
use common::*;
use serde_json::{Value, json};
use std::time::Duration;

#[test]
fn tools_command_prints_eight_small_definitions() {
    let out = std::process::Command::new(shadow_bin())
        .arg("tools")
        .output()
        .unwrap();
    assert!(out.status.success());
    let defs: Vec<Value> = serde_json::from_slice(&out.stdout).unwrap();
    let names: Vec<&str> = defs.iter().map(|d| d["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        [
            "Read", "Write", "Edit", "Bash", "TaskStop", "Grep", "Glob", "Branch"
        ]
    );
    assert!(out.stdout.len() <= 12_000, "{} bytes", out.stdout.len());
    for d in &defs {
        assert_eq!(d["input_schema"]["type"], "object");
    }
}

#[test]
fn requests_before_and_after_initialize() {
    let data = tmpdir("init");
    let mut h =
        shadow_testkit::host::ScriptedHost::start(&[shadow_bin(), "serve".into()], vec![], false)
            .unwrap();
    let r = h.call("session.info", json!({ "session": "a" }));
    assert_eq!(r["error"]["code"], "not_initialized");
    let r = h.call("initialize", json!({ "protocol": 2, "data_dir": data }));
    assert_eq!(r["error"]["code"], "bad_request");
    let r = h.call(
        "initialize",
        json!({ "protocol": 1, "data_dir": data, "client": { "name": "t", "version": "0" } }),
    );
    assert_eq!(r["result"]["protocol"], 1);
    assert_eq!(r["result"]["pid"], h.pid().unwrap());
    let r = h.call("session.info", json!({ "session": "a" }));
    assert_eq!(
        r["result"],
        json!({ "exists": false, "cursor": null, "messages": 0, "last_model_call_at": null, "running": false, "tasks": 0 })
    );
    let r = h.call("session.info", json!({ "session": "../escape" }));
    assert_eq!(r["error"]["code"], "bad_request");
    let r = h.call("no.such", json!({}));
    assert_eq!(r["error"]["code"], "unknown_method");
    // every line carries seq 1, 2, 3 …
    let seqs: Vec<u64> = h
        .lines()
        .iter()
        .map(|l| l["seq"].as_u64().unwrap())
        .collect();
    assert_eq!(seqs, (1..=seqs.len() as u64).collect::<Vec<_>>());
    let r = h.call("shutdown", json!({}));
    assert_eq!(r["result"], json!({}));
    assert_eq!(h.close(), Some(0));
}

#[test]
fn full_turn_order() {
    let rules = vec![
        json!({ "method": "tool.check", "match": { "call": { "name": "Bash" } }, "reply": { "decision": "deny", "message": "No shell today." } }),
        json!({ "method": "tool.check", "match": { "call": { "name": "Read" } }, "reply": { "decision": "allow" }, "delay_ms": 100 }),
        json!({ "method": "tool.call", "match": { "call": { "name": "notes" } }, "reply": { "content": [{ "type": "text", "text": "noted" }] } }),
    ];
    let mut e = Env::new("p2", vec![], rules);
    write(&e.work.join("one.txt"), "first\n");
    write(&e.work.join("two.txt"), "second\nline\n");
    let marker = e.work.join("denied-marker");
    e.fake.push([
        json!({ "text": "Reading both.", "tool_calls": [
            { "id": "r1", "name": "Read", "input": { "file_path": "one.txt" } },
            { "id": "r2", "name": "Read", "input": { "file_path": e.work.join("two.txt") } },
            { "id": "b1", "name": "Bash", "input": { "command": format!("touch {}", marker.display()) } },
            { "id": "h1", "name": "notes", "input": { "text": "x" } },
            { "id": "x1", "name": "Invented", "input": {} },
        ]}),
        json!({ "text": "Done." }),
    ]);
    let host_tools = json!([{ "name": "notes", "description": "Keep a note", "input_schema": { "type": "object" } }]);
    let turn = e.start(
        "chat",
        "go",
        json!({ "host_tools": host_tools, "cursor": "cur-7" }),
    );
    let end = e.end(&turn);
    assert_eq!(end["reason"], "done");
    assert_eq!(end["steps"], 2);
    assert_eq!(end["cursor"], "cur-7");
    assert!(!marker.exists(), "the denied call ran");

    // The order of what the host saw for this turn.
    let seen: Vec<String> = e
        .host
        .lines()
        .iter()
        .filter(|l| l["params"]["turn"] == turn.as_str())
        .map(|l| {
            let p = &l["params"];
            let m = l["method"].as_str().unwrap();
            match m {
                "model.event" => format!("event:{}", p["event"]["type"].as_str().unwrap()),
                "tool.check" | "tool.start" | "tool.call" => {
                    format!("{m}:{}", p["call"]["id"].as_str().unwrap())
                }
                "tool.end" => format!("tool.end:{}", p["call_id"].as_str().unwrap()),
                "step.start" => format!("step.start:{}", p["step"]),
                _ => m.to_string(),
            }
        })
        .filter(|s| !s.starts_with("event:content_block_delta"))
        .collect();
    let pos = |s: &str| {
        seen.iter()
            .position(|x| x == s)
            .unwrap_or_else(|| panic!("{s} missing in {seen:?}"))
    };
    assert_eq!(seen[0], "step.start:1");
    assert_eq!(seen[1], "event:message_start");
    assert!(!seen.iter().any(|s| s == "event:ping"));
    assert!(pos("event:message_stop") < pos("message.end"));
    // the two Reads: both checks go out (in call order) before either starts
    assert_eq!(pos("tool.check:r1") + 1, pos("tool.check:r2"));
    assert!(
        pos("tool.check:r2") < pos("tool.start:r1") && pos("tool.check:r2") < pos("tool.start:r2")
    );
    assert!(pos("tool.end:r1").max(pos("tool.end:r2")) < pos("tool.check:b1"));
    // denied: no tool.start / tool.end
    assert!(
        !seen
            .iter()
            .any(|s| s == "tool.start:b1" || s == "tool.end:b1")
    );
    // host tool: tool.start (native false) then tool.call, no check
    assert!(
        pos("tool.check:b1") < pos("tool.start:h1") && pos("tool.start:h1") < pos("tool.call:h1")
    );
    assert!(
        !seen
            .iter()
            .any(|s| s == "tool.check:h1" || s == "tool.end:h1")
    );
    assert!(!seen.iter().any(|s| s.ends_with(":x1")));
    assert!(pos("tool.call:h1") < pos("step.start:2"));
    assert_eq!(seen.last().unwrap(), "turn.end");
    let start_h1 = e
        .host
        .lines()
        .into_iter()
        .find(|l| l["method"] == "tool.start" && l["params"]["call"]["id"] == "h1")
        .unwrap();
    assert_eq!(start_h1["params"]["native"], false);

    // The second request: results in call order, then nothing else.
    let bodies = e.fake.bodies();
    assert_eq!(bodies.len(), 2);
    let msgs = bodies[1]["messages"].as_array().unwrap();
    let results = msgs.last().unwrap()["content"].as_array().unwrap();
    let ids: Vec<&str> = results
        .iter()
        .map(|r| r["tool_use_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["r1", "r2", "b1", "h1", "x1"]);
    assert_eq!(text(&results[0]), "     1\tfirst");
    assert_eq!(text(&results[2]), "No shell today.");
    assert_eq!(results[2]["is_error"], true);
    assert_eq!(text(&results[3]), "noted");
    assert_eq!(text(&results[4]), "No such tool: Invented");
    // the body: model, system, tools (native, then host), messages, stream, then request
    let keys: Vec<&String> = bodies[1].as_object().unwrap().keys().collect();
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
    let tools: Vec<&str> = bodies[1]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        tools,
        [
            "Read", "Write", "Edit", "Bash", "TaskStop", "Grep", "Glob", "Branch", "notes"
        ]
    );
    let message_end = e
        .host
        .lines()
        .into_iter()
        .find(|l| l["method"] == "message.end")
        .unwrap();
    assert_eq!(message_end["params"]["stop_reason"], "tool_use");
    assert_eq!(message_end["params"]["message"], msgs[1]);
    let info = e.host.call("session.info", json!({ "session": "chat" }));
    assert_eq!(info["result"]["cursor"], "cur-7");
    assert_eq!(info["result"]["messages"], 4);
    assert!(info["result"]["last_model_call_at"].as_u64().is_some());
}

#[test]
fn cache_marks_and_native_subset() {
    let mut e = Env::new(
        "cache",
        vec![
            json!({ "text": "a" }),
            json!({ "text": "b" }),
            json!({ "text": "c" }),
        ],
        vec![],
    );
    let t = e.start("c", "one", json!({}));
    e.end(&t);
    let t = e.start(
        "c",
        "two",
        json!({ "cache": { "mode": "auto", "ttl": "1h" }, "native_tools": ["Glob", "Read"] }),
    );
    e.end(&t);
    let t = e.start("c", "three", json!({ "cache": { "mode": "off" } }));
    e.end(&t);
    let raw = e.fake.requests();
    let b: Vec<Value> = e.fake.bodies();
    assert_eq!(raw[0].body.matches("cache_control").count(), 3);
    assert_eq!(
        b[0]["system"][0]["cache_control"],
        json!({ "type": "ephemeral" })
    );
    assert_eq!(
        b[0]["tools"][7]["cache_control"],
        json!({ "type": "ephemeral" })
    );
    assert_eq!(
        b[0]["messages"][0]["content"][0]["cache_control"],
        json!({ "type": "ephemeral" })
    );
    let names: Vec<&str> = b[1]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["Read", "Glob"], "fixed order, only those offered");
    assert_eq!(
        b[1]["tools"][1]["cache_control"],
        json!({ "type": "ephemeral", "ttl": "1h" })
    );
    assert_eq!(raw[2].body.matches("cache_control").count(), 0);
    // marks are never logged
    assert!(!e.wire("c").contains("cache_control"));
}

#[test]
fn bad_turn_start_and_busy() {
    let mut e = Env::new(
        "bad",
        vec![json!({ "text": "slow", "delay_ms": 300 })],
        vec![],
    );
    let bad = |e: &mut Env, extra: Value| {
        e.host.call("turn.start", e.params("s", "x", extra))["error"]["code"].clone()
    };
    assert_eq!(bad(&mut e, json!({ "cwd": "relative/dir" })), "bad_request");
    assert_eq!(
        bad(&mut e, json!({ "native_tools": ["Read", "Nope"] })),
        "bad_request"
    );
    assert_eq!(
        bad(&mut e, json!({ "request": { "messages": [] } })),
        "bad_request"
    );
    assert_eq!(
        bad(
            &mut e,
            json!({ "endpoint": { "url": "ftp://127.0.0.1:1/x" } })
        ),
        "bad_request"
    );
    assert_eq!(bad(&mut e, json!({ "input": [] })), "bad_request");
    assert_eq!(
        bad(
            &mut e,
            json!({ "host_tools": [{ "name": "Read", "input_schema": {} }] })
        ),
        "bad_request"
    );
    assert_eq!(
        bad(&mut e, json!({ "cache": { "mode": "sometimes" } })),
        "bad_request"
    );
    assert_eq!(
        bad(&mut e, json!({ "limits": { "result_cap_chars": 50 } })),
        "bad_request"
    );
    assert_eq!(
        bad(&mut e, json!({ "limits": { "max_steps": 0 } })),
        "bad_request"
    );
    let t = e.start(
        "s",
        "x",
        json!({ "auto_compact": { "at": 0.85 }, "limits": { "context_tokens": 1000 } }),
    );
    assert_eq!(bad(&mut e, json!({})), "busy");
    assert_eq!(e.end(&t)["reason"], "done");
}

#[test]
fn retries_then_success() {
    let script = vec![
        json!({ "status": 529, "headers": { "retry-after": "0" } }),
        json!({ "status": 503, "headers": { "retry-after": "0.05" } }),
        json!({ "text": "partial", "break_after": 4 }),
        json!({ "text": "fine" }),
    ];
    let mut e = Env::new("retry", script, vec![]);
    let t = e.start("r", "hello", json!({}));
    let end = e.end(&t);
    assert_eq!(end["reason"], "done", "{end}");
    let retries: Vec<Value> = e
        .host
        .lines()
        .into_iter()
        .filter(|l| l["method"] == "retry")
        .map(|l| l["params"].clone())
        .collect();
    assert_eq!(retries.len(), 3, "{retries:?}");
    assert_eq!(retries[0]["status"], 529);
    assert_eq!(retries[0]["wait_ms"], 0);
    assert_eq!(retries[1]["wait_ms"], 50);
    assert_eq!(retries[2]["status"], Value::Null);
    assert_eq!(retries[2]["attempt"], 3);
    assert_eq!(retries[2]["wait_ms"], 4000, "1 s doubling");
    let wire = e.wire("r");
    assert!(
        wire.contains("\"fine\"") && !wire.contains("partial"),
        "{wire}"
    );
}

#[test]
fn client_error_ends_the_turn() {
    let body = json!({ "type": "error", "error": { "type": "invalid_request_error", "message": "max_tokens is too large" } });
    let mut e = Env::new(
        "err",
        vec![
            json!({ "status": 400, "body": body }),
            json!({ "text": "next" }),
        ],
        vec![],
    );
    let t = e.start("e", "hello", json!({}));
    let end = e.end(&t);
    assert_eq!(end["reason"], "error");
    assert_eq!(end["error"]["status"], 400);
    assert_eq!(end["error"]["body"], body);
    assert!(
        end["error"]["message"]
            .as_str()
            .unwrap()
            .contains("max_tokens is too large")
    );
    let t = e.start("e", "again", json!({}));
    assert_eq!(e.end(&t)["reason"], "done");
}

#[test]
fn retries_run_out() {
    let mut e = Env::new(
        "out",
        vec![json!({ "status": 500, "repeat": true, "headers": { "retry-after": "0" } })],
        vec![],
    );
    let t = e.start("o", "hello", json!({ "limits": { "retry_max": 2 } }));
    let end = e.end(&t);
    assert_eq!(end["reason"], "error");
    assert_eq!(end["error"]["status"], 500);
    assert_eq!(e.fake.requests().len(), 3);
}

#[test]
fn max_steps_ends_the_turn() {
    let call = json!({ "repeat": true, "tool_calls": [{ "name": "Glob", "input": { "pattern": "*.none" } }] });
    let mut e = Env::new("max", vec![call], vec![]);
    let t = e.start("m", "loop", json!({ "limits": { "max_steps": 3 } }));
    let end = e.end(&t);
    assert_eq!(end["reason"], "max_steps");
    assert_eq!(end["steps"], 3);
    assert_eq!(e.fake.requests().len(), 3);
    // the log is valid: the last step's results are there
    let last = e.wire("m").lines().last().unwrap().to_string();
    assert!(last.contains("tool_result"), "{last}");
}

#[test]
fn thinking_and_usage_totals() {
    let mut e = Env::new(
        "think",
        vec![
            json!({ "thinking": "hmm, let me see", "text": "Answer.", "usage": { "input_tokens": 100, "cache_read_input_tokens": 40 } }),
        ],
        vec![],
    );
    let t = e.start("t", "q", json!({}));
    let end = e.end(&t);
    assert_eq!(end["usage"]["input_tokens"], 100);
    assert_eq!(end["usage"]["cache_read_input_tokens"], 40);
    assert_eq!(end["usage"]["output_tokens"], 5);
    let msg = e
        .host
        .wait_any(&json!({ "method": "message.end" }), Duration::from_secs(1))
        .unwrap();
    assert_eq!(
        msg["params"]["message"]["content"][0],
        json!({ "type": "thinking", "thinking": "hmm, let me see", "signature": "sig-fake" })
    );
    assert_eq!(
        msg["params"]["message"]["content"][1],
        json!({ "type": "text", "text": "Answer." })
    );
}

#[test]
fn invalid_tool_input_is_retried_never_run() {
    // Attempt 1 streams a tool call whose input is cut off; attempt 2 is valid.
    let broken = json!({ "events": [
        json!({ "type": "message_start", "message": { "usage": { "input_tokens": 3 } } }),
        json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "tool_use", "id": "w1", "name": "memo", "input": {} } }),
        json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "input_json_delta", "partial_json": "{\"lines\": [\"a\", " } }),
        json!({ "type": "content_block_stop", "index": 0 }),
        json!({ "type": "message_delta", "delta": { "stop_reason": "tool_use" }, "usage": { "output_tokens": 2 } }),
        json!({ "type": "message_stop" }),
    ] });
    let rules = vec![json!({ "method": "tool.call", "reply": { "content": "memo kept" } })];
    let script = vec![
        broken,
        json!({ "tool_calls": [{ "id": "w2", "name": "memo", "input": { "lines": ["a", "b"] } }] }),
        json!({ "text": "done" }),
    ];
    let mut e = Env::new("badjson", script, rules);
    let host_tools = json!([{ "name": "memo", "description": "Keep lines", "input_schema": { "type": "object" } }]);
    let t = e.start("bj", "write it", json!({ "host_tools": host_tools }));
    let end = e.end(&t);
    assert_eq!(end["reason"], "done", "{end}");
    let calls: Vec<Value> = e
        .host
        .lines()
        .into_iter()
        .filter(|l| l["method"] == "tool.call")
        .map(|l| l["params"]["call"].clone())
        .collect();
    assert_eq!(
        calls,
        [json!({ "id": "w2", "name": "memo", "input": { "lines": ["a", "b"] } })],
        "only the valid call ran"
    );
    let retry = e
        .host
        .wait_any(&json!({ "method": "retry" }), Duration::from_secs(1))
        .unwrap();
    assert_eq!(
        (
            retry["params"]["attempt"].as_u64(),
            &retry["params"]["status"]
        ),
        (Some(1), &Value::Null)
    );
    assert!(
        !e.wire("bj").contains("\"w1\""),
        "the broken message is not logged"
    );
}

#[test]
fn split_signatures_reach_the_log_and_the_next_request() {
    let thinking = json!({ "events": [
        json!({ "type": "message_start", "message": { "usage": {} } }),
        json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "thinking", "thinking": "", "signature": "" } }),
        json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "thinking_delta", "thinking": "look first" } }),
        json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "signature_delta", "signature": "AbC" } }),
        json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "signature_delta", "signature": "dEf" } }),
        json!({ "type": "content_block_stop", "index": 0 }),
        json!({ "type": "content_block_start", "index": 1, "content_block": { "type": "tool_use", "id": "g", "name": "Glob", "input": {} } }),
        json!({ "type": "content_block_delta", "index": 1, "delta": { "type": "input_json_delta", "partial_json": "{\"pattern\":\"*.md\"}" } }),
        json!({ "type": "content_block_stop", "index": 1 }),
        json!({ "type": "message_delta", "delta": { "stop_reason": "tool_use" }, "usage": { "output_tokens": 2 } }),
        json!({ "type": "message_stop" }),
    ] });
    let mut e = Env::new("sig", vec![thinking, json!({ "text": "ok" })], vec![]);
    let t = e.start("sg", "think", json!({}));
    assert_eq!(e.end(&t)["reason"], "done");
    let sent = &e.fake.bodies()[1]["messages"][1]["content"][0];
    assert_eq!(sent["signature"], "AbCdEf");
    assert!(e.wire("sg").contains("\"signature\":\"AbCdEf\""));
}

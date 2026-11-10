//! The context engine and Branch: fade, cold wake, state delta, gauge, loop
//! breaker, ledger and auto-compaction, Branch (checks C1–C12 of the
//! context contract; C14 is in tests/run.rs).

mod common;
use common::*;
use serde_json::{Value, json};
use shadow_testkit::fake::FakeModel;
use shadow_testkit::host::ScriptedHost;
use std::time::{Duration, Instant};

/// A daemon whose prompt cache lives `ttl_ms` (the test-only variable).
fn env_ttl(name: &str, ttl_ms: u64, rules: Vec<Value>) -> Env {
    let fake = FakeModel::start(vec![], 0, None).unwrap();
    let data = tmpdir(&format!("{name}-data"));
    let work = tmpdir(&format!("{name}-work"));
    let cmd = [
        "/usr/bin/env".to_string(),
        format!("SHADOW_TEST_CACHE_TTL_MS={ttl_ms}"),
        shadow_bin(),
        "serve".into(),
    ];
    let mut host = ScriptedHost::start(&cmd, rules, false).unwrap();
    let r = host.call(
        "initialize",
        json!({ "protocol": 1, "data_dir": data, "client": { "name": "test", "version": "1" } }),
    );
    assert_eq!(r["result"]["protocol"], 1, "{r}");
    Env {
        host,
        fake,
        data,
        work,
    }
}

/// True when `next`'s messages begin with all of `prev`'s, byte for byte.
fn extends(prev: &Value, next: &Value) -> bool {
    let (p, n) = (plain_messages(prev), plain_messages(next));
    n.len() >= p.len() && n[..p.len()] == p[..]
}

fn calls(list: Value) -> Value {
    json!({ "tool_calls": list })
}

/// Every text block of a request's messages (tool results included).
fn all_text(body: &Value) -> String {
    body.to_string()
}

fn fades(wire: &str) -> Vec<Value> {
    wire.lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .filter(|l| l["kind"] == "fade")
        .collect()
}

/// `n` lines of `width` characters.
fn lines(n: usize, width: usize, tag: &str) -> String {
    (0..n)
        .map(|i| format!("{tag}{i:0>w$}", w = width - tag.len()))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

#[test]
fn c1_thirty_steps_under_60_percent_rewrite_nothing() {
    let mut e = Env::new("c1", vec![], vec![]);
    for i in 0..30 {
        write(
            &e.work.join(format!("f{i}.txt")),
            &lines(80, 40, &format!("f{i}-")),
        );
        e.fake.push([calls(
            json!([{ "name": "Read", "input": { "file_path": format!("f{i}.txt") } }]),
        )]);
    }
    e.fake.push([json!({ "text": "read them all" })]);
    let t = e.start(
        "c1",
        "read the files",
        json!({ "limits": { "context_tokens": 200000, "max_steps": 40 } }),
    );
    assert_eq!(e.end(&t)["reason"], "done");
    let b = e.fake.bodies();
    assert_eq!(b.len(), 31);
    for i in 1..b.len() {
        assert!(
            extends(&b[i - 1], &b[i]),
            "request {} rewrote the context",
            i + 1
        );
    }
    assert!(fades(&e.wire("c1")).is_empty());
    assert!(
        !all_text(&b[30]).contains("Context: "),
        "no gauge under 50%"
    );
}

#[test]
fn c2_crossing_60_percent_fades_once_and_survives_a_restart() {
    let mut e = Env::new("c2", vec![], vec![]);
    for i in 1..=21 {
        write(
            &e.work.join(format!("big{i}.txt")),
            &lines(100, 60, &format!("b{i}-")),
        );
        let tokens = if i < 21 { 500 * i } else { 26_000 };
        e.fake.push([json!({ "tool_calls": [{ "id": format!("c{i}"), "name": "Bash", "input": { "command": format!("cat big{i}.txt") } }], "usage": { "input_tokens": tokens } })]);
    }
    e.fake
        .push([json!({ "text": "done", "usage": { "input_tokens": 9000 } })]);
    let params = json!({ "limits": { "context_tokens": 40000, "max_steps": 40 } });
    let t = e.start("c2", "look", params.clone());
    assert_eq!(e.end(&t)["reason"], "done");
    let b = e.fake.bodies();
    assert_eq!(b.len(), 22);
    let rewrites: Vec<usize> = (1..b.len())
        .filter(|&i| !extends(&b[i - 1], &b[i]))
        .collect();
    assert_eq!(
        rewrites,
        [21],
        "exactly one rewrite: the request after step 21"
    );
    let wire = e.wire("c2");
    let marks = fades(&wire);
    assert_eq!(marks.len(), 1);
    let ids: Vec<String> = marks[0]["stubs"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    // oldest first; never one of the last 10 steps (12..21)
    let expected: Vec<String> = (1..=ids.len()).map(|i| format!("c{i}")).collect();
    assert_eq!(ids, expected);
    assert!(ids.len() <= 11);
    let (before, after) = (plain_messages(&b[20]), plain_messages(&b[21]));
    for (i, (x, y)) in before.iter().zip(&after).enumerate() {
        let (x, y): (Value, Value) = (
            serde_json::from_str(x).unwrap(),
            serde_json::from_str(y).unwrap(),
        );
        if x["role"] == "assistant" || i == 0 {
            assert_eq!(
                x, y,
                "the model's messages and the user's are never touched"
            );
        }
    }
    // each stub names the tool, its argument, what it returned and how to get it back
    let stub = marks[0]["stubs"]["c1"].as_str().unwrap();
    assert_eq!(
        stub,
        "[Faded to save context: Bash cat big1.txt returned 100 lines (6099 characters) at step 1. Run it again if you need it.]"
    );
    assert!(all_text(&b[21]).contains(stub));
    // the context after the fade: at most 40% of the window, or nothing left to fade
    let chars = |m: &[String]| m.iter().map(|s| s.chars().count() as u64).sum::<u64>();
    let reported = 26_000 + 5;
    let at_report = chars(&before) + plain_messages(&b[21])[before.len()].chars().count() as u64;
    let now = reported as i64 + (chars(&after) as i64 - at_report as i64) / 4;
    assert!(
        now <= 16_000 || ids.len() == 11,
        "{now} tokens with {} faded",
        ids.len()
    );
    // a restart rebuilds the same bytes: the next turn extends the last request
    e.restart();
    e.fake.push([json!({ "text": "again" })]);
    let t = e.start("c2", "and now?", params);
    assert_eq!(e.end(&t)["reason"], "done");
    let b = e.fake.bodies();
    assert!(
        extends(&b[21], &b[22]),
        "the context after a restart is the same bytes"
    );
}

#[test]
fn c3_c4_a_cold_turn_fades_first_and_a_faded_read_reads_again() {
    let mut e = env_ttl("c34", 1500, vec![]);
    write(&e.work.join("long.txt"), &lines(300, 40, "L"));
    e.fake.push([
        calls(json!([{ "id": "r1", "name": "Read", "input": { "file_path": "long.txt", "limit": 100 } }])),
        calls(json!([{ "id": "r2", "name": "Read", "input": { "file_path": "long.txt", "offset": 101, "limit": 100 } }])),
    ]);
    for i in 0..10 {
        e.fake.push([calls(
            json!([{ "name": "Glob", "input": { "pattern": format!("*.{i}") } }]),
        )]);
    }
    e.fake.push([json!({ "text": "ok" })]);
    let t = e.start("c34", "read", json!({}));
    assert_eq!(e.end(&t)["reason"], "done");
    // warm: nothing changes
    e.fake.push([json!({ "text": "warm" })]);
    let t = e.start("c34", "still there?", json!({}));
    assert_eq!(e.end(&t)["reason"], "done");
    let b = e.fake.bodies();
    assert!(extends(&b[12], &b[13]), "a warm turn rewrites nothing");
    assert!(fades(&e.wire("c34")).is_empty());
    // cold: the first request already has the candidates faded
    std::thread::sleep(Duration::from_millis(1600));
    e.fake.push([
        calls(json!([
            { "id": "r3", "name": "Read", "input": { "file_path": "long.txt", "limit": 100 } },
            { "id": "r4", "name": "Read", "input": { "file_path": "long.txt", "offset": 101, "limit": 100 } },
        ])),
        json!({ "text": "cold" }),
    ]);
    let t = e.start("c34", "read again", json!({}));
    assert_eq!(e.end(&t)["reason"], "done");
    let b = e.fake.bodies();
    let marks = fades(&e.wire("c34"));
    assert_eq!(marks.len(), 1);
    assert_eq!(
        marks[0]["stubs"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["r1"]
    );
    let first_cold = all_text(&b[14]);
    assert!(first_cold.contains("[Faded to save context: Read long.txt returned 102 lines (4864 characters) at step 1. Run it again if you need it.]"));
    assert!(!extends(&b[13], &b[14]));
    assert!(extends(&b[14], &b[15]));
    // r3 reads lines 1-100 again: their result was faded, so the text comes back
    let last = plain_messages(&b[15]).last().cloned().unwrap();
    let last: Value = serde_json::from_str(&last).unwrap();
    let r3 = last["content"][0]["content"][0]["text"].as_str().unwrap();
    assert!(r3.starts_with("     1\tL"), "{r3}");
    let r4 = last["content"][1]["content"][0]["text"].as_str().unwrap();
    assert_eq!(r4, "Unchanged since step 2 (lines 101–200 are above).");
}

#[test]
fn c7_the_gauge_speaks_at_50_75_and_90_once_each() {
    let mut e = Env::new("c7", vec![], vec![]);
    for tokens in [400, 520, 560, 760, 800, 905, 950] {
        e.fake.push([json!({ "tool_calls": [{ "name": "Glob", "input": { "pattern": format!("*.{tokens}") } }], "usage": { "input_tokens": tokens } })]);
    }
    e.fake.push([json!({ "text": "full" })]);
    let t = e.start("c7", "go", json!({ "limits": { "context_tokens": 1000 } }));
    assert_eq!(e.end(&t)["reason"], "done");
    let last = all_text(e.fake.bodies().last().unwrap());
    let said: Vec<&str> = last
        .split("Context: ")
        .skip(1)
        .map(|s| &s[..s.find('.').unwrap()])
        .collect();
    assert_eq!(said.len(), 3, "{said:?}");
    let pct = |s: &str| s[..s.find('%').unwrap()].parse::<u64>().unwrap();
    assert!(
        (50..75).contains(&pct(said[0])) && (75..90).contains(&pct(said[1])) && pct(said[2]) >= 90,
        "{said:?}"
    );
    assert!(said[0].ends_with(" of 1k tokens)"), "{said:?}");
}

#[test]
fn c8_the_third_identical_call_with_the_same_result_is_one_line() {
    let mut e = Env::new("c8", vec![], vec![]);
    write(&e.work.join("s.txt"), "state one\n");
    let cat = json!({ "name": "Bash", "input": { "command": "cat s.txt" } });
    e.fake.push([
        calls(json!([cat])),
        calls(json!([cat])),
        calls(json!([cat])),
        calls(json!([cat])),
        calls(json!([{ "name": "Bash", "input": { "command": "echo state two > s.txt" } }, cat])),
        json!({ "text": "ok" }),
    ]);
    let t = e.start("c8", "watch it", json!({}));
    assert_eq!(e.end(&t)["reason"], "done");
    let last = plain_messages(e.fake.bodies().last().unwrap());
    let results: Vec<String> = last
        .iter()
        .map(|m| serde_json::from_str::<Value>(m).unwrap())
        .filter(|m| m["role"] == "user")
        .flat_map(|m| m["content"].as_array().unwrap().clone())
        .filter(|b| b["type"] == "tool_result")
        .map(|b| b["content"][0]["text"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        results,
        [
            "state one",
            "state one",
            "Unchanged. You already have this result (step 2).",
            "Unchanged. You already have this result (step 2).",
            "(no output)",
            "state two",
        ]
    );
    // the tool still ran: the host saw the real result
    let ends: Vec<String> = e
        .host
        .lines()
        .into_iter()
        .filter(|l| l["method"] == "tool.end" && l["params"]["input"]["command"] == "cat s.txt")
        .map(|l| text(&l["params"]))
        .collect();
    assert_eq!(
        ends,
        [
            "state one",
            "state one",
            "state one",
            "state one",
            "state two"
        ]
    );
}

#[test]
fn c12_auto_compaction_at_85_percent_only_when_asked() {
    for (name, auto) in [
        ("c12-on", json!(true)),
        ("c12-off", json!(false)),
        ("c12-absent", Value::Null),
    ] {
        let mut e = Env::new(name, vec![], vec![]);
        e.fake.push([
            json!({ "match": "Write a summary", "text": "SUMMARY-OF-WORK" }),
            json!({ "tool_calls": [{ "name": "Glob", "input": { "pattern": "*" } }], "usage": { "input_tokens": 900 } }),
            json!({ "text": "went on" }),
        ]);
        let mut p = json!({ "limits": { "context_tokens": 1000 } });
        if !auto.is_null() {
            p["auto_compact"] = auto.clone();
        }
        let t = e.start("s", "work", p);
        let end = e.end(&t);
        assert_eq!(end["reason"], "done", "{end}");
        let b = e.fake.bodies();
        let summaries = b
            .iter()
            .filter(|x| all_text(x).contains("Write a summary"))
            .count();
        if auto == json!(true) {
            assert_eq!(summaries, 1);
            let next = &b[2]["messages"];
            assert_eq!(next.as_array().unwrap().len(), 1, "{next}");
            assert!(
                next[0]["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .ends_with("SUMMARY-OF-WORK")
            );
            let states: Vec<Value> = e
                .host
                .lines()
                .into_iter()
                .filter(|l| l["method"] == "compaction")
                .map(|l| l["params"]["state"].clone())
                .collect();
            assert_eq!(states, ["start", "done"]);
            assert!(e.wire("s").contains("\"kind\":\"compaction\""));
            // the summary call's usage is in the turn's totals
            assert_eq!(end["usage"]["output_tokens"], 15);
        } else {
            assert_eq!(summaries, 0, "{name}");
            assert_eq!(b.len(), 2);
        }
    }
}

#[test]
fn c12_a_failed_auto_compaction_changes_nothing_and_is_not_tried_again() {
    let mut e = Env::new("c12f", vec![], vec![]);
    e.fake.push([
        json!({ "match": "Write a summary", "status": 400, "repeat": true }),
        json!({ "tool_calls": [{ "name": "Glob", "input": { "pattern": "*" } }], "usage": { "input_tokens": 900 } }),
        json!({ "tool_calls": [{ "name": "Glob", "input": { "pattern": "*.x" } }], "usage": { "input_tokens": 950 } }),
        json!({ "text": "went on" }),
    ]);
    let t = e.start(
        "s",
        "work",
        json!({ "limits": { "context_tokens": 1000 }, "auto_compact": true }),
    );
    assert_eq!(e.end(&t)["reason"], "done");
    let b = e.fake.bodies();
    assert_eq!(
        b.iter()
            .filter(|x| all_text(x).contains("Write a summary"))
            .count(),
        1
    );
    assert!(!e.wire("s").contains("\"kind\":\"compaction\""));
    assert!(extends(&b[0], &b[2]) && extends(&b[2], &b[3]));
}

/// The raw bytes of a top-level field of a request body.
fn raw_field<'a>(raw: &'a str, key: &str, next: &str) -> &'a str {
    let start = raw.find(&format!("\"{key}\":")).unwrap() + key.len() + 3;
    let end = raw[start..].find(&format!(",\"{next}\":")).unwrap() + start;
    &raw[start..end]
}

const BRANCH_NOTE: &str = "You are a branch of the conversation above.";

#[test]
fn c9_branches_share_the_prefix_run_at_once_and_return_only_their_answer() {
    let mut e = Env::new("c9", vec![], vec![]);
    write(&e.work.join("x.txt"), "SECRET-X\n");
    let x = e.work.join("x.txt");
    let y = e.work.join("y.txt");
    e.fake.push([
        json!({ "match": "start the work", "text": "Branching.", "tool_calls": [
            { "id": "br1", "name": "Branch", "input": { "task": "TASK-ONE: what is in x.txt?" } },
            { "id": "br2", "name": "Branch", "input": { "task": "TASK-TWO: say two" } },
            { "id": "br3", "name": "Branch", "input": { "task": "TASK-THREE: try to change files" } },
        ] }),
        json!({ "match": "TASK-ONE", "tool_calls": [{ "id": "bx", "name": "Read", "input": { "file_path": "x.txt" } }], "delay_ms": 40 }),
        json!({ "match": "SECRET-X", "text": "ONE-ANSWER: x holds the secret", "delay_ms": 20 }),
        json!({ "match": "TASK-TWO", "text": "TWO-ANSWER", "delay_ms": 40 }),
        json!({ "match": "TASK-THREE", "delay_ms": 20, "tool_calls": [
            { "id": "be", "name": "Edit", "input": { "file_path": x, "old_string": "SECRET", "new_string": "PUBLIC" } },
            { "id": "bw", "name": "Write", "input": { "file_path": y, "content": "new" } },
            { "id": "bb", "name": "Branch", "input": { "task": "nested" } },
        ] }),
        json!({ "match": "Not available in a branch.", "text": "THREE-ANSWER: could not" }),
        json!({ "match": "ONE-ANSWER", "text": "all done" }),
    ]);
    let t = e.start("c9", "start the work", json!({}));
    let end = e.end(&t);
    assert_eq!(end["reason"], "done", "{end}");
    let raw: Vec<String> = e.fake.requests().iter().map(|r| r.body.clone()).collect();
    let bodies = e.fake.bodies();
    assert_eq!(bodies.len(), 7);
    let parent = &raw[0];
    let firsts: Vec<usize> = (0..raw.len())
        .filter(|&i| {
            let m = plain_messages(&bodies[i]);
            m.len() == 2 && m[1].contains(BRANCH_NOTE)
        })
        .collect();
    assert_eq!(firsts.len(), 3, "three branches");
    for &i in &firsts {
        assert_eq!(
            raw_field(&raw[i], "system", "tools"),
            raw_field(parent, "system", "tools")
        );
        assert_eq!(
            raw_field(&raw[i], "tools", "messages"),
            raw_field(parent, "tools", "messages")
        );
        assert_eq!(plain_messages(&bodies[i])[0], plain_messages(&bodies[0])[0]);
    }
    let task: Value = serde_json::from_str(&plain_messages(&bodies[firsts[0]])[1]).unwrap();
    assert_eq!(
        task["content"][1]["text"].as_str().unwrap(),
        "You are a branch of the conversation above. Only your final answer goes back to it, and you cannot change files here (Edit, Write and Branch are not available)."
    );
    // at the same time: every branch had asked the model before any branch ended
    let reqs = e.fake.requests();
    let last_first = firsts.iter().map(|&i| reqs[i].at).max().unwrap();
    let lines = e.host.lines();
    let times = e.host.times();
    let first_end = (0..lines.len())
        .find(|&i| lines[i]["method"] == "tool.end" && lines[i]["params"]["name"] == "Branch")
        .map(|i| times[i])
        .unwrap();
    assert!(last_first < first_end);
    // nothing is written inside a branch, and no check is asked for those calls
    assert_eq!(std::fs::read_to_string(&x).unwrap(), "SECRET-X\n");
    assert!(!y.exists());
    let checks: Vec<&Value> = lines
        .iter()
        .filter(|l| l["method"] == "tool.check")
        .collect();
    let checked: Vec<(&str, Option<&str>)> = checks
        .iter()
        .map(|l| {
            (
                l["params"]["call"]["id"].as_str().unwrap(),
                l["params"]["branch"].as_str(),
            )
        })
        .collect();
    assert!(checked.contains(&("bx", Some("br1"))), "{checked:?}");
    assert!(
        !checked
            .iter()
            .any(|(id, _)| matches!(*id, "be" | "bw" | "bb"))
    );
    let refused = bodies
        .iter()
        .map(|b| plain_messages(b).last().cloned().unwrap())
        .find(|m| m.contains("\"tool_use_id\":\"be\""))
        .unwrap();
    let refused: Value = serde_json::from_str(&refused).unwrap();
    assert_eq!(refused["content"].as_array().unwrap().len(), 3);
    for r in refused["content"].as_array().unwrap() {
        assert_eq!(r["content"][0]["text"], "Not available in a branch.");
        assert_eq!(r["is_error"], true);
    }
    // every notification of a branch names it
    assert!(lines.iter().any(|l| l["method"] == "step.start"
        && l["params"]["branch"] == "br1"
        && l["params"]["step"] == 2));
    assert!(
        lines
            .iter()
            .filter(|l| l["method"] == "message.end")
            .all(|l| l["params"]["step"] == 2
                || l["params"].get("branch").is_some()
                || l["params"]["message"]["content"][0]["text"] == "Branching.")
    );
    // only the final text comes back; the parent's log has one call and one result each
    let last = plain_messages(bodies.last().unwrap());
    let results: Value = serde_json::from_str(&last[2]).unwrap();
    let texts: Vec<&Value> = results["content"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| &r["content"])
        .collect();
    assert_eq!(
        texts,
        [
            &json!([{ "type": "text", "text": "ONE-ANSWER: x holds the secret" }]),
            &json!([{ "type": "text", "text": "TWO-ANSWER" }]),
            &json!([{ "type": "text", "text": "THREE-ANSWER: could not" }]),
        ]
    );
    let wire = e.wire("c9");
    assert_eq!(wire.matches("\"kind\":\"message\"").count(), 4);
    assert!(!wire.contains(BRANCH_NOTE) && !wire.contains("\"bx\""));
    // the branches' usage is in the turn's totals (7 calls of 10 input tokens)
    assert_eq!(end["usage"]["input_tokens"], 70);
    assert_eq!(end["steps"], 2);
}

#[test]
fn c10_a_steer_ends_the_running_branches_at_once() {
    let mut e = Env::new("c10", vec![], vec![]);
    e.fake.push([
        json!({ "match": "go wide", "tool_calls": [
            { "id": "q1", "name": "Branch", "input": { "task": "LONG-ONE" } },
            { "id": "q2", "name": "Branch", "input": { "task": "LONG-TWO" } },
        ] }),
        json!({ "match": "LONG-ONE", "text": "never finished", "hang_after": 3 }),
        json!({ "match": "LONG-TWO", "text": "never finished", "hang_after": 3 }),
        json!({ "match": "change of plan", "text": "ok, new plan" }),
    ]);
    let t = e.start("c10", "go wide", json!({}));
    for b in ["q1", "q2"] {
        e.host
            .wait_any(
                &json!({ "method": "model.event", "params": { "branch": b } }),
                LONG,
            )
            .unwrap();
    }
    let sent = Instant::now();
    e.host.call("turn.steer", json!({ "session": "c10", "id": "s1", "input": [{ "type": "text", "text": "change of plan" }] }));
    for b in ["q1", "q2"] {
        let end = e
            .host
            .wait_any(
                &json!({ "method": "tool.end", "params": { "call_id": b } }),
                LONG,
            )
            .unwrap();
        assert_eq!(
            text(&end["params"]),
            "Not finished: a message from the user arrived first."
        );
        assert_eq!(end["params"]["is_error"], true);
    }
    assert!(
        sent.elapsed() < Duration::from_secs(1),
        "{:?}",
        sent.elapsed()
    );
    let end = e.end(&t);
    assert_eq!(end["reason"], "done", "{end}");
    let b = e.fake.bodies();
    let last = b.last().unwrap()["messages"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    let kinds: Vec<String> = last["content"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            c["tool_use_id"]
                .as_str()
                .or(c["text"].as_str())
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(kinds, ["q1", "q2", "change of plan"]);
}

#[test]
fn c5_a_compaction_keeps_the_ledger_word_for_word() {
    let rules = vec![
        json!({ "method": "tool.call", "match": { "call": { "input": { "n": 1 } } }, "reply": { "content": [{ "type": "text", "text": "status one" }] } }),
        json!({ "method": "tool.call", "match": { "call": { "input": { "n": 2 } } }, "reply": { "content": "status two\nline two" } }),
    ];
    let mut e = Env::new("c5", vec![], rules);
    let (a, b) = (e.work.join("a.txt"), e.work.join("b.txt"));
    write(&b, "old text\n");
    e.fake.push([
        calls(json!([{ "name": "Read", "input": { "file_path": b } }, { "name": "Write", "input": { "file_path": a, "content": "made\n" } }])),
        calls(json!([{ "name": "Edit", "input": { "file_path": b, "old_string": "old", "new_string": "new" } }])),
        calls(json!([{ "name": "Bash", "input": { "command": "sleep 30", "run_in_background": true } }])),
        calls(json!([{ "name": "status", "input": { "n": 1 } }])),
        calls(json!([{ "name": "status", "input": { "n": 2 } }])),
        json!({ "text": "worked" }),
    ]);
    let tools = json!({ "host_tools": [{ "name": "status", "input_schema": { "type": "object" }, "keep_latest": true }] });
    let t = e.start("c5", "work", tools.clone());
    assert_eq!(e.end(&t)["reason"], "done");
    let task = e
        .host
        .wait_any(
            &json!({ "method": "task.update", "params": { "task": { "state": "running" } } }),
            LONG,
        )
        .unwrap();
    let task = task["params"]["task"].clone();
    e.fake
        .push([json!({ "match": "Write a summary", "text": "SUMMARY-C5" })]);
    let r = e.host.call(
        "session.compact",
        json!({ "session": "c5", "model": "m", "endpoint": { "url": e.fake.url() } }),
    );
    assert_eq!(r["result"]["messages_after"], 1, "{r}");
    e.fake.push([
        calls(json!([{ "name": "Glob", "input": { "pattern": "*" } }])),
        json!({ "text": "after" }),
    ]);
    let t = e.start("c5", "go on", tools);
    assert_eq!(e.end(&t)["reason"], "done");
    let bodies = e.fake.bodies();
    let n = bodies.len();
    let first = &bodies[n - 2]["messages"][0];
    assert_eq!(
        first["content"][0]["text"],
        "This chat was compacted. Summary of the conversation before this point:\n\nSUMMARY-C5"
    );
    let ledger = first["content"][1]["text"].as_str().unwrap();
    assert_eq!(
        ledger,
        format!(
            "Kept word for word at this compaction (not summarized):\n\nFiles created or changed in this chat:\n- {}\n- {}\n\nBackground commands still running:\n- {}: sleep 30 (output: {})\n\nThe newest call of status:\nInput: {{\"n\":2}}\nResult:\nstatus two\nline two",
            a.display(),
            b.display(),
            task["id"].as_str().unwrap(),
            task["output_path"].as_str().unwrap()
        )
    );
    assert_eq!(
        plain_messages(&bodies[n - 1])[0],
        plain_messages(&bodies[n - 2])[0],
        "later requests keep it unchanged"
    );
    e.host.call("shutdown", json!({}));
}

#[test]
fn c6_the_state_delta_names_only_what_changed() {
    let mut e = Env::new("c6", vec![], vec![]);
    let f = e.work.join("f.txt");
    write(&f, "one\ntwo\nthree\n");
    e.fake.push([
        calls(json!([{ "name": "Read", "input": { "file_path": "f.txt" } }, { "name": "Bash", "input": { "command": "sleep 1; exit 4", "run_in_background": true } }])),
        json!({ "text": "started" }),
    ]);
    let t = e.start("c6", "start", json!({}));
    assert_eq!(e.end(&t)["reason"], "done");
    let upd = e
        .host
        .wait_any(
            &json!({ "method": "task.update", "params": { "task": { "state": "exited" } } }),
            LONG,
        )
        .unwrap();
    let task = upd["params"]["task"].clone();
    write(&f, "one\nTWO\nthree\nfour\nfive\n");
    let first_text = |e: &Env| {
        let b = e.fake.bodies();
        let m = b.last().unwrap()["messages"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()
            .clone();
        m["content"][0]["text"].as_str().unwrap().to_string()
    };
    e.fake.push([json!({ "text": "seen" })]);
    let t = e.start("c6", "what now?", json!({}));
    assert_eq!(e.end(&t)["reason"], "done");
    assert_eq!(
        first_text(&e),
        format!(
            "What changed since the last model call:\n- {} was changed outside this chat: 3 lines added, 1 removed.\n- Background task {} (sleep 1; exit 4) ended: exit 4. Output: {}",
            f.display(),
            task["id"].as_str().unwrap(),
            task["output_path"].as_str().unwrap()
        )
    );
    // nothing changed: no block at all
    e.fake.push([json!({ "text": "same" })]);
    let t = e.start("c6", "and now?", json!({}));
    assert_eq!(e.end(&t)["reason"], "done");
    assert_eq!(first_text(&e), "and now?");
    // 31 minutes since the last model call
    e.host.kill();
    let wire = e.data.join("sessions/c6/wire.jsonl");
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        - 31 * 60_000
        - 500;
    let mut log = std::fs::read_to_string(&wire).unwrap();
    log.push_str(&format!("{{\"kind\":\"model_call\",\"at\":{at}}}\n"));
    std::fs::write(&wire, log).unwrap();
    e.restart();
    e.fake.push([json!({ "text": "back" })]);
    let t = e.start("c6", "back again", json!({}));
    assert_eq!(e.end(&t)["reason"], "done");
    assert_eq!(
        first_text(&e),
        "What changed since the last model call:\n- 31 minutes have passed since the last model call."
    );
}

#[test]
fn c11_sixty_reads_of_one_file_return_its_text_once() {
    let mut e = Env::new("c11", vec![], vec![]);
    let body: String = (1..=2000).map(|i| format!("r{i:04}\n")).collect();
    write(&e.work.join("same.txt"), &body);
    for _ in 0..60 {
        e.fake.push([json!({ "tool_calls": [{ "name": "Read", "input": { "file_path": "same.txt" } }], "usage": { "input_tokens": 10_000 } })]);
    }
    e.fake.push([json!({ "text": "read" })]);
    let t = e.start(
        "c11",
        "read it",
        json!({ "limits": { "context_tokens": 16_000, "max_steps": 70 } }),
    );
    assert_eq!(e.end(&t)["reason"], "done");
    let last = plain_messages(e.fake.bodies().last().unwrap());
    let results: Vec<String> = last
        .iter()
        .map(|m| serde_json::from_str::<Value>(m).unwrap())
        .filter(|m| m["role"] == "user")
        .flat_map(|m| m["content"].as_array().unwrap().clone())
        .filter(|b| b["type"] == "tool_result")
        .map(|b| b["content"][0]["text"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(results.len(), 60);
    assert_eq!(
        results[0].lines().count(),
        2000,
        "the full text once, never faded"
    );
    assert!(results[0].ends_with("  2000\tr2000"));
    for r in &results[1..] {
        assert!(!r.contains('\n') && r.starts_with("Unchanged"), "{r}");
    }
    assert!(fades(&e.wire("c11")).is_empty());
    let chars: usize = last.iter().map(|m| m.chars().count()).sum();
    assert!(chars / 4 < 16_000, "{} tokens", chars / 4);
}

/// The results of a request's tool calls, by call id.
fn results_by_id(body: &Value) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    for m in body["messages"].as_array().unwrap() {
        for b in m["content"].as_array().into_iter().flatten() {
            if b["type"] == "tool_result" {
                out.insert(b["tool_use_id"].as_str().unwrap().to_string(), text(b));
            }
        }
    }
    out
}

#[test]
fn f1_a_ledger_that_cannot_be_written_fails_the_compaction() {
    let long = format!("BOARD-{}", "row of the board\n".repeat(70));
    let rules = vec![json!({ "method": "tool.call", "reply": { "content": long } })];
    let mut e = Env::new("f1", vec![], rules);
    let tools = json!({ "limits": { "result_cap_chars": 300 }, "host_tools": [{ "name": "board", "input_schema": { "type": "object" }, "keep_latest": true }] });
    // session.compact: an error, and the log is left as it was
    e.fake.push([
        calls(json!([{ "name": "board", "input": { "page": 7 } }])),
        json!({ "text": "seen" }),
        json!({ "match": "Write a summary", "text": "SUMMARY-F1", "repeat": true }),
    ]);
    let t = e.start("f1", "look", tools.clone());
    assert_eq!(e.end(&t)["reason"], "done");
    let scratch = e.data.join("sessions/f1/scratch");
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::write(&scratch, "a file where the folder should be").unwrap();
    let before = e.wire("f1");
    let r = e.host.call(
        "session.compact",
        json!({ "session": "f1", "model": "m", "endpoint": { "url": e.fake.url() } }),
    );
    assert_eq!(r["error"]["code"], "compaction_failed", "{r}");
    assert!(
        r["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("the ledger cannot keep the result of board: "),
        "{r}"
    );
    assert_eq!(e.wire("f1"), before, "the log is unchanged");
    // auto-compaction: counts as failed, the turn goes on, the log keeps no marker
    let dir = e.data.join("sessions/f1b");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("scratch"), "not a folder").unwrap();
    e.fake.push([
        calls(json!([{ "name": "board", "input": { "page": 8 } }])),
        json!({ "tool_calls": [{ "name": "Glob", "input": { "pattern": "*.f1" } }], "usage": { "input_tokens": 950 } }),
        json!({ "text": "went on" }),
    ]);
    let mut p = tools;
    p["limits"]["context_tokens"] = json!(1000);
    p["auto_compact"] = json!(true);
    let t = e.start("f1b", "work", p);
    let end = e.end(&t);
    assert_eq!(end["reason"], "done", "{end}");
    let states: Vec<Value> = e
        .host
        .lines()
        .into_iter()
        .filter(|l| l["method"] == "compaction" && l["params"]["turn"] == t)
        .map(|l| l["params"]["state"].clone())
        .collect();
    assert_eq!(states, ["start", "failed"]);
    assert!(!e.wire("f1b").contains("\"kind\":\"compaction\""));
    let b = e.fake.bodies();
    let n = b.len();
    assert!(
        extends(&b[n - 3], &b[n - 1]),
        "the turn went on from the same context"
    );
}

#[test]
fn f2_the_ledger_keeps_the_real_result_the_loop_breaker_replaced() {
    let rules = vec![
        json!({ "method": "tool.call", "match": { "call": { "input": { "q": "a" } } }, "reply": { "content": [{ "type": "text", "text": "A-REAL\nsecond line" }] } }),
        json!({ "method": "tool.call", "match": { "call": { "input": { "q": "b" } } }, "reply": { "content": "B-REAL" } }),
    ];
    let mut e = Env::new("f2", vec![], rules);
    let a = json!({ "name": "status", "input": { "q": "a" } });
    e.fake.push([
        calls(json!([a])),
        calls(json!([a])),
        calls(json!([a, { "name": "status", "input": { "q": "b" } }])),
        calls(json!([a])),
        json!({ "text": "watched" }),
    ]);
    let tools = json!({ "host_tools": [{ "name": "status", "input_schema": { "type": "object" }, "keep_latest": true }] });
    let t = e.start("f2", "watch", tools.clone());
    assert_eq!(e.end(&t)["reason"], "done");
    let last = e.fake.bodies().last().unwrap().clone();
    let shown: Vec<String> = results_by_id(&last).into_values().collect();
    assert_eq!(
        shown
            .iter()
            .filter(|r| r.starts_with("Unchanged. You already have this result"))
            .count(),
        2,
        "{shown:?}"
    );
    // after a restart the real result comes from the log alone
    e.restart();
    e.fake.push([
        json!({ "match": "next", "text": "back" }),
        json!({ "match": "Write a summary", "text": "SUMMARY-F2" }),
    ]);
    let t = e.start("f2", "next", tools);
    assert_eq!(e.end(&t)["reason"], "done");
    let r = e.host.call(
        "session.compact",
        json!({ "session": "f2", "model": "m", "endpoint": { "url": e.fake.url() } }),
    );
    assert_eq!(r["result"]["messages_after"], 1, "{r}");
    let marker: Value = e
        .wire("f2")
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .find(|l| l["kind"] == "compaction")
        .unwrap();
    assert_eq!(
        marker["ledger"]["latest"],
        json!([{ "name": "status", "input": { "q": "a" }, "result": "A-REAL\nsecond line" }])
    );
    assert!(
        marker["message"]["content"][1]["text"]
            .as_str()
            .unwrap()
            .ends_with("Input: {\"q\":\"a\"}\nResult:\nA-REAL\nsecond line")
    );
}

#[test]
fn f3_branches_start_from_the_version_table_before_their_step() {
    // the parent's Read runs first; both branches' checks are held 300 ms
    let rules = vec![
        json!({ "method": "tool.check", "match": { "call": { "name": "Branch" } }, "reply": { "decision": "allow" }, "delay_ms": 300 }),
    ];
    let mut e = Env::new("f3", vec![], rules);
    write(
        &e.work.join("f.txt"),
        "line one\nLINE-TWO here\nline three\nline four\n",
    );
    let part = json!({ "file_path": "f.txt", "offset": 2, "limit": 2 });
    e.fake.push([
        json!({ "match": "go fork", "tool_calls": [
            { "id": "pr", "name": "Read", "input": { "file_path": "f.txt" } },
            { "id": "k1", "name": "Branch", "input": { "task": "FORK-ONE" } },
            { "id": "k2", "name": "Branch", "input": { "task": "FORK-TWO" } },
        ] }),
        // each branch looks around first, then reads part of the file
        json!({ "match": "FORK-", "tool_calls": [{ "name": "Glob", "input": { "pattern": "*.zz" } }], "repeat": true }),
        json!({ "match": "No files found", "tool_calls": [{ "name": "Read", "input": part }], "repeat": true }),
        json!({ "match": "LINE-TWO", "text": "fork answer", "repeat": true }),
    ]);
    let t = e.start("f3", "go fork", json!({}));
    let end = e.end(&t);
    assert_eq!(end["reason"], "done", "{end}");
    let lines = e.host.lines();
    let at = |m: &str, id: &str| {
        lines
            .iter()
            .position(|l| {
                l["method"] == m
                    && (l["params"]["call"]["id"] == id || l["params"]["call_id"] == id)
            })
            .unwrap()
    };
    assert!(
        at("tool.end", "pr") < at("tool.start", "k1"),
        "the parent's Read ran first"
    );
    let reads: Vec<(Value, String)> = lines
        .iter()
        .filter(|l| {
            l["method"] == "tool.end"
                && l["params"]["name"] == "Read"
                && l["params"].get("branch").is_some()
        })
        .map(|l| (l["params"]["branch"].clone(), text(&l["params"])))
        .collect();
    assert_eq!(reads.len(), 2, "{reads:?}");
    for (_, r) in &reads {
        assert_eq!(
            r,
            "     2\tLINE-TWO here\n     3\tline three\n\n(Lines 2–3 of 4. Use offset and limit to read other lines.)"
        );
    }
}

#[test]
fn f4_one_line_reads_never_take_the_newest_view_through_a_fade_and_a_cold_turn() {
    let mut e = env_ttl("f4", 1500, vec![]);
    write(&e.work.join("same.txt"), &lines(150, 30, "S"));
    for i in 1..=14 {
        write(
            &e.work.join(format!("b{i}.txt")),
            &lines(100, 60, &format!("b{i}-")),
        );
        let tokens = if i < 14 { 1000 * i } else { 25_000 };
        e.fake.push([json!({ "tool_calls": [
            { "id": format!("r{i}"), "name": "Read", "input": { "file_path": "same.txt" } },
            { "id": format!("c{i}"), "name": "Bash", "input": { "command": format!("cat b{i}.txt") } },
        ], "usage": { "input_tokens": tokens } })]);
    }
    e.fake.push([json!({ "text": "done" })]);
    let p = json!({ "limits": { "context_tokens": 40000, "max_steps": 30 } });
    let t = e.start("f4", "read", p.clone());
    assert_eq!(e.end(&t)["reason"], "done");
    let marks = fades(&e.wire("f4"));
    assert_eq!(marks.len(), 1, "a 60% fade happened");
    let faded: Vec<&String> = marks[0]["stubs"].as_object().unwrap().keys().collect();
    assert_eq!(faded, ["c1", "c2", "c3", "c4"]);
    let res = results_by_id(e.fake.bodies().last().unwrap());
    assert_eq!(res["r1"].lines().count(), 150, "the only full view stays");
    assert!(
        res["r4"].starts_with("Unchanged. You already have this result"),
        "{}",
        res["r4"]
    );
    // a cold turn fades every candidate, still never that view
    std::thread::sleep(Duration::from_millis(1600));
    e.fake.push([json!({ "text": "cold" })]);
    let t = e.start("f4", "again", p);
    assert_eq!(e.end(&t)["reason"], "done");
    let marks = fades(&e.wire("f4"));
    assert_eq!(marks.len(), 2);
    assert!(marks.iter().all(|m| {
        m["stubs"]
            .as_object()
            .unwrap()
            .keys()
            .all(|k| k.starts_with('c'))
    }));
    let res = results_by_id(e.fake.bodies().last().unwrap());
    assert_eq!(res["r1"].lines().count(), 150);
}

#[test]
fn f5_at_60_percent_a_fade_waits_until_the_candidates_free_10_percent() {
    let mut e = Env::new("f5", vec![], vec![]);
    for i in 1..=16 {
        write(
            &e.work.join(format!("m{i}.txt")),
            &lines(60, 60, &format!("m{i}-")),
        );
        let tokens = if i == 1 { 2000 } else { 25_000 };
        e.fake.push([json!({ "tool_calls": [{ "id": format!("m{i}"), "name": "Bash", "input": { "command": format!("cat m{i}.txt") } }], "usage": { "input_tokens": tokens } })]);
    }
    e.fake
        .push([json!({ "text": "done", "usage": { "input_tokens": 25_000 } })]);
    let t = e.start(
        "f5",
        "go",
        json!({ "limits": { "context_tokens": 40000, "max_steps": 30 } }),
    );
    assert_eq!(e.end(&t)["reason"], "done");
    // 62.5% from step 2 on. Each result frees about 870 tokens: 4 candidates
    // (boundary 14) are under 10% of the window, 5 (boundary 15) are over.
    let b = e.fake.bodies();
    let rewrites: Vec<usize> = (1..b.len())
        .filter(|&i| !extends(&b[i - 1], &b[i]))
        .collect();
    assert_eq!(
        rewrites,
        [15],
        "one fade, after step 15; none at 2-14 or at 16"
    );
    let marks = fades(&e.wire("f5"));
    assert_eq!(marks.len(), 1);
    let ids: Vec<&String> = marks[0]["stubs"].as_object().unwrap().keys().collect();
    assert_eq!(ids, ["m1", "m2", "m3", "m4", "m5"]);
}

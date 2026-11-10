//! Stop (P3), steer (P4), and background commands.

mod common;
use common::*;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

fn bash(id: &str, command: &str) -> Value {
    json!({ "tool_calls": [{ "id": id, "name": "Bash", "input": { "command": command } }] })
}

#[test]
fn stop_kills_the_foreground_command_and_the_chat_goes_on() {
    let sleeper = format!("sleep 61.{}", std::process::id());
    let mut e = Env::new(
        "stop",
        vec![bash("s1", &format!("echo started; {sleeper}; echo never"))],
        vec![],
    );
    let turn = e.start("st", "sleep please", json!({}));
    e.host
        .wait(
            &json!({ "method": "tool.start", "params": { "call": { "id": "s1" } } }),
            LONG,
        )
        .unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let sleeps: Vec<u32> = pids_with(&sleeper)
        .into_iter()
        .filter(|p| {
            std::fs::read(format!("/proc/{p}/cmdline"))
                .unwrap_or_default()
                .starts_with(b"sleep")
        })
        .collect();
    assert_eq!(sleeps.len(), 1, "{sleeps:?}");
    let pgid = pgid_of(sleeps[0]).unwrap();
    let leader = String::from_utf8_lossy(&std::fs::read(format!("/proc/{pgid}/cmdline")).unwrap())
        .replace('\0', " ");
    assert!(
        leader.starts_with("/bin/bash -c "),
        "the group leader is the command's bash: {leader}"
    );
    assert_ne!(pgid_of(std::process::id()), Some(pgid));
    let asked = Instant::now();
    e.host.call("turn.stop", json!({ "session": "st" }));
    let end = e.end(&turn);
    let took = asked.elapsed();
    assert_eq!(end["reason"], "stopped");
    assert!(took < Duration::from_secs(1), "turn.end after {took:?}");
    while !group_members(pgid).is_empty() {
        assert!(
            asked.elapsed() < Duration::from_secs(1),
            "process group {pgid} still has {:?}",
            group_members(pgid)
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    eprintln!(
        "P3: turn.end {took:?} after turn.stop; process group gone {:?} after it",
        asked.elapsed()
    );
    // the log stays valid: the stopped call has its result
    let wire = e.wire("st");
    let last = wire.lines().last().unwrap();
    assert!(
        last.contains("\"tool_use_id\":\"s1\"")
            && last.contains("Stopped by the user.")
            && last.contains("started"),
        "{last}"
    );
    e.fake.push([json!({ "text": "back" })]);
    let t2 = e.start("st", "again", json!({}));
    assert_eq!(e.end(&t2)["reason"], "done");
    assert_eq!(
        e.host.call("task.list", json!({ "session": "st" }))["result"]["tasks"],
        json!([])
    );
    assert_eq!(
        e.host.call("session.info", json!({ "session": "st" }))["result"]["tasks"],
        0
    );
}

#[test]
fn stop_mid_stream_keeps_complete_blocks() {
    let hang = json!({ "text": "complete block", "tool_calls": [{ "id": "w", "name": "Glob", "input": { "pattern": "*" } }], "hang_after": 7 });
    let mut e = Env::new("midstream", vec![hang], vec![]);
    let turn = e.start("ms", "go", json!({}));
    e.host.wait(&json!({ "method": "model.event", "params": { "event": { "type": "content_block_stop", "index": 0 } } }), LONG).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    let asked = Instant::now();
    e.host.call("turn.stop", json!({ "session": "ms" }));
    let end = e.end(&turn);
    assert!(asked.elapsed() < Duration::from_secs(1));
    assert_eq!(end["reason"], "stopped");
    let wire = e.wire("ms");
    assert!(
        wire.contains("complete block") && !wire.contains("\"tool_use\""),
        "{wire}"
    );
}

#[test]
fn stop_during_the_tool_check_and_steers_left_over() {
    let rules =
        vec![json!({ "method": "tool.check", "reply": { "decision": "allow" }, "delay_ms": 3000 })];
    let mut e = Env::new(
        "stopcheck",
        vec![
            json!({ "tool_calls": [{ "id": "g1", "name": "Glob", "input": { "pattern": "*" } }, { "id": "b1", "name": "Bash", "input": { "command": "true" } }] }),
        ],
        rules,
    );
    let turn = e.start("sc", "go", json!({}));
    e.host
        .wait(&json!({ "method": "tool.check" }), LONG)
        .unwrap();
    e.host.call(
        "turn.steer",
        json!({ "session": "sc", "id": "late-1", "input": [{ "type": "text", "text": "hey" }] }),
    );
    e.host.call("turn.stop", json!({ "session": "sc" }));
    let end = e.end(&turn);
    assert_eq!(end["reason"], "stopped");
    assert_eq!(end["undelivered"], json!(["late-1"]));
    let last = e.wire("sc").lines().last().unwrap().to_string();
    assert_eq!(last.matches("Stopped by the user.").count(), 2, "{last}");
    let r = e.host.call(
        "turn.steer",
        json!({ "session": "sc", "id": "after", "input": [{ "type": "text", "text": "x" }] }),
    );
    assert_eq!(r["error"]["code"], "not_running");
}

#[test]
fn steer_during_a_batch_lands_after_its_results() {
    let rules = vec![
        json!({ "method": "tool.check", "match": { "call": { "name": "Read" } }, "reply": { "decision": "allow" }, "delay_ms": 400 }),
    ];
    let mut e = Env::new("steerbatch", vec![], rules);
    write(&e.work.join("f.txt"), "data\n");
    e.fake.push([
        json!({ "tool_calls": [{ "id": "r1", "name": "Read", "input": { "file_path": "f.txt" } }, { "id": "g1", "name": "Glob", "input": { "pattern": "*.txt" } }] }),
        json!({ "text": "seen" }),
    ]);
    let turn = e.start("sb", "read it", json!({}));
    e.host
        .wait(
            &json!({ "method": "tool.check", "params": { "call": { "id": "r1" } } }),
            LONG,
        )
        .unwrap();
    e.host.call("turn.steer", json!({ "session": "sb", "id": 7, "input": [{ "type": "text", "text": "also check the README" }] }));
    e.host.call(
        "turn.steer",
        json!({ "session": "sb", "id": "s2", "input": [{ "type": "text", "text": "second" }] }),
    );
    let end = e.end(&turn);
    assert_eq!(end["reason"], "done");
    assert_eq!(end["undelivered"], json!([]));
    let body = &e.fake.bodies()[1];
    let content = body["messages"].as_array().unwrap().last().unwrap()["content"]
        .as_array()
        .unwrap()
        .clone();
    let kinds: Vec<String> = content
        .iter()
        .map(|b| {
            b["tool_use_id"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| b["text"].as_str().unwrap().to_string())
        })
        .collect();
    assert_eq!(kinds, ["r1", "g1", "also check the README", "second"]);
    let delivered: Vec<Value> = e
        .host
        .lines()
        .into_iter()
        .filter(|l| l["method"] == "steer.delivered")
        .map(|l| l["params"]["id"].clone())
        .collect();
    assert_eq!(delivered, [json!(7), json!("s2")]);
}

#[test]
fn steer_moves_a_waiting_command_to_the_background() {
    let mut e = Env::new(
        "steerbash",
        vec![
            bash("b1", "echo begin; sleep 30; echo end"),
            json!({ "text": "ok" }),
        ],
        vec![],
    );
    let turn = e.start("sw", "run it", json!({}));
    e.host
        .wait(
            &json!({ "method": "tool.start", "params": { "call": { "id": "b1" } } }),
            LONG,
        )
        .unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let asked = Instant::now();
    e.host.call("turn.steer", json!({ "session": "sw", "id": "u1", "input": [{ "type": "text", "text": "stop waiting" }] }));
    let end_call = e
        .host
        .wait(
            &json!({ "method": "tool.end", "params": { "call_id": "b1" } }),
            LONG,
        )
        .unwrap();
    assert!(
        asked.elapsed() < Duration::from_millis(500),
        "moved after {:?}",
        asked.elapsed()
    );
    let out = text(&end_call["params"]);
    let task = end_call["params"]["meta"]["task_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        out.starts_with("begin\nStill running in the background as task ")
            && out.contains(&task)
            && out.ends_with("You will be told when it ends."),
        "{out}"
    );
    let update = e.host.wait_any(&json!({ "method": "task.update", "params": { "task": { "id": task, "state": "running" } } }), LONG).unwrap();
    assert!(
        update["params"]["task"]["output_path"]
            .as_str()
            .unwrap()
            .ends_with(&format!("scratch/tasks/{task}.log"))
    );
    assert_eq!(e.end(&turn)["reason"], "done");
    let body = &e.fake.bodies()[1];
    let last = body["messages"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .to_string();
    assert!(last.contains("stop waiting"));
    let list = e.host.call("task.list", json!({ "session": "sw" }));
    assert_eq!(list["result"]["tasks"][0]["state"], "running");
    // the host can stop it; background commands outlive the turn
    let r = e
        .host
        .call("task.stop", json!({ "session": "sw", "task": task }));
    assert_eq!(r["result"]["stopped"], true);
    e.host.wait_any(&json!({ "method": "task.update", "params": { "task": { "id": task, "state": "stopped" } } }), LONG).unwrap();
}

#[test]
fn steer_after_the_last_message_continues_the_turn() {
    let mut e = Env::new(
        "steerend",
        vec![
            json!({ "text": "first answer", "delay_ms": 60 }),
            json!({ "text": "second answer" }),
        ],
        vec![],
    );
    let turn = e.start("se", "q", json!({}));
    e.host
        .wait(
            &json!({ "method": "model.event", "params": { "event": { "type": "message_start" } } }),
            LONG,
        )
        .unwrap();
    e.host.call("turn.steer", json!({ "session": "se", "id": "more", "input": [{ "type": "text", "text": "one more thing" }] }));
    let end = e.end(&turn);
    assert_eq!(end["reason"], "done");
    assert_eq!(end["steps"], 2);
    let msgs = e.fake.bodies()[1]["messages"].as_array().unwrap().clone();
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[2]["content"][0]["text"], "one more thing");
}

#[test]
fn background_task_end_is_noted_at_the_next_step() {
    let script = vec![
        json!({ "tool_calls": [{ "id": "bg", "name": "Bash", "input": { "command": "sleep 0.3; echo finished-work; exit 3", "run_in_background": true } }] }),
        bash("fg", "sleep 1"),
        json!({ "text": "noted" }),
    ];
    let mut e = Env::new("bgnote", script, vec![]);
    let turn = e.start("bn", "go", json!({}));
    let first = e
        .host
        .wait(
            &json!({ "method": "tool.end", "params": { "call_id": "bg" } }),
            LONG,
        )
        .unwrap();
    let task = first["params"]["meta"]["task_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        text(&first["params"]).starts_with("Still running in the background as task "),
        "{first}"
    );
    assert_eq!(e.end(&turn)["reason"], "done");
    let upd = e.host.wait_any(&json!({ "method": "task.update", "params": { "task": { "id": task, "state": "exited", "exit_code": 3 } } }), LONG).unwrap();
    let log = upd["params"]["task"]["output_path"]
        .as_str()
        .unwrap()
        .to_string();
    let body = &e.fake.bodies()[2];
    let last = body["messages"].as_array().unwrap().last().unwrap();
    let note = last["content"][1]["text"].as_str().unwrap();
    assert_eq!(
        note,
        format!(
            "Background task {task} (sleep 0.3; echo finished-work; exit 3) ended: exit 3. Output: {log}"
        )
    );
    assert_eq!(std::fs::read_to_string(&log).unwrap(), "finished-work\n");
}

#[test]
fn task_stop_ends_the_group_and_returns_last_lines() {
    let cmd = "trap '' TERM; for i in $(seq 1 30); do echo tick $i; done; sleep 30";
    let mut e = Env::new(
        "taskstop",
        vec![
            json!({ "tool_calls": [{ "id": "bg", "name": "Bash", "input": { "command": cmd, "run_in_background": true } }] }),
            json!({ "text": "started" }),
        ],
        vec![],
    );
    let t = e.start("ts", "go", json!({}));
    let first = e
        .host
        .wait(
            &json!({ "method": "tool.end", "params": { "call_id": "bg" } }),
            LONG,
        )
        .unwrap();
    e.end(&t);
    let task = first["params"]["meta"]["task_id"]
        .as_str()
        .unwrap()
        .to_string();
    std::thread::sleep(Duration::from_millis(200));
    let started = Instant::now();
    let r = e.tool("ts", "TaskStop", json!({ "task_id": task }));
    let out = text(&r);
    assert!(
        started.elapsed() >= Duration::from_secs(2),
        "TERM is ignored, so KILL comes after 2 s"
    );
    assert!(
        out.starts_with(&format!(
            "Stopped task {task} (exit 137). Last lines of its output:\ntick 11\n"
        )),
        "{out}"
    );
    assert!(out.ends_with("tick 30"), "{out}");
    let again = e.tool("ts", "TaskStop", json!({ "task_id": task }));
    assert!(
        text(&again).starts_with(&format!("Task {task} had already been stopped (exit 137).")),
        "{again}"
    );
    let none = e.tool("ts", "TaskStop", json!({ "task_id": "nope" }));
    assert_eq!(
        (text(&none).as_str(), &none["is_error"]),
        ("No background task nope.", &json!(true))
    );
    // stopped by the model: no note in the next request
    assert!(
        !e.fake
            .requests()
            .last()
            .unwrap()
            .body
            .contains("ended: exit")
    );
}

#[test]
fn shutdown_kills_background_commands() {
    let sleeper = format!("sleep 62.{}", std::process::id());
    let mut e = Env::new(
        "shutdown",
        vec![
            json!({ "tool_calls": [{ "id": "bg", "name": "Bash", "input": { "command": sleeper, "run_in_background": true } }] }),
            json!({ "text": "ok" }),
        ],
        vec![],
    );
    let t = e.start("sd", "go", json!({}));
    e.end(&t);
    assert_eq!(pids_with(&sleeper).len(), 1);
    e.host.call("shutdown", json!({}));
    assert_eq!(e.host.close(), Some(0));
    assert!(pids_with(&sleeper).is_empty());
}

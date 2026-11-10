//! `shadow run`: the checks R1, R3–R6, R9, R10 of the shadow-cli contract,
//! plus where the API key may and may not appear.

mod common;
use common::*;
use serde_json::{Value, json};
use shadow_testkit::fake::FakeModel;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const KEY: &str = "sk-test-0123456789-SECRET";

struct Box_ {
    work: PathBuf,
    home: PathBuf,
}

fn place(name: &str) -> Box_ {
    Box_ {
        work: tmpdir(&format!("{name}-work")),
        home: tmpdir(&format!("{name}-home")),
    }
}

/// `shadow run` in `work`, with HOME = `home` and none of the caller's keys.
fn cmd(b: &Box_, url: &str, args: &[&str]) -> Command {
    let mut c = Command::new(shadow_bin());
    c.arg("run")
        .args(["--model", "fake-model", "--base-url", url])
        .args(args)
        .current_dir(&b.work);
    for v in [
        "ANTHROPIC_API_KEY",
        "OPENROUTER_API_KEY",
        "SHADOW_MODEL",
        "SHADOW_TEST_CA",
    ] {
        c.env_remove(v);
    }
    c.env("HOME", &b.home).env("ANTHROPIC_API_KEY", KEY);
    c
}

fn out(o: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&o.stdout).into_owned(),
        String::from_utf8_lossy(&o.stderr).into_owned(),
    )
}

/// Every file under `dir`, read as text.
fn all_text(dir: &Path) -> String {
    let mut text = String::new();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            text.push_str(&all_text(&p));
        } else {
            text.push_str(&String::from_utf8_lossy(
                &std::fs::read(&p).unwrap_or_default(),
            ));
        }
    }
    text
}

/// A running `shadow run` whose stderr lines are collected as they come.
struct Live {
    child: Child,
    err: Arc<Mutex<Vec<String>>>,
}

fn live(mut c: Command) -> Live {
    let mut child = c
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let err = Arc::new(Mutex::new(Vec::new()));
    let (lines, pipe) = (err.clone(), child.stderr.take().unwrap());
    std::thread::spawn(move || {
        for l in BufReader::new(pipe).lines().map_while(Result::ok) {
            lines.lock().unwrap().push(l);
        }
    });
    Live { child, err }
}

impl Live {
    fn wait_err(&self, needle: &str) {
        let until = Instant::now() + LONG;
        while !self.err.lock().unwrap().iter().any(|l| l.contains(needle)) {
            assert!(
                Instant::now() < until,
                "no stderr line with {needle:?}: {:?}",
                self.err.lock().unwrap()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn signal(&self, sig: i32) -> Instant {
        // SAFETY: kill(2) with a pid this test started.
        assert_eq!(unsafe { libc::kill(self.child.id() as i32, sig) }, 0);
        Instant::now()
    }

    fn exit(&mut self) -> i32 {
        let until = Instant::now() + LONG;
        loop {
            if let Some(s) = self.child.try_wait().unwrap() {
                return s.code().unwrap();
            }
            assert!(Instant::now() < until, "shadow run did not exit");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

#[test]
fn r1_text_over_http_and_the_request_sent() {
    let fake = FakeModel::start(
        vec![json!({ "text": "Hi there, from the fake model." })],
        0,
        None,
    )
    .unwrap();
    let b = place("r1");
    let o = cmd(&b, &fake.url(), &["say hi"]).output().unwrap();
    let (stdout, stderr) = out(&o);
    assert_eq!(
        (o.status.code(), stdout.as_str(), stderr.as_str()),
        (Some(0), "Hi there, from the fake model.\n", "")
    );
    let req = &fake.requests()[0];
    let header = |n: &str| {
        req.headers
            .iter()
            .find(|(k, _)| k == n)
            .map(|(_, v)| v.as_str())
    };
    assert_eq!(
        (
            header("x-api-key"),
            header("anthropic-version"),
            header("authorization")
        ),
        (Some(KEY), Some("2023-06-01"), None)
    );
    let body: Value = serde_json::from_str(&req.body).unwrap();
    assert_eq!(
        (&body["model"], &body["max_tokens"], &body["stream"]),
        (&json!("fake-model"), &json!(16000), &json!(true))
    );
    let cwd = std::fs::canonicalize(&b.work).unwrap();
    let system = body["system"].as_array().unwrap();
    assert_eq!(system.len(), 1, "no AGENTS.md: only the default prompt");
    assert!(system[0]["text"].as_str().unwrap().starts_with(&format!(
        "You are Shadow, a coding agent working in the user's terminal, in the folder {}.\n",
        cwd.display()
    )));
    assert_eq!(body["messages"][0]["content"][0]["text"], "say hi");
    // the chat is in ~/.shadow; a run without --session is a new chat
    let sessions: Vec<_> = std::fs::read_dir(b.home.join(".shadow/sessions"))
        .unwrap()
        .flatten()
        .collect();
    assert_eq!(sessions.len(), 1);
    assert!(
        sessions[0]
            .file_name()
            .to_string_lossy()
            .starts_with("run-")
    );
    assert!(!all_text(&b.home).contains(KEY));
}

#[test]
fn r1_https_trusts_the_test_ca_only_when_named() {
    let fake = FakeModel::start_tls(
        vec![json!({ "text": "Hello over TLS.", "repeat": true })],
        0,
        None,
    )
    .unwrap();
    let b = place("r1tls");
    let ca = b.home.join("ca.pem");
    std::fs::write(&ca, fake.ca_pem.as_ref().unwrap()).unwrap();
    assert!(fake.url().starts_with("https://127.0.0.1:"));
    let o = cmd(&b, &fake.url(), &["say hi"])
        .env("SHADOW_TEST_CA", &ca)
        .output()
        .unwrap();
    assert_eq!(
        (o.status.code(), out(&o).0.as_str()),
        (Some(0), "Hello over TLS.\n"),
        "{}",
        out(&o).1
    );
    assert_eq!(fake.requests().len(), 1);
    // without the variable the certificate is not trusted: no request gets through
    let mut run = live(cmd(&b, &fake.url(), &["say hi"]));
    run.wait_err("retrying");
    assert_eq!(
        fake.requests().len(),
        1,
        "the handshake failed before any request"
    );
    run.signal(libc::SIGINT);
    assert_eq!(run.exit(), 130);
}

#[test]
fn r3_edit_refused_without_allow_and_done_with_it() {
    let edit = |id: &str| {
        json!({ "tool_calls": [
        { "id": format!("r{id}"), "name": "Read", "input": { "file_path": "notes.txt" } },
        { "id": format!("e{id}"), "name": "Edit", "input": { "file_path": "notes.txt", "old_string": "draft", "new_string": "final" } },
        { "id": format!("b{id}"), "name": "Bash", "input": { "command": "touch ran.txt" } },
    ] })
    };
    let fake = FakeModel::start(
        vec![
            edit("1"),
            json!({ "text": "I could not edit it." }),
            edit("2"),
            json!({ "text": "Edited." }),
        ],
        0,
        None,
    )
    .unwrap();
    let b = place("r3");
    write(&b.work.join("notes.txt"), "a draft note\n");
    let o = cmd(&b, &fake.url(), &["make it final"]).output().unwrap();
    let (_, stderr) = out(&o);
    assert_eq!(o.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(b.work.join("notes.txt")).unwrap(),
        "a draft note\n",
        "no file changed"
    );
    assert!(!b.work.join("ran.txt").exists());
    assert!(
        stderr
            .contains("> Edit notes.txt\n  Edit notes.txt: not allowed (run with --allow edit)\n"),
        "{stderr}"
    );
    let results = fake.bodies()[1]["messages"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()["content"]
        .clone();
    assert_eq!(
        text(&results[1]),
        "Not allowed: this run was started without --allow edit."
    );
    assert_eq!(results[1]["is_error"], true);
    assert_eq!(
        text(&results[2]),
        "Not allowed: this run was started without --allow shell."
    );
    // with --allow edit the file changes; Bash is still refused
    let o = cmd(&b, &fake.url(), &["--allow", "edit", "make it final"])
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(0), "{}", out(&o).1);
    assert_eq!(
        std::fs::read_to_string(b.work.join("notes.txt")).unwrap(),
        "a final note\n"
    );
    assert!(!b.work.join("ran.txt").exists());
    let results = fake.bodies()[3]["messages"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()["content"]
        .clone();
    assert!(text(&results[1]).starts_with("The file "), "{results}");
}

#[test]
fn r4_agents_md_is_a_second_system_block() {
    let fake = FakeModel::start(vec![json!({ "text": "ok", "repeat": true })], 0, None).unwrap();
    let b = place("r4");
    write(
        &b.work.join("AGENTS.md"),
        "Use tabs.\nRun make test before you finish.\n",
    );
    assert_eq!(
        cmd(&b, &fake.url(), &["hi"]).status().unwrap().code(),
        Some(0)
    );
    std::fs::remove_file(b.work.join("AGENTS.md")).unwrap();
    assert_eq!(
        cmd(&b, &fake.url(), &["hi"]).status().unwrap().code(),
        Some(0)
    );
    let bodies = fake.bodies();
    let with = bodies[0]["system"].as_array().unwrap();
    assert_eq!(with.len(), 2);
    assert_eq!(
        with[1]["text"],
        "Project instructions (AGENTS.md):\nUse tabs.\nRun make test before you finish.\n"
    );
    assert_eq!(bodies[1]["system"].as_array().unwrap().len(), 1);
    assert_eq!(bodies[1]["system"][0]["text"], with[0]["text"]);
}

#[test]
fn r5_session_continues_the_chat() {
    let fake = FakeModel::start(
        vec![
            json!({ "text": "The answer is 42." }),
            json!({ "text": "You asked about 42." }),
        ],
        0,
        None,
    )
    .unwrap();
    let b = place("r5");
    assert_eq!(
        cmd(&b, &fake.url(), &["--session", "a", "what is the answer?"])
            .status()
            .unwrap()
            .code(),
        Some(0)
    );
    assert_eq!(
        cmd(&b, &fake.url(), &["--session", "a", "what did I ask?"])
            .status()
            .unwrap()
            .code(),
        Some(0)
    );
    let msgs = fake.bodies()[1]["messages"].as_array().unwrap().clone();
    let texts: Vec<&str> = msgs
        .iter()
        .map(|m| m["content"][0]["text"].as_str().unwrap())
        .collect();
    assert_eq!(
        texts,
        [
            "what is the answer?",
            "The answer is 42.",
            "what did I ask?"
        ]
    );
    assert!(b.home.join(".shadow/sessions/a/wire.jsonl").exists());
    // --data-dir moves the data; a relative one is in the current folder
    assert_eq!(
        cmd(
            &b,
            &fake.url(),
            &["--data-dir", "here", "--session", "a", "x"]
        )
        .status()
        .unwrap()
        .code(),
        Some(1),
        "script used up: error"
    );
    assert!(b.work.join("here/sessions/a/wire.jsonl").exists());
    let bad = cmd(&b, &fake.url(), &["--session", "../up", "x"])
        .output()
        .unwrap();
    assert_eq!(bad.status.code(), Some(2));
}

#[test]
fn r6_r10_missing_key_or_model_is_one_line() {
    let fake = FakeModel::start(vec![], 0, None).unwrap();
    let b = place("r6");
    let no_key = cmd(&b, &fake.url(), &["hi"])
        .env_remove("ANTHROPIC_API_KEY")
        .output()
        .unwrap();
    assert_eq!(
        (no_key.status.code(), out(&no_key).1.as_str()),
        (
            Some(2),
            "shadow: no API key: set ANTHROPIC_API_KEY or OPENROUTER_API_KEY\n"
        )
    );
    let mut c = Command::new(shadow_bin());
    c.args(["run", "hi"])
        .current_dir(&b.work)
        .env("HOME", &b.home)
        .env("ANTHROPIC_API_KEY", KEY)
        .env_remove("SHADOW_MODEL");
    let no_model = c.output().unwrap();
    assert_eq!(
        (no_model.status.code(), out(&no_model).1.as_str()),
        (
            Some(2),
            "shadow: no model: give --model or set SHADOW_MODEL\n"
        )
    );
    let neither = c
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENROUTER_API_KEY")
        .output()
        .unwrap();
    assert_eq!(out(&neither).1.lines().count(), 1);
    let from_env = c
        .env("SHADOW_MODEL", "env-model")
        .env("ANTHROPIC_API_KEY", KEY)
        .args(["--base-url", &fake.url()])
        .output()
        .unwrap();
    assert_eq!(
        from_env.status.code(),
        Some(1),
        "the fake has no script: an error after the request"
    );
    assert_eq!(fake.bodies()[0]["model"], "env-model");
    assert_eq!(
        fake.requests().len(),
        1,
        "the two refused runs sent nothing"
    );
}

#[test]
fn r9_ctrl_c_kills_the_waiting_command() {
    let marker = format!("sleep 60.{}", std::process::id());
    let fake = FakeModel::start(vec![json!({ "tool_calls": [{ "name": "Bash", "input": { "command": format!("echo go; {marker}; echo never") } }] })], 0, None).unwrap();
    let b = place("r9");
    let mut run = live(cmd(&b, &fake.url(), &["--allow", "shell", "wait a minute"]));
    run.wait_err("> Bash");
    let until = Instant::now() + LONG;
    let sleeper = loop {
        if let Some(p) = pids_with(&marker).into_iter().find(|p| {
            std::fs::read(format!("/proc/{p}/cmdline"))
                .unwrap_or_default()
                .starts_with(b"sleep")
        }) {
            break p;
        }
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(20));
    };
    let pgid = pgid_of(sleeper).unwrap();
    assert_ne!(
        pgid_of(run.child.id()),
        Some(pgid),
        "the command runs in its own process group"
    );
    let sent = run.signal(libc::SIGINT);
    let code = run.exit();
    let exited = sent.elapsed();
    assert_eq!(code, 130);
    while !group_members(pgid).is_empty() {
        assert!(
            sent.elapsed() < Duration::from_secs(1),
            "process group {pgid} still has {:?}",
            group_members(pgid)
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    eprintln!(
        "R9: exit 130 {exited:?} after SIGINT; process group gone {:?} after it",
        sent.elapsed()
    );
    assert!(pids_with(&marker).is_empty());
    let wire = all_text(&b.home);
    assert!(wire.contains("Stopped by the user."));
}

#[test]
fn r9_background_commands_end_with_the_run() {
    let marker = format!("sleep 62.{}", std::process::id());
    let script = vec![
        json!({ "tool_calls": [{ "name": "Bash", "input": { "command": marker, "run_in_background": true } }] }),
        json!({ "text": "Started it; done." }),
    ];
    let fake = FakeModel::start(script, 0, None).unwrap();
    let b = place("r9bg");
    let o = cmd(&b, &fake.url(), &["--yes", "start a server"])
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(0), "{}", out(&o).1);
    assert!(
        out(&o).1.contains("running in the background as task "),
        "{}",
        out(&o).1
    );
    assert!(
        pids_with(&marker).is_empty(),
        "the background command was left running"
    );
    // the same when the run is ended by a signal while the command runs
    let marker = format!("sleep 63.{}", std::process::id());
    fake.push([json!({ "tool_calls": [{ "name": "Bash", "input": { "command": format!("trap '' TERM; {marker}"), "run_in_background": true } }] }), json!({ "text": "waiting", "delay_ms": 3000 })]);
    let mut run = live(cmd(&b, &fake.url(), &["--yes", "again"]));
    run.wait_err("running in the background");
    run.signal(libc::SIGTERM);
    assert_eq!(run.exit(), 143);
    assert!(
        pids_with(&marker).is_empty(),
        "a TERM-proof background command was left running"
    );
}

#[test]
fn the_key_never_reaches_commands_files_or_output() {
    // the command's own environment, then its parent's (the shadow process) through /proc
    let look = "echo \"seen=${ANTHROPIC_API_KEY:-none} ${OPENROUTER_API_KEY:-none}\"; env | grep -c API_KEY; cat /proc/$PPID/environ";
    let probe = json!({ "tool_calls": [{ "name": "Bash", "input": { "command": look } }] });
    let denied = json!({ "status": 401, "body": { "type": "error", "error": { "type": "authentication_error", "message": "invalid x-api-key" } } });
    let fake =
        FakeModel::start(vec![probe, json!({ "text": "checked" }), denied], 0, None).unwrap();
    let b = place("key");
    let o = cmd(&b, &fake.url(), &["--allow", "shell", "look"])
        .env("OPENROUTER_API_KEY", "or-test-SECRET-2")
        .output()
        .unwrap();
    let (stdout, stderr) = out(&o);
    assert_eq!(o.status.code(), Some(0), "{stderr}");
    let result = fake.bodies()[1]["messages"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()["content"][0]
        .clone();
    let seen = text(&result);
    assert!(
        seen.starts_with("seen=none none\n0\ncat: /proc/")
            && seen.contains("/environ: Permission denied"),
        "{seen}"
    );
    // an API error: printed, exit 1, without the request headers
    let o2 = cmd(&b, &fake.url(), &["again"]).output().unwrap();
    let (stdout2, stderr2) = out(&o2);
    assert_eq!(
        (o2.status.code(), stderr2.as_str()),
        (Some(1), "shadow: HTTP 401: invalid x-api-key\n")
    );
    for text in [
        &stdout,
        &stderr,
        &stdout2,
        &stderr2,
        &all_text(&b.home),
        &all_text(&b.work),
    ] {
        assert!(
            !text.contains(KEY) && !text.contains("or-test-SECRET-2"),
            "the key leaked: {text}"
        );
    }
}

#[test]
fn openrouter_key_uses_bearer() {
    let fake = FakeModel::start(vec![json!({ "text": "ok", "repeat": true })], 0, None).unwrap();
    let b = place("or");
    let o = cmd(&b, &fake.url(), &["hi"])
        .env_remove("ANTHROPIC_API_KEY")
        .env("OPENROUTER_API_KEY", "or-key-1")
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(0));
    let h = &fake.requests()[0].headers;
    assert!(
        h.contains(&("authorization".into(), "Bearer or-key-1".into()))
            && !h.iter().any(|(k, _)| k == "x-api-key")
    );
    cmd(&b, &fake.url(), &["hi"])
        .env("OPENROUTER_API_KEY", "or-key-1")
        .status()
        .unwrap();
    let h = &fake.requests()[1].headers;
    assert!(
        h.contains(&("x-api-key".into(), KEY.into()))
            && !h.iter().any(|(k, _)| k == "authorization"),
        "both set: Anthropic's"
    );
}

#[test]
fn usage_errors() {
    let b = place("usage");
    let run = |a: &[&str]| {
        Command::new(shadow_bin())
            .arg("run")
            .args(a)
            .current_dir(&b.work)
            .output()
            .unwrap()
    };
    assert_eq!(run(&[]).status.code(), Some(2));
    assert!(
        out(&run(&[]))
            .1
            .starts_with("shadow: no prompt given\nusage: shadow run ")
    );
    assert_eq!(run(&["--bogus", "x"]).status.code(), Some(2));
    let help = run(&["--help"]);
    assert_eq!(help.status.code(), Some(0));
    assert!(out(&help).0.starts_with("usage: shadow run "));
}

#[test]
fn a_closed_stdout_ends_the_run_instead_of_hanging() {
    use std::io::Read;
    // The text arrives, then the reader goes; the closing newline cannot be written.
    let fake = FakeModel::start(
        vec![json!({ "text": "first words", "chunk": 100, "delay_ms": 300 })],
        0,
        None,
    )
    .unwrap();
    let b = place("pipe");
    let mut child = cmd(&b, &fake.url(), &["talk"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut got = Vec::new();
    while !got.ends_with(b"first words") {
        let mut buf = [0; 64];
        let n = stdout.read(&mut buf).unwrap();
        assert!(n > 0, "stdout closed early");
        got.extend_from_slice(&buf[..n]);
    }
    drop(stdout);
    let started = Instant::now();
    let o = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&o.stderr);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "took {:?}",
        started.elapsed()
    );
    assert_eq!(
        o.status.code(),
        Some(1),
        "the output is incomplete: {stderr}"
    );
    assert_eq!(
        stderr,
        "shadow: cannot write the output (Broken pipe (os error 32)); stopping\n"
    );
}

/// Live processes whose command line contains `needle`.
fn alive(needle: &str) -> Vec<u32> {
    pids_with(needle)
}

#[test]
fn f1_key_values_are_redacted_from_stderr() {
    let or_key = "or-test-REDACT-me-7781";
    let refused = json!({ "status": 403, "body": { "type": "error", "error": { "type": "permission_error", "message": format!("permission denied for key {KEY} on this workspace") } } });
    // a tool line with a key in it, then the API error
    let read = json!({ "tool_calls": [{ "name": "Read", "input": { "file_path": format!("/nonexistent/{KEY}/{or_key}.txt") } }] });
    let fake = FakeModel::start(vec![read, refused], 0, None).unwrap();
    let b = place("f1");
    let o = cmd(&b, &fake.url(), &["go"])
        .env("OPENROUTER_API_KEY", or_key)
        .output()
        .unwrap();
    let (_, stderr) = out(&o);
    assert_eq!(o.status.code(), Some(1));
    assert!(
        stderr.contains("> Read /nonexistent/[redacted]/[redacted].txt\n"),
        "{stderr}"
    );
    assert!(
        stderr.ends_with(
            "shadow: HTTP 403: permission denied for key [redacted] on this workspace\n"
        ),
        "{stderr}"
    );
    assert!(
        !stderr.contains(KEY) && !stderr.contains(or_key),
        "{stderr}"
    );
    // the OpenRouter key alone, echoed in a 401
    let echo = json!({ "status": 401, "body": { "error": { "code": 401, "message": format!("No auth credentials found for Bearer {or_key}") } } });
    let fake = FakeModel::start(vec![echo], 0, None).unwrap();
    let o = cmd(&b, &fake.url(), &["go"])
        .env_remove("ANTHROPIC_API_KEY")
        .env("OPENROUTER_API_KEY", or_key)
        .output()
        .unwrap();
    assert_eq!(
        (o.status.code(), out(&o).1.as_str()),
        (
            Some(1),
            "shadow: HTTP 401: No auth credentials found for Bearer [redacted]\n"
        )
    );
}

#[test]
fn f2_a_closed_stderr_still_ends_everything() {
    let marker = format!("sleep 67.{}", std::process::id());
    let script = vec![
        json!({ "tool_calls": [{ "name": "Bash", "input": { "command": marker, "run_in_background": true } }] }),
        // the next tool line comes well after the reader has gone
        json!({ "text": "still going", "chunk": 100, "delay_ms": 400, "tool_calls": [{ "name": "Read", "input": { "file_path": "notes.md" } }] }),
        json!({ "text": "never" }),
    ];
    let fake = FakeModel::start(script, 0, None).unwrap();
    let b = place("f2");
    let mut child = cmd(&b, &fake.url(), &["--yes", "go"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
    while !lines
        .next()
        .unwrap()
        .unwrap()
        .contains("running in the background")
    {}
    // the command may be mid-exec (an empty cmdline) for an instant
    let until = Instant::now() + LONG;
    while alive(&marker).len() != 1 {
        assert!(
            Instant::now() < until,
            "the background command never showed"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(lines);
    let started = Instant::now();
    let code = child.wait().unwrap().code();
    assert_eq!(code, Some(1), "stopped, not 0 and not a panic (101)");
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(
        alive(&marker).is_empty(),
        "the background command was left running"
    );
}

#[test]
fn f3_processes_left_behind_by_commands_are_ended() {
    let id = std::process::id();
    let (grandchild, daemon, orphan) = (
        format!("sleep 68.{id}"),
        format!("sleep 69.{id}"),
        format!("sleep 70.{id}"),
    );
    let script = vec![
        // a double-forked grandchild and a daemon in its own session, from a foreground shell that ends at once
        json!({ "tool_calls": [{ "name": "Bash", "input": { "command": format!("( ({grandchild}; :) & ); setsid {daemon} > /dev/null 2>&1 & echo spawned") } }] }),
        // a background shell whose leader exits before its child
        json!({ "tool_calls": [{ "name": "Bash", "input": { "command": format!("{orphan} & echo leader-done"), "run_in_background": true } }] }),
        // the leader's end is noted to the model, so the turn takes one more answer
        json!({ "text": "All started.", "delay_ms": 300, "repeat": true }),
    ];
    let fake = FakeModel::start(script, 0, None).unwrap();
    let b = place("f3");
    let mut run = live(cmd(&b, &fake.url(), &["--yes", "start things"]));
    run.wait_err("leader-done");
    for m in [&grandchild, &daemon, &orphan] {
        let until = Instant::now() + LONG;
        while alive(m).is_empty() {
            assert!(Instant::now() < until, "{m} never started");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    assert_eq!(run.exit(), 0);
    for m in [&grandchild, &daemon, &orphan] {
        assert!(alive(m).is_empty(), "{m} was left running");
    }
}

/// Zombie children of `pid`.
fn zombies_of(pid: u32) -> Vec<u32> {
    let mut out = Vec::new();
    for e in std::fs::read_dir("/proc").unwrap().flatten() {
        let Ok(p) = e.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let stat = std::fs::read_to_string(e.path().join("stat")).unwrap_or_default();
        let f: Vec<&str> = stat
            .rsplit(')')
            .next()
            .unwrap_or("")
            .split_whitespace()
            .collect();
        if f.len() > 1 && f[0] == "Z" && f[1] == pid.to_string() {
            out.push(p);
        }
    }
    out
}

#[test]
fn f3_orphans_are_reaped_while_the_run_goes_on() {
    let script = vec![
        json!({ "tool_calls": [{ "name": "Bash", "input": { "command": "(sleep 0.2 &); (sleep 0.3 &); echo forked" } }] }),
        json!({ "text": "a slow answer", "chunk": 2, "delay_ms": 150 }),
    ];
    let fake = FakeModel::start(script, 0, None).unwrap();
    let b = place("f3reap");
    let mut run = live(cmd(&b, &fake.url(), &["--yes", "fork"]));
    run.wait_err("forked");
    std::thread::sleep(Duration::from_millis(800));
    assert!(
        run.child.try_wait().unwrap().is_none(),
        "the run is still going"
    );
    assert_eq!(
        zombies_of(run.child.id()),
        Vec::<u32>::new(),
        "ended orphans are reaped at once"
    );
    assert_eq!(run.exit(), 0);
}

#[test]
fn f3_ending_the_run_is_not_slowed_by_orphans() {
    let id = std::process::id();
    let (a, b2) = (format!("sleep 71.{id}"), format!("sleep 72.{id}"));
    let script = vec![
        json!({ "tool_calls": [{ "name": "Bash", "input": { "command": format!("{a} & {b2}"), "run_in_background": true } }] }),
        json!({ "text": "ok" }),
    ];
    let fake = FakeModel::start(script, 0, None).unwrap();
    let b = place("f3fast");
    let started = Instant::now();
    let o = cmd(&b, &fake.url(), &["--yes", "go"]).output().unwrap();
    let took = started.elapsed();
    assert_eq!(o.status.code(), Some(0), "{}", out(&o).1);
    // the stopped shell's orphan is reaped during the stop, so its group is
    // seen empty at once instead of after the 2 s grace
    assert!(took < Duration::from_millis(1500), "took {took:?}");
    assert!(alive(&a).is_empty() && alive(&b2).is_empty());
}

use super::*;
use crate::session::{Session, Task, TaskState};
use std::path::PathBuf;

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("shadow-unit-context-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// One call of a step: id, tool, input, result text.
type C<'a> = (&'a str, &'a str, Value, String);

/// A chat: a first user message, then per step one assistant message with
/// the calls and one user message with their results.
fn chat(dir: &Path, steps: &[Vec<C>]) -> Wire {
    let mut w = Wire::load(&dir.join("wire.jsonl")).unwrap();
    w.push_message(&json!({ "role": "user", "content": [{ "type": "text", "text": "start" }] }));
    for calls in steps {
        let uses: Vec<Value> = calls
            .iter()
            .map(|(id, name, input, _)| json!({ "type": "tool_use", "id": id, "name": name, "input": input }))
            .collect();
        w.push_message(&json!({ "role": "assistant", "content": uses }));
        let results: Vec<Value> = calls
            .iter()
            .map(|(id, _, _, text)| json!({ "type": "tool_result", "tool_use_id": id, "content": [{ "type": "text", "text": text }] }))
            .collect();
        w.push_message(&json!({ "role": "user", "content": results }));
    }
    w
}

fn view(n: usize) -> String {
    (1..=n)
        .map(|i| format!("{i:>6}\tline number {i} of the file"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn filler(i: usize) -> Vec<C<'static>> {
    vec![(
        Box::leak(format!("g{i}").into_boxed_str()),
        "Glob",
        json!({ "pattern": format!("*.{i}") }),
        "none".into(),
    )]
}

#[test]
fn fade_order_and_what_is_never_faded() {
    let dir = tmp("order");
    let big = |c: char, n: usize| c.to_string().repeat(n);
    let mut steps = vec![
        vec![("r1", "Read", json!({ "file_path": "/w/a.rs" }), view(200))],
        vec![("b2", "Bash", json!({ "command": "ls -R" }), big('l', 3000))],
        vec![("r3", "Read", json!({ "file_path": "/w/a.rs" }), view(150))],
        vec![("b4", "Bash", json!({ "command": "ls -R" }), big('m', 3000))],
        vec![
            ("g5", "Grep", json!({ "pattern": "x" }), big('g', 2500)),
            ("g6", "Grep", json!({ "pattern": "y" }), big('h', 4000)),
            ("s5", "Grep", json!({ "pattern": "z" }), big('s', 1999)),
        ],
        vec![("k6", "status", json!({ "q": 1 }), big('k', 5000))],
        vec![(
            "r7",
            "Read",
            json!({ "file_path": "/w/b.rs" }),
            "Unchanged since step 1 (lines 1–9 are above).".into(),
        )],
    ];
    for i in 8..=17 {
        steps.push(filler(i));
    }
    // a big result in the last 10 steps
    steps[9] = vec![(
        "n10",
        "Bash",
        json!({ "command": "cat big" }),
        big('n', 9000),
    )];
    let w = chat(&dir, &steps);
    assert_eq!(w.assistant_count, 17);
    let keep = vec!["status".to_string()];
    let f = fade(&w, 0, Path::new("/w"), &dir, &keep, None);
    let ids: Vec<&str> = f.stubs.keys().map(String::as_str).collect();
    // superseded first (the older Read of a.rs, the Bash repeated later),
    // then the rest oldest first, larger before smaller in one step.
    assert_eq!(ids, ["r1", "b2", "b4", "g6", "g5"]);
    assert_eq!(f.reads, vec![("/w/a.rs".to_string(), 1)]);
    let stub = f.stubs["r1"].as_str().unwrap();
    assert_eq!(
        stub,
        "[Faded to save context: Read /w/a.rs returned 200 lines (6891 characters) at step 1. Run it again if you need it.]"
    );
    assert!(!stub.contains('\n'));
}

#[test]
fn a_threshold_fade_stops_at_the_target_and_names_the_overflow_file() {
    let dir = tmp("target");
    let mut steps = vec![
        vec![("a1", "Bash", json!({ "command": "make" }), "a".repeat(8000))],
        vec![(
            "a2",
            "Bash",
            json!({ "command": "make test" }),
            "b".repeat(8000),
        )],
    ];
    for i in 3..=12 {
        steps.push(filler(i));
    }
    let mut w = chat(&dir, &steps);
    w.push_model_call(1, Some(10_000));
    std::fs::write(tools::overflow_file(&dir, "a1"), "full").unwrap();
    let f = fade(&w, 0, Path::new("/w"), &dir, &[], Some(9_000));
    assert_eq!(f.stubs.keys().collect::<Vec<_>>(), ["a1"]);
    assert!(f.size <= 9_000 && f.size >= 8_000, "{}", f.size);
    let stub = f.stubs["a1"].as_str().unwrap();
    assert!(
        stub.contains(&format!(
            "Bash make returned 1 lines (8000 characters) at step 1. Full output: {}.",
            dir.join("a1.txt").display()
        )),
        "{stub}"
    );
    // nothing left once both are faded
    let mut stubs = f.stubs;
    stubs.insert("a2".into(), "x".into());
    w.push_fade(stubs);
    assert!(
        fade(&w, 0, Path::new("/w"), &dir, &[], None)
            .stubs
            .is_empty()
    );
}

#[test]
fn cold_means_past_the_lifetime_or_cache_off() {
    let s = Settings {
        cache_ms: Some(300_000),
        ..Settings::default()
    };
    assert!(!s.cold(Some(1_000), 301_000));
    assert!(s.cold(Some(1_000), 301_001));
    assert!(!s.cold(None, 10_000_000));
    let off = Settings::default();
    assert!(off.cold(Some(1_000), 1_000));
}

#[test]
fn the_gauge_speaks_once_per_mark() {
    let line = |m: &[String], size| gauge(m, size, 200_000);
    assert_eq!(line(&[], 99_000), None);
    let first = line(&[], 104_000).unwrap();
    assert_eq!(first, "Context: 52% used (104k of 200k tokens).");
    let msg = json!({ "role": "user", "content": [{ "type": "text", "text": first }] }).to_string();
    let seen = vec![msg];
    assert_eq!(line(&seen, 120_000), None, "50% was already said");
    let jump = line(&seen, 185_000).unwrap();
    assert_eq!(
        jump, "Context: 92% used (185k of 200k tokens).",
        "75 and 90 crossed together: one line"
    );
    let both = vec![
        seen[0].clone(),
        json!({ "role": "user", "content": [{ "type": "text", "text": jump }] }).to_string(),
    ];
    assert_eq!(line(&both, 195_000), None);
    assert_eq!(
        gauge(&[], 600, 1000).unwrap(),
        "Context: 60% used (600 of 1k tokens)."
    );
}

fn call(id: &str, name: &str, input: Value) -> Call {
    Call {
        id: id.into(),
        name: name.into(),
        input,
    }
}

fn result(id: &str, text: &str) -> Value {
    json!({ "type": "tool_result", "tool_use_id": id, "content": [{ "type": "text", "text": text }] })
}

#[test]
fn the_third_identical_call_gets_one_line() {
    let mut r = Repeats::default();
    let none = HashSet::new();
    let input = json!({ "command": "git status" });
    let step = |r: &mut Repeats, n: u64, text: &str, faded: &HashSet<String>| {
        let id = format!("c{n}");
        let mut res = vec![result(&id, text)];
        r.apply(n, &[call(&id, "Bash", input.clone())], &mut res, faded);
        res[0]["content"][0]["text"].as_str().unwrap().to_string()
    };
    assert_eq!(step(&mut r, 4, "clean", &none), "clean");
    assert_eq!(step(&mut r, 5, "clean", &none), "clean");
    assert_eq!(
        step(&mut r, 6, "clean", &none),
        "Unchanged. You already have this result (step 5)."
    );
    assert_eq!(
        step(&mut r, 7, "clean", &none),
        "Unchanged. You already have this result (step 5)."
    );
    assert_eq!(
        step(&mut r, 8, "dirty", &none),
        "dirty",
        "a changed result is sent in full"
    );
    assert_eq!(step(&mut r, 9, "dirty", &none), "dirty");
    let faded: HashSet<String> = ["c9".to_string()].into();
    assert_eq!(
        step(&mut r, 10, "dirty", &faded),
        "dirty",
        "the full copy was faded: send it again"
    );
    assert_eq!(
        step(&mut r, 11, "dirty", &none),
        "Unchanged. You already have this result (step 10)."
    );
    // a step without the call breaks the row
    r.apply(12, &[], &mut [], &none);
    assert_eq!(step(&mut r, 13, "dirty", &none), "dirty");
}

fn task(id: &str, state: TaskState, code: Option<i32>) -> Task {
    Task {
        id: id.into(),
        command: format!("run {id}"),
        pgid: 0,
        started_at: 0,
        output: PathBuf::from(format!("/s/{id}.log")),
        state,
        exit_code: code,
        background: true,
        stop_requested: false,
        stopped_by_tool: false,
        announced: false,
        done: tokio::sync::watch::channel(false).1,
    }
}

#[test]
fn the_delta_names_what_changed_once() {
    let dir = tmp("delta");
    let s = Session::new("d".into(), dir.clone());
    assert_eq!(delta(Vec::new(), &s, Some(0), 30 * 60_000), None);
    s.lock().tasks.extend([
        task("b1", TaskState::Exited, Some(2)),
        task("b2", TaskState::Running, None),
        task("b3", TaskState::Exited, Some(0)),
    ]);
    s.lock().tasks[2].announced = true;
    let d = delta(
        vec!["/w/a.rs was deleted.".into()],
        &s,
        Some(0),
        31 * 60_000,
    )
    .unwrap();
    assert_eq!(
        d["text"],
        "What changed since the last model call:\n- /w/a.rs was deleted.\n- Background task b1 (run b1) ended: exit 2. Output: /s/b1.log\n- 31 minutes have passed since the last model call."
    );
    assert_eq!(
        delta(Vec::new(), &s, Some(0), 60_000),
        None,
        "b1 is not named twice"
    );
    let d = delta(Vec::new(), &s, Some(0), 5 * 3_600_000).unwrap();
    assert!(
        d["text"]
            .as_str()
            .unwrap()
            .ends_with("- 5 hours have passed since the last model call.")
    );
}

#[test]
fn the_ledger_keeps_files_commands_and_the_newest_kept_call() {
    let dir = tmp("ledger");
    let s = Session::new("l".into(), dir.clone());
    s.lock().tasks.push(task("b7", TaskState::Running, None));
    let w = chat(
        &dir,
        &[
            vec![
                (
                    "w1",
                    "Write",
                    json!({ "file_path": "/w/new.txt", "content": "x" }),
                    "File created successfully at: /w/new.txt".into(),
                ),
                ("st1", "status", json!({ "n": 1 }), "first status".into()),
            ],
            vec![(
                "e2",
                "Edit",
                json!({ "file_path": "/w/a.rs", "old_string": "a", "new_string": "b" }),
                "The file /w/a.rs has been updated.".into(),
            )],
            vec![("st2", "status", json!({ "n": 2 }), "second status".into())],
        ],
    );
    let earlier = json!({ "files": ["/w/old.txt"], "latest": [{ "name": "board", "input": {}, "result": "old board" }] });
    let keep = vec![
        "status".to_string(),
        "board".to_string(),
        "never".to_string(),
    ];
    let (l, text) = ledger(&w.messages, Some(&earlier), &s, &keep, 30_000).unwrap();
    assert_eq!(l["files"], json!(["/w/old.txt", "/w/new.txt", "/w/a.rs"]));
    assert_eq!(
        l["latest"],
        json!([{ "name": "status", "input": { "n": 2 }, "result": "second status" }, { "name": "board", "input": {}, "result": "old board" }])
    );
    assert_eq!(
        text,
        "Kept word for word at this compaction (not summarized):\n\nFiles created or changed in this chat:\n- /w/old.txt\n- /w/new.txt\n- /w/a.rs\n\nBackground commands still running:\n- b7: run b7 (output: /s/b7.log)\n\nThe newest call of status:\nInput: {\"n\":2}\nResult:\nsecond status\n\nThe newest call of board:\nInput: {}\nResult:\nold board"
    );
    s.lock().tasks.clear();
    let empty = chat(&tmp("ledger-empty"), &[]);
    assert!(ledger(&empty.messages, None, &s, &[], 30_000).is_none());
}

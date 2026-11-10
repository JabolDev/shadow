//! P5: every tool row of Appendix C, by running the daemon.

mod common;
use common::*;
use serde_json::json;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

#[test]
fn long_results_are_cut_with_an_overflow_file() {
    let mut e = Env::new("overflow", vec![], vec![]);
    let r = e.tool("o", "Bash", json!({ "command": "seq 1 100000" }));
    let out = text(&r);
    let path = r["meta"]["overflow_path"].as_str().unwrap().to_string();
    assert_eq!(r["meta"]["truncated"], true);
    assert!(
        path.ends_with(&format!(
            "sessions/o/scratch/{}.txt",
            r["call_id"].as_str().unwrap()
        )),
        "{path}"
    );
    assert!(
        out.starts_with("1\n2\n3\n") && out.ends_with("99999\n100000"),
        "{}",
        &out[out.len() - 40..]
    );
    let marker = out.lines().find(|l| l.starts_with("[… ")).unwrap();
    let omitted: usize = marker["[… ".len()..]
        .split(' ')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        marker,
        format!("[… {omitted} lines omitted. Full output: {path}]")
    );
    assert_eq!(out.lines().count() - 1 + omitted, 100_000);
    assert!(out.chars().count() <= 30_000);
    let full = fs::read_to_string(&path).unwrap();
    assert_eq!(full.lines().count(), 100_000);
    // a smaller cap from limits
    e.fake.push([json!({ "tool_calls": [{ "id": "small", "name": "Bash", "input": { "command": "seq 1 500" } }] }), json!({ "text": "ok" })]);
    let t = e.start("o", "x", json!({ "limits": { "result_cap_chars": 300 } }));
    e.end(&t);
    let small = e
        .host
        .wait_any(
            &json!({ "method": "tool.end", "params": { "call_id": "small" } }),
            LONG,
        )
        .unwrap();
    assert!(text(&small["params"]).chars().count() <= 300);
}

#[test]
fn read_numbers_lines_and_says_the_total() {
    let mut e = Env::new("read", vec![], vec![]);
    let body: String = (1..=2500).map(|i| format!("line {i}\n")).collect();
    write(&e.work.join("big.txt"), &body);
    let r = e.tool(
        "r",
        "Read",
        json!({ "file_path": "big.txt", "offset": 2400, "limit": 3 }),
    );
    assert_eq!(
        text(&r),
        "  2400\tline 2400\n  2401\tline 2401\n  2402\tline 2402\n\n(Lines 2400–2402 of 2500. Use offset and limit to read other lines.)"
    );
    let r = e.tool("r", "Read", json!({ "file_path": e.work.join("big.txt") }));
    let out = text(&r);
    assert!(
        out.starts_with("     1\tline 1\n")
            && out.contains("[… ")
            && out.ends_with("(Lines 1–2000 of 2500. Use offset and limit to read other lines.)"),
        "{}",
        &out[out.len() - 200..]
    );
    write(
        &e.work.join("wide.txt"),
        &format!("{}\nshort\n", "é".repeat(2500)),
    );
    let out = text(&e.tool("r", "Read", json!({ "file_path": "wide.txt" })));
    assert_eq!(
        out,
        format!(
            "     1\t{}… [line cut at 2000 characters]\n     2\tshort",
            "é".repeat(2000)
        )
    );
    write(&e.work.join("crlf.txt"), "a\r\nb\r\n");
    assert_eq!(
        text(&e.tool("r", "Read", json!({ "file_path": "crlf.txt" }))),
        "     1\ta\n     2\tb"
    );
    write(&e.work.join("empty.txt"), "");
    assert_eq!(
        text(&e.tool("r", "Read", json!({ "file_path": "empty.txt" }))),
        format!("{} is empty.", e.work.join("empty.txt").display())
    );
    let past = e.tool("r", "Read", json!({ "file_path": "crlf.txt", "offset": 9 }));
    assert!(text(&past).ends_with("has 2 lines; offset 9 is past its end."));
    let dir = e.tool("r", "Read", json!({ "file_path": "." }));
    assert_eq!(dir["is_error"], true);
    assert!(text(&dir).contains("is a folder"));
}

#[test]
fn read_images_binaries_and_missing_files() {
    let mut e = Env::new("readbin", vec![], vec![]);
    let png: &[u8] =
        b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\x06\0\0\0\x1f\x15\xc4\x89";
    fs::write(e.work.join("picture.dat"), png).unwrap();
    let r = e.tool("b", "Read", json!({ "file_path": "picture.dat" }));
    assert_eq!(r["content"][0]["type"], "image");
    assert_eq!(r["content"][0]["source"]["media_type"], "image/png");
    assert_eq!(r["content"][0]["source"]["type"], "base64");
    fs::write(e.work.join("photo.webp"), b"RIFF\x10\0\0\0WEBPVP8 rest").unwrap();
    assert_eq!(
        e.tool("b", "Read", json!({ "file_path": "photo.webp" }))["content"][0]["source"]["media_type"],
        "image/webp"
    );
    fs::write(e.work.join("doc.pdf"), b"%PDF-1.7\n\0\x01\x02binary").unwrap();
    let r = e.tool("b", "Read", json!({ "file_path": "doc.pdf" }));
    assert_eq!(r["is_error"], true);
    assert_eq!(
        text(&r),
        format!(
            "{} is a binary file (PDF document, 18 bytes). Read shows text and PNG, JPEG, GIF or WebP images only.",
            e.work.join("doc.pdf").display()
        )
    );
    for n in [
        "config.json",
        "config.yaml",
        "confug.json",
        "conf.json",
        "README.md",
        "Config.JSON.bak",
        "configs.json",
    ] {
        write(&e.work.join(n), "x");
    }
    let r = e.tool("b", "Read", json!({ "file_path": "config.jsn" }));
    let out = text(&r);
    assert!(
        out.starts_with(&format!(
            "File does not exist: {}. Similar names in that folder: ",
            e.work.join("config.jsn").display()
        )),
        "{out}"
    );
    assert_eq!(
        out.matches(", ").count() + 1 - out.split(": ").next().unwrap().matches(", ").count(),
        5,
        "five names: {out}"
    );
    assert!(
        out.contains(&e.work.join("config.json").display().to_string()) && !out.contains("README"),
        "{out}"
    );
    let r = e.tool("b", "Read", json!({ "file_path": "no/such/dir/x.txt" }));
    assert!(text(&r).ends_with("does not exist either."), "{}", text(&r));
}

#[test]
fn read_again_unchanged_is_one_line() {
    let mut e = Env::new("reread", vec![], vec![]);
    write(&e.work.join("f.txt"), "one\ntwo\nthree\nfour\n");
    let first = text(&e.tool("u", "Read", json!({ "file_path": "f.txt" })));
    assert!(first.starts_with("     1\tone"));
    // the first Read was the chat's step 1 (its first assistant message)
    assert_eq!(
        text(&e.tool("u", "Read", json!({ "file_path": "f.txt" }))),
        "Unchanged since step 1 (lines 1–4 are above)."
    );
    assert_eq!(
        text(&e.tool(
            "u",
            "Read",
            json!({ "file_path": "f.txt", "offset": 2, "limit": 2 })
        )),
        "Unchanged since step 1 (lines 2–3 are above)."
    );
    write(&e.work.join("f.txt"), "one\nTWO\nthree\nfour\n");
    assert!(
        text(&e.tool(
            "u",
            "Read",
            json!({ "file_path": "f.txt", "offset": 2, "limit": 1 })
        ))
        .starts_with("     2\tTWO")
    );
    // after compaction the lines are no longer "above"
    e.fake
        .push([json!({ "match": "Write a summary", "text": "summary" })]);
    let r = e.host.call(
        "session.compact",
        json!({ "session": "u", "model": "m", "endpoint": { "url": e.fake.url() } }),
    );
    assert!(r.get("result").is_some(), "{r}");
    assert!(
        text(&e.tool(
            "u",
            "Read",
            json!({ "file_path": "f.txt", "offset": 2, "limit": 1 })
        ))
        .starts_with("     2\tTWO")
    );
}

#[test]
fn write_needs_the_current_version() {
    let mut e = Env::new("write", vec![], vec![]);
    let r = e.tool(
        "w",
        "Write",
        json!({ "file_path": "deep/new/dir/n.txt", "content": "hello\n" }),
    );
    assert_eq!(
        text(&r),
        format!(
            "File created successfully at: {}",
            e.work.join("deep/new/dir/n.txt").display()
        )
    );
    // written by the model: it may overwrite
    let r = e.tool(
        "w",
        "Write",
        json!({ "file_path": "deep/new/dir/n.txt", "content": "hello again\n" }),
    );
    assert!(text(&r).starts_with("The file ") && text(&r).ends_with(" has been updated."));
    write(&e.work.join("theirs.txt"), "not read\n");
    let r = e.tool(
        "w",
        "Write",
        json!({ "file_path": "theirs.txt", "content": "x" }),
    );
    assert_eq!(r["is_error"], true);
    assert_eq!(
        text(&r),
        format!(
            "{} has not been read yet. Read it first, then change it.",
            e.work.join("theirs.txt").display()
        )
    );
    assert_eq!(
        fs::read_to_string(e.work.join("theirs.txt")).unwrap(),
        "not read\n"
    );
    e.tool("w", "Read", json!({ "file_path": "theirs.txt" }));
    write(&e.work.join("theirs.txt"), "not read\nchanged by someone\n");
    let r = e.tool(
        "w",
        "Write",
        json!({ "file_path": "theirs.txt", "content": "mine\n" }),
    );
    let out = text(&r);
    assert!(
        out.contains(
            "changed since you last saw it. What changed (seen → now):\n--- seen\n+++ now\n"
        ) && out.contains("+changed by someone"),
        "{out}"
    );
    let r = e.tool(
        "w",
        "Write",
        json!({ "file_path": "theirs.txt", "content": "mine\n" }),
    );
    assert_eq!(
        r["is_error"], false,
        "the diff fitted, so the new version counts as seen"
    );
    assert_eq!(
        fs::read_to_string(e.work.join("theirs.txt")).unwrap(),
        "mine\n"
    );
}

#[test]
fn edit_rules() {
    let mut e = Env::new("edit", vec![], vec![]);
    let f = e.work.join("code.rs");
    let src: String = (1..=12)
        .map(|i| format!("let v{i} = {i};\n"))
        .collect::<String>()
        + "dup\ndup\n";
    write(&f, &src);
    let r = e.tool(
        "e",
        "Edit",
        json!({ "file_path": "code.rs", "old_string": "v1 ", "new_string": "w1 " }),
    );
    assert!(text(&r).starts_with(&format!("{} has not been read yet.", f.display())));
    e.tool("e", "Read", json!({ "file_path": "code.rs" }));
    let r = e.tool("e", "Edit", json!({ "file_path": "code.rs", "old_string": "let v6 = 6;", "new_string": "let six = 6;\nlet extra = 0;" }));
    assert_eq!(
        text(&r),
        format!(
            "The file {} has been updated. Changed lines:\n     3\tlet v3 = 3;\n     4\tlet v4 = 4;\n     5\tlet v5 = 5;\n     6\tlet six = 6;\n     7\tlet extra = 0;\n     8\tlet v7 = 7;\n     9\tlet v8 = 8;\n    10\tlet v9 = 9;",
            f.display()
        )
    );
    let r = e.tool(
        "e",
        "Edit",
        json!({ "file_path": "code.rs", "old_string": "dup", "new_string": "x" }),
    );
    assert_eq!(
        text(&r),
        format!(
            "old_string appears 2 times in {} (lines 14, 15). Add surrounding text to make it unique, or set replace_all.",
            f.display()
        )
    );
    let r = e.tool(
        "e",
        "Edit",
        json!({ "file_path": "code.rs", "old_string": "let v9 = 99;", "new_string": "x" }),
    );
    assert_eq!(
        text(&r),
        format!(
            "old_string was not found in {}. The nearest match starts at line 10:\n    10\tlet v9 = 9;",
            f.display()
        )
    );
    // edits: all or nothing; the failure names the edit
    let before = fs::read_to_string(&f).unwrap();
    let r = e.tool("e", "Edit", json!({ "file_path": "code.rs", "edits": [{ "old_string": "v2", "new_string": "two" }, { "old_string": "missing text", "new_string": "y" }] }));
    assert!(
        text(&r).starts_with("Edit 2 of 2 failed: old_string was not found")
            && text(&r).ends_with("Nothing was written."),
        "{}",
        text(&r)
    );
    assert_eq!(fs::read_to_string(&f).unwrap(), before);
    let r = e.tool("e", "Edit", json!({ "file_path": "code.rs", "edits": [{ "old_string": "v2", "new_string": "two" }, { "old_string": "dup", "new_string": "d", "replace_all": true }] }));
    assert_eq!(r["is_error"], false, "{r}");
    let now = fs::read_to_string(&f).unwrap();
    assert!(now.contains("let two = 2;") && now.ends_with("d\nd\n"));
    // changed by someone else: the diff; then it counts as seen
    fs::write(&f, now.replace("let v3 = 3;", "let v3 = 33;")).unwrap();
    let r = e.tool("e", "Edit", json!({ "file_path": "code.rs", "old_string": "let v4 = 4;", "new_string": "let four = 4;" }));
    let out = text(&r);
    assert!(
        out.contains("-let v3 = 3;\n+let v3 = 33;") && out.contains("now counts as seen"),
        "{out}"
    );
    let r = e.tool("e", "Edit", json!({ "file_path": "code.rs", "old_string": "let v4 = 4;", "new_string": "let four = 4;" }));
    assert_eq!(r["is_error"], false);
    // no temp file left behind
    assert_eq!(fs::read_dir(&e.work).unwrap().count(), 1);
}

#[test]
fn edit_keeps_mode_line_endings_and_final_newline() {
    let mut e = Env::new("editkeep", vec![], vec![]);
    let crlf = e.work.join("win.txt");
    fs::write(&crlf, "first\r\nsecond\r\nthird\r\n").unwrap();
    fs::set_permissions(&crlf, fs::Permissions::from_mode(0o750)).unwrap();
    e.tool("k", "Read", json!({ "file_path": "win.txt" }));
    let r = e.tool("k", "Edit", json!({ "file_path": "win.txt", "old_string": "second\nthird", "new_string": "2nd\nline\nthird" }));
    assert_eq!(r["is_error"], false, "{r}");
    assert_eq!(
        fs::read(&crlf).unwrap(),
        b"first\r\n2nd\r\nline\r\nthird\r\n"
    );
    assert_eq!(
        fs::metadata(&crlf).unwrap().permissions().mode() & 0o777,
        0o750
    );
    let bare = e.work.join("bare.txt");
    fs::write(&bare, "a\nb").unwrap();
    e.tool("k", "Read", json!({ "file_path": "bare.txt" }));
    e.tool(
        "k",
        "Edit",
        json!({ "file_path": "bare.txt", "old_string": "b", "new_string": "b\nc\n" }),
    );
    assert_eq!(
        fs::read_to_string(&bare).unwrap(),
        "a\nb\nc",
        "no final newline before, none after"
    );
    let ended = e.work.join("ended.txt");
    fs::write(&ended, "x\ny\n").unwrap();
    e.tool("k", "Read", json!({ "file_path": "ended.txt" }));
    e.tool(
        "k",
        "Edit",
        json!({ "file_path": "ended.txt", "old_string": "y\n", "new_string": "z" }),
    );
    assert_eq!(fs::read_to_string(&ended).unwrap(), "x\nz\n");
}

#[test]
fn bash_runs_fresh_in_cwd_every_time() {
    let mut e = Env::new("bash", vec![], vec![]);
    let r = e.tool(
        "b",
        "Bash",
        json!({ "command": "cd /; export LEFT=1; pwd" }),
    );
    assert_eq!(text(&r), "/");
    let r = e.tool(
        "b",
        "Bash",
        json!({ "command": "pwd; echo \"left=${LEFT:-unset}\"; echo err >&2" }),
    );
    assert_eq!(text(&r), format!("{}\nleft=unset\nerr", e.work.display()));
    let r = e.tool("b", "Bash", json!({ "command": "true" }));
    assert_eq!(
        (text(&r).as_str(), &r["is_error"], &r["meta"]["exit_code"]),
        ("(no output)", &json!(false), &json!(0))
    );
    let r = e.tool("b", "Bash", json!({ "command": "echo almost; exit 7" }));
    assert_eq!(
        (text(&r).as_str(), &r["is_error"], &r["meta"]["exit_code"]),
        ("almost\nExit code 7", &json!(true), &json!(7))
    );
    let r = e.tool("b", "Bash", json!({ "command": "exit 2" }));
    assert_eq!(text(&r), "Exit code 2");
    // the turn's env is added for every command
    e.fake.push([json!({ "tool_calls": [{ "id": "env1", "name": "Bash", "input": { "command": "echo $GREETING-$HOME" } }] }), json!({ "text": "ok" })]);
    let t = e.start("b", "env", json!({ "env": { "GREETING": "hi" } }));
    e.end(&t);
    let r = e
        .host
        .wait_any(
            &json!({ "method": "tool.end", "params": { "call_id": "env1" } }),
            LONG,
        )
        .unwrap();
    assert_eq!(
        text(&r["params"]),
        format!("hi-{}", std::env::var("HOME").unwrap())
    );
}

#[test]
fn bash_wait_limit_moves_to_the_background_without_killing() {
    let mut e = Env::new("bashwait", vec![], vec![]);
    let started = Instant::now();
    let r = e.tool(
        "bw",
        "Bash",
        json!({ "command": "echo early; sleep 2; echo late", "timeout": 1000 }),
    );
    assert!(started.elapsed() < Duration::from_millis(1900));
    let task = r["meta"]["task_id"].as_str().unwrap().to_string();
    let log = e.data.join(format!("sessions/bw/scratch/tasks/{task}.log"));
    assert_eq!(
        text(&r),
        format!(
            "early\nStill running in the background as task {task}. Output: {}. You will be told when it ends.",
            log.display()
        )
    );
    e.host.wait_any(&json!({ "method": "task.update", "params": { "task": { "id": task, "state": "exited", "exit_code": 0 } } }), LONG).unwrap();
    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        "early\nlate\n",
        "not killed at the limit"
    );
    // bash_wait_ms from limits; run_in_background at once
    e.fake.push([json!({ "tool_calls": [{ "id": "lim", "name": "Bash", "input": { "command": "sleep 1; echo done" } }] }), json!({ "text": "ok" })]);
    let t = e.start("bw", "x", json!({ "limits": { "bash_wait_ms": 200 } }));
    e.end(&t);
    let lim = e
        .host
        .wait_any(
            &json!({ "method": "tool.end", "params": { "call_id": "lim" } }),
            LONG,
        )
        .unwrap();
    assert!(text(&lim["params"]).starts_with("Still running in the background as task "));
    let r = e.tool(
        "bw",
        "Bash",
        json!({ "command": "sleep 0.5", "run_in_background": true, "description": "nap" }),
    );
    assert!(text(&r).starts_with("Still running in the background as task "));
}

#[test]
fn grep_finds_with_ripgrep_rules() {
    let mut e = Env::new("grep", vec![], vec![]);
    write(&e.work.join(".gitignore"), "ignored/\n*.log\n");
    write(
        &e.work.join("src/a.rs"),
        "fn alpha() {}\nlet Needle = 1;\nfn beta() {}\n",
    );
    write(&e.work.join("src/b.rs"), "needle here\nand needle there\n");
    write(&e.work.join("src/c.txt"), "needle in text\n");
    write(&e.work.join("ignored/x.rs"), "needle\n");
    write(&e.work.join("run.log"), "needle\n");
    write(&e.work.join(".hidden/h.rs"), "needle\n");
    fs::write(e.work.join("bin.dat"), b"needle\0\x01\x02").unwrap();
    let w = e.work.display().to_string();
    let r = e.tool("g", "Grep", json!({ "pattern": "needle" }));
    assert_eq!(
        text(&r),
        format!("Found 3 files\n{w}/.hidden/h.rs\n{w}/src/b.rs\n{w}/src/c.txt")
    );
    let r = e.tool(
        "g",
        "Grep",
        json!({ "pattern": "needle", "-i": true, "glob": "*.rs", "output_mode": "count" }),
    );
    assert_eq!(
        text(&r),
        format!("{w}/.hidden/h.rs:1\n{w}/src/a.rs:1\n{w}/src/b.rs:2")
    );
    let r = e.tool("g", "Grep", json!({ "pattern": "Needle", "path": "src", "output_mode": "content", "-n": true, "-C": 1 }));
    assert_eq!(
        text(&r),
        format!(
            "{w}/src/a.rs:1:fn alpha() {{}}\n{w}/src/a.rs:2:let Needle = 1;\n{w}/src/a.rs:3:fn beta() {{}}"
        )
    );
    let r = e.tool(
        "g",
        "Grep",
        json!({ "pattern": "needle", "output_mode": "content", "head_limit": 2 }),
    );
    assert_eq!(
        text(&r),
        format!("{w}/.hidden/h.rs:needle\n{w}/src/b.rs:needle here\n(2 more not shown)")
    );
    let r = e.tool("g", "Grep", json!({ "pattern": "alpha.*\\n.*Needle", "multiline": true, "output_mode": "content", "-n": true }));
    assert_eq!(
        text(&r),
        format!("{w}/src/a.rs:1:fn alpha() {{}}\n{w}/src/a.rs:2:let Needle = 1;")
    );
    let r = e.tool(
        "g",
        "Grep",
        json!({ "pattern": "needle", "glob": "src/*.txt" }),
    );
    assert_eq!(text(&r), format!("Found 1 file\n{w}/src/c.txt"));
    let r = e.tool("g", "Grep", json!({ "pattern": "zzz" }));
    assert_eq!(text(&r), "No files found");
    let r = e.tool("g", "Grep", json!({ "pattern": "(" }));
    assert!(text(&r).starts_with("Invalid pattern:") && r["is_error"] == true);
}

#[test]
fn grep_and_glob_results_are_cut_like_every_tool() {
    let mut e = Env::new("grepcap", vec![], vec![]);
    let body: String = (0..3000)
        .map(|i| format!("entry {i:05} of the ledger\n"))
        .collect();
    write(&e.work.join("ledger.txt"), &body);
    // over the cap: first part, last part, the marker, the full text in the overflow file
    let r = e.tool(
        "gc",
        "Grep",
        json!({ "pattern": "ledger", "output_mode": "content", "-n": true }),
    );
    let out = text(&r);
    assert!(out.chars().count() <= 30_000, "{}", out.chars().count());
    let path = r["meta"]["overflow_path"].as_str().unwrap().to_string();
    assert!(
        out.contains(&format!(" lines omitted. Full output: {path}]")),
        "{}",
        &out[..200]
    );
    assert!(
        out.ends_with(":3000:entry 02999 of the ledger"),
        "the last part is kept"
    );
    let full = fs::read_to_string(&path).unwrap();
    assert_eq!(full.lines().count(), 3000);
    assert!(full.lines().all(|l| l.contains(":entry ")));
    // head_limit keeps its own count, and the whole list spills to a file when it is large
    let wide: String = (0..40_000)
        .map(|i| format!("row {i:06} {}\n", "w".repeat(30)))
        .collect();
    write(&e.work.join("wide.txt"), &wide);
    let r = e.tool("gc", "Grep", json!({ "pattern": "^row", "path": "wide.txt", "output_mode": "content", "head_limit": 35_000 }));
    let out = text(&r);
    assert!(out.chars().count() <= 30_000);
    assert_eq!(out.lines().last().unwrap(), "(5000 more not shown)");
    let path = r["meta"]["overflow_path"].as_str().unwrap().to_string();
    let full = fs::read_to_string(&path).unwrap();
    assert_eq!(full.lines().count(), 35_001);
    assert!(
        full.lines()
            .nth(34_999)
            .unwrap()
            .ends_with(&format!("row 034999 {}", "w".repeat(30)))
    );
    assert!(
        !fs::read_dir(e.data.join("sessions/gc/scratch"))
            .unwrap()
            .any(|f| f.unwrap().path().extension().is_some_and(|x| x == "part"))
    );
    // Glob within a small cap
    for i in 0..40 {
        write(&e.work.join(format!("many/deeper/name-{i:02}.txt")), "x");
    }
    e.fake.push([json!({ "tool_calls": [{ "id": "gl", "name": "Glob", "input": { "pattern": "many/**/*.txt" } }] }), json!({ "text": "ok" })]);
    let t = e.start(
        "gc",
        "list",
        json!({ "limits": { "result_cap_chars": 400 } }),
    );
    e.end(&t);
    let r = e
        .host
        .wait_any(
            &json!({ "method": "tool.end", "params": { "call_id": "gl" } }),
            LONG,
        )
        .unwrap()["params"]
        .clone();
    let out = text(&r);
    assert!(out.chars().count() <= 400, "{}", out.chars().count());
    let full = fs::read_to_string(r["meta"]["overflow_path"].as_str().unwrap()).unwrap();
    assert_eq!(full.lines().count(), 40);
}

#[test]
fn glob_newest_first_and_bounded() {
    let mut e = Env::new("glob", vec![], vec![]);
    for i in 0..105 {
        write(&e.work.join(format!("d/f{i:03}.txt")), "x");
    }
    write(&e.work.join("d/sub/deep.md"), "x");
    let newest = e.work.join("d/f007.txt");
    let later = std::time::SystemTime::now() + Duration::from_secs(60);
    fs::File::options()
        .write(true)
        .open(&newest)
        .unwrap()
        .set_modified(later)
        .unwrap();
    let r = e.tool("gl", "Glob", json!({ "pattern": "**/*.txt" }));
    let out = text(&r);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], newest.display().to_string());
    assert_eq!(lines.len(), 101);
    assert_eq!(lines[100], "(5 more not shown)");
    let r = e.tool("gl", "Glob", json!({ "pattern": "*.md", "path": "d/sub" }));
    assert_eq!(text(&r), e.work.join("d/sub/deep.md").display().to_string());
    let r = e.tool(
        "gl",
        "Glob",
        json!({ "pattern": format!("{}/d/**/*.{{md,none}}", e.work.display()) }),
    );
    assert_eq!(text(&r), e.work.join("d/sub/deep.md").display().to_string());
    let r = e.tool("gl", "Glob", json!({ "pattern": "*.txt" }));
    assert_eq!(text(&r), "No files found", "* stays within one folder");
    let r = e.tool("gl", "Glob", json!({ "pattern": "*", "path": "missing" }));
    assert_eq!(
        (text(&r), &r["is_error"]),
        (
            format!(
                "Folder does not exist: {}",
                e.work.join("missing").display()
            ),
            &json!(true)
        )
    );
}

#[test]
fn checked_input_is_what_runs() {
    let rules = vec![
        json!({ "method": "tool.check", "match": { "call": { "name": "Bash" } }, "reply": { "decision": "allow", "input": { "command": "echo rewritten" } } }),
    ];
    let mut e = Env::new("checkinput", vec![], rules);
    let r = e.tool("ci", "Bash", json!({ "command": "echo original" }));
    assert_eq!(text(&r), "rewritten");
    assert_eq!(r["input"]["command"], "echo rewritten");
    let start = e
        .host
        .wait_any(
            &json!({ "method": "tool.start", "params": { "call": { "name": "Bash" } } }),
            LONG,
        )
        .unwrap();
    assert_eq!(
        start["params"]["call"]["input"]["command"],
        "echo rewritten"
    );
    // the logged model message keeps what the model sent
    assert!(e.wire("ci").contains("echo original"));
}

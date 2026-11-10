use super::*;

fn args(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

#[test]
fn parses_flags_and_the_prompt() {
    let o = parse(&args(&[
        "--model", "m1", "--allow", "edit", "fix", "the", "bug",
    ]))
    .unwrap()
    .unwrap();
    assert_eq!(
        (
            o.prompt.as_str(),
            o.model.as_deref(),
            o.edit,
            o.shell,
            o.max_tokens
        ),
        ("fix the bug", Some("m1"), true, false, 16000)
    );
    let o = parse(&args(&[
        "--allow=edit,shell",
        "--max-tokens=2048",
        "--session",
        "a",
        "--data-dir",
        "d",
        "--base-url",
        "http://x/y",
        "hi",
    ]))
    .unwrap()
    .unwrap();
    assert!(o.edit && o.shell);
    assert_eq!(
        (
            o.max_tokens,
            o.session.as_deref(),
            o.data_dir,
            o.base_url.as_deref()
        ),
        (
            2048,
            Some("a"),
            Some(PathBuf::from("d")),
            Some("http://x/y")
        )
    );
    let o = parse(&args(&["--yes", "--", "--not-a-flag"]))
        .unwrap()
        .unwrap();
    assert!(o.edit && o.shell && o.prompt == "--not-a-flag");
    assert_eq!(parse(&args(&["--help"])).unwrap(), None);
}

#[test]
fn bad_arguments_are_errors() {
    assert_eq!(parse(&args(&[])).unwrap_err(), "no prompt given");
    assert_eq!(
        parse(&args(&["--model"])).unwrap_err(),
        "--model needs a value"
    );
    assert_eq!(
        parse(&args(&["--allow", "net", "x"])).unwrap_err(),
        "--allow takes edit or shell, not \"net\""
    );
    assert_eq!(
        parse(&args(&["--max-tokens", "0", "x"])).unwrap_err(),
        "--max-tokens needs a positive number"
    );
    assert_eq!(
        parse(&args(&["--frobnicate", "x"])).unwrap_err(),
        "unknown option --frobnicate"
    );
}

#[test]
fn the_endpoint_rule() {
    let k = |s: &str| Some(s.to_string());
    let anthropic = endpoint(k("a-key"), None, None).unwrap();
    assert_eq!(
        anthropic,
        (
            "https://api.anthropic.com/v1/messages".into(),
            ("x-api-key".into(), "a-key".into())
        )
    );
    let openrouter = endpoint(None, k("o-key"), None).unwrap();
    assert_eq!(
        openrouter,
        (
            "https://openrouter.ai/api/v1/messages".into(),
            ("authorization".into(), "Bearer o-key".into())
        )
    );
    // both set: Anthropic's key; --base-url replaces only the URL
    let both = endpoint(
        k("a-key"),
        k("o-key"),
        Some("http://127.0.0.1:9/v1/messages"),
    )
    .unwrap();
    assert_eq!(
        both,
        (
            "http://127.0.0.1:9/v1/messages".into(),
            ("x-api-key".into(), "a-key".into())
        )
    );
    assert_eq!(endpoint(None, None, Some("http://x/")), None);
}

#[test]
fn labels_are_short() {
    assert_eq!(
        label("Read", &json!({ "file_path": "src/a.rs" })),
        "Read src/a.rs"
    );
    assert_eq!(
        label("Bash", &json!({ "command": "make\nmake test" })),
        "Bash make…"
    );
    assert_eq!(
        label("Grep", &json!({ "pattern": "x".repeat(100) })),
        format!("Grep {}…", "x".repeat(80))
    );
    assert_eq!(
        label("TaskStop", &json!({ "task_id": "b1" })),
        "TaskStop b1"
    );
    assert_eq!(label("Glob", &json!({})), "Glob");
}

#[test]
fn the_default_prompt_names_the_folder() {
    let text = PROMPT.replace("{cwd}", "/w/p");
    assert!(text.starts_with("You are Shadow, a coding agent working in the user's terminal, in the folder /w/p.\n\nDo what"));
    assert!(text.ends_with("what you did, what you found, and anything left open."));
}

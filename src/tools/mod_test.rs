use super::*;

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("shadow-unit-tools-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn resolves_paths() {
    let cwd = Path::new("/w/project");
    assert_eq!(resolve(cwd, "a/b.txt"), PathBuf::from("/w/project/a/b.txt"));
    assert_eq!(resolve(cwd, "./a/../c"), PathBuf::from("/w/project/c"));
    assert_eq!(
        resolve(cwd, "/etc/./x/../hosts"),
        PathBuf::from("/etc/hosts")
    );
    assert_eq!(resolve(cwd, "../../.."), PathBuf::from("/"));
}

#[test]
fn short_text_is_kept_whole() {
    let d = tmp("short");
    let b = bound("a\nb\nc", 100, &d.join("o.txt")).unwrap();
    assert_eq!(
        (b.text.as_str(), b.head, b.overflow.is_none()),
        ("a\nb\nc", 3, true)
    );
    assert!(!d.join("o.txt").exists());
}

#[test]
fn long_text_keeps_head_and_tail() {
    let d = tmp("long");
    let text: String = (1..=1000).map(|i| format!("line {i}\n")).collect();
    let o = d.join("call.txt");
    let b = bound(&text, 200, &o).unwrap();
    assert!(b.text.starts_with("line 1\nline 2\n"));
    assert!(b.text.ends_with("line 999\nline 1000\n"), "{}", b.text);
    let omitted = 1001 - b.head - b.tail;
    assert!(
        b.text.contains(&format!(
            "[… {omitted} lines omitted. Full output: {}]",
            o.display()
        )),
        "{}",
        b.text
    );
    assert!(b.text.chars().count() <= 200, "{}", b.text.chars().count());
    assert_eq!(fs::read_to_string(&o).unwrap(), text);
}

#[test]
fn one_huge_line_is_cut_by_characters() {
    let d = tmp("huge");
    let text = "x".repeat(10_000);
    let b = bound(&text, 300, &d.join("o.txt")).unwrap();
    assert!(
        b.text.starts_with("xxxxxxxxxx") && b.text.contains("[… 1 lines omitted."),
        "{}",
        b.text
    );
    assert!(b.text.chars().count() <= 300, "{}", b.text.chars().count());
}

#[test]
fn big_files_are_read_at_both_ends() {
    let d = tmp("file");
    let log = d.join("task.log");
    let text: String = (1..=200_000).map(|i| format!("row {i}\n")).collect();
    fs::write(&log, &text).unwrap();
    let o = d.join("o.txt");
    let b = bound_file(&log, 1000, &o).unwrap();
    assert!(
        b.text.starts_with("row 1\n") && b.text.ends_with("row 200000"),
        "{}",
        &b.text[b.text.len() - 50..]
    );
    assert!(
        b.text
            .contains(&format!("[… {} lines omitted.", 200_000 - b.head - b.tail))
    );
    assert_eq!(fs::metadata(&o).unwrap().len(), text.len() as u64);
}

#[test]
fn arguments_are_typed() {
    let v = serde_json::json!({ "s": "x", "n": 3.9, "b": true, "bad": [1] });
    assert_eq!(opt_str(&v, "s").unwrap(), Some("x"));
    assert!(opt_str(&v, "bad").is_err());
    assert_eq!(req_str(&v, "missing").unwrap_err(), "missing is required.");
    assert_eq!(opt_num(&v, "n").unwrap(), Some(3));
    assert!(opt_bool(&v, "b").unwrap() && !opt_bool(&v, "none").unwrap());
}

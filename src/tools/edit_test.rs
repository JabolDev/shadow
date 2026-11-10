use super::*;

const P: &str = "/f.txt";

#[test]
fn replaces_one_or_all() {
    let p = Path::new(P);
    assert_eq!(apply("a b a", "b", "c", false, p).unwrap(), "a c a");
    assert_eq!(apply("a b a", "a", "z", true, p).unwrap(), "z b z");
    assert!(apply("x", "", "y", false, p).unwrap_err().contains("empty"));
    assert!(apply("x", "x", "x", false, p).unwrap_err().contains("same"));
}

#[test]
fn not_unique_lists_every_line() {
    let src: String = (1..=30)
        .map(|i| {
            if i % 2 == 0 {
                "dup\n".to_string()
            } else {
                format!("line {i}\n")
            }
        })
        .collect();
    let err = apply(&src, "dup", "x", false, Path::new(P)).unwrap_err();
    assert!(
        err.starts_with("old_string appears 15 times in /f.txt (lines 2, 4, 6,"),
        "{err}"
    );
    assert!(err.contains("30") && !err.contains("and"), "{err}");
    let src: String = (0..25).map(|_| "dup\n").collect();
    let err = apply(&src, "dup", "x", false, Path::new(P)).unwrap_err();
    assert!(
        err.contains("lines 1, 2,") && err.contains("20, and 5 more"),
        "{err}"
    );
}

#[test]
fn not_found_points_at_the_nearest_match() {
    let src = "fn main() {\n    let total = compute(1, 2);\n    println!(\"{total}\");\n}\n";
    let err = apply(
        src,
        "let total = compute(1,2);\nprintln!(\"{totl}\");",
        "x",
        false,
        Path::new(P),
    )
    .unwrap_err();
    assert!(
        err.contains("not found") && err.contains("nearest match starts at line 2"),
        "{err}"
    );
    assert!(
        err.contains("     2\t    let total = compute(1, 2);"),
        "{err}"
    );
    let err = apply(
        src,
        "something else entirely different",
        "x",
        false,
        Path::new(P),
    )
    .unwrap_err();
    assert!(!err.contains("nearest"), "{err}");
}

#[test]
fn whitespace_only_differences_are_a_perfect_near_match() {
    let src = "a\n\tif x {\n\t\ty()\n\t}\nb\n";
    assert_eq!(near_match(src, "if x {\n  y()\n}").map(|m| m.0), Some(2));
}

#[test]
fn changed_lines_show_three_around() {
    let before: String = (1..=20).map(|i| format!("l{i}\n")).collect();
    let after = before.replace("l10\n", "ten\nTEN\n");
    let out = changed_lines(&before, &after);
    let first = out.lines().next().unwrap();
    assert_eq!(first, "     7\tl7");
    assert!(out.contains("    10\tten\n    11\tTEN"));
    assert_eq!(out.lines().last().unwrap(), "    14\tl13");
    let two = changed_lines(
        &before,
        &before.replace("l2\n", "two\n").replace("l18\n", "x\n"),
    );
    assert!(two.contains("     …"), "{two}");
}

#[test]
fn atomic_write_keeps_the_mode() {
    use std::os::unix::fs::PermissionsExt;
    let d = std::env::temp_dir().join(format!("shadow-unit-edit-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    let f = d.join("run.sh");
    fs::write(&f, "old").unwrap();
    fs::set_permissions(&f, fs::Permissions::from_mode(0o751)).unwrap();
    write_atomic(&f, b"new").unwrap();
    assert_eq!(fs::read_to_string(&f).unwrap(), "new");
    assert_eq!(
        fs::metadata(&f).unwrap().permissions().mode() & 0o777,
        0o751
    );
    assert_eq!(fs::read_dir(&d).unwrap().count(), 1, "no temp file left");
}

use super::*;

#[test]
fn redacts_every_key_value() {
    let keys = vec!["sk-AAA".to_string(), "or-BB".to_string()];
    assert_eq!(
        redact(b"key sk-AAA and or-BB, sk-AAAsk-AAA.", &keys),
        b"key [redacted] and [redacted], [redacted][redacted]."
    );
    assert_eq!(
        redact(b"sk-AA is not the key", &keys),
        b"sk-AA is not the key"
    );
    assert_eq!(
        redact("é sk-AAA ü".as_bytes(), &keys),
        "é [redacted] ü".as_bytes()
    );
    assert_eq!(redact(b"nothing", &[]), b"nothing");
}

#[test]
fn output_is_never_cut_through_a_key() {
    let keys = vec!["sk-SPLIT-0001".to_string(), "or-2".to_string()];
    // whole lines go out; the rest waits
    assert_eq!(ready(b"one\ntwo", &keys), 4);
    assert_eq!(ready(b"no end yet", &keys), 0);
    // a long line without an end: all but a key-long tail …
    let long = "x".repeat(70_000);
    assert_eq!(ready(long.as_bytes(), &keys), 70_000 - 12);
    // … and never through a key that crosses that point
    for back in 1..13 {
        let text = format!(
            "{}sk-SPLIT-0001{}",
            &long[..70_000 - 12 - back],
            "y".repeat(12 + back - 13 + 1)
        );
        let cut = ready(text.as_bytes(), &keys);
        let start = text.find("sk-SPLIT").unwrap();
        assert!(
            cut <= start || cut >= start + 13,
            "back {back}: cut {cut} is inside the key at {start}"
        );
        let mut out = redact(&text.as_bytes()[..cut], &keys);
        out.extend(redact(&text.as_bytes()[cut..], &keys));
        assert!(
            String::from_utf8(out).unwrap().contains("[redacted]"),
            "back {back}"
        );
    }
}

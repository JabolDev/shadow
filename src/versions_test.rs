use super::*;

fn v(hash: &str, lines: usize) -> Version<'_> {
    Version {
        hash,
        size: 10,
        lines,
    }
}

#[test]
fn fnv1a_known_values() {
    assert_eq!(hash(b""), "cbf29ce484222325");
    assert_eq!(hash(b"a"), "af63dc4c8601ec8c");
    let mut h = Hasher::new();
    h.update(b"fo");
    h.update(b"obar");
    assert_eq!(h.hex(), hash(b"foobar"));
}

#[test]
fn coverage_and_gaps() {
    let shown = |r: &[(usize, usize)]| {
        r.iter()
            .map(|&(from, to)| Shown { step: 1, from, to })
            .collect::<Vec<_>>()
    };
    assert_eq!(unseen(&shown(&[(1, 10)]), 10), vec![]);
    assert_eq!(
        unseen(&shown(&[(1, 3), (6, 10)]), 12),
        vec![(4, 5), (11, 12)]
    );
    assert_eq!(unseen(&shown(&[(5, 8), (1, 6)]), 8), vec![]);
    assert_eq!(unseen(&[], 0), vec![]);
    assert_eq!(line_count("a\nb\n"), 2);
    assert_eq!(line_count("a\nb"), 2);
    assert_eq!(line_count(""), 0);
}

#[test]
fn partial_reads_add_up_to_full() {
    let mut t = Versions::default();
    t.record_read("/f", v("h1", 30), &[(1, 10)], 1, Some("text".into()));
    assert!(!t.get("/f").unwrap().full);
    assert!(t.get("/f").unwrap().text.is_none());
    t.record_read("/f", v("h1", 30), &[(11, 30)], 2, Some("text".into()));
    let s = t.get("/f").unwrap();
    assert!(s.full);
    assert_eq!(s.text.as_deref(), Some("text"));
    // a new version starts over
    t.record_read("/f", v("h2", 30), &[(1, 5)], 3, None);
    let s = t.get("/f").unwrap();
    assert!(!s.full && s.shown.len() == 1);
}

#[test]
fn unchanged_only_for_earlier_steps_and_same_version() {
    let mut t = Versions::default();
    t.record_read("/f", v("h1", 100), &[(1, 50)], 4, None);
    assert_eq!(t.unchanged_since("/f", "h1", 10, 20, 5), Some(4));
    assert_eq!(t.unchanged_since("/f", "h1", 10, 20, 4), None, "same step");
    assert_eq!(
        t.unchanged_since("/f", "h1", 40, 60, 5),
        None,
        "not all shown"
    );
    assert_eq!(t.unchanged_since("/f", "h2", 10, 20, 5), None, "changed");
    t.clear_shown();
    assert_eq!(t.unchanged_since("/f", "h1", 10, 20, 5), None, "compacted");
}

#[test]
fn saves_and_loads() {
    let dir = std::env::temp_dir().join(format!("shadow-unit-versions-{}", std::process::id()));
    let path = dir.join("versions.json");
    let _ = std::fs::remove_dir_all(&dir);
    let mut t = Versions::load(&path);
    t.record_written("/a", "h", Some("hello\n".into()));
    t.save().unwrap();
    let t2 = Versions::load(&path);
    let s = t2.get("/a").unwrap();
    assert!(s.full);
    assert_eq!(s.lines, 1);
    assert_eq!(s.text.as_deref(), Some("hello\n"));
}

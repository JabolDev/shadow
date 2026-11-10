//! Write and Edit. Both need the file's current version in the version
//! table; both write through a temp file and a rename.

use super::{Cx, Out, opt_bool, req_str};
use crate::session::random_id;
use crate::versions::{self, unseen};
use serde_json::Value;
use similar::{DiffTag, TextDiff};
use std::fs;
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

const MAX_DIFF_LINES: usize = 200;
const MAX_MATCH_LINES: usize = 20;
const CONTEXT: usize = 3;

pub fn write(cx: &Cx, input: &Value) -> Out {
    run_write(cx, input).unwrap_or_else(Out::error)
}

pub fn edit(cx: &Cx, input: &Value) -> Out {
    run_edit(cx, input).unwrap_or_else(Out::error)
}

fn run_write(cx: &Cx, input: &Value) -> Result<Out, String> {
    let path = cx.path(req_str(input, "file_path")?);
    let Some(Value::String(content)) = input.get("content") else {
        return Err("content is required (a string).".into());
    };
    let existed = match fs::metadata(&path) {
        Ok(m) if m.is_dir() => return Err(format!("{} is a folder.", path.display())),
        Ok(_) => {
            check_seen(
                cx,
                &path,
                &fs::read(&path).map_err(|e| format!("Cannot read {}: {e}", path.display()))?,
            )?;
            true
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => false,
        Err(e) => return Err(format!("Cannot read {}: {e}", path.display())),
    };
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)
            .map_err(|e| format!("Cannot create the folder {}: {e}", dir.display()))?;
    }
    write_atomic(&path, content.as_bytes())?;
    cx.turn.versions().record_written(
        &path.to_string_lossy(),
        &versions::hash(content.as_bytes()),
        Some(content.clone()),
    );
    Ok(Out::text(if existed {
        format!("The file {} has been updated.", path.display())
    } else {
        format!("File created successfully at: {}", path.display())
    }))
}

struct Change {
    old: String,
    new: String,
    all: bool,
}

fn changes(input: &Value) -> Result<Vec<Change>, String> {
    let one = |v: &Value, at: &str| -> Result<Change, String> {
        let s = |k: &str| match v.get(k) {
            Some(Value::String(s)) => Ok(s.clone()),
            _ => Err(format!("{at}{k} is required (a string).")),
        };
        Ok(Change {
            old: s("old_string")?,
            new: s("new_string")?,
            all: opt_bool(v, "replace_all")?,
        })
    };
    match input.get("edits") {
        None | Some(Value::Null) => Ok(vec![one(input, "")?]),
        Some(_) if input.get("old_string").is_some() => {
            Err("Give either old_string and new_string, or edits; not both.".into())
        }
        Some(Value::Array(list)) if !list.is_empty() => list
            .iter()
            .enumerate()
            .map(|(i, e)| one(e, &format!("edits[{i}].")))
            .collect(),
        Some(_) => {
            Err("edits must be a non-empty list of {old_string, new_string, replace_all?}.".into())
        }
    }
}

fn run_edit(cx: &Cx, input: &Value) -> Result<Out, String> {
    let path = cx.path(req_str(input, "file_path")?);
    let list = changes(input)?;
    let current = match fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(format!(
                "File does not exist: {}. Use Write to create it.",
                path.display()
            ));
        }
        Err(e) => return Err(format!("Cannot read {}: {e}", path.display())),
    };
    check_seen(cx, &path, &current)?;
    let src = String::from_utf8(current).map_err(|_| {
        format!(
            "{} is not UTF-8 text; Edit changes text files only.",
            path.display()
        )
    })?;
    // A file whose every line ends in CRLF is edited as LF and written back as CRLF.
    let crlf = src.contains("\r\n") && !src.replace("\r\n", "").contains('\n');
    let lf = |s: &str| {
        if crlf {
            s.replace("\r\n", "\n")
        } else {
            s.to_string()
        }
    };
    let before = lf(&src);
    let mut after = before.clone();
    let n = list.len();
    for (i, c) in list.iter().enumerate() {
        after = apply(&after, &lf(&c.old), &lf(&c.new), c.all, &path).map_err(|e| {
            if n > 1 {
                format!("Edit {} of {n} failed: {e} Nothing was written.", i + 1)
            } else {
                e
            }
        })?;
    }
    if before.ends_with('\n') && !after.ends_with('\n') {
        after.push('\n');
    }
    if !before.ends_with('\n') {
        while after.ends_with('\n') {
            after.pop();
        }
    }
    let written = if crlf {
        after.replace('\n', "\r\n")
    } else {
        after.clone()
    };
    write_atomic(&path, written.as_bytes())?;
    let hash = versions::hash(written.as_bytes());
    cx.turn
        .versions()
        .record_written(&path.to_string_lossy(), &hash, Some(written));
    let text = format!(
        "The file {} has been updated. Changed lines:\n{}",
        path.display(),
        changed_lines(&before, &after)
    );
    Ok(Out::bounded(
        cx.bound(&text)
            .map_err(|e| format!("Cannot write the overflow file: {e}"))?,
        false,
    ))
}

/// Ok when the file's current version is in the version table. Otherwise the
/// error to return: read it first, or what changed since it was seen.
fn check_seen(cx: &Cx, path: &Path, current: &[u8]) -> Result<(), String> {
    let key = path.to_string_lossy();
    let now = versions::hash(current);
    let mut table = cx.turn.versions();
    let shown = path.display();
    let Some(seen) = table.get(&key).cloned() else {
        return Err(format!(
            "{shown} has not been read yet. Read it first, then change it."
        ));
    };
    if seen.hash == now {
        if seen.full {
            return Ok(());
        }
        let gaps: Vec<String> = unseen(&seen.shown, seen.lines)
            .iter()
            .map(|(a, b)| {
                if a == b {
                    a.to_string()
                } else {
                    format!("{a}–{b}")
                }
            })
            .collect();
        return Err(format!(
            "{shown} has not been read in full (lines {} not yet). Read those lines first, then change it.",
            gaps.join(", ")
        ));
    }
    let (true, Some(old), Ok(new)) = (
        seen.full,
        seen.text.as_deref(),
        std::str::from_utf8(current),
    ) else {
        return Err(format!(
            "{shown} changed since you last read it. Read it again first, then change it."
        ));
    };
    let diff = TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(CONTEXT)
        .header("seen", "now")
        .to_string();
    let lines: Vec<&str> = diff.lines().collect();
    if lines.len() <= MAX_DIFF_LINES {
        table.record_written(&key, &now, Some(new.to_string()));
        return Err(format!(
            "{shown} changed since you last saw it. What changed (seen → now):\n{}\nThis version now counts as seen: make your change against it.",
            diff.trim_end()
        ));
    }
    Err(format!(
        "{shown} changed since you last saw it. The first {MAX_DIFF_LINES} of {} diff lines (seen → now):\n{}\nRead it again before changing it.",
        lines.len(),
        lines[..MAX_DIFF_LINES].join("\n")
    ))
}

fn apply(src: &str, old: &str, new: &str, all: bool, path: &Path) -> Result<String, String> {
    if old.is_empty() {
        return Err(
            "old_string is empty. To fill an empty file or replace all of it, use Write.".into(),
        );
    }
    if old == new {
        return Err("old_string and new_string are the same; nothing would change.".into());
    }
    let at: Vec<usize> = src.match_indices(old).map(|(i, _)| i).collect();
    match at.len() {
        0 => {
            let mut msg = format!("old_string was not found in {}.", path.display());
            if let Some((line, text)) = near_match(src, old) {
                msg.push_str(&format!(
                    " The nearest match starts at line {line}:\n{text}"
                ));
            }
            Err(msg)
        }
        1 => Ok(src.replacen(old, new, 1)),
        _ if all => Ok(src.replace(old, new)),
        n => {
            let mut lines = Vec::new();
            let (mut line, mut from) = (1, 0);
            for &i in at.iter().take(MAX_MATCH_LINES) {
                line += src[from..i].matches('\n').count();
                from = i;
                lines.push(line.to_string());
            }
            if n > MAX_MATCH_LINES {
                lines.push(format!("and {} more", n - MAX_MATCH_LINES));
            }
            Err(format!(
                "old_string appears {n} times in {} (lines {}). Add surrounding text to make it unique, or set replace_all.",
                path.display(),
                lines.join(", ")
            ))
        }
    }
}

/// The run of lines most like `old` (similarity ≥ 0.6), with its first line number.
fn near_match(src: &str, old: &str) -> Option<(usize, String)> {
    let lines: Vec<&str> = src.split('\n').collect();
    let target = old.trim_matches('\n');
    let k = target.split('\n').count();
    if k > lines.len() {
        return None;
    }
    let squash = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let target_squashed = squash(target);
    let deadline = Instant::now() + Duration::from_millis(500);
    let mut best: Option<(f32, usize)> = None;
    for start in 0..=lines.len() - k {
        let window = lines[start..start + k].join("\n");
        let score = if squash(&window) == target_squashed {
            1.0
        } else if window.len() > 2 * target.len() + 40 || target.len() > 2 * window.len() + 40 {
            continue;
        } else {
            TextDiff::configure()
                .deadline(deadline)
                .diff_chars(window.as_str(), target)
                .ratio()
        };
        if best.is_none_or(|(s, _)| score > s) {
            best = Some((score, start));
        }
        if score >= 1.0 || Instant::now() > deadline {
            break;
        }
    }
    let (score, start) = best?;
    (score >= 0.6).then(|| (start + 1, number(&lines[start..start + k], start + 1)))
}

fn number(lines: &[&str], first: usize) -> String {
    lines
        .iter()
        .enumerate()
        .map(|(i, l)| format!("{:>6}\t{l}", first + i))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The changed lines of `after` with their line numbers and 3 lines around;
/// separate places are split by a "…" line.
fn changed_lines(before: &str, after: &str) -> String {
    let lines: Vec<&str> = after.split_terminator('\n').collect();
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for op in TextDiff::from_lines(before, after).ops() {
        let (tag, _, new) = op.as_tag_tuple();
        if tag == DiffTag::Equal {
            continue;
        }
        let (s, e) = (
            new.start.saturating_sub(CONTEXT),
            (new.end + CONTEXT).min(lines.len()),
        );
        match ranges.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => ranges.push((s, e)),
        }
    }
    let parts: Vec<String> = ranges
        .iter()
        .filter(|(s, e)| s < e)
        .map(|&(s, e)| number(&lines[s..e], s + 1))
        .collect();
    if parts.is_empty() {
        "(the file is now empty)".into()
    } else {
        parts.join("\n     …\n")
    }
}

/// Writes through a temp file in the same folder and a rename, keeping the
/// mode of the file it replaces (a symlink's target is replaced).
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let target = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let (Some(dir), Some(name)) = (target.parent(), target.file_name()) else {
        return Err(format!("Cannot write {}: no folder", path.display()));
    };
    let tmp = dir.join(format!(".{}.{}.tmp", name.to_string_lossy(), random_id()));
    let mode = fs::metadata(&target).ok().map(|m| m.permissions());
    let res = (|| {
        fs::write(&tmp, bytes)?;
        if let Some(p) = mode {
            fs::set_permissions(&tmp, p)?;
        }
        fs::rename(&tmp, &target)
    })();
    res.map_err(|e| {
        if let Err(rm) = fs::remove_file(&tmp)
            && rm.kind() != io::ErrorKind::NotFound
        {
            eprintln!("shadow: cannot remove {}: {rm}", tmp.display());
        }
        format!("Cannot write {}: {e}", path.display())
    })
}

#[cfg(test)]
#[path = "edit_test.rs"]
mod tests;

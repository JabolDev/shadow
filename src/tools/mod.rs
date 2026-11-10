//! The seven native tools: their definitions, the dispatcher, and what every
//! tool shares — paths, arguments, and bounded results with overflow files.

mod bash;
mod edit;
mod read;
mod search;

pub use bash::{StopOutcome, kill_all, stop_task};

use crate::turn::Turn;
use serde_json::{Value, json};
use std::fs;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

/// The native tools, in the order they are offered.
pub const NATIVE: [&str; 7] = ["Read", "Write", "Edit", "Bash", "TaskStop", "Grep", "Glob"];

/// The seven definitions, as sent to the model.
pub fn definitions() -> Vec<Value> {
    let string = |d: &str| json!({ "type": "string", "description": d });
    let edit_pair = json!({
        "old_string": string("The exact text to replace"),
        "new_string": string("The text to put in its place"),
        "replace_all": { "type": "boolean", "description": "Replace every occurrence (default false)" },
    });
    vec![
        json!({
            "name": "Read",
            "description": "Read a file (absolute path, or relative to the working folder). Returns numbered lines, up to 2000 from offset; use offset and limit for other parts. PNG, JPEG, GIF and WebP files come back as images. Re-reading lines you already have from an unchanged file returns a one-line note instead.",
            "input_schema": schema(json!({
                "file_path": string("The file to read"),
                "offset": { "type": "integer", "description": "First line to read (1-based)" },
                "limit": { "type": "integer", "description": "Number of lines to read (max 2000)" },
            }), &["file_path"]),
        }),
        json!({
            "name": "Write",
            "description": "Write a file, creating parent folders. Overwriting an existing file needs its current version read first.",
            "input_schema": schema(json!({
                "file_path": string("The file to write"),
                "content": string("The full new content"),
            }), &["file_path", "content"]),
        }),
        json!({
            "name": "Edit",
            "description": "Replace exact text in a file you have read. Give old_string and new_string (old_string must be unique unless replace_all), or edits: a list of such replacements applied in order, all or nothing. Line endings and the final newline are kept. Returns the changed lines with their line numbers.",
            "input_schema": schema(json!({
                "file_path": string("The file to edit"),
                "old_string": edit_pair["old_string"],
                "new_string": edit_pair["new_string"],
                "replace_all": edit_pair["replace_all"],
                "edits": { "type": "array", "description": "Several replacements, applied in order", "items": schema(edit_pair.clone(), &["old_string", "new_string"]) },
            }), &["file_path"]),
        }),
        json!({
            "name": "Bash",
            "description": "Run a command with bash in the working folder. Waits up to timeout ms (default 120000, max 600000); a command still running then is not killed: it goes on as a background task and you get its output so far. run_in_background starts it as a background task at once. You are told when a background task ends; its full output is in a file. Long output is cut to its first and last part.",
            "input_schema": schema(json!({
                "command": string("The command"),
                "description": string("What the command does, in a few words"),
                "timeout": { "type": "integer", "description": "How long to wait in ms (max 600000)" },
                "run_in_background": { "type": "boolean", "description": "Start it as a background task" },
            }), &["command"]),
        }),
        json!({
            "name": "TaskStop",
            "description": "Stop a background task started by Bash (TERM, then KILL after 2 s). Returns its last lines of output.",
            "input_schema": schema(json!({ "task_id": string("The task id Bash gave") }), &["task_id"]),
        }),
        json!({
            "name": "Grep",
            "description": "Search file contents with a regular expression (ripgrep). Skips files ignored by .gitignore and binary files. output_mode: files_with_matches (default), content (matching lines; -n adds line numbers, -A/-B/-C add context lines) or count. glob filters file names. head_limit keeps the first N entries.",
            "input_schema": schema(json!({
                "pattern": string("The regular expression"),
                "path": string("File or folder to search (default: the working folder)"),
                "glob": string("Glob filter for file names, e.g. \"*.js\""),
                "output_mode": { "type": "string", "enum": ["content", "files_with_matches", "count"] },
                "-i": { "type": "boolean", "description": "Case insensitive" },
                "-n": { "type": "boolean", "description": "Line numbers (content mode)" },
                "-A": { "type": "integer", "description": "Lines after each match" },
                "-B": { "type": "integer", "description": "Lines before each match" },
                "-C": { "type": "integer", "description": "Lines before and after each match" },
                "head_limit": { "type": "integer", "description": "Keep the first N entries" },
                "multiline": { "type": "boolean", "description": "Let . and the pattern span lines" },
            }), &["pattern"]),
        }),
        json!({
            "name": "Glob",
            "description": "Find files by glob pattern (e.g. \"**/*.js\", \"src/**/*.{ts,tsx}\"), newest first, at most 100.",
            "input_schema": schema(json!({
                "pattern": string("The glob pattern"),
                "path": string("The folder to search in (default: the working folder)"),
            }), &["pattern"]),
        }),
    ]
}

fn schema(properties: Value, required: &[&str]) -> Value {
    json!({ "type": "object", "properties": properties, "required": required, "additionalProperties": false })
}

/// A native tool's result.
#[derive(Default)]
pub struct Out {
    pub content: Vec<Value>,
    pub is_error: bool,
    pub exit_code: Option<i32>,
    pub task_id: Option<String>,
    pub overflow: Option<PathBuf>,
}

impl Out {
    pub fn text(t: impl Into<String>) -> Out {
        Out {
            content: vec![json!({ "type": "text", "text": t.into() })],
            ..Out::default()
        }
    }

    pub fn error(t: impl Into<String>) -> Out {
        Out {
            is_error: true,
            ..Out::text(t)
        }
    }

    /// A text result cut to the cap (see `Bounded`).
    pub fn bounded(b: Bounded, is_error: bool) -> Out {
        Out {
            is_error,
            overflow: b.overflow,
            ..Out::text(b.text)
        }
    }

    pub fn meta_json(&self, duration_ms: u64) -> Value {
        let mut m = json!({ "duration_ms": duration_ms });
        if let Some(c) = self.exit_code {
            m["exit_code"] = c.into();
        }
        if let Some(t) = &self.task_id {
            m["task_id"] = t.clone().into();
        }
        if let Some(p) = &self.overflow {
            m["truncated"] = true.into();
            m["overflow_path"] = p.to_string_lossy().into();
        }
        m
    }
}

/// What a tool sees of its turn.
pub struct Cx {
    pub turn: Arc<Turn>,
    pub call_id: String,
    /// The chat's step: assistant messages in the context so far.
    pub step: u64,
}

impl Cx {
    pub fn path(&self, p: &str) -> PathBuf {
        resolve(&self.turn.cwd, p)
    }

    pub fn cap(&self) -> usize {
        self.turn.limits.result_cap_chars
    }

    /// `<scratch>/<call id>.txt`
    pub fn overflow_path(&self) -> PathBuf {
        let id: String = self
            .call_id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        self.turn.session.scratch().join(format!("{id}.txt"))
    }

    pub fn bound(&self, text: &str) -> io::Result<Bounded> {
        bound(text, self.cap(), &self.overflow_path())
    }
}

pub async fn run(turn: &Arc<Turn>, call_id: &str, name: &str, input: &Value) -> Out {
    let cx = Cx {
        turn: turn.clone(),
        call_id: call_id.to_string(),
        step: turn.chat_step.load(std::sync::atomic::Ordering::Relaxed),
    };
    if !input.is_object() {
        return Out::error("The tool input must be a JSON object.");
    }
    match name {
        "Bash" => return bash::bash(&cx, input).await,
        "TaskStop" => return bash::task_stop(&cx, input).await,
        _ => {}
    }
    let (name, input) = (name.to_string(), input.clone());
    let run = tokio::task::spawn_blocking(move || match name.as_str() {
        "Read" => read::read(&cx, &input),
        "Write" => edit::write(&cx, &input),
        "Edit" => edit::edit(&cx, &input),
        "Grep" => search::grep(&cx, &input),
        "Glob" => search::glob(&cx, &input),
        other => Out::error(format!("No such tool: {other}")),
    });
    run.await
        .unwrap_or_else(|e| Out::error(format!("shadow: internal error: {e}")))
}

/// A string argument: Ok(None) when absent, Err when not a string.
pub fn opt_str<'a>(input: &'a Value, key: &str) -> Result<Option<&'a str>, String> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(format!("{key} must be a string.")),
    }
}

pub fn req_str<'a>(input: &'a Value, key: &str) -> Result<&'a str, String> {
    opt_str(input, key)?
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("{key} is required."))
}

/// A whole-number argument (a float is cut to its whole part).
pub fn opt_num(input: &Value, key: &str) -> Result<Option<i64>, String> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => Ok(n.as_i64().or_else(|| n.as_f64().map(|f| f as i64))),
        Some(_) => Err(format!("{key} must be a number.")),
    }
}

pub fn opt_bool(input: &Value, key: &str) -> Result<bool, String> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(format!("{key} must be true or false.")),
    }
}

/// An absolute path, or one relative to `cwd`, with "." and ".." resolved.
pub fn resolve(cwd: &Path, p: &str) -> PathBuf {
    let joined = if Path::new(p).is_absolute() {
        PathBuf::from(p)
    } else {
        cwd.join(p)
    };
    let mut out = PathBuf::new();
    for c in joined.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// A text result within the cap: either the whole text, or its first and
/// last lines around `[… N lines omitted. Full output: <path>]`, with the
/// whole text written to the overflow file.
pub struct Bounded {
    pub text: String,
    /// Whole lines kept at the start and at the end (all lines when not cut).
    pub head: usize,
    pub tail: usize,
    pub overflow: Option<PathBuf>,
}

pub fn bound(text: &str, cap: usize, overflow: &Path) -> io::Result<Bounded> {
    let lines: Vec<&str> = text.split('\n').collect();
    if text.len() <= cap || text.chars().count() <= cap {
        return Ok(Bounded {
            text: text.to_string(),
            head: lines.len(),
            tail: 0,
            overflow: None,
        });
    }
    write_overflow(overflow, |f| io::Write::write_all(f, text.as_bytes()))?;
    Ok(cut(&lines, &lines, lines.len(), cap, overflow))
}

/// `bound` for a file that may be large (a command's output): only its first
/// and last parts are read; the overflow file is a copy of it.
pub fn bound_file(path: &Path, cap: usize, overflow: &Path) -> io::Result<Bounded> {
    let size = fs::metadata(path)?.len();
    if size <= (4 * cap as u64).max(1 << 20) {
        let bytes = fs::read(path)?;
        return bound(&String::from_utf8_lossy(&bytes), cap, overflow);
    }
    let mut f = fs::File::open(path)?;
    let part = 2 * cap as u64;
    let mut head = vec![0; part as usize];
    f.read_exact(&mut head)?;
    f.seek(SeekFrom::Start(size - part))?;
    let mut tail = vec![0; part as usize];
    f.read_exact(&mut tail)?;
    f.seek(SeekFrom::Start(0))?;
    let mut total = 0;
    let mut buf = vec![0; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        total += buf[..n].iter().filter(|&&b| b == b'\n').count();
    }
    let total = total + usize::from(!tail.ends_with(b"\n"));
    write_overflow(overflow, |out| {
        io::copy(&mut fs::File::open(path)?, out).map(drop)
    })?;
    let head = String::from_utf8_lossy(&head);
    let tail = String::from_utf8_lossy(&tail);
    let mut head_lines: Vec<&str> = head.split('\n').collect();
    head_lines.pop();
    let mut tail_lines: Vec<&str> = tail.split('\n').skip(1).collect();
    if tail.ends_with('\n') {
        tail_lines.pop();
    }
    Ok(cut(&head_lines, &tail_lines, total, cap, overflow))
}

fn write_overflow(
    path: &Path,
    fill: impl FnOnce(&mut fs::File) -> io::Result<()>,
) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fill(&mut fs::File::create(path)?)
}

/// Keeps whole lines from the start of `head` and the end of `tail`, each
/// side within half of what the cap leaves beside the marker line; a first
/// or last line longer than that is cut.
fn cut(head: &[&str], tail: &[&str], total: usize, cap: usize, overflow: &Path) -> Bounded {
    let marker_max = format!(
        "[… {total} lines omitted. Full output: {}]",
        overflow.display()
    )
    .chars()
    .count()
        + 4;
    let half = (cap.saturating_sub(marker_max) / 2).max(1);
    let (mut used, mut h) = (0, 0);
    while h < head.len() && used + head[h].chars().count() < half {
        used += head[h].chars().count() + 1;
        h += 1;
    }
    let same = std::ptr::eq(head.as_ptr(), tail.as_ptr());
    let floor = if same { h } else { 0 };
    let (mut used, mut t) = (0, 0);
    while tail.len() - t > floor && used + tail[tail.len() - 1 - t].chars().count() < half {
        used += tail[tail.len() - 1 - t].chars().count() + 1;
        t += 1;
    }
    let mut text = head[..h].join("\n");
    if h == 0 && !head.is_empty() {
        text = head[0].chars().take(half).collect::<String>() + "…";
    }
    let omitted = total.saturating_sub(h + t);
    text.push_str(&format!(
        "\n[… {omitted} lines omitted. Full output: {}]\n",
        overflow.display()
    ));
    if t == 0 && !tail.is_empty() {
        let last = tail[tail.len() - 1];
        let n = last.chars().count();
        text.push('…');
        text.extend(last.chars().skip(n.saturating_sub(half)));
    } else {
        text.push_str(&tail[tail.len() - t..].join("\n"));
    }
    Bounded {
        text,
        head: h,
        tail: t,
        overflow: Some(overflow.to_path_buf()),
    }
}

#[cfg(test)]
#[path = "mod_test.rs"]
mod tests;

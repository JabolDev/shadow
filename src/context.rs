//! The context engine: what keeps a long chat small and its prompt cache
//! warm. The fade (old bulky tool results become one-line stubs, in one
//! batch), the ledger a compaction keeps, the state delta at a turn's start,
//! the gauge, and the loop breaker.

use crate::session::{Session, TaskState};
use crate::tools::{self, UNCHANGED_SINCE, resolve};
use crate::turn::Call;
use crate::wire::{self, Wire};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Results of the last 10 steps are never faded.
const RECENT_STEPS: u64 = 10;
/// Results shorter than this are never faded.
const MIN_FADE_CHARS: usize = 2000;
const GAUGE: &str = "\"text\":\"Context: ";
const MINUTE: u64 = 60_000;

/// What a turn's context engine works with.
#[derive(Clone, Default)]
pub struct Settings {
    /// `limits.context_tokens`: the size of the model's window. Without it
    /// there is no threshold fade, no gauge and no auto-compaction.
    pub window: Option<u64>,
    pub auto_compact: bool,
    /// The prompt cache's lifetime in ms; None when the cache is off.
    pub cache_ms: Option<u64>,
    /// Host tools flagged `keep_latest`.
    pub keep: Vec<String>,
}

impl Settings {
    /// The cache has expired: more than its lifetime has passed since the
    /// chat's last model call (always, with the cache off).
    pub fn cold(&self, last_call: Option<u64>, now: u64) -> bool {
        self.cache_ms
            .is_none_or(|ms| last_call.is_some_and(|at| now.saturating_sub(at) > ms))
    }

    /// `pct` percent of the window, in tokens.
    pub fn share(&self, pct: u64) -> Option<u64> {
        self.window.map(|w| w * pct / 100)
    }
}

/// A tool result in the context, with its call.
struct Item {
    id: String,
    name: String,
    input: Value,
    /// The chat step of its call (its assistant message's number).
    step: u64,
    is_error: bool,
    chars: usize,
    lines: usize,
    image: bool,
    /// A Read whose result shows the file: the file's path.
    view: Option<String>,
    content: Value,
}

fn items(messages: &[String], cwd: &Path) -> Vec<Item> {
    let mut calls: HashMap<String, (String, Value, u64)> = HashMap::new();
    let (mut step, mut out) = (0, Vec::new());
    for m in messages {
        let Ok(v) = serde_json::from_str::<Value>(m) else {
            continue;
        };
        if v["role"] == "assistant" {
            step += 1;
            for b in wire::tool_uses(&v) {
                let (id, name) = (b["id"].as_str(), b["name"].as_str());
                let (id, name) = (id.unwrap_or_default(), name.unwrap_or_default());
                calls.insert(id.into(), (name.into(), b["input"].clone(), step));
            }
            continue;
        }
        let blocks = v["content"].as_array().into_iter().flatten();
        for b in blocks.filter(|b| b["type"] == "tool_result") {
            let id = b["tool_use_id"].as_str().unwrap_or_default();
            let Some((name, input, step)) = calls.remove(id) else {
                continue;
            };
            let (chars, lines, image) = measure(&b["content"]);
            let is_error = b["is_error"] == true;
            let view = (name == "Read"
                && !is_error
                && !text_of(&b["content"]).starts_with(UNCHANGED_SINCE))
            .then(|| {
                let path = input["file_path"].as_str().unwrap_or_default();
                resolve(cwd, path).to_string_lossy().into_owned()
            });
            out.push(Item {
                id: id.into(),
                name,
                input,
                step,
                is_error,
                chars,
                lines,
                image,
                view,
                content: b["content"].clone(),
            });
        }
    }
    out
}

/// Characters and lines of a result's content; images count as their data.
fn measure(content: &Value) -> (usize, usize, bool) {
    let (mut chars, mut lines, mut image) = (0, 0, false);
    let one = [content.clone()];
    let blocks = match content {
        Value::Array(a) => a.as_slice(),
        _ => &one,
    };
    for b in blocks {
        match (b.as_str(), b["text"].as_str()) {
            (Some(t), _) | (None, Some(t)) => {
                chars += t.chars().count();
                lines += t.lines().count();
            }
            _ => {
                image |= b["type"] == "image";
                chars += b.to_string().chars().count();
            }
        }
    }
    (chars, lines, image)
}

/// The text blocks of a result's content (other blocks as their JSON).
fn text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .map(|b| {
                b["text"]
                    .as_str()
                    .map_or_else(|| b.to_string(), str::to_string)
            })
            .collect::<Vec<_>>()
            .join("\n"),
        other => other.to_string(),
    }
}

/// A fade: the stub of each faded call, the Reads (path, step) whose lines
/// are no longer above, and the context's size after it.
pub struct Fade {
    pub stubs: Map<String, Value>,
    pub reads: Vec<(String, u64)>,
    pub size: u64,
}

/// Plans a fade of the context (its size plus `extra` tokens not logged
/// yet): candidates in order — superseded results first, then the rest,
/// oldest first, larger before smaller — until the size is at `down_to`, or
/// every one.
pub fn fade(
    wire: &Wire,
    extra: u64,
    cwd: &Path,
    scratch: &Path,
    keep: &[String],
    down_to: Option<u64>,
) -> Fade {
    let items = items(&wire.messages, cwd);
    let mut cands: Vec<(bool, &Item)> = Vec::new();
    for (i, it) in items.iter().enumerate() {
        let later = &items[i + 1..];
        let newest_view = it
            .view
            .as_ref()
            .is_some_and(|p| !later.iter().any(|l| l.view.as_ref() == Some(p)));
        let newest_kept = keep.contains(&it.name) && !later.iter().any(|l| l.name == it.name);
        if it.step + RECENT_STEPS > wire.assistant_count
            || it.chars < MIN_FADE_CHARS
            || wire.faded.contains(&it.id)
            || newest_view
            || newest_kept
        {
            continue;
        }
        let superseded = it.view.is_some()
            || later
                .iter()
                .any(|l| l.name == it.name && l.input == it.input);
        cands.push((superseded, it));
    }
    cands.sort_by(|a, b| {
        (b.0.cmp(&a.0))
            .then(a.1.step.cmp(&b.1.step))
            .then(b.1.chars.cmp(&a.1.chars))
    });
    let mut f = Fade {
        stubs: Map::new(),
        reads: Vec::new(),
        size: wire.size() + extra,
    };
    for (_, it) in cands {
        if down_to.is_some_and(|t| f.size <= t) {
            break;
        }
        let stub = stub(it, scratch);
        let saved = it.chars.saturating_sub(stub.chars().count()) as u64 / 4;
        f.size = f.size.saturating_sub(saved);
        if let Some(p) = &it.view {
            f.reads.push((p.clone(), it.step));
        }
        f.stubs.insert(it.id.clone(), stub.into());
    }
    f
}

/// "Read src/a.rs", "Bash make test", or a host tool with its input.
fn call_label(name: &str, input: &Value) -> String {
    if tools::NATIVE.contains(&name) {
        return crate::cli::label(name, input);
    }
    let all = format!("{name} {input}");
    match all.char_indices().nth(100) {
        Some((cut, _)) => format!("{}…", &all[..cut]),
        None => all,
    }
}

/// The one line a faded result is sent as.
fn stub(it: &Item, scratch: &Path) -> String {
    let what = if it.image {
        "an image".to_string()
    } else {
        format!("{} lines ({} characters)", it.lines, it.chars)
    };
    let file = tools::overflow_file(scratch, &it.id);
    let full = match file.is_file() {
        true => format!(" Full output: {}.", file.display()),
        false => String::new(),
    };
    format!(
        "[Faded to save context: {} returned {what} at step {}.{full} Run it again if you need it.]",
        call_label(&it.name, &it.input),
        it.step
    )
}

/// The ledger of a compaction of `messages`, and its text for the summary
/// message: every file changed with Write or Edit (with the earlier
/// ledger's), the background commands still running, and the newest call of
/// each `keep_latest` host tool, word for word within `cap`. None when it is
/// empty.
pub fn ledger(
    messages: &[String],
    earlier: Option<&Value>,
    session: &Session,
    keep: &[String],
    cap: usize,
) -> Option<(Value, String)> {
    let items = items(messages, Path::new("/"));
    let mut files: Vec<String> = earlier
        .and_then(|l| l["files"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|f| f.as_str().map(str::to_string))
        .collect();
    for it in items
        .iter()
        .filter(|i| matches!(i.name.as_str(), "Write" | "Edit") && !i.is_error)
    {
        let path = it.input["file_path"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        if !path.is_empty() && !files.contains(&path) {
            files.push(path);
        }
    }
    let mut latest: Vec<Value> = Vec::new();
    for name in keep {
        let newest = items.iter().rev().find(|i| &i.name == name).map(|it| {
            let text = text_of(&it.content);
            let file = tools::overflow_file(&session.scratch(), &it.id);
            let text = tools::bound(&text, cap, &file)
                .map_or_else(|_| text.chars().take(cap).collect(), |b| b.text);
            json!({ "name": name, "input": it.input, "result": text })
        });
        let older = || {
            earlier.and_then(|l| {
                l["latest"]
                    .as_array()?
                    .iter()
                    .find(|e| e["name"] == *name)
                    .cloned()
            })
        };
        latest.extend(newest.or_else(older));
    }
    let running: Vec<String> = session
        .lock()
        .tasks
        .iter()
        .filter(|k| k.background && k.state == TaskState::Running)
        .map(|k| format!("- {}: {} (output: {})", k.id, k.command, k.output.display()))
        .collect();
    if files.is_empty() && latest.is_empty() && running.is_empty() {
        return None;
    }
    let mut text = String::from("Kept word for word at this compaction (not summarized):");
    if !files.is_empty() {
        text.push_str("\n\nFiles created or changed in this chat:\n- ");
        text.push_str(&files.join("\n- "));
    }
    if !running.is_empty() {
        text.push_str("\n\nBackground commands still running:\n");
        text.push_str(&running.join("\n"));
    }
    for l in &latest {
        let name = l["name"].as_str().unwrap_or_default();
        let result = l["result"].as_str().unwrap_or_default();
        text.push_str(&format!(
            "\n\nThe newest call of {name}:\nInput: {}\nResult:\n{result}",
            l["input"]
        ));
    }
    Some((json!({ "files": files, "latest": latest }), text))
}

/// The state delta at a turn's start: files changed by someone else,
/// background commands that ended and were not announced, and the time
/// since the last model call when over 30 minutes. None when nothing applies.
pub fn delta(
    mut lines: Vec<String>,
    session: &Session,
    last_call: Option<u64>,
    now: u64,
) -> Option<Value> {
    for k in session.lock().tasks.iter_mut() {
        if k.background && k.state != TaskState::Running && !k.announced && !k.stopped_by_tool {
            k.announced = true;
            let first: String = k
                .command
                .lines()
                .next()
                .unwrap_or_default()
                .chars()
                .take(100)
                .collect();
            let more = if first.len() < k.command.len() {
                "…"
            } else {
                ""
            };
            lines.push(format!(
                "Background task {} ({first}{more}) ended: exit {}. Output: {}",
                k.id,
                k.exit_code.unwrap_or(-1),
                k.output.display()
            ));
        }
    }
    let idle = last_call.map_or(0, |at| now.saturating_sub(at));
    if idle > 30 * MINUTE {
        let m = idle / MINUTE;
        let ago = match m {
            0..120 => format!("{m} minutes"),
            120..2880 => format!("{} hours", m / 60),
            _ => format!("{} days", m / 1440),
        };
        lines.push(format!("{ago} have passed since the last model call."));
    }
    (!lines.is_empty()).then(|| {
        let text = format!(
            "What changed since the last model call:\n- {}",
            lines.join("\n- ")
        );
        json!({ "type": "text", "text": text })
    })
}

/// The gauge line, when the context (`size` of `window` tokens) has crossed
/// 50, 75 or 90% for the first time since the last compaction (the earlier
/// gauge lines are in `messages`).
pub fn gauge(messages: &[String], size: u64, window: u64) -> Option<String> {
    let level = |pct: u64| [90, 75, 50].into_iter().find(|l| pct >= *l).unwrap_or(0);
    let shown = messages
        .iter()
        .flat_map(|m| m.split(GAUGE).skip(1))
        .filter_map(|rest| {
            let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
            let pct = rest[..digits].parse::<u64>().ok()?;
            rest[digits..].starts_with("% used (").then_some(level(pct))
        })
        .max()
        .unwrap_or(0);
    let pct = size * 100 / window.max(1);
    let k = |n: u64| match n {
        0..1000 => n.to_string(),
        _ => format!("{}k", (n + 500) / 1000),
    };
    (level(pct) > shown).then(|| {
        format!(
            "Context: {pct}% used ({} of {} tokens).",
            k(size),
            k(window)
        )
    })
}

/// The loop breaker's memory: the calls of the last two steps.
#[derive(Default)]
pub struct Repeats {
    steps: Vec<Vec<Seen>>,
}

struct Seen {
    name: String,
    input: Value,
    /// The result as the tool returned it (content and is_error).
    result: Value,
    /// The chat step and the call whose result holds this text in full.
    full: (u64, String),
}

impl Repeats {
    /// A call with the same tool and input as a call in each of the two steps
    /// before, and the same result as theirs, is sent one line instead of
    /// its result (the tool did run). `step` is the chat step of `calls`.
    pub fn apply(
        &mut self,
        step: u64,
        calls: &[Call],
        results: &mut [Value],
        faded: &HashSet<String>,
    ) {
        let mut now = Vec::with_capacity(calls.len());
        for (c, r) in calls.iter().zip(results.iter_mut()) {
            let result = json!([r.get("content"), r.get("is_error")]);
            let same = |s: &Vec<Seen>| {
                s.iter()
                    .find(|p| p.name == c.name && p.input == c.input && p.result == result)
                    .map(|p| p.full.clone())
            };
            let full = match self.steps.as_slice() {
                [two, one] => same(two).and(same(one)).filter(|f| !faded.contains(&f.1)),
                _ => None,
            };
            let full = match full {
                Some((at, id)) => {
                    r["content"] = json!([{ "type": "text", "text": format!("Unchanged. You already have this result (step {at}).") }]);
                    (at, id)
                }
                None => (step, c.id.clone()),
            };
            now.push(Seen {
                name: c.name.clone(),
                input: c.input.clone(),
                result,
                full,
            });
        }
        self.steps.push(now);
        if self.steps.len() > 2 {
            self.steps.remove(0);
        }
    }
}

#[cfg(test)]
#[path = "context_test.rs"]
mod tests;

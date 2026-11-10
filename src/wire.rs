//! The wire log: `<data_dir>/sessions/<session>/wire.jsonl`, append-only.
//! One JSON object per line:
//!
//!   {"kind":"message","message":{…}}     a message exactly as sent / received
//!   {"kind":"compaction","message":{…},"ledger"?:{…}}  the summary message; the context restarts from it
//!   {"kind":"cursor","cursor":"…"}       the host's cursor for a turn
//!   {"kind":"model_call","at":<ms>,"tokens"?:n}  a completed model call and the context size it reported
//!   {"kind":"fade","stubs":{"<call id>":"<stub>",…}}  those tool results are sent as their stub from here on
//!
//! Messages are kept as their JSON text, so a request rebuilt from the log
//! repeats earlier requests byte for byte. A fade rewrites the messages it
//! names in the same way at every load, so the rebuilt context is the same
//! bytes after a restart.

use serde::Deserialize;
use serde_json::value::RawValue;
use serde_json::{Map, Value, json};
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

pub const INTERRUPTED: &str = "Interrupted: the harness restarted.";

pub struct Wire {
    path: PathBuf,
    /// The context: messages since the last compaction, as JSON text.
    pub messages: Vec<String>,
    pub cursor: Option<String>,
    pub last_model_call_at: Option<u64>,
    pub exists: bool,
    /// Assistant messages in the context (the chat's step count).
    pub assistant_count: u64,
    /// Calls whose results are faded (since the last compaction).
    pub faded: HashSet<String>,
    /// The ledger of the last compaction.
    pub ledger: Option<Value>,
    /// Characters of the context's messages.
    chars: usize,
    /// Tokens the last model call reported, and `chars` at that moment.
    report: Option<(u64, usize)>,
    /// Bytes of complete lines in the file; a torn last line lies past it.
    good_len: u64,
    /// Lines not yet written (repairs from loading, new entries).
    pending: Vec<u8>,
    file: Option<File>,
}

#[derive(Deserialize)]
struct Entry<'a> {
    kind: &'a str,
    #[serde(borrow)]
    message: Option<&'a RawValue>,
    cursor: Option<String>,
    at: Option<u64>,
    tokens: Option<u64>,
    stubs: Option<Map<String, Value>>,
    ledger: Option<Value>,
}

#[derive(Deserialize)]
struct Head {
    role: String,
}

impl Wire {
    /// Reads the log. A torn last line (crash mid-write) is left out and cut
    /// off at the next write; an assistant message whose tool calls have no
    /// results gets "Interrupted" results, written with the next entries.
    pub fn load(path: &Path) -> io::Result<Wire> {
        let bytes = match std::fs::read(path) {
            Ok(b) => Some(b),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        };
        let mut w = Wire {
            path: path.to_path_buf(),
            messages: Vec::new(),
            cursor: None,
            last_model_call_at: None,
            exists: bytes.is_some(),
            assistant_count: 0,
            faded: HashSet::new(),
            ledger: None,
            chars: 0,
            report: None,
            good_len: 0,
            pending: Vec::new(),
            file: None,
        };
        let bytes = bytes.unwrap_or_default();
        let mut start = 0;
        let mut line_no = 0;
        while let Some(nl) = bytes[start..].iter().position(|&b| b == b'\n') {
            line_no += 1;
            let line = &bytes[start..start + nl];
            start += nl + 1;
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let e: Entry = serde_json::from_slice(line).map_err(|err| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{} line {line_no} is not valid: {err}", path.display()),
                )
            })?;
            match (e.kind, e.message) {
                ("message", Some(m)) => w.add(m.get().to_string()),
                ("compaction", Some(m)) => w.restart(m.get().to_string(), e.ledger),
                ("cursor", _) => w.cursor = e.cursor,
                ("model_call", _) => {
                    w.last_model_call_at = e.at.or(w.last_model_call_at);
                    w.report = e.tokens.map(|t| (t, w.chars));
                }
                ("fade", _) => w.fade(&e.stubs.unwrap_or_default()),
                (kind, _) => eprintln!(
                    "shadow: {} line {line_no}: unknown kind {kind:?}",
                    path.display()
                ),
            }
        }
        w.good_len = start as u64;
        if let Some(ids) = w.messages.last().and_then(|m| unanswered_calls(m)) {
            let content: Vec<Value> = ids.iter().map(|id| error_result(id, INTERRUPTED)).collect();
            w.push_message(&json!({ "role": "user", "content": content }));
        }
        Ok(w)
    }

    fn add(&mut self, text: String) {
        if serde_json::from_str::<Head>(&text).is_ok_and(|h| h.role == "assistant") {
            self.assistant_count += 1;
        }
        self.chars += text.chars().count();
        self.messages.push(text);
    }

    /// A compaction: the context restarts from the summary message.
    fn restart(&mut self, text: String, ledger: Option<Value>) {
        self.messages.clear();
        self.assistant_count = 0;
        self.faded.clear();
        self.chars = 0;
        self.report = None;
        self.ledger = ledger;
        self.add(text);
    }

    /// Sends the named tool results as their stubs from now on.
    fn fade(&mut self, stubs: &Map<String, Value>) {
        for m in self.messages.iter_mut() {
            if !m.contains("\"tool_result\"") {
                continue;
            }
            let Ok(mut v) = serde_json::from_str::<Value>(m) else {
                continue;
            };
            let mut changed = false;
            for b in v
                .get_mut("content")
                .and_then(Value::as_array_mut)
                .into_iter()
                .flatten()
            {
                let stub = b
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .and_then(|id| stubs.get(id));
                if let (Some(stub), Some("tool_result")) =
                    (stub.cloned(), b.get("type").and_then(Value::as_str))
                {
                    b["content"] = json!([{ "type": "text", "text": stub }]);
                    changed = true;
                }
            }
            if changed {
                let text = v.to_string();
                self.chars = self.chars + text.chars().count() - m.chars().count();
                *m = text;
            }
        }
        self.faded.extend(stubs.keys().cloned());
    }

    /// The context's size in tokens: what the last model call reported, plus
    /// (or minus) the characters changed since then at 4 per token.
    pub fn size(&self) -> u64 {
        match self.report {
            Some((tokens, at)) => {
                (tokens as i64 + (self.chars as i64 - at as i64) / 4).max(0) as u64
            }
            None => (self.chars / 4) as u64,
        }
    }

    fn line(&mut self, line: String) {
        self.pending.extend_from_slice(line.as_bytes());
        self.pending.push(b'\n');
    }

    pub fn push_message(&mut self, msg: &Value) {
        let text = msg.to_string();
        self.line(format!(r#"{{"kind":"message","message":{text}}}"#));
        self.add(text);
    }

    pub fn push_compaction(&mut self, msg: &Value, ledger: Option<Value>) {
        let text = msg.to_string();
        match &ledger {
            Some(l) => self.line(format!(
                r#"{{"kind":"compaction","message":{text},"ledger":{l}}}"#
            )),
            None => self.line(format!(r#"{{"kind":"compaction","message":{text}}}"#)),
        }
        self.restart(text, ledger);
    }

    /// One fade marker: `stubs` maps call ids to the line sent instead.
    pub fn push_fade(&mut self, stubs: Map<String, Value>) {
        self.line(json!({ "kind": "fade", "stubs": stubs }).to_string());
        self.fade(&stubs);
    }

    pub fn push_cursor(&mut self, cursor: &str) {
        self.line(json!({ "kind": "cursor", "cursor": cursor }).to_string());
        self.cursor = Some(cursor.to_string());
    }

    /// A model call that reported `tokens` for the context up to its message.
    pub fn push_model_call(&mut self, at: u64, tokens: Option<u64>) {
        let mut line = json!({ "kind": "model_call", "at": at });
        if let Some(t) = tokens {
            line["tokens"] = t.into();
        }
        self.line(line.to_string());
        self.last_model_call_at = Some(at);
        self.report = tokens.map(|t| (t, self.chars));
    }

    /// Writes the pending lines and syncs them to disk.
    pub fn commit(&mut self) -> io::Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        if self.file.is_none() {
            if let Some(dir) = self.path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let f = OpenOptions::new()
                .create(true)
                .read(true)
                .append(true)
                .open(&self.path)?;
            if f.metadata()?.len() > self.good_len {
                f.set_len(self.good_len)?;
            }
            self.file = Some(f);
        }
        let f = self.file.as_mut().expect("opened above");
        f.write_all(&self.pending)?;
        f.sync_data()?;
        self.good_len += self.pending.len() as u64;
        self.pending.clear();
        self.exists = true;
        Ok(())
    }
}

/// The tool_use ids of an assistant message (JSON text) — None when it is
/// not an assistant message or has no tool calls.
pub fn unanswered_calls(msg: &str) -> Option<Vec<String>> {
    let v: Value = serde_json::from_str(msg).ok()?;
    if v.get("role")? != "assistant" {
        return None;
    }
    let ids: Vec<String> = tool_uses(&v)
        .map(|b| b["id"].as_str().unwrap_or_default().to_string())
        .collect();
    (!ids.is_empty()).then_some(ids)
}

/// The tool_use blocks of a message.
pub fn tool_uses(msg: &Value) -> impl Iterator<Item = &Value> {
    msg.get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
}

pub fn error_result(id: &str, text: &str) -> Value {
    json!({ "type": "tool_result", "tool_use_id": id, "content": [{ "type": "text", "text": text }], "is_error": true })
}

#[cfg(test)]
#[path = "wire_test.rs"]
mod tests;

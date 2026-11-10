//! Shared helpers for the integration tests: a daemon behind a scripted
//! host, a fake model, and scratch folders.
#![allow(dead_code)]

use serde_json::{Value, json};
use shadow_testkit::fake::FakeModel;
use shadow_testkit::host::ScriptedHost;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

pub const LONG: Duration = Duration::from_secs(30);

/// A fresh, empty folder under the system temp folder.
pub fn tmpdir(name: &str) -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "shadow-test-{name}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

pub struct Env {
    pub host: ScriptedHost,
    pub fake: FakeModel,
    pub data: PathBuf,
    pub work: PathBuf,
}

pub fn shadow_bin() -> String {
    env!("CARGO_BIN_EXE_shadow").to_string()
}

impl Env {
    /// A daemon (initialized) and a fake model with `script`.
    pub fn new(name: &str, script: Vec<Value>, rules: Vec<Value>) -> Env {
        let fake = FakeModel::start(script, 0, None).unwrap();
        let data = tmpdir(&format!("{name}-data"));
        let work = tmpdir(&format!("{name}-work"));
        let mut host = ScriptedHost::start(&[shadow_bin(), "serve".into()], rules, false).unwrap();
        let r = host.call("initialize", json!({ "protocol": 1, "data_dir": data, "client": { "name": "test", "version": "1" } }));
        assert_eq!(r["result"]["protocol"], 1, "{r}");
        Env {
            host,
            fake,
            data,
            work,
        }
    }

    pub fn restart(&mut self) {
        self.host.restart().unwrap();
        let r = self.host.call("initialize", json!({ "protocol": 1, "data_dir": self.data, "client": { "name": "test", "version": "1" } }));
        assert_eq!(r["result"]["protocol"], 1, "{r}");
    }

    /// turn.start params for `session` with a text input; `extra` overrides fields.
    pub fn params(&self, session: &str, text: &str, extra: Value) -> Value {
        let mut p = json!({
            "session": session,
            "cwd": self.work,
            "model": "fake-model",
            "system": [{ "type": "text", "text": "Be brief." }],
            "host_tools": [],
            "endpoint": { "url": self.fake.url(), "headers": { "x-test": "1" } },
            "request": { "max_tokens": 1000 },
            "input": [{ "type": "text", "text": text }],
        });
        for (k, v) in extra.as_object().into_iter().flatten() {
            p[k] = v.clone();
        }
        p
    }

    /// Starts a turn; returns its id.
    pub fn start(&mut self, session: &str, text: &str, extra: Value) -> String {
        let p = self.params(session, text, extra);
        let r = self.host.call("turn.start", p);
        r["result"]["turn"]
            .as_str()
            .unwrap_or_else(|| panic!("turn.start failed: {r}"))
            .to_string()
    }

    /// Waits for the turn.end of `turn`.
    pub fn end(&self, turn: &str) -> Value {
        self.host
            .wait_any(
                &json!({ "method": "turn.end", "params": { "turn": turn } }),
                LONG,
            )
            .unwrap()["params"]
            .clone()
    }

    /// Runs one native tool call in a turn of its own: the model calls it, then says "ok".
    /// Returns the tool.end params (or the tool_result, when no tool.end came).
    pub fn tool(&mut self, session: &str, name: &str, input: Value) -> Value {
        let id = format!("call_{}", self.host.lines().len());
        self.fake.push([
            json!({ "tool_calls": [{ "id": id, "name": name, "input": input }] }),
            json!({ "text": "ok" }),
        ]);
        let turn = self.start(session, &format!("run {name}"), json!({}));
        let end = self.end(&turn);
        assert_eq!(end["reason"], "done", "{end}");
        match self.host.wait_any(
            &json!({ "method": "tool.end", "params": { "call_id": id } }),
            Duration::from_millis(10),
        ) {
            Ok(v) => v["params"].clone(),
            Err(_) => {
                let body = self.fake.bodies().last().cloned().unwrap();
                let last = body["messages"].as_array().unwrap().last().unwrap().clone();
                let r = last["content"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|b| b["tool_use_id"] == id)
                    .unwrap()
                    .clone();
                json!({ "content": r["content"], "is_error": r.get("is_error").cloned().unwrap_or(json!(false)) })
            }
        }
    }

    pub fn wire(&self, session: &str) -> String {
        std::fs::read_to_string(self.data.join("sessions").join(session).join("wire.jsonl"))
            .unwrap_or_default()
    }
}

/// The text of a tool result's content.
pub fn text(r: &Value) -> String {
    r["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|b| b["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// A request's messages with every cache_control removed, each serialized.
pub fn plain_messages(body: &Value) -> Vec<String> {
    fn strip(v: &mut Value) {
        match v {
            Value::Object(m) => {
                m.remove("cache_control");
                m.values_mut().for_each(strip);
            }
            Value::Array(a) => a.iter_mut().for_each(strip),
            _ => {}
        }
    }
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            let mut m = m.clone();
            strip(&mut m);
            m.to_string()
        })
        .collect()
}

/// The raw text of a request's `messages` array with `"cache_control":{…}` removed.
pub fn raw_messages(raw: &str) -> String {
    let start = raw.find("\"messages\":[").unwrap() + "\"messages\":".len();
    let end = raw.rfind("],\"stream\":true").unwrap() + 1;
    let mut s = raw[start..end].to_string();
    while let Some(i) = s.find(",\"cache_control\":{") {
        let j = s[i..].find('}').unwrap();
        s.replace_range(i..i + j + 1, "");
    }
    s
}

/// Process ids whose command line contains `needle`.
pub fn pids_with(needle: &str) -> Vec<u32> {
    let mut out = Vec::new();
    for e in std::fs::read_dir("/proc").unwrap().flatten() {
        let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let cmd = std::fs::read(e.path().join("cmdline")).unwrap_or_default();
        let state = std::fs::read_to_string(e.path().join("stat")).unwrap_or_default();
        let zombie = state
            .rsplit(')')
            .next()
            .is_some_and(|s| s.trim_start().starts_with('Z'));
        if !zombie
            && String::from_utf8_lossy(&cmd)
                .replace('\0', " ")
                .contains(needle)
        {
            out.push(pid);
        }
    }
    out
}

/// The process group of a process.
pub fn pgid_of(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit(')')
        .next()?
        .split_whitespace()
        .nth(2)?
        .parse()
        .ok()
}

/// Live (not zombie) processes in process group `pgid`.
pub fn group_members(pgid: u32) -> Vec<u32> {
    let mut out = Vec::new();
    for e in std::fs::read_dir("/proc").unwrap().flatten() {
        let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let stat = std::fs::read_to_string(e.path().join("stat")).unwrap_or_default();
        let rest: Vec<&str> = stat
            .rsplit(')')
            .next()
            .unwrap_or("")
            .split_whitespace()
            .collect();
        if rest.first() != Some(&"Z") && rest.get(2) == Some(&pgid.to_string().as_str()) {
            out.push(pid);
        }
    }
    out
}

/// Resident memory of a process in kB.
pub fn rss_kb(pid: u32) -> u64 {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    status
        .lines()
        .find(|l| l.starts_with("VmRSS:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|n| n.parse().ok())
        .unwrap()
}

pub fn write(path: &Path, text: &str) {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(path, text).unwrap();
}

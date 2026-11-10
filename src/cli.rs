//! `shadow run`: one prompt in the current folder, from a terminal. The
//! command is the host of the same engine `shadow serve` drives: it starts
//! one turn, prints what happens, answers the tool checks itself and has no
//! host tools. The model's text goes to stdout; tool lines and errors to stderr.

use crate::rpc::Host;
use crate::session::{Session, now_ms, random_id, valid_id};
use crate::{tools, turn};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::signal::unix::{SignalKind, signal};

const USAGE: &str = "usage: shadow run [--model NAME] [--max-tokens N] [--base-url URL] \
[--allow edit|shell] [--yes] [--session NAME] [--data-dir DIR] \"<prompt>\"";

const PROMPT: &str = "You are Shadow, a coding agent working in the user's terminal, in the folder {cwd}.

Do what the user asks, using your tools. Read before you change. Keep changes small and to the point, and do not touch what was not asked. When you can check your work by running something, do it, and say plainly what you ran and what you did not.

When a tool result says it was cut, the full text is in the file it names. A result of \"Not run\" means the user sent a message first: read it and decide again. A result that says a tool is not allowed means this run was started without that permission: say so instead of trying another way around it.

Finish with a short plain answer: what you did, what you found, and anything left open.";

const ANTHROPIC: (&str, &str) = ("ANTHROPIC_API_KEY", "https://api.anthropic.com/v1/messages");
const OPENROUTER: (&str, &str) = (
    "OPENROUTER_API_KEY",
    "https://openrouter.ai/api/v1/messages",
);

#[derive(Debug, Default, PartialEq)]
struct Opts {
    prompt: String,
    model: Option<String>,
    max_tokens: u64,
    base_url: Option<String>,
    edit: bool,
    shell: bool,
    session: Option<String>,
    data_dir: Option<PathBuf>,
}

fn parse(args: &[String]) -> Result<Option<Opts>, String> {
    let mut o = Opts {
        max_tokens: 16000,
        ..Opts::default()
    };
    let mut words = Vec::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v.to_string())),
            _ => (arg.as_str(), None),
        };
        let mut value = || {
            inline
                .clone()
                .or_else(|| it.next().cloned())
                .ok_or(format!("{flag} needs a value"))
        };
        match flag {
            "-h" | "--help" => return Ok(None),
            "--yes" => (o.edit, o.shell) = (true, true),
            "--model" => o.model = Some(value()?),
            "--base-url" => o.base_url = Some(value()?),
            "--session" => o.session = Some(value()?),
            "--data-dir" => o.data_dir = Some(PathBuf::from(value()?)),
            "--max-tokens" => {
                o.max_tokens = value()?
                    .parse()
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or("--max-tokens needs a positive number")?;
            }
            "--allow" => {
                for what in value()?.split(',') {
                    match what.trim() {
                        "edit" => o.edit = true,
                        "shell" => o.shell = true,
                        other => return Err(format!("--allow takes edit or shell, not {other:?}")),
                    }
                }
            }
            "--" => words.extend(it.by_ref().cloned()),
            f if f.starts_with('-') && f.len() > 1 => return Err(format!("unknown option {f}")),
            _ => words.push(arg.clone()),
        }
    }
    o.prompt = words.join(" ");
    if o.prompt.trim().is_empty() {
        return Err("no prompt given".into());
    }
    Ok(Some(o))
}

/// The endpoint URL and its key header: ANTHROPIC_API_KEY first, then
/// OPENROUTER_API_KEY; `--base-url` replaces only the URL.
fn endpoint(
    anthropic: Option<String>,
    openrouter: Option<String>,
    base_url: Option<&str>,
) -> Option<(String, (String, String))> {
    let (url, header) = match (anthropic, openrouter) {
        (Some(key), _) => (ANTHROPIC.1, ("x-api-key".to_string(), key)),
        (None, Some(key)) => (
            OPENROUTER.1,
            ("authorization".to_string(), format!("Bearer {key}")),
        ),
        (None, None) => return None,
    };
    Some((base_url.unwrap_or(url).to_string(), header))
}

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

pub fn run(args: &[String]) -> i32 {
    let opts = match parse(args) {
        Ok(Some(o)) => o,
        Ok(None) => {
            println!("{USAGE}");
            return 0;
        }
        Err(e) => {
            eprintln!("shadow: {e}\n{USAGE}");
            return 2;
        }
    };
    let keys = (var(ANTHROPIC.0), var(OPENROUTER.0));
    // SAFETY: no other thread exists yet (the runtime starts below), so
    // nothing reads the environment at the same time. The keys leave it so
    // that the commands the model runs never see them.
    unsafe {
        std::env::remove_var(ANTHROPIC.0);
        std::env::remove_var(OPENROUTER.0);
    }
    // The kernel still shows the starting environment, keys included, in
    // /proc/<pid>/environ. A process that cannot be dumped hides it, and its
    // memory, from the commands the model runs (they exec, so they can be).
    #[cfg(target_os = "linux")]
    {
        // SAFETY: prctl(2) with PR_SET_DUMPABLE only reads its integer arguments.
        if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
            let e = std::io::Error::last_os_error();
            eprintln!("shadow: cannot hide the API key from commands: {e}");
            return 1;
        }
    }
    let model = opts.model.clone().or_else(|| var("SHADOW_MODEL"));
    let endpoint = endpoint(keys.0, keys.1, opts.base_url.as_deref());
    let missing: Vec<&str> = [
        model
            .is_none()
            .then_some("no model: give --model or set SHADOW_MODEL"),
        endpoint
            .is_none()
            .then_some("no API key: set ANTHROPIC_API_KEY or OPENROUTER_API_KEY"),
    ]
    .into_iter()
    .flatten()
    .collect();
    let (Some(model), Some(endpoint)) = (model, endpoint) else {
        eprintln!("shadow: {}", missing.join("; "));
        return 2;
    };
    let (data_dir, params) = match turn_params(&opts, model, endpoint) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("shadow: {e}");
            return 2;
        }
    };
    match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => {
            let code = rt.block_on(go(opts, data_dir, params));
            // Everything the run started has ended; do not wait for idle threads.
            rt.shutdown_background();
            code
        }
        Err(e) => {
            eprintln!("shadow: cannot start the runtime: {e}");
            1
        }
    }
}

/// The data folder and the `turn.start` params of this run: the same
/// request a host would send.
fn turn_params(
    o: &Opts,
    model: String,
    (url, key): (String, (String, String)),
) -> Result<(PathBuf, Value), String> {
    let cwd =
        std::env::current_dir().map_err(|e| format!("cannot read the current folder: {e}"))?;
    let data_dir = match &o.data_dir {
        Some(d) => cwd.join(d),
        None => {
            PathBuf::from(var("HOME").ok_or("HOME is not set; give --data-dir")?).join(".shadow")
        }
    };
    let session = match &o.session {
        Some(s) if valid_id(s) => s.clone(),
        Some(s) => {
            return Err(format!(
                "bad --session {s:?}: use letters, digits, '.', '_' or '-'"
            ));
        }
        None => format!("run-{}-{}", now_ms(), &random_id()[..6]),
    };
    let mut system =
        vec![json!({ "type": "text", "text": PROMPT.replace("{cwd}", &cwd.to_string_lossy()) })];
    if let Some(agents) = agents_md(&cwd)? {
        system.push(json!({ "type": "text", "text": format!("Project instructions (AGENTS.md):\n{agents}") }));
    }
    let headers: BTreeMap<String, String> =
        [key, ("anthropic-version".into(), "2023-06-01".into())].into();
    let params = json!({
        "session": session,
        "cwd": cwd,
        "model": model,
        "request": { "max_tokens": o.max_tokens },
        "system": system,
        "host_tools": [],
        "endpoint": { "url": url, "headers": headers },
        "input": [{ "type": "text", "text": o.prompt }],
    });
    Ok((data_dir, params))
}

/// The text of `AGENTS.md` in the folder; None when there is none or it is blank.
fn agents_md(cwd: &Path) -> Result<Option<String>, String> {
    match std::fs::read_to_string(cwd.join("AGENTS.md")) {
        Ok(t) if t.trim().is_empty() => Ok(None),
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("cannot read AGENTS.md: {e}")),
    }
}

async fn go(opts: Opts, data_dir: PathBuf, params: Value) -> i32 {
    let watch = |kind| signal(kind).map_err(|e| eprintln!("shadow: cannot watch for signals: {e}"));
    let (Ok(mut int), Ok(mut term), Ok(mut hup)) = (
        watch(SignalKind::interrupt()),
        watch(SignalKind::terminate()),
        watch(SignalKind::hangup()),
    ) else {
        return 1;
    };
    let id = params["session"].as_str().unwrap_or_default().to_string();
    let session = Arc::new(Session::new(
        id.clone(),
        data_dir.join("sessions").join(&id),
    ));
    let (host, mut lines) = Host::local();
    match turn::start(&host, session.clone(), params) {
        Ok((t, start)) => turn::spawn(t, start),
        Err((_, e)) => {
            eprintln!("shadow: {e}");
            return 2;
        }
    }
    let mut view = View {
        opts,
        at_line_start: true,
        labels: BTreeMap::new(),
        out_error: None,
    };
    let mut signalled: Option<i32> = None;
    let reason = loop {
        let got = tokio::select! {
            line = lines.recv() => {
                let Some(line) = line else { break "error".to_string() };
                let had_error = view.out_error.is_some();
                let ended = view.line(&host, line);
                if let (false, Some(e)) = (had_error, &view.out_error) {
                    eprintln!("shadow: cannot write the output ({e}); stopping");
                    stop(&session);
                }
                if let Some(reason) = ended {
                    break reason;
                }
                None
            }
            _ = int.recv(), if signalled.is_none() => Some(libc::SIGINT),
            _ = term.recv(), if signalled.is_none() => Some(libc::SIGTERM),
            _ = hup.recv(), if signalled.is_none() => Some(libc::SIGHUP),
        };
        if got.is_some() {
            signalled = got;
            stop(&session);
        }
    };
    // Whatever ended the run, nothing it started keeps running.
    tools::kill_all(vec![session]).await;
    match (signalled, reason.as_str(), &view.out_error) {
        (Some(signo), _, _) => 128 + signo,
        (None, "done", None) => 0,
        _ => 1,
    }
}

/// Stops the running turn, as `turn.stop` does.
fn stop(session: &Session) {
    if let Some(ctl) = session.lock().turn.as_ref() {
        ctl.stop.send_replace(true);
    }
}

struct View {
    opts: Opts,
    at_line_start: bool,
    /// Call id → its short label, for the result line.
    labels: BTreeMap<String, String>,
    /// The first failure to write stdout; nothing more is written after it.
    out_error: Option<std::io::Error>,
}

impl View {
    /// One line from the engine. Requests are answered at once. Some(reason)
    /// when the turn ended.
    fn line(&mut self, host: &Host, msg: Map<String, Value>) -> Option<String> {
        let method = msg
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let p = &msg["params"];
        let result = match (method, msg.get("id")) {
            ("tool.check", Some(_)) => Some(self.check(&p["call"])),
            ("tool.end", Some(_)) => {
                self.ended(p);
                Some(json!({}))
            }
            (_, Some(_)) => Some(Value::Null),
            _ => None,
        };
        if let (Some(result), Some(id)) = (result, msg.get("id")) {
            let mut reply = Map::new();
            reply.insert("id".into(), id.clone());
            match result {
                Value::Null => reply.insert(
                    "error".into(),
                    json!({ "code": "no_host_tools", "message": "shadow run has no host tools" }),
                ),
                r => reply.insert("result".into(), r),
            };
            host.on_reply(&reply);
            return None;
        }
        match method {
            "model.event"
                if p.pointer("/event/delta/type").and_then(Value::as_str) == Some("text_delta") =>
            {
                let text = p
                    .pointer("/event/delta/text")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !text.is_empty() {
                    self.print(text);
                }
            }
            "message.end" => self.newline(),
            "retry" => {
                let why = p["status"]
                    .as_u64()
                    .map_or("the connection broke".to_string(), |s| format!("HTTP {s}"));
                self.newline();
                eprintln!(
                    "shadow: {why}; retrying in {:.1} s (attempt {})",
                    p["wait_ms"].as_f64().unwrap_or(0.0) / 1000.0,
                    p["attempt"]
                );
            }
            "turn.end" => {
                self.newline();
                let reason = p["reason"].as_str().unwrap_or("error").to_string();
                match reason.as_str() {
                    "error" => eprintln!(
                        "shadow: {}",
                        p.pointer("/error/message")
                            .and_then(Value::as_str)
                            .unwrap_or("the turn failed")
                    ),
                    "max_steps" => eprintln!("shadow: stopped after the maximum number of steps"),
                    "stopped" => eprintln!("shadow: stopped"),
                    _ => {}
                }
                return Some(reason);
            }
            _ => {}
        }
        None
    }

    /// The model's text on stdout, as it comes.
    fn print(&mut self, text: &str) {
        if self.out_error.is_some() {
            return;
        }
        let mut out = std::io::stdout().lock();
        match out.write_all(text.as_bytes()).and_then(|_| out.flush()) {
            Ok(()) => self.at_line_start = text.ends_with('\n'),
            Err(e) => self.out_error = Some(e),
        }
    }

    /// Ends the model's text with a newline before a line on stderr.
    fn newline(&mut self) {
        if !self.at_line_start {
            self.print("\n");
        }
    }

    /// Reading and searching are always allowed; Write and Edit need
    /// `--allow edit`, Bash `--allow shell`.
    fn check(&mut self, call: &Value) -> Value {
        let name = call["name"].as_str().unwrap_or_default();
        let label = label(name, &call["input"]);
        self.newline();
        eprintln!("> {label}");
        let refused = match name {
            "Write" | "Edit" if !self.opts.edit => Some("edit"),
            "Bash" if !self.opts.shell => Some("shell"),
            _ => None,
        };
        if let Some(what) = refused {
            eprintln!("  {label}: not allowed (run with --allow {what})");
            return json!({ "decision": "deny", "message": format!("Not allowed: this run was started without --allow {what}.") });
        }
        self.labels
            .insert(call["id"].as_str().unwrap_or_default().to_string(), label);
        json!({ "decision": "allow" })
    }

    /// A short line for a tool result.
    fn ended(&mut self, p: &Value) {
        let label = self
            .labels
            .remove(p["call_id"].as_str().unwrap_or_default())
            .unwrap_or_default();
        let blocks = p["content"].as_array().cloned().unwrap_or_default();
        let text: String = blocks
            .iter()
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let summary = if p["is_error"] == true {
            text.lines()
                .next()
                .unwrap_or("error")
                .chars()
                .take(160)
                .collect::<String>()
        } else if let Some(task) = p.pointer("/meta/task_id").and_then(Value::as_str) {
            format!("running in the background as task {task}")
        } else if blocks.iter().any(|b| b["type"] == "image") {
            "an image".to_string()
        } else {
            format!("{} lines", text.lines().count())
        };
        eprintln!("  {label}: {summary}");
    }
}

/// "Read src/main.rs", "Bash cargo test" — the call's main argument, cut short.
fn label(name: &str, input: &Value) -> String {
    let key = match name {
        "Read" | "Write" | "Edit" => "file_path",
        "Bash" => "command",
        "Grep" | "Glob" => "pattern",
        _ => "task_id",
    };
    let arg = input[key].as_str().unwrap_or_default();
    let first: String = arg
        .lines()
        .next()
        .unwrap_or_default()
        .chars()
        .take(80)
        .collect();
    let more = if first.len() < arg.len() { "…" } else { "" };
    format!("{name} {first}{more}").trim_end().to_string()
}

#[cfg(test)]
#[path = "cli_test.rs"]
mod tests;

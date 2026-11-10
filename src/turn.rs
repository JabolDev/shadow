//! A turn: the loop of Appendix B. One step is one model call plus its tool
//! calls; the turn ends when the model's message has no tool call (done), on
//! Stop, at max_steps, or on an error retries cannot fix.

use crate::model::{self, Endpoint, Outcome};
use crate::rpc::{Host, Reply, error_text};
use crate::session::{Inbox, Session, Steer, TurnCtl, now_ms, random_id};
use crate::tools;
use crate::versions::Versions;
use crate::wire::{self, Wire, error_result};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Instant;
use tokio::sync::{oneshot, watch};

pub const STOPPED: &str = "Stopped by the user.";
const RESERVED: [&str; 5] = ["model", "system", "tools", "messages", "stream"];

pub struct Limits {
    pub max_steps: u64,
    pub bash_wait_ms: u64,
    pub result_cap_chars: usize,
    pub retry_max: u32,
}

pub struct HostTool {
    pub name: String,
    pub parallel: bool,
}

pub struct Turn {
    pub host: Host,
    pub session: Arc<Session>,
    pub id: String,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    pub limits: Limits,
    pub stop: watch::Receiver<bool>,
    /// Steer messages waiting (see TurnCtl::waiting).
    pub waiting: watch::Receiver<usize>,
    /// Assistant messages in the context so far (the chat's step number).
    pub chat_step: AtomicU64,
    model: String,
    system: Value,
    request: Map<String, Value>,
    endpoint: Endpoint,
    tools: Arc<Vec<Value>>,
    native: Vec<&'static str>,
    host_tools: Vec<HostTool>,
    cache: Option<Value>,
    versions: OnceLock<Mutex<Versions>>,
    steps: AtomicU64,
    usage: Mutex<Map<String, Value>>,
    cursor: Mutex<Option<String>>,
}

/// What the turn appends before its first model call.
pub struct Start {
    catchup: Vec<Value>,
    cursor: Option<String>,
    input: Vec<Value>,
}

#[derive(Deserialize)]
struct Params {
    cwd: String,
    #[serde(default)]
    env: BTreeMap<String, String>,
    model: String,
    #[serde(default)]
    request: Map<String, Value>,
    system: Vec<Value>,
    host_tools: Vec<HostToolSpec>,
    native_tools: Option<Vec<String>>,
    endpoint: EndpointSpec,
    #[serde(default)]
    limits: LimitsSpec,
    cache: Option<CacheSpec>,
    #[serde(default)]
    catchup: Vec<Value>,
    cursor: Option<String>,
    input: Vec<Value>,
}

#[derive(Deserialize)]
struct HostToolSpec {
    name: String,
    description: Option<String>,
    input_schema: Map<String, Value>,
    #[serde(default)]
    parallel: bool,
}

#[derive(Deserialize)]
pub struct EndpointSpec {
    url: String,
    #[serde(default)]
    headers: BTreeMap<String, String>,
}

impl EndpointSpec {
    pub fn build(self) -> Result<Endpoint, String> {
        Ok(Endpoint {
            url: crate::http::Url::parse(&self.url)?,
            headers: self.headers.into_iter().collect(),
        })
    }
}

#[derive(Deserialize, Default)]
struct LimitsSpec {
    max_steps: Option<u64>,
    bash_wait_ms: Option<u64>,
    result_cap_chars: Option<usize>,
    retry_max: Option<u32>,
}

#[derive(Deserialize)]
struct CacheSpec {
    mode: String,
    ttl: Option<String>,
}

pub type BadRequest = String;

/// Checks `turn.start` params and registers the turn in its chat. The caller
/// replies `{turn}`, then calls `spawn`.
pub fn start(
    host: &Host,
    session: Arc<Session>,
    params: Value,
) -> Result<(Arc<Turn>, Start), (&'static str, BadRequest)> {
    let bad = |m: String| ("bad_request", m);
    let p: Params = serde_json::from_value(params).map_err(|e| bad(format!("turn.start: {e}")))?;
    let cwd = PathBuf::from(&p.cwd);
    if !cwd.is_absolute() || !cwd.is_dir() {
        return Err(bad(format!(
            "cwd must be an existing absolute folder: {}",
            p.cwd
        )));
    }
    let native: Vec<&'static str> = match &p.native_tools {
        None => tools::NATIVE.to_vec(),
        Some(names) => {
            if let Some(n) = names.iter().find(|n| !tools::NATIVE.contains(&n.as_str())) {
                return Err(bad(format!("native_tools: no native tool {n:?}")));
            }
            tools::NATIVE
                .iter()
                .copied()
                .filter(|n| names.iter().any(|m| m == n))
                .collect()
        }
    };
    let mut defs: Vec<Value> = tools::definitions()
        .into_iter()
        .filter(|d| native.contains(&d["name"].as_str().unwrap_or("")))
        .collect();
    let mut host_tools = Vec::new();
    for h in p.host_tools {
        if h.name.is_empty()
            || native.contains(&h.name.as_str())
            || host_tools.iter().any(|t: &HostTool| t.name == h.name)
        {
            return Err(bad(format!(
                "host_tools: the name {:?} is empty or used twice",
                h.name
            )));
        }
        let mut def = Map::new();
        def.insert("name".into(), h.name.clone().into());
        if let Some(d) = h.description {
            def.insert("description".into(), d.into());
        }
        def.insert("input_schema".into(), Value::Object(h.input_schema));
        defs.push(Value::Object(def));
        host_tools.push(HostTool {
            name: h.name,
            parallel: h.parallel,
        });
    }
    if let Some(k) = p.request.keys().find(|k| RESERVED.contains(&k.as_str())) {
        return Err(bad(format!("request may not set {k:?}")));
    }
    let cache = match p.cache {
        None => Some(json!({ "type": "ephemeral" })),
        Some(c) => match (c.mode.as_str(), c.ttl.as_deref()) {
            ("off", _) => None,
            ("auto", None) => Some(json!({ "type": "ephemeral" })),
            ("auto", Some(ttl @ ("5m" | "1h"))) => Some(json!({ "type": "ephemeral", "ttl": ttl })),
            _ => {
                return Err(bad(
                    "cache: mode is \"auto\" or \"off\", ttl is \"5m\" or \"1h\"".into(),
                ));
            }
        },
    };
    if p.input.is_empty()
        || p.catchup.iter().any(|m| {
            !matches!(
                m.get("role").and_then(Value::as_str),
                Some("user" | "assistant")
            )
        })
    {
        return Err(bad("input must be a non-empty list of content blocks; catchup items need role user or assistant".into()));
    }
    let endpoint = EndpointSpec::build(p.endpoint).map_err(bad)?;
    let l = p.limits;
    let limits = Limits {
        max_steps: l.max_steps.unwrap_or(50),
        bash_wait_ms: l.bash_wait_ms.unwrap_or(120_000),
        result_cap_chars: l.result_cap_chars.unwrap_or(30_000),
        retry_max: l.retry_max.unwrap_or(6),
    };
    if limits.max_steps == 0 || limits.result_cap_chars < 200 {
        return Err(bad(
            "limits: max_steps is at least 1 and result_cap_chars at least 200".into(),
        ));
    }

    let id = random_id();
    let (stop_tx, stop) = watch::channel(false);
    let (waiting_tx, waiting) = watch::channel(0usize);
    let tools = Arc::new(defs);
    {
        let mut st = session.lock();
        if st.turn.is_some() || st.compacting {
            return Err((
                "busy",
                "a turn or a compaction is running in this chat".into(),
            ));
        }
        st.turn = Some(TurnCtl {
            id: id.clone(),
            stop: stop_tx,
            waiting: waiting_tx,
            steers: Vec::new(),
            notes: Vec::new(),
        });
        st.last_tools = Some(tools.clone());
    }
    let turn = Turn {
        host: host.clone(),
        session,
        id,
        cwd,
        env: p.env.into_iter().collect(),
        limits,
        stop,
        waiting,
        chat_step: AtomicU64::new(0),
        model: p.model,
        system: Value::Array(p.system),
        request: p.request,
        endpoint,
        tools,
        native,
        host_tools,
        cache,
        versions: OnceLock::new(),
        steps: AtomicU64::new(0),
        usage: Mutex::new(Map::new()),
        cursor: Mutex::new(None),
    };
    Ok((
        Arc::new(turn),
        Start {
            catchup: p.catchup,
            cursor: p.cursor,
            input: p.input,
        },
    ))
}

/// Runs the turn in the background; it always ends with `turn.end`.
pub fn spawn(turn: Arc<Turn>, start: Start) {
    tokio::spawn(async move {
        let t = turn.clone();
        let end = match tokio::spawn(async move { drive(&t, start).await }).await {
            Ok(Ok(end)) => end,
            Ok(Err(e)) => Some(End::error(json!({ "message": format!("shadow: {e}") }))),
            Err(e) => Some(End::error(
                json!({ "message": format!("shadow: internal error: {e}") }),
            )),
        };
        if let Some(end) = end {
            turn.finish(end);
        }
    });
}

struct End {
    reason: &'static str,
    error: Option<Value>,
}

impl End {
    fn new(reason: &'static str) -> End {
        End {
            reason,
            error: None,
        }
    }

    fn error(e: Value) -> End {
        End {
            reason: "error",
            error: Some(e),
        }
    }
}

#[derive(Clone)]
struct Call {
    id: String,
    name: String,
    input: Value,
}

impl Call {
    fn json(&self) -> Value {
        json!({ "id": self.id, "name": self.name, "input": self.input })
    }
}

/// The turn's steps. Ok(None): the turn already sent `turn.end` (done).
async fn drive(t: &Arc<Turn>, start: Start) -> io::Result<Option<End>> {
    let mut wire = Wire::load(&t.session.wire_path())?;
    for m in &start.catchup {
        wire.push_message(m);
    }
    if let Some(c) = &start.cursor {
        wire.push_cursor(c);
    }
    wire.push_message(&json!({ "role": "user", "content": start.input }));
    wire.commit()?;
    *lock(&t.cursor) = wire.cursor.clone();
    t.chat_step.store(wire.assistant_count, Ordering::Relaxed);
    let mut stop = t.stop.clone();
    loop {
        if t.stopped() {
            return Ok(Some(End::new("stopped")));
        }
        let step = t.steps.fetch_add(1, Ordering::Relaxed) + 1;
        t.emit("step.start", json!({ "step": step }));
        let body = model::body(
            &t.model,
            Some(&t.system),
            &t.tools,
            &wire.messages,
            &t.request,
            t.cache.as_ref(),
        );
        let mut on_event = |ev: &Value| t.emit("model.event", json!({ "step": step, "event": ev }));
        let mut on_retry = |attempt: u32, wait_ms: u64, status: Option<u16>| {
            t.emit(
                "retry",
                json!({ "step": step, "attempt": attempt, "wait_ms": wait_ms, "status": status }),
            )
        };
        let reply = match model::call(
            &t.endpoint,
            &body,
            t.limits.retry_max,
            &mut stop,
            &mut on_event,
            &mut on_retry,
        )
        .await
        {
            Outcome::Done(r) => r,
            Outcome::Failed(e) => return Ok(Some(End::error(e))),
            Outcome::Stopped(partial) => {
                if let Some(msg) = partial {
                    wire.push_message(&msg);
                    let results: Vec<Value> = wire::tool_uses(&msg)
                        .map(|b| error_result(b["id"].as_str().unwrap_or(""), STOPPED))
                        .collect();
                    if !results.is_empty() {
                        wire.push_message(&json!({ "role": "user", "content": results }));
                    }
                    wire.commit()?;
                    t.emit(
                        "message.end",
                        json!({ "step": step, "message": msg, "usage": {}, "stop_reason": null }),
                    );
                }
                return Ok(Some(End::new("stopped")));
            }
        };
        t.add_usage(&reply.usage);
        let calls: Vec<Call> = wire::tool_uses(&reply.message)
            .map(|b| Call {
                id: b["id"].as_str().unwrap_or("").into(),
                name: b["name"].as_str().unwrap_or("").into(),
                input: b["input"].clone(),
            })
            .collect();
        let empty = reply.message["content"]
            .as_array()
            .is_none_or(Vec::is_empty);
        wire.push_model_call(now_ms());
        if !empty {
            wire.push_message(&reply.message);
        }
        wire.commit()?;
        t.chat_step.store(wire.assistant_count, Ordering::Relaxed);
        t.emit("message.end", json!({ "step": step, "message": reply.message, "usage": reply.usage, "stop_reason": reply.stop_reason }));

        if calls.is_empty() {
            let Some(inbox) = t.done_or_continue(step < t.limits.max_steps) else {
                return Ok(None);
            };
            wire.push_message(&json!({ "role": "user", "content": inbox.blocks() }));
            wire.commit()?;
            t.delivered(&inbox);
            continue;
        }
        let mut content = run_batch(t, step, calls).await;
        let stopped = t.stopped();
        let inbox = if stopped {
            Inbox::default()
        } else {
            t.session.take_inbox(&t.id)
        };
        content.extend(inbox.blocks());
        wire.push_message(&json!({ "role": "user", "content": content }));
        wire.commit()?;
        t.save_versions();
        t.delivered(&inbox);
        if stopped {
            return Ok(Some(End::new("stopped")));
        }
        if step >= t.limits.max_steps {
            return Ok(Some(End::new("max_steps")));
        }
    }
}

/// Runs one step's calls: neighbouring parallel calls at once, the rest
/// alone, in call order. Results come back in call order.
async fn run_batch(t: &Arc<Turn>, step: u64, calls: Vec<Call>) -> Vec<Value> {
    let mut out = Vec::with_capacity(calls.len());
    let mut i = 0;
    while i < calls.len() {
        let mut j = i + 1;
        if t.parallel(&calls[i].name) {
            while j < calls.len() && t.parallel(&calls[j].name) {
                j += 1;
            }
        }
        let group = &calls[i..j];
        if t.stopped() {
            out.extend(group.iter().map(|c| error_result(&c.id, STOPPED)));
        } else if group.len() == 1 {
            let first = begin(t, step, &group[0]);
            out.push(finish(t.clone(), step, group[0].clone(), first).await);
        } else {
            // Every call's first request goes out in call order; then they run at once.
            let firsts: Vec<Pending> = group.iter().map(|c| begin(t, step, c)).collect();
            let running: Vec<_> = group
                .iter()
                .zip(firsts)
                .map(|(c, p)| tokio::spawn(finish(t.clone(), step, c.clone(), p)))
                .collect();
            for (h, c) in running.into_iter().zip(group) {
                out.push(h.await.unwrap_or_else(|e| {
                    error_result(&c.id, &format!("shadow: internal error: {e}"))
                }));
            }
        }
        i = j;
    }
    out
}

/// A call after its first message went out.
enum Pending {
    /// A host tool: tool.start sent, waiting for tool.call's reply.
    Host(oneshot::Receiver<Reply>),
    /// A native tool: waiting for tool.check's reply.
    Check(oneshot::Receiver<Reply>),
    Done(Value),
}

fn begin(t: &Turn, step: u64, c: &Call) -> Pending {
    if t.host_tools.iter().any(|h| h.name == c.name) {
        t.emit(
            "tool.start",
            json!({ "step": step, "call": c.json(), "native": false }),
        );
        return Pending::Host(t.ask("tool.call", json!({ "step": step, "call": c.json() })));
    }
    if !t.native.contains(&c.name.as_str()) {
        return Pending::Done(error_result(&c.id, &format!("No such tool: {}", c.name)));
    }
    Pending::Check(t.ask("tool.check", json!({ "step": step, "call": c.json() })))
}

async fn finish(t: Arc<Turn>, step: u64, c: Call, first: Pending) -> Value {
    let check = match first {
        Pending::Done(v) => return v,
        Pending::Host(rx) => {
            let Some(reply) = t.reply_or_stop(rx).await else {
                return error_result(&c.id, STOPPED);
            };
            return match reply {
                Ok(r) => match r.get("content") {
                    Some(content @ (Value::String(_) | Value::Array(_))) => result_block(
                        &c.id,
                        content.clone(),
                        r.get("is_error") == Some(&Value::Bool(true)),
                    ),
                    _ => error_result(&c.id, "The host returned no content for this tool."),
                },
                Err(e) => error_result(&c.id, &format!("The host tool failed: {}", error_text(&e))),
            };
        }
        Pending::Check(rx) => rx,
    };
    let Some(reply) = t.reply_or_stop(check).await else {
        return error_result(&c.id, STOPPED);
    };
    let input = match reply {
        Ok(r) => match (r.get("decision").and_then(Value::as_str), r.get("input")) {
            (Some("allow"), None | Some(Value::Null)) => c.input.clone(),
            (Some("allow"), Some(i @ Value::Object(_))) => i.clone(),
            (Some("allow"), Some(_)) => {
                return error_result(
                    &c.id,
                    "The host's check returned an input that is not an object.",
                );
            }
            (Some("deny"), _) => {
                return error_result(
                    &c.id,
                    r.get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("Denied."),
                );
            }
            _ => return error_result(&c.id, &format!("The host's check gave no decision: {r}")),
        },
        Err(e) => {
            return error_result(
                &c.id,
                &format!("The host's check failed: {}", error_text(&e)),
            );
        }
    };
    t.emit("tool.start", json!({ "step": step, "call": { "id": c.id, "name": c.name, "input": input }, "native": true }));
    let started = Instant::now();
    let out = tools::run(&t, &c.id, &c.name, &input).await;
    let meta = out.meta_json(started.elapsed().as_millis() as u64);
    let content = Value::Array(out.content);
    let end = t.ask(
        "tool.end",
        json!({ "step": step, "call_id": c.id, "name": c.name, "input": input, "content": content, "is_error": out.is_error, "meta": meta }),
    );
    if !t.stopped()
        && let Some(Err(e)) = t.reply_or_stop(end).await
    {
        eprintln!("shadow: tool.end for {}: {}", c.id, error_text(&e));
    }
    result_block(&c.id, content, out.is_error)
}

fn result_block(id: &str, content: Value, is_error: bool) -> Value {
    let mut b = json!({ "type": "tool_result", "tool_use_id": id, "content": content });
    if is_error {
        b["is_error"] = Value::Bool(true);
    }
    b
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Turn {
    fn with_ids(&self, extra: Value) -> Value {
        let mut m = Map::new();
        m.insert("session".into(), self.session.id.clone().into());
        m.insert("turn".into(), self.id.clone().into());
        if let Value::Object(e) = extra {
            m.extend(e);
        }
        Value::Object(m)
    }

    pub fn emit(&self, method: &str, extra: Value) {
        self.host.notify(method, self.with_ids(extra));
    }

    fn ask(&self, method: &str, extra: Value) -> oneshot::Receiver<Reply> {
        self.host.request(method, self.with_ids(extra))
    }

    pub fn stopped(&self) -> bool {
        *self.stop.borrow()
    }

    /// The host's reply, or None when the turn is stopped first.
    async fn reply_or_stop(&self, rx: oneshot::Receiver<Reply>) -> Option<Reply> {
        let mut stop = self.stop.clone();
        tokio::select! {
            r = rx => Some(r.unwrap_or_else(|_| Err(json!({ "message": "the request was dropped" })))),
            _ = stop.wait_for(|s| *s) => None,
        }
    }

    fn parallel(&self, name: &str) -> bool {
        (matches!(name, "Read" | "Grep" | "Glob") && self.native.contains(&name))
            || self.host_tools.iter().any(|h| h.name == name && h.parallel)
    }

    pub fn versions(&self) -> MutexGuard<'_, Versions> {
        lock(
            self.versions
                .get_or_init(|| Mutex::new(Versions::load(&self.session.versions_path()))),
        )
    }

    fn save_versions(&self) {
        if let Some(v) = self.versions.get()
            && let Err(e) = lock(v).save()
        {
            eprintln!(
                "shadow: cannot save {}: {e}",
                self.session.versions_path().display()
            );
        }
    }

    fn add_usage(&self, usage: &Value) {
        let mut total = lock(&self.usage);
        for (k, v) in usage.as_object().into_iter().flatten() {
            if let Some(n) = v.as_u64() {
                let sum = total.get(k).and_then(Value::as_u64).unwrap_or(0) + n;
                total.insert(k.clone(), sum.into());
            }
        }
    }

    fn delivered(&self, inbox: &Inbox) {
        for Steer { id, .. } in &inbox.steers {
            self.emit("steer.delivered", json!({ "id": id }));
        }
    }

    /// The model is done. Waiting steers (or task notes) go on as a new user
    /// message when `may_continue`; otherwise the turn ends here.
    fn done_or_continue(&self, may_continue: bool) -> Option<Inbox> {
        {
            let mut st = self.session.lock();
            if let Some(ctl) = st.turn.as_mut().filter(|c| c.id == self.id)
                && may_continue
                && (!ctl.steers.is_empty() || !ctl.notes.is_empty())
            {
                ctl.waiting.send_replace(0);
                return Some(Inbox {
                    notes: std::mem::take(&mut ctl.notes),
                    steers: std::mem::take(&mut ctl.steers),
                });
            }
        }
        self.finish(End::new("done"));
        None
    }

    /// Sends `turn.end`; steers still waiting are listed as undelivered.
    fn finish(&self, end: End) {
        self.save_versions();
        let mut st = self.session.lock();
        let undelivered: Vec<Value> = match st.turn.take_if(|c| c.id == self.id) {
            Some(ctl) => ctl.steers.into_iter().map(|s| s.id).collect(),
            None => Vec::new(),
        };
        let mut params = json!({
            "reason": end.reason,
            "steps": self.steps.load(Ordering::Relaxed),
            "usage": Value::Object(lock(&self.usage).clone()),
            "undelivered": undelivered,
            "cursor": lock(&self.cursor).clone(),
        });
        if let Some(e) = end.error {
            params["error"] = e;
        }
        self.emit("turn.end", params);
    }
}

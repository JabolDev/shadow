//! A turn: the loop of Appendix B. One step is one model call plus its tool
//! calls; the turn ends when the model's message has no tool call (done), on
//! Stop, at max_steps, or on an error retries cannot fix. A branch runs the
//! same loop on a copy of the context and returns only its final text.

use crate::compact;
use crate::context::{self, Repeats};
use crate::model::{self, Endpoint, Outcome};
use crate::rpc::{Host, Reply, error_text};
use crate::session::{Inbox, Session, Steer, TurnCtl, now_ms, random_id};
use crate::tools;
use crate::versions::Versions;
use crate::wire::{self, Wire, error_result};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashSet};
use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Instant;
use tokio::sync::{oneshot, watch};

pub const STOPPED: &str = "Stopped by the user.";
const NOT_RUN: &str = "Not run: a message from the user arrived first. Read it, then decide again.";
const NOT_FINISHED: &str = "Not finished: a message from the user arrived first.";
const NOT_IN_BRANCH: &str = "Not available in a branch.";
const BRANCH_NOTE: &str = "You are a branch of the conversation above. Only your final answer goes back to it, and you cannot change files here (Edit, Write and Branch are not available).";
const RESERVED: [&str; 5] = ["model", "system", "tools", "messages", "stream"];
/// Test only: the prompt cache's lifetime in ms, instead of 5 minutes or 1 hour.
const TEST_CACHE_TTL: &str = "SHADOW_TEST_CACHE_TTL_MS";

#[derive(Clone)]
pub struct Limits {
    pub max_steps: u64,
    pub bash_wait_ms: u64,
    pub result_cap_chars: usize,
    pub retry_max: u32,
}

#[derive(Clone)]
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
    ctx: context::Settings,
    /// In a branch: the id of its Branch call (sent as `branch`).
    branch: Option<String>,
    versions: OnceLock<Mutex<Versions>>,
    steps: AtomicU64,
    /// The turn's usage; its branches add theirs.
    usage: Arc<Mutex<Map<String, Value>>>,
    cursor: Mutex<Option<String>>,
    /// Ids of steers taken from the queue whose `steer.delivered` is not sent yet.
    taken: Mutex<Vec<Value>>,
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
    /// `true` turns auto-compaction on; anything else leaves it off.
    auto_compact: Option<Value>,
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
    #[serde(default)]
    keep_latest: bool,
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
    context_tokens: Option<u64>,
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
    let mut keep = Vec::new();
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
        if h.keep_latest {
            keep.push(h.name.clone());
        }
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
    if limits.max_steps == 0 || limits.result_cap_chars < 200 || l.context_tokens == Some(0) {
        return Err(bad(
            "limits: max_steps and context_tokens are at least 1, result_cap_chars at least 200"
                .into(),
        ));
    }
    let lifetime = match cache.as_ref().map(|c| c.get("ttl")) {
        None => None,
        Some(Some(t)) if t == "1h" => Some(3_600_000),
        Some(_) => Some(300_000),
    };
    // Read only when set (tests).
    let test_ttl = std::env::var(TEST_CACHE_TTL)
        .ok()
        .and_then(|v| v.parse().ok());
    let ctx = context::Settings {
        window: l.context_tokens,
        auto_compact: p.auto_compact == Some(Value::Bool(true)),
        cache_ms: lifetime.map(|ms| test_ttl.unwrap_or(ms)),
        keep,
    };

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
        st.last_keep = ctx.keep.clone();
        st.last_cap = Some(limits.result_cap_chars);
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
        ctx,
        branch: None,
        versions: OnceLock::new(),
        steps: AtomicU64::new(0),
        usage: Arc::default(),
        cursor: Mutex::new(None),
        taken: Mutex::new(Vec::new()),
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
pub struct Call {
    pub id: String,
    pub name: String,
    pub input: Value,
}

impl Call {
    fn json(&self) -> Value {
        json!({ "id": self.id, "name": self.name, "input": self.input })
    }
}

/// The turn's steps. Ok(None): the turn already sent `turn.end` (done).
async fn drive(t: &Arc<Turn>, start: Start) -> io::Result<Option<End>> {
    let mut wire = Wire::load(&t.session.wire_path())?;
    let now = now_ms();
    // Cold wake: the cache is gone anyway, so every candidate fades now.
    if t.ctx.cold(wire.last_model_call_at, now) {
        let f = context::fade(&wire, 0, &t.cwd, &t.session.scratch(), &t.ctx.keep, None);
        t.apply_fade(&mut wire, f);
    }
    for m in &start.catchup {
        wire.push_message(m);
    }
    if let Some(c) = &start.cursor {
        wire.push_cursor(c);
    }
    let mut input = start.input;
    let changed = t.versions().changed_outside();
    if let Some(d) = context::delta(changed, &t.session, wire.last_model_call_at, now) {
        input.insert(0, d);
    }
    wire.push_message(&json!({ "role": "user", "content": input }));
    wire.commit()?;
    t.save_versions();
    *lock(&t.cursor) = wire.cursor.clone();
    t.chat_step.store(wire.assistant_count, Ordering::Relaxed);
    let mut pace = Pace::default();
    loop {
        if t.stopped() {
            return Ok(Some(End::new("stopped")));
        }
        let step = t.steps.fetch_add(1, Ordering::Relaxed) + 1;
        t.emit("step.start", json!({ "step": step }));
        let reply = match model_step(t, step, &wire.messages).await {
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
        let calls = calls_of(&reply.message);
        let empty = reply.message["content"]
            .as_array()
            .is_none_or(Vec::is_empty);
        if !empty {
            wire.push_message(&reply.message);
        }
        wire.push_model_call(now_ms(), reported(&reply.usage));
        wire.commit()?;
        t.chat_step.store(wire.assistant_count, Ordering::Relaxed);
        t.emit("message.end", json!({ "step": step, "message": reply.message, "usage": reply.usage, "stop_reason": reply.stop_reason }));

        if calls.is_empty() {
            pace.repeats
                .apply(wire.assistant_count, &[], &mut [], &wire.faded);
            let Some(inbox) = t.done_or_continue(step < t.limits.max_steps) else {
                return Ok(None);
            };
            boundary(t, &mut wire, Vec::new(), inbox, &mut pace).await?;
            continue;
        }
        // A branch starts from the messages and the version table before
        // this step, taken before any of its calls runs.
        let before = &wire.messages[..wire.messages.len() - 1];
        let fork = calls.iter().any(|c| c.name == "Branch").then(|| {
            Arc::new(Fork {
                messages: before.to_vec(),
                versions: t.versions().clone(),
            })
        });
        let mut content = run_batch(t, step, &calls, fork).await;
        pace.repeats
            .apply(wire.assistant_count, &calls, &mut content, &wire.faded);
        if t.stopped() {
            wire.push_message(&json!({ "role": "user", "content": content }));
            wire.commit()?;
            t.save_versions();
            return Ok(Some(End::new("stopped")));
        }
        let inbox = t.take_inbox();
        boundary(t, &mut wire, content, inbox, &mut pace).await?;
        if step >= t.limits.max_steps {
            return Ok(Some(End::new("max_steps")));
        }
    }
}

/// Where a step's branches start: the messages before the step and the
/// version table before any call of the step ran.
struct Fork {
    messages: Vec<String>,
    versions: Versions,
}

/// What the loop remembers between step boundaries.
#[derive(Default)]
struct Pace {
    repeats: Repeats,
    /// Auto-compaction failed in this turn: it is not tried again.
    compact_failed: bool,
}

/// A step boundary: the step's results, then waiting notes and steers, go
/// into one user message. With a window: at 85% (and `auto_compact`) the
/// chat is compacted instead; at 60%, when the candidates would free 10% of
/// the window, old results fade in one batch down to 40%; the gauge line
/// follows the results when the context crossed 50, 75 or 90%.
async fn boundary(
    t: &Arc<Turn>,
    wire: &mut Wire,
    results: Vec<Value>,
    inbox: Inbox,
    pace: &mut Pace,
) -> io::Result<()> {
    let user = |blocks: &[Value]| json!({ "role": "user", "content": blocks });
    let n = results.len();
    let mut content = results;
    content.extend(inbox.blocks());
    let extra = user(&content).to_string().chars().count() as u64 / 4;
    let size = wire.size() + extra;
    let over = |pct| t.ctx.share(pct).is_some_and(|at| size >= at);
    if t.ctx.auto_compact && !pace.compact_failed && over(85) {
        let mut messages = wire.messages.clone();
        if n > 0 {
            messages.push(user(&content[..n]).to_string());
        }
        if let Some((msg, ledger)) = auto_compact(t, messages, wire.ledger.as_ref()).await {
            if n > 0 {
                wire.push_message(&user(&content[..n]));
            }
            wire.push_compaction(&msg, ledger);
            // Steers and notes come after the summary, word for word.
            if n < content.len() {
                wire.push_message(&user(&content[n..]));
            }
            wire.commit()?;
            t.emit("compaction", json!({ "state": "done" }));
            t.versions().clear_shown();
            t.save_versions();
            t.delivered(&inbox);
            t.chat_step.store(wire.assistant_count, Ordering::Relaxed);
            *pace = Pace::default();
            return Ok(());
        }
        // The log is unchanged; the turn goes on and does not try again.
        pace.compact_failed = true;
    }
    let mut fade = None;
    if over(60) {
        let f = context::fade(
            wire,
            extra,
            &t.cwd,
            &t.session.scratch(),
            &t.ctx.keep,
            t.ctx.share(40),
        );
        // Only a fade worth 10% of the window: never a small one every step.
        fade = t.ctx.share(10).filter(|ten| f.worth >= *ten).map(|_| f);
    }
    let after = fade.as_ref().map_or(size, |f| f.size);
    if let Some(line) = t
        .ctx
        .window
        .and_then(|w| context::gauge(&wire.messages, after, w))
    {
        content.insert(n, json!({ "type": "text", "text": line }));
    }
    wire.push_message(&user(&content));
    if let Some(f) = fade {
        t.apply_fade(wire, f);
    }
    wire.commit()?;
    t.save_versions();
    t.delivered(&inbox);
    Ok(())
}

/// The turn's own compaction (as `session.compact` sends it, with the turn's
/// model, endpoint, request, system and tools): the summary message and the
/// ledger for the marker. None when the summary or the ledger failed.
async fn auto_compact(
    t: &Turn,
    messages: Vec<String>,
    earlier: Option<&Value>,
) -> Option<(Value, Option<Value>)> {
    t.emit("compaction", json!({ "state": "start" }));
    let ask = compact::Ask {
        model: &t.model,
        system: Some(&t.system),
        tools: &t.tools,
        request: &t.request,
        cache: t.cache.as_ref(),
        endpoint: &t.endpoint,
        retry_max: t.limits.retry_max,
    };
    let mut stop = t.stop.clone();
    let made = match compact::summary(&ask, messages.clone(), None, &mut stop).await {
        Ok((summary, usage)) => {
            t.add_usage(&usage);
            let (keep, cap) = (&t.ctx.keep, t.limits.result_cap_chars);
            compact::message(&messages, earlier, &summary, &t.session, keep, cap)
        }
        Err(e) => Err(e),
    };
    match made {
        Ok(m) => Some(m),
        Err(e) => {
            eprintln!("shadow: auto-compaction of {}: {e}", t.session.id);
            t.emit("compaction", json!({ "state": "failed" }));
            None
        }
    }
}

/// The tokens a model call reported for the context up to its message.
fn reported(usage: &Value) -> Option<u64> {
    let keys = [
        "input_tokens",
        "cache_read_input_tokens",
        "cache_creation_input_tokens",
        "output_tokens",
    ];
    let n: Vec<u64> = keys.iter().filter_map(|k| usage[k].as_u64()).collect();
    (!n.is_empty()).then(|| n.iter().sum())
}

fn calls_of(message: &Value) -> Vec<Call> {
    wire::tool_uses(message)
        .map(|b| Call {
            id: b["id"].as_str().unwrap_or("").into(),
            name: b["name"].as_str().unwrap_or("").into(),
            input: b["input"].clone(),
        })
        .collect()
}

/// One model call of a turn or a branch, with its notifications.
async fn model_step(t: &Turn, step: u64, messages: &[String]) -> Outcome {
    let body = model::body(
        &t.model,
        Some(&t.system),
        &t.tools,
        messages,
        &t.request,
        t.cache.as_ref(),
    );
    let mut stop = t.stop.clone();
    let mut on_event = |ev: &Value| t.emit("model.event", json!({ "step": step, "event": ev }));
    let mut on_retry = |attempt: u32, wait_ms: u64, status: Option<u16>| {
        t.emit(
            "retry",
            json!({ "step": step, "attempt": attempt, "wait_ms": wait_ms, "status": status }),
        )
    };
    model::call(
        &t.endpoint,
        &body,
        t.limits.retry_max,
        &mut stop,
        &mut on_event,
        &mut on_retry,
    )
    .await
}

/// Branch: the task on a copy of the context before this step; only its
/// final text comes back. A waiting steer ends it at once.
async fn branch(t: &Arc<Turn>, call_id: &str, input: &Value, fork: Arc<Fork>) -> tools::Out {
    let task = match tools::req_str(input, "task") {
        Ok(task) => task,
        Err(e) => return tools::Out::error(e),
    };
    let b = Arc::new(t.branch_of(call_id, fork.versions.clone()));
    let mut messages = fork.messages.clone();
    let ask = json!([{ "type": "text", "text": task }, { "type": "text", "text": BRANCH_NOTE }]);
    messages.push(json!({ "role": "user", "content": ask }).to_string());
    let mut waiting = t.waiting.clone();
    let end = tokio::select! {
        biased;
        r = branch_steps(&b, messages) => r,
        Ok(_) = waiting.wait_for(|n| *n > 0) => Err(NOT_FINISHED.to_string()),
    };
    match end {
        Ok(text) if text.trim().is_empty() => tools::Out::text("(The branch gave no answer.)"),
        Ok(text) => {
            let file = tools::overflow_file(&t.session.scratch(), call_id);
            match tools::bound(&text, t.limits.result_cap_chars, &file) {
                Ok(bounded) => tools::Out::bounded(bounded, false),
                Err(e) => tools::Out::error(format!("Cannot write the overflow file: {e}")),
            }
        }
        Err(e) => tools::Out::error(e),
    }
}

/// A branch's loop: like the turn's, but nothing is logged, no steer or note
/// is taken, and the model's first message without a tool call is the answer.
async fn branch_steps(b: &Arc<Turn>, mut messages: Vec<String>) -> Result<String, String> {
    let mut repeats = Repeats::default();
    let none = HashSet::new();
    for step in 1..=b.limits.max_steps {
        b.steps.store(step, Ordering::Relaxed);
        b.emit("step.start", json!({ "step": step }));
        let reply = match model_step(b, step, &messages).await {
            Outcome::Done(r) => r,
            Outcome::Stopped(_) => return Err(STOPPED.into()),
            Outcome::Failed(e) => return Err(format!("The branch failed: {}", error_text(&e))),
        };
        b.add_usage(&reply.usage);
        b.emit("message.end", json!({ "step": step, "message": reply.message, "usage": reply.usage, "stop_reason": reply.stop_reason }));
        let calls = calls_of(&reply.message);
        if calls.is_empty() {
            let text = reply.message["content"].as_array().into_iter().flatten();
            let text: Vec<&str> = text.filter_map(|c| c["text"].as_str()).collect();
            return Ok(text.join("\n"));
        }
        messages.push(reply.message.to_string());
        let chat_step = b.chat_step.fetch_add(1, Ordering::Relaxed) + 1;
        let mut results = run_batch(b, step, &calls, None).await;
        repeats.apply(chat_step, &calls, &mut results, &none);
        messages.push(json!({ "role": "user", "content": results }).to_string());
    }
    Err(format!(
        "Not finished: the branch used all its {} steps.",
        b.limits.max_steps
    ))
}

/// Runs one step's calls: neighbouring parallel calls at once, the rest
/// alone, in call order. Results come back in call order.
async fn run_batch(
    t: &Arc<Turn>,
    step: u64,
    calls: &[Call],
    fork: Option<Arc<Fork>>,
) -> Vec<Value> {
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
            let call = group[0].clone();
            out.push(finish(t.clone(), step, call, first, fork.clone()).await);
        } else {
            // Every call's first request goes out in call order; then they run at once.
            let firsts: Vec<Pending> = group.iter().map(|c| begin(t, step, c)).collect();
            let running: Vec<_> = group
                .iter()
                .zip(firsts)
                .map(|(c, p)| tokio::spawn(finish(t.clone(), step, c.clone(), p, fork.clone())))
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
    let known =
        t.native.contains(&c.name.as_str()) || t.host_tools.iter().any(|h| h.name == c.name);
    if known && t.branch.is_some() && matches!(c.name.as_str(), "Edit" | "Write" | "Branch") {
        return Pending::Done(error_result(&c.id, NOT_IN_BRANCH));
    }
    if known && !t.parallel(&c.name) && t.steer_waiting() {
        return Pending::Done(error_result(&c.id, NOT_RUN));
    }
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

async fn finish(
    t: Arc<Turn>,
    step: u64,
    c: Call,
    first: Pending,
    fork: Option<Arc<Fork>>,
) -> Value {
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
    if !t.parallel(&c.name) && t.steer_waiting() {
        return error_result(&c.id, NOT_RUN);
    }
    t.emit("tool.start", json!({ "step": step, "call": { "id": c.id, "name": c.name, "input": input }, "native": true }));
    let started = Instant::now();
    let mut stop = t.stop.clone();
    // Stop ends the wait for any tool; Bash, polled first, handles Stop itself.
    let out = tokio::select! {
        biased;
        out = run_call(&t, &c, &input, fork) => out,
        _ = stop.wait_for(|s| *s) => tools::Out::error(STOPPED),
    };
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

/// A native tool; Branch runs a branch (boxed: a branch runs calls itself).
fn run_call<'a>(
    t: &'a Arc<Turn>,
    c: &'a Call,
    input: &'a Value,
    fork: Option<Arc<Fork>>,
) -> Pin<Box<dyn Future<Output = tools::Out> + Send + 'a>> {
    match fork {
        Some(p) if c.name == "Branch" => Box::pin(branch(t, &c.id, input, p)),
        _ => Box::pin(tools::run(t, &c.id, &c.name, input)),
    }
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
        if let Some(b) = &self.branch {
            m.insert("branch".into(), b.clone().into());
        }
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

    fn steer_waiting(&self) -> bool {
        *self.waiting.borrow() > 0
    }

    /// Steers and notes for a step boundary; the steers count as taken until delivered.
    fn take_inbox(&self) -> Inbox {
        let inbox = self.session.take_inbox(&self.id);
        lock(&self.taken).extend(inbox.steers.iter().map(|s| s.id.clone()));
        inbox
    }

    fn parallel(&self, name: &str) -> bool {
        (matches!(name, "Read" | "Grep" | "Glob" | "Branch") && self.native.contains(&name))
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
        if !inbox.notes.is_empty() {
            let mut st = self.session.lock();
            let noted = |id: &String| inbox.notes.iter().any(|(n, _)| n == id);
            for k in st.tasks.iter_mut().filter(|k| noted(&k.id)) {
                k.announced = true;
            }
        }
        let mut taken = lock(&self.taken);
        for Steer { id, .. } in &inbox.steers {
            self.emit("steer.delivered", json!({ "id": id }));
            taken.retain(|t| t != id);
        }
    }

    /// One fade marker (when anything fades); the faded Reads' lines are no
    /// longer "above".
    fn apply_fade(&self, wire: &mut Wire, f: context::Fade) {
        if f.stubs.is_empty() {
            return;
        }
        let mut v = self.versions();
        for (path, step) in &f.reads {
            v.forget(path, *step);
        }
        drop(v);
        wire.push_fade(f.stubs);
    }

    /// A branch of this turn for the Branch call `call_id`: the same model,
    /// endpoint, request, system and tools, its own steps and `versions`, a
    /// copy of the version table (never saved).
    fn branch_of(&self, call_id: &str, versions: Versions) -> Turn {
        Turn {
            host: self.host.clone(),
            session: self.session.clone(),
            id: self.id.clone(),
            cwd: self.cwd.clone(),
            env: self.env.clone(),
            limits: self.limits.clone(),
            stop: self.stop.clone(),
            waiting: self.waiting.clone(),
            chat_step: AtomicU64::new(self.chat_step.load(Ordering::Relaxed).saturating_sub(1)),
            model: self.model.clone(),
            system: self.system.clone(),
            request: self.request.clone(),
            endpoint: self.endpoint.clone(),
            tools: self.tools.clone(),
            native: self.native.clone(),
            host_tools: self.host_tools.clone(),
            cache: self.cache.clone(),
            ctx: self.ctx.clone(),
            branch: Some(call_id.to_string()),
            versions: OnceLock::from(Mutex::new(versions)),
            steps: AtomicU64::new(0),
            usage: self.usage.clone(),
            cursor: Mutex::new(None),
            taken: Mutex::new(Vec::new()),
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
                let steers = std::mem::take(&mut ctl.steers);
                lock(&self.taken).extend(steers.iter().map(|s| s.id.clone()));
                return Some(Inbox {
                    notes: std::mem::take(&mut ctl.notes),
                    steers,
                });
            }
        }
        self.finish(End::new("done"));
        None
    }

    /// Sends `turn.end`; every accepted steer not delivered (taken from the
    /// queue or still in it) is listed as undelivered.
    fn finish(&self, end: End) {
        self.save_versions();
        let mut st = self.session.lock();
        let mut undelivered: Vec<Value> = std::mem::take(&mut *lock(&self.taken));
        if let Some(ctl) = st.turn.take_if(|c| c.id == self.id) {
            undelivered.extend(ctl.steers.into_iter().map(|s| s.id));
        }
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

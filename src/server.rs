//! `shadow serve`: protocol v1 over stdin/stdout. Reads one request per
//! line and answers it; turns, task stops and compactions run in the
//! background while more lines are read.

use crate::compact;
use crate::rpc::Host;
use crate::session::{Session, Steer, TaskState, valid_id};
use crate::tools::{self, StopOutcome};
use crate::turn;
use crate::wire::Wire;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};

type RpcResult = Result<Value, (&'static str, String)>;

pub fn serve() -> i32 {
    match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt.block_on(run()),
        Err(e) => {
            eprintln!("shadow: cannot start the runtime: {e}");
            1
        }
    }
}

struct Daemon {
    host: Host,
    data_dir: OnceLock<PathBuf>,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
}

async fn run() -> i32 {
    let d = Arc::new(Daemon {
        host: Host::start(),
        data_dir: OnceLock::new(),
        sessions: Mutex::default(),
    });
    let mut stdin = BufReader::new(tokio::io::stdin());
    let mut line = Vec::new();
    loop {
        line.clear();
        match stdin.read_until(b'\n', &mut line).await {
            Ok(0) => break,
            Ok(_) => {
                if d.line(&line).await {
                    return 0;
                }
            }
            Err(e) => {
                eprintln!("shadow: reading stdin: {e}");
                break;
            }
        }
    }
    // stdin closed: the host is gone; end as on shutdown.
    d.shutdown().await;
    0
}

fn bad(m: impl Into<String>) -> (&'static str, String) {
    ("bad_request", m.into())
}

impl Daemon {
    /// Handles one line; true once `shutdown` is done.
    async fn line(self: &Arc<Self>, raw: &[u8]) -> bool {
        let Ok(text) = std::str::from_utf8(raw) else {
            eprintln!("shadow: a line that is not UTF-8 was ignored");
            return false;
        };
        let text = text.trim();
        if text.is_empty() {
            return false;
        }
        let msg: Map<String, Value> = match serde_json::from_str(text) {
            Ok(Value::Object(m)) => m,
            _ => {
                eprintln!(
                    "shadow: not a JSON object, ignored: {}",
                    text.chars().take(200).collect::<String>()
                );
                return false;
            }
        };
        let Some(method) = msg.get("method").and_then(Value::as_str) else {
            if msg.contains_key("id") {
                self.host.on_reply(&msg);
            } else {
                eprintln!("shadow: a line with neither method nor id was ignored");
            }
            return false;
        };
        let Some(id) = msg.get("id").cloned() else {
            eprintln!("shadow: notification {method:?} ignored (the host sends requests)");
            return false;
        };
        let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
        if method == "shutdown" && self.data_dir.get().is_some() {
            self.host.reply(id, json!({}));
            self.shutdown().await;
            return true;
        }
        self.dispatch(id, method, params);
        false
    }

    fn reply(&self, id: Value, r: RpcResult) {
        match r {
            Ok(v) => self.host.reply(id, v),
            Err((code, msg)) => self.host.reply_error(id, code, &msg),
        }
    }

    fn dispatch(self: &Arc<Self>, id: Value, method: &str, params: Value) {
        if method == "initialize" {
            return self.reply(id, self.initialize(&params));
        }
        if self.data_dir.get().is_none() {
            return self.reply(id, Err(("not_initialized", "send initialize first".into())));
        }
        let session = match self.session(&params) {
            Ok(s) => s,
            Err(e)
                if matches!(
                    method,
                    "session.info"
                        | "turn.start"
                        | "turn.steer"
                        | "turn.stop"
                        | "task.list"
                        | "task.stop"
                        | "session.compact"
                ) =>
            {
                return self.reply(id, Err(e));
            }
            Err(_) => {
                return self.reply(id, Err(("unknown_method", format!("no method {method:?}"))));
            }
        };
        match method {
            "session.info" => self.reply(id, info(&session)),
            "turn.start" => match turn::start(&self.host, session, params) {
                Ok((t, start)) => {
                    self.host.reply(id, json!({ "turn": t.id }));
                    turn::spawn(t, start);
                }
                Err((code, msg)) => self.reply(id, Err((code, msg))),
            },
            "turn.steer" => self.reply(id, steer(&session, &params)),
            "turn.stop" => {
                if let Some(ctl) = session.lock().turn.as_ref() {
                    ctl.stop.send_replace(true);
                }
                self.reply(id, Ok(json!({})));
            }
            "task.list" => {
                let tasks: Vec<Value> = session
                    .lock()
                    .tasks
                    .iter()
                    .filter(|t| t.background)
                    .map(|t| t.json())
                    .collect();
                self.reply(id, Ok(json!({ "tasks": tasks })));
            }
            "task.stop" => {
                let Some(task) = params
                    .get("task")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                else {
                    return self.reply(id, Err(bad("task is required")));
                };
                let d = self.clone();
                tokio::spawn(async move {
                    let stopped = matches!(
                        tools::stop_task(&session, &task, false).await,
                        StopOutcome::Stopped(_)
                    );
                    d.reply(id, Ok(json!({ "stopped": stopped })));
                });
            }
            "session.compact" => {
                let d = self.clone();
                tokio::spawn(async move {
                    let r = compact::compact(&d.host, session, params).await;
                    d.reply(id, r);
                });
            }
            _ => self.reply(id, Err(("unknown_method", format!("no method {method:?}")))),
        }
    }

    fn initialize(&self, p: &Value) -> RpcResult {
        if self.data_dir.get().is_some() {
            return Err(bad("already initialized"));
        }
        if p.get("protocol").and_then(Value::as_u64) != Some(1) {
            return Err(bad(format!(
                "this daemon speaks protocol 1, not {}",
                p.get("protocol").unwrap_or(&Value::Null)
            )));
        }
        let dir = p
            .get("data_dir")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .filter(|d| d.is_absolute())
            .ok_or_else(|| bad("data_dir must be an absolute path"))?;
        std::fs::create_dir_all(dir.join("sessions"))
            .map_err(|e| bad(format!("cannot create {}: {e}", dir.display())))?;
        if let Some(c) = p.get("client") {
            eprintln!("shadow: serving {c}");
        }
        self.data_dir.get_or_init(|| dir);
        Ok(
            json!({ "protocol": 1, "version": env!("CARGO_PKG_VERSION"), "pid": std::process::id() }),
        )
    }

    fn session(&self, p: &Value) -> Result<Arc<Session>, (&'static str, String)> {
        let id = p
            .get("session")
            .and_then(Value::as_str)
            .ok_or_else(|| bad("session is required"))?;
        if !valid_id(id) {
            return Err(bad(
                "session must be letters, digits, '.', '_' or '-', not starting with '.'",
            ));
        }
        let dir = self
            .data_dir
            .get()
            .ok_or(("not_initialized", "send initialize first".to_string()))?;
        let mut map = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let s = map.entry(id.to_string()).or_insert_with(|| {
            Arc::new(Session::new(id.to_string(), dir.join("sessions").join(id)))
        });
        Ok(s.clone())
    }

    /// Stops running turns, then every running command; flushes stdout.
    async fn shutdown(&self) {
        let sessions: Vec<Arc<Session>> = self
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        for s in &sessions {
            if let Some(ctl) = s.lock().turn.as_ref() {
                ctl.stop.send_replace(true);
            }
        }
        let until = tokio::time::Instant::now() + Duration::from_secs(3);
        while tokio::time::Instant::now() < until
            && sessions.iter().any(|s| s.lock().turn.is_some())
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tools::kill_all(sessions).await;
        self.host.flush().await;
    }
}

fn info(s: &Session) -> RpcResult {
    let wire = Wire::load(&s.wire_path()).map_err(|e| ("io_error", e.to_string()))?;
    let st = s.lock();
    Ok(json!({
        "exists": wire.exists,
        "cursor": wire.cursor,
        "messages": wire.messages.len(),
        "last_model_call_at": wire.last_model_call_at,
        "running": st.turn.is_some(),
        "tasks": st.tasks.iter().filter(|t| t.background && t.state == TaskState::Running).count(),
    }))
}

fn steer(s: &Session, p: &Value) -> RpcResult {
    let id = p
        .get("id")
        .filter(|v| v.is_string() || v.is_number())
        .cloned()
        .ok_or_else(|| bad("id is required (a string or a number)"))?;
    let input = p
        .get("input")
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty())
        .cloned()
        .ok_or_else(|| bad("input must be a non-empty list of content blocks"))?;
    let mut st = s.lock();
    let Some(ctl) = st.turn.as_mut() else {
        return Err(("not_running", "no turn is running in this chat".into()));
    };
    ctl.steers.push(Steer { id, input });
    ctl.waiting.send_replace(ctl.steers.len());
    Ok(json!({}))
}

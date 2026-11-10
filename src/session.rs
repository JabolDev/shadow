//! One chat: its folder, the running turn's controls, and its shell tasks.
//! A sleeping chat is only its folder; this struct holds what lives in the
//! process (the running turn, background commands).

use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::sync::watch;

pub struct Session {
    pub id: String,
    pub dir: PathBuf,
    state: Mutex<State>,
}

#[derive(Default)]
pub struct State {
    pub turn: Option<TurnCtl>,
    pub compacting: bool,
    /// Commands started by Bash, in start order (finished foreground ones are removed).
    pub tasks: Vec<Task>,
    /// The tool definitions of the latest turn (sent again by compaction).
    pub last_tools: Option<Arc<Vec<Value>>>,
}

/// The host's handles on a running turn.
pub struct TurnCtl {
    pub id: String,
    pub stop: watch::Sender<bool>,
    /// The number of steer messages waiting (a foreground command watches it).
    pub waiting: watch::Sender<usize>,
    pub steers: Vec<Steer>,
    /// "Background task … ended" lines for the next step boundary.
    pub notes: Vec<String>,
}

pub struct Steer {
    pub id: Value,
    pub input: Vec<Value>,
}

/// What a step boundary hands to the model besides tool results.
#[derive(Default)]
pub struct Inbox {
    pub notes: Vec<String>,
    pub steers: Vec<Steer>,
}

impl Inbox {
    /// Notes (one text block each), then every steer's blocks in arrival order.
    pub fn blocks(&self) -> Vec<Value> {
        let mut out: Vec<Value> = self
            .notes
            .iter()
            .map(|n| json!({ "type": "text", "text": n }))
            .collect();
        for s in &self.steers {
            out.extend(s.input.iter().cloned());
        }
        out
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TaskState {
    Running,
    Exited,
    Stopped,
}

impl TaskState {
    pub fn name(self) -> &'static str {
        match self {
            TaskState::Running => "running",
            TaskState::Exited => "exited",
            TaskState::Stopped => "stopped",
        }
    }
}

pub struct Task {
    pub id: String,
    pub command: String,
    pub pgid: i32,
    pub started_at: u64,
    pub output: PathBuf,
    pub state: TaskState,
    pub exit_code: Option<i32>,
    /// It became a background task (task.list, task.update, notes).
    pub background: bool,
    pub stop_requested: bool,
    /// Stopped by the model's TaskStop: it already has the outcome, no note.
    pub stopped_by_tool: bool,
    /// Turns true when the process has exited.
    pub done: watch::Receiver<bool>,
}

impl Task {
    pub fn json(&self) -> Value {
        json!({
            "id": self.id,
            "command": self.command,
            "state": self.state.name(),
            "exit_code": self.exit_code,
            "started_at": self.started_at,
            "output_path": self.output,
        })
    }
}

impl Session {
    pub fn new(id: String, dir: PathBuf) -> Session {
        Session {
            id,
            dir,
            state: Mutex::new(State::default()),
        }
    }

    pub fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn wire_path(&self) -> PathBuf {
        self.dir.join("wire.jsonl")
    }

    pub fn versions_path(&self) -> PathBuf {
        self.dir.join("versions.json")
    }

    pub fn scratch(&self) -> PathBuf {
        self.dir.join("scratch")
    }

    /// Steers and notes waiting for turn `turn`, taken out of the queue.
    pub fn take_inbox(&self, turn: &str) -> Inbox {
        let mut st = self.lock();
        match st.turn.as_mut().filter(|t| t.id == turn) {
            Some(t) => {
                t.waiting.send_replace(0);
                Inbox {
                    notes: std::mem::take(&mut t.notes),
                    steers: std::mem::take(&mut t.steers),
                }
            }
            None => Inbox::default(),
        }
    }
}

/// True when `id` can name a folder: letters, digits, ".", "_", "-"; not starting with ".".
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 200
        && !id.starts_with('.')
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A random 16-hex-digit id (std's per-process random hash keys).
pub fn random_id() -> String {
    use std::hash::BuildHasher;
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let h =
        std::collections::hash_map::RandomState::new().hash_one((n, now_ms(), std::process::id()));
    format!("{h:016x}")
}

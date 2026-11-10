//! Bash and TaskStop. Every command runs with `/bin/bash -c` in the chat's
//! folder, in its own process group, with stdout and stderr in one log file.
//! The wait limit never kills: a command still running then goes on as a
//! background task.

use super::{Cx, Out, bound_file, opt_bool, opt_num, opt_str, req_str};
use crate::rpc::Host;
use crate::session::{Session, Task, TaskState, now_ms, random_id};
use crate::turn::STOPPED;
use serde_json::{Map, Value, json};
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

const MAX_WAIT_MS: u64 = 600_000;
const LAST_LINES: usize = 20;
const TERM_GRACE: Duration = Duration::from_secs(2);
/// On Stop the foreground command gets this long after TERM before KILL.
const STOP_GRACE: Duration = Duration::from_millis(500);

pub async fn bash(cx: &Cx, input: &Value) -> Out {
    run(cx, input).await.unwrap_or_else(Out::error)
}

enum Wait {
    Ended,
    Background,
    Stopped,
}

async fn run(cx: &Cx, input: &Value) -> Result<Out, String> {
    let t = &cx.turn;
    let command = req_str(input, "command")?;
    opt_str(input, "description")?;
    let wait_ms = match opt_num(input, "timeout")? {
        Some(ms) => (ms.max(1) as u64).min(MAX_WAIT_MS),
        None => t.limits.bash_wait_ms,
    };
    let background_now = opt_bool(input, "run_in_background")?;

    let id = format!("b{}", &random_id()[..10]);
    let dir = t.session.scratch().join("tasks");
    fs::create_dir_all(&dir).map_err(|e| format!("Cannot create {}: {e}", dir.display()))?;
    let log_path = dir.join(format!("{id}.log"));
    let log = File::create(&log_path)
        .map_err(|e| format!("Cannot create {}: {e}", log_path.display()))?;
    let log2 = log
        .try_clone()
        .map_err(|e| format!("Cannot open {}: {e}", log_path.display()))?;
    let mut cmd = tokio::process::Command::new("/bin/bash");
    cmd.arg("-c")
        .arg(command)
        .current_dir(&t.cwd)
        .envs(t.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log2))
        .process_group(0);
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Could not start /bin/bash in {}: {e}", t.cwd.display()))?;
    let pgid = child
        .id()
        .ok_or("The command ended before it could be tracked.")? as i32;
    let (done_tx, done) = watch::channel(false);
    t.session.lock().tasks.push(Task {
        id: id.clone(),
        command: command.to_string(),
        pgid,
        started_at: now_ms(),
        output: log_path.clone(),
        state: TaskState::Running,
        exit_code: None,
        background: false,
        stop_requested: false,
        stopped_by_tool: false,
        done: done.clone(),
    });
    let (host, session, tid) = (t.host.clone(), t.session.clone(), id.clone());
    tokio::spawn(async move {
        let code = match child.wait().await {
            Ok(s) => s.code().unwrap_or_else(|| 128 + s.signal().unwrap_or(0)),
            Err(e) => {
                eprintln!("shadow: waiting for task {tid}: {e}");
                -1
            }
        };
        exited(&host, &session, &tid, code);
        done_tx.send_replace(true);
    });

    let (mut done, mut stop, mut waiting) = (done, t.stop.clone(), t.waiting.clone());
    let why = if background_now {
        Wait::Background
    } else {
        tokio::select! {
            biased;
            _ = done.wait_for(|d| *d) => Wait::Ended,
            _ = stop.wait_for(|s| *s) => Wait::Stopped,
            _ = waiting.wait_for(|n| *n > 0) => Wait::Background,
            _ = tokio::time::sleep(Duration::from_millis(wait_ms)) => Wait::Background,
        }
    };
    let so_far = |cx: &Cx| {
        bound_file(&log_path, cx.cap(), &cx.overflow_path())
            .map_err(|e| format!("Cannot read {}: {e}", log_path.display()))
    };
    match why {
        Wait::Stopped => {
            if let Some(task) = t.session.lock().tasks.iter_mut().find(|k| k.id == id) {
                task.stop_requested = true;
            }
            tokio::spawn(end_group(pgid, STOP_GRACE));
            let mut b = so_far(cx)?;
            b.text = format!("{}\n{STOPPED}", b.text.trim_end())
                .trim_start()
                .to_string();
            Ok(Out::bounded(b, true))
        }
        Wait::Background if to_background(&t.host, &t.session, &id) => {
            let mut b = so_far(cx)?;
            b.text = format!(
                "{}\nStill running in the background as task {id}. Output: {}. You will be told when it ends.",
                b.text.trim_end(),
                log_path.display()
            )
            .trim_start()
            .to_string();
            Ok(Out {
                task_id: Some(id),
                ..Out::bounded(b, false)
            })
        }
        Wait::Ended | Wait::Background => {
            let code = {
                let mut st = t.session.lock();
                let code = st
                    .tasks
                    .iter()
                    .find(|k| k.id == id)
                    .and_then(|k| k.exit_code)
                    .unwrap_or(-1);
                st.tasks.retain(|k| k.id != id);
                code
            };
            let mut b = so_far(cx)?;
            let body = b.text.trim_end();
            b.text = match (code, body.is_empty()) {
                (0, true) => "(no output)".into(),
                (0, false) => body.to_string(),
                (_, true) => format!("Exit code {code}"),
                (_, false) => format!("{body}\nExit code {code}"),
            };
            Ok(Out {
                exit_code: Some(code),
                ..Out::bounded(b, code != 0)
            })
        }
    }
}

/// Makes a running command a background task; false when it already ended.
fn to_background(host: &Host, session: &Session, id: &str) -> bool {
    let mut st = session.lock();
    let turn = st.turn.as_ref().map(|c| c.id.clone());
    let Some(task) = st
        .tasks
        .iter_mut()
        .find(|k| k.id == id && k.state == TaskState::Running)
    else {
        return false;
    };
    task.background = true;
    notify_update(host, &session.id, turn.as_deref(), task);
    true
}

/// The command's process ended with `code`.
fn exited(host: &Host, session: &Session, id: &str, code: i32) {
    let mut st = session.lock();
    let turn = st.turn.as_ref().map(|c| c.id.clone());
    let Some(task) = st.tasks.iter_mut().find(|k| k.id == id) else {
        return;
    };
    task.state = if task.stop_requested {
        TaskState::Stopped
    } else {
        TaskState::Exited
    };
    task.exit_code = Some(code);
    if !task.background {
        // A foreground command killed by Stop has nobody left to read it.
        if task.stop_requested {
            st.tasks.retain(|k| k.id != id);
        }
        return;
    }
    notify_update(host, &session.id, turn.as_deref(), task);
    if !task.stopped_by_tool {
        let first = task.command.lines().next().unwrap_or("");
        let short: String = if first.chars().count() > 100 || task.command.contains('\n') {
            first.chars().take(100).collect::<String>() + "…"
        } else {
            first.to_string()
        };
        let note = format!(
            "Background task {} ({short}) ended: exit {code}. Output: {}",
            task.id,
            task.output.display()
        );
        if let Some(ctl) = st.turn.as_mut() {
            ctl.notes.push(note);
        }
    }
}

fn notify_update(host: &Host, session: &str, turn: Option<&str>, task: &Task) {
    let mut p = Map::new();
    p.insert("session".into(), session.into());
    if let Some(t) = turn {
        p.insert("turn".into(), t.into());
    }
    let mut info = json!({ "id": task.id, "command": task.command, "state": task.state.name(), "output_path": task.output });
    if let Some(c) = task.exit_code {
        info["exit_code"] = c.into();
    }
    p.insert("task".into(), info);
    host.notify("task.update", Value::Object(p));
}

/// TERM to the process group, KILL after `grace` while any of it is left.
async fn end_group(pgid: i32, grace: Duration) {
    signal_group(pgid, libc::SIGTERM);
    let until = tokio::time::Instant::now() + grace;
    while tokio::time::Instant::now() < until {
        tokio::time::sleep(Duration::from_millis(25)).await;
        if !signal_group(pgid, 0) {
            return;
        }
    }
    signal_group(pgid, libc::SIGKILL);
}

/// Sends `sig` to the process group; false when the group has no process.
fn signal_group(pgid: i32, sig: libc::c_int) -> bool {
    // SAFETY: kill(2) only reads its two integer arguments; a negative pid names the process group.
    if unsafe { libc::kill(-pgid, sig) } == 0 {
        return true;
    }
    let err = io::Error::last_os_error();
    if err.raw_os_error() != Some(libc::ESRCH) {
        eprintln!("shadow: signal {sig} to process group {pgid}: {err}");
    }
    false
}

pub enum StopOutcome {
    NotFound,
    AlreadyEnded(TaskState, Option<i32>),
    Stopped(Option<i32>),
    Stuck,
}

/// Ends a background task's process group: TERM, then KILL after 2 s.
pub async fn stop_task(session: &Session, id: &str, by_tool: bool) -> StopOutcome {
    let (pgid, mut done) = {
        let mut st = session.lock();
        let Some(task) = st.tasks.iter_mut().find(|k| k.id == id && k.background) else {
            return StopOutcome::NotFound;
        };
        if task.state != TaskState::Running {
            return StopOutcome::AlreadyEnded(task.state, task.exit_code);
        }
        task.stop_requested = true;
        task.stopped_by_tool = by_tool;
        (task.pgid, task.done.clone())
    };
    end_group(pgid, TERM_GRACE).await;
    if tokio::time::timeout(TERM_GRACE, done.wait_for(|d| *d))
        .await
        .is_err()
    {
        return StopOutcome::Stuck;
    }
    StopOutcome::Stopped(
        session
            .lock()
            .tasks
            .iter()
            .find(|k| k.id == id)
            .and_then(|k| k.exit_code),
    )
}

pub async fn task_stop(cx: &Cx, input: &Value) -> Out {
    let id = match req_str(input, "task_id") {
        Ok(id) => id,
        Err(e) => return Out::error(e),
    };
    let session = &cx.turn.session;
    let output = session
        .lock()
        .tasks
        .iter()
        .find(|k| k.id == id)
        .map(|k| k.output.clone());
    let head = match stop_task(session, id, true).await {
        StopOutcome::NotFound => return Out::error(format!("No background task {id}.")),
        StopOutcome::Stuck => {
            return Out::error(format!(
                "Task {id} got TERM and KILL but has not exited yet."
            ));
        }
        StopOutcome::AlreadyEnded(state, code) => format!(
            "Task {id} had already {} (exit {}).",
            if state == TaskState::Stopped {
                "been stopped"
            } else {
                "ended"
            },
            code.unwrap_or(-1)
        ),
        StopOutcome::Stopped(code) => format!("Stopped task {id} (exit {}).", code.unwrap_or(-1)),
    };
    let tail = output
        .as_deref()
        .map(last_lines)
        .unwrap_or_else(|| Ok(String::new()));
    match tail {
        Ok(t) if t.trim().is_empty() => Out::text(format!("{head} It wrote no output.")),
        Ok(t) => match cx.bound(&format!("{head} Last lines of its output:\n{t}")) {
            Ok(b) => Out::bounded(b, false),
            Err(e) => Out::error(format!("{head} Its output could not be shown: {e}")),
        },
        Err(e) => Out::error(format!("{head} Its output could not be read: {e}")),
    }
}

/// The last 20 lines of a log file.
fn last_lines(path: &Path) -> io::Result<String> {
    let mut f = File::open(path)?;
    let size = f.metadata()?.len();
    let from = size.saturating_sub(64 * 1024);
    f.seek(SeekFrom::Start(from))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf);
    let lines: Vec<&str> = text.trim_end().lines().collect();
    Ok(lines[lines.len().saturating_sub(LAST_LINES)..].join("\n"))
}

/// Shutdown: ends every running command of every chat (TERM, then KILL
/// after 2 s) and waits for them.
pub async fn kill_all(sessions: Vec<Arc<Session>>) {
    let mut running = Vec::new();
    for s in sessions {
        for task in s
            .lock()
            .tasks
            .iter_mut()
            .filter(|k| k.state == TaskState::Running)
        {
            task.stop_requested = true;
            let (pgid, mut done) = (task.pgid, task.done.clone());
            running.push(tokio::spawn(async move {
                end_group(pgid, TERM_GRACE).await;
                tokio::time::timeout(TERM_GRACE, done.wait_for(|d| *d))
                    .await
                    .is_ok()
            }));
        }
    }
    for h in running {
        match h.await {
            Ok(true) => {}
            Ok(false) => eprintln!("shadow: a command did not exit after TERM and KILL"),
            Err(e) => eprintln!("shadow: stopping a command at shutdown: {e}"),
        }
    }
}

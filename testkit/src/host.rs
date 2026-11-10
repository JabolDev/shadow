//! A scripted host: starts the daemon, sends it protocol lines, answers the
//! daemon's requests (tool.check, tool.call, tool.end) by rules, and keeps
//! every line it receives.
//!
//! A rule: {"method": "tool.check", "match": {…}, "reply": {…}, "error": {…}, "delay_ms": n}
//! — the first rule whose method is the request's and whose `match` is a
//! subset of its params answers it. Without a rule: tool.check allows,
//! tool.end gets {}, anything else gets an error.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// True when every field of `pattern` is in `value` (objects: recursively;
/// anything else: equal).
pub fn subset(pattern: &Value, value: &Value) -> bool {
    match (pattern, value) {
        (Value::Object(p), Value::Object(v)) => p
            .iter()
            .all(|(k, pv)| v.get(k).is_some_and(|vv| subset(pv, vv))),
        _ => pattern == value,
    }
}

#[derive(Default)]
struct Lines {
    all: Vec<(Instant, Value)>,
    closed: bool,
}

pub struct ScriptedHost {
    command: Vec<String>,
    child: Option<Child>,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    lines: Arc<(Mutex<Lines>, Condvar)>,
    rules: Arc<Mutex<Vec<Value>>>,
    echo: bool,
    next_id: u64,
    /// `wait` looks at lines from here on.
    pub cursor: usize,
}

impl ScriptedHost {
    /// Starts `command`; with `echo`, prints every received line to stdout.
    pub fn start(
        command: &[String],
        rules: Vec<Value>,
        echo: bool,
    ) -> std::io::Result<ScriptedHost> {
        let mut h = ScriptedHost {
            command: command.to_vec(),
            child: None,
            stdin: Arc::default(),
            lines: Arc::default(),
            rules: Arc::new(Mutex::new(rules)),
            echo,
            next_id: 1,
            cursor: 0,
        };
        h.spawn()?;
        Ok(h)
    }

    fn spawn(&mut self) -> std::io::Result<()> {
        let mut child = Command::new(&self.command[0])
            .args(&self.command[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let out = child.stdout.take().expect("piped");
        *self.stdin.lock().unwrap() = child.stdin.take();
        self.lines.0.lock().unwrap().closed = false;
        self.child = Some(child);
        let (lines, stdin, rules, echo) = (
            self.lines.clone(),
            self.stdin.clone(),
            self.rules.clone(),
            self.echo,
        );
        thread::spawn(move || {
            for line in BufReader::new(out).lines() {
                let Ok(line) = line else { break };
                if echo {
                    println!("{line}");
                }
                let v: Value =
                    serde_json::from_str(&line).unwrap_or_else(|_| json!({ "unparsed": line }));
                if v.get("id").is_some() && v.get("method").is_some() {
                    answer(&v, &rules, &stdin);
                }
                let (m, cv) = &*lines;
                m.lock().unwrap().all.push((Instant::now(), v));
                cv.notify_all();
            }
            let (m, cv) = &*lines;
            m.lock().unwrap().closed = true;
            cv.notify_all();
        });
        Ok(())
    }

    pub fn add_rules(&self, rules: impl IntoIterator<Item = Value>) {
        let mut r = self.rules.lock().unwrap();
        let new: Vec<Value> = rules.into_iter().collect();
        r.splice(0..0, new);
    }

    /// Writes one line to the daemon. A line with a method and no id gets the next id.
    pub fn send(&mut self, mut line: Value) -> Value {
        if line.get("method").is_some() && line.get("id").is_none() {
            line["id"] = json!(format!("h{}", self.next_id));
            self.next_id += 1;
        }
        write_line(&self.stdin, &line);
        line["id"].clone()
    }

    /// Sends a request and waits for its reply.
    pub fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.send(json!({ "method": method, "params": params }));
        self.wait_any(&json!({ "id": id }), Duration::from_secs(30))
            .unwrap_or_else(|e| panic!("{method}: {e}"))
    }

    /// The next line (from the cursor on) that contains `pattern`; moves the cursor past it.
    pub fn wait(&mut self, pattern: &Value, timeout: Duration) -> Result<Value, String> {
        let (i, v) = self.find(pattern, self.cursor, timeout)?;
        self.cursor = i + 1;
        Ok(v)
    }

    /// Like `wait`, from the first line on, cursor unchanged.
    pub fn wait_any(&self, pattern: &Value, timeout: Duration) -> Result<Value, String> {
        self.find(pattern, 0, timeout).map(|(_, v)| v)
    }

    fn find(
        &self,
        pattern: &Value,
        from: usize,
        timeout: Duration,
    ) -> Result<(usize, Value), String> {
        let until = Instant::now() + timeout;
        let (m, cv) = &*self.lines;
        let mut l = m.lock().unwrap();
        loop {
            if let Some(i) = (from..l.all.len()).find(|&i| subset(pattern, &l.all[i].1)) {
                return Ok((i, l.all[i].1.clone()));
            }
            let now = Instant::now();
            if now >= until {
                return Err(format!("no line like {pattern} within {timeout:?}"));
            }
            if l.closed && l.all.len() <= from.max(l.all.len()) && self.child.is_none() {
                return Err(format!("the daemon is gone; no line like {pattern}"));
            }
            l = cv.wait_timeout(l, until - now).unwrap().0;
        }
    }

    /// Every line received so far.
    pub fn lines(&self) -> Vec<Value> {
        self.lines
            .0
            .lock()
            .unwrap()
            .all
            .iter()
            .map(|(_, v)| v.clone())
            .collect()
    }

    /// When each line was received.
    pub fn times(&self) -> Vec<Instant> {
        self.lines
            .0
            .lock()
            .unwrap()
            .all
            .iter()
            .map(|(t, _)| *t)
            .collect()
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(Child::id)
    }

    /// SIGKILL to the daemon.
    pub fn kill(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        *self.stdin.lock().unwrap() = None;
    }

    /// Starts the daemon again; the cursor moves to the end of what was received.
    pub fn restart(&mut self) -> std::io::Result<()> {
        self.kill();
        self.cursor = self.lines.0.lock().unwrap().all.len();
        self.spawn()
    }

    /// Closes the daemon's stdin and waits for it to exit.
    pub fn close(&mut self) -> Option<i32> {
        *self.stdin.lock().unwrap() = None;
        let mut c = self.child.take()?;
        let until = Instant::now() + Duration::from_secs(10);
        while Instant::now() < until {
            if let Ok(Some(s)) = c.try_wait() {
                return s.code();
            }
            thread::sleep(Duration::from_millis(20));
        }
        let _ = c.kill();
        c.wait().ok().and_then(|s| s.code())
    }
}

impl Drop for ScriptedHost {
    fn drop(&mut self) {
        self.kill();
    }
}

fn write_line(stdin: &Mutex<Option<ChildStdin>>, line: &Value) {
    if let Some(s) = stdin.lock().unwrap().as_mut() {
        let _ = writeln!(s, "{line}").and_then(|_| s.flush());
    }
}

fn answer(req: &Value, rules: &Mutex<Vec<Value>>, stdin: &Arc<Mutex<Option<ChildStdin>>>) {
    let method = req["method"].as_str().unwrap_or("");
    let params = &req["params"];
    let rule = rules
        .lock()
        .unwrap()
        .iter()
        .find(|r| r["method"] == method && r.get("match").is_none_or(|m| subset(m, params)))
        .cloned();
    let reply = match (&rule, method) {
        (Some(r), _) if r.get("error").is_some() => json!({ "id": req["id"], "error": r["error"] }),
        (Some(r), _) => {
            json!({ "id": req["id"], "result": r.get("reply").cloned().unwrap_or_else(|| json!({})) })
        }
        (None, "tool.check") => json!({ "id": req["id"], "result": { "decision": "allow" } }),
        (None, "tool.end") => json!({ "id": req["id"], "result": {} }),
        (None, _) => {
            json!({ "id": req["id"], "error": { "code": "no_rule", "message": format!("the scripted host has no rule for {method}") } })
        }
    };
    let delay = rule
        .as_ref()
        .and_then(|r| r.get("delay_ms"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if delay == 0 {
        write_line(stdin, &reply);
    } else {
        let stdin = stdin.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(delay));
            write_line(&stdin, &reply);
        });
    }
}

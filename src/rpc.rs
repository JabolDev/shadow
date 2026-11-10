//! Protocol v1 transport: one JSON object per line on stdout. Every line the
//! daemon writes carries "seq" (1, 2, 3 …). Requests to the host wait for the
//! matching reply, which the stdin reader hands to `Host::on_reply`.
//! `Host::local` is the same host inside this process (`shadow run`).

use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, oneshot};

/// A reply from the host: its `result`, or its `error` object.
pub type Reply = Result<Value, Value>;

enum Out {
    Line(Map<String, Value>),
    Flush(oneshot::Sender<()>),
}

/// The host at the other end of stdin/stdout.
#[derive(Clone)]
pub struct Host {
    out: mpsc::UnboundedSender<Out>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Reply>>>>,
    next_id: Arc<AtomicU64>,
}

impl Host {
    /// Starts the stdout writer (inside a tokio runtime).
    pub fn start() -> Host {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(write_lines(rx));
        Host {
            out: tx,
            pending: Arc::default(),
            next_id: Arc::new(AtomicU64::new(1)),
        }
    }

    /// A host inside this process: the lines come out of the receiver, in
    /// order, instead of stdout; replies go in through `on_reply`.
    pub fn local() -> (Host, mpsc::UnboundedReceiver<Map<String, Value>>) {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (lines, out) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(item) = rx.recv().await {
                match item {
                    Out::Line(m) => {
                        if lines.send(m).is_err() {
                            break;
                        }
                    }
                    // The waiter may have stopped waiting; nothing to tell it then.
                    Out::Flush(done) => {
                        let _ = done.send(());
                    }
                }
            }
        });
        let host = Host {
            out: tx,
            pending: Arc::default(),
            next_id: Arc::new(AtomicU64::new(1)),
        };
        (host, out)
    }

    fn send(&self, obj: Map<String, Value>) {
        if self.out.send(Out::Line(obj)).is_err() {
            eprintln!("shadow: stdout writer is gone; a line was not sent");
        }
    }

    pub fn notify(&self, method: &str, params: Value) {
        let mut m = Map::new();
        m.insert("method".into(), method.into());
        m.insert("params".into(), params);
        self.send(m);
    }

    pub fn reply(&self, id: Value, result: Value) {
        let mut m = Map::new();
        m.insert("id".into(), id);
        m.insert("result".into(), result);
        self.send(m);
    }

    pub fn reply_error(&self, id: Value, code: &str, message: &str) {
        let mut m = Map::new();
        m.insert("id".into(), id);
        m.insert("error".into(), json!({ "code": code, "message": message }));
        self.send(m);
    }

    /// Sends a request to the host; the receiver gets its reply.
    pub fn request(&self, method: &str, params: Value) -> oneshot::Receiver<Reply> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, tx);
        let mut m = Map::new();
        m.insert("id".into(), id.into());
        m.insert("method".into(), method.into());
        m.insert("params".into(), params);
        self.send(m);
        rx
    }

    /// A reply line from the host (a line with "id" and no "method").
    pub fn on_reply(&self, msg: &Map<String, Value>) {
        let Some(id) = msg.get("id").and_then(Value::as_u64) else {
            eprintln!(
                "shadow: reply with an unknown id: {}",
                Value::Object(msg.clone())
            );
            return;
        };
        let Some(tx) = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id)
        else {
            eprintln!("shadow: reply to request {id}, which is not waiting");
            return;
        };
        let reply = match msg.get("error") {
            Some(e) => Err(e.clone()),
            None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
        };
        // The waiter may have stopped waiting (Stop); its reply is then not needed.
        let _ = tx.send(reply);
    }

    /// Resolves once every line queued before this call is on stdout.
    pub async fn flush(&self) {
        let (tx, rx) = oneshot::channel();
        if self.out.send(Out::Flush(tx)).is_ok() {
            let _ = rx.await;
        }
    }
}

/// The text of an error reply from the host.
pub fn error_text(err: &Value) -> String {
    err.get("message")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| err.to_string())
}

async fn write_lines(mut rx: mpsc::UnboundedReceiver<Out>) {
    let mut stdout = tokio::io::stdout();
    let mut seq: u64 = 0;
    let mut buf = Vec::with_capacity(64 * 1024);
    while let Some(first) = rx.recv().await {
        let mut flushed = Vec::new();
        let mut item = Some(first);
        while let Some(it) = item.take() {
            match it {
                Out::Line(obj) => {
                    seq += 1;
                    let mut m = Map::with_capacity(obj.len() + 1);
                    m.insert("seq".into(), seq.into());
                    m.extend(obj);
                    serde_json::to_writer(&mut buf, &Value::Object(m))
                        .expect("a JSON value always serializes");
                    buf.push(b'\n');
                }
                Out::Flush(tx) => flushed.push(tx),
            }
            if buf.len() < (1 << 20) {
                item = rx.try_recv().ok();
            }
        }
        let wrote = async {
            stdout.write_all(&buf).await?;
            stdout.flush().await
        };
        if let Err(e) = wrote.await {
            eprintln!("shadow: stdout is closed ({e}); exiting");
            std::process::exit(1);
        }
        buf.clear();
        for tx in flushed {
            let _ = tx.send(());
        }
    }
}

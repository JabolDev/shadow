//! A fake model server: answers `POST` requests with Claude Messages
//! streaming (server-sent events), from a script of responses.
//!
//! A response (one JSON object):
//!   match        text that must occur in the request's last message (optional)
//!   repeat       true: never used up
//!   status       HTTP status (default 200); with `body` and `headers` for errors
//!   text         a text block, streamed in pieces of `chunk` characters (default 8)
//!   thinking     a thinking block (with a signature)
//!   tool_calls   [{id?, name, input}] tool_use blocks, input streamed as JSON pieces
//!   events       a raw list of events, sent instead of the blocks above
//!   stop_reason  default "tool_use" with tool calls, else "end_turn"
//!   usage        merged into message_start's usage
//!   delay_ms     pause before each event
//!   break_after  close the connection after this many events
//!   hang_after   after this many events, send nothing more until the client leaves
//!
//! Responses are taken in order: the first unused one whose `match` fits.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// One request as received.
#[derive(Clone)]
pub struct Request {
    pub at: Instant,
    pub body: String,
}

#[derive(Clone)]
pub struct FakeModel {
    pub port: u16,
    script: Arc<Mutex<Vec<(Value, bool)>>>,
    requests: Arc<Mutex<Vec<Request>>>,
    record: Option<PathBuf>,
    count: Arc<AtomicUsize>,
}

impl FakeModel {
    /// Listens on 127.0.0.1:`port` (0: any free port).
    pub fn start(
        script: Vec<Value>,
        port: u16,
        record: Option<PathBuf>,
    ) -> std::io::Result<FakeModel> {
        let listener = TcpListener::bind(("127.0.0.1", port))?;
        if let Some(dir) = &record {
            std::fs::create_dir_all(dir)?;
        }
        let fake = FakeModel {
            port: listener.local_addr()?.port(),
            script: Arc::new(Mutex::new(script.into_iter().map(|r| (r, false)).collect())),
            requests: Arc::default(),
            record,
            count: Arc::default(),
        };
        let f = fake.clone();
        thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                let f = f.clone();
                thread::spawn(move || {
                    if let Err(e) = f.serve(conn) {
                        eprintln!("fake-model: {e}");
                    }
                });
            }
        });
        Ok(fake)
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/v1/messages", self.port)
    }

    /// Adds responses at the end of the script.
    pub fn push(&self, responses: impl IntoIterator<Item = Value>) {
        self.script
            .lock()
            .unwrap()
            .extend(responses.into_iter().map(|r| (r, false)));
    }

    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }

    /// The request bodies parsed as JSON.
    pub fn bodies(&self) -> Vec<Value> {
        self.requests()
            .iter()
            .map(|r| serde_json::from_str(&r.body).unwrap_or(Value::Null))
            .collect()
    }

    fn pick(&self, body: &str) -> Option<Value> {
        let last = serde_json::from_str::<Value>(body)
            .ok()
            .and_then(|b| b["messages"].as_array().and_then(|m| m.last().cloned()))
            .map(|m| m.to_string())
            .unwrap_or_default();
        let mut script = self.script.lock().unwrap();
        for (r, used) in script.iter_mut() {
            if *used
                || r.get("match")
                    .and_then(Value::as_str)
                    .is_some_and(|m| !last.contains(m))
            {
                continue;
            }
            if r.get("repeat") != Some(&Value::Bool(true)) {
                *used = true;
            }
            return Some(r.clone());
        }
        None
    }

    fn serve(&self, mut conn: TcpStream) -> std::io::Result<()> {
        let mut r = BufReader::new(conn.try_clone()?);
        let mut len = 0usize;
        let mut line = String::new();
        r.read_line(&mut line)?;
        let at = Instant::now();
        loop {
            line.clear();
            if r.read_line(&mut line)? == 0 || line.trim().is_empty() {
                break;
            }
            if let Some((k, v)) = line.split_once(':')
                && k.trim().eq_ignore_ascii_case("content-length")
            {
                len = v.trim().parse().unwrap_or(0);
            }
        }
        let mut body = vec![0; len];
        r.read_exact(&mut body)?;
        let body = String::from_utf8_lossy(&body).into_owned();
        let n = self.count.fetch_add(1, Ordering::SeqCst) + 1;
        if let Some(dir) = &self.record {
            std::fs::write(dir.join(format!("{n:04}.json")), &body)?;
        }
        self.requests.lock().unwrap().push(Request {
            at,
            body: body.clone(),
        });
        let Some(resp) = self.pick(&body) else {
            eprintln!("fake-model: request {n}: script exhausted");
            let msg = json!({ "type": "error", "error": { "type": "invalid_request_error", "message": "fake model: no response left in the script" } }).to_string();
            return write!(
                conn,
                "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{msg}",
                msg.len()
            );
        };
        eprintln!("fake-model: request {n} answered");
        let status = resp.get("status").and_then(Value::as_u64).unwrap_or(200);
        let mut extra = String::new();
        for (k, v) in resp
            .get("headers")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
        {
            extra.push_str(&format!(
                "{k}: {}\r\n",
                v.as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| v.to_string())
            ));
        }
        if status != 200 {
            let body = match resp.get("body") {
                Some(Value::String(s)) => s.clone(),
                Some(v) => v.to_string(),
                None => json!({ "type": "error", "error": { "type": "api_error", "message": format!("fake status {status}") } }).to_string(),
            };
            return write!(
                conn,
                "HTTP/1.1 {status} Fake\r\ncontent-type: application/json\r\ncontent-length: {}\r\n{extra}connection: close\r\n\r\n{body}",
                body.len()
            );
        }
        write!(
            conn,
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n{extra}connection: close\r\n\r\n"
        )?;
        let delay =
            Duration::from_millis(resp.get("delay_ms").and_then(Value::as_u64).unwrap_or(0));
        let num = |k: &str| resp.get(k).and_then(Value::as_u64).map(|v| v as usize);
        for (i, ev) in events(&resp, n).iter().enumerate() {
            if num("break_after") == Some(i) {
                return Ok(());
            }
            if num("hang_after") == Some(i) {
                let until = Instant::now() + Duration::from_secs(120);
                while Instant::now() < until {
                    thread::sleep(Duration::from_millis(50));
                    if chunk(&mut conn, ": waiting\n\n").is_err() {
                        return Ok(());
                    }
                }
                return Ok(());
            }
            if !delay.is_zero() {
                thread::sleep(delay);
            }
            let kind = ev["type"].as_str().unwrap_or("message");
            chunk(&mut conn, &format!("event: {kind}\ndata: {ev}\n\n"))?;
        }
        conn.write_all(b"0\r\n\r\n")
    }
}

fn chunk(conn: &mut TcpStream, s: &str) -> std::io::Result<()> {
    write!(conn, "{:x}\r\n{s}\r\n", s.len())?;
    conn.flush()
}

fn pieces(s: &str, size: usize) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    chars
        .chunks(size.max(1))
        .map(|c| c.iter().collect())
        .collect()
}

/// The events of a scripted response.
pub fn events(resp: &Value, n: usize) -> Vec<Value> {
    if let Some(list) = resp.get("events").and_then(Value::as_array) {
        return list.clone();
    }
    let size = resp.get("chunk").and_then(Value::as_u64).unwrap_or(8) as usize;
    let calls = resp
        .get("tool_calls")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut usage = json!({ "input_tokens": 10, "output_tokens": 1, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0 });
    if let Some(u) = resp.get("usage").and_then(Value::as_object) {
        for (k, v) in u {
            usage[k] = v.clone();
        }
    }
    let mut out = vec![
        json!({ "type": "message_start", "message": { "id": format!("msg_{n}"), "type": "message", "role": "assistant", "model": "fake", "content": [], "stop_reason": null, "usage": usage } }),
        json!({ "type": "ping" }),
    ];
    let mut index = 0;
    let mut block = |start: Value, deltas: Vec<Value>, out: &mut Vec<Value>| {
        out.push(json!({ "type": "content_block_start", "index": index, "content_block": start }));
        for d in deltas {
            out.push(json!({ "type": "content_block_delta", "index": index, "delta": d }));
        }
        out.push(json!({ "type": "content_block_stop", "index": index }));
        index += 1;
    };
    if let Some(t) = resp.get("thinking").and_then(Value::as_str) {
        let mut d: Vec<Value> = pieces(t, size)
            .into_iter()
            .map(|p| json!({ "type": "thinking_delta", "thinking": p }))
            .collect();
        d.push(json!({ "type": "signature_delta", "signature": "sig-fake" }));
        block(
            json!({ "type": "thinking", "thinking": "", "signature": "" }),
            d,
            &mut out,
        );
    }
    if let Some(t) = resp.get("text").and_then(Value::as_str) {
        let d = pieces(t, size)
            .into_iter()
            .map(|p| json!({ "type": "text_delta", "text": p }))
            .collect();
        block(json!({ "type": "text", "text": "" }), d, &mut out);
    }
    for (i, c) in calls.iter().enumerate() {
        let id = c
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("toolu_{n}_{i}"));
        let input = c
            .get("input")
            .cloned()
            .unwrap_or_else(|| json!({}))
            .to_string();
        let d = pieces(&input, size)
            .into_iter()
            .map(|p| json!({ "type": "input_json_delta", "partial_json": p }))
            .collect();
        block(
            json!({ "type": "tool_use", "id": id, "name": c["name"], "input": {} }),
            d,
            &mut out,
        );
    }
    let stop = resp.get("stop_reason").cloned().unwrap_or_else(|| {
        json!(if calls.is_empty() {
            "end_turn"
        } else {
            "tool_use"
        })
    });
    out.push(json!({ "type": "message_delta", "delta": { "stop_reason": stop, "stop_sequence": null }, "usage": { "output_tokens": 5 } }));
    out.push(json!({ "type": "message_stop" }));
    out
}

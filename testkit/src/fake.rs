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
//!
//! `start_tls` serves HTTPS instead, with a certificate for 127.0.0.1 and
//! localhost signed by a test CA made at start-up (`ca_pem`).

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// One request as received.
#[derive(Clone)]
pub struct Request {
    pub at: Instant,
    /// Header names in lower case, as received.
    pub headers: Vec<(String, String)>,
    pub body: String,
}

#[derive(Clone)]
pub struct FakeModel {
    pub port: u16,
    script: Arc<Mutex<Vec<(Value, bool)>>>,
    requests: Arc<Mutex<Vec<Request>>>,
    record: Option<PathBuf>,
    count: Arc<AtomicUsize>,
    tls: Option<Arc<rustls::ServerConfig>>,
    /// When set, each request's headers are saved there as 0001.json, …
    record_headers: Arc<std::sync::OnceLock<PathBuf>>,
    /// The test CA's certificate (PEM) when serving HTTPS.
    pub ca_pem: Option<String>,
}

/// A test CA and a server certificate for 127.0.0.1 and localhost signed by it.
fn test_tls() -> (Arc<rustls::ServerConfig>, String) {
    use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair};
    let ca_key = KeyPair::generate().unwrap();
    let mut ca = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca.distinguished_name
        .push(DnType::CommonName, "shadow test CA");
    let ca_pem = ca.self_signed(&ca_key).unwrap().pem();
    let issuer = Issuer::new(ca, ca_key);
    let key = KeyPair::generate().unwrap();
    let names = vec!["127.0.0.1".to_string(), "localhost".to_string()];
    let cert = CertificateParams::new(names)
        .unwrap()
        .signed_by(&key, &issuer)
        .unwrap();
    let key = rustls::pki_types::PrivateKeyDer::Pkcs8(key.serialize_der().into());
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert.der().clone()], key)
        .unwrap();
    (Arc::new(config), ca_pem)
}

impl FakeModel {
    /// Listens on 127.0.0.1:`port` (0: any free port).
    pub fn start(
        script: Vec<Value>,
        port: u16,
        record: Option<PathBuf>,
    ) -> std::io::Result<FakeModel> {
        FakeModel::listen(script, port, record, false)
    }

    /// Like `start`, over HTTPS.
    pub fn start_tls(
        script: Vec<Value>,
        port: u16,
        record: Option<PathBuf>,
    ) -> std::io::Result<FakeModel> {
        FakeModel::listen(script, port, record, true)
    }

    fn listen(
        script: Vec<Value>,
        port: u16,
        record: Option<PathBuf>,
        tls: bool,
    ) -> std::io::Result<FakeModel> {
        let (tls, ca_pem) = match tls {
            true => {
                let (config, pem) = test_tls();
                (Some(config), Some(pem))
            }
            false => (None, None),
        };
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
            tls,
            ca_pem,
            record_headers: Arc::default(),
        };
        let f = fake.clone();
        thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                let f = f.clone();
                thread::spawn(move || {
                    let served = match &f.tls {
                        Some(config) => rustls::ServerConnection::new(config.clone())
                            .map_err(std::io::Error::other)
                            .and_then(|tls| f.serve(rustls::StreamOwned::new(tls, conn))),
                        None => f.serve(conn),
                    };
                    if let Err(e) = served {
                        eprintln!("fake-model: {e}");
                    }
                });
            }
        });
        Ok(fake)
    }

    /// Saves each request's headers in `dir` as 0001.json, 0002.json, …
    pub fn record_headers(&self, dir: PathBuf) {
        self.record_headers.get_or_init(|| dir);
    }

    pub fn url(&self) -> String {
        let scheme = if self.tls.is_some() { "https" } else { "http" };
        format!("{scheme}://127.0.0.1:{}/v1/messages", self.port)
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

    fn serve(&self, conn: impl Read + Write) -> std::io::Result<()> {
        let mut r = BufReader::new(conn);
        let mut len = 0usize;
        let mut headers = Vec::new();
        let mut line = String::new();
        r.read_line(&mut line)?;
        let at = Instant::now();
        loop {
            line.clear();
            if r.read_line(&mut line)? == 0 || line.trim().is_empty() {
                break;
            }
            if let Some((k, v)) = line.split_once(':') {
                let (k, v) = (k.trim().to_ascii_lowercase(), v.trim().to_string());
                if k == "content-length" {
                    len = v.parse().unwrap_or(0);
                }
                headers.push((k, v));
            }
        }
        let mut body = vec![0; len];
        r.read_exact(&mut body)?;
        let body = String::from_utf8_lossy(&body).into_owned();
        let n = self.count.fetch_add(1, Ordering::SeqCst) + 1;
        if let Some(dir) = &self.record {
            std::fs::write(dir.join(format!("{n:04}.json")), &body)?;
        }
        if let Some(dir) = self.record_headers.get() {
            let names: serde_json::Map<String, Value> = headers
                .iter()
                .map(|(k, v)| (k.clone(), v.clone().into()))
                .collect();
            std::fs::create_dir_all(dir)?;
            std::fs::write(
                dir.join(format!("{n:04}.json")),
                Value::Object(names).to_string(),
            )?;
        }
        self.requests.lock().unwrap().push(Request {
            at,
            headers,
            body: body.clone(),
        });
        let conn = r.get_mut();
        let Some(resp) = self.pick(&body) else {
            eprintln!("fake-model: request {n}: script exhausted");
            let msg = json!({ "type": "error", "error": { "type": "invalid_request_error", "message": "fake model: no response left in the script" } }).to_string();
            write!(
                conn,
                "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{msg}",
                msg.len()
            )?;
            return conn.flush();
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
            write!(
                conn,
                "HTTP/1.1 {status} Fake\r\ncontent-type: application/json\r\ncontent-length: {}\r\n{extra}connection: close\r\n\r\n{body}",
                body.len()
            )?;
            return conn.flush();
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
                    if chunk(conn, ": waiting\n\n").is_err() {
                        return Ok(());
                    }
                }
                return Ok(());
            }
            if !delay.is_zero() {
                thread::sleep(delay);
            }
            let kind = ev["type"].as_str().unwrap_or("message");
            chunk(conn, &format!("event: {kind}\ndata: {ev}\n\n"))?;
        }
        conn.write_all(b"0\r\n\r\n")?;
        conn.flush()
    }
}

fn chunk(conn: &mut impl Write, s: &str) -> std::io::Result<()> {
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

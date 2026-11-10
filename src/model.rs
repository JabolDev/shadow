//! The model client: a Claude Messages request with `stream: true`, read as
//! server-sent events, assembled into the assistant message, with retries.

use crate::http::{self, Url};
use serde_json::{Map, Value, json};
use std::time::Duration;
use tokio::sync::watch;

const RETRY_STATUSES: [u16; 6] = [429, 500, 502, 503, 504, 529];

#[derive(Clone)]
pub struct Endpoint {
    pub url: Url,
    pub headers: Vec<(String, String)>,
}

/// A finished model message.
pub struct Reply {
    /// `{role: "assistant", content: [...]}` — what goes into the wire log.
    pub message: Value,
    pub usage: Value,
    pub stop_reason: Value,
}

pub enum Outcome {
    Done(Reply),
    /// Stopped by the user; the blocks completed so far, if any.
    Stopped(Option<Value>),
    Failed(Value),
}

/// The request body: model, system, tools, messages (JSON texts, spliced in
/// as they are), stream, then the host's `request` fields. With `cache`, the
/// last system block, the last tool and the last block of the last message
/// get `cache_control: cache` (only in what is sent).
pub fn body(
    model: &str,
    system: Option<&Value>,
    tools: &[Value],
    messages: &[String],
    request: &Map<String, Value>,
    cache: Option<&Value>,
) -> Vec<u8> {
    let size: usize = messages.iter().map(|m| m.len() + 1).sum();
    let mut out = String::with_capacity(size + 16 * 1024);
    out.push_str("{\"model\":");
    out.push_str(&Value::from(model).to_string());
    if let Some(system) = system {
        let mut system = system.clone();
        if let (Some(c), Some(last)) = (cache, system.as_array_mut().and_then(|a| a.last_mut())) {
            mark(last, c);
        }
        out.push_str(",\"system\":");
        out.push_str(&system.to_string());
    }
    out.push_str(",\"tools\":[");
    for (i, t) in tools.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        match cache {
            Some(c) if i + 1 == tools.len() => {
                let mut t = t.clone();
                mark(&mut t, c);
                out.push_str(&t.to_string());
            }
            _ => out.push_str(&t.to_string()),
        }
    }
    out.push_str("],\"messages\":[");
    for (i, m) in messages.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        if let (Some(c), true) = (cache, i + 1 == messages.len()) {
            out.push_str(&mark_last_block(m, c));
        } else {
            out.push_str(m);
        }
    }
    out.push_str("],\"stream\":true");
    for (k, v) in request {
        out.push(',');
        out.push_str(&Value::from(k.as_str()).to_string());
        out.push(':');
        out.push_str(&v.to_string());
    }
    out.push('}');
    out.into_bytes()
}

fn mark(block: &mut Value, cache: &Value) {
    if let Some(obj) = block.as_object_mut() {
        obj.insert("cache_control".into(), cache.clone());
    }
}

/// The message with `cache_control` on its last content block — unless that
/// block cannot carry one (thinking) or the content is a plain string.
fn mark_last_block(msg: &str, cache: &Value) -> String {
    let Ok(mut v) = serde_json::from_str::<Value>(msg) else {
        return msg.to_string();
    };
    let Some(last) = v
        .get_mut("content")
        .and_then(Value::as_array_mut)
        .and_then(|a| a.last_mut())
    else {
        return msg.to_string();
    };
    if matches!(
        last.get("type").and_then(Value::as_str),
        Some("thinking" | "redacted_thinking")
    ) {
        return msg.to_string();
    }
    mark(last, cache);
    v.to_string()
}

/// Server-sent events: feed bytes, get each event's `data`.
#[derive(Default)]
pub struct Sse {
    buf: Vec<u8>,
    data: String,
}

impl Sse {
    pub fn feed(&mut self, bytes: &[u8], out: &mut Vec<String>) {
        self.buf.extend_from_slice(bytes);
        let mut start = 0;
        while let Some(nl) = self.buf[start..].iter().position(|&b| b == b'\n') {
            let mut line = &self.buf[start..start + nl];
            start += nl + 1;
            if line.last() == Some(&b'\r') {
                line = &line[..line.len() - 1];
            }
            if line.is_empty() {
                if !self.data.is_empty() {
                    out.push(std::mem::take(&mut self.data));
                }
            } else if let Some(d) = line.strip_prefix(b"data:") {
                if !self.data.is_empty() {
                    self.data.push('\n');
                }
                let d = d.strip_prefix(b" ").unwrap_or(d);
                self.data.push_str(&String::from_utf8_lossy(d));
            }
        }
        self.buf.drain(..start);
    }
}

struct Block {
    value: Value,
    json: String,
    complete: bool,
}

/// Builds the assistant message from the stream's events.
#[derive(Default)]
pub struct Assembler {
    blocks: Vec<Option<Block>>,
    usage: Map<String, Value>,
    stop_reason: Value,
    pub finished: bool,
}

impl Assembler {
    /// One event; Err when the stream reports an error.
    pub fn on_event(&mut self, ev: &Value) -> Result<(), String> {
        let index = ev.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
        match ev.get("type").and_then(Value::as_str).unwrap_or("") {
            "message_start" => {
                *self = Assembler::default();
                if let Some(u) = ev.pointer("/message/usage").and_then(Value::as_object) {
                    self.usage.extend(u.clone());
                }
            }
            "content_block_start" => {
                if self.blocks.len() <= index {
                    self.blocks.resize_with(index + 1, || None);
                }
                let value = ev
                    .get("content_block")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                self.blocks[index] = Some(Block {
                    value,
                    json: String::new(),
                    complete: false,
                });
            }
            "content_block_delta" => {
                let Some(Some(b)) = self.blocks.get_mut(index) else {
                    return Ok(());
                };
                let d = ev.get("delta").cloned().unwrap_or_default();
                let append = |b: &mut Block, key: &str, add: &str| {
                    let cur = b
                        .value
                        .get(key)
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    b.value[key] = Value::from(cur + add);
                };
                let s = |k: &str| d.get(k).and_then(Value::as_str).unwrap_or("");
                match s("type") {
                    "text_delta" => append(b, "text", s("text")),
                    "thinking_delta" => append(b, "thinking", s("thinking")),
                    "signature_delta" => append(b, "signature", s("signature")),
                    "input_json_delta" => b.json.push_str(s("partial_json")),
                    "citations_delta" => {
                        let c = d.get("citation").cloned().unwrap_or_default();
                        match b.value.get_mut("citations").and_then(Value::as_array_mut) {
                            Some(list) => list.push(c),
                            None => b.value["citations"] = json!([c]),
                        }
                    }
                    other => eprintln!("shadow: unknown delta type {other:?} ignored"),
                }
            }
            "content_block_stop" => {
                if let Some(Some(b)) = self.blocks.get_mut(index) {
                    if !b.json.trim().is_empty() {
                        // Input that is not valid JSON breaks the message: the step is retried.
                        b.value["input"] = serde_json::from_str(&b.json).map_err(|e| {
                            format!("the tool input of block {index} is not valid JSON: {e}")
                        })?;
                    }
                    b.complete = true;
                }
            }
            "message_delta" => {
                if let Some(r) = ev.pointer("/delta/stop_reason") {
                    self.stop_reason = r.clone();
                }
                if let Some(u) = ev.get("usage").and_then(Value::as_object) {
                    self.usage.extend(u.clone());
                }
            }
            "message_stop" => self.finished = true,
            "error" => {
                return Err(ev
                    .get("error")
                    .map(Value::to_string)
                    .unwrap_or_else(|| ev.to_string()));
            }
            _ => {}
        }
        Ok(())
    }

    /// The assistant message of the complete blocks (None when there is none).
    pub fn message(&self) -> Option<Value> {
        let content: Vec<Value> = self
            .blocks
            .iter()
            .flatten()
            .filter(|b| b.complete)
            .map(|b| b.value.clone())
            .collect();
        (!content.is_empty()).then(|| json!({ "role": "assistant", "content": content }))
    }

    pub fn reply(&self) -> Reply {
        Reply {
            message: self
                .message()
                .unwrap_or_else(|| json!({ "role": "assistant", "content": [] })),
            usage: Value::Object(self.usage.clone()),
            stop_reason: self.stop_reason.clone(),
        }
    }
}

enum Attempt {
    Retry {
        status: Option<u16>,
        wait: Option<Duration>,
        error: Value,
    },
    Fatal(Value),
}

/// Calls the model, retrying what retries can fix. `on_event` gets every
/// event but `ping`; `on_retry(attempt, wait_ms, status)` each retry.
pub async fn call(
    ep: &Endpoint,
    body: &[u8],
    retry_max: u32,
    stop: &mut watch::Receiver<bool>,
    on_event: &mut (dyn FnMut(&Value) + Send),
    on_retry: &mut (dyn FnMut(u32, u64, Option<u16>) + Send),
) -> Outcome {
    let mut attempt = 0;
    loop {
        let mut asm = Assembler::default();
        let res = tokio::select! {
            r = once(ep, body, &mut asm, on_event) => r,
            _ = stop.wait_for(|s| *s) => return Outcome::Stopped(asm.message()),
        };
        let (status, wait, error) = match res {
            Ok(()) => return Outcome::Done(asm.reply()),
            Err(Attempt::Fatal(e)) => return Outcome::Failed(e),
            Err(Attempt::Retry {
                status,
                wait,
                error,
            }) => (status, wait, error),
        };
        if attempt >= retry_max {
            return Outcome::Failed(error);
        }
        attempt += 1;
        let wait = wait.unwrap_or_else(|| {
            Duration::from_millis((1000u64 << (attempt - 1).min(5)).min(30_000))
        });
        on_retry(attempt, wait.as_millis() as u64, status);
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = stop.wait_for(|s| *s) => return Outcome::Stopped(None),
        }
    }
}

async fn once(
    ep: &Endpoint,
    body: &[u8],
    asm: &mut Assembler,
    on_event: &mut (dyn FnMut(&Value) + Send),
) -> Result<(), Attempt> {
    let broken = |message: String| Attempt::Retry {
        status: None,
        wait: None,
        error: json!({ "message": message }),
    };
    let mut res = http::post(&ep.url, &ep.headers, body)
        .await
        .map_err(|e| broken(format!("model request failed: {e}")))?;
    if res.status != 200 {
        let status = res.status;
        let wait = res
            .header("retry-after")
            .and_then(|v| v.trim().parse::<f64>().ok())
            .map(|s| Duration::from_secs_f64(s.max(0.0)));
        let text = res
            .text(1 << 20)
            .await
            .unwrap_or_else(|e| format!("(body unreadable: {e})"));
        let body = serde_json::from_str::<Value>(&text).unwrap_or(Value::String(text));
        let message = body
            .pointer("/error/message")
            .and_then(Value::as_str)
            .map(|m| format!("HTTP {status}: {m}"))
            .unwrap_or_else(|| format!("HTTP {status}"));
        let error = json!({ "message": message, "status": status, "body": body });
        return Err(if RETRY_STATUSES.contains(&status) {
            Attempt::Retry {
                status: Some(status),
                wait,
                error,
            }
        } else {
            Attempt::Fatal(error)
        });
    }
    let mut sse = Sse::default();
    let mut events = Vec::new();
    loop {
        let chunk = res
            .chunk()
            .await
            .map_err(|e| broken(format!("the model stream broke: {e}")))?;
        let Some(chunk) = chunk else {
            return Err(broken("the model stream ended before message_stop".into()));
        };
        sse.feed(&chunk, &mut events);
        for data in events.drain(..) {
            let ev: Value = serde_json::from_str(&data)
                .map_err(|e| broken(format!("bad event from the model: {e}")))?;
            if ev.get("type").and_then(Value::as_str) == Some("ping") {
                continue;
            }
            on_event(&ev);
            asm.on_event(&ev)
                .map_err(|e| broken(format!("the model stream reported an error: {e}")))?;
            if asm.finished {
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
#[path = "model_test.rs"]
mod tests;

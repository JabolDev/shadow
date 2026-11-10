//! Plain compaction (`session.compact`): the model writes a summary of the
//! chat; a compaction marker holding it is logged, and from then on the
//! context is that one user message plus later messages.

use crate::model::{self, Endpoint, Outcome};
use crate::rpc::Host;
use crate::session::Session;
use crate::tools;
use crate::turn::EndpointSpec;
use crate::versions::Versions;
use crate::wire::Wire;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::sync::Arc;
use tokio::sync::watch;

const RETRY_MAX: u32 = 6;

const PROMPT: &str = "Write a summary of this conversation so far, so that the work can go on from it without the earlier messages. \
Include: what the user asked for and still wants; decisions made; files read, created or changed (with their paths) and what changed; \
commands that mattered and their results; errors and how they were fixed; what is done, what is left, and the next step. \
Be specific. Reply with the summary only and do not call any tool.";

const LEAD: &str = "This chat was compacted. Summary of the conversation before this point:\n\n";

#[derive(Deserialize)]
struct Params {
    model: String,
    endpoint: EndpointSpec,
    #[serde(default)]
    request: Map<String, Value>,
    system: Option<Vec<Value>>,
    prompt: Option<String>,
}

pub async fn compact(
    host: &Host,
    session: Arc<Session>,
    params: Value,
) -> Result<Value, (&'static str, String)> {
    let bad = |m: String| ("bad_request", m);
    let p: Params =
        serde_json::from_value(params).map_err(|e| bad(format!("session.compact: {e}")))?;
    if let Some(k) = p.request.keys().find(|k| {
        matches!(
            k.as_str(),
            "model" | "system" | "tools" | "messages" | "stream"
        )
    }) {
        return Err(bad(format!("request may not set {k:?}")));
    }
    let endpoint = p.endpoint.build().map_err(bad)?;
    {
        let mut st = session.lock();
        if st.turn.is_some() || st.compacting {
            return Err((
                "busy",
                "a turn or a compaction is running in this chat".into(),
            ));
        }
        st.compacting = true;
    }
    host.notify(
        "compaction",
        json!({ "session": session.id, "state": "start" }),
    );
    let res = summarize(&session, &p.model, p.system, p.request, p.prompt, &endpoint).await;
    session.lock().compacting = false;
    let state = if res.is_ok() { "done" } else { "failed" };
    host.notify(
        "compaction",
        json!({ "session": session.id, "state": state }),
    );
    res.map_err(|e| ("compaction_failed", e))
}

async fn summarize(
    session: &Session,
    model: &str,
    system: Option<Vec<Value>>,
    request: Map<String, Value>,
    prompt: Option<String>,
    ep: &Endpoint,
) -> Result<Value, String> {
    let mut wire = Wire::load(&session.wire_path()).map_err(|e| e.to_string())?;
    if wire.messages.is_empty() {
        return Err("the chat has no messages to compact".into());
    }
    let before = wire.messages.len();
    let mut messages = wire.messages.clone();
    let ask = json!({ "role": "user", "content": [{ "type": "text", "text": prompt.as_deref().unwrap_or(PROMPT) }] });
    messages.push(ask.to_string());
    let tools = session
        .lock()
        .last_tools
        .clone()
        .unwrap_or_else(|| Arc::new(tools::definitions()));
    let system = system.map(Value::Array);
    let body = model::body(
        model,
        system.as_ref(),
        &tools,
        &messages,
        &request,
        Some(&json!({ "type": "ephemeral" })),
    );
    let (_never_stopped, mut stop) = watch::channel(false);
    let reply = match model::call(
        ep,
        &body,
        RETRY_MAX,
        &mut stop,
        &mut |_| {},
        &mut |_, _, _| {},
    )
    .await
    {
        Outcome::Done(r) => r,
        Outcome::Failed(e) => {
            return Err(format!(
                "the summary call failed: {}",
                e.get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error")
            ));
        }
        Outcome::Stopped(_) => return Err("the summary call was stopped".into()),
    };
    let summary: Vec<&str> = reply.message["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|b| b["type"] == "text")
        .filter_map(|b| b["text"].as_str())
        .collect();
    let summary = summary.join("\n").trim().to_string();
    if summary.is_empty() {
        return Err("the model returned no summary text".into());
    }
    wire.push_compaction(&json!({ "role": "user", "content": [{ "type": "text", "text": format!("{LEAD}{summary}") }] }));
    wire.commit()
        .map_err(|e| format!("cannot write the wire log: {e}"))?;
    let mut versions = Versions::load(&session.versions_path());
    versions.clear_shown();
    versions
        .save()
        .map_err(|e| format!("cannot write the version table: {e}"))?;
    Ok(
        json!({ "summary": summary, "messages_before": before, "messages_after": wire.messages.len() }),
    )
}

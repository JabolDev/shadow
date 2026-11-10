//! scripted-host --script FILE [-- COMMAND ARGS…]
//!
//! Starts the daemon (COMMAND, or the script's "command"), runs the script's
//! steps, answers the daemon's requests by the script's "replies" rules, and
//! prints every line it receives on stdout (lines it sends go to stderr,
//! after ">> "). `${NAME}` in the script takes an environment variable.
//!
//! Script: {"command": [...], "replies": [rule…], "steps": [step…]}
//!   {"send": {…}}                     a line; a request without "id" gets one
//!   {"wait": {…}, "timeout_ms": n}    the next line containing this (default 30 s)
//!   {"sleep_ms": n}
//!   {"kill": true}                    SIGKILL the daemon
//!   {"restart": true}                 start it again
//!   {"close": true}                   close its stdin, wait for it to exit
//! Exit code 0 when every step passed, 1 otherwise.

use serde_json::Value;
use shadow_testkit::{arg, expand_env, host::ScriptedHost};
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let Some(file) = arg(&args, "--script") else {
        eprintln!("usage: scripted-host --script FILE [-- COMMAND ARGS…]");
        std::process::exit(2);
    };
    let text = std::fs::read_to_string(&file).unwrap_or_else(|e| panic!("{file}: {e}"));
    let script: Value =
        serde_json::from_str(&expand_env(&text)).unwrap_or_else(|e| panic!("{file}: {e}"));
    let command: Vec<String> = match args.iter().position(|a| a == "--") {
        Some(i) => args[i + 1..].to_vec(),
        None => script["command"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
    };
    if command.is_empty() {
        eprintln!("scripted-host: no command (give one after -- or as \"command\" in the script)");
        std::process::exit(2);
    }
    let rules = script["replies"].as_array().cloned().unwrap_or_default();
    let mut host = ScriptedHost::start(&command, rules, true)
        .unwrap_or_else(|e| panic!("cannot start {command:?}: {e}"));
    for (i, step) in script["steps"].as_array().into_iter().flatten().enumerate() {
        let fail = |m: String| -> ! {
            eprintln!("scripted-host: step {}: {m}", i + 1);
            std::process::exit(1);
        };
        if let Some(line) = step.get("send") {
            let mut line = line.clone();
            let id = host.send(line.clone());
            if line.get("method").is_some() {
                line["id"] = id;
            }
            eprintln!(">> {line}");
        } else if let Some(pattern) = step.get("wait") {
            let timeout = Duration::from_millis(
                step.get("timeout_ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(30_000),
            );
            if let Err(e) = host.wait(pattern, timeout) {
                fail(e);
            }
        } else if let Some(ms) = step.get("sleep_ms").and_then(Value::as_u64) {
            std::thread::sleep(Duration::from_millis(ms));
        } else if step.get("kill").is_some() {
            host.kill();
        } else if step.get("restart").is_some() {
            if let Err(e) = host.restart() {
                fail(format!("restart: {e}"));
            }
        } else if step.get("close").is_some() {
            eprintln!("scripted-host: daemon exited with {:?}", host.close());
        } else {
            fail(format!("unknown step {step}"));
        }
    }
    host.close();
}

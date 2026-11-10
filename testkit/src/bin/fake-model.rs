//! fake-model --script FILE [--port PORT] [--record DIR]
//!
//! Serves Claude Messages streaming on 127.0.0.1 from a script: a JSON list
//! of responses (see testkit/src/fake.rs; `${NAME}` takes an environment
//! variable). Prints `listening <url>` first; with --record, each request
//! body is saved as DIR/0001.json, 0002.json, …

use shadow_testkit::{arg, expand_env, fake::FakeModel};
use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let Some(script) = arg(&args, "--script") else {
        eprintln!("usage: fake-model --script FILE [--port PORT] [--record DIR]");
        std::process::exit(2);
    };
    let text = std::fs::read_to_string(&script).unwrap_or_else(|e| panic!("{script}: {e}"));
    let responses: Vec<serde_json::Value> =
        serde_json::from_str(&expand_env(&text)).unwrap_or_else(|e| panic!("{script}: {e}"));
    let port = arg(&args, "--port").map_or(0, |p| p.parse().expect("--port must be a number"));
    let fake = FakeModel::start(responses, port, arg(&args, "--record").map(PathBuf::from))
        .expect("cannot listen");
    println!("listening {}", fake.url());
    loop {
        std::thread::park();
    }
}

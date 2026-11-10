//! fake-model --script FILE [--port PORT] [--record DIR] [--tls-ca FILE]
//!
//! Serves Claude Messages streaming on 127.0.0.1 from a script: a JSON list
//! of responses (see testkit/src/fake.rs; `${NAME}` takes an environment
//! variable). Prints `listening <url>` first; with --record, each request
//! body is saved as DIR/0001.json, 0002.json, … and its headers as
//! DIR/0001.headers.json, … With --tls-ca it serves HTTPS and writes the
//! test CA certificate (PEM) to that file, for SHADOW_TEST_CA.

use shadow_testkit::{arg, expand_env, fake::FakeModel};
use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let Some(script) = arg(&args, "--script") else {
        eprintln!("usage: fake-model --script FILE [--port PORT] [--record DIR] [--tls-ca FILE]");
        std::process::exit(2);
    };
    let text = std::fs::read_to_string(&script).unwrap_or_else(|e| panic!("{script}: {e}"));
    let responses: Vec<serde_json::Value> =
        serde_json::from_str(&expand_env(&text)).unwrap_or_else(|e| panic!("{script}: {e}"));
    let port = arg(&args, "--port").map_or(0, |p| p.parse().expect("--port must be a number"));
    let record = arg(&args, "--record").map(PathBuf::from);
    let tls_ca = arg(&args, "--tls-ca");
    let fake = match &tls_ca {
        Some(_) => FakeModel::start_tls(responses, port, record),
        None => FakeModel::start(responses, port, record),
    }
    .expect("cannot listen");
    if let (Some(file), Some(pem)) = (tls_ca, &fake.ca_pem) {
        std::fs::write(&file, pem).unwrap_or_else(|e| panic!("{file}: {e}"));
    }
    println!("listening {}", fake.url());
    loop {
        std::thread::park();
    }
}

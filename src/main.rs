//! Shadow: a headless agent harness.
//!
//!   shadow serve   serves many chats over stdin/stdout (protocol v1, PROTOCOL.md)
//!   shadow tools   prints the seven native tool definitions as JSON

mod compact;
mod http;
mod model;
mod rpc;
mod server;
mod session;
mod tools;
mod turn;
mod versions;
mod wire;

fn main() {
    let code = match std::env::args().nth(1).as_deref() {
        Some("serve") => server::serve(),
        Some("tools") => {
            println!("{}", serde_json::Value::Array(tools::definitions()));
            0
        }
        _ => {
            eprintln!("usage: shadow serve | shadow tools");
            2
        }
    };
    std::process::exit(code);
}

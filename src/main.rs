//! Shadow: a headless agent harness.
//!
//!   shadow run "<prompt>"   runs one prompt in the current folder (README.md)
//!   shadow serve            serves many chats over stdin/stdout (protocol v1, PROTOCOL.md)
//!   shadow tools            prints the seven native tool definitions as JSON

mod cli;
mod cli_os;
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
    let args: Vec<String> = std::env::args().collect();
    let code = match args.get(1).map(String::as_str) {
        Some("run") => cli::run(&args[2..]),
        Some("serve") => server::serve(),
        Some("tools") => {
            println!("{}", serde_json::Value::Array(tools::definitions()));
            0
        }
        _ => {
            eprintln!("usage: shadow run \"<prompt>\" | shadow serve | shadow tools");
            2
        }
    };
    std::process::exit(code);
}

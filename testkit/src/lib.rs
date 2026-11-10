//! Test-only tools for shadow: a fake model server (`fake`) and a scripted
//! host (`host`). Both are libraries for the crate's tests and command-line
//! tools for checkers (`fake-model`, `scripted-host`).

pub mod fake;
pub mod host;

/// Replaces `${NAME}` with the environment variable NAME (empty when unset).
pub fn expand_env(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find("${") {
        out.push_str(&rest[..i]);
        match rest[i + 2..].find('}') {
            Some(j) => {
                out.push_str(&std::env::var(&rest[i + 2..i + 2 + j]).unwrap_or_default());
                rest = &rest[i + 3 + j..];
            }
            None => {
                out.push_str(&rest[i..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// The value after `--name` in the arguments.
pub fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

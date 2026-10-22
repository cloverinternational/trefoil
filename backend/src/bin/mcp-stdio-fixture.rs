use serde_json::Value;
use std::io::{self, BufRead, Write};

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_default();
    let stdin = io::stdin();
    let mut stdout = io::BufWriter::new(io::stdout().lock());
    for line in stdin.lock().lines() {
        let line = line.expect("fixture stdin");
        match mode.as_str() {
            "eof" => return,
            "malformed" => {
                writeln!(stdout, "not-json").unwrap();
                stdout.flush().unwrap();
                return;
            }
            "rpc-error" => {
                let message: Value = serde_json::from_str(&line).unwrap();
                if let Some(id) = message.get("id") {
                    writeln!(
                        stdout,
                        r#"{{"jsonrpc":"2.0","id":{id},"error":{{"code":-32602,"message":"bad params","data":{{"field":"value"}}}}}}"#
                    )
                    .unwrap();
                    stdout.flush().unwrap();
                    return;
                }
            }
            "success" => {
                let message: Value = serde_json::from_str(&line).unwrap();
                if message.get("method").and_then(Value::as_str) == Some("notify") {
                    writeln!(
                        stdout,
                        r#"{{"jsonrpc":"2.0","method":"notice","params":{{"seen":true}}}}"#
                    )
                    .unwrap();
                } else if let Some(id) = message.get("id") {
                    writeln!(
                        stdout,
                        r#"{{"jsonrpc":"2.0","id":{id},"result":{{"ok":true}}}}"#
                    )
                    .unwrap();
                }
                stdout.flush().unwrap();
            }
            _ => return,
        }
    }
}

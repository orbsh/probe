//! ADR-0035 `exec` fixture — the bare cgi shape: NO protocol. Read the
//! whole stdin as one JSON request (`{"handler":..., "args":...}`),
//! dispatch, print one JSON result, exit. Nothing survives the call —
//! that is the definition, not a limitation (the php-fpm lineage; the
//! SKILL shape; nushell's landing until its bgi adapter ships).

use serde_json::Value;

fn main() {
    let mut input = String::new();
    if std::io::Read::read_to_string(&mut std::io::stdin(), &mut input).is_err() {
        return;
    }
    let req: Value = match serde_json::from_str(input.trim()) {
        Ok(v) => v,
        Err(_) => {
            println!("{{\"error\": \"one-shot: stdin is not one JSON document\"}}");
            return;
        }
    };
    let handler = req.get("handler").and_then(|v| v.as_str()).unwrap_or("");
    let args = req.get("args").cloned().unwrap_or(Value::Null);
    let result = match handler {
        // Echo the args — the invoke shape, no loop involved.
        "echo" => serde_json::json!({ "echoed": args }),
        // A guard counter would be meaningless across calls (the process
        // died); within ONE call it just counts — included to make the
        // statelessness observable, not assumed.
        "count" => {
            let total = args.get("total").and_then(|v| v.as_u64()).unwrap_or(0);
            serde_json::json!({ "counted": total })
        }
        other => serde_json::json!({ "error": format!("one-shot: no handler '{other}'") }),
    };
    println!("{result}");
}

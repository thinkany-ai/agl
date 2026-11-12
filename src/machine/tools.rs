//! Model-facing schemas and adapters for the four built-in tools.
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use super::{allowed, link_app, Runtime};
use crate::{config::Config, session::Origin};

pub fn names() -> Vec<String> {
    ["read", "write", "edit", "exec"].into_iter().map(String::from).collect()
}

pub fn list(cfg: &Config, origin: &Origin) -> Value {
    let tools = [
        ("read", "fs.read", "Read a local UTF-8 file. Paths are absolute or start with ~/. Results are limited to 64 KiB and include next_offset for continuation.", json!({
            "path": {"type":"string"}, "offset":{"type":"integer","minimum":1,"default":1},
            "limit":{"type":"integer","minimum":1,"maximum":10000,"default":2000}
        }), vec!["path"]),
        ("write", "fs.write", "Create or overwrite a local file with UTF-8 text. Parent directories are created. Paths are absolute or start with ~/.", json!({
            "path":{"type":"string"},"content":{"type":"string"}
        }), vec!["path", "content"]),
        ("edit", "fs.edit", "Replace exactly one occurrence of old_text with new_text in a local UTF-8 file. No write occurs if the match is missing or ambiguous. Include enough context for a unique match.", json!({
            "path":{"type":"string"},"old_text":{"type":"string","minLength":1},"new_text":{"type":"string"}
        }), vec!["path", "old_text", "new_text"]),
        ("exec", "exec", "Run a local command, or read/cancel a previous execution. Start with argv (no shell) or command (/bin/sh -c). Use a stable execution_id when retrying start to avoid duplicates. Read incrementally with returned byte cursors until status=completed and has_more=false. No coding agent is required; commands use installed programs.", json!({
            "action":{"type":"string","enum":["start","read","cancel"],"default":"start"},
            "argv":{"type":"array","items":{"type":"string"},"minItems":1},
            "command":{"type":"string"},"cwd":{"type":"string"},
            "timeout_ms":{"type":"integer","minimum":1,"maximum":1800000},
            "env":{"type":"object","additionalProperties":{"type":"string"}},"stdin":{"type":"string"},
            "app_name":{"type":"string"},"execution_id":{"type":"string","minLength":1,"maxLength":128},
            "stdout_offset":{"type":"integer","minimum":0},"stderr_offset":{"type":"integer","minimum":0},
            "limit":{"type":"integer","minimum":4,"maximum":65536},
            "wait_ms":{"type":"integer","minimum":0,"maximum":30000,"default":1000}
        }), vec![]),
    ];
    Value::Array(tools.into_iter().filter(|(_, method, ..)| allowed(cfg, origin, method).is_ok()).map(|(name, _, description, properties, required)| {
        json!({"name":name,"description":description,"input_schema":{"type":"object","properties":properties,"required":required,"additionalProperties":false}})
    }).collect())
}

pub async fn call(runtime: &Runtime, cfg: &Config, origin: &Origin, params: Value) -> Result<Value> {
    #[derive(Deserialize)]
    struct Call {
        name: String,
        arguments: Value,
    }
    let p: Call = serde_json::from_value(params).context("tools.call params")?;
    let mut args = p.arguments;
    let object = args.as_object_mut().context("arguments must be an object")?;
    match p.name.as_str() {
        "read" => {
            allowed(cfg, origin, "fs.read")?;
            object.insert("encoding".into(), json!("utf8"));
            super::read(args).await
        }
        "write" | "edit" => {
            let method = if p.name == "write" { "fs.write" } else { "fs.edit" };
            allowed(cfg, origin, method)?;
            let _guard = runtime.files.lock().await;
            if p.name == "write" {
                super::write(args).await
            } else {
                super::edit(args).await
            }
        }
        "exec" => {
            allowed(cfg, origin, "exec")?;
            let action = object.remove("action").unwrap_or(json!("start"));
            match action.as_str() {
                Some("start") => {
                    let wait = object.remove("wait_ms").unwrap_or(json!(1000));
                    if wait.as_u64().is_none_or(|n| n > 30_000) {
                        bail!("wait_ms must be 0..30000");
                    }
                    // Cursors belong to read, not to a command's identity.
                    object.remove("stdout_offset");
                    object.remove("stderr_offset");
                    let limit = object.remove("limit");
                    if limit.as_ref().is_some_and(|v| v.as_u64().is_none_or(|n| !(4..=65536).contains(&n))) {
                        bail!("limit must be 4..65536");
                    }
                    let mut result = runtime.executions.start(args, link_app(cfg, origin), origin)?;
                    result["wait_ms"] = wait;
                    if let Some(limit) = limit {
                        result["limit"] = limit;
                    }
                    runtime.executions.read(result, origin).await
                }
                Some("read") => runtime.executions.read(args, origin).await,
                Some("cancel") => runtime.executions.cancel(args, origin),
                _ => bail!("exec action must be start, read or cancel"),
            }
        }
        _ => bail!("unknown built-in tool `{}`", p.name),
    }
}

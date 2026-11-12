//! Approvals for built-in tools called by a hub that opted in
//! (`approvals: true` on `tools.list` / `tools.call`).
//!
//! The link's access still sets the ceiling, and nothing raises it:
//!
//! | Access      | Runs at once                                   | Asks first               |
//! |-------------|------------------------------------------------|--------------------------|
//! | `read_only` | `read`                                         | (nothing else exists)    |
//! | `ask`       | `read`, read-only commands                     | `write`, `edit`, `exec`  |
//! | `edit`      | `read`, `write`, `edit`, read-only commands    | `exec`                   |
//! | `full`      | everything                                     | nothing                  |
//!
//! "Asks first" means `tools.call` holds the call and answers
//! `{status: "approval_required", approval_id, …}` at once. The hub asks the
//! person and answers with `tools.approve {approval_id, decision}`, which
//! runs the held call and returns its result. Held calls expire after
//! `HOLD_FOR`. Which commands are read-only is decided here (`read_only_command`),
//! never by the hub or the model, and only a short, conservative list counts.
//!
//! Without the opt-in, hubs keep the old behaviour: what isn't allowed by
//! the access outright is refused.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use serde_json::{json, Value};

use crate::config::Config;
use crate::session::Origin;

/// How long a held call waits for its answer.
pub const HOLD_FOR: Duration = Duration::from_secs(10 * 60);
/// How long an `allow_session` answer covers the same tool for the same
/// hub session.
pub const GRANT_FOR: Duration = Duration::from_secs(60 * 60);
const MAX_HELD: usize = 64;
const MAX_GRANTS: usize = 1024;

/// What happens to one built-in tool call.
#[derive(Debug, PartialEq)]
pub enum Gate {
    Allow,
    /// Ask the person first; the title says what would happen.
    Ask(String),
    Deny(String),
}

fn access(cfg: &Config, origin: &Origin) -> Option<String> {
    match origin {
        Origin::Local => None,
        Origin::Hub { url, .. } => Some(cfg.hub(url).map(|h| h.access.clone()).unwrap_or_else(|| "ask".into())),
    }
}

fn denied(tool: &str, access: &str) -> Gate {
    Gate::Deny(format!("`{tool}` needs a link with more access than `{access}` (agl connect … --access)"))
}

/// Decides one call. `approvals` is the hub's opt-in.
pub fn gate(cfg: &Config, origin: &Origin, tool: &str, args: &Value, approvals: bool) -> Gate {
    let Some(access) = access(cfg, origin) else { return Gate::Allow };
    let access = access.as_str();
    let start = tool == "exec" && args.get("action").and_then(Value::as_str).unwrap_or("start") == "start";
    match (tool, access) {
        (_, "full") | ("read", _) => Gate::Allow,
        (_, "read_only") => denied(tool, access),
        _ if !approvals => {
            // The 0.3.14 policy, unchanged for hubs that can't ask.
            if matches!(tool, "write" | "edit") && access == "edit" {
                Gate::Allow
            } else {
                denied(tool, access)
            }
        }
        ("write" | "edit", "edit") => Gate::Allow,
        // Reading or cancelling an execution this hub started.
        ("exec", _) if !start => Gate::Allow,
        ("exec", _) if read_only_command(args) => Gate::Allow,
        ("write" | "edit" | "exec", "ask" | "edit") => Gate::Ask(title(tool, args)),
        _ => denied(tool, access),
    }
}

/// Whether `tools.list` offers `tool` to this caller at all.
pub fn offered(cfg: &Config, origin: &Origin, tool: &str, approvals: bool) -> Option<&'static str> {
    let probe = if tool == "exec" { json!({"argv": ["false-probe"]}) } else { json!({}) };
    match gate(cfg, origin, tool, &probe, approvals) {
        Gate::Deny(_) => None,
        Gate::Allow => Some(""),
        Gate::Ask(_) if tool == "exec" => Some(
            " On this link, read-only commands (ls, cat, find, grep, du, git status/log/diff …) run at once; any other command waits for the person to approve it, and the result says so (status approval_required).",
        ),
        Gate::Ask(_) => Some(" On this link it waits for the person to approve it, and the result says so (status approval_required)."),
    }
}

fn clip(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    format!("{}…", s.chars().take(max).collect::<String>())
}

/// What a held call would do, in a line the person can judge.
pub fn title(tool: &str, args: &Value) -> String {
    let path = args.get("path").and_then(Value::as_str).unwrap_or("");
    match tool {
        "write" => {
            let n = args.get("content").and_then(Value::as_str).map_or(0, str::len);
            format!("Write {} ({n} bytes)", clip(path, 200))
        }
        "edit" => format!("Edit {}", clip(path, 200)),
        "exec" => {
            let cmd = match args.get("command").and_then(Value::as_str) {
                Some(c) => c.to_string(),
                None => args
                    .get("argv")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" "))
                    .unwrap_or_default(),
            };
            let mut t = format!("Run: {}", clip(&cmd, 300));
            if let Some(cwd) = args.get("cwd").and_then(Value::as_str) {
                t.push_str(&format!(" (in {})", clip(cwd, 120)));
            }
            t
        }
        other => other.to_string(),
    }
}

struct Held {
    hub: String,
    session: String,
    tool: String,
    args: Value,
    at: Instant,
}

/// Held calls and `allow_session` grants, per daemon.
#[derive(Default)]
pub struct Approvals {
    held: Mutex<HashMap<String, Held>>,
    grants: Mutex<HashMap<(String, String, String), Instant>>,
}

fn hub_url(origin: &Origin) -> String {
    match origin {
        Origin::Hub { url, .. } => url.clone(),
        Origin::Local => String::new(),
    }
}

impl Approvals {
    /// Whether an earlier `allow_session` covers `tool` in `session`.
    pub fn granted(&self, origin: &Origin, session: &str, tool: &str) -> bool {
        if session.is_empty() {
            return false;
        }
        let key = (hub_url(origin), session.to_string(), tool.to_string());
        let mut g = self.grants.lock().unwrap();
        match g.get(&key) {
            Some(at) if at.elapsed() < GRANT_FOR => true,
            Some(_) => {
                g.remove(&key);
                false
            }
            None => false,
        }
    }

    fn grant(&self, origin: &Origin, session: &str, tool: &str) {
        if session.is_empty() {
            return;
        }
        let mut g = self.grants.lock().unwrap();
        g.retain(|_, at| at.elapsed() < GRANT_FOR);
        if g.len() >= MAX_GRANTS {
            if let Some(oldest) = g.iter().min_by_key(|(_, at)| **at).map(|(k, _)| k.clone()) {
                g.remove(&oldest);
            }
        }
        g.insert((hub_url(origin), session.to_string(), tool.to_string()), Instant::now());
    }

    /// Holds a call and returns the answer for the hub.
    pub fn hold(&self, origin: &Origin, session: &str, tool: &str, args: Value, title: String) -> Value {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let mut h = self.held.lock().unwrap();
        h.retain(|_, c| c.at.elapsed() < HOLD_FOR);
        if h.len() >= MAX_HELD {
            if let Some(oldest) = h.iter().min_by_key(|(_, c)| c.at).map(|(k, _)| k.clone()) {
                h.remove(&oldest);
            }
        }
        h.insert(id.clone(), Held { hub: hub_url(origin), session: session.into(), tool: tool.into(), args, at: Instant::now() });
        let mut options = vec!["allow"];
        if !session.is_empty() {
            options.push("allow_session");
        }
        options.push("deny");
        let expires_at = crate::protocol::now_ms() + HOLD_FOR.as_millis() as u64;
        json!({"status": "approval_required", "approval_id": id, "tool": tool, "title": title, "options": options, "expires_at": expires_at})
    }

    /// Takes the held call `id` for `decision`. Returns the tool and its
    /// arguments to run now (allow / allow_session); deny and unknown or
    /// expired ids are errors.
    pub fn take(&self, origin: &Origin, id: &str, decision: &str) -> Result<(String, Value)> {
        if !matches!(decision, "allow" | "allow_session" | "deny") {
            bail!("decision must be allow, allow_session or deny");
        }
        let held = {
            let mut h = self.held.lock().unwrap();
            match h.get(id) {
                Some(c) if c.hub == hub_url(origin) => h.remove(id),
                _ => None,
            }
        };
        let Some(held) = held else { bail!("no pending approval `{id}` (it may have expired)") };
        if held.at.elapsed() >= HOLD_FOR {
            bail!("approval `{id}` expired; call the tool again");
        }
        if decision == "deny" {
            bail!("the person denied this: {}", title(&held.tool, &held.args));
        }
        if decision == "allow_session" {
            self.grant(origin, &held.session, &held.tool);
        }
        Ok((held.tool, held.args))
    }
}

// ---- read-only commands ----

/// Whether an `exec` start only reads: every command in it (a plain
/// pipeline at most) is on the read-only list with no writing option, no
/// `env`, and no shell feature beyond quoting, `|`, `2>/dev/null` and
/// `2>&1`.
pub fn read_only_command(args: &Value) -> bool {
    let Some(o) = args.as_object() else { return false };
    if o.get("env").and_then(Value::as_object).is_some_and(|m| !m.is_empty()) {
        return false;
    }
    let pipeline: Vec<Vec<String>> = match (o.get("argv"), o.get("command").and_then(Value::as_str)) {
        (Some(Value::Array(a)), None) => {
            let words: Option<Vec<String>> = a.iter().map(|v| v.as_str().map(String::from)).collect();
            match words {
                Some(w) => vec![w],
                None => return false,
            }
        }
        (None, Some(c)) => match split_pipeline(c) {
            Some(p) => p,
            None => return false,
        },
        _ => return false,
    };
    !pipeline.is_empty() && pipeline.iter().all(|argv| read_only_argv(argv))
}

/// Splits a shell command into a pipeline of words, or None when it uses
/// anything but quotes, `|`, `2>/dev/null` and `2>&1`.
fn split_pipeline(cmd: &str) -> Option<Vec<Vec<String>>> {
    let mut pipeline = vec![];
    let mut words: Vec<String> = vec![];
    let mut word = String::new();
    let mut in_word = false;
    // The word has an unquoted redirect character; only the two allowed
    // stderr redirects may.
    let mut redirect = false;
    let mut chars = cmd.chars().peekable();
    fn finish(word: &mut String, in_word: &mut bool, redirect: &mut bool, words: &mut Vec<String>) -> bool {
        if *in_word {
            if *redirect {
                if word != "2>/dev/null" && word != "2>&1" {
                    return false;
                }
            } else {
                words.push(std::mem::take(word));
            }
        }
        word.clear();
        *in_word = false;
        *redirect = false;
        true
    }
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' => {
                if !finish(&mut word, &mut in_word, &mut redirect, &mut words) {
                    return None;
                }
            }
            '|' => {
                if !finish(&mut word, &mut in_word, &mut redirect, &mut words) || words.is_empty() || chars.peek() == Some(&'|') {
                    return None;
                }
                pipeline.push(std::mem::take(&mut words));
            }
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(ch) => word.push(ch),
                        None => return None,
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('$' | '`' | '\\' | '!') | None => return None,
                        Some(ch) => word.push(ch),
                    }
                }
            }
            '>' | '&' => {
                in_word = true;
                redirect = true;
                word.push(c);
            }
            '#' if !in_word => return None,
            ';' | '<' | '$' | '`' | '(' | ')' | '{' | '}' | '\\' | '\n' | '\r' | '!' => return None,
            _ => {
                in_word = true;
                word.push(c);
            }
        }
    }
    if !finish(&mut word, &mut in_word, &mut redirect, &mut words) || words.is_empty() {
        return None;
    }
    pipeline.push(words);
    Some(pipeline)
}

/// Commands that only read, whatever their arguments (bare names only, so
/// a path can't stand in for one).
const ALWAYS_READ_ONLY: &[&str] = &[
    "ls", "pwd", "cat", "head", "tail", "wc", "stat", "du", "df", "grep", "egrep", "fgrep", "which", "whereis", "whoami", "id",
    "uname", "sw_vers", "mdfind", "mdls", "cut", "tr", "echo", "printf", "basename", "dirname", "realpath", "readlink", "ps",
    "uptime", "jq", "md5", "shasum", "sha256sum", "md5sum", "diff", "cmp", "nl", "column", "true",
];

fn read_only_argv(argv: &[String]) -> bool {
    let Some((prog, args)) = argv.split_first() else { return false };
    let has = |bad: &[&str]| args.iter().any(|a| bad.iter().any(|b| a == b || a.starts_with(&format!("{b}="))));
    let operands = || args.iter().filter(|a| !a.starts_with('-')).count();
    match prog.as_str() {
        "file" => !args.iter().any(|a| a == "-C" || a == "--compile"),
        p if ALWAYS_READ_ONLY.contains(&p) => true,
        "find" => !has(&["-exec", "-execdir", "-ok", "-okdir", "-delete", "-fprint", "-fprint0", "-fprintf", "-fls"]),
        "rg" => !args.iter().any(|a| a.starts_with("--pre")),
        "tree" => !args.iter().any(|a| a == "-o"),
        "sort" => !args.iter().any(|a| a.starts_with("--output") || a.starts_with("--compress") || (a.starts_with('-') && !a.starts_with("--") && a.contains('o'))),
        "uniq" => operands() <= 1,
        "date" => args.iter().all(|a| a.starts_with('+') || a == "-u" || a == "-R" || a == "-r"),
        "hostname" => args.is_empty(),
        "git" => read_only_git(args),
        _ => false,
    }
}

fn read_only_git(args: &[String]) -> bool {
    let mut i = 0;
    // Global options: only -C <dir> and --no-pager (never -c, which can run anything).
    while let Some(a) = args.get(i) {
        match a.as_str() {
            "-C" => i += 2,
            "--no-pager" => i += 1,
            _ if a.starts_with('-') => return false,
            _ => break,
        }
    }
    let Some(sub) = args.get(i) else { return false };
    let rest = &args[i + 1..];
    if rest.iter().any(|a| a.starts_with("--output") || a.starts_with("--ext-diff") || a.starts_with("--exec") || a.starts_with("--upload-pack")) {
        return false;
    }
    match sub.as_str() {
        "status" | "log" | "show" | "diff" | "rev-parse" | "ls-files" | "blame" | "shortlog" | "describe" => true,
        "remote" => rest.iter().all(|a| a == "-v" || a == "--verbose"),
        "branch" => rest.iter().all(|a| matches!(a.as_str(), "-a" | "-r" | "-v" | "-vv" | "--all" | "--remotes" | "--list" | "--show-current")),
        "tag" => rest.iter().all(|a| a == "-l" || a == "--list"),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(c: &str) -> bool {
        read_only_command(&json!({"command": c}))
    }

    #[test]
    fn read_only_commands() {
        for ok in [
            "ls -la ~/Downloads",
            "ls -lt ~/Downloads | head -20",
            "du -sh ~/Downloads/* 2>/dev/null | sort -rh | head",
            "find ~/Downloads -name '*.dmg' -maxdepth 1",
            "grep -rn \"todo list\" ~/notes",
            "git -C ~/code/app status --short",
            "git log --oneline -5",
            "date +%F",
            "mdfind -onlyin ~ 'kind:pdf'",
        ] {
            assert!(cmd(ok), "{ok}");
        }
        for bad in [
            "rm -rf ~/Downloads",
            "ls; rm x",
            "ls && rm x",
            "ls || rm x",
            "ls > out.txt",
            "cat a >> b",
            "echo $(whoami)",
            "echo `id`",
            "echo \"$HOME\"",
            "find . -delete",
            "find . -exec rm {} ;",
            "sort -o out in",
            "sort -uo out in",
            "uniq in out",
            "git -c core.pager=sh log",
            "git push",
            "git branch -D main",
            "FOO=1 ls",
            "/tmp/ls",
            "ls &",
            "cat < /etc/passwd",
            "ls\nrm x",
            "ls # comment",
            "open -a Safari",
            "sh -c ls",
            "",
            "'unterminated",
        ] {
            assert!(!cmd(bad), "{bad}");
        }
        assert!(read_only_command(&json!({"argv": ["ls", "-la", "a b"]})));
        assert!(!read_only_command(&json!({"argv": ["ls"], "env": {"LD_PRELOAD": "x"}})));
        assert!(!read_only_command(&json!({"argv": ["sh", "-c", "ls"]})));
        assert!(!read_only_command(&json!({"argv": ["ls"], "command": "ls"})));
    }
}

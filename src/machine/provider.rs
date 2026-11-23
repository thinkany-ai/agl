//! Host-registered tools: an application that embeds agl exposes its own
//! tools through `tools.list`, `tools.call` and the machine `hello`, next to
//! the built-ins, with agl's machinery applied to them:
//!
//! - the same link rule as computer use: a `full` link may call them; `ask`
//!   / `edit` only with `approvals: true`, each tool waiting once per session
//!   for the person (`allow_session` keeps it); `read_only` never;
//! - `session` and `images` passed through;
//! - images in the result (`images: [{mime_type, data}]`) shaped like
//!   computer-use screenshots: returned only with `images: true`, PUT to an
//!   `image_upload` URL when given (inline with a warning when that fails),
//!   and marked `unchanged` when the same as the last one sent for the
//!   session and place (the result's `image_key`, else the tool name).
//!
//! Register before the daemon starts. A tool name may not clash with a
//! built-in tool or another provider's. The key `_agl` in arguments is
//! reserved: agl keeps a held call's options there.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use super::computer::{self, Level};
use crate::config::Config;
use crate::session::Origin;

/// A boxed future, so the trait can be used as `dyn ToolProvider`.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A tool as `tools.list` lists it.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    /// A JSON Schema object for the arguments.
    pub input_schema: Value,
}

/// Who calls, besides the arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallContext {
    /// The caller's conversation (`tools.call {session}`), opaque.
    pub session: String,
    /// The link the call came over (its hub URL), empty for a local client.
    pub origin_key: String,
    /// Whether the caller takes images (`tools.call {images: true}`).
    pub images: bool,
}

/// Tools an embedding application adds.
pub trait ToolProvider: Send + Sync + 'static {
    /// The tools, listed after the built-ins. Called for every listing.
    fn tools(&self) -> Vec<ToolDef>;
    /// Runs one of them. An error is the call's error, its message as is.
    /// A result may carry `images: [{mime_type, data}]` (base64) and an
    /// `image_key` naming the place they show (stripped before returning).
    fn call<'a>(&'a self, name: &'a str, args: Value, ctx: CallContext) -> BoxFuture<'a, Result<Value>>;
}

static PROVIDERS: RwLock<Vec<Arc<dyn ToolProvider>>> = RwLock::new(Vec::new());

/// The built-in tool names, whatever this system offers.
fn builtin(name: &str) -> bool {
    ["read", "write", "edit", "exec"].contains(&name) || computer::is_tool(name)
}

/// Adds a provider. Fails when one of its tools has no name, repeats a
/// name, or clashes with a built-in tool or another provider's.
pub fn register_provider(p: Arc<dyn ToolProvider>) -> Result<()> {
    let mut list = PROVIDERS.write().unwrap();
    let taken: Vec<String> = list.iter().flat_map(|q| q.tools()).map(|t| t.name).collect();
    let mut mine: Vec<String> = Vec::new();
    for t in p.tools() {
        if t.name.trim().is_empty() {
            bail!("a provider tool needs a name");
        }
        if builtin(&t.name) {
            bail!("`{}` is a built-in tool", t.name);
        }
        if taken.contains(&t.name) || mine.contains(&t.name) {
            bail!("`{}` is already registered", t.name);
        }
        mine.push(t.name);
    }
    list.push(p);
    Ok(())
}

fn providers() -> Vec<Arc<dyn ToolProvider>> {
    PROVIDERS.read().unwrap().clone()
}

fn find(name: &str) -> Option<Arc<dyn ToolProvider>> {
    providers().into_iter().find(|p| p.tools().iter().any(|t| t.name == name))
}

pub fn is_tool(name: &str) -> bool {
    find(name).is_some()
}

/// Every provider tool name, for the machine `hello`.
pub fn names() -> Vec<String> {
    providers().iter().flat_map(|p| p.tools()).map(|t| t.name).collect()
}

fn level(cfg: &Config, origin: &Origin, approvals: bool, name: &str) -> Level {
    computer::link_level(cfg, origin, approvals, &format!("`{name}`"))
}

/// The provider tools this caller may use, for `tools.list`.
pub fn list(cfg: &Config, origin: &Origin, approvals: bool) -> Vec<Value> {
    providers()
        .iter()
        .flat_map(|p| p.tools())
        .filter_map(|t| {
            let note = match level(cfg, origin, approvals, &t.name) {
                Level::Deny(_) => return None,
                Level::Allow => "",
                Level::Ask => " On this link, its first use in a session waits for the person to approve it, and the result says so (status approval_required).",
            };
            Some(json!({"name": t.name, "description": format!("{}{note}", t.description), "input_schema": t.input_schema}))
        })
        .collect()
}

/// How a provider tool call is made, besides its arguments.
#[derive(Default, Clone, Debug, PartialEq)]
pub struct CallOpts {
    pub approvals: bool,
    pub session: String,
    pub images: bool,
    pub image_upload: Option<String>,
}

/// Options a held call keeps for when it runs.
fn opts(args: &Value) -> CallOpts {
    let o = args.get("_agl");
    let s = |k: &str| o.and_then(|o| o.get(k)).and_then(Value::as_str).map(str::to_string);
    CallOpts {
        approvals: true,
        session: s("session").unwrap_or_default(),
        images: o.and_then(|o| o.get("images")).and_then(Value::as_bool).unwrap_or(false),
        image_upload: s("image_upload"),
    }
}

/// `tools.call` for a provider tool.
pub async fn call(runtime: &super::Runtime, cfg: &Config, origin: &Origin, name: &str, mut args: Value, o: CallOpts) -> Result<Value> {
    let lvl = level(cfg, origin, o.approvals, name);
    if let Level::Deny(e) = &lvl {
        bail!("{e}");
    }
    args.as_object_mut().context("arguments must be an object")?.insert(
        "_agl".into(),
        json!({"session": o.session, "images": o.images, "image_upload": o.image_upload}),
    );
    if lvl == Level::Ask && !runtime.approvals.granted(origin, &o.session, name) {
        return Ok(runtime.approvals.hold(origin, &o.session, name, name, args, format!("Use {name}")));
    }
    run(runtime, origin, name, args).await
}

/// Runs a provider tool the person approved (`tools.approve`): the link is
/// checked again.
pub async fn approved(runtime: &super::Runtime, cfg: &Config, origin: &Origin, name: &str, args: Value) -> Result<Value> {
    if let Level::Deny(e) = level(cfg, origin, true, name) {
        bail!(e);
    }
    run(runtime, origin, name, args).await
}

async fn run(runtime: &super::Runtime, origin: &Origin, name: &str, mut args: Value) -> Result<Value> {
    let o = opts(&args);
    if let Some(a) = args.as_object_mut() {
        a.remove("_agl");
    }
    let p = find(name).with_context(|| format!("`{name}` is no longer offered"))?;
    let hub = computer::hub_key(origin);
    let ctx = CallContext { session: o.session.clone(), origin_key: hub.clone(), images: o.images };
    let mut r = p.call(name, args, ctx).await?;
    let place = r
        .as_object_mut()
        .and_then(|m| m.remove("image_key"))
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| name.to_string());
    super::images::shape_images(&mut r, o.images);
    if o.images {
        let key = (hub, o.session, format!("tool:{name}"), place);
        runtime.provider_shots.deliver(key, &mut r, o.image_upload.as_deref()).await;
    }
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn held_calls_keep_their_options() {
        let o = opts(&json!({"_agl": {"session": "s", "images": true, "image_upload": "https://u"}}));
        assert_eq!(o, CallOpts { approvals: true, session: "s".into(), images: true, image_upload: Some("https://u".into()) });
        assert_eq!(opts(&json!({})), CallOpts { approvals: true, ..Default::default() });
    }
}

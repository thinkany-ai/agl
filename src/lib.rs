//! agl: drive local coding agents with persistent sessions.
//!
//! The binary is a thin CLI over this crate; apps written in Rust (Tauri,
//! GPUI, …) can embed the session manager directly instead of running the
//! daemon: `let mgr = agl::session::Manager::new(agl::config::Config::load()?);`

pub mod agents;
pub mod client;
pub mod config;
pub mod daemon;
pub mod hub;
pub mod link;
pub mod link_http;
pub mod machine;
pub mod protocol;
pub mod proxy;
pub mod service;
pub mod session;
pub mod shellenv;
pub mod update;

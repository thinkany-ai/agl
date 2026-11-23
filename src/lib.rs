//! agl: drive local coding agents with persistent sessions.
//!
//! The binary is a thin CLI over this crate (`cli::run`); apps written in
//! Rust (Tauri, GPUI, …) can embed the session manager directly instead of
//! running the daemon: `let mgr = agl::session::Manager::new(agl::config::Config::load()?);`,
//! or the whole daemon (`daemon::spawn_embedded`). A product shipping agl
//! under its own name sets `brand::set` first.

pub mod agents;
pub mod app;
pub mod brand;
pub mod cli;
pub mod client;
pub mod computer;
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
mod session_file;
pub mod shellenv;
pub mod update;

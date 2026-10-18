//! Scout — lightweight browser automation runtime.
//!
//! Layering, from the outside in:
//!
//! * `server`   — HTTP transport, auth, rate limits, body limits, metrics.
//! * `scout`    — orchestration; transport-agnostic.
//! * `navigation`, `interaction`, `extract`, `adapters` — the operations.
//! * `worker`   — a dedicated thread owning every DOM and QuickJS context,
//!                because neither is `Send`.
//! * `security`, `tasks`, `resources`, `session`, `metrics` — the guardrails.

pub mod abort;
pub mod adapters;
pub mod auth;
pub mod config;
pub mod error;
pub mod extract;
pub mod html;
pub mod interaction;
pub mod js;
pub mod logger;
pub mod metrics;
pub mod mcp;
pub mod page;
pub mod resources;
pub mod schedules;
pub mod scout;
pub mod server;
pub mod security;
pub mod selector;
pub mod session;
pub mod tasks;
pub mod tools;
pub mod util;
pub mod worker;
pub mod validate;

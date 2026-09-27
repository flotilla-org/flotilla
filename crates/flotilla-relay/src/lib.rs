//! Cloudflare Worker relay for one mailbox per install (ADR 0041).
//!
//! `route`, `service`, and `store` are runtime-independent and tested on the host against
//! SQLite; `runtime` is the Workers glue around them.
#[cfg(any(test, target_arch = "wasm32"))]
mod route;
#[cfg(target_arch = "wasm32")]
mod runtime;
#[cfg(any(test, target_arch = "wasm32"))]
mod service;
#[cfg(any(test, target_arch = "wasm32"))]
mod store;

//! Cloudflare Worker relay for one mailbox per install.
#[cfg(any(test, target_arch = "wasm32"))]
mod route;
#[cfg(target_arch = "wasm32")]
mod runtime;

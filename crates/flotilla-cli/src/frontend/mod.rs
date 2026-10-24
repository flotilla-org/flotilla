//! CLI clients shared by the operator executable and the pushed vessel CLI.
pub mod artifact;
pub mod attach;
pub mod connection;
pub mod hooks;
pub mod logs;
pub mod manifest;
pub mod resource;
pub mod targets;
pub use resource::dispatch;

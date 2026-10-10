//! Tauri-free observability stores shared by the native and browser hosts.
//!
//! The stores record backend tracing events and LLM API requests and broadcast new entries.
//! Each host owns delivery: the native shell emits window events, the browser host pushes to
//! the pages that asked for them.

pub mod devtools;
pub mod llm_api_logs;

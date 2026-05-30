//! donto-memory binary's library facade.
//!
//! Re-exports the axum API so integration tests in `tests/` can
//! drive `donto_memory::api::router(...)` directly without going
//! through a TCP listener.

// The single `json!` macro in `api::openapi` documents 19 paths plus
// nested schemas; default 128 is not enough.
#![recursion_limit = "512"]

pub mod api;

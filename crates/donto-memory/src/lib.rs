//! donto-memory binary's library facade.
//!
//! Re-exports the axum API so integration tests in `tests/` can
//! drive `donto_memory::api::router(...)` directly without going
//! through a TCP listener.

pub mod api;

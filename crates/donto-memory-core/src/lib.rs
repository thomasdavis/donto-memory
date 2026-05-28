//! donto-memory core library — substrate client, types, modules, hot/sleep paths.
//!
//! The library is split into:
//!
//! - [`config`]: runtime configuration ([`Settings`]).
//! - [`substrate`]: reqwest-based wrapper for dontosrv.
//! - [`overlays`]: read/write helpers for the consumer overlay tables
//!   created by `migrations/0001_memory_overlays.sql`.
//! - [`module`]: the [`MemoryModule`] trait and [`module::MODULE_REGISTRY`].
//! - [`modules`]: the three default modules (episodic, semantic-claim,
//!   preference).
//! - [`hot_path`]: recall composer + RRF fusion.
//! - [`sleep_path`]: reconsolidation worker + reflection.
//! - [`delta`]: the substrate-blessed delta vocabulary
//!   ([`delta::DontoDelta`]).
//!
//! Public types are re-exported here for convenience.

#![warn(missing_debug_implementations, rust_2018_idioms)]

pub mod config;
pub mod delta;
pub mod extract;
pub mod fusion;
pub mod hot_path;
pub mod module;
pub mod modules;
pub mod overlays;
pub mod sleep_path;
pub mod substrate;
pub mod types;

pub use config::Settings;
pub use delta::{DontoDelta, DontoDeltaOp};
pub use module::{MemoryModule, MemoryModuleArc, ModuleRegistry, ModuleSpec, MODULE_REGISTRY};
pub use substrate::SubstrateClient;
pub use types::{
    AccessKind, MemoryEvidenceBundle, MemoryRecord, MemoryRecordRef, ModuleForm,
    ModuleFunction, Polarity, PolicyAction, RecallQuery, RecallRow,
};

/// donto-memory binds to a specific substrate contract version.
pub const SUBSTRATE_CONTRACT_FLOOR: &str = "0.1.0-m10";

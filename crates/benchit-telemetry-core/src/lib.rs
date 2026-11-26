//! # benchit-telemetry-core
//!
//! Linux workstation telemetry for benchmarking: CPU (utilisation, clocks, temperatures,
//! RAPL package/platform power), memory and swap pressure, disk and network throughput,
//! filesystem usage, fans, battery, and NVIDIA GPUs (via `nvidia-smi`).
//!
//! * [`collect::Collector`] takes rate-aware [`model::Snapshot`]s from `/proc`, `/sys` and
//!   `nvidia-smi`.
//! * [`store::Store`] persists snapshots to a SQLite database with a fixed retention window
//!   (30 days by default) and answers history, summary and benchmark-mark queries.
//! * [`panel`] renders snapshots for desktop panels (text, conky, legacy JSON feed).

#![recursion_limit = "256"]

pub mod collect;
pub mod gpu;
pub mod model;
pub mod panel;
pub mod parse;
pub mod paths;
pub mod store;
pub mod timefmt;

pub use collect::{Collector, CollectorConfig};
pub use model::Snapshot;
pub use store::Store;

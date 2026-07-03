//! The `krab-server` HTTP surface for the engine.
//!
//! This module owns everything the HTTP service decides, kept free of socket
//! I/O so it is unit-testable: configuration resolution, memory admission,
//! and the request → response mapping. The thin `krab-server` binary
//! (`src/bin/krab-server.rs`) wires these to `tiny_http` and real hardware.
//!
//! # Structure
//!
//! - [`config`] — [`Config`]: environment-variable knobs with
//!   hardware-derived defaults (cgroup-aware memory and core detection).
//! - [`gate`] — [`MemGate`]: the global byte-budget admission gate that makes
//!   traffic-driven OOM impossible; reservations are RAII [`Guard`]s.
//! - [`handle`] — [`handle()`](handle::handle): query + body bytes →
//!   [`Reply`] (status, body, warnings); reuses the [`cli`](crate::cli)
//!   format-resolution and diagnostic-rendering helpers.
//!
//! # Behavior
//!
//! One request transforms one document (`POST /transform`). There is no
//! per-document size limit: a request reserves `Content-Length x
//! blowup` bytes from the gate before its body is read, runs in parallel
//! with others while budget remains, and blocks (FIFO) when it is exhausted.
//! The only size-based rejection is a reservation larger than the whole
//! budget. Requests without a Content-Length cannot be sized and are refused
//! with 411.
//!
//! # Testing
//!
//! Each submodule unit-tests its own decision table (see their module docs).
//! The binary's socket loop is deliberately untested I/O; `README.md` in this
//! directory records the smoke-test commands.

pub mod config;
pub mod gate;
pub mod handle;

pub use config::{Config, ConfigError};
pub use gate::{Guard, MemGate, NeverFits};
pub use handle::{Reply, analyze, formats, handle};

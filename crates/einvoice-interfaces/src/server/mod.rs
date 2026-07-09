//! The `krab-server` HTTP surface for the engine.
//!
//! This module owns everything the HTTP service decides, kept free of socket
//! I/O so it is unit-testable: configuration resolution, memory admission,
//! the request → response mapping, and the axum router that ties them
//! together. The thin `krab-server` binary (`src/bin/krab-server.rs`) wires
//! the router to a listener, the tokio runtime, and signal handling.
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
//! - [`router`] — [`router()`](router::router): the axum [`Router`] wiring
//!   routes, per-frame body timeouts, and the gate to the handlers.
//!
//! # Behavior
//!
//! One request transforms one document (`POST /transform`). There is no
//! per-document size limit: a request reserves `Content-Length x
//! blowup` bytes from the gate before its body is read, runs in parallel
//! with others while budget remains, and waits (FIFO) when it is exhausted.
//! The only size-based rejection is a reservation larger than the whole
//! budget. Requests without a Content-Length cannot be sized and are refused
//! with 411. Body reads and writes carry a per-frame timeout, so a
//! live-but-silent peer cannot pin a reservation indefinitely.
//!
//! # Testing
//!
//! Each submodule unit-tests its own decision table (see their module docs);
//! [`router`] tests the full HTTP surface in-process via `oneshot`. Only the
//! binary's listener/runtime/signal wiring is untested I/O; `README.md` in
//! this directory records the smoke-test commands.

pub mod config;
pub mod gate;
pub mod handle;
pub mod router;

pub use axum::Router;
pub use config::{Config, ConfigError};
pub use gate::{Guard, MemGate, NeverFits};
pub use handle::{Reply, analyze, formats, handle};
pub use router::router;

//! Global memory admission gate for the HTTP server.
//!
//! [`MemGate`] bounds the total bytes reserved by in-flight requests so the
//! process can never allocate past its budget (no OOM from traffic). A request
//! reserves its estimated peak memory *before* reading its body via
//! [`MemGate::acquire`]; the returned [`Guard`] releases the reservation on
//! drop. There is deliberately no per-request size limit — only requests that
//! could never fit in the whole budget are rejected ([`NeverFits`]).
//!
//! # Behavior
//!
//! - Reservations within the free budget succeed immediately; requests run in
//!   parallel as long as budget remains.
//! - When the budget is exhausted, `acquire` blocks until a `Guard` drops.
//! - Waiters are admitted one at a time (an internal admission lock): while a
//!   blocked reservation waits, later callers queue behind it, so a large
//!   blocked reservation cannot be starved by a stream of small ones. The
//!   order in which queued waiters are admitted is the mutex's wake order,
//!   not strictly FIFO. The price is head-of-line blocking, which only occurs
//!   once the budget is already exhausted.
//!
//! # Invariants
//!
//! - The sum of live reservations never exceeds the budget.
//! - Every successful `acquire` is paired with exactly one release (RAII).
//! - `acquire` takes at most one all-or-nothing reservation and holds nothing
//!   while waiting except the admission ticket, so no deadlock is possible.
//!
//! # Testing
//!
//! Unit tests below cover immediate admission, RAII release, the `NeverFits`
//! rejection, blocking + resumption across threads, and parallel admission
//! under partial load.

use std::sync::{Condvar, Mutex};

/// The reservation can never be satisfied: it exceeds the *total* budget.
///
/// This is the only rejection the gate produces — it is a statement about the
/// configured budget, not a per-request size policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NeverFits {
    /// Bytes the caller asked to reserve.
    pub requested: u64,
    /// The gate's total budget.
    pub budget: u64,
}

impl std::fmt::Display for NeverFits {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "reservation of {} bytes can never fit the total budget of {} bytes",
            self.requested, self.budget
        )
    }
}

impl std::error::Error for NeverFits {}

/// A byte-budget admission gate. See the module docs for semantics.
#[derive(Debug)]
pub struct MemGate {
    budget: u64,
    free: Mutex<u64>,
    freed: Condvar,
    /// Serializes waiters so only one contends for budget at a time; held
    /// only while acquiring, never while a reservation is in use. No FIFO
    /// guarantee (`std` mutexes are unfair), but the holder cannot be starved.
    admission: Mutex<()>,
}

/// A live reservation. Dropping it returns the bytes to the gate and wakes
/// waiters.
#[derive(Debug)]
pub struct Guard<'a> {
    gate: &'a MemGate,
    bytes: u64,
}

impl MemGate {
    /// Creates a gate with `budget` total bytes.
    pub fn new(budget: u64) -> Self {
        MemGate {
            budget,
            free: Mutex::new(budget),
            freed: Condvar::new(),
            admission: Mutex::new(()),
        }
    }

    /// The gate's total budget in bytes.
    pub fn budget(&self) -> u64 {
        self.budget
    }

    /// Bytes currently unreserved. Snapshot for logging/tests; another thread
    /// may change it immediately after.
    pub fn available(&self) -> u64 {
        *lock_ignoring_poison(&self.free)
    }

    /// Reserves `bytes`, blocking while the budget is exhausted.
    ///
    /// Returns a [`Guard`] that releases the reservation on drop.
    ///
    /// # Errors
    ///
    /// [`NeverFits`] if `bytes` exceeds the total budget — such a request
    /// could block forever.
    pub fn acquire(&self, bytes: u64) -> Result<Guard<'_>, NeverFits> {
        if bytes > self.budget {
            return Err(NeverFits {
                requested: bytes,
                budget: self.budget,
            });
        }
        // Only the ticket holder contends for budget; later callers queue
        // here, so a large blocked reservation cannot be starved. Wake order
        // among queued callers is unspecified (not strict FIFO).
        let _ticket = lock_ignoring_poison(&self.admission);
        let mut free = lock_ignoring_poison(&self.free);
        while *free < bytes {
            free = self
                .freed
                .wait(free)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        *free -= bytes;
        Ok(Guard { gate: self, bytes })
    }
}

/// Locks `m`, recovering from poisoning: the guarded value is a plain byte
/// counter whose invariant survives a panicking holder.
fn lock_ignoring_poison<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        *lock_ignoring_poison(&self.gate.free) += self.bytes;
        self.gate.freed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn test_acquire_within_budget_succeeds_and_reduces_available() {
        let gate = MemGate::new(100);
        let guard = gate.acquire(60).expect("60 of 100 fits");
        assert_eq!(gate.available(), 40);
        drop(guard);
    }

    #[test]
    fn test_guard_drop_restores_available() {
        let gate = MemGate::new(100);
        let guard = gate.acquire(60).expect("fits");
        drop(guard);
        assert_eq!(gate.available(), 100);
    }

    #[test]
    fn test_acquire_over_total_budget_returns_never_fits() {
        let gate = MemGate::new(100);
        // Rejected immediately even though the budget is fully free.
        let err = gate.acquire(101).expect_err("can never fit");
        assert_eq!(
            err,
            NeverFits {
                requested: 101,
                budget: 100
            }
        );
        assert_eq!(gate.available(), 100, "failed acquire reserves nothing");
    }

    #[test]
    fn test_acquire_exactly_budget_succeeds() {
        let gate = MemGate::new(100);
        let guard = gate.acquire(100).expect("exact fit is admitted");
        assert_eq!(gate.available(), 0);
        drop(guard);
    }

    #[test]
    fn test_parallel_reservations_coexist_within_budget() {
        let gate = MemGate::new(100);
        let big = gate.acquire(60).expect("fits");
        // A second reservation is admitted immediately while the first lives.
        let small = gate.acquire(30).expect("still fits beside the first");
        assert_eq!(gate.available(), 10);
        drop(big);
        drop(small);
        assert_eq!(gate.available(), 100);
    }

    #[test]
    fn test_exhausted_budget_blocks_until_guard_drops() {
        let gate = Arc::new(MemGate::new(100));
        let held = gate.acquire(80).expect("fits");

        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let waiter = {
            let gate = Arc::clone(&gate);
            std::thread::spawn(move || {
                started_tx.send(()).expect("main alive");
                // 50 > 20 free: must block until `held` drops.
                let guard = gate.acquire(50).expect("fits budget, must wait");
                done_tx.send(()).expect("main alive");
                drop(guard);
            })
        };

        started_rx.recv().expect("waiter started");
        // The waiter must still be blocked while 80/100 is held.
        assert!(
            done_rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "acquire must block while the budget is exhausted"
        );

        drop(held);
        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("waiter resumes after release");
        waiter.join().expect("waiter exits cleanly");
        assert_eq!(gate.available(), 100);
    }
}

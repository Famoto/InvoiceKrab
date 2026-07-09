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
//! - When the budget is exhausted, `acquire` waits (async) until a `Guard`
//!   drops.
//! - Waiters are admitted one at a time (an internal admission lock): while a
//!   blocked reservation waits, later callers queue behind it, so a large
//!   blocked reservation cannot be starved by a stream of small ones. The
//!   admission lock is a tokio mutex, which is FIFO-fair, so queued waiters
//!   are admitted in arrival order. The price is head-of-line blocking, which
//!   only occurs once the budget is already exhausted.
//!
//! # Invariants
//!
//! - The sum of live reservations never exceeds the budget.
//! - Every successful `acquire` is paired with exactly one release (RAII);
//!   [`Guard`] owns an `Arc` of the gate, so it may outlive the acquiring
//!   task — the response path holds it until the last byte is written.
//! - `acquire` takes at most one all-or-nothing reservation and holds nothing
//!   while waiting except the admission ticket, so no deadlock is possible.
//!   Cancelling (dropping) a waiting `acquire` future reserves nothing.
//!
//! # Testing
//!
//! Unit tests below cover immediate admission, RAII release, the `NeverFits`
//! rejection, waiting + resumption across tasks, and parallel admission
//! under partial load.

use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

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
    /// Free bytes. A std mutex: critical sections are a compare and an
    /// add/subtract, never held across an await.
    free: Mutex<u64>,
    /// Signalled on every release. `notify_one` stores a permit when nobody
    /// is waiting yet, so a release between a failed check and the wait
    /// cannot be lost.
    freed: Notify,
    /// Serializes waiters so only one contends for budget at a time; held
    /// only while acquiring, never while a reservation is in use. Tokio
    /// mutexes are FIFO-fair, so admission order is arrival order.
    admission: tokio::sync::Mutex<()>,
}

/// A live reservation. Dropping it returns the bytes to the gate and wakes
/// the next waiter. Owns the gate (`Arc`), so it is `'static` and can be
/// attached to a response body outliving the handler.
#[derive(Debug)]
pub struct Guard {
    gate: Arc<MemGate>,
    bytes: u64,
}

impl MemGate {
    /// Creates a gate with `budget` total bytes.
    pub fn new(budget: u64) -> Self {
        MemGate {
            budget,
            free: Mutex::new(budget),
            freed: Notify::new(),
            admission: tokio::sync::Mutex::new(()),
        }
    }

    /// The gate's total budget in bytes.
    pub fn budget(&self) -> u64 {
        self.budget
    }

    /// Bytes currently unreserved. Snapshot for logging/tests; another task
    /// may change it immediately after.
    pub fn available(&self) -> u64 {
        *lock_ignoring_poison(&self.free)
    }

    /// Reserves `bytes`, waiting while the budget is exhausted.
    ///
    /// Takes the gate by `Arc` so the returned [`Guard`] owns its gate.
    /// Returns a [`Guard`] that releases the reservation on drop.
    ///
    /// # Errors
    ///
    /// [`NeverFits`] if `bytes` exceeds the total budget — such a request
    /// could wait forever.
    pub async fn acquire(self: Arc<Self>, bytes: u64) -> Result<Guard, NeverFits> {
        if bytes > self.budget {
            return Err(NeverFits {
                requested: bytes,
                budget: self.budget,
            });
        }
        // Only the ticket holder contends for budget; later callers queue
        // here in FIFO order, so a large blocked reservation cannot be
        // starved.
        let ticket = self.admission.lock().await;
        loop {
            {
                let mut free = lock_ignoring_poison(&self.free);
                if *free >= bytes {
                    *free -= bytes;
                    drop(free);
                    drop(ticket);
                    return Ok(Guard { gate: self, bytes });
                }
            }
            // A release after the check above stored a permit in `freed`
            // (`notify_one` with no waiter), so this cannot miss the wakeup;
            // a stale permit merely causes one spurious re-check.
            self.freed.notified().await;
        }
    }
}

/// Locks `m`, recovering from poisoning: the guarded value is a plain byte
/// counter whose invariant survives a panicking holder.
fn lock_ignoring_poison<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Drop for Guard {
    fn drop(&mut self) {
        *lock_ignoring_poison(&self.gate.free) += self.bytes;
        // At most one task waits on `freed` (the admission ticket holder);
        // it re-checks in a loop, so consecutive releases accumulate in
        // `free` even though only one permit is stored.
        self.gate.freed.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn gate(budget: u64) -> Arc<MemGate> {
        Arc::new(MemGate::new(budget))
    }

    #[tokio::test]
    async fn test_acquire_within_budget_succeeds_and_reduces_available() {
        let gate = gate(100);
        let guard = gate.clone().acquire(60).await.expect("60 of 100 fits");
        assert_eq!(gate.available(), 40);
        drop(guard);
    }

    #[tokio::test]
    async fn test_guard_drop_restores_available() {
        let gate = gate(100);
        let guard = gate.clone().acquire(60).await.expect("fits");
        drop(guard);
        assert_eq!(gate.available(), 100);
    }

    #[tokio::test]
    async fn test_acquire_over_total_budget_returns_never_fits() {
        let gate = gate(100);
        // Rejected immediately even though the budget is fully free.
        let err = gate.clone().acquire(101).await.expect_err("can never fit");
        assert_eq!(
            err,
            NeverFits {
                requested: 101,
                budget: 100
            }
        );
        assert_eq!(gate.available(), 100, "failed acquire reserves nothing");
    }

    #[tokio::test]
    async fn test_acquire_exactly_budget_succeeds() {
        let gate = gate(100);
        let guard = gate
            .clone()
            .acquire(100)
            .await
            .expect("exact fit is admitted");
        assert_eq!(gate.available(), 0);
        drop(guard);
    }

    #[tokio::test]
    async fn test_parallel_reservations_coexist_within_budget() {
        let gate = gate(100);
        let big = gate.clone().acquire(60).await.expect("fits");
        // A second reservation is admitted immediately while the first lives.
        let small = gate
            .clone()
            .acquire(30)
            .await
            .expect("still fits beside the first");
        assert_eq!(gate.available(), 10);
        drop(big);
        drop(small);
        assert_eq!(gate.available(), 100);
    }

    // Current-thread runtime: after `yield_now` the spawned waiter has run
    // exactly until it parked on the gate, so `is_finished` is deterministic.
    #[tokio::test]
    async fn test_exhausted_budget_waits_until_guard_drops() {
        let gate = gate(100);
        let held = gate.clone().acquire(80).await.expect("fits");

        let waiter = tokio::spawn({
            let gate = gate.clone();
            // 50 > 20 free: must wait until `held` drops.
            async move { gate.acquire(50).await.expect("fits budget, must wait") }
        });

        tokio::task::yield_now().await;
        assert!(
            !waiter.is_finished(),
            "acquire must wait while the budget is exhausted"
        );

        drop(held);
        let guard = tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("waiter resumes after release")
            .expect("waiter task exits cleanly");
        assert_eq!(gate.available(), 50);
        drop(guard);
        assert_eq!(gate.available(), 100);
    }

    #[tokio::test]
    async fn test_cancelled_waiter_reserves_nothing() {
        let gate = gate(100);
        let held = gate.clone().acquire(80).await.expect("fits");

        let waiter = tokio::spawn({
            let gate = gate.clone();
            async move { gate.acquire(50).await }
        });
        tokio::task::yield_now().await;
        waiter.abort();
        let _ = waiter.await;

        drop(held);
        assert_eq!(gate.available(), 100, "aborted waiter must not leak bytes");
        // The gate still admits after an aborted waiter (ticket released).
        let guard = gate
            .clone()
            .acquire(100)
            .await
            .expect("gate usable after cancellation");
        drop(guard);
    }
}

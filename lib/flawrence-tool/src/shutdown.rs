//! A shared, monotonic shutdown level driving the staged Ctrl-C abort.
//!
//! One run fans out to many subagents whose work is nested under a single
//! tokio task, so a plain `abort_all` is all-or-nothing: it kills the root loop
//! together with its children, losing the consolidation turns that turn raw
//! findings into submissions and a summary. This lets an operator ask for that
//! consolidation to still happen.
//!
//! The level only ever rises. Each Ctrl-C escalates it by one:
//!
//! - `0` [`RUNNING`]: normal operation.
//! - `1` [`DRAINING`]: stop starting new work, let in-flight work finish.
//! - `2` [`CONSOLIDATE`]: cancel in-flight subagents, but let each root loop
//!   keep running so it can submit what was found and write its summary.
//! - `3` [`ABORT`]: stop everything, including the root loops.

use std::sync::atomic::{AtomicU8, Ordering};

use tokio::sync::Notify;

/// Normal operation: nothing has been asked to stop.
pub const RUNNING: u8 = 0;
/// Finish in-flight work, start nothing new.
pub const DRAINING: u8 = 1;
/// Cancel in-flight subagents, let root loops consolidate and submit.
pub const CONSOLIDATE: u8 = 2;
/// Stop everything, root loops included.
pub const ABORT: u8 = 3;

/// A monotonic shutdown level shared across the whole task tree.
#[derive(Debug)]
pub struct ShutdownSignal {
    level: AtomicU8,
    /// Woken on every escalation so awaiters re-check the level.
    changed: Notify,
}

impl Default for ShutdownSignal {
    fn default() -> Self {
        Self {
            level: AtomicU8::new(RUNNING),
            changed: Notify::new(),
        }
    }
}

impl ShutdownSignal {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The current level.
    #[must_use]
    pub fn level(&self) -> u8 {
        self.level.load(Ordering::Acquire)
    }

    /// Whether the level has reached at least `target`.
    #[must_use]
    pub fn at_least(&self, target: u8) -> bool {
        self.level() >= target
    }

    /// Raise the level by one (never past [`ABORT`]) and wake every awaiter.
    /// Returns the new level.
    pub fn escalate(&self) -> u8 {
        // A CAS loop rather than fetch_add so the level saturates at ABORT
        // instead of wrapping, and so concurrent callers agree on the result.
        let mut current = self.level();
        loop {
            let next = current.saturating_add(1).min(ABORT);
            match self
                .level
                .compare_exchange(current, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => {
                    self.changed.notify_waiters();
                    return next;
                }
                Err(observed) => current = observed,
            }
        }
    }

    /// Resolve once the level is at least `target`, immediately if it already
    /// is.
    pub async fn reached(&self, target: u8) {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            // Register as a waiter *before* the check. `notify_waiters` only
            // wakes registered waiters, so an escalation that lands after this
            // check would otherwise be lost until the next one.
            notified.as_mut().enable();
            if self.at_least(target) {
                return;
            }
            notified.await;
            if self.at_least(target) {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn escalates_one_step_at_a_time_and_saturates() {
        let signal = ShutdownSignal::new();
        assert_eq!(signal.level(), RUNNING);
        assert_eq!(signal.escalate(), DRAINING);
        assert_eq!(signal.escalate(), CONSOLIDATE);
        assert_eq!(signal.escalate(), ABORT);
        // Further presses do not wrap past ABORT.
        assert_eq!(signal.escalate(), ABORT);
        assert!(signal.at_least(CONSOLIDATE));
    }

    #[tokio::test]
    async fn reached_returns_immediately_when_already_past_target() {
        let signal = ShutdownSignal::new();
        signal.escalate();
        signal.escalate();
        // Would hang if it waited; the test's own completion is the assertion.
        signal.reached(CONSOLIDATE).await;
    }

    #[tokio::test]
    async fn reached_wakes_on_escalation() {
        let signal = Arc::new(ShutdownSignal::new());
        let waiter = {
            let signal = signal.clone();
            tokio::spawn(async move {
                signal.reached(CONSOLIDATE).await;
                signal.level()
            })
        };
        // Give the waiter a chance to park on the first escalation.
        signal.escalate();
        tokio::task::yield_now().await;
        signal.escalate();
        let observed = waiter.await.unwrap();
        assert!(observed >= CONSOLIDATE);
    }
}

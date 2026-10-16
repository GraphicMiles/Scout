//! Memory admission control and a per-job watchdog.
//!
//! Two independent protections, because they stop different failures:
//!   * `admit` refuses *new* work once the process is above its soft budget, so
//!     a busy service sheds load instead of being killed by the kernel.
//!   * `watch` aborts a job that pushes the process past the hard budget, so one
//!     pathological page cannot take the whole container down.

use crate::abort::Abort;
use crate::config::Config;
use crate::error::{ErrorCode, ScoutError};
use crate::metrics::rss_bytes;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone)]
pub struct ResourceManager {
    cfg: Arc<Config>,
}

impl ResourceManager {
    pub fn new(cfg: Arc<Config>) -> Self {
        Self { cfg }
    }

    pub fn rss_bytes(&self) -> u64 {
        rss_bytes()
    }

    /// Refuse new work while the process sits above its soft memory budget.
    pub fn admit(&self) -> Result<(), ScoutError> {
        let soft = self.cfg.memory_soft_limit_bytes;
        if self.rss_bytes() <= soft {
            return Ok(());
        }
        // One sampling after a beat: RSS includes pages that have been freed but
        // not yet returned, and rejecting work on a transient spike would make
        // the service flap.
        std::thread::sleep(Duration::from_millis(15));
        if self.rss_bytes() > soft {
            return Err(ScoutError::new(
                ErrorCode::Overloaded,
                "Service is above its memory budget; retry shortly",
            )
            .with_retry_after(5));
        }
        Ok(())
    }

    /// Spawn a watchdog that cancels `token` if RSS crosses the hard budget.
    /// The returned guard stops it when the job finishes.
    pub fn watch(&self, abort: Abort) -> WatchGuard {
        let hard = self.cfg.memory_hard_limit_bytes;
        let handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_millis(200));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                if abort.is_cancelled() {
                    return;
                }
                if rss_bytes() > hard {
                    abort.cancel_with(ScoutError::new(
                        ErrorCode::ResourceLimit,
                        "Aborted: memory limit exceeded while executing this task",
                    ));
                    return;
                }
            }
        });
        WatchGuard { handle }
    }
}

pub struct WatchGuard {
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for WatchGuard {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn re_admits_below_budget() {
        let rm = ResourceManager::new(Arc::new(Config::dev()));
        assert!(rm.admit().is_ok(), "an idle process is well under the soft limit");
    }

    #[test]
    fn refuses_above_a_soft_budget() {
        let mut cfg = Config::dev();
        // A budget of zero bytes is unconditionally exceeded, standing in for a
        // process that really is out of headroom.
        cfg.memory_soft_limit_bytes = 0;
        let rm = ResourceManager::new(Arc::new(cfg));
        let err = rm.admit().unwrap_err();
        assert_eq!(err.code, ErrorCode::Overloaded);
        assert!(err.retryable);
    }

    #[tokio::test]
    async fn watchdog_cancels_above_a_hard_budget() {
        let mut cfg = Config::dev();
        cfg.memory_hard_limit_bytes = 0;
        let rm = ResourceManager::new(Arc::new(cfg));
        let abort = Abort::new();
        let _guard = rm.watch(abort.clone());
        for _ in 0..40 {
            if abort.is_cancelled() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(abort.is_cancelled(), "watchdog should have tripped");
        assert_eq!(abort.reason().code, ErrorCode::ResourceLimit);
    }
}

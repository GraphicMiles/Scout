//! Bounded queue and bounded concurrency, shared by background tasks and by
//! synchronous API work.
//!
//! Both kinds of work pass through the same limiter on purpose: a burst of
//! synchronous `/v1/extract` calls must not be able to starve background tasks,
//! and `SCOUT_CONCURRENCY` must remain the single knob that bounds CPU use.

use crate::abort::Abort;
use crate::config::Config;
use crate::error::{ErrorCode, Result, ScoutError};
use crate::logger::iso8601_from_unix_ms;
use crate::util::{now_ms, random_id};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Semaphore;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    TimedOut,
}

impl TaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            TaskStatus::Queued => "queued",
            TaskStatus::Running => "running",
            TaskStatus::Succeeded => "succeeded",
            TaskStatus::Failed => "failed",
            TaskStatus::Cancelled => "cancelled",
            TaskStatus::TimedOut => "timed_out",
        }
    }
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            TaskStatus::Succeeded | TaskStatus::Failed | TaskStatus::Cancelled | TaskStatus::TimedOut
        )
    }
}

pub struct TaskRecord {
    pub id: String,
    pub owner: String,
    pub status: TaskStatus,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub result: Option<Value>,
    pub error: Option<Value>,
    pub abort: Abort,
}

impl TaskRecord {
    pub fn view(&self) -> Value {
        json!({
            "id": self.id,
            "status": self.status.as_str(),
            "createdAt": iso8601_from_unix_ms(self.created_at),
            "startedAt": self.started_at.map(iso8601_from_unix_ms),
            "finishedAt": self.finished_at.map(iso8601_from_unix_ms),
            "result": self.result,
            "error": self.error,
        })
    }
}

/// RAII admission slot: releasing it is guaranteed even if the job panics.
struct Admission {
    inflight: Arc<AtomicUsize>,
}

impl Drop for Admission {
    fn drop(&mut self) {
        self.inflight.fetch_sub(1, Ordering::SeqCst);
    }
}

pub struct TaskManager {
    cfg: Arc<Config>,
    sem: Arc<Semaphore>,
    inflight: Arc<AtomicUsize>,
    tasks: Arc<Mutex<HashMap<String, Arc<Mutex<TaskRecord>>>>>,
    stopping: Arc<AtomicBool>,
    metrics: Arc<crate::metrics::Metrics>,
    /// Memory admission and the hard-limit watchdog. Node applied the memory
    /// budget to task admission as well as the queue bound; both are needed, so
    /// both are checked here.
    resources: crate::resources::ResourceManager,
}

impl TaskManager {
    pub fn new(
        cfg: Arc<Config>,
        metrics: Arc<crate::metrics::Metrics>,
        resources: crate::resources::ResourceManager,
    ) -> Self {
        Self {
            resources,
            sem: Arc::new(Semaphore::new(cfg.concurrency.max(1))),
            inflight: Arc::new(AtomicUsize::new(0)),
            tasks: Arc::new(Mutex::new(HashMap::new())),
            stopping: Arc::new(AtomicBool::new(false)),
            cfg,
            metrics,
        }
    }

    pub fn is_stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }

    pub fn stats(&self) -> (usize, usize) {
        let running = self.cfg.concurrency.max(1) - self.sem.available_permits();
        let queued = self.inflight.load(Ordering::SeqCst).saturating_sub(running);
        (queued, running)
    }

    /// Reserve capacity, or reject with a retryable error.
    ///
    /// Rejecting *before* waiting is what keeps the queue bounded: a caller that
    /// arrives when the service is saturated is told to come back rather than
    /// joining an unbounded backlog.
    fn admit(&self) -> Result<Admission> {
        // Above the soft memory budget nothing new starts, whatever the queue
        // looks like: shed load rather than be killed by the kernel.
        self.resources.admit()?;
        if self.stopping.load(Ordering::SeqCst) {
            return Err(ScoutError::new(
                ErrorCode::ShuttingDown,
                "Service is shutting down",
            ));
        }
        let mut current = self.inflight.load(Ordering::SeqCst);
        loop {
            if current >= self.cfg.max_queued + self.cfg.concurrency.max(1) {
                return Err(ScoutError::new(
                    ErrorCode::QueueFull,
                    format!("Queue is full ({} waiting)", self.cfg.max_queued),
                )
                .with_retry_after(2));
            }
            match self.inflight.compare_exchange_weak(
                current,
                current + 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => {
                    return Ok(Admission {
                        inflight: self.inflight.clone(),
                    })
                }
                Err(actual) => current = actual,
            }
        }
    }

    /// Run work on the caller's behalf and return its value.
    ///
    /// Used by the synchronous endpoints. The work still acquires a concurrency
    /// permit, so it is subject to the same limit as background tasks.
    pub async fn run_inline<F, Fut, T>(
        self: &Arc<Self>,
        parent: Option<&Abort>,
        timeout_override_ms: Option<u64>,
        f: F,
    ) -> Result<T>
    where
        F: FnOnce(Abort) -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        let admission = self.admit()?;
        let abort = Abort::new();
        if let Some(p) = parent {
            let pa = p.clone();
            let a = abort.clone();
            tokio::spawn(async move {
                pa.token().cancelled().await;
                a.cancel_with(pa.reason());
            });
        }

        let permit = self
            .sem
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| ScoutError::new(ErrorCode::ShuttingDown, "Service is shutting down"))?;

        let timeout_ms = timeout_override_ms.unwrap_or(self.cfg.request_timeout_ms);
        let work = f(abort.clone());
        let out = tokio::select! {
            biased;
            _ = abort.token().cancelled() => Err(abort.reason()),
            r = tokio::time::timeout(Duration::from_millis(timeout_ms), work) => match r {
                Ok(v) => v,
                Err(_) => Err(ScoutError::new(
                    ErrorCode::TaskTimeout,
                    format!("Exceeded the {timeout_ms}ms execution limit"),
                )),
            },
        };
        drop(permit);
        drop(admission);
        out
    }

    /// Queue a background task and return its initial view.
    pub fn submit<F, Fut>(self: &Arc<Self>, owner: String, timeout_override_ms: Option<u64>, f: F) -> Result<Value>
    where
        F: FnOnce(Abort) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<Value>> + Send + 'static,
    {
        self.admit()?;
        let id = random_id("tsk_", 12);
        let abort = Abort::new();
        let record = Arc::new(Mutex::new(TaskRecord {
            id: id.clone(),
            owner,
            status: TaskStatus::Queued,
            created_at: now_ms(),
            started_at: None,
            finished_at: None,
            result: None,
            error: None,
            abort: abort.clone(),
        }));

        {
            let mut tasks = self.tasks.lock().unwrap();
            tasks.insert(id.clone(), record.clone());
            self.prune(&mut tasks);
        }

        let view = record.lock().unwrap().view();
        let this = self.clone();
        let timeout_ms = timeout_override_ms.unwrap_or(self.cfg.task_timeout_ms);

        tokio::spawn(async move {
            // Acquire capacity; a task cancelled before it starts never runs.
            let permit = match this.sem.clone().acquire_owned().await {
                Ok(p) => p,
                Err(_) => {
                    let mut r = record.lock().unwrap();
                    if !r.status.is_terminal() {
                        r.status = TaskStatus::Cancelled;
                        r.finished_at = Some(now_ms());
                        r.error = Some(
                            ScoutError::new(ErrorCode::ShuttingDown, "Service is shutting down")
                                .to_json()["error"]
                                .clone(),
                        );
                    }
                    return;
                }
            };

            if abort.is_cancelled() {
                let mut r = record.lock().unwrap();
                let err = abort.reason();
                r.status = if err.code == ErrorCode::TaskTimeout {
                    TaskStatus::TimedOut
                } else {
                    TaskStatus::Cancelled
                };
                r.finished_at = Some(now_ms());
                r.error = Some(err.to_json()["error"].clone());
                return;
            }

            {
                let mut r = record.lock().unwrap();
                r.status = TaskStatus::Running;
                r.started_at = Some(now_ms());
            }

            let _watchdog = this.resources.watch(abort.clone());
            let result = tokio::select! {
                biased;
                _ = abort.token().cancelled() => Err(abort.reason()),
                r = tokio::time::timeout(Duration::from_millis(timeout_ms), f(abort.clone())) => {
                    match r {
                        Ok(v) => v,
                        Err(_) => Err(ScoutError::new(
                            ErrorCode::TaskTimeout,
                            format!("Exceeded the {timeout_ms}ms execution limit"),
                        )),
                    }
                }
            };

            // A job that completes after being cancelled must never be recorded as
            // a success, no matter what its future returned.
            let result = if abort.is_cancelled() {
                Err(abort.reason())
            } else {
                result
            };

            let mut r = record.lock().unwrap();
            if r.status.is_terminal() {
                return; // already cancelled by the caller
            }
            r.finished_at = Some(now_ms());
            match result {
                Ok(v) => {
                    r.status = TaskStatus::Succeeded;
                    r.result = Some(v);
                }
                Err(e) => {
                    r.status = match e.code {
                        ErrorCode::TaskTimeout => TaskStatus::TimedOut,
                        ErrorCode::Cancelled | ErrorCode::ShuttingDown => TaskStatus::Cancelled,
                        _ => TaskStatus::Failed,
                    };
                    r.error = Some(e.to_json()["error"].clone());
                }
            }
            let status = r.status.as_str().to_string();
            drop(r);
            this.metrics.inc(
                "scout_tasks_total",
                "Background tasks by terminal status",
                &[("status", &status)],
            );
            drop(permit);
        });

        Ok(view)
    }

    pub fn get(&self, owner: &str, id: &str) -> Result<Value> {
        let record = {
            let tasks = self.tasks.lock().unwrap();
            tasks.get(id).cloned()
        };
        match record {
            Some(r) if r.lock().unwrap().owner == owner => Ok(r.lock().unwrap().view()),
            // Another key gets the same answer as a nonexistent id.
            _ => Err(ScoutError::not_found("Task not found")),
        }
    }

    pub fn cancel(&self, owner: &str, id: &str) -> Result<Value> {
        let record = {
            let tasks = self.tasks.lock().unwrap();
            tasks.get(id).cloned()
        };
        let record = match record {
            Some(r) => r,
            None => return Err(ScoutError::not_found("Task not found")),
        };
        let mut r = record.lock().unwrap();
        if r.owner != owner {
            return Err(ScoutError::not_found("Task not found"));
        }
        if r.status == TaskStatus::Cancelled {
            return Ok(r.view());
        }
        if r.status.is_terminal() {
            return Err(ScoutError::new(
                ErrorCode::Conflict,
                format!("Task already {}", r.status.as_str()),
            ));
        }
        r.status = TaskStatus::Cancelled;
        r.finished_at = Some(now_ms());
        let err = ScoutError::new(ErrorCode::Cancelled, "Task cancelled by caller");
        r.error = Some(err.to_json()["error"].clone());
        let abort = r.abort.clone();
        drop(r);
        abort.cancel_with(err);
        self.metrics.inc(
            "scout_tasks_total",
            "Background tasks by terminal status",
            &[("status", "cancelled")],
        );
        let view = record.lock().unwrap().view();
        Ok(view)
    }

    /// Drop finished records past their TTL, and enforce the retention cap.
    fn prune(&self, tasks: &mut HashMap<String, Arc<Mutex<TaskRecord>>>) {
        let now = now_ms();
        let ttl = self.cfg.task_result_ttl_ms as i64;
        tasks.retain(|_, r| {
            let r = r.lock().unwrap();
            !(r.status.is_terminal() && now - r.finished_at.unwrap_or(now) > ttl)
        });
        if tasks.len() > self.cfg.max_retained_tasks {
            let mut finished: Vec<(String, i64)> = tasks
                .iter()
                .filter_map(|(id, r)| {
                    let r = r.lock().unwrap();
                    if r.status.is_terminal() {
                        Some((id.clone(), r.finished_at.unwrap_or(0)))
                    } else {
                        None
                    }
                })
                .collect();
            finished.sort_by_key(|(_, t)| *t);
            for (id, _) in finished {
                if tasks.len() <= self.cfg.max_retained_tasks {
                    break;
                }
                tasks.remove(&id);
            }
        }
    }

    /// Stop accepting work, let in-flight jobs finish within the grace period,
    /// then cancel whatever remains.
    pub async fn shutdown(&self, grace_ms: u64) {
        self.stopping.store(true, Ordering::SeqCst);
        let err = ScoutError::new(ErrorCode::ShuttingDown, "Service is shutting down");

        let records: Vec<Arc<Mutex<TaskRecord>>> = {
            let tasks = self.tasks.lock().unwrap();
            tasks.values().cloned().collect()
        };

        let deadline = std::time::Instant::now() + Duration::from_millis(grace_ms);
        loop {
            let running = records
                .iter()
                .filter(|r| !r.lock().unwrap().status.is_terminal())
                .count();
            if running == 0 || std::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        for r in &records {
            let mut rec = r.lock().unwrap();
            if !rec.status.is_terminal() {
                rec.status = TaskStatus::Cancelled;
                rec.finished_at = Some(now_ms());
                rec.error = Some(err.to_json()["error"].clone());
                rec.abort.cancel_with(err.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::Metrics;

    fn mgr(concurrency: usize, max_queued: usize) -> Arc<TaskManager> {
        let mut cfg = Config::dev();
        cfg.concurrency = concurrency;
        cfg.max_queued = max_queued;
        cfg.task_timeout_ms = 5_000;
        cfg.request_timeout_ms = 5_000;
        let cfg = Arc::new(cfg);
        Arc::new(TaskManager::new(
            cfg.clone(),
            Arc::new(Metrics::new()),
            crate::resources::ResourceManager::new(cfg),
        ))
    }

    #[tokio::test]
    async fn inline_work_returns_its_value() {
        let m = mgr(1, 4);
        let out = m
            .run_inline(None, None, |_a| async { Ok(42u32) })
            .await
            .unwrap();
        assert_eq!(out, 42);
    }

    #[tokio::test]
    async fn inline_work_times_out() {
        let mut cfg = Config::dev();
        cfg.concurrency = 1;
        cfg.max_queued = 4;
        cfg.request_timeout_ms = 100;
        let cfg = Arc::new(cfg);
        let m = Arc::new(TaskManager::new(
            cfg.clone(),
            Arc::new(Metrics::new()),
            crate::resources::ResourceManager::new(cfg),
        ));
        let err = m
            .run_inline(None, None, |_a| async {
                tokio::time::sleep(Duration::from_millis(600)).await;
                Ok(1)
            })
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::TaskTimeout);
    }

    #[tokio::test]
    async fn queue_is_bounded() {
        let m = mgr(1, 2);
        // Hold the single permit with a slow job.
        let m2 = m.clone();
        let holder = tokio::spawn(async move {
            m2.run_inline(None, None, |_a| async {
                tokio::time::sleep(Duration::from_millis(600)).await;
                Ok(())
            })
            .await
        });
        tokio::time::sleep(Duration::from_millis(60)).await;

        // Fill the queue.
        let mut handles = Vec::new();
        for _ in 0..2 {
            let m3 = m.clone();
            handles.push(tokio::spawn(async move {
                m3.run_inline(None, None, |_a| async {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    Ok(())
                })
                .await
            }));
        }
        tokio::time::sleep(Duration::from_millis(60)).await;

        // The next caller is refused rather than queueing forever.
        let err = m
            .run_inline(None, None, |_a| async { Ok(()) })
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::QueueFull);
        assert!(err.retryable);

        holder.abort();
        for h in handles {
            h.abort();
        }
    }

    #[tokio::test]
    async fn background_task_lifecycle() {
        let m = mgr(1, 8);
        let view = m
            .submit("key_a".into(), None, |_a| async { Ok(json!({ "ok": true })) })
            .unwrap();
        let id = view["id"].as_str().unwrap().to_string();
        assert_eq!(view["status"], "queued");

        for _ in 0..50 {
            let v = m.get("key_a", &id).unwrap();
            if v["status"] == "succeeded" {
                assert_eq!(v["result"]["ok"], true);
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("task did not complete");
    }

    #[tokio::test]
    async fn tasks_are_scoped_to_the_creating_key() {
        let m = mgr(1, 8);
        let view = m.submit("key_a".into(), None, |_a| async { Ok(json!(1)) }).unwrap();
        let id = view["id"].as_str().unwrap();
        assert!(m.get("key_a", id).is_ok());
        assert!(m.get("key_b", id).is_err());
        assert!(m.cancel("key_b", id).is_err());
    }

    #[tokio::test]
    async fn cancelled_task_is_never_reported_as_success() {
        let m = mgr(1, 8);
        // The job ignores cancellation and tries to return success late.
        let view = m
            .submit("k".into(), None, |_a| async {
                tokio::time::sleep(Duration::from_millis(300)).await;
                Ok(json!("should not be recorded"))
            })
            .unwrap();
        let id = view["id"].as_str().unwrap().to_string();
        tokio::time::sleep(Duration::from_millis(60)).await;
        m.cancel("k", &id).unwrap();

        tokio::time::sleep(Duration::from_millis(450)).await;
        let v = m.get("k", &id).unwrap();
        assert_eq!(v["status"], "cancelled");
        assert!(v["result"].is_null(), "a cancelled task must not expose a result");
    }

    #[tokio::test]
    async fn cancelling_a_finished_task_conflicts() {
        let m = mgr(1, 8);
        let view = m.submit("k".into(), None, |_a| async { Ok(json!(1)) }).unwrap();
        let id = view["id"].as_str().unwrap().to_string();
        for _ in 0..60 {
            if m.get("k", &id).unwrap()["status"] == "succeeded" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let err = m.cancel("k", &id).unwrap_err();
        assert_eq!(err.code, ErrorCode::Conflict);
    }
}

//! Recurring task schedules: run a task spec every so often, notice when the
//! result changed, and tell a webhook about it.
//!
//! A schedule is a task spec plus an interval. The first run happens at
//! creation, then every interval. Each run's result is hashed (FNV-1a 64) and
//! compared with the previous run's hash: only a changed result fires the
//! callback, so a watcher is told about news, not about the clock.
//!
//! When `SCOUT_TASK_STORE` is set, schedules are written to that JSON file and
//! reloaded at boot, so a restart resumes watching instead of forgetting.

use crate::config::Config;
use crate::error::{ErrorCode, Result, ScoutError};
use crate::util::{now_ms, random_id};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub struct Schedule {
    pub id: String,
    pub owner: String,
    /// The validated task body: steps, timeoutMs, retries.
    pub spec: Value,
    pub every_ms: u64,
    pub created_at: i64,
    pub next_run_at: i64,
    pub last_run_at: Option<i64>,
    pub last_status: Option<String>,
    pub last_task_id: Option<String>,
    pub last_hash: Option<u64>,
    /// Whether the most recent run's result differed from the one before it.
    pub last_changed: Option<bool>,
    pub runs: u64,
    pub callback_url: Option<String>,
    pub callback_secret: Option<String>,
}

impl Schedule {
    fn view(&self) -> Value {
        json!({
            "id": self.id,
            "everyMs": self.every_ms,
            "createdAt": crate::logger::iso8601_from_unix_ms(self.created_at),
            "nextRunAt": crate::logger::iso8601_from_unix_ms(self.next_run_at),
            "lastRunAt": self.last_run_at.map(crate::logger::iso8601_from_unix_ms),
            "lastStatus": self.last_status,
            "lastRunId": self.last_task_id,
            "changed": self.last_changed,
            "runs": self.runs,
            "callbackUrl": self.callback_url,
            "spec": self.spec,
        })
    }

}

/// FNV-1a 64. Not cryptographic; it is only used to notice that a result's
/// bytes differ from the previous run's bytes, and it is stable across
/// processes and platforms, which a HashMap hash would not be.
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in bytes {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// HMAC-SHA256, hex-encoded. Used to sign webhook bodies so the receiver can
/// prove a callback came from whoever holds the secret.
pub fn hmac_sha256_hex(secret: &str, body: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let key = secret.as_bytes();
    // Keys longer than the block size are hashed first, per RFC 2104.
    let key = if key.len() > 64 {
        let d = Sha256::digest(key);
        let mut k = vec![0u8; 64];
        k[..d.len()].copy_from_slice(&d);
        k
    } else {
        let mut k = vec![0u8; 64];
        k[..key.len()].copy_from_slice(key);
        k
    };
    let mut ipad = Vec::with_capacity(64 + body.len());
    let mut opad = Vec::with_capacity(64 + 32);
    for i in 0..64 {
        ipad.push(key[i] ^ 0x36);
        opad.push(key[i] ^ 0x5c);
    }
    ipad.extend_from_slice(body);
    let inner = Sha256::digest(&ipad);
    opad.extend_from_slice(&inner);
    let out = Sha256::digest(&opad);
    out.iter().map(|b| format!("{b:02x}")).collect()
}

pub struct Scheduler {
    schedules: Mutex<HashMap<String, Schedule>>,
    store_path: Option<String>,
    max: usize,
}

impl Scheduler {
    pub fn new(cfg: &Config) -> Self {
        let s = Self {
            schedules: Mutex::new(HashMap::new()),
            store_path: cfg.task_store.clone(),
            max: cfg.max_schedules.max(1),
        };
        s.load();
        s
    }

    fn load(&self) {
        let Some(path) = &self.store_path else { return };
        let Ok(text) = std::fs::read_to_string(path) else { return };
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            crate::logger::warn("schedule store is not valid JSON; starting empty", json!({ "path": path }));
            return;
        };
        let Some(list) = v.get("schedules").and_then(|s| s.as_array()) else { return };
        let mut map = self.schedules.lock().unwrap();
        for s in list {
            let Some(id) = s.get("id").and_then(|x| x.as_str()) else { continue };
            let Some(spec) = s.get("spec") else { continue };
            let every_ms = s.get("everyMs").and_then(|x| x.as_u64()).unwrap_or(0);
            if every_ms == 0 {
                continue;
            }
            let owner = s.get("owner").and_then(|x| x.as_str()).unwrap_or("dev").to_string();
            let created_at = s.get("createdAtMs").and_then(|x| x.as_i64()).unwrap_or_else(now_ms);
            let last_hash = s.get("lastHash").and_then(|x| x.as_u64());
            map.insert(
                id.to_string(),
                Schedule {
                    id: id.to_string(),
                    owner,
                    spec: spec.clone(),
                    every_ms,
                    created_at,
                    // Missed intervals are not backfilled; the next run is one
                    // interval from boot.
                    next_run_at: now_ms() + every_ms as i64,
                    last_run_at: None,
                    last_status: s.get("lastStatus").and_then(|x| x.as_str()).map(String::from),
                    last_task_id: None,
                    last_hash,
                    last_changed: None,
                    runs: s.get("runs").and_then(|x| x.as_u64()).unwrap_or(0),
                    callback_url: s.get("callbackUrl").and_then(|x| x.as_str()).map(String::from),
                    callback_secret: s
                        .get("callbackSecret")
                        .and_then(|x| x.as_str())
                        .map(String::from),
                },
            );
        }
    }

    fn save(&self) {
        let Some(path) = &self.store_path else { return };
        let map = self.schedules.lock().unwrap();
        let list: Vec<Value> = map
            .values()
            .map(|s| {
                json!({
                    "id": s.id,
                    "owner": s.owner,
                    "spec": s.spec,
                    "everyMs": s.every_ms,
                    "createdAtMs": s.created_at,
                    "lastStatus": s.last_status,
                    "lastHash": s.last_hash,
                    "runs": s.runs,
                    "callbackUrl": s.callback_url,
                    "callbackSecret": s.callback_secret,
                })
            })
            .collect();
        let text = json!({ "schedules": list }).to_string();
        drop(map);
        // Atomic write: a crash mid-save cannot leave a half-written store.
        let tmp = format!("{path}.tmp");
        if std::fs::write(&tmp, text).and_then(|_| std::fs::rename(&tmp, path)).is_err() {
            crate::logger::warn("could not persist the schedule store", json!({ "path": path }));
        }
    }

    pub fn create(
        &self,
        owner: &str,
        spec: Value,
        every_ms: u64,
        callback_url: Option<String>,
        callback_secret: Option<String>,
    ) -> Result<Value> {
        let mut map = self.schedules.lock().unwrap();
        if map.len() >= self.max {
            return Err(ScoutError::new(
                ErrorCode::TooManySessions,
                format!("Schedule limit reached ({}); delete one first", self.max),
            ));
        }
        let id = random_id("sch_", 12);
        let schedule = Schedule {
            id: id.clone(),
            owner: owner.to_string(),
            spec,
            every_ms,
            created_at: now_ms(),
            next_run_at: now_ms(), // first run happens at the next tick
            last_run_at: None,
            last_status: None,
            last_task_id: None,
            last_hash: None,
            last_changed: None,
            runs: 0,
            callback_url,
            callback_secret,
        };
        let view = schedule.view();
        map.insert(id, schedule);
        drop(map);
        self.save();
        Ok(view)
    }

    pub fn list(&self, owner: &str) -> Value {
        let map = self.schedules.lock().unwrap();
        let mut list: Vec<Value> = map
            .values()
            .filter(|s| s.owner == owner)
            .map(|s| s.view())
            .collect();
        list.sort_by(|a, b| {
            a.get("createdAt")
                .and_then(|v| v.as_str())
                .cmp(&b.get("createdAt").and_then(|v| v.as_str()))
        });
        json!({ "schedules": list, "count": list.len() })
    }

    pub fn get(&self, owner: &str, id: &str) -> Result<Value> {
        let map = self.schedules.lock().unwrap();
        match map.get(id) {
            Some(s) if s.owner == owner => Ok(s.view()),
            _ => Err(ScoutError::not_found("Schedule not found")),
        }
    }

    pub fn delete(&self, owner: &str, id: &str) -> Result<()> {
        let mut map = self.schedules.lock().unwrap();
        match map.get(id) {
            Some(s) if s.owner == owner => {
                map.remove(id);
                drop(map);
                self.save();
                Ok(())
            }
            _ => Err(ScoutError::not_found("Schedule not found")),
        }
    }

    /// Schedules whose run is due now. Their `next_run_at` is pushed forward
    /// before the runs start, so a slow run cannot cause a stampede.
    pub fn take_due(&self) -> Vec<(String, String, Value, Option<String>, Option<String>)> {
        let now = now_ms();
        let mut map = self.schedules.lock().unwrap();
        let mut due = Vec::new();
        for s in map.values_mut() {
            if s.next_run_at <= now {
                s.next_run_at = now + s.every_ms as i64;
                due.push((
                    s.id.clone(),
                    s.owner.clone(),
                    s.spec.clone(),
                    s.callback_url.clone(),
                    s.callback_secret.clone(),
                ));
            }
        }
        drop(map);
        if !due.is_empty() {
            self.save();
        }
        due
    }

    /// Record a completed run. Returns whether the result changed and the
    /// task id, for the caller's callback decision.
    pub fn record(&self, id: &str, status: &str, task_id: &str, hash: Option<u64>) -> Option<bool> {
        let mut map = self.schedules.lock().unwrap();
        let s = map.get_mut(id)?;
        s.last_run_at = Some(now_ms());
        s.last_status = Some(status.to_string());
        s.last_task_id = Some(task_id.to_string());
        s.runs += 1;
        let changed = match (s.last_hash, hash) {
            (Some(prev), Some(h)) => prev != h,
            (None, Some(_)) => true, // first successful run is news
            _ => false,
        };
        s.last_changed = Some(changed);
        if status == "succeeded" {
            if let Some(h) = hash {
                s.last_hash = Some(h);
            }
        }
        drop(map);
        self.save();
        Some(changed)
    }

    pub fn size(&self) -> usize {
        self.schedules.lock().unwrap().len()
    }
}

/// Fire a signed webhook: one POST, JSON body, HMAC-SHA256 signature header.
/// Best effort by design: a webhook that is down must not take the scheduler
/// with it. The outcome is logged, not propagated.
pub async fn fire_callback(client: &reqwest::Client, url: &str, secret: Option<&str>, payload: &Value) {
    let body = serde_json::to_vec(payload).unwrap_or_default();
    let mut req = client
        .post(url)
        .timeout(std::time::Duration::from_secs(10))
        .header("content-type", "application/json")
        .header("user-agent", "Scout-Webhook");
    if let Some(s) = secret {
        req = req.header("x-scout-signature", format!("sha256={}", hmac_sha256_hex(s, &body)));
    }
    match req.body(body).send().await {
        Ok(r) => {
            if !r.status().is_success() {
                crate::logger::warn(
                    "webhook returned an error status",
                    json!({ "url": url, "status": r.status().as_u16() }),
                );
            }
        }
        Err(e) => {
            crate::logger::warn("webhook request failed", json!({ "url": url, "error": e.to_string() }));
        }
    }
}

/// The ticker: every second, run what is due. One spawn per Scout process.
pub fn spawn_ticker(scout: Arc<crate::scout::Scout>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
        // The first tick completes immediately; skip it so a fresh boot does
        // not run everything in the same instant it starts.
        tick.tick().await;
        loop {
            tick.tick().await;
            if scout.is_stopping() {
                return;
            }
            for (id, owner, spec, callback_url, callback_secret) in scout.schedules.take_due() {
                let scout = scout.clone();
                tokio::spawn(async move {
                    scout.run_schedule(&owner, &id, &spec, callback_url, callback_secret).await;
                });
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv_matches_known_vectors() {
        // FNV-1a 64 of empty input and of "a".
        assert_eq!(fnv1a64(b""), 0xcbf29ce484222325);
        assert_eq!(fnv1a64(b"a"), 0xaf63dc4c8601ec8c);
        assert_eq!(fnv1a64(b"hello"), 0xa430d84680aabd0b);
    }

    #[test]
    fn hmac_matches_rfc4231_case_2() {
        // RFC 4231 test case 2: key "Jefe", data "what do ya want for nothing?"
        let out = hmac_sha256_hex("Jefe", b"what do ya want for nothing?");
        assert_eq!(
            out,
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn schedules_persist_across_a_restart() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("scout-test-store-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut cfg = Config::dev();
        cfg.task_store = Some(path.to_string_lossy().to_string());

        let first = Scheduler::new(&cfg);
        let view = first
            .create(
                "key_a",
                json!({ "steps": [{ "op": "navigate", "url": "https://example.com/" }] }),
                60_000,
                None,
                None,
            )
            .unwrap();
        let id = view["id"].as_str().unwrap().to_string();
        assert!(first.record(&id, "succeeded", "run_x", Some(12345)).is_some());

        // A fresh scheduler over the same file sees the schedule and its hash.
        let second = Scheduler::new(&cfg);
        let list = second.list("key_a");
        assert_eq!(list["count"], 1);
        let v = &list["schedules"][0];
        assert_eq!(v["id"], id);
        assert_eq!(v["everyMs"], 60_000);
        assert_eq!(v["lastStatus"], "succeeded");
        // The owner boundary holds after a reload too.
        assert_eq!(second.list("key_b")["count"], 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn change_detection_compares_hashes() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("scout-test-store-cd-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut cfg = Config::dev();
        cfg.task_store = Some(path.to_string_lossy().to_string());
        let s = Scheduler::new(&cfg);
        let v = s
            .create("k", json!({ "steps": [] }), 60_000, None, None)
            .unwrap();
        let id = v["id"].as_str().unwrap().to_string();
        // First successful run is news.
        assert_eq!(s.record(&id, "succeeded", "r1", Some(10)), Some(true));
        // Same hash: nothing changed.
        assert_eq!(s.record(&id, "succeeded", "r2", Some(10)), Some(false));
        // Different hash: news.
        assert_eq!(s.record(&id, "succeeded", "r3", Some(11)), Some(true));
        // Failures do not clobber the baseline.
        assert_eq!(s.record(&id, "failed", "r4", None), Some(false));
        let _ = std::fs::remove_file(&path);
    }
}

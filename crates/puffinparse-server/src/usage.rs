//! Per-key accounting (monthly spend, request counts, the sliding-window rate limiter) and the
//! gateway's table of submitted async jobs.
//!
//! State lives in memory. With `server.state_file` set, spend, counts and jobs are written to a
//! small JSON file after every change (temp file + rename) and reloaded on startup, so a restart
//! does not reset budgets or lose jobs. Rate-limit windows are deliberately not persisted.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Spend and volume for one key in one calendar month (UTC).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct KeyUsage {
    /// `YYYY-MM` the counters belong to; they reset when the month changes.
    pub month: String,
    pub spend_usd: f64,
    pub requests: u64,
    pub pages: u64,
}

/// An async job submitted through `POST /v1/jobs`, keyed by the gateway's own opaque job id.
///
/// Holds no secret: the provider credentials are looked up again from the config at retrieve time
/// (through `alias` / `target_index`, or `[providers.*]`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredJob {
    /// Principal that submitted the job (`"master"`, `"anonymous"` or a virtual key id). Only it
    /// (and the master key) can read the job, and the job's cost is charged to it on success.
    pub owner: String,
    /// The core's handle, as returned by `submit_parse`.
    pub handle: puffinparse_core::JobHandle,
    /// Alias the job was submitted through, if any, and which of its targets served it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    #[serde(default)]
    pub target_index: usize,
    /// `output_format` sent at submit time: the default shape of the retrieved result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_format: Option<String>,
    /// Set once, the first time the job is observed terminal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<JobOutcome>,
    /// Unix seconds of the submission (retention clock).
    pub created_unix: i64,
    /// Set for jobs submitted through `POST /v1/playground/runs`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playground: Option<PlaygroundJob>,
}

/// A playground job's accounting. Its `owner` is `playground:free:<hash>` or
/// `playground:byok:<hash>`; neither is a virtual key, so no monthly usage is recorded for it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlaygroundJob {
    /// Free tier: the Supabase user id whose daily model-pages were reserved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Model-pages reserved at submit time (released on failure, trued up on success).
    #[serde(default)]
    pub reserved_pages: u32,
    /// Free-tier spend reserved at submit time: list price x reserved pages (refunded on failure,
    /// trued up to the job's cost on success).
    #[serde(default)]
    pub reserved_usd: f64,
}

/// Owners of playground jobs start with this; they are not virtual keys.
pub const PLAYGROUND_OWNER_PREFIX: &str = "playground:";

/// Terminal state of a job, recorded the first time it is observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobOutcome {
    Succeeded,
    Failed,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Persisted {
    keys: BTreeMap<String, KeyUsage>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    jobs: BTreeMap<String, StoredJob>,
}

#[derive(Debug, Default)]
struct Inner {
    usage: BTreeMap<String, KeyUsage>,
    windows: HashMap<String, VecDeque<Instant>>,
    jobs: BTreeMap<String, StoredJob>,
}

impl Inner {
    fn add(&mut self, key_id: &str, cost_usd: f64, pages: u32) {
        let month = current_month();
        let u = self.usage.entry(key_id.to_string()).or_default();
        if u.month != month {
            *u = KeyUsage { month, ..KeyUsage::default() };
        }
        u.spend_usd += cost_usd;
        u.requests += 1;
        u.pages += u64::from(pages);
    }
}

#[derive(Debug)]
pub struct UsageStore {
    inner: Mutex<Inner>,
    state_file: Option<PathBuf>,
    /// Serialises file writes so an older snapshot never overwrites a newer one.
    write_lock: Mutex<()>,
}

pub fn current_month() -> String {
    chrono::Utc::now().format("%Y-%m").to_string()
}

impl UsageStore {
    pub fn new(state_file: Option<PathBuf>) -> Result<Self, String> {
        let mut inner = Inner::default();
        if let Some(path) = &state_file {
            match std::fs::read_to_string(path) {
                Ok(text) => {
                    let p: Persisted = serde_json::from_str(&text)
                        .map_err(|e| format!("state file {} is not valid: {e}", path.display()))?;
                    inner.usage = p.keys;
                    inner.jobs = p.jobs;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("cannot read state file {}: {e}", path.display())),
            }
        }
        Ok(Self { inner: Mutex::new(inner), state_file, write_lock: Mutex::new(()) })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// This month's usage for `key_id` (zeroed if the stored month is stale).
    pub fn get(&self, key_id: &str) -> KeyUsage {
        let month = current_month();
        match self.lock().usage.get(key_id) {
            Some(u) if u.month == month => u.clone(),
            _ => KeyUsage { month, ..KeyUsage::default() },
        }
    }

    /// Sliding 60-second window. `Err(secs)` carries a Retry-After hint.
    pub fn check_rate(&self, key_id: &str, rpm: u32) -> Result<(), u64> {
        self.check_rate_at(key_id, rpm, Instant::now())
    }

    fn check_rate_at(&self, key_id: &str, rpm: u32, now: Instant) -> Result<(), u64> {
        let window = Duration::from_secs(60);
        let mut inner = self.lock();
        // Per-IP playground buckets would otherwise accumulate forever: drop idle ones.
        if inner.windows.len() > 4096 {
            inner.windows.retain(|_, q| q.back().is_some_and(|t| now.duration_since(*t) < window));
        }
        let q = inner.windows.entry(key_id.to_string()).or_default();
        while q.front().is_some_and(|t| now.duration_since(*t) >= window) {
            q.pop_front();
        }
        if q.len() >= rpm as usize {
            let oldest = q.front().copied().unwrap_or(now);
            let wait = window.saturating_sub(now.duration_since(oldest));
            return Err(wait.as_secs().max(1));
        }
        q.push_back(now);
        Ok(())
    }

    /// Add one finished request to `key_id`'s month and persist if configured.
    pub fn record(&self, key_id: &str, cost_usd: f64, pages: u32) {
        self.lock().add(key_id, cost_usd, pages);
        self.persist();
    }

    /// Store a submitted job, dropping jobs submitted more than `retention_secs` before it, and
    /// persist.
    pub fn insert_job(&self, id: &str, job: StoredJob, retention_secs: i64) {
        {
            let mut inner = self.lock();
            let cutoff = job.created_unix.saturating_sub(retention_secs);
            inner.jobs.retain(|_, j| j.created_unix >= cutoff);
            inner.jobs.insert(id.to_string(), job);
        }
        self.persist();
    }

    /// A stored job by gateway id.
    pub fn job(&self, id: &str) -> Option<StoredJob> {
        self.lock().jobs.get(id).cloned()
    }

    /// The gateway jobs for a provider's own job id (webhook bodies name only the provider's id).
    /// Usually one, but a provider may hand out the same id twice: LlamaParse returns the cached
    /// job for an identical upload, so two keys submitting the same file share a provider job.
    pub fn find_jobs(&self, provider: &str, provider_job_id: &str) -> Vec<(String, StoredJob)> {
        self.lock()
            .jobs
            .iter()
            .filter(|(_, j)| j.handle.provider == provider && j.handle.job_id == provider_job_id)
            .map(|(id, j)| (id.clone(), j.clone()))
            .collect()
    }

    /// Record a job's terminal state. Only the first call for a job has any effect: it returns
    /// `true` and, for `Succeeded`, charges `cost_usd` / `pages` to the job's owner — exactly
    /// once, even when several retrievals observe the success concurrently.
    pub fn settle_job(&self, id: &str, outcome: JobOutcome, cost_usd: f64, pages: u32) -> bool {
        {
            let mut inner = self.lock();
            let Some(job) = inner.jobs.get_mut(id) else { return false };
            if job.outcome.is_some() {
                return false;
            }
            job.outcome = Some(outcome);
            if outcome == JobOutcome::Succeeded && !job.owner.starts_with(PLAYGROUND_OWNER_PREFIX) {
                let owner = job.owner.clone();
                inner.add(&owner, cost_usd, pages);
            }
        }
        self.persist();
        true
    }

    fn persist(&self) {
        let Some(path) = &self.state_file else { return };
        let _guard = self.write_lock.lock().unwrap_or_else(|p| p.into_inner());
        let snapshot = {
            let inner = self.lock();
            Persisted { keys: inner.usage.clone(), jobs: inner.jobs.clone() }
        };
        let text = match serde_json::to_string_pretty(&snapshot) {
            Ok(t) => t,
            Err(e) => {
                tracing::error!(error = %e, "puffinparse-server: cannot serialise usage state");
                return;
            }
        };
        let tmp = path.with_extension("tmp");
        let result = std::fs::write(&tmp, text).and_then(|()| std::fs::rename(&tmp, path));
        if let Err(e) = result {
            tracing::error!(error = %e, path = %path.display(), "puffinparse-server: cannot write usage state");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_window_slides() {
        let s = UsageStore::new(None).unwrap();
        let t0 = Instant::now();
        assert!(s.check_rate_at("k", 2, t0).is_ok());
        assert!(s.check_rate_at("k", 2, t0 + Duration::from_secs(1)).is_ok());
        let wait = s.check_rate_at("k", 2, t0 + Duration::from_secs(2)).unwrap_err();
        assert_eq!(wait, 58);
        assert!(s.check_rate_at("other", 2, t0).is_ok());
        assert!(s.check_rate_at("k", 2, t0 + Duration::from_secs(60)).is_ok());
    }

    #[test]
    fn persists_and_reloads() {
        let dir = std::env::temp_dir().join(format!("puffinparse-usage-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        let s = UsageStore::new(Some(path.clone())).unwrap();
        s.record("a", 0.25, 3);
        s.record("a", 0.5, 1);
        let reloaded = UsageStore::new(Some(path)).unwrap();
        let u = reloaded.get("a");
        assert_eq!((u.requests, u.pages), (2, 4));
        assert!((u.spend_usd - 0.75).abs() < 1e-9);
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn job(owner: &str, provider_job_id: &str, created_unix: i64) -> StoredJob {
        StoredJob {
            owner: owner.into(),
            handle: puffinparse_core::JobHandle::new("reducto", "reducto/standard", provider_job_id),
            alias: None,
            target_index: 0,
            output_format: None,
            outcome: None,
            created_unix,
            playground: None,
        }
    }

    #[test]
    fn jobs_settle_once_persist_and_expire() {
        let dir = std::env::temp_dir().join(format!("puffinparse-jobs-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        let s = UsageStore::new(Some(path.clone())).unwrap();
        s.insert_job("job_old", job("a", "p0", 1_000), 100);
        s.insert_job("job_1", job("a", "p1", 2_000), 100);
        assert!(s.job("job_old").is_none(), "expired on insert");
        s.insert_job("job_2", job("b", "p1", 2_001), 100);
        let ids: Vec<String> = s.find_jobs("reducto", "p1").into_iter().map(|(id, _)| id).collect();
        assert_eq!(ids, ["job_1", "job_2"]);
        assert!(s.find_jobs("extend", "p1").is_empty());
        assert!(s.settle_job("job_1", JobOutcome::Succeeded, 0.5, 2));
        assert!(!s.settle_job("job_1", JobOutcome::Succeeded, 0.5, 2), "charged only once");
        assert!(!s.settle_job("missing", JobOutcome::Failed, 0.0, 0));
        let reloaded = UsageStore::new(Some(path)).unwrap();
        assert_eq!(reloaded.job("job_1").unwrap().outcome, Some(JobOutcome::Succeeded));
        let u = reloaded.get("a");
        assert_eq!((u.requests, u.pages), (1, 2));
        assert!((u.spend_usd - 0.5).abs() < 1e-9);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stale_month_resets() {
        let s = UsageStore::new(None).unwrap();
        s.lock().usage.insert("a".into(), KeyUsage { month: "1999-01".into(), spend_usd: 9.0, requests: 9, pages: 9 });
        assert_eq!(s.get("a").spend_usd, 0.0);
        s.record("a", 1.0, 1);
        assert_eq!(s.get("a").requests, 1);
    }
}

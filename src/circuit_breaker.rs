use crate::metrics;
use dashmap::{mapref::one::Ref, DashMap};
use std::sync::{
    atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering},
    Arc,
};
use std::time::{SystemTime, UNIX_EPOCH};

pub const STATE_CLOSED: u8 = 0;
pub const STATE_OPEN: u8 = 1;
pub const STATE_HALF_OPEN: u8 = 2;

pub struct ProjectHealth {
    pub consecutive_failures: AtomicU32,
    pub last_failure_epoch: AtomicU64,
    pub state: AtomicU8,
}

impl Default for ProjectHealth {
    fn default() -> Self {
        Self {
            consecutive_failures: AtomicU32::new(0),
            last_failure_epoch: AtomicU64::new(0),
            state: AtomicU8::new(STATE_CLOSED),
        }
    }
}

#[derive(Clone)]
pub struct CircuitBreaker {
    projects: Arc<DashMap<String, ProjectHealth>>,
    threshold: u32,
    timeout_secs: u64,
}

impl CircuitBreaker {
    pub fn new(threshold: u32, timeout_secs: u64) -> Self {
        Self {
            projects: Arc::new(DashMap::new()),
            threshold: threshold.max(1),
            timeout_secs,
        }
    }

    pub fn is_available(&self, project: &str) -> bool {
        let health = self.project_health(project);
        match health.state.load(Ordering::Acquire) {
            STATE_CLOSED => true,
            STATE_OPEN => {
                let now = now_epoch_secs();
                let last_failure = health.last_failure_epoch.load(Ordering::Acquire);
                if now.saturating_sub(last_failure) >= self.timeout_secs {
                    if health
                        .state
                        .compare_exchange(
                            STATE_OPEN,
                            STATE_HALF_OPEN,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        metrics::global()
                            .circuit_breaker_state
                            .with_label_values(&[project])
                            .set(STATE_HALF_OPEN as f64);
                    }
                    true
                } else {
                    false
                }
            }
            STATE_HALF_OPEN => true,
            _ => true,
        }
    }

    pub fn record_success(&self, project: &str) {
        let health = self.project_health(project);
        health.consecutive_failures.store(0, Ordering::Release);

        let mut current = health.state.load(Ordering::Acquire);
        while current != STATE_CLOSED {
            match health.state.compare_exchange(
                current,
                STATE_CLOSED,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }

        metrics::global()
            .circuit_breaker_state
            .with_label_values(&[project])
            .set(STATE_CLOSED as f64);
    }

    pub fn record_failure(&self, project: &str) {
        let health = self.project_health(project);
        let failures = health.consecutive_failures.fetch_add(1, Ordering::AcqRel) + 1;
        health
            .last_failure_epoch
            .store(now_epoch_secs(), Ordering::Release);

        if failures >= self.threshold {
            let mut current = health.state.load(Ordering::Acquire);
            while current != STATE_OPEN {
                match health.state.compare_exchange(
                    current,
                    STATE_OPEN,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => break,
                    Err(actual) => current = actual,
                }
            }
        }

        metrics::global()
            .circuit_breaker_state
            .with_label_values(&[project])
            .set(health.state.load(Ordering::Acquire) as f64);
    }

    fn project_health(&self, project: &str) -> Ref<'_, String, ProjectHealth> {
        self.projects.entry(project.to_string()).or_default();
        self.projects
            .get(project)
            .expect("project health entry should exist")
    }
}

#[inline]
fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    fn state_for(cb: &CircuitBreaker, project: &str) -> u8 {
        cb.projects
            .get(project)
            .map(|entry| entry.state.load(Ordering::Acquire))
            .unwrap_or(STATE_CLOSED)
    }

    #[test]
    fn test_circuit_breaker_opens_after_threshold() {
        let cb = CircuitBreaker::new(2, 30);
        let project = "dockerhub";

        assert!(cb.is_available(project));
        cb.record_failure(project);
        assert!(cb.is_available(project));

        cb.record_failure(project);
        assert_eq!(state_for(&cb, project), STATE_OPEN);
        assert!(!cb.is_available(project));

        let rendered = metrics::render().expect("render metrics");
        assert!(rendered.contains("harbor_router_circuit_breaker_state"));
    }

    #[test]
    fn test_circuit_breaker_recovers_after_timeout() {
        let cb = CircuitBreaker::new(1, 1);
        let project = "ghcr";

        cb.record_failure(project);
        assert!(!cb.is_available(project));

        thread::sleep(Duration::from_millis(1100));
        assert!(cb.is_available(project));
    }

    #[test]
    fn test_circuit_breaker_half_open() {
        let cb = CircuitBreaker::new(1, 1);
        let project = "quay";

        cb.record_failure(project);
        assert_eq!(state_for(&cb, project), STATE_OPEN);
        assert!(!cb.is_available(project));

        thread::sleep(Duration::from_millis(1100));
        assert!(cb.is_available(project));
        assert_eq!(state_for(&cb, project), STATE_HALF_OPEN);

        cb.record_success(project);
        assert_eq!(state_for(&cb, project), STATE_CLOSED);
        assert!(cb.is_available(project));
    }
}

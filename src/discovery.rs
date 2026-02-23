use crate::{cache, metrics};
use anyhow::{bail, Context, Result};
use arc_swap::ArcSwap;
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, error, info};

const DISCOVERY_CACHE_KEY: &str = "discovery:projects";

/// Represents the relevant fields from Harbor's GET /api/v2.0/projects response.
#[derive(Debug, Deserialize)]
struct HarborProject {
    name: String,
    registry_id: Option<serde_json::Value>,
}

/// Validates that a project name is safe for use in URL path construction.
///
/// Rejects names containing path traversal sequences (`..`, `/`), empty names,
/// and names with control characters. This prevents cache poisoning attacks
/// where a compromised Redis could inject malicious project names to access
/// unintended Harbor API endpoints.
pub(crate) fn is_safe_project_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains("..")
        && !name.contains(|c: char| c.is_control())
        && name == name.trim()
}

/// Discoverer periodically queries the Harbor API to find all proxy-cache projects
/// (projects where `registry_id` is not null).
///
/// Uses `ArcSwap` for the project list so that `get_projects()` is entirely lock-free
/// on the hot path — equivalent to the `atomic.Value` in the Go implementation.
///
/// When a shared cache (Redis) is configured, the discovered project list is written
/// to Redis after each successful refresh and seeded from Redis on startup for an
/// instant warm start across pod restarts or rolling updates.
///
/// # Security
/// Credentials are stored as `SecretString` and only exposed when making API calls.
#[derive(Clone)]
pub struct Discoverer {
    inner: Arc<Inner>,
}

struct Inner {
    harbor_url: String,
    username: SecretString,
    password: SecretString,
    client: reqwest::Client,
    /// ArcSwap<Vec<String>> — reads are lock-free.
    projects: ArcSwap<Vec<String>>,
    /// Optional shared cache for cross-pod discovery seeding.
    cache: Option<cache::Cache>,
}

impl Discoverer {
    pub fn new(
        harbor_url: &str,
        username: SecretString,
        password: SecretString,
        cache: Option<cache::Cache>,
    ) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()?;

        Ok(Self {
            inner: Arc::new(Inner {
                harbor_url: harbor_url.to_string(),
                username,
                password,
                client,
                projects: ArcSwap::from_pointee(Vec::new()),
                cache,
            }),
        })
    }

    /// Returns the current list of discovered proxy-cache project names.
    /// Lock-free — safe to call at any RPS.
    pub fn get_projects(&self) -> Arc<Vec<String>> {
        self.inner.projects.load_full()
    }

    /// Runs the background discovery loop. Seeds from cache first (instant warm start),
    /// then performs an initial API fetch, and re-discovers at `interval`.
    pub async fn start(
        &self,
        interval: Duration,
        mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
    ) {
        self.seed_from_cache().await;
        self.refresh().await;
        let mut ticker = tokio::time::interval(interval);
        ticker.tick().await; // consume the immediate first tick
        loop {
            tokio::select! {
                _ = ticker.tick() => self.refresh().await,
                _ = shutdown_rx.changed() => {
                    info!(event = "discovery", "shutting down gracefully");
                    break;
                }
            }
        }
    }

    /// Attempts to load the project list from the shared cache (Redis).
    /// Provides an instant warm start when a new pod comes up while other pods
    /// have already discovered projects.
    async fn seed_from_cache(&self) {
        let Some(ref cache) = self.inner.cache else {
            return;
        };
        match cache.get(DISCOVERY_CACHE_KEY).await {
            Some(json) => match serde_json::from_str::<Vec<String>>(&json) {
                Ok(projects) if !projects.is_empty() => {
                    let (safe, rejected): (Vec<_>, Vec<_>) =
                        projects.into_iter().partition(|n| is_safe_project_name(n));
                    if !rejected.is_empty() {
                        error!(
                            event = "discovery",
                            rejected_count = rejected.len(),
                            "rejected unsafe project names from cache (possible cache poisoning)"
                        );
                    }
                    if safe.is_empty() {
                        debug!(
                            event = "discovery",
                            "all cached project names were rejected, will fetch from API"
                        );
                        return;
                    }
                    let count = safe.len();
                    self.inner.projects.store(Arc::new(safe));
                    metrics::global().discovered_projects.set(count as f64);
                    info!(
                        event = "discovery",
                        project_count = count,
                        source = "cache",
                        "seeded projects from cache"
                    );
                }
                Ok(_) => {
                    debug!(
                        event = "discovery",
                        "cache had empty project list, skipping seed"
                    );
                }
                Err(e) => {
                    debug!(
                        event = "discovery",
                        error = %e,
                        "failed to parse cached project list, will fetch from API"
                    );
                }
            },
            None => {
                debug!(event = "discovery", "no cached project list found");
            }
        }
    }

    /// Writes the project list to the shared cache for cross-pod seeding.
    async fn persist_to_cache(&self, projects: &[String]) {
        let Some(ref cache) = self.inner.cache else {
            return;
        };
        match serde_json::to_string(projects) {
            Ok(json) => {
                cache.set(DISCOVERY_CACHE_KEY.to_string(), json).await;
                debug!(
                    event = "discovery",
                    project_count = projects.len(),
                    "persisted projects to cache"
                );
            }
            Err(e) => {
                debug!(event = "discovery", error = %e, "failed to serialize projects for cache");
            }
        }
    }

    async fn refresh(&self) {
        match self.fetch_proxy_cache_projects().await {
            Ok(projects) => {
                let count = projects.len();
                self.persist_to_cache(&projects).await;
                self.inner.projects.store(Arc::new(projects));
                metrics::global().discovered_projects.set(count as f64);
                info!(
                    event = "discovery",
                    project_count = count,
                    result = "ok",
                    "discovered proxy-cache projects"
                );
            }
            Err(e) => {
                error!(
                    event = "discovery",
                    error = %e,
                    result = "error",
                    "failed to discover proxy-cache projects"
                );
            }
        }
    }

    /// Paginates through all Harbor projects and returns the names of those
    /// with a non-null `registry_id` (proxy-cache projects).
    async fn fetch_proxy_cache_projects(&self) -> Result<Vec<String>> {
        let mut result = Vec::new();
        let mut page = 1u32;
        let page_size = 100u32;

        loop {
            let url = format!(
                "{}/api/v2.0/projects?page={}&page_size={}&with_detail=true",
                self.inner.harbor_url, page, page_size
            );

            let resp = self
                .inner
                .client
                .get(&url)
                .basic_auth(
                    self.inner.username.expose_secret(),
                    Some(self.inner.password.expose_secret()),
                )
                .send()
                .await
                .context("execute discovery request")?;

            let status = resp.status();
            if status == reqwest::StatusCode::UNAUTHORIZED
                || status == reqwest::StatusCode::FORBIDDEN
            {
                bail!(
                    "harbor API auth failed (status {}): check HARBOR_USERNAME/HARBOR_PASSWORD",
                    status
                );
            }
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                bail!("unexpected status {}: {}", status, body);
            }

            let projects: Vec<HarborProject> =
                resp.json().await.context("unmarshal projects response")?;

            let fetched = projects.len();
            for p in projects {
                if p.registry_id.is_some() {
                    if is_safe_project_name(&p.name) {
                        result.push(p.name);
                    } else {
                        error!(
                            event = "discovery",
                            project = p.name,
                            "skipped project with unsafe name from Harbor API"
                        );
                    }
                }
            }

            if fetched < page_size as usize {
                break; // last page
            }
            page += 1;
        }

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_project_names() {
        assert!(is_safe_project_name("dockerhub"));
        assert!(is_safe_project_name("my-project"));
        assert!(is_safe_project_name("project_123"));
        assert!(is_safe_project_name("a"));
    }

    #[test]
    fn rejects_empty() {
        assert!(!is_safe_project_name(""));
    }

    #[test]
    fn rejects_path_traversal() {
        assert!(!is_safe_project_name(".."));
        assert!(!is_safe_project_name("../admin"));
        assert!(!is_safe_project_name("../../admin"));
        assert!(!is_safe_project_name("foo..bar"));
    }

    #[test]
    fn rejects_slashes() {
        assert!(!is_safe_project_name("foo/bar"));
        assert!(!is_safe_project_name("/leading"));
        assert!(!is_safe_project_name("trailing/"));
        assert!(!is_safe_project_name("foo\\bar"));
    }

    #[test]
    fn rejects_control_characters() {
        assert!(!is_safe_project_name("has\x00null"));
        assert!(!is_safe_project_name("has\nnewline"));
        assert!(!is_safe_project_name("has\ttab"));
        assert!(!is_safe_project_name("has\rreturn"));
    }

    #[test]
    fn rejects_whitespace_padding() {
        assert!(!is_safe_project_name(" leading"));
        assert!(!is_safe_project_name("trailing "));
        assert!(!is_safe_project_name(" both "));
    }
}

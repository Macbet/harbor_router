use crate::metrics;
use anyhow::{bail, Context, Result};
use arc_swap::ArcSwap;
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info};

/// Represents the relevant fields from Harbor's GET /api/v2.0/projects response.
#[derive(Debug, Deserialize)]
struct HarborProject {
    name: String,
    registry_id: Option<serde_json::Value>,
}

/// Discoverer periodically queries the Harbor API to find all proxy-cache projects
/// (projects where `registry_id` is not null).
///
/// Uses `ArcSwap` for the project list so that `get_projects()` is entirely lock-free
/// on the hot path — equivalent to the `atomic.Value` in the Go implementation.
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
}

impl Discoverer {
    pub fn new(harbor_url: &str, username: SecretString, password: SecretString) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .expect("build discovery http client");

        Self {
            inner: Arc::new(Inner {
                harbor_url: harbor_url.to_string(),
                username,
                password,
                client,
                projects: ArcSwap::from_pointee(Vec::new()),
            }),
        }
    }

    /// Returns the current list of discovered proxy-cache project names.
    /// Lock-free — safe to call at any RPS.
    pub fn get_projects(&self) -> Arc<Vec<String>> {
        self.inner.projects.load_full()
    }

    /// Runs the background discovery loop. Performs an initial fetch, then
    /// re-discovers at `interval`. Cancels when the provided `CancellationToken`
    /// future resolves (i.e. when the owning task is dropped / abort is signalled).
    pub async fn start(&self, interval: Duration) {
        self.refresh().await;

        let mut ticker = tokio::time::interval(interval);
        ticker.tick().await; // consume the immediate first tick
        loop {
            ticker.tick().await;
            self.refresh().await;
        }
    }

    async fn refresh(&self) {
        match self.fetch_proxy_cache_projects().await {
            Ok(projects) => {
                let count = projects.len();
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
                    result.push(p.name);
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

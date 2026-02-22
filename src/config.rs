use anyhow::{bail, Result};
use secrecy::{ExposeSecret, SecretString};
use std::time::Duration;

/// All configuration for harbor-router, loaded from environment variables.
///
/// Supports Vault Agent Injector: credentials can be read from files instead of
/// environment variables. Set `HARBOR_USERNAME_FILE` and `HARBOR_PASSWORD_FILE`
/// to paths where Vault injects the secrets (e.g., `/vault/secrets/username`).
///
/// # Security
/// Credentials are wrapped in `SecretString` to prevent accidental logging.
/// The `Debug` implementation redacts sensitive fields.
pub struct Config {
    // Harbor connection
    pub harbor_url: String,
    pub harbor_username: SecretString,
    pub harbor_password: SecretString,
    pub discovery_interval: Duration,

    // Resolver
    pub resolver_timeout: Duration,
    pub cache_ttl: Duration,

    // Connection pool (tuned for 500k RPS)
    pub max_idle_conns_per_host: usize,
    #[allow(dead_code)] // Reserved for future use
    pub max_conns_per_host: usize,
    pub idle_conn_timeout: Duration,

    // Performance tuning
    pub listen_backlog: u32,

    // Server
    pub listen_addr: String,
    pub metrics_addr: String,

    // Routing
    pub proxy_project: String,

    // Security settings
    /// Maximum number of projects to fan out to (DoS protection).
    pub max_fanout_projects: usize,
    /// Use HTTP/2 prior knowledge (set to false if Harbor is behind HTTP/1.1 proxy).
    pub http2_prior_knowledge: bool,
    /// Maximum requests per IP per second (0 = unlimited).
    pub rate_limit_per_ip: u32,

    // Redis cache (optional — when set, uses Redis Sentinel instead of in-memory Moka)
    /// Comma-separated Redis Sentinel endpoints (e.g. "sentinel1:26379,sentinel2:26379").
    /// Leave empty to use the default in-memory cache.
    pub redis_sentinels: String,
    /// Sentinel master group name (default: "mymaster").
    pub redis_master_name: String,
    /// Optional Redis AUTH password.
    pub redis_password: Option<SecretString>,
    /// Redis database number (default: 0).
    pub redis_db: u8,
    /// Key prefix for cache entries (default: "hr").
    pub redis_key_prefix: String,

    // Observability
    pub log_level: String,
    pub log_format: String,
    #[allow(dead_code)] // Reserved for future pprof endpoint
    pub enable_pprof: bool,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("harbor_url", &self.harbor_url)
            .field("harbor_username", &"[REDACTED]")
            .field("harbor_password", &"[REDACTED]")
            .field("discovery_interval", &self.discovery_interval)
            .field("resolver_timeout", &self.resolver_timeout)
            .field("cache_ttl", &self.cache_ttl)
            .field("max_idle_conns_per_host", &self.max_idle_conns_per_host)
            .field("max_conns_per_host", &self.max_conns_per_host)
            .field("idle_conn_timeout", &self.idle_conn_timeout)
            .field("listen_backlog", &self.listen_backlog)
            .field("listen_addr", &self.listen_addr)
            .field("metrics_addr", &self.metrics_addr)
            .field("proxy_project", &self.proxy_project)
            .field("max_fanout_projects", &self.max_fanout_projects)
            .field("http2_prior_knowledge", &self.http2_prior_knowledge)
            .field("rate_limit_per_ip", &self.rate_limit_per_ip)
            .field("redis_sentinels", &self.redis_sentinels)
            .field("redis_master_name", &self.redis_master_name)
            .field("redis_password", &"[REDACTED]")
            .field("redis_db", &self.redis_db)
            .field("redis_key_prefix", &self.redis_key_prefix)
            .field("log_level", &self.log_level)
            .field("log_format", &self.log_format)
            .field("enable_pprof", &self.enable_pprof)
            .finish()
    }
}

impl Clone for Config {
    fn clone(&self) -> Self {
        Self {
            harbor_url: self.harbor_url.clone(),
            harbor_username: SecretString::from(self.harbor_username.expose_secret().to_string()),
            harbor_password: SecretString::from(self.harbor_password.expose_secret().to_string()),
            discovery_interval: self.discovery_interval,
            resolver_timeout: self.resolver_timeout,
            cache_ttl: self.cache_ttl,
            max_idle_conns_per_host: self.max_idle_conns_per_host,
            max_conns_per_host: self.max_conns_per_host,
            idle_conn_timeout: self.idle_conn_timeout,
            listen_backlog: self.listen_backlog,
            listen_addr: self.listen_addr.clone(),
            metrics_addr: self.metrics_addr.clone(),
            proxy_project: self.proxy_project.clone(),
            max_fanout_projects: self.max_fanout_projects,
            http2_prior_knowledge: self.http2_prior_knowledge,
            rate_limit_per_ip: self.rate_limit_per_ip,
            redis_sentinels: self.redis_sentinels.clone(),
            redis_master_name: self.redis_master_name.clone(),
            redis_password: self
                .redis_password
                .as_ref()
                .map(|s| SecretString::from(s.expose_secret().to_string())),
            redis_db: self.redis_db,
            redis_key_prefix: self.redis_key_prefix.clone(),
            log_level: self.log_level.clone(),
            log_format: self.log_format.clone(),
            enable_pprof: self.enable_pprof,
        }
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        let harbor_url = env_str("HARBOR_URL", "http://harbor-core:80");

        // Support both direct env vars and file-based secrets (Vault injector).
        // Priority: *_FILE env var > direct env var
        let harbor_username = env_str_or_file("HARBOR_USERNAME")?;
        let harbor_password = env_str_or_file("HARBOR_PASSWORD")?;

        if harbor_url.is_empty() {
            bail!("HARBOR_URL must be set");
        }
        if harbor_username.is_empty() || harbor_password.is_empty() {
            bail!(
                "HARBOR_USERNAME and HARBOR_PASSWORD must be set \
                 (via env var or *_FILE pointing to Vault-injected secret)"
            );
        }

        Ok(Self {
            harbor_url,
            harbor_username: SecretString::from(harbor_username),
            harbor_password: SecretString::from(harbor_password),
            discovery_interval: env_duration("DISCOVERY_INTERVAL", Duration::from_secs(60)),
            resolver_timeout: env_duration("RESOLVER_TIMEOUT", Duration::from_secs(10)),
            cache_ttl: env_duration("CACHE_TTL", Duration::from_secs(300)),
            // Connection pool defaults tuned for 500k RPS
            max_idle_conns_per_host: env_usize("MAX_IDLE_CONNS_PER_HOST", 512),
            max_conns_per_host: env_usize("MAX_CONNS_PER_HOST", 0),
            idle_conn_timeout: env_duration("IDLE_CONN_TIMEOUT", Duration::from_secs(90)),
            // Performance tuning
            listen_backlog: env_u32("LISTEN_BACKLOG", 8192),
            listen_addr: env_str("LISTEN_ADDR", ":8080"),
            metrics_addr: env_str("METRICS_ADDR", ":9090"),
            proxy_project: env_str("PROXY_PROJECT", "proxy"),
            // Security settings
            max_fanout_projects: env_usize("MAX_FANOUT_PROJECTS", 50),
            http2_prior_knowledge: env_bool("HTTP2_PRIOR_KNOWLEDGE", false),
            rate_limit_per_ip: env_u32("RATE_LIMIT_PER_IP", 0), // 0 = unlimited
            // Redis cache (optional)
            redis_sentinels: env_str("REDIS_SENTINELS", ""),
            redis_master_name: env_str("REDIS_MASTER_NAME", "mymaster"),
            redis_password: {
                let v = env_str_or_file("REDIS_PASSWORD")?;
                if v.is_empty() {
                    None
                } else {
                    Some(SecretString::from(v))
                }
            },
            redis_db: {
                let v = env_u32("REDIS_DB", 0);
                if v > 15 {
                    bail!("REDIS_DB must be 0-15, got {}", v);
                }
                v as u8
            },
            redis_key_prefix: env_str("REDIS_KEY_PREFIX", "hr"),
            // Observability
            log_level: env_str("LOG_LEVEL", "info"),
            log_format: env_str("LOG_FORMAT", "pretty"), // "pretty" or "json"
            enable_pprof: env_bool("ENABLE_PPROF", false),
        })
    }
}

/// Reads a secret from either:
/// 1. `{KEY}_FILE` env var pointing to a file (Vault injector pattern)
/// 2. `{KEY}` env var directly
///
/// File-based takes precedence when both are set.
fn env_str_or_file(key: &str) -> Result<String> {
    let file_key = format!("{}_FILE", key);

    // Check for file-based secret first (Vault injector)
    if let Ok(path) = std::env::var(&file_key) {
        if !path.is_empty() {
            match std::fs::read_to_string(&path) {
                Ok(contents) => return Ok(contents.trim().to_string()),
                Err(e) => bail!("failed to read {} from {}: {}", key, path, e),
            }
        }
    }

    // Fall back to direct env var
    Ok(std::env::var(key).unwrap_or_default())
}

fn env_str(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn env_duration(key: &str, default: Duration) -> Duration {
    let v = match std::env::var(key) {
        Ok(v) if !v.is_empty() => v,
        _ => return default,
    };
    // Accept Go-style durations: e.g. "60s", "5m", "1h".
    parse_go_duration(&v).unwrap_or(default)
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn env_u32(key: &str, default: u32) -> u32 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn env_bool(key: &str, default: bool) -> bool {
    match std::env::var(key).as_deref() {
        Ok("true") | Ok("1") | Ok("yes") => true,
        Ok("false") | Ok("0") | Ok("no") => false,
        _ => default,
    }
}

/// Parses a subset of Go duration strings: "300ms", "10s", "5m", "1h", "2h30m".
fn parse_go_duration(s: &str) -> Option<Duration> {
    let mut total_ms: u64 = 0;
    let mut buf = String::new();

    for ch in s.chars() {
        if ch.is_ascii_digit() || ch == '.' {
            buf.push(ch);
        } else {
            let n: f64 = buf.parse().ok()?;
            buf.clear();
            let ms = match ch {
                's' => (n * 1_000.0) as u64,
                'm' => (n * 60_000.0) as u64,
                'h' => (n * 3_600_000.0) as u64,
                _ => return None,
            };
            total_ms += ms;
        }
    }
    if !buf.is_empty() {
        return None; // trailing number without unit
    }
    Some(Duration::from_millis(total_ms))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_parse_go_duration_seconds() {
        assert_eq!(parse_go_duration("10s"), Some(Duration::from_secs(10)));
        assert_eq!(parse_go_duration("1s"), Some(Duration::from_secs(1)));
        assert_eq!(parse_go_duration("0s"), Some(Duration::from_secs(0)));
    }

    #[test]
    fn test_parse_go_duration_minutes() {
        assert_eq!(parse_go_duration("5m"), Some(Duration::from_secs(300)));
        assert_eq!(parse_go_duration("1m"), Some(Duration::from_secs(60)));
    }

    #[test]
    fn test_parse_go_duration_hours() {
        assert_eq!(parse_go_duration("1h"), Some(Duration::from_secs(3600)));
        assert_eq!(parse_go_duration("2h"), Some(Duration::from_secs(7200)));
    }

    #[test]
    fn test_parse_go_duration_combined() {
        assert_eq!(parse_go_duration("1h30m"), Some(Duration::from_secs(5400)));
        assert_eq!(
            parse_go_duration("2h30m15s"),
            Some(Duration::from_secs(9015))
        );
        assert_eq!(parse_go_duration("1m30s"), Some(Duration::from_secs(90)));
    }

    #[test]
    fn test_parse_go_duration_fractional() {
        assert_eq!(parse_go_duration("1.5s"), Some(Duration::from_millis(1500)));
        assert_eq!(parse_go_duration("0.5m"), Some(Duration::from_secs(30)));
    }

    #[test]
    fn test_parse_go_duration_invalid() {
        assert_eq!(parse_go_duration("10"), None); // no unit
        assert_eq!(parse_go_duration("10x"), None); // invalid unit
        assert_eq!(parse_go_duration(""), Some(Duration::from_millis(0))); // empty is valid (0)
    }

    #[test]
    fn test_env_str_or_file_from_env() {
        std::env::set_var("TEST_SECRET_1", "from_env");
        std::env::remove_var("TEST_SECRET_1_FILE");

        let result = env_str_or_file("TEST_SECRET_1").unwrap();
        assert_eq!(result, "from_env");

        std::env::remove_var("TEST_SECRET_1");
    }

    #[test]
    fn test_env_str_or_file_from_file() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_secret_file");

        let mut file = std::fs::File::create(&path).unwrap();
        writeln!(file, "from_file_with_whitespace  ").unwrap();
        drop(file);

        std::env::remove_var("TEST_SECRET_2");
        std::env::set_var("TEST_SECRET_2_FILE", path.to_str().unwrap());

        let result = env_str_or_file("TEST_SECRET_2").unwrap();
        assert_eq!(result, "from_file_with_whitespace"); // trimmed

        std::env::remove_var("TEST_SECRET_2_FILE");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_env_str_or_file_file_takes_precedence() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_secret_file_precedence");

        let mut file = std::fs::File::create(&path).unwrap();
        write!(file, "from_file").unwrap();
        drop(file);

        std::env::set_var("TEST_SECRET_3", "from_env");
        std::env::set_var("TEST_SECRET_3_FILE", path.to_str().unwrap());

        let result = env_str_or_file("TEST_SECRET_3").unwrap();
        assert_eq!(result, "from_file"); // file takes precedence

        std::env::remove_var("TEST_SECRET_3");
        std::env::remove_var("TEST_SECRET_3_FILE");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_env_str_or_file_missing_file_error() {
        std::env::set_var("TEST_SECRET_4_FILE", "/nonexistent/path/to/secret");

        let result = env_str_or_file("TEST_SECRET_4");
        assert!(result.is_err());

        std::env::remove_var("TEST_SECRET_4_FILE");
    }

    #[test]
    fn test_env_str_or_file_empty_returns_empty() {
        std::env::remove_var("TEST_SECRET_5");
        std::env::remove_var("TEST_SECRET_5_FILE");

        let result = env_str_or_file("TEST_SECRET_5").unwrap();
        assert_eq!(result, "");
    }
}

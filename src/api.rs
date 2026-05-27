use anyhow::Result;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;

use crate::cache::ResponseCache;

#[derive(Deserialize)]
struct CrateVersionResponse {
    version: VersionInfo,
}

#[derive(Deserialize)]
struct VersionInfo {
    created_at: DateTime<Utc>,
}

#[derive(Deserialize)]
struct CrateResponse {
    versions: Vec<CrateVersionInfo>,
}

#[derive(Deserialize, Serialize, Clone)]
pub struct CrateVersionInfo {
    pub num: String,
    pub created_at: DateTime<Utc>,
    pub yanked: bool,
}

/// Classifies API fetch errors for retry decision-making.
#[derive(Debug)]
pub enum FetchError {
    /// Retryable: network timeout, connection error, 429, 5xx
    Retryable(String),
    /// Permanent: 4xx (other than 404/429), parse errors
    Permanent(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Retryable(msg) => write!(f, "{msg}"),
            FetchError::Permanent(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for FetchError {}

pub struct CratesIoClient {
    agent: ureq::Agent,
    cache: ResponseCache,
    cache_max_age_hours: u64,
    last_was_cache_hit: bool,
}

impl CratesIoClient {
    pub fn new(timeout_secs: u64, cache_path: Option<&Path>, cache_max_age_hours: u64) -> Result<Self> {
        let agent = ureq::Agent::config_builder()
            .user_agent("cargo-oxidate/0.1 (https://github.com/timweri/cargo-oxidate)")
            .timeout_global(Some(Duration::from_secs(timeout_secs)))
            .http_status_as_error(false)
            .build()
            .new_agent();

        let cache = ResponseCache::load(cache_path);

        Ok(Self {
            agent,
            cache,
            cache_max_age_hours,
            last_was_cache_hit: false,
        })
    }

    pub fn save_cache(&self) {
        if let Err(e) = self.cache.save() {
            eprintln!("Warning: failed to save cache: {e}");
        }
    }

    /// Sleeps for the inter-request rate limit window if the most recent fetch
    /// hit the network. No-op if it was served from cache.
    pub fn rate_limit(&self) {
        if !self.last_was_cache_hit {
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn fetch_publish_date_uncached(
        &self,
        name: &str,
        version: &str,
    ) -> Result<Option<DateTime<Utc>>, FetchError> {
        let url = format!("https://crates.io/api/v1/crates/{name}/{version}");

        let mut response = match self.agent.get(&url).call() {
            Ok(resp) => resp,
            Err(e) => {
                // Classify ureq errors (network/timeout)
                return Err(FetchError::Retryable(format!(
                    "Network error for {name}@{version}: {e}"
                )));
            }
        };

        let status = response.status();

        if status == 404 {
            return Ok(None);
        }

        if status == 429 {
            return Err(FetchError::Retryable(format!(
                "Rate limited for {name}@{version}"
            )));
        }

        if status.is_server_error() {
            return Err(FetchError::Retryable(format!(
                "Server error {status} for {name}@{version}"
            )));
        }

        if status.is_client_error() {
            return Err(FetchError::Permanent(format!(
                "Client error {status} for {name}@{version}"
            )));
        }

        let data: CrateVersionResponse = response.body_mut().read_json().map_err(|e| {
            FetchError::Permanent(format!(
                "Failed to parse response for {name}@{version}: {e}"
            ))
        })?;

        Ok(Some(data.version.created_at))
    }

    pub fn fetch_publish_date_with_retry(
        &mut self,
        name: &str,
        version: &str,
    ) -> Result<Option<DateTime<Utc>>, FetchError> {
        if let Some(date) = self.cache.get_publish_date(name, version) {
            self.last_was_cache_hit = true;
            return Ok(Some(date));
        }

        self.last_was_cache_hit = false;
        let mut last_error = None;

        for attempt in 0..3 {
            match self.fetch_publish_date_uncached(name, version) {
                Ok(result) => {
                    if let Some(date) = result {
                        self.cache.set_publish_date(name, version, date);
                    }
                    return Ok(result);
                }
                Err(FetchError::Permanent(msg)) => return Err(FetchError::Permanent(msg)),
                Err(e) => {
                    if attempt < 2 {
                        std::thread::sleep(Duration::from_millis(500));
                    }
                    last_error = Some(e);
                }
            }
        }

        Err(last_error.unwrap())
    }

    fn fetch_all_versions_uncached(&self, name: &str) -> Result<Vec<CrateVersionInfo>, FetchError> {
        let url = format!("https://crates.io/api/v1/crates/{name}");

        let mut response = match self.agent.get(&url).call() {
            Ok(resp) => resp,
            Err(e) => {
                return Err(FetchError::Retryable(format!(
                    "Network error fetching versions for {name}: {e}"
                )));
            }
        };

        let status = response.status();

        if status == 404 {
            return Ok(Vec::new());
        }

        if status == 429 {
            return Err(FetchError::Retryable(format!(
                "Rate limited fetching versions for {name}"
            )));
        }

        if status.is_server_error() {
            return Err(FetchError::Retryable(format!(
                "Server error {status} fetching versions for {name}"
            )));
        }

        if status.is_client_error() {
            return Err(FetchError::Permanent(format!(
                "Client error {status} fetching versions for {name}"
            )));
        }

        let data: CrateResponse = response.body_mut().read_json().map_err(|e| {
            FetchError::Permanent(format!("Failed to parse versions response for {name}: {e}"))
        })?;

        Ok(data.versions)
    }

    pub fn fetch_all_versions_with_retry(
        &mut self,
        name: &str,
    ) -> Result<Vec<CrateVersionInfo>, FetchError> {
        let max_age = ChronoDuration::hours(self.cache_max_age_hours as i64);

        if let Some(versions) = self.cache.get_all_versions(name, max_age) {
            self.last_was_cache_hit = true;
            return Ok(versions);
        }

        self.last_was_cache_hit = false;
        let mut last_error = None;

        for attempt in 0..3 {
            match self.fetch_all_versions_uncached(name) {
                Ok(result) => {
                    if !result.is_empty() {
                        self.cache.set_all_versions(name, result.clone());
                    }
                    return Ok(result);
                }
                Err(FetchError::Permanent(msg)) => return Err(FetchError::Permanent(msg)),
                Err(e) => {
                    if attempt < 2 {
                        std::thread::sleep(Duration::from_millis(500));
                    }
                    last_error = Some(e);
                }
            }
        }

        Err(last_error.unwrap())
    }
}

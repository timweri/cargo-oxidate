use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::time::Duration;

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

#[derive(Deserialize, Clone)]
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
}

impl CratesIoClient {
    pub fn new(timeout_secs: u64) -> Result<Self> {
        let agent = ureq::Agent::config_builder()
            .user_agent("cargo-oxidate/0.1 (https://github.com/timweri/cargo-oxidate)")
            .timeout_global(Some(Duration::from_secs(timeout_secs)))
            .http_status_as_error(false)
            .build()
            .new_agent();

        Ok(Self { agent })
    }

    pub fn fetch_publish_date(
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
        &self,
        name: &str,
        version: &str,
    ) -> Result<Option<DateTime<Utc>>, FetchError> {
        let mut last_error = None;

        for attempt in 0..3 {
            match self.fetch_publish_date(name, version) {
                Ok(result) => return Ok(result),
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

    pub fn fetch_all_versions(&self, name: &str) -> Result<Vec<CrateVersionInfo>, FetchError> {
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
        &self,
        name: &str,
    ) -> Result<Vec<CrateVersionInfo>, FetchError> {
        let mut last_error = None;

        for attempt in 0..3 {
            match self.fetch_all_versions(name) {
                Ok(result) => return Ok(result),
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

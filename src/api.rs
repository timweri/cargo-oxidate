use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use reqwest::Client;
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
    client: Client,
}

impl CratesIoClient {
    pub fn new(timeout_secs: u64) -> Result<Self> {
        let client = Client::builder()
            .user_agent("cargo-oxidate/0.1 (https://github.com/timweri/cargo-oxidate)")
            .timeout(Duration::from_secs(timeout_secs))
            .build()
            .context("Failed to build HTTP client")?;

        Ok(Self { client })
    }

    pub async fn fetch_publish_date(
        &self,
        name: &str,
        version: &str,
    ) -> Result<Option<DateTime<Utc>>, FetchError> {
        let url = format!("https://crates.io/api/v1/crates/{name}/{version}");

        let response = match self.client.get(&url).send().await {
            Ok(resp) => resp,
            Err(e) => {
                // Classify reqwest errors
                if e.is_timeout() || e.is_connect() {
                    return Err(FetchError::Retryable(format!(
                        "Network error for {name}@{version}: {e}"
                    )));
                }
                return Err(FetchError::Permanent(format!(
                    "Request failed for {name}@{version}: {e}"
                )));
            }
        };

        let status = response.status();

        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }

        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
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

        let data: CrateVersionResponse = response.json().await.map_err(|e| {
            FetchError::Permanent(format!(
                "Failed to parse response for {name}@{version}: {e}"
            ))
        })?;

        Ok(Some(data.version.created_at))
    }

    pub async fn fetch_publish_date_with_retry(
        &self,
        name: &str,
        version: &str,
    ) -> Result<Option<DateTime<Utc>>, FetchError> {
        let mut last_error = None;

        for attempt in 0..3 {
            match self.fetch_publish_date(name, version).await {
                Ok(result) => return Ok(result),
                Err(FetchError::Permanent(msg)) => return Err(FetchError::Permanent(msg)),
                Err(e) => {
                    if attempt < 2 {
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                    last_error = Some(e);
                }
            }
        }

        Err(last_error.unwrap())
    }
}

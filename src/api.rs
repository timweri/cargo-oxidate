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

pub struct CratesIoClient {
    client: Client,
}

impl CratesIoClient {
    pub fn new(timeout_secs: u64) -> Result<Self> {
        let client = Client::builder()
            .user_agent("cargo-oxidate/0.1 (https://github.com/owner/cargo-oxidate)")
            .timeout(Duration::from_secs(timeout_secs))
            .build()
            .context("Failed to build HTTP client")?;

        Ok(Self { client })
    }

    pub async fn fetch_publish_date(
        &self,
        name: &str,
        version: &str,
    ) -> Result<Option<DateTime<Utc>>> {
        let url = format!("https://crates.io/api/v1/crates/{name}/{version}");

        let response = self.client.get(&url).send().await?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }

        let response = response.error_for_status()
            .context(format!("API request failed for {name}@{version}"))?;

        let data: CrateVersionResponse = response.json().await
            .context(format!("Failed to parse response for {name}@{version}"))?;

        Ok(Some(data.version.created_at))
    }
}

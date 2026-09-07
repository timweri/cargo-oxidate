use anyhow::Result;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::num::NonZeroU32;
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

/// A raw HTTP response: a status code and a body, with no interpretation of
/// either.
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

/// A transport-level failure: the request never produced an HTTP response
/// (timeout, connection refused, TLS error, ...).
#[derive(Debug)]
pub struct TransportError(pub String);

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for TransportError {}

/// The only part of the crates.io client that is generic: issuing a GET
/// request. Retry, status classification, and caching are all built on top
/// of this trait, so tests can drive them by implementing it instead of
/// hitting the network.
pub trait Transport {
    fn get(&self, url: &str) -> Result<HttpResponse, TransportError>;
}

/// The production transport, backed by a `ureq::Agent`.
pub struct UreqTransport {
    agent: ureq::Agent,
}

impl UreqTransport {
    pub fn new(timeout_secs: u64) -> Self {
        let agent = ureq::Agent::config_builder()
            .user_agent("cargo-oxidate/0.1 (https://github.com/timweri/cargo-oxidate)")
            .timeout_global(Some(Duration::from_secs(timeout_secs)))
            .http_status_as_error(false)
            .build()
            .new_agent();

        Self { agent }
    }
}

impl Transport for UreqTransport {
    fn get(&self, url: &str) -> Result<HttpResponse, TransportError> {
        let mut response = self
            .agent
            .get(url)
            .call()
            .map_err(|e| TransportError(e.to_string()))?;

        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .read_to_vec()
            .map_err(|e| TransportError(e.to_string()))?;

        Ok(HttpResponse { status, body })
    }
}

/// Retry and pacing timing, fixed at construction. Tests build one with
/// zeroed delays so the suite doesn't sleep; production uses `Default`.
pub(crate) struct RetryPolicy {
    pub(crate) retry_count: NonZeroU32,
    pub(crate) retry_delay: Duration,
    pub(crate) pacing_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            retry_count: NonZeroU32::new(3).unwrap(),
            retry_delay: Duration::from_millis(500),
            pacing_delay: Duration::from_millis(100),
        }
    }
}

pub struct CratesIoClient<T: Transport = UreqTransport> {
    transport: T,
    cache: ResponseCache,
    cache_max_age_hours: u64,
    retry_policy: RetryPolicy,
}

impl CratesIoClient<UreqTransport> {
    pub fn new(
        timeout_secs: u64,
        cache_path: Option<&Path>,
        cache_max_age_hours: u64,
    ) -> Result<Self> {
        Ok(Self::with_transport(
            UreqTransport::new(timeout_secs),
            cache_path,
            cache_max_age_hours,
            RetryPolicy::default(),
        ))
    }
}

impl<T: Transport> CratesIoClient<T> {
    pub(crate) fn with_transport(
        transport: T,
        cache_path: Option<&Path>,
        cache_max_age_hours: u64,
        retry_policy: RetryPolicy,
    ) -> Self {
        Self {
            transport,
            cache: ResponseCache::load(cache_path),
            cache_max_age_hours,
            retry_policy,
        }
    }

    /// Consumes the client, saving the cache. Cache write failures are
    /// reported to stderr rather than propagated, since a broken cache
    /// directory shouldn't fail the whole run.
    pub fn finish(self) {
        if let Err(e) = self.cache.save() {
            eprintln!("Warning: failed to save cache: {e}");
        }
    }

    /// Sleeps for the inter-request rate limit window. Called only from the
    /// fetch methods' cache-miss path, so a cache hit never pays it.
    fn pace(&self) {
        std::thread::sleep(self.retry_policy.pacing_delay);
    }

    /// Issues a GET, classifies HTTP errors, and parses JSON.
    ///
    /// Returns `Ok(None)` on HTTP 404 — this is hardcoded; callers that need a
    /// different 404 policy must wrap or replace this helper.
    ///
    /// `subject` is the trailing fragment used in every error message
    /// (`"Network error {subject}: …"`, `"Rate limited {subject}"`, …,
    /// `"Failed to parse response {subject}: …"`).
    fn fetch_json<D: DeserializeOwned>(
        &self,
        url: &str,
        subject: &str,
    ) -> Result<Option<D>, FetchError> {
        let response = self
            .transport
            .get(url)
            .map_err(|e| FetchError::Retryable(format!("Network error {subject}: {e}")))?;

        match response.status {
            404 => Ok(None),
            429 => Err(FetchError::Retryable(format!("Rate limited {subject}"))),
            status if (500..600).contains(&status) => Err(FetchError::Retryable(format!(
                "Server error {status} {subject}"
            ))),
            status if (400..500).contains(&status) => Err(FetchError::Permanent(format!(
                "Client error {status} {subject}"
            ))),
            _ => serde_json::from_slice(&response.body)
                .map(Some)
                .map_err(|e| {
                    FetchError::Permanent(format!("Failed to parse response {subject}: {e}"))
                }),
        }
    }

    /// Runs `op` up to `retry_count` times, sleeping `retry_delay` between
    /// transient failures. `Permanent` errors short-circuit immediately.
    fn with_retry<R>(
        &mut self,
        mut op: impl FnMut(&mut Self) -> Result<R, FetchError>,
    ) -> Result<R, FetchError> {
        let retry_count = self.retry_policy.retry_count.get();
        let mut last_error = None;

        for attempt in 0..retry_count {
            match op(self) {
                Ok(result) => return Ok(result),
                Err(FetchError::Permanent(msg)) => return Err(FetchError::Permanent(msg)),
                Err(e) => {
                    if attempt + 1 < retry_count {
                        std::thread::sleep(self.retry_policy.retry_delay);
                    }
                    last_error = Some(e);
                }
            }
        }

        // `retry_count` is a `NonZeroU32`, so the loop above ran at least
        // once and `last_error` is always populated here.
        Err(last_error.unwrap())
    }

    fn fetch_publish_date_uncached(
        &self,
        name: &str,
        version: &str,
    ) -> Result<Option<DateTime<Utc>>, FetchError> {
        let url = format!("https://crates.io/api/v1/crates/{name}/{version}");
        let subject = format!("for {name}@{version}");
        let data: Option<CrateVersionResponse> = self.fetch_json(&url, &subject)?;
        Ok(data.map(|d| d.version.created_at))
    }

    pub fn fetch_publish_date(
        &mut self,
        name: &str,
        version: &str,
    ) -> Result<Option<DateTime<Utc>>, FetchError> {
        if let Some(date) = self.cache.get_publish_date(name, version) {
            return Ok(Some(date));
        }

        let result = self.with_retry(|client| {
            let result = client.fetch_publish_date_uncached(name, version)?;
            if let Some(date) = result {
                client.cache.set_publish_date(name, version, date);
            }
            Ok(result)
        });
        self.pace();
        result
    }

    fn fetch_all_versions_uncached(&self, name: &str) -> Result<Vec<CrateVersionInfo>, FetchError> {
        let url = format!("https://crates.io/api/v1/crates/{name}");
        let subject = format!("fetching versions for {name}");
        let data: Option<CrateResponse> = self.fetch_json(&url, &subject)?;
        Ok(data.map(|d| d.versions).unwrap_or_default())
    }

    pub fn fetch_all_versions(&mut self, name: &str) -> Result<Vec<CrateVersionInfo>, FetchError> {
        let max_age = ChronoDuration::hours(self.cache_max_age_hours as i64);

        if let Some(versions) = self.cache.get_all_versions(name, max_age) {
            return Ok(versions);
        }

        let result = self.with_retry(|client| {
            let result = client.fetch_all_versions_uncached(name)?;
            if !result.is_empty() {
                client.cache.set_all_versions(name, result.clone());
            }
            Ok(result)
        });
        self.pace();
        result
    }
}

/// Test-only fake `Transport` and URL helpers, shared by this module's own
/// tests and by the suggest-flow tests in `suggest.rs` (which previously
/// maintained a second, divergent copy of the same fake).
#[cfg(test)]
pub(crate) mod test_support {
    use super::{HttpResponse, Transport, TransportError};
    use std::cell::RefCell;
    use std::collections::{HashMap, VecDeque};

    pub(crate) enum ScriptedResponse {
        Http(u16, String),
        Error,
    }

    /// An in-memory `Transport` driven by a script of canned responses per
    /// URL, consumed in order. Also records every call made, so tests can
    /// assert on request counts (e.g. "a cache hit issues no request").
    #[derive(Default)]
    pub(crate) struct FakeTransport {
        responses: RefCell<HashMap<String, VecDeque<ScriptedResponse>>>,
        calls: RefCell<Vec<String>>,
    }

    impl FakeTransport {
        pub(crate) fn new() -> Self {
            Self::default()
        }

        pub(crate) fn push(&self, url: &str, response: ScriptedResponse) {
            self.responses
                .borrow_mut()
                .entry(url.to_string())
                .or_default()
                .push_back(response);
        }

        pub(crate) fn call_count(&self) -> usize {
            self.calls.borrow().len()
        }
    }

    impl Transport for FakeTransport {
        fn get(&self, url: &str) -> Result<HttpResponse, TransportError> {
            self.calls.borrow_mut().push(url.to_string());

            let mut responses = self.responses.borrow_mut();
            let queued = responses
                .get_mut(url)
                .and_then(|q| q.pop_front())
                .unwrap_or_else(|| panic!("no scripted response left for {url}"));

            match queued {
                ScriptedResponse::Http(status, body) => Ok(HttpResponse {
                    status,
                    body: body.as_bytes().to_vec(),
                }),
                ScriptedResponse::Error => Err(TransportError("connection timed out".to_string())),
            }
        }
    }

    pub(crate) fn versions_url(name: &str) -> String {
        format!("https://crates.io/api/v1/crates/{name}")
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{FakeTransport, ScriptedResponse, versions_url};
    use super::*;
    use std::num::NonZeroU32;
    use std::time::Instant;

    fn version_url(name: &str, version: &str) -> String {
        format!("https://crates.io/api/v1/crates/{name}/{version}")
    }

    fn version_body(created_at: &str) -> String {
        format!(r#"{{"version":{{"created_at":"{created_at}"}}}}"#)
    }

    /// Builds a client with retry/pacing delays zeroed out, so the test
    /// suite doesn't sleep.
    fn fast_client(transport: FakeTransport) -> CratesIoClient<FakeTransport> {
        CratesIoClient::with_transport(
            transport,
            None,
            24,
            RetryPolicy {
                retry_count: NonZeroU32::new(3).unwrap(),
                retry_delay: Duration::from_millis(0),
                pacing_delay: Duration::from_millis(0),
            },
        )
    }

    #[test]
    fn retries_429_then_succeeds() {
        let url = version_url("serde", "1.0.0");
        let transport = FakeTransport::new();
        transport.push(&url, ScriptedResponse::Http(429, String::new()));
        transport.push(
            &url,
            ScriptedResponse::Http(200, version_body("2020-01-01T00:00:00Z")),
        );

        let mut client = fast_client(transport);
        let result = client.fetch_publish_date("serde", "1.0.0").unwrap();
        assert!(result.is_some());
    }

    #[test]
    fn retries_5xx_then_succeeds() {
        let url = version_url("serde", "1.0.0");
        let transport = FakeTransport::new();
        transport.push(&url, ScriptedResponse::Http(503, String::new()));
        transport.push(
            &url,
            ScriptedResponse::Http(200, version_body("2020-01-01T00:00:00Z")),
        );

        let mut client = fast_client(transport);
        let result = client.fetch_publish_date("serde", "1.0.0").unwrap();
        assert!(result.is_some());
    }

    #[test]
    fn permanent_4xx_fails_without_retry() {
        let url = version_url("serde", "1.0.0");
        let transport = FakeTransport::new();
        transport.push(&url, ScriptedResponse::Http(400, String::new()));

        let mut client = fast_client(transport);
        let result = client.fetch_publish_date("serde", "1.0.0");
        assert!(matches!(result, Err(FetchError::Permanent(_))));
        assert_eq!(client.transport.call_count(), 1);
    }

    #[test]
    fn not_found_yields_no_date_not_an_error() {
        let url = version_url("serde", "1.0.0");
        let transport = FakeTransport::new();
        transport.push(&url, ScriptedResponse::Http(404, String::new()));

        let mut client = fast_client(transport);
        let result = client.fetch_publish_date("serde", "1.0.0").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn malformed_body_is_permanent() {
        let url = version_url("serde", "1.0.0");
        let transport = FakeTransport::new();
        transport.push(&url, ScriptedResponse::Http(200, "not json".to_string()));

        let mut client = fast_client(transport);
        let result = client.fetch_publish_date("serde", "1.0.0");
        assert!(matches!(result, Err(FetchError::Permanent(_))));
        assert_eq!(client.transport.call_count(), 1);
    }

    #[test]
    fn gives_up_after_exactly_three_attempts() {
        let url = version_url("serde", "1.0.0");
        let transport = FakeTransport::new();
        for _ in 0..3 {
            transport.push(&url, ScriptedResponse::Http(503, String::new()));
        }

        let mut client = fast_client(transport);
        let result = client.fetch_publish_date("serde", "1.0.0");
        assert!(matches!(result, Err(FetchError::Retryable(_))));
        assert_eq!(client.transport.call_count(), 3);
    }

    #[test]
    fn gives_up_after_a_single_attempt_without_panicking() {
        let url = version_url("serde", "1.0.0");
        let transport = FakeTransport::new();
        transport.push(&url, ScriptedResponse::Http(503, String::new()));

        let mut client = CratesIoClient::with_transport(
            transport,
            None,
            24,
            RetryPolicy {
                retry_count: NonZeroU32::new(1).unwrap(),
                retry_delay: Duration::from_millis(0),
                pacing_delay: Duration::from_millis(0),
            },
        );
        let result = client.fetch_publish_date("serde", "1.0.0");
        assert!(matches!(result, Err(FetchError::Retryable(_))));
        assert_eq!(client.transport.call_count(), 1);
    }

    #[test]
    fn transport_error_is_retried() {
        let url = version_url("serde", "1.0.0");
        let transport = FakeTransport::new();
        transport.push(&url, ScriptedResponse::Error);
        transport.push(
            &url,
            ScriptedResponse::Http(200, version_body("2020-01-01T00:00:00Z")),
        );

        let mut client = fast_client(transport);
        let result = client.fetch_publish_date("serde", "1.0.0").unwrap();
        assert!(result.is_some());
    }

    #[test]
    fn cache_hit_issues_no_request() {
        let transport = FakeTransport::new();
        let mut client = fast_client(transport);
        client.cache.set_publish_date(
            "serde",
            "1.0.0",
            DateTime::parse_from_rfc3339("2020-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        );

        let result = client.fetch_publish_date("serde", "1.0.0").unwrap();
        assert!(result.is_some());
        assert_eq!(client.transport.call_count(), 0);
    }

    #[test]
    fn cache_hit_skips_pacing_delay() {
        let transport = FakeTransport::new();
        let mut client = CratesIoClient::with_transport(
            transport,
            None,
            24,
            RetryPolicy {
                retry_count: NonZeroU32::new(3).unwrap(),
                retry_delay: Duration::from_millis(0),
                pacing_delay: Duration::from_millis(300),
            },
        );
        client.cache.set_publish_date(
            "serde",
            "1.0.0",
            DateTime::parse_from_rfc3339("2020-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        );

        let started = Instant::now();
        client.fetch_publish_date("serde", "1.0.0").unwrap();
        assert!(started.elapsed() < Duration::from_millis(300));
    }

    #[test]
    fn all_versions_cache_hit_issues_no_request() {
        let transport = FakeTransport::new();
        let mut client = fast_client(transport);
        client.cache.set_all_versions("serde", vec![]);

        let result = client.fetch_all_versions("serde").unwrap();
        assert!(result.is_empty());
        assert_eq!(client.transport.call_count(), 0);
    }

    #[test]
    fn fetch_all_versions_retries_and_parses() {
        let url = versions_url("serde");
        let transport = FakeTransport::new();
        transport.push(&url, ScriptedResponse::Http(500, String::new()));
        transport.push(
            &url,
            ScriptedResponse::Http(
                200,
                r#"{"versions":[{"num":"1.0.0","created_at":"2020-01-01T00:00:00Z","yanked":false}]}"#.to_string(),
            ),
        );

        let mut client = fast_client(transport);
        let result = client.fetch_all_versions("serde").unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].num, "1.0.0");
    }
}

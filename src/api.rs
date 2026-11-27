use anyhow::Result;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::cell::Cell;
use std::collections::HashMap;
use std::num::NonZeroU32;
use std::path::Path;
use std::time::Duration;

use crate::cache::{CacheWarning, ResponseCache};

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

/// One version's record from the crates.io sparse index: its own version
/// string, whether it's yanked, and the requirements it places on its own
/// dependencies (used to check whether a candidate downgrade would still
/// satisfy a dependent).
#[derive(Deserialize, Serialize, Clone)]
pub struct IndexRecord {
    pub vers: String,
    #[serde(default)]
    pub yanked: bool,
    #[serde(default)]
    pub deps: Vec<IndexDep>,
}

#[derive(Deserialize, Serialize, Clone)]
pub struct IndexDep {
    pub name: String,
    pub req: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub optional: Option<bool>,
    /// The original crate name, present when `name` is a rename alias.
    #[serde(default)]
    pub package: Option<String>,
    /// The alternate registry this dependency is resolved from, if any.
    #[serde(default)]
    pub registry: Option<String>,
}

/// Computes the sparse-index path fragment for a crate name, per the rules
/// at <https://doc.rust-lang.org/cargo/reference/registry-index.html#index-files>:
/// 1-char names live under `1/`, 2-char under `2/`, 3-char under
/// `3/<first char>/`, and everything else is split into two two-character
/// prefix directories. Matching is done on the lowercased name.
pub fn sparse_index_path(name: &str) -> String {
    let lower = name.to_lowercase();
    let chars: Vec<char> = lower.chars().collect();
    match chars.len() {
        1 => format!("1/{lower}"),
        2 => format!("2/{lower}"),
        3 => format!("3/{}/{lower}", chars[0]),
        _ => format!(
            "{}/{}/{lower}",
            chars[0..2].iter().collect::<String>(),
            chars[2..4].iter().collect::<String>()
        ),
    }
}

/// Classifies API fetch errors for retry decision-making.
#[derive(Debug)]
pub enum FetchError {
    /// Retryable: network timeout, connection error, 429, 5xx
    Retryable(String),
    /// Permanent: 4xx (other than 404/429), parse errors
    Permanent(String),
    /// The server's rate limit asks for more waiting than a run allows.
    /// Not retried within this run, but a later run may succeed.
    RateLimited(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Retryable(msg) | Self::Permanent(msg) | Self::RateLimited(msg) => {
                write!(f, "{msg}")
            }
        }
    }
}

impl std::error::Error for FetchError {}

/// A raw HTTP response: a status code, a body, and the `Retry-After` header
/// value if the server sent one, with no interpretation of any of them.
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
    pub retry_after: Option<String>,
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
            .user_agent(concat!(
                "cargo-oxidate/",
                env!("CARGO_PKG_VERSION"),
                " (https://github.com/timweri/cargo-oxidate)"
            ))
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
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let body = response
            .body_mut()
            .read_to_vec()
            .map_err(|e| TransportError(e.to_string()))?;

        Ok(HttpResponse {
            status,
            body,
            retry_after,
        })
    }
}

/// Retry and request-spacing timing, fixed at construction. Every wait goes through
/// `sleep`, so tests substitute a recorder (or zeroed delays) and the suite
/// never really sleeps; production uses `Default`.
pub(crate) struct RetryPolicy {
    pub(crate) retry_count: NonZeroU32,
    pub(crate) retry_delay: Duration,
    /// Minimum gap between the starts of two crates.io API requests.
    pub(crate) api_spacing: Duration,
    pub(crate) sleep: Box<dyn Fn(Duration)>,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            retry_count: NonZeroU32::new(3).unwrap(),
            retry_delay: Duration::from_millis(500),
            api_spacing: Duration::from_secs(1),
            sleep: Box::new(std::thread::sleep),
        }
    }
}

const CRATES_IO_API: &str = "https://crates.io/api/v1/";

/// The longest `Retry-After` wait a lookup will honour; a longer ask fails it.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);

/// The most time a run spends waiting on `Retry-After` in total.
const RETRY_AFTER_BUDGET: Duration = Duration::from_secs(180);

/// Reads a `Retry-After` header as integer seconds. Any other form, including
/// an HTTP-date, is `None`. A number too large to represent saturates.
fn parse_retry_after(value: &str) -> Option<Duration> {
    let value = value.trim();
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some(Duration::from_secs(value.parse().unwrap_or(u64::MAX)))
}

/// Time this run has spent, or may still spend, on `Retry-After` waits.
#[derive(Default)]
struct RetryAfterBudget {
    spent: Cell<Duration>,
    /// Set once a wait did not fit; every later 429 then fails at once.
    exhausted: Cell<bool>,
    /// The honoured wait for the 429 just seen, consumed by `with_retry`.
    pending: Cell<Option<Duration>>,
}

pub struct CratesIoClient<T: Transport = UreqTransport> {
    transport: T,
    cache: ResponseCache,
    cache_max_age: ChronoDuration,
    retry_policy: RetryPolicy,
    retry_after: RetryAfterBudget,
    /// Whether this run has issued a crates.io API request yet; only the
    /// requests after the first wait out `api_spacing`.
    api_requested: Cell<bool>,
    /// Per-run memo of successfully fetched index records, keyed by crate
    /// name. Confined to this process; never persisted.
    fetched_index_records: HashMap<String, Vec<IndexRecord>>,
}

impl CratesIoClient<UreqTransport> {
    pub fn new(timeout_secs: u64, cache_path: Option<&Path>, cache_max_age_hours: u64) -> Self {
        Self::with_transport(
            UreqTransport::new(timeout_secs),
            cache_path,
            cache_max_age_hours,
            RetryPolicy::default(),
        )
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
            cache_max_age: ChronoDuration::hours(cache_max_age_hours as i64),
            retry_policy,
            retry_after: RetryAfterBudget::default(),
            api_requested: Cell::new(false),
            fetched_index_records: HashMap::new(),
        }
    }

    pub fn take_cache_warning(&mut self) -> Option<CacheWarning> {
        self.cache.take_load_warning()
    }

    /// Saves the cache and returns any save error as a warning.
    pub fn finish(self) -> Option<CacheWarning> {
        self.cache
            .save()
            .err()
            .map(|error| self.cache.save_warning(error))
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
        let Some(body) = self.fetch_body(url, subject)? else {
            return Ok(None);
        };

        serde_json::from_slice(&body)
            .map(Some)
            .map_err(|e| FetchError::Permanent(format!("Failed to parse response {subject}: {e}")))
    }

    /// Issues a GET and classifies HTTP errors, without interpreting the
    /// body. Used by `fetch_json` and by the index fetch, whose body is
    /// newline-delimited JSON rather than a single document.
    ///
    /// Returns `Ok(None)` on HTTP 404, same as `fetch_json`.
    fn fetch_body(&self, url: &str, subject: &str) -> Result<Option<Vec<u8>>, FetchError> {
        self.retry_after.pending.set(None);
        // Every crates.io API request, retries included, starts at least
        // `api_spacing` after the previous one. Cache and memo hits never
        // reach here, so they never wait.
        if url.starts_with(CRATES_IO_API) && self.api_requested.replace(true) {
            (self.retry_policy.sleep)(self.retry_policy.api_spacing);
        }
        let response = self
            .transport
            .get(url)
            .map_err(|e| FetchError::Retryable(format!("Network error {subject}: {e}")))?;

        match response.status {
            404 => Ok(None),
            429 => Err(self.rate_limited(response.retry_after.as_deref(), subject)),
            status if (500..600).contains(&status) => Err(FetchError::Retryable(format!(
                "Server error {status} {subject}"
            ))),
            status if (400..500).contains(&status) => Err(FetchError::Permanent(format!(
                "Client error {status} {subject}"
            ))),
            _ => Ok(Some(response.body)),
        }
    }

    /// Classifies a 429. A `Retry-After` of integer seconds is honoured up to
    /// `MAX_RETRY_AFTER` and within what is left of `RETRY_AFTER_BUDGET`;
    /// those waits are retried (the wait is left for `with_retry` to spend).
    /// A longer ask, or any 429 once the budget is spent, fails the lookup
    /// at once. A missing or non-integer header is a plain retryable failure,
    /// retried after the fixed `retry_delay`.
    fn rate_limited(&self, retry_after: Option<&str>, subject: &str) -> FetchError {
        let budget = &self.retry_after;
        let wait = retry_after.and_then(parse_retry_after);
        if let Some(wait) = wait
            && wait > MAX_RETRY_AFTER
        {
            return FetchError::RateLimited(format!(
                "Rate limited {subject}: crates.io asked to wait {} s, over the {} s limit",
                wait.as_secs(),
                MAX_RETRY_AFTER.as_secs()
            ));
        }
        if let Some(wait) = wait
            && budget.spent.get() + wait > RETRY_AFTER_BUDGET
        {
            budget.exhausted.set(true);
        }
        if budget.exhausted.get() {
            return FetchError::RateLimited(format!(
                "Rate limited {subject}: the {} s rate-limit wait budget for this run is exhausted",
                RETRY_AFTER_BUDGET.as_secs()
            ));
        }
        budget.pending.set(wait);
        FetchError::Retryable(format!("Rate limited {subject}"))
    }

    /// Runs `op` up to `retry_count` times, sleeping between transient
    /// failures: the server's `Retry-After` when `rate_limited` honoured one,
    /// else `retry_delay`. `Permanent` and `RateLimited` errors short-circuit
    /// immediately.
    fn with_retry<R>(
        &mut self,
        mut op: impl FnMut(&mut Self) -> Result<R, FetchError>,
    ) -> Result<R, FetchError> {
        for _ in 1..self.retry_policy.retry_count.get() {
            match op(self) {
                Ok(result) => return Ok(result),
                Err(error @ (FetchError::Permanent(_) | FetchError::RateLimited(_))) => {
                    return Err(error);
                }
                Err(FetchError::Retryable(_)) => {
                    let wait = match self.retry_after.pending.take() {
                        Some(wait) => {
                            let spent = &self.retry_after.spent;
                            spent.set(spent.get() + wait);
                            wait
                        }
                        None => self.retry_policy.retry_delay,
                    };
                    (self.retry_policy.sleep)(wait);
                }
            }
        }
        // The final attempt runs outside the loop so its error is returned as is.
        op(self)
    }

    fn fetch_publish_date_uncached(
        &self,
        name: &str,
        version: &str,
    ) -> Result<Option<DateTime<Utc>>, FetchError> {
        let url = format!("{CRATES_IO_API}crates/{name}/{version}");
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

        self.with_retry(|client| {
            let result = client.fetch_publish_date_uncached(name, version)?;
            if let Some(date) = result {
                client.cache.set_publish_date(name, version, date);
            }
            Ok(result)
        })
    }

    fn fetch_all_versions_uncached(&self, name: &str) -> Result<Vec<CrateVersionInfo>, FetchError> {
        let url = format!("{CRATES_IO_API}crates/{name}");
        let subject = format!("fetching versions for {name}");
        let data: Option<CrateResponse> = self.fetch_json(&url, &subject)?;
        Ok(data.map(|d| d.versions).unwrap_or_default())
    }

    pub fn fetch_all_versions(&mut self, name: &str) -> Result<Vec<CrateVersionInfo>, FetchError> {
        if let Some(versions) = self.cache.get_all_versions(name, self.cache_max_age) {
            return Ok(versions);
        }

        self.with_retry(|client| {
            let result = client.fetch_all_versions_uncached(name)?;
            if !result.is_empty() {
                client.cache.set_all_versions(name, result.clone());
            }
            Ok(result)
        })
    }

    fn fetch_index_record_uncached(&self, name: &str) -> Result<Vec<IndexRecord>, FetchError> {
        let url = format!("https://index.crates.io/{}", sparse_index_path(name));
        let subject = format!("fetching index record for {name}");
        let Some(body) = self.fetch_body(&url, &subject)? else {
            return Ok(vec![]);
        };
        let text = String::from_utf8_lossy(&body);

        text.lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                serde_json::from_str(line).map_err(|e| {
                    FetchError::Permanent(format!("Failed to parse index record {subject}: {e}"))
                })
            })
            .collect()
    }

    /// Fetches every published version's index record for `name` from the
    /// crates.io sparse index — one line of JSON per version, each listing
    /// that version's own dependency requirements. An unknown crate yields
    /// an empty vector rather than an error.
    ///
    /// A lookup first consults a per-run memo of index records already
    /// fetched successfully in this process, keyed by crate name; a memo
    /// hit is returned as-is, with no age check. Otherwise, a persisted
    /// cache entry satisfies the lookup only when it is fresh and contains
    /// a record whose `vers` equals `needed_version`; a non-empty entry
    /// that lacks that version (e.g. a dependent published after the
    /// cache entry was written) is treated as a miss, same as an empty
    /// entry. A miss falls through to the network, and a successful fetch
    /// is written to both the memo and the persisted cache.
    ///
    /// Unlike the other two fetches, this one is not subject to the
    /// crates.io API's inter-request pacing: the sparse index is a static
    /// endpoint outside that rate limit.
    pub fn fetch_index_record(
        &mut self,
        name: &str,
        needed_version: &str,
    ) -> Result<Vec<IndexRecord>, FetchError> {
        if let Some(records) = self.fetched_index_records.get(name) {
            return Ok(records.clone());
        }

        if let Some(records) = self.cache.get_index_records(name, self.cache_max_age)
            && records.iter().any(|r| r.vers == needed_version)
        {
            return Ok(records);
        }

        self.with_retry(|client| {
            let result = client.fetch_index_record_uncached(name)?;
            client.cache.set_index_records(name, result.clone());
            client
                .fetched_index_records
                .insert(name.to_string(), result.clone());
            Ok(result)
        })
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
        /// A 429 carrying the given `Retry-After` header value, if any.
        RateLimited(Option<&'static str>),
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
                    retry_after: None,
                }),
                ScriptedResponse::RateLimited(retry_after) => Ok(HttpResponse {
                    status: 429,
                    body: Vec::new(),
                    retry_after: retry_after.map(str::to_string),
                }),
                ScriptedResponse::Error => Err(TransportError("connection timed out".to_string())),
            }
        }
    }

    pub(crate) fn versions_url(name: &str) -> String {
        format!("https://crates.io/api/v1/crates/{name}")
    }

    pub(crate) fn index_url(name: &str) -> String {
        format!("https://index.crates.io/{}", super::sparse_index_path(name))
    }

    pub(crate) fn version_url(name: &str, version: &str) -> String {
        format!("https://crates.io/api/v1/crates/{name}/{version}")
    }

    pub(crate) fn version_body(created_at: impl std::fmt::Display) -> String {
        format!(r#"{{"version":{{"created_at":"{created_at}"}}}}"#)
    }

    /// Retry and spacing delays zeroed out, so the test suite doesn't sleep.
    pub(crate) fn instant_policy(retry_count: u32) -> super::RetryPolicy {
        super::RetryPolicy {
            retry_count: super::NonZeroU32::new(retry_count).unwrap(),
            retry_delay: super::Duration::ZERO,
            api_spacing: super::Duration::ZERO,
            sleep: Box::new(|_| {}),
        }
    }

    /// Waits requested through the sleep seam, in order.
    pub(crate) type SleepLog = std::rc::Rc<RefCell<Vec<super::Duration>>>;

    /// The production delays, with every wait recorded instead of slept, so
    /// tests assert the exact waits the client asks for.
    pub(crate) fn recording_policy(retry_count: u32) -> (super::RetryPolicy, SleepLog) {
        let waits = SleepLog::default();
        let recorder = std::rc::Rc::clone(&waits);
        let policy = super::RetryPolicy {
            retry_count: super::NonZeroU32::new(retry_count).unwrap(),
            sleep: Box::new(move |wait| recorder.borrow_mut().push(wait)),
            ..super::RetryPolicy::default()
        };
        (policy, waits)
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{
        FakeTransport, ScriptedResponse, SleepLog, index_url, instant_policy, recording_policy,
        version_body, version_url, versions_url,
    };
    use super::*;
    use tempfile::tempdir;

    fn record(vers: &str) -> IndexRecord {
        IndexRecord {
            vers: vers.to_string(),
            yanked: false,
            deps: vec![],
        }
    }

    fn fast_client(transport: FakeTransport) -> CratesIoClient<FakeTransport> {
        CratesIoClient::with_transport(transport, None, 24, instant_policy(3))
    }

    #[test]
    fn finish_returns_cache_save_warning_without_failing_the_lookup() {
        let dir = tempdir().unwrap();
        let blocking_parent = dir.path().join("not-a-directory");
        std::fs::write(&blocking_parent, "file").unwrap();
        let cache_path = blocking_parent.join("responses.json");
        let url = version_url("serde", "1.0.0");
        let transport = FakeTransport::default();
        transport.push(
            &url,
            ScriptedResponse::Http(200, version_body("2020-01-01T00:00:00Z")),
        );
        let mut client =
            CratesIoClient::with_transport(transport, Some(&cache_path), 24, instant_policy(3));

        assert!(
            client
                .fetch_publish_date("serde", "1.0.0")
                .unwrap()
                .is_some()
        );
        let warning = client
            .finish()
            .expect("cache save failure should be reported");
        assert_eq!(warning.path, cache_path);
        assert!(
            warning
                .message
                .contains("does not change the dependency check result")
        );
    }

    #[test]
    fn transient_failure_is_retried_then_succeeds() {
        let failures = [
            ScriptedResponse::Http(429, String::new()),
            ScriptedResponse::Http(503, String::new()),
            ScriptedResponse::Error,
        ];
        for failure in failures {
            let url = version_url("serde", "1.0.0");
            let transport = FakeTransport::default();
            transport.push(&url, failure);
            transport.push(
                &url,
                ScriptedResponse::Http(200, version_body("2020-01-01T00:00:00Z")),
            );

            let mut client = fast_client(transport);
            let result = client.fetch_publish_date("serde", "1.0.0").unwrap();
            assert!(result.is_some());
            assert_eq!(client.transport.call_count(), 2);
        }
    }

    #[test]
    fn permanent_failure_is_not_retried() {
        let responses = [
            ScriptedResponse::Http(400, String::new()),
            ScriptedResponse::Http(200, "not json".to_string()),
        ];
        for response in responses {
            let transport = FakeTransport::default();
            transport.push(&version_url("serde", "1.0.0"), response);

            let mut client = fast_client(transport);
            let result = client.fetch_publish_date("serde", "1.0.0");
            assert!(matches!(result, Err(FetchError::Permanent(_))));
            assert_eq!(client.transport.call_count(), 1);
        }
    }

    #[test]
    fn not_found_yields_no_date_not_an_error() {
        let url = version_url("serde", "1.0.0");
        let transport = FakeTransport::default();
        transport.push(&url, ScriptedResponse::Http(404, String::new()));

        let mut client = fast_client(transport);
        let result = client.fetch_publish_date("serde", "1.0.0").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn gives_up_after_retry_count_attempts() {
        for retry_count in [1, 3] {
            let url = version_url("serde", "1.0.0");
            let transport = FakeTransport::default();
            for _ in 0..retry_count {
                transport.push(&url, ScriptedResponse::Http(503, String::new()));
            }

            let mut client =
                CratesIoClient::with_transport(transport, None, 24, instant_policy(retry_count));
            let result = client.fetch_publish_date("serde", "1.0.0");
            assert!(matches!(result, Err(FetchError::Retryable(_))));
            assert_eq!(client.transport.call_count(), retry_count as usize);
        }
    }

    #[test]
    fn cache_hit_issues_no_request_and_never_waits() {
        let (mut client, waits) = recording_client(FakeTransport::default());
        client
            .cache
            .set_publish_date("serde", "1.0.0", "2020-01-01T00:00:00Z".parse().unwrap());

        let result = client.fetch_publish_date("serde", "1.0.0").unwrap();
        assert!(result.is_some());
        assert_eq!(client.transport.call_count(), 0);
        assert!(waits.borrow().is_empty());
    }

    #[test]
    fn api_requests_start_one_second_apart() {
        let transport = FakeTransport::default();
        transport.push(
            &version_url("serde", "1.0.0"),
            ScriptedResponse::Http(200, OK_BODY.to_string()),
        );
        transport.push(
            &versions_url("serde"),
            ScriptedResponse::Http(200, r#"{"versions":[]}"#.to_string()),
        );
        transport.push(
            &version_url("serde", "1.0.1"),
            ScriptedResponse::Http(404, String::new()),
        );
        let (mut client, waits) = recording_client(transport);

        client.fetch_publish_date("serde", "1.0.0").unwrap();
        assert!(waits.borrow().is_empty(), "the first request never waits");
        client.fetch_all_versions("serde").unwrap();
        client.fetch_publish_date("serde", "1.0.1").unwrap();
        assert_eq!(*waits.borrow(), [secs(1), secs(1)]);
    }

    #[test]
    fn api_retries_keep_the_spacing_the_retry_delay_would_undercut() {
        let url = version_url("serde", "1.0.0");
        let transport = FakeTransport::default();
        for _ in 0..3 {
            transport.push(&url, ScriptedResponse::Http(503, String::new()));
        }
        let (mut client, waits) = recording_client(transport);

        assert!(client.fetch_publish_date("serde", "1.0.0").is_err());
        let retry_delay = Duration::from_millis(500);
        assert_eq!(
            *waits.borrow(),
            [retry_delay, secs(1), retry_delay, secs(1)]
        );
    }

    #[test]
    fn cache_hits_between_api_requests_add_no_wait() {
        let transport = FakeTransport::default();
        transport.push(
            &version_url("serde", "1.0.0"),
            ScriptedResponse::Http(200, OK_BODY.to_string()),
        );
        transport.push(
            &version_url("serde", "1.0.2"),
            ScriptedResponse::Http(200, OK_BODY.to_string()),
        );
        let (mut client, waits) = recording_client(transport);

        client.fetch_publish_date("serde", "1.0.0").unwrap();
        client.fetch_publish_date("serde", "1.0.0").unwrap();
        client.fetch_publish_date("serde", "1.0.2").unwrap();
        assert_eq!(client.transport.call_count(), 2);
        assert_eq!(*waits.borrow(), [secs(1)]);
    }

    #[test]
    fn index_requests_are_not_paced_and_do_not_delay_the_first_api_request() {
        let transport = FakeTransport::default();
        for name in ["serde", "rand"] {
            transport.push(&index_url(name), ScriptedResponse::Http(404, String::new()));
        }
        transport.push(
            &version_url("serde", "1.0.0"),
            ScriptedResponse::Http(200, OK_BODY.to_string()),
        );
        let (mut client, waits) = recording_client(transport);

        client.fetch_index_record("serde", "1.0.0").unwrap();
        client.fetch_index_record("rand", "1.0.0").unwrap();
        client.fetch_publish_date("serde", "1.0.0").unwrap();
        assert!(waits.borrow().is_empty());
    }

    #[test]
    fn all_versions_cache_hit_issues_no_request() {
        let transport = FakeTransport::default();
        let mut client = fast_client(transport);
        client.cache.set_all_versions("serde", vec![]);

        let result = client.fetch_all_versions("serde").unwrap();
        assert!(result.is_empty());
        assert_eq!(client.transport.call_count(), 0);
    }

    #[test]
    fn fetch_all_versions_retries_and_parses() {
        let url = versions_url("serde");
        let transport = FakeTransport::default();
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

    const OK_BODY: &str = r#"{"version":{"created_at":"2020-01-01T00:00:00Z"}}"#;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn recording_client(transport: FakeTransport) -> (CratesIoClient<FakeTransport>, SleepLog) {
        let (policy, waits) = recording_policy(3);
        (
            CratesIoClient::with_transport(transport, None, 24, policy),
            waits,
        )
    }

    /// How many recorded waits are `Retry-After` waits: the others (the
    /// 500 ms retry delay and the 1 s API spacing) are at most 1 s.
    fn retry_after_waits(waits: &SleepLog) -> usize {
        waits
            .borrow()
            .iter()
            .filter(|wait| **wait > secs(1))
            .count()
    }

    #[test]
    fn retry_after_seconds_up_to_the_cap_replace_the_retry_delay() {
        for seconds in ["5", "60"] {
            let url = version_url("serde", "1.0.0");
            let transport = FakeTransport::default();
            transport.push(&url, ScriptedResponse::RateLimited(Some(seconds)));
            transport.push(&url, ScriptedResponse::Http(200, OK_BODY.to_string()));

            let (mut client, waits) = recording_client(transport);
            assert!(
                client
                    .fetch_publish_date("serde", "1.0.0")
                    .unwrap()
                    .is_some()
            );
            // The requested wait replaces the retry delay; the retry then
            // keeps the API's request spacing.
            let requested = secs(seconds.parse().unwrap());
            assert_eq!(*waits.borrow(), [requested, secs(1)]);
        }
    }

    #[test]
    fn missing_or_unusable_retry_after_falls_back_to_the_retry_delay() {
        let headers = [
            None,
            Some("Wed, 21 Oct 2026 07:28:00 GMT"),
            Some("soon"),
            Some("-1"),
            Some("1.5"),
            Some(""),
        ];
        for header in headers {
            let url = version_url("serde", "1.0.0");
            let transport = FakeTransport::default();
            transport.push(&url, ScriptedResponse::RateLimited(header));
            transport.push(&url, ScriptedResponse::Http(200, OK_BODY.to_string()));

            let (mut client, waits) = recording_client(transport);
            assert!(
                client
                    .fetch_publish_date("serde", "1.0.0")
                    .unwrap()
                    .is_some()
            );
            assert_eq!(
                *waits.borrow(),
                [Duration::from_millis(500), secs(1)],
                "Retry-After {header:?}"
            );
        }
    }

    #[test]
    fn retry_after_over_the_cap_fails_the_lookup_at_once() {
        let url = version_url("serde", "1.0.0");
        let transport = FakeTransport::default();
        transport.push(&url, ScriptedResponse::RateLimited(Some("61")));

        let (mut client, waits) = recording_client(transport);
        let error = client.fetch_publish_date("serde", "1.0.0").unwrap_err();

        assert!(
            matches!(&error, FetchError::RateLimited(message) if message.contains("wait 61 s")),
            "{error}"
        );
        assert_eq!(client.transport.call_count(), 1);
        assert!(waits.borrow().is_empty());
    }

    #[test]
    fn retry_after_waits_are_capped_at_180_seconds_per_run() {
        let transport = FakeTransport::default();
        for version in ["1.0.0", "1.0.1", "1.0.2"] {
            let url = version_url("serde", version);
            transport.push(&url, ScriptedResponse::RateLimited(Some("60")));
            transport.push(&url, ScriptedResponse::Http(200, OK_BODY.to_string()));
        }
        let (mut client, waits) = recording_client(transport);
        for version in ["1.0.0", "1.0.1", "1.0.2"] {
            client.fetch_publish_date("serde", version).unwrap();
        }
        assert_eq!(retry_after_waits(&waits), 3);

        // The budget is spent: even a 1 s request no longer fits.
        let url = version_url("serde", "1.0.3");
        client
            .transport
            .push(&url, ScriptedResponse::RateLimited(Some("1")));
        let error = client.fetch_publish_date("serde", "1.0.3").unwrap_err();
        assert!(
            matches!(&error, FetchError::RateLimited(message) if message.contains("budget")),
            "{error}"
        );
        assert_eq!(client.transport.call_count(), 7);
        assert_eq!(retry_after_waits(&waits), 3);
    }

    #[test]
    fn a_wait_that_does_not_fit_the_budget_fails_it_and_every_later_429() {
        let transport = FakeTransport::default();
        for (version, seconds) in [("1.0.0", "60"), ("1.0.1", "60"), ("1.0.2", "30")] {
            let url = version_url("serde", version);
            transport.push(&url, ScriptedResponse::RateLimited(Some(seconds)));
            transport.push(&url, ScriptedResponse::Http(200, OK_BODY.to_string()));
        }
        let (mut client, waits) = recording_client(transport);
        for version in ["1.0.0", "1.0.1", "1.0.2"] {
            client.fetch_publish_date("serde", version).unwrap();
        }
        let calls_before = client.transport.call_count();

        // 150 s spent, 30 s left: 31 s does not fit.
        let does_not_fit = version_url("serde", "1.0.3");
        client
            .transport
            .push(&does_not_fit, ScriptedResponse::RateLimited(Some("31")));
        // Later 429s fail at once, whatever they ask for: a 1 s wait that
        // would have fit, and one with no Retry-After at all.
        let later = version_url("serde", "1.0.4");
        client
            .transport
            .push(&later, ScriptedResponse::RateLimited(Some("1")));
        let no_header = version_url("serde", "1.0.5");
        client
            .transport
            .push(&no_header, ScriptedResponse::RateLimited(None));

        for version in ["1.0.3", "1.0.4", "1.0.5"] {
            let error = client.fetch_publish_date("serde", version).unwrap_err();
            assert!(
                matches!(&error, FetchError::RateLimited(message) if message.contains("budget")),
                "{error}"
            );
        }
        assert_eq!(client.transport.call_count(), calls_before + 3);
        assert_eq!(retry_after_waits(&waits), 3);
    }

    #[test]
    fn sparse_index_path_prefix_rules() {
        assert_eq!(sparse_index_path("a"), "1/a");
        assert_eq!(sparse_index_path("io"), "2/io");
        assert_eq!(sparse_index_path("syn"), "3/s/syn");
        assert_eq!(sparse_index_path("serde"), "se/rd/serde");
        assert_eq!(sparse_index_path("Serde"), "se/rd/serde");
        assert_eq!(sparse_index_path("éa"), "2/éa");
        assert_eq!(sparse_index_path("éabcd"), "éa/bc/éabcd");
    }

    #[test]
    fn fetch_index_record_parses_multiple_lines_with_defaults() {
        let url = index_url("serde");
        let transport = FakeTransport::default();
        transport.push(
            &url,
            ScriptedResponse::Http(
                200,
                concat!(
                    r#"{"vers":"1.0.0","yanked":false,"deps":[{"name":"quote","req":"^1.0"}]}"#,
                    "\n",
                    r#"{"vers":"1.0.1","yanked":true}"#,
                    "\n",
                )
                .to_string(),
            ),
        );

        let mut client = fast_client(transport);
        let records = client.fetch_index_record("serde", "1.0.0").unwrap();

        assert_eq!(records.len(), 2);
        assert_eq!(records[0].vers, "1.0.0");
        assert!(!records[0].yanked);
        assert_eq!(records[0].deps.len(), 1);
        assert_eq!(records[0].deps[0].name, "quote");
        assert_eq!(records[0].deps[0].req, "^1.0");
        assert_eq!(records[0].deps[0].kind, None);
        assert_eq!(records[0].deps[0].package, None);

        assert_eq!(records[1].vers, "1.0.1");
        assert!(records[1].yanked);
        assert!(records[1].deps.is_empty());
    }

    #[test]
    fn empty_sparse_index_result_is_cached_within_one_client() {
        let responses = [
            ScriptedResponse::Http(404, String::new()),
            ScriptedResponse::Http(200, " \n\t\n ".to_string()),
        ];
        for response in responses {
            let transport = FakeTransport::default();
            transport.push(&index_url("empty-index"), response);

            let mut client = fast_client(transport);
            for version in ["1.0.0", "2.0.0"] {
                let records = client.fetch_index_record("empty-index", version).unwrap();
                assert!(records.is_empty());
            }
            assert_eq!(client.transport.call_count(), 1);
        }
    }

    #[test]
    fn empty_sparse_index_result_is_refetched_by_a_new_client() {
        // A new client opened against a cache file holding an empty entry
        // must not treat that entry as a hit: it issues a request rather
        // than reusing the stale empty result.
        let dir = tempdir().unwrap();
        let cache_path = dir.path().join("cache.json");
        let url = index_url("does-not-exist");
        let first_transport = FakeTransport::default();
        first_transport.push(&url, ScriptedResponse::Http(404, String::new()));

        let mut first_client = CratesIoClient::with_transport(
            first_transport,
            Some(&cache_path),
            24,
            instant_policy(1),
        );
        assert!(
            first_client
                .fetch_index_record("does-not-exist", "1.0.0")
                .unwrap()
                .is_empty()
        );
        first_client.finish();

        let second_transport = FakeTransport::default();
        second_transport.push(&url, ScriptedResponse::Http(404, String::new()));
        let mut second_client = CratesIoClient::with_transport(
            second_transport,
            Some(&cache_path),
            24,
            instant_policy(1),
        );
        assert!(
            second_client
                .fetch_index_record("does-not-exist", "2.0.0")
                .unwrap()
                .is_empty()
        );
        assert_eq!(second_client.transport.call_count(), 1);
    }

    #[test]
    fn sparse_index_transport_failure_is_not_cached() {
        let url = index_url("retryable");
        let transport = FakeTransport::default();
        transport.push(&url, ScriptedResponse::Error);
        transport.push(&url, ScriptedResponse::Http(404, String::new()));

        let mut client = CratesIoClient::with_transport(transport, None, 24, instant_policy(1));
        assert!(matches!(
            client.fetch_index_record("retryable", "1.0.0"),
            Err(FetchError::Retryable(_))
        ));
        assert!(
            client
                .fetch_index_record("retryable", "1.0.0")
                .unwrap()
                .is_empty()
        );
        assert_eq!(client.transport.call_count(), 2);
    }

    #[test]
    fn fetch_index_record_cache_hit_issues_no_request() {
        let transport = FakeTransport::default();
        let mut client = fast_client(transport);
        client
            .cache
            .set_index_records("serde", vec![record("1.0.0")]);

        let records = client.fetch_index_record("serde", "1.0.0").unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].vers, "1.0.0");
        assert_eq!(client.transport.call_count(), 0);
    }

    #[test]
    fn fetch_index_record_missing_version_is_refetched_once_per_run() {
        // A non-empty cached entry that lacks the needed version is fetched
        // once per run across repeated lookups, not once per lookup.
        let url = index_url("serde");
        let transport = FakeTransport::default();
        transport.push(
            &url,
            ScriptedResponse::Http(200, r#"{"vers":"1.0.1","yanked":false}"#.to_string()),
        );

        let mut client = fast_client(transport);
        client
            .cache
            .set_index_records("serde", vec![record("1.0.0")]);

        for _ in 0..2 {
            let records = client.fetch_index_record("serde", "1.0.1").unwrap();
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].vers, "1.0.1");
        }
        assert_eq!(client.transport.call_count(), 1);
    }

    #[test]
    fn fetch_index_record_memo_holds_records_even_with_zero_cache_age() {
        // With `cache_max_age_hours = 0`, two lookups in one run issue one
        // request and both return the real records. This must fail if the
        // memo were a name set that re-reads the cache, since a
        // just-written cache entry would then be judged expired.
        let url = index_url("serde");
        let transport = FakeTransport::default();
        transport.push(
            &url,
            ScriptedResponse::Http(200, r#"{"vers":"1.0.0","yanked":false}"#.to_string()),
        );

        let mut client = CratesIoClient::with_transport(transport, None, 0, instant_policy(3));

        for _ in 0..2 {
            let records = client.fetch_index_record("serde", "1.0.0").unwrap();
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].vers, "1.0.0");
        }
        assert_eq!(client.transport.call_count(), 1);
    }
}

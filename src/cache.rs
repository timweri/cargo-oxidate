use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::api::{CrateVersionInfo, IndexRecord};

const CACHE_VERSION: u32 = 1;

#[derive(Serialize, Deserialize, Default)]
struct CacheData {
    version: u32,
    publish_dates: HashMap<String, DateTime<Utc>>,
    all_versions: HashMap<String, AllVersionsEntry>,
    #[serde(default)]
    index_records: HashMap<String, IndexRecordsEntry>,
}

#[derive(Serialize, Deserialize)]
struct AllVersionsEntry {
    fetched_at: DateTime<Utc>,
    versions: Vec<CrateVersionInfo>,
}

#[derive(Serialize, Deserialize)]
struct IndexRecordsEntry {
    fetched_at: DateTime<Utc>,
    records: Vec<IndexRecord>,
}

pub struct ResponseCache {
    path: Option<PathBuf>,
    data: CacheData,
    dirty: bool,
    load_warning: Option<CacheWarning>,
}

/// A cache failure that does not stop the dependency check.
pub struct CacheWarning {
    pub path: PathBuf,
    pub message: String,
}

impl ResponseCache {
    pub fn load(path: Option<&Path>) -> Self {
        let path = path.map(Path::to_path_buf);
        let mut loaded = None;
        let mut load_warning = None;

        if let Some(p) = &path {
            let mut warn = |message: String| {
                load_warning = Some(CacheWarning {
                    path: p.clone(),
                    message,
                });
            };
            match std::fs::read_to_string(p) {
                Ok(contents) => match serde_json::from_str::<CacheData>(&contents) {
                    Ok(data) if data.version == CACHE_VERSION => loaded = Some(data),
                    Ok(data) => warn(format!(
                        "Cache version {} is unsupported. Checking without cached responses",
                        data.version
                    )),
                    Err(e) => warn(format!(
                        "Could not read cache: {e}. Checking without cached responses"
                    )),
                },
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => warn(format!(
                    "Could not read cache: {error}. Checking without cached responses"
                )),
            }
        }

        Self {
            path,
            data: loaded.unwrap_or_else(|| CacheData {
                version: CACHE_VERSION,
                ..Default::default()
            }),
            dirty: false,
            load_warning,
        }
    }

    pub fn take_load_warning(&mut self) -> Option<CacheWarning> {
        self.load_warning.take()
    }

    pub fn save_warning(&self, error: anyhow::Error) -> CacheWarning {
        CacheWarning {
            path: self
                .path
                .clone()
                .expect("cache save failures require a configured path"),
            message: format!(
                "Could not save cache: {error}. This does not change the dependency check result"
            ),
        }
    }

    pub fn get_publish_date(&self, name: &str, version: &str) -> Option<DateTime<Utc>> {
        let key = format!("{name}/{version}");
        self.data.publish_dates.get(&key).copied()
    }

    pub fn set_publish_date(&mut self, name: &str, version: &str, date: DateTime<Utc>) {
        let key = format!("{name}/{version}");
        self.data.publish_dates.insert(key, date);
        self.dirty = true;
    }

    pub fn get_all_versions(&self, name: &str, max_age: Duration) -> Option<Vec<CrateVersionInfo>> {
        let entry = self.data.all_versions.get(name)?;
        (Utc::now() - entry.fetched_at <= max_age).then(|| entry.versions.clone())
    }

    pub fn set_all_versions(&mut self, name: &str, versions: Vec<CrateVersionInfo>) {
        let entry = AllVersionsEntry {
            fetched_at: Utc::now(),
            versions,
        };
        self.data.all_versions.insert(name.to_string(), entry);
        self.dirty = true;
    }

    pub fn get_index_records(&self, name: &str, max_age: Duration) -> Option<Vec<IndexRecord>> {
        let entry = self.data.index_records.get(name)?;
        (Utc::now() - entry.fetched_at <= max_age).then(|| entry.records.clone())
    }

    pub fn set_index_records(&mut self, name: &str, records: Vec<IndexRecord>) {
        let entry = IndexRecordsEntry {
            fetched_at: Utc::now(),
            records,
        };
        self.data.index_records.insert(name.to_string(), entry);
        self.dirty = true;
    }

    pub fn save(&self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }

        let Some(ref path) = self.path else {
            return Ok(());
        };

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("Failed to create cache directory: {}", parent.display())
            })?;
        }

        let tmp_path = path.with_extension("tmp");
        let contents =
            serde_json::to_string(&self.data).context("Failed to serialize cache data")?;

        std::fs::write(&tmp_path, contents)
            .with_context(|| format!("Failed to write cache to {}", tmp_path.display()))?;

        std::fs::rename(&tmp_path, path).with_context(|| {
            format!(
                "Failed to rename cache from {} to {}",
                tmp_path.display(),
                path.display()
            )
        })?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn sample_date() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2024-09-02T15:30:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn sample_version() -> CrateVersionInfo {
        CrateVersionInfo {
            num: "1.0.0".to_string(),
            created_at: sample_date(),
            yanked: false,
        }
    }

    #[test]
    fn round_trip_publish_date() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("cache.json");

        let mut cache = ResponseCache::load(Some(&path));
        cache.set_publish_date("serde", "1.0.0", sample_date());
        cache.save().unwrap();

        let loaded = ResponseCache::load(Some(&path));
        assert_eq!(
            loaded.get_publish_date("serde", "1.0.0"),
            Some(sample_date())
        );
        assert_eq!(loaded.get_publish_date("serde", "9.9.9"), None);
        assert!(
            !path.with_extension("tmp").exists(),
            "tmp file should be renamed away"
        );
    }

    #[test]
    fn all_versions_ttl_expiry() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("cache.json");

        let mut cache = ResponseCache::load(Some(&path));
        cache.set_all_versions("serde", vec![sample_version()]);

        // Fresh entry within TTL is returned.
        assert!(
            cache
                .get_all_versions("serde", Duration::hours(1))
                .is_some()
        );

        // With TTL of 0 (or negative), the entry is considered stale.
        assert!(
            cache
                .get_all_versions("serde", Duration::seconds(-1))
                .is_none()
        );
    }

    #[test]
    fn empty_index_record_cache_entry_expires() {
        let mut cache = ResponseCache::load(None);
        cache.set_index_records("does-not-exist", vec![]);

        assert!(
            cache
                .get_index_records("does-not-exist", Duration::hours(1))
                .is_some_and(|records| records.is_empty())
        );
        assert!(
            cache
                .get_index_records("does-not-exist", Duration::seconds(-1))
                .is_none()
        );
    }

    #[test]
    fn unusable_file_starts_fresh_with_a_warning() {
        let cases = [
            ("{ not valid json", "Could not read cache"),
            (
                r#"{"version":99,"publish_dates":{"serde/1.0.0":"2020-01-01T00:00:00Z"},"all_versions":{}}"#,
                "Cache version 99 is unsupported",
            ),
        ];
        for (contents, expected) in cases {
            let dir = tempdir().unwrap();
            let path = dir.path().join("cache.json");
            std::fs::write(&path, contents).unwrap();

            let mut cache = ResponseCache::load(Some(&path));
            assert_eq!(cache.get_publish_date("serde", "1.0.0"), None);
            assert_eq!(cache.data.version, CACHE_VERSION);
            let warning = cache
                .take_load_warning()
                .expect("unusable cache should produce a warning");
            assert_eq!(warning.path, path);
            assert!(warning.message.contains(expected));
            assert!(
                warning
                    .message
                    .contains("Checking without cached responses")
            );
        }
    }

    #[test]
    fn clean_save_writes_nothing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("cache.json");

        let cache = ResponseCache::load(Some(&path));
        cache.save().unwrap();

        assert!(!path.exists(), "save() must not write when cache is clean");
    }

    #[test]
    fn load_missing_file_is_empty() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("does-not-exist.json");

        let mut cache = ResponseCache::load(Some(&path));
        assert_eq!(cache.get_publish_date("serde", "1.0.0"), None);
        assert!(cache.take_load_warning().is_none());
    }

    #[test]
    fn no_path_save_is_noop() {
        let mut cache = ResponseCache::load(None);
        cache.set_publish_date("serde", "1.0.0", sample_date());
        // Should not panic or error even though there's no path.
        cache.save().unwrap();
    }

    #[test]
    fn cache_file_without_index_records_still_loads() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("cache.json");

        // A cache file as written by a release before index_records existed.
        std::fs::write(
            &path,
            r#"{"version":1,"publish_dates":{"serde/1.0.0":"2020-01-01T00:00:00Z"},"all_versions":{}}"#,
        )
        .unwrap();

        let cache = ResponseCache::load(Some(&path));
        assert_eq!(
            cache.get_publish_date("serde", "1.0.0"),
            Some(
                DateTime::parse_from_rfc3339("2020-01-01T00:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc)
            )
        );
        assert!(
            cache
                .get_index_records("serde", Duration::hours(1))
                .is_none()
        );
    }
}

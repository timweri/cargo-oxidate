use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::api::CrateVersionInfo;

#[derive(Serialize, Deserialize, Default)]
struct CacheData {
    version: u32,
    publish_dates: HashMap<String, DateTime<Utc>>,
    all_versions: HashMap<String, AllVersionsEntry>,
}

#[derive(Serialize, Deserialize)]
struct AllVersionsEntry {
    fetched_at: DateTime<Utc>,
    versions: Vec<CrateVersionInfo>,
}

pub struct ResponseCache {
    path: Option<PathBuf>,
    data: CacheData,
    dirty: bool,
}

impl ResponseCache {
    pub fn load(path: Option<&Path>) -> Self {
        let path = path.map(|p| p.to_path_buf());

        let data = if let Some(ref p) = path {
            match std::fs::read_to_string(p) {
                Ok(contents) => match serde_json::from_str::<CacheData>(&contents) {
                    Ok(data) => data,
                    Err(e) => {
                        eprintln!(
                            "Warning: corrupted cache file at {}, starting fresh: {e}",
                            p.display()
                        );
                        CacheData {
                            version: 1,
                            ..Default::default()
                        }
                    }
                },
                Err(_) => CacheData {
                    version: 1,
                    ..Default::default()
                },
            }
        } else {
            CacheData {
                version: 1,
                ..Default::default()
            }
        };

        Self {
            path,
            data,
            dirty: false,
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
        let age = Utc::now() - entry.fetched_at;

        if age > max_age {
            return None;
        }

        Some(entry.versions.clone())
    }

    pub fn set_all_versions(&mut self, name: &str, versions: Vec<CrateVersionInfo>) {
        let entry = AllVersionsEntry {
            fetched_at: Utc::now(),
            versions,
        };
        self.data.all_versions.insert(name.to_string(), entry);
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
    fn corrupt_file_starts_fresh() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("cache.json");
        std::fs::write(&path, "{ not valid json").unwrap();

        let cache = ResponseCache::load(Some(&path));
        assert_eq!(cache.get_publish_date("anything", "0.0.1"), None);
        assert!(
            cache
                .get_all_versions("anything", Duration::hours(1))
                .is_none()
        );
    }

    #[test]
    fn save_leaves_no_tmp_orphan() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("cache.json");

        let mut cache = ResponseCache::load(Some(&path));
        cache.set_publish_date("serde", "1.0.0", sample_date());
        cache.save().unwrap();

        let tmp = path.with_extension("tmp");
        assert!(!tmp.exists(), "tmp file should be renamed away");
        assert!(path.exists(), "final cache file should exist");
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

        let cache = ResponseCache::load(Some(&path));
        assert_eq!(cache.get_publish_date("serde", "1.0.0"), None);
    }

    #[test]
    fn no_path_save_is_noop() {
        let mut cache = ResponseCache::load(None);
        cache.set_publish_date("serde", "1.0.0", sample_date());
        // Should not panic or error even though there's no path.
        cache.save().unwrap();
    }
}

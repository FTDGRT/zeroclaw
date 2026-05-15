use chrono::{Local, NaiveDate, TimeDelta};
use std::path::Path;

use crate::config::RotationConfig;
use crate::error::Result;
use crate::rotate::parse_rotated_filename;

/// Statistics returned after a cleanup pass.
#[derive(Debug, Default)]
pub struct CleanupStats {
    pub files_deleted: usize,
    pub bytes_freed: u64,
}

/// A parsed rotated-file name with its date and sequence number.
struct RotatedEntry {
    path: std::path::PathBuf,
    date: NaiveDate,
    seq: u32,
    size: u64,
}

/// Clean up rotated files that exceed the configured retention policy.
///
/// Two criteria are applied:
/// 1. Delete files older than `max_age_days`
/// 2. If the remaining count exceeds `max_rotated_files`, delete the oldest
pub async fn cleanup_rotated_files(
    active_path: &Path,
    config: &RotationConfig,
    now: &chrono::DateTime<Local>,
) -> Result<CleanupStats> {
    let dir = active_path.parent().unwrap_or(Path::new("."));
    let stem = active_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("log");
    let ext = active_path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("log");

    let mut entries: Vec<RotatedEntry> = Vec::new();

    let read_dir = tokio::fs::read_dir(dir)
        .await
        .map_err(|e| crate::error::RotationError::io(dir, e))?;

    let mut dir_entries = read_dir;
    while let Some(entry) = dir_entries
        .next_entry()
        .await
        .map_err(|e| crate::error::RotationError::io(dir, e))?
    {
        let name = entry.file_name();
        let name = name.to_string_lossy();

        let Some(parts) = parse_rotated_filename(&name, stem, ext) else {
            continue;
        };

        let metadata = entry
            .metadata()
            .await
            .map_err(|e| crate::error::RotationError::io(entry.path(), e))?;

        entries.push(RotatedEntry {
            path: entry.path(),
            date: parts.date,
            seq: parts.seq,
            size: metadata.len(),
        });
    }

    let cutoff = now.date_naive() - TimeDelta::days(config.max_age_days as i64);
    let mut stats = CleanupStats::default();

    // Phase 1: Delete files older than max_age_days
    let mut retained = Vec::with_capacity(entries.len());
    for entry in entries {
        if entry.date < cutoff {
            match tokio::fs::remove_file(&entry.path).await {
                Ok(()) => {
                    stats.files_deleted += 1;
                    stats.bytes_freed += entry.size;
                }
                Err(e) => {
                    tracing::warn!(path = %entry.path.display(), error = %e, "Failed to delete aged rotated file");
                    retained.push(entry);
                }
            }
        } else {
            retained.push(entry);
        }
    }
    entries = retained;

    // Phase 2: If still over max_rotated_files, delete oldest
    entries.sort_by_key(|e| (e.date, e.seq));
    let excess = entries.len().saturating_sub(config.max_rotated_files);
    if excess > 0 {
        for entry in entries.iter().take(excess) {
            match tokio::fs::remove_file(&entry.path).await {
                Ok(()) => {
                    stats.files_deleted += 1;
                    stats.bytes_freed += entry.size;
                }
                Err(e) => {
                    tracing::warn!(path = %entry.path.display(), error = %e, "Failed to delete excess rotated file");
                }
            }
        }
    }

    if stats.files_deleted > 0 {
        tracing::info!(
            files_deleted = stats.files_deleted,
            bytes_freed = stats.bytes_freed,
            "Cleaned up rotated files"
        );
    }

    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[tokio::test]
    async fn cleanup_deletes_aged_files() {
        let tmp = tempfile::tempdir().unwrap();
        let active_path = tmp.path().join("app.log");

        let now = Local::now();
        let old_date = (now - TimeDelta::days(60)).date_naive();
        let recent_date = (now - TimeDelta::days(5)).date_naive();

        let old_file = tmp
            .path()
            .join(format!("app.{}.1.log", old_date.format("%Y-%m-%d")));
        let recent_file = tmp
            .path()
            .join(format!("app.{}.1.log", recent_date.format("%Y-%m-%d")));

        fs::write(&old_file, "old data that is long enough").unwrap();
        fs::write(&recent_file, "recent data").unwrap();

        let config = RotationConfig {
            max_age_days: 30,
            ..Default::default()
        };

        let stats = cleanup_rotated_files(&active_path, &config, &now)
            .await
            .unwrap();

        assert_eq!(stats.files_deleted, 1);
        assert!(!old_file.exists());
        assert!(recent_file.exists());
    }

    #[tokio::test]
    async fn cleanup_enforces_max_file_count() {
        let tmp = tempfile::tempdir().unwrap();
        let active_path = tmp.path().join("app.log");

        let now = Local::now();
        let date = now.date_naive();

        for i in 1..=5 {
            let name = format!("app.{}.{}.log", date.format("%Y-%m-%d"), i);
            fs::write(tmp.path().join(&name), format!("entry-{i}")).unwrap();
        }

        let config = RotationConfig {
            max_age_days: 365,
            max_rotated_files: 3,
            ..Default::default()
        };

        let stats = cleanup_rotated_files(&active_path, &config, &now)
            .await
            .unwrap();

        assert_eq!(stats.files_deleted, 2);

        // The two oldest (seq 1, 2) should be gone
        assert!(
            !tmp.path()
                .join(format!("app.{}.1.log", date.format("%Y-%m-%d")))
                .exists()
        );
        assert!(
            !tmp.path()
                .join(format!("app.{}.2.log", date.format("%Y-%m-%d")))
                .exists()
        );
    }
}

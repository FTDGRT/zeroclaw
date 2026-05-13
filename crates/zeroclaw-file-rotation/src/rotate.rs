use chrono::Local;
use std::path::Path;

use crate::error::{Result, RotationError};

/// 轮转文件名的解析结果。
pub struct RotatedNameParts {
    pub date: chrono::NaiveDate,
    pub seq: u32,
}

/// 解析形如 `app.2026-05-06.1.jsonl` 的轮转文件名。
/// 不匹配预期格式时返回 `None`。
pub fn parse_rotated_filename(name: &str, stem: &str, ext: &str) -> Option<RotatedNameParts> {
    let prefix = format!("{stem}.");
    let suffix = format!(".{ext}");
    if !name.starts_with(&prefix) || !name.ends_with(&suffix) {
        return None;
    }
    let start = prefix.len();
    let end = name.len().saturating_sub(suffix.len());
    if start >= end {
        return None;
    }
    let middle = &name[start..end];
    let (date_str, seq_str) = middle.rsplit_once('.')?;
    let date = chrono::NaiveDate::parse_from_str(date_str, "%Y-%m-%d").ok()?;
    let seq = seq_str.parse::<u32>().ok()?;
    Some(RotatedNameParts { date, seq })
}

/// 如果活跃文件的 mtime 日期与 `now` 不同，返回 mtime 日期（表示需要轮转）。
/// 文件不存在或不需要轮转时返回 `None`。
pub fn check_date_rotation(
    path: &Path,
    now: &chrono::DateTime<Local>,
) -> Option<chrono::NaiveDate> {
    let metadata = std::fs::metadata(path).ok()?;
    let modified = metadata.modified().ok()?;
    let modified_dt: chrono::DateTime<Local> = modified.into();
    let mtime_date = modified_dt.date_naive();
    if mtime_date != now.date_naive() {
        Some(mtime_date)
    } else {
        None
    }
}

/// Check whether the file at `path` exceeds `max_size` bytes.
/// Returns `false` if the file does not exist.
pub fn should_rotate_by_size(path: &Path, max_size: u64) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    metadata.len() >= max_size
}

/// Rotate the active file by renaming it to a date + sequence-number pattern.
///
/// The rotated file name format is: `<stem>.YYYY-MM-DD.<seq>.<ext>`
/// Sequence numbers start at 1 and increment for same-day rotations.
/// `date` is explicitly provided by the caller:
///   - For date rotation: the file's mtime date (data belongs to that day)
///   - For size rotation: the current day (data was just written today)
pub async fn rotate_file(active_path: &Path, date: chrono::NaiveDate) -> Result<()> {
    if !active_path.exists() {
        return Ok(());
    }

    let seq = next_sequence(active_path, date);
    let rotated_name = format_rotated_name(active_path, date, seq);
    let rotated_path = active_path.with_file_name(&rotated_name);

    let result = tokio::fs::rename(active_path, &rotated_path).await;
    if let Err(e) = result {
        if e.raw_os_error() == Some(18) {
            // EXDEV: cross-device link — fallback to copy + delete
            tokio::fs::copy(active_path, &rotated_path)
                .await
                .map_err(|e2| RotationError::io(&rotated_path, e2))?;
            tokio::fs::remove_file(active_path)
                .await
                .map_err(|e2| RotationError::io(active_path, e2))?;
        } else {
            return Err(RotationError::io(&rotated_path, e));
        }
    }

    tracing::debug!(
        active = %active_path.display(),
        rotated = %rotated_path.display(),
        "Rotated file"
    );

    Ok(())
}

/// Format a rotated file name from the active file path, a date, and a sequence number.
///
/// E.g. `runtime-trace.jsonl` + `2026-04-20` + 1 → `runtime-trace.2026-04-20.1.jsonl`
pub fn format_rotated_name(active_path: &Path, date: chrono::NaiveDate, seq: u32) -> String {
    let stem = active_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("log");
    let ext = active_path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("log");
    format!("{stem}.{date}.{seq}.{ext}")
}

/// Scan the directory for rotated files with the same stem and date, and return
/// the next available sequence number (max existing + 1, or 1 if none exist).
fn next_sequence(active_path: &Path, date: chrono::NaiveDate) -> u32 {
    let dir = active_path.parent().unwrap_or(Path::new("."));
    let stem = active_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("log");
    let ext = active_path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("log");

    let Ok(entries) = std::fs::read_dir(dir) else {
        return 1;
    };

    let mut max_seq: u32 = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(parts) = parse_rotated_filename(&name, stem, ext)
            && parts.date == date
            && parts.seq > max_seq
        {
            max_seq = parts.seq;
        }
    }

    max_seq + 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn parse_rotated_filename_valid() {
        let parts =
            parse_rotated_filename("runtime-trace.2026-05-06.1.jsonl", "runtime-trace", "jsonl");
        let p = parts.unwrap();
        assert_eq!(p.date, chrono::NaiveDate::from_ymd_opt(2026, 5, 6).unwrap());
        assert_eq!(p.seq, 1);
    }

    #[test]
    fn parse_rotated_filename_higher_seq() {
        let parts = parse_rotated_filename("app.2025-12-31.42.log", "app", "log");
        let p = parts.unwrap();
        assert_eq!(
            p.date,
            chrono::NaiveDate::from_ymd_opt(2025, 12, 31).unwrap()
        );
        assert_eq!(p.seq, 42);
    }

    #[test]
    fn parse_rotated_filename_malformed_bad_date() {
        let parts = parse_rotated_filename("app.not-a-date.1.log", "app", "log");
        assert!(parts.is_none());
    }

    #[test]
    fn parse_rotated_filename_malformed_bad_seq() {
        let parts = parse_rotated_filename("app.2026-05-06.abc.log", "app", "log");
        assert!(parts.is_none());
    }

    #[test]
    fn parse_rotated_filename_wrong_stem() {
        let parts = parse_rotated_filename("other.2026-05-06.1.log", "app", "log");
        assert!(parts.is_none());
    }

    #[test]
    fn parse_rotated_filename_wrong_ext() {
        let parts = parse_rotated_filename("app.2026-05-06.1.txt", "app", "log");
        assert!(parts.is_none());
    }

    #[test]
    fn format_rotated_name_basic() {
        let path = Path::new("state/runtime-trace.jsonl");
        let date = chrono::NaiveDate::from_ymd_opt(2026, 4, 20).unwrap();
        assert_eq!(
            format_rotated_name(path, date, 1),
            "runtime-trace.2026-04-20.1.jsonl"
        );
        assert_eq!(
            format_rotated_name(path, date, 3),
            "runtime-trace.2026-04-20.3.jsonl"
        );
    }

    #[test]
    fn next_sequence_returns_one_when_no_files() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("app.log");
        let date = chrono::NaiveDate::from_ymd_opt(2026, 4, 20).unwrap();
        assert_eq!(next_sequence(&path, date), 1);
    }

    #[test]
    fn next_sequence_increments_existing() {
        let tmp = tempfile::tempdir().unwrap();
        let date = chrono::NaiveDate::from_ymd_opt(2026, 4, 20).unwrap();

        // Create existing rotated files
        fs::write(tmp.path().join("app.2026-04-20.1.log"), "").unwrap();
        fs::write(tmp.path().join("app.2026-04-20.2.log"), "").unwrap();

        let path = tmp.path().join("app.log");
        assert_eq!(next_sequence(&path, date), 3);
    }

    #[test]
    fn should_rotate_by_size_returns_false_for_missing() {
        assert!(!should_rotate_by_size(Path::new("/nonexistent"), 100));
    }

    #[test]
    fn should_rotate_by_size_checks_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test.log");
        fs::write(&path, "hello").unwrap();
        assert!(!should_rotate_by_size(&path, 100));
        assert!(should_rotate_by_size(&path, 3));
    }

    #[test]
    fn check_date_rotation_returns_mtime_date_for_old_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("old.log");
        fs::write(&path, "data").unwrap();

        let yesterday = std::time::SystemTime::now() - std::time::Duration::from_secs(24 * 3600);
        filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(yesterday)).unwrap();

        let now = Local::now();
        let result = check_date_rotation(&path, &now);
        assert!(result.is_some(), "Old file should trigger date rotation");
        let yesterday_date = (now - chrono::Duration::seconds(24 * 3600)).date_naive();
        assert_eq!(result.unwrap(), yesterday_date);
    }

    #[test]
    fn check_date_rotation_returns_none_for_today() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("today.log");
        fs::write(&path, "data").unwrap();

        let now = Local::now();
        assert!(
            check_date_rotation(&path, &now).is_none(),
            "File modified today should not trigger date rotation"
        );
    }

    #[test]
    fn check_date_rotation_returns_none_for_missing() {
        let now = Local::now();
        assert!(
            check_date_rotation(Path::new("/nonexistent_12345"), &now).is_none(),
            "Missing file should not trigger date rotation"
        );
    }

    #[tokio::test]
    async fn rotate_file_missing_path_is_noop() {
        let date = chrono::NaiveDate::from_ymd_opt(2026, 5, 7).unwrap();
        let result = rotate_file(Path::new("/nonexistent_12345.log"), date).await;
        assert!(result.is_ok(), "Rotating a missing file should be Ok(())");
    }

    #[test]
    fn next_sequence_with_gap() {
        let tmp = tempfile::tempdir().unwrap();
        let date = chrono::NaiveDate::from_ymd_opt(2026, 4, 20).unwrap();

        // Create files with a gap: seq 1 and 5 exist, 2-4 are missing
        fs::write(tmp.path().join("app.2026-04-20.1.log"), "").unwrap();
        fs::write(tmp.path().join("app.2026-04-20.5.log"), "").unwrap();

        let path = tmp.path().join("app.log");
        // Should return max(1,5) + 1 = 6
        assert_eq!(
            next_sequence(&path, date),
            6,
            "Should return max sequence + 1 even with gaps"
        );
    }

    #[test]
    fn format_rotated_name_with_no_extension() {
        let path = Path::new("state/mylog");
        let date = chrono::NaiveDate::from_ymd_opt(2026, 4, 20).unwrap();
        let name = format_rotated_name(path, date, 1);
        assert_eq!(name, "mylog.2026-04-20.1.log");
    }
}

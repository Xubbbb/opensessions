//! Shared append-only debug log for live diagnosis.
//!
//! Logging is off unless `OPENSESSIONS_DEBUG_LOG` names a file (an empty value
//! also means off). Every opensessions process appends to that one file, so:
//!
//! - each line is written with a single `write_all` on an `O_APPEND` handle so
//!   concurrent writers never interleave mid-line;
//! - the file is truncated in place once it grows past [`MAX_LOG_BYTES`], so a
//!   long-running server cannot fill `/tmp` (an always-on log without a cap
//!   grew by ~100 MB a day).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// Truncate the log once it exceeds this size.
pub const MAX_LOG_BYTES: u64 = 16 * 1024 * 1024;

const PATH_ENV: &str = "OPENSESSIONS_DEBUG_LOG";

/// The configured log file, resolved once per process from
/// `OPENSESSIONS_DEBUG_LOG`; `None` when logging is disabled.
pub fn log_path() -> Option<&'static Path> {
    static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    PATH.get_or_init(|| {
        std::env::var_os(PATH_ENV)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    })
    .as_deref()
}

/// Append one line as `[<unix ms>] [<tag>] <line>`, e.g. tag `server pid=42`.
/// Errors are swallowed: diagnostics must never take the product down.
pub fn log_with_tag(tag: &str, line: impl AsRef<str>) {
    if let Some(path) = log_path() {
        append_line(path, tag, line.as_ref());
    }
}

fn append_line(path: &Path, tag: &str, line: &str) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or(0);
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };
    if file
        .metadata()
        .is_ok_and(|metadata| metadata.len() > MAX_LOG_BYTES)
    {
        // In-place truncation: other O_APPEND writers simply continue at the
        // new end of file, so no cross-process coordination is needed.
        let _ = file.set_len(0);
    }
    let _ = file.write_all(format!("[{now}] [{tag}] {line}\n").as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "opensessions-debug-log-{name}-{}",
            std::process::id()
        ))
    }

    #[test]
    fn appends_tagged_lines() {
        let path = scratch_path("append");
        let _ = std::fs::remove_file(&path);

        append_line(&path, "server pid=1", "first");
        append_line(&path, "sidebar pid=2", "second");

        let contents = std::fs::read_to_string(&path).expect("read log");
        let _ = std::fs::remove_file(&path);
        let lines = contents.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with('[') && lines[0].ends_with("] [server pid=1] first"));
        assert!(lines[1].ends_with("] [sidebar pid=2] second"));
    }

    #[test]
    fn oversized_log_is_truncated_before_the_append() {
        let path = scratch_path("cap");
        let file = std::fs::File::create(&path).expect("create log");
        file.set_len(MAX_LOG_BYTES + 1).expect("grow sparse log");
        drop(file);

        append_line(&path, "test", "after-cap");

        let contents = std::fs::read_to_string(&path).expect("read log");
        let _ = std::fs::remove_file(&path);
        assert!(
            contents.ends_with("[test] after-cap\n") && contents.len() < 128,
            "log should hold only the fresh line, got {} bytes",
            contents.len()
        );
    }
}

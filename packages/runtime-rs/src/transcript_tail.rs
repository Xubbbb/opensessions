//! Incremental readers for append-only transcripts.
//!
//! Agent transcripts (Claude Code and Codex JSONL files) only ever grow, and
//! the interesting ones can reach tens of megabytes. Re-parsing a whole file
//! every poll tick is what made the old watcher expensive, so a `TailCache`
//! remembers, per file, how far it has read and the parse state so far, and
//! on each refresh parses only the bytes appended since. A file that shrank
//! (rewritten, rotated) is parsed again from the start.

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Parse state that can absorb one transcript line at a time.
pub trait TailState: Default + Clone {
    fn apply_line(&mut self, line: &str);
}

#[derive(Debug, Clone)]
struct TailEntry<S> {
    len: u64,
    mtime_ms: u64,
    offset: u64,
    /// Bytes after the last newline, waiting for the rest of the line.
    partial: Vec<u8>,
    state: S,
}

#[derive(Debug, Clone)]
pub struct TailCache<S> {
    entries: HashMap<PathBuf, TailEntry<S>>,
}

impl<S> Default for TailCache<S> {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }
}

/// Result of one refresh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tail<S> {
    pub state: S,
    pub mtime_ms: u64,
    /// New lines were parsed since the previous refresh (or the file was
    /// read for the first time).
    pub changed: bool,
}

impl<S: TailState> TailCache<S> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bring the parse state for `path` up to date. `None` when the file
    /// cannot be read.
    pub fn refresh(&mut self, path: &Path) -> Option<Tail<S>> {
        let metadata = fs::metadata(path).ok()?;
        let len = metadata.len();
        let mtime_ms = metadata
            .modified()
            .ok()
            .and_then(|modified| modified.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or_default();

        let entry = self
            .entries
            .entry(path.to_path_buf())
            .or_insert_with(|| TailEntry {
                len: 0,
                mtime_ms: 0,
                offset: 0,
                partial: Vec::new(),
                state: S::default(),
            });
        if len == entry.len && mtime_ms == entry.mtime_ms && entry.offset == len {
            return Some(Tail {
                state: entry.state.clone(),
                mtime_ms,
                changed: false,
            });
        }
        if len < entry.offset {
            // Truncated or replaced: start over.
            entry.offset = 0;
            entry.partial.clear();
            entry.state = S::default();
        }

        let mut file = fs::File::open(path).ok()?;
        file.seek(SeekFrom::Start(entry.offset)).ok()?;
        let mut appended = Vec::new();
        file.read_to_end(&mut appended).ok()?;
        let read = appended.len() as u64;

        let mut buffer = std::mem::take(&mut entry.partial);
        buffer.extend_from_slice(&appended);
        let mut consumed = 0;
        while let Some(newline) = buffer[consumed..].iter().position(|byte| *byte == b'\n') {
            let line = &buffer[consumed..consumed + newline];
            let text = String::from_utf8_lossy(line);
            let text = text.trim();
            if !text.is_empty() {
                entry.state.apply_line(text);
            }
            consumed += newline + 1;
        }
        entry.partial = buffer[consumed..].to_vec();
        entry.offset += read;
        entry.len = len;
        entry.mtime_ms = mtime_ms;
        Some(Tail {
            state: entry.state.clone(),
            mtime_ms,
            changed: true,
        })
    }

    /// Forget files that are no longer of interest.
    pub fn retain(&mut self, keep: impl Fn(&Path) -> bool) {
        self.entries.retain(|path, _| keep(path));
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    struct Lines(Vec<String>);

    impl TailState for Lines {
        fn apply_line(&mut self, line: &str) {
            self.0.push(line.to_string());
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("opensessions-tail-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.join("t.jsonl")
    }

    #[test]
    fn parses_only_appended_lines_and_reports_whether_anything_changed() {
        let path = scratch("append");
        fs::write(&path, "one\ntwo\n").unwrap();
        let mut cache = TailCache::<Lines>::new();

        let first = cache.refresh(&path).unwrap();
        assert_eq!(first.state.0, vec!["one", "two"]);
        assert!(first.changed);

        let again = cache.refresh(&path).unwrap();
        assert!(!again.changed);
        assert_eq!(again.state, first.state);

        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"three\n").unwrap();
        drop(file);
        // Force a visible mtime/len change regardless of timestamp granularity.
        let third = cache.refresh(&path).unwrap();
        assert!(third.changed);
        assert_eq!(third.state.0, vec!["one", "two", "three"]);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn holds_back_an_unterminated_last_line_until_it_completes() {
        let path = scratch("partial");
        fs::write(&path, "one\ntw").unwrap();
        let mut cache = TailCache::<Lines>::new();

        assert_eq!(cache.refresh(&path).unwrap().state.0, vec!["one"]);

        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"o\nthree\n").unwrap();
        drop(file);
        assert_eq!(
            cache.refresh(&path).unwrap().state.0,
            vec!["one", "two", "three"]
        );
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_file_that_shrank_is_parsed_from_scratch() {
        let path = scratch("shrink");
        fs::write(&path, "one\ntwo\nthree\n").unwrap();
        let mut cache = TailCache::<Lines>::new();
        cache.refresh(&path).unwrap();

        fs::write(&path, "fresh\n").unwrap();

        assert_eq!(cache.refresh(&path).unwrap().state.0, vec!["fresh"]);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn missing_files_yield_none_and_retain_drops_forgotten_paths() {
        let path = scratch("missing");
        let mut cache = TailCache::<Lines>::new();
        assert!(cache.refresh(&path).is_none());
        fs::write(&path, "x\n").unwrap();
        cache.refresh(&path).unwrap();
        assert_eq!(cache.len(), 1);

        cache.retain(|_| false);

        assert!(cache.is_empty());
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }
}

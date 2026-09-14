//! Claude Code's session registry.
//!
//! Every interactive `claude` process (2.1.x and later) keeps
//! `<config dir>/sessions/<pid>.json` up to date with its UI state: the
//! session id, the tmux pane it was started in, the user-visible name and a
//! status that changes exactly when the status line does (`busy`, `idle`,
//! `waiting` with a reason, or `shell` while a background command runs).
//! Claude Code writes it for its own cross-session messaging, which makes it
//! the ground truth this runtime uses for Claude Code identity, pane binding
//! and status. Transcripts only enrich what the registry says.
//!
//! The format is internal to Claude Code, so parsing is defensive: unknown
//! fields are ignored, an unknown status reads as idle, and anything that
//! does not carry a session id is skipped.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::mux::MuxPane;

/// Agent id used for every Claude Code row, whatever its source.
pub const CLAUDE_CODE_AGENT: &str = "claude-code";

/// Registry `status` values, in Claude Code's own vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RegistryStatus {
    /// A turn is in flight, or delegated work (background subagents,
    /// workflows, teammates) is still running.
    Busy,
    /// At the prompt, nothing running.
    Idle,
    /// A dialog, permission prompt or elicitation is open; see
    /// `waiting_for`.
    Waiting,
    /// At the prompt, but a background shell command is still running.
    Shell,
}

impl RegistryStatus {
    fn parse(raw: Option<&str>) -> Self {
        match raw {
            Some("busy") => Self::Busy,
            Some("waiting") => Self::Waiting,
            Some("shell") => Self::Shell,
            _ => Self::Idle,
        }
    }
}

/// The `tmux` field: where the process was started, captured by Claude Code
/// at startup as `#{session_name}:#{window_id}.#{pane_id}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxTarget {
    pub session_name: String,
    pub window_id: String,
    pub pane_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeRegistryRecord {
    pub pid: u32,
    pub session_id: String,
    pub cwd: Option<String>,
    pub status: RegistryStatus,
    pub waiting_for: Option<String>,
    pub status_updated_at: Option<u64>,
    pub started_at: Option<u64>,
    /// Pane the process was started in; `None` outside tmux.
    pub tmux: Option<TmuxTarget>,
    /// The `/rename` name (or a derived one); see `name_source`.
    pub name: Option<String>,
    pub name_source: Option<String>,
    /// Set for teammate processes spawned by another Claude session.
    pub agent: Option<String>,
    /// Process start token (Linux: `/proc/<pid>/stat` starttime) that lets a
    /// reader tell a reused pid from the process that wrote the record.
    pub proc_start: Option<String>,
    /// Config directory the record was found under; its `projects/` holds
    /// the session transcript.
    pub config_dir: PathBuf,
}

/// Parse `<config>/sessions/<file_name>` contents. Only interactive
/// sessions with a session id are records; the pid comes from the file name,
/// as in Claude Code's own reader.
pub fn parse_registry_record(
    file_name: &str,
    raw: &str,
    config_dir: &Path,
) -> Option<ClaudeRegistryRecord> {
    let stem = file_name.strip_suffix(".json")?;
    let pid = stem.parse::<u32>().ok().filter(|pid| *pid > 0)?;
    let value: Value = serde_json::from_str(raw).ok()?;
    let object = value.as_object()?;
    if let Some(body_pid) = object.get("pid").and_then(Value::as_u64)
        && body_pid != u64::from(pid)
    {
        return None;
    }
    if let Some(kind) = object.get("kind").and_then(Value::as_str)
        && kind != "interactive"
    {
        return None;
    }
    let session_id = string_field(object, "sessionId")?;

    Some(ClaudeRegistryRecord {
        pid,
        session_id,
        cwd: string_field(object, "cwd"),
        status: RegistryStatus::parse(object.get("status").and_then(Value::as_str)),
        waiting_for: string_field(object, "waitingFor"),
        status_updated_at: object.get("statusUpdatedAt").and_then(Value::as_u64),
        started_at: object.get("startedAt").and_then(Value::as_u64),
        tmux: object
            .get("tmux")
            .and_then(Value::as_str)
            .and_then(parse_tmux_target),
        name: string_field(object, "name"),
        name_source: string_field(object, "nameSource"),
        agent: string_field(object, "agent"),
        proc_start: string_field(object, "procStart"),
        config_dir: config_dir.to_path_buf(),
    })
}

fn string_field(object: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

/// Parse `name:@window.%pane`. Session names may themselves contain `:` and
/// `.`; the window and pane ids never do, so split from the right.
pub fn parse_tmux_target(raw: &str) -> Option<TmuxTarget> {
    let (rest, pane_id) = raw.rsplit_once('.')?;
    let (session_name, window_id) = rest.rsplit_once(':')?;
    let pane_id = pane_id.strip_prefix('%')?;
    let window_id = window_id.strip_prefix('@')?;
    if session_name.is_empty()
        || pane_id.is_empty()
        || window_id.is_empty()
        || !pane_id.bytes().all(|byte| byte.is_ascii_digit())
        || !window_id.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    Some(TmuxTarget {
        session_name: session_name.to_string(),
        window_id: format!("@{window_id}"),
        pane_id: format!("%{pane_id}"),
    })
}

/// Every Claude Code config directory on this machine: `~/.claude`, the
/// `CLAUDE_CONFIG_DIR` override, then every `~/.claude*` sibling directory
/// that holds Claude state (multi-account setups), deduplicated in that
/// order.
pub fn claude_code_config_dirs(home: &Path) -> Vec<PathBuf> {
    claude_code_config_dirs_from(home, std::env::var_os("CLAUDE_CONFIG_DIR").as_deref())
}

pub fn claude_code_config_dirs_from(home: &Path, config_dir: Option<&OsStr>) -> Vec<PathBuf> {
    claude_code_config_dirs_with(home, config_dir, &[])
}

/// `claude_code_config_dirs_from` plus directories the user configured
/// explicitly (`claudeConfigDirs` in `config.json`), for accounts that live
/// outside `~/.claude*`.
pub fn claude_code_config_dirs_with(
    home: &Path,
    config_dir: Option<&OsStr>,
    configured: &[String],
) -> Vec<PathBuf> {
    let mut dirs = vec![home.join(".claude")];
    let mut push_unique = |dir: PathBuf| {
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    };

    if let Some(config_dir) = config_dir.filter(|value| !value.is_empty()) {
        push_unique(expand_home(home, Path::new(config_dir)));
    }
    for dir in configured {
        if !dir.is_empty() {
            push_unique(expand_home(home, Path::new(dir)));
        }
    }

    let mut siblings = fs::read_dir(home)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(".claude"))
        })
        .map(|entry| entry.path())
        .filter(|dir| dir.join("projects").is_dir() || dir.join("sessions").is_dir())
        .collect::<Vec<_>>();
    siblings.sort();
    for dir in siblings {
        push_unique(dir);
    }

    dirs
}

fn expand_home(home: &Path, path: &Path) -> PathBuf {
    match path.to_str() {
        Some("~") => home.to_path_buf(),
        Some(text) => match text.strip_prefix("~/") {
            Some(rest) => home.join(rest),
            None => path.to_path_buf(),
        },
        None => path.to_path_buf(),
    }
}

/// Read every record under `<config>/sessions` for each config dir.
pub fn scan_registry(config_dirs: &[PathBuf]) -> Vec<ClaudeRegistryRecord> {
    let mut records = Vec::new();
    for config_dir in config_dirs {
        let Ok(entries) = fs::read_dir(config_dir.join("sessions")) else {
            continue;
        };
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let Some(file_name) = file_name.to_str() else {
                continue;
            };
            if !file_name.ends_with(".json") {
                continue;
            }
            let Ok(raw) = fs::read_to_string(entry.path()) else {
                continue;
            };
            if let Some(record) = parse_registry_record(file_name, &raw, config_dir) {
                records.push(record);
            }
        }
    }
    records
}

/// What the runtime needs to know about a live process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessFacts {
    pub ppid: u32,
    /// Stable token for this incarnation of the pid (Linux: starttime in
    /// clock ticks), comparable with a record's `proc_start`.
    pub start_token: Option<String>,
}

/// Source of process facts; the system implementation reads `/proc`, tests
/// use a table.
pub trait ProcessInspector: Send + Sync {
    /// Facts about `pid`, or `None` when no such process exists.
    fn facts(&self, pid: u32) -> Option<ProcessFacts>;
}

/// `/proc`-backed inspector (Linux). On other platforms it falls back to
/// `ps`, which cannot supply a start token, so pid reuse goes unnoticed
/// there until the pane check catches it.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemProcessInspector;

impl ProcessInspector for SystemProcessInspector {
    fn facts(&self, pid: u32) -> Option<ProcessFacts> {
        if let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) {
            return parse_proc_stat(&stat);
        }
        if Path::new("/proc").is_dir() {
            return None;
        }
        let output = std::process::Command::new("ps")
            .args(["-o", "ppid=", "-p", &pid.to_string()])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let ppid = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse::<u32>()
            .ok()?;
        Some(ProcessFacts {
            ppid,
            start_token: None,
        })
    }
}

/// Parse `/proc/<pid>/stat`: `pid (comm) state ppid ... starttime ...`. The
/// command name may contain spaces and parentheses, so fields are counted
/// from the last `)`.
pub fn parse_proc_stat(stat: &str) -> Option<ProcessFacts> {
    let (_, rest) = stat.rsplit_once(')')?;
    let fields = rest.split_whitespace().collect::<Vec<_>>();
    // `rest` starts at field 3 (state); ppid is field 4, starttime field 22.
    let ppid = fields.get(1)?.parse::<u32>().ok()?;
    let start_token = fields.get(19).map(|token| (*token).to_string());
    Some(ProcessFacts { ppid, start_token })
}

/// Liveness of the process behind a record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordLiveness {
    Alive,
    /// No such pid, or the pid now belongs to a different process.
    Dead,
}

/// Registry records resolved against this mux: whether the process is
/// alive and, if it runs inside one of our panes, which one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRecord {
    pub record: ClaudeRegistryRecord,
    pub liveness: RecordLiveness,
    /// The pane of this mux whose process tree contains the record's pid.
    /// `None` means the session is not ours (another tmux server, outside
    /// tmux, or already gone).
    pub pane: Option<MuxPane>,
}

/// Resolves records against processes and panes, remembering process
/// ancestry per pid incarnation so the walk happens once per process.
#[derive(Default)]
pub struct RegistryResolver {
    ancestry_cache: HashMap<u32, (Option<String>, Vec<u32>)>,
}

const MAX_ANCESTRY_DEPTH: usize = 32;

impl RegistryResolver {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn resolve(
        &mut self,
        records: Vec<ClaudeRegistryRecord>,
        panes: &[MuxPane],
        inspector: &dyn ProcessInspector,
    ) -> Vec<ResolvedRecord> {
        let mut resolved = Vec::with_capacity(records.len());
        let mut live_pids = std::collections::HashSet::new();
        for record in records {
            let facts = inspector.facts(record.pid);
            let alive = facts.as_ref().is_some_and(|facts| {
                match (&record.proc_start, &facts.start_token) {
                    (Some(expected), Some(actual)) => expected == actual,
                    _ => true,
                }
            });
            if !alive {
                resolved.push(ResolvedRecord {
                    record,
                    liveness: RecordLiveness::Dead,
                    pane: None,
                });
                continue;
            }
            live_pids.insert(record.pid);
            let start_token = facts.and_then(|facts| facts.start_token);
            let ancestry = self.ancestry(record.pid, start_token, inspector);
            let pane = match_record_pane(&record, panes, &ancestry).cloned();
            resolved.push(ResolvedRecord {
                record,
                liveness: RecordLiveness::Alive,
                pane,
            });
        }
        self.ancestry_cache.retain(|pid, _| live_pids.contains(pid));
        resolved
    }

    fn ancestry(
        &mut self,
        pid: u32,
        start_token: Option<String>,
        inspector: &dyn ProcessInspector,
    ) -> Vec<u32> {
        if let Some((cached_token, ancestry)) = self.ancestry_cache.get(&pid)
            && *cached_token == start_token
        {
            return ancestry.clone();
        }
        let ancestry = process_ancestry(pid, inspector);
        self.ancestry_cache
            .insert(pid, (start_token, ancestry.clone()));
        ancestry
    }
}

/// `pid` followed by its parent chain, stopping at pid 1/0 or a cycle.
pub fn process_ancestry(pid: u32, inspector: &dyn ProcessInspector) -> Vec<u32> {
    let mut chain = vec![pid];
    let mut current = pid;
    while chain.len() < MAX_ANCESTRY_DEPTH {
        let Some(facts) = inspector.facts(current) else {
            break;
        };
        if facts.ppid <= 1 || chain.contains(&facts.ppid) {
            break;
        }
        chain.push(facts.ppid);
        current = facts.ppid;
    }
    chain
}

/// The pane whose process started the record's process. The record's own
/// `tmux` pane is trusted only when the ancestry confirms it, because pane
/// ids are per tmux server and a record may describe another server's pane.
fn match_record_pane<'a>(
    record: &ClaudeRegistryRecord,
    panes: &'a [MuxPane],
    ancestry: &[u32],
) -> Option<&'a MuxPane> {
    let contains_pane_process = |pane: &MuxPane| pane.pid > 0 && ancestry.contains(&pane.pid);
    if let Some(target) = &record.tmux
        && let Some(pane) = panes
            .iter()
            .find(|pane| pane.pane_id == target.pane_id && contains_pane_process(pane))
    {
        return Some(pane);
    }
    panes.iter().find(|pane| contains_pane_process(pane))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record_json(status: &str, extra: &str) -> String {
        format!(
            r#"{{"pid":4242,"sessionId":"f68da871-58bc-4b91-919f-c10c5228c827","cwd":"/home/me/code/app","startedAt":1789315528128,"procStart":"19734874","version":"2.1.270","kind":"interactive","entrypoint":"cli","tmux":"work:@16.%435","name":"app","nameSource":"user","status":"{status}","updatedAt":1789315554749,"statusUpdatedAt":1789315554749{extra}}}"#
        )
    }

    fn pane(pane_id: &str, session: &str, pid: u32) -> MuxPane {
        MuxPane {
            pane_id: pane_id.to_string(),
            session_name: session.to_string(),
            window_id: "@16".to_string(),
            window_active: true,
            active: true,
            pid,
            command: "claude".to_string(),
            title: "✳ app".to_string(),
            dead: false,
        }
    }

    struct TableInspector(HashMap<u32, ProcessFacts>);

    impl TableInspector {
        fn new(rows: &[(u32, u32, &str)]) -> Self {
            Self(
                rows.iter()
                    .map(|(pid, ppid, token)| {
                        (
                            *pid,
                            ProcessFacts {
                                ppid: *ppid,
                                start_token: Some((*token).to_string()),
                            },
                        )
                    })
                    .collect(),
            )
        }
    }

    impl ProcessInspector for TableInspector {
        fn facts(&self, pid: u32) -> Option<ProcessFacts> {
            self.0.get(&pid).cloned()
        }
    }

    #[test]
    fn parses_an_interactive_record_with_its_pane_and_status() {
        let record = parse_registry_record(
            "4242.json",
            &record_json("busy", ""),
            Path::new("/home/me/.claude"),
        )
        .expect("record");

        assert_eq!(record.pid, 4242);
        assert_eq!(record.session_id, "f68da871-58bc-4b91-919f-c10c5228c827");
        assert_eq!(record.cwd.as_deref(), Some("/home/me/code/app"));
        assert_eq!(record.status, RegistryStatus::Busy);
        assert_eq!(record.waiting_for, None);
        assert_eq!(record.status_updated_at, Some(1789315554749));
        assert_eq!(record.started_at, Some(1789315528128));
        assert_eq!(
            record.tmux,
            Some(TmuxTarget {
                session_name: "work".to_string(),
                window_id: "@16".to_string(),
                pane_id: "%435".to_string(),
            })
        );
        assert_eq!(record.name.as_deref(), Some("app"));
        assert_eq!(record.name_source.as_deref(), Some("user"));
        assert_eq!(record.agent, None);
        assert_eq!(record.proc_start.as_deref(), Some("19734874"));
        assert_eq!(record.config_dir, PathBuf::from("/home/me/.claude"));
    }

    #[test]
    fn parses_waiting_shell_idle_and_teammate_records() {
        let waiting = parse_registry_record(
            "4242.json",
            &record_json("waiting", r#","waitingFor":"dialog open""#),
            Path::new("/c"),
        )
        .unwrap();
        assert_eq!(waiting.status, RegistryStatus::Waiting);
        assert_eq!(waiting.waiting_for.as_deref(), Some("dialog open"));

        let shell =
            parse_registry_record("4242.json", &record_json("shell", ""), Path::new("/c")).unwrap();
        assert_eq!(shell.status, RegistryStatus::Shell);

        let idle =
            parse_registry_record("4242.json", &record_json("idle", ""), Path::new("/c")).unwrap();
        assert_eq!(idle.status, RegistryStatus::Idle);

        let teammate = parse_registry_record(
            "4242.json",
            &record_json("busy", r#","agent":"researcher""#),
            Path::new("/c"),
        )
        .unwrap();
        assert_eq!(teammate.agent.as_deref(), Some("researcher"));
    }

    #[test]
    fn unknown_status_reads_as_idle_and_missing_tmux_is_none() {
        let raw = r#"{"pid":7,"sessionId":"s","kind":"interactive","status":"dreaming"}"#;
        let record = parse_registry_record("7.json", raw, Path::new("/c")).unwrap();
        assert_eq!(record.status, RegistryStatus::Idle);
        assert_eq!(record.tmux, None);
        assert_eq!(record.cwd, None);
    }

    #[test]
    fn skips_records_that_are_not_interactive_sessions() {
        let no_session = r#"{"pid":7,"kind":"interactive","status":"idle"}"#;
        assert_eq!(
            parse_registry_record("7.json", no_session, Path::new("/c")),
            None
        );

        let daemon = r#"{"pid":7,"sessionId":"s","kind":"daemon","status":"idle"}"#;
        assert_eq!(
            parse_registry_record("7.json", daemon, Path::new("/c")),
            None
        );

        let background = r#"{"pid":7,"sessionId":"s","kind":"bg","status":"busy"}"#;
        assert_eq!(
            parse_registry_record("7.json", background, Path::new("/c")),
            None
        );

        let pid_mismatch = r#"{"pid":8,"sessionId":"s","kind":"interactive","status":"idle"}"#;
        assert_eq!(
            parse_registry_record("7.json", pid_mismatch, Path::new("/c")),
            None
        );

        let valid = r#"{"pid":7,"sessionId":"s","status":"idle"}"#;
        assert_eq!(parse_registry_record("7.key", valid, Path::new("/c")), None);
        assert_eq!(
            parse_registry_record("latest.json", valid, Path::new("/c")),
            None
        );
        assert_eq!(
            parse_registry_record("7.json", "not json", Path::new("/c")),
            None
        );
        assert!(parse_registry_record("7.json", valid, Path::new("/c")).is_some());
    }

    #[test]
    fn tmux_target_parsing_splits_from_the_right() {
        assert_eq!(
            parse_tmux_target("my:odd.name:@3.%12"),
            Some(TmuxTarget {
                session_name: "my:odd.name".to_string(),
                window_id: "@3".to_string(),
                pane_id: "%12".to_string(),
            })
        );
        assert_eq!(parse_tmux_target("work:@3.12"), None);
        assert_eq!(parse_tmux_target("work:3.%12"), None);
        assert_eq!(parse_tmux_target(":@3.%12"), None);
        assert_eq!(parse_tmux_target("work"), None);
    }

    #[test]
    fn proc_stat_is_parsed_from_the_last_paren_even_with_spaces_in_comm() {
        let stat = "369933 (claude (x) y) S 368848 369933 368848 34824 369933 4194304 1 2 3 4 5 6 7 8 20 0 1 0 19734874 999 1 2 3";
        assert_eq!(
            parse_proc_stat(stat),
            Some(ProcessFacts {
                ppid: 368848,
                start_token: Some("19734874".to_string()),
            })
        );
        assert_eq!(parse_proc_stat("garbage"), None);
    }

    #[test]
    fn config_dirs_include_the_override_and_siblings_holding_claude_state() {
        let home =
            std::env::temp_dir().join(format!("opensessions-registry-dirs-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(home.join(".claude/projects")).unwrap();
        fs::create_dir_all(home.join(".claude-personal/sessions")).unwrap();
        fs::create_dir_all(home.join(".claude-empty")).unwrap();
        fs::write(home.join(".claude.json"), "{}").unwrap();
        fs::create_dir_all(home.join(".not-claude/projects")).unwrap();

        let dirs = claude_code_config_dirs_from(&home, Some(OsStr::new("~/.claude-work")));

        assert_eq!(
            dirs,
            vec![
                home.join(".claude"),
                home.join(".claude-work"),
                home.join(".claude-personal"),
            ]
        );
        assert_eq!(
            claude_code_config_dirs_from(&home, Some(home.join(".claude-personal").as_os_str())),
            vec![home.join(".claude"), home.join(".claude-personal")]
        );
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn configured_config_dirs_are_added_after_discovery_with_home_expanded() {
        let home = std::env::temp_dir().join(format!(
            "opensessions-registry-configured-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(home.join(".claude-personal/sessions")).unwrap();

        let dirs = claude_code_config_dirs_with(
            &home,
            None,
            &[
                "~/accounts/work-claude".to_string(),
                "/srv/claude-b".to_string(),
                "~/.claude-personal".to_string(),
                String::new(),
            ],
        );

        assert_eq!(
            dirs,
            vec![
                home.join(".claude"),
                home.join("accounts/work-claude"),
                PathBuf::from("/srv/claude-b"),
                home.join(".claude-personal"),
            ]
        );
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn scan_reads_every_json_record_under_each_sessions_dir() {
        let home =
            std::env::temp_dir().join(format!("opensessions-registry-scan-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        let sessions = home.join(".claude/sessions");
        fs::create_dir_all(&sessions).unwrap();
        fs::write(sessions.join("4242.json"), record_json("busy", "")).unwrap();
        fs::write(sessions.join("4242.abcdef.key"), "secret").unwrap();
        fs::write(sessions.join("notes.json"), "{}").unwrap();
        fs::write(sessions.join("77.json"), "{broken").unwrap();

        let records = scan_registry(&[home.join(".claude"), home.join(".missing")]);

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].pid, 4242);
        assert_eq!(records[0].config_dir, home.join(".claude"));
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn resolver_binds_a_live_record_to_the_pane_whose_shell_started_it() {
        let record =
            parse_registry_record("4242.json", &record_json("busy", ""), Path::new("/c")).unwrap();
        let panes = vec![pane("%435", "work", 4000), pane("%436", "work", 4100)];
        let inspector = TableInspector::new(&[(4242, 4000, "19734874"), (4000, 1, "1")]);

        let resolved = RegistryResolver::new().resolve(vec![record], &panes, &inspector);

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].liveness, RecordLiveness::Alive);
        assert_eq!(
            resolved[0].pane.as_ref().map(|pane| pane.pane_id.as_str()),
            Some("%435")
        );
    }

    #[test]
    fn resolver_rejects_a_pane_id_match_from_another_tmux_server() {
        // Same pane id exists here, but its process tree does not contain
        // the record's pid: the record describes a different tmux server.
        let record =
            parse_registry_record("4242.json", &record_json("busy", ""), Path::new("/c")).unwrap();
        let panes = vec![pane("%435", "work", 9000)];
        let inspector = TableInspector::new(&[(4242, 4000, "19734874"), (4000, 1, "1")]);

        let resolved = RegistryResolver::new().resolve(vec![record], &panes, &inspector);

        assert_eq!(resolved[0].liveness, RecordLiveness::Alive);
        assert_eq!(resolved[0].pane, None);
    }

    #[test]
    fn resolver_finds_the_pane_by_ancestry_when_the_record_has_no_or_stale_tmux_field() {
        let mut record =
            parse_registry_record("4242.json", &record_json("idle", ""), Path::new("/c")).unwrap();
        record.tmux = None;
        // claude (4242) <- nix-shell (4200) <- bash (4000, the pane process)
        let inspector =
            TableInspector::new(&[(4242, 4200, "19734874"), (4200, 4000, "5"), (4000, 1, "1")]);
        let panes = vec![pane("%1", "docs", 3000), pane("%2", "work", 4000)];

        let resolved = RegistryResolver::new().resolve(vec![record.clone()], &panes, &inspector);
        assert_eq!(
            resolved[0].pane.as_ref().map(|pane| pane.pane_id.as_str()),
            Some("%2")
        );

        // A renamed session leaves the recorded session name stale; the pane
        // id still matches.
        record.tmux = Some(TmuxTarget {
            session_name: "old-name".to_string(),
            window_id: "@1".to_string(),
            pane_id: "%2".to_string(),
        });
        let resolved = RegistryResolver::new().resolve(vec![record], &panes, &inspector);
        assert_eq!(
            resolved[0]
                .pane
                .as_ref()
                .map(|pane| pane.session_name.as_str()),
            Some("work")
        );
    }

    #[test]
    fn resolver_reports_dead_when_the_pid_is_gone_or_reused() {
        let record =
            parse_registry_record("4242.json", &record_json("busy", ""), Path::new("/c")).unwrap();
        let panes = vec![pane("%435", "work", 4000)];

        let gone = TableInspector::new(&[(4000, 1, "1")]);
        let resolved = RegistryResolver::new().resolve(vec![record.clone()], &panes, &gone);
        assert_eq!(resolved[0].liveness, RecordLiveness::Dead);
        assert_eq!(resolved[0].pane, None);

        let reused = TableInspector::new(&[(4242, 4000, "99999999"), (4000, 1, "1")]);
        let resolved = RegistryResolver::new().resolve(vec![record.clone()], &panes, &reused);
        assert_eq!(resolved[0].liveness, RecordLiveness::Dead);

        // Without a start token on either side the pid alone decides.
        let mut untokened = record;
        untokened.proc_start = None;
        let resolved = RegistryResolver::new().resolve(vec![untokened], &panes, &reused);
        assert_eq!(resolved[0].liveness, RecordLiveness::Alive);
    }

    #[test]
    fn resolver_caches_ancestry_per_pid_incarnation() {
        use std::sync::Mutex;
        struct CountingInspector {
            inner: TableInspector,
            calls: Mutex<u32>,
        }
        impl ProcessInspector for CountingInspector {
            fn facts(&self, pid: u32) -> Option<ProcessFacts> {
                *self.calls.lock().unwrap() += 1;
                self.inner.facts(pid)
            }
        }
        let record =
            parse_registry_record("4242.json", &record_json("busy", ""), Path::new("/c")).unwrap();
        let panes = vec![pane("%435", "work", 4000)];
        let inspector = CountingInspector {
            inner: TableInspector::new(&[(4242, 4000, "19734874"), (4000, 1, "1")]),
            calls: Mutex::new(0),
        };
        let mut resolver = RegistryResolver::new();

        resolver.resolve(vec![record.clone()], &panes, &inspector);
        let first_pass = *inspector.calls.lock().unwrap();
        resolver.resolve(vec![record], &panes, &inspector);
        let second_pass = *inspector.calls.lock().unwrap() - first_pass;

        assert!(first_pass >= 2, "first pass walks the ancestry");
        assert_eq!(second_pass, 1, "later passes only re-check liveness");
    }
}

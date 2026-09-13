use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::agent_parsers::{
    determine_amp_message_status, determine_codex_status, determine_opencode_status,
};
use crate::claude_registry::claude_code_config_dirs_from;
use crate::protocol::AgentStatus;
use crate::tracker::{LastEntry, TranscriptHint};
use crate::transcript_tail::TailState;

const THREAD_NAME_MAX: usize = 80;
const TOOL_USE_WAIT_MS: u64 = 3_000;
const STUCK_MS: u64 = 15_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentWatcherSnapshot {
    pub agent: &'static str,
    pub thread_id: Option<String>,
    pub thread_name: Option<String>,
    pub last_user_prompt: Option<String>,
    pub project_dir: Option<String>,
    pub status: AgentStatus,
    pub ts: u64,
}

pub fn amp_snapshot_from_thread_json(raw: &str, ts: u64) -> Option<AgentWatcherSnapshot> {
    let thread: Value = serde_json::from_str(raw).ok()?;
    let thread_id = thread
        .get("id")
        .and_then(Value::as_str)
        .map(ToString::to_string);
    let thread_name = thread
        .get("title")
        .and_then(Value::as_str)
        .filter(|title| !title.is_empty())
        .map(ToString::to_string);
    let project_dir = extract_amp_project_dir(&thread);
    let last_user_prompt = extract_amp_last_user_prompt(&thread);
    let status = thread
        .get("messages")
        .and_then(Value::as_array)
        .and_then(|messages| messages.last())
        .map(determine_amp_message_status)
        .unwrap_or(AgentStatus::Idle);

    Some(AgentWatcherSnapshot {
        agent: "amp",
        thread_id,
        thread_name,
        last_user_prompt,
        project_dir,
        status,
        ts,
    })
}

/// Whole-file convenience over `ClaudeTranscriptState`; the server keeps
/// per-file states in a `TailCache` and only feeds appended lines.
pub fn claude_code_snapshot_from_jsonl(
    thread_id: &str,
    project_dir: &str,
    raw: &str,
    mtime_ms: u64,
    now_ms: u64,
) -> Option<AgentWatcherSnapshot> {
    let mut state = ClaudeTranscriptState::default();
    for line in raw.lines().filter(|line| !line.trim().is_empty()) {
        state.apply_line(line);
    }
    state.snapshot(thread_id, project_dir, mtime_ms, now_ms)
}

/// Incremental parse state of one Claude Code transcript.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClaudeTranscriptState {
    pub session_id: Option<String>,
    /// Working directory recorded on the entries themselves; more reliable
    /// than decoding the project folder name.
    pub cwd: Option<String>,
    custom_title: Option<String>,
    first_prompt_name: Option<String>,
    /// Claude Code's own `last-prompt` record, when present.
    last_prompt_record: Option<String>,
    last_user_prompt: Option<String>,
    status: Option<AgentStatus>,
    last_entry: Option<LastEntry>,
    saw_entry: bool,
}

impl TailState for ClaudeTranscriptState {
    fn apply_line(&mut self, line: &str) {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            return;
        };
        self.apply_entry(&entry);
    }
}

impl ClaudeTranscriptState {
    fn apply_entry(&mut self, entry: &Value) {
        match entry.get("type").and_then(Value::as_str) {
            Some("custom-title") => {
                if let Some(title) = extract_claude_custom_title(entry) {
                    self.custom_title = Some(title);
                }
                return;
            }
            Some("last-prompt") => {
                if let Some(prompt) = entry
                    .get("lastPrompt")
                    .and_then(Value::as_str)
                    .and_then(normalize_prompt)
                {
                    self.last_prompt_record = Some(prompt);
                }
                return;
            }
            _ => {}
        }
        let Some(role) = entry.pointer("/message/role").and_then(Value::as_str) else {
            // Metadata records (mode, snapshots, system notes) say nothing
            // about the conversation's state.
            return;
        };
        self.saw_entry = true;
        if self.session_id.is_none() {
            self.session_id = entry
                .get("sessionId")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(ToString::to_string);
        }
        if self.cwd.is_none() {
            self.cwd = entry
                .get("cwd")
                .and_then(Value::as_str)
                .filter(|cwd| !cwd.is_empty())
                .map(ToString::to_string);
        }
        // Injected context (/context output, skill bodies, command output)
        // and compaction summaries are not prompts and carry no status (F040).
        if entry.get("isMeta") == Some(&Value::Bool(true))
            || entry.get("isCompactSummary") == Some(&Value::Bool(true))
        {
            return;
        }
        let content = entry.pointer("/message/content");
        match role {
            "user" => {
                let text = content_text(content);
                if text
                    .as_deref()
                    .is_some_and(|text| text.starts_with("[Request interrupted"))
                {
                    self.last_entry = Some(LastEntry::Interrupted);
                    self.status = Some(AgentStatus::Interrupted);
                } else if content_has_type(content, "tool_result") {
                    self.last_entry = Some(LastEntry::ToolResult);
                    self.status = Some(AgentStatus::Running);
                } else if let Some(text) = text {
                    if text.contains("<command-name>/exit</command-name>") {
                        self.status = Some(AgentStatus::Done);
                    } else if text.contains("<command-name>/")
                        || is_noise_user_text(&text)
                        || text.starts_with("[Request")
                    {
                        // Slash commands and their output are not prompts.
                    } else if let Some(prompt) = normalize_prompt(&text) {
                        if self.first_prompt_name.is_none() {
                            self.first_prompt_name = normalize_thread_name(&prompt);
                        }
                        self.last_user_prompt = Some(prompt);
                        self.last_entry = Some(LastEntry::UserPrompt);
                        self.status = Some(AgentStatus::Running);
                    }
                } else {
                    self.last_entry = Some(LastEntry::UserPrompt);
                    self.status = Some(AgentStatus::Running);
                }
            }
            "assistant" => {
                if entry.get("isApiErrorMessage") == Some(&Value::Bool(true)) {
                    self.last_entry = Some(LastEntry::ApiError);
                    self.status = Some(AgentStatus::Error);
                } else if content_has_type(content, "tool_use") {
                    self.last_entry = Some(LastEntry::AssistantToolUse);
                    self.status = Some(AgentStatus::Running);
                } else {
                    match entry
                        .pointer("/message/stop_reason")
                        .and_then(Value::as_str)
                    {
                        None | Some("tool_use") => {
                            self.last_entry = Some(LastEntry::Other);
                            self.status = Some(AgentStatus::Running);
                        }
                        Some(_) => {
                            self.last_entry = Some(LastEntry::AssistantEndTurn);
                            self.status = Some(AgentStatus::Done);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    pub fn thread_name(&self) -> Option<String> {
        self.custom_title
            .clone()
            .or_else(|| self.first_prompt_name.clone())
    }

    pub fn last_user_prompt(&self) -> Option<String> {
        self.last_prompt_record
            .clone()
            .or_else(|| self.last_user_prompt.clone())
    }

    /// What a registry-sourced row can learn from this transcript.
    pub fn hint(&self) -> TranscriptHint {
        TranscriptHint {
            thread_name: self.thread_name(),
            last_user_prompt: self.last_user_prompt(),
            last_entry: self.last_entry.unwrap_or(LastEntry::Other),
        }
    }

    /// Fallback snapshot for sessions without a registry record: status is
    /// inferred from the last entry plus silence timers.
    pub fn snapshot(
        &self,
        thread_id: &str,
        project_dir_fallback: &str,
        mtime_ms: u64,
        now_ms: u64,
    ) -> Option<AgentWatcherSnapshot> {
        if !self.saw_entry {
            return None;
        }
        let mut status = self.status.unwrap_or(AgentStatus::Idle);
        let idle_for = now_ms.saturating_sub(mtime_ms);
        if status == AgentStatus::Running
            && self.last_entry == Some(LastEntry::AssistantToolUse)
            && idle_for >= TOOL_USE_WAIT_MS
        {
            status = AgentStatus::Waiting;
        }
        if matches!(status, AgentStatus::Running | AgentStatus::Waiting) && idle_for >= STUCK_MS {
            status = AgentStatus::Stale;
        }
        Some(AgentWatcherSnapshot {
            agent: "claude-code",
            thread_id: Some(thread_id.to_string()),
            thread_name: self.thread_name(),
            last_user_prompt: self.last_user_prompt(),
            project_dir: Some(
                self.cwd
                    .clone()
                    .unwrap_or_else(|| project_dir_fallback.to_string()),
            ),
            status,
            ts: mtime_ms,
        })
    }
}

/// Whole-file convenience over `CodexTranscriptState`.
pub fn codex_snapshot_from_jsonl(
    thread_id: &str,
    raw: &str,
    indexed_thread_name: Option<&str>,
    mtime_ms: u64,
    now_ms: u64,
) -> Option<AgentWatcherSnapshot> {
    let mut state = CodexTranscriptState::default();
    for line in raw.lines().filter(|line| !line.trim().is_empty()) {
        state.apply_line(line);
    }
    state.snapshot(thread_id, indexed_thread_name, mtime_ms, now_ms)
}

/// Incremental parse state of one Codex rollout.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CodexTranscriptState {
    project_dir: Option<String>,
    thread_name: Option<String>,
    last_user_prompt: Option<String>,
    status: Option<AgentStatus>,
    last_entry_is_tool_call: bool,
    /// Rollouts of Codex's own subagents are not conversations of their own.
    subagent: bool,
    saw_entry: bool,
}

impl TailState for CodexTranscriptState {
    fn apply_line(&mut self, line: &str) {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            return;
        };
        self.saw_entry = true;
        if entry.get("type").and_then(Value::as_str) == Some("session_meta")
            && is_codex_subagent_session_meta(&entry)
        {
            self.subagent = true;
        }
        if self.project_dir.is_none() {
            self.project_dir = extract_codex_project_dir(&entry);
        }
        if let Some(prompt) = extract_codex_user_prompt(&entry) {
            if self.thread_name.is_none() {
                self.thread_name = normalize_thread_name(&prompt);
            }
            self.last_user_prompt = Some(prompt);
        }
        if let Some(next_status) = determine_codex_status(&entry) {
            self.status = Some(next_status);
            self.last_entry_is_tool_call = is_codex_tool_call_entry(&entry);
        }
    }
}

impl CodexTranscriptState {
    pub fn snapshot(
        &self,
        thread_id: &str,
        indexed_thread_name: Option<&str>,
        mtime_ms: u64,
        now_ms: u64,
    ) -> Option<AgentWatcherSnapshot> {
        if !self.saw_entry || self.subagent {
            return None;
        }
        let mut status = self.status.unwrap_or(AgentStatus::Idle);
        let idle_for = now_ms.saturating_sub(mtime_ms);
        if status == AgentStatus::Running
            && self.last_entry_is_tool_call
            && idle_for >= TOOL_USE_WAIT_MS
        {
            status = AgentStatus::Waiting;
        }
        if matches!(status, AgentStatus::Running | AgentStatus::Waiting) && idle_for >= STUCK_MS {
            status = AgentStatus::Stale;
        }
        Some(AgentWatcherSnapshot {
            agent: "codex",
            thread_id: Some(thread_id.to_string()),
            thread_name: indexed_thread_name
                .map(ToString::to_string)
                .or_else(|| self.thread_name.clone()),
            last_user_prompt: self.last_user_prompt.clone(),
            project_dir: self.project_dir.clone(),
            status,
            ts: mtime_ms,
        })
    }
}

fn is_codex_subagent_session_meta(entry: &Value) -> bool {
    entry.pointer("/payload/source/subagent").is_some()
        || matches!(
            entry
                .pointer("/payload/thread_source")
                .and_then(Value::as_str),
            Some("subagent" | "subagent_review" | "subagent_thread_spawn")
        )
}

pub fn opencode_snapshot_from_row(
    session_id: &str,
    title: Option<&str>,
    directory: &str,
    time_updated: u64,
    last_message_json: &str,
    last_user_prompt_json: Option<&str>,
    now_ms: u64,
) -> Option<AgentWatcherSnapshot> {
    let message = serde_json::from_str::<Value>(last_message_json).ok()?;
    let mut status = determine_opencode_status(&message);
    if status == AgentStatus::Running && now_ms.saturating_sub(time_updated) >= STUCK_MS {
        status = AgentStatus::Stale;
    }
    let last_user_prompt = last_user_prompt_json.and_then(extract_opencode_user_prompt_json);

    Some(AgentWatcherSnapshot {
        agent: "opencode",
        thread_id: Some(session_id.to_string()),
        thread_name: title
            .filter(|title| !title.is_empty())
            .map(ToString::to_string),
        last_user_prompt,
        project_dir: (!directory.is_empty()).then(|| directory.to_string()),
        status,
        ts: now_ms,
    })
}

pub fn pi_snapshot_from_jsonl(
    thread_id: &str,
    raw: &str,
    mtime_ms: u64,
    now_ms: u64,
) -> Option<AgentWatcherSnapshot> {
    let mut session_id = Some(thread_id.to_string());
    let mut project_dir = None;
    let mut thread_name = None;
    let mut last_user_prompt = None;
    let mut status = AgentStatus::Idle;
    let mut saw_entry = false;

    for line in raw.lines().filter(|line| !line.trim().is_empty()) {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        saw_entry = true;

        if entry.get("type").and_then(Value::as_str) == Some("session") {
            if let Some(id) = entry.get("id").and_then(Value::as_str) {
                session_id = Some(id.to_string());
            }
            if project_dir.is_none() {
                project_dir = entry
                    .get("cwd")
                    .and_then(Value::as_str)
                    .map(ToString::to_string);
            }
            continue;
        }

        if let Some(name) = extract_pi_session_name(&entry) {
            thread_name = Some(name);
        }
        if let Some(prompt) = extract_pi_user_prompt(&entry) {
            if thread_name.is_none() {
                thread_name = normalize_thread_name(&prompt);
            }
            last_user_prompt = Some(prompt);
        }
        let next_status = crate::agent_parsers::determine_pi_status(&entry);
        if next_status != AgentStatus::Idle {
            status = next_status;
        }
    }

    if !saw_entry {
        return None;
    }

    let idle_for = now_ms.saturating_sub(mtime_ms);
    if matches!(status, AgentStatus::Running | AgentStatus::Waiting) && idle_for >= STUCK_MS {
        status = AgentStatus::Stale;
    }

    Some(AgentWatcherSnapshot {
        agent: "pi",
        thread_id: session_id,
        thread_name,
        last_user_prompt,
        project_dir,
        status,
        ts: mtime_ms,
    })
}

pub fn droid_snapshot_from_jsonl(
    thread_id: &str,
    raw: &str,
    mtime_ms: u64,
    now_ms: u64,
) -> Option<AgentWatcherSnapshot> {
    let mut session_id = Some(thread_id.to_string());
    let mut project_dir = None;
    let mut thread_name = None;
    let mut last_user_prompt = None;
    let mut status = AgentStatus::Idle;
    let mut saw_entry = false;

    for line in raw.lines().filter(|line| !line.trim().is_empty()) {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        saw_entry = true;

        if let Some(id) = entry
            .get("session_id")
            .or_else(|| entry.get("sessionId"))
            .or_else(|| entry.get("id"))
            .and_then(Value::as_str)
        {
            session_id = Some(id.to_string());
        }
        if project_dir.is_none() {
            project_dir = entry
                .get("cwd")
                .or_else(|| entry.get("directory"))
                .and_then(Value::as_str)
                .map(ToString::to_string);
        }
        if thread_name.is_none() {
            thread_name = entry
                .get("title")
                .or_else(|| entry.get("name"))
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .map(ToString::to_string);
        }

        if let Some(prompt) = extract_droid_user_prompt(&entry) {
            if thread_name.is_none() {
                thread_name = normalize_thread_name(&prompt);
            }
            last_user_prompt = Some(prompt);
        }
        if let Some(next_status) = determine_droid_status(&entry) {
            status = next_status;
        }
    }

    if !saw_entry {
        return None;
    }

    let idle_for = now_ms.saturating_sub(mtime_ms);
    if matches!(status, AgentStatus::Running | AgentStatus::Waiting) && idle_for >= STUCK_MS {
        status = AgentStatus::Stale;
    }

    Some(AgentWatcherSnapshot {
        agent: "droid",
        thread_id: session_id,
        thread_name,
        last_user_prompt,
        project_dir,
        status,
        ts: mtime_ms,
    })
}

pub fn codex_thread_id_from_path(path: &str) -> String {
    let name = path
        .rsplit_once('/')
        .map(|(_, name)| name)
        .unwrap_or(path)
        .strip_suffix(".jsonl")
        .unwrap_or_else(|| path.rsplit_once('/').map(|(_, name)| name).unwrap_or(path));

    find_uuid_suffix(name).unwrap_or(name).to_string()
}

/// Every `projects/` directory Claude Code may write transcripts into: one
/// per config directory (see `claude_registry::claude_code_config_dirs`).
pub fn claude_code_projects_dirs(home: &Path) -> Vec<PathBuf> {
    claude_code_projects_dirs_from(home, std::env::var_os("CLAUDE_CONFIG_DIR").as_deref())
}

fn claude_code_projects_dirs_from(home: &Path, config_dir: Option<&OsStr>) -> Vec<PathBuf> {
    claude_code_config_dirs_from(home, config_dir)
        .into_iter()
        .map(|dir| dir.join("projects"))
        .collect()
}

pub fn decode_claude_project_dir(encoded: &str, exists: impl Fn(&str) -> bool) -> String {
    let naive = encoded.replace('-', "/");
    if exists(&naive) {
        naive
    } else {
        format!("__encoded__:{encoded}")
    }
}

pub fn parse_codex_session_index(raw: &str) -> Vec<(String, String)> {
    raw.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|entry| {
            let id = entry.get("id")?.as_str()?;
            let name = entry.get("thread_name")?.as_str()?;
            Some((id.to_string(), name.to_string()))
        })
        .collect()
}

fn extract_amp_project_dir(thread: &Value) -> Option<String> {
    let uri = thread
        .pointer("/env/initial/trees/0/uri")
        .and_then(Value::as_str)?;
    uri.strip_prefix("file://").map(ToString::to_string)
}

fn extract_amp_last_user_prompt(thread: &Value) -> Option<String> {
    thread
        .get("messages")
        .and_then(Value::as_array)?
        .iter()
        .rev()
        .find_map(|message| {
            (message.get("role").and_then(Value::as_str) == Some("user"))
                .then(|| content_text(message.get("content")))?
        })
        .and_then(|prompt| normalize_prompt(&prompt))
}

fn extract_claude_custom_title(entry: &Value) -> Option<String> {
    if entry.get("type").and_then(Value::as_str) != Some("custom-title") {
        return None;
    }
    entry
        .get("customTitle")
        .and_then(Value::as_str)
        .filter(|title| !title.is_empty())
        .map(ToString::to_string)
}

fn is_noise_user_text(text: &str) -> bool {
    text.starts_with('<') || text.starts_with('{')
}

fn extract_codex_project_dir(entry: &Value) -> Option<String> {
    match entry.get("type").and_then(Value::as_str) {
        Some("session_meta" | "turn_context") => entry
            .pointer("/payload/cwd")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        _ => None,
    }
}

fn extract_codex_user_prompt(entry: &Value) -> Option<String> {
    if entry.get("type").and_then(Value::as_str) == Some("event_msg")
        && entry.pointer("/payload/type").and_then(Value::as_str) == Some("user_message")
    {
        let message = entry.pointer("/payload/message").and_then(Value::as_str)?;
        return normalize_codex_user_prompt(message);
    }

    if entry.get("type").and_then(Value::as_str) == Some("response_item")
        && entry.pointer("/payload/type").and_then(Value::as_str) == Some("message")
        && entry.pointer("/payload/role").and_then(Value::as_str) == Some("user")
    {
        let text = entry
            .pointer("/payload/content")
            .and_then(Value::as_array)?
            .iter()
            .filter(|item| item.get("type").and_then(Value::as_str) == Some("input_text"))
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
        return normalize_codex_user_prompt(&text);
    }

    if entry.get("type").and_then(Value::as_str) == Some("message")
        && entry.get("role").and_then(Value::as_str) == Some("user")
    {
        let text = entry
            .get("content")
            .and_then(Value::as_array)?
            .iter()
            .filter(|item| item.get("type").and_then(Value::as_str) == Some("input_text"))
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
        return normalize_codex_user_prompt(&text);
    }

    None
}

/// The user's own words out of a Codex user message: everything after the
/// last `## My request for Codex:` marker (the IDE prepends context blocks
/// before it), rejecting messages that are only injected context (F041).
fn normalize_codex_user_prompt(text: &str) -> Option<String> {
    const REQUEST_MARKER: &str = "## My request for Codex:";
    let prompt = match text.rfind(REQUEST_MARKER) {
        Some(index) => &text[index + REQUEST_MARKER.len()..],
        None => text,
    };
    let candidate = normalize_prompt(prompt)?;
    (!is_codex_internal_prompt(&candidate)).then_some(candidate)
}

fn extract_opencode_user_prompt_json(raw: &str) -> Option<String> {
    let value = serde_json::from_str::<Value>(raw).ok()?;
    let text = value
        .get("text")
        .or_else(|| value.pointer("/prompt/text"))
        .or_else(|| value.pointer("/data/text"))
        .and_then(Value::as_str)?;
    normalize_prompt(text)
}

fn extract_pi_session_name(entry: &Value) -> Option<String> {
    (entry.get("type").and_then(Value::as_str) == Some("session_info"))
        .then(|| entry.get("name")?.as_str())?
        .filter(|name| !name.is_empty())
        .map(ToString::to_string)
}

fn extract_pi_user_prompt(entry: &Value) -> Option<String> {
    if entry.get("type").and_then(Value::as_str) != Some("message")
        || entry.pointer("/message/role").and_then(Value::as_str) != Some("user")
    {
        return None;
    }
    normalize_prompt(&message_content_text(entry.pointer("/message/content")?)?)
}

fn extract_droid_user_prompt(entry: &Value) -> Option<String> {
    if entry.get("hook_event_name").and_then(Value::as_str) == Some("UserPromptSubmit") {
        return entry
            .get("prompt")
            .and_then(Value::as_str)
            .and_then(normalize_prompt);
    }

    if entry.get("role").and_then(Value::as_str) == Some("user") {
        return entry
            .get("content")
            .or_else(|| entry.get("message"))
            .and_then(message_content_text)
            .and_then(|text| normalize_prompt(&text));
    }

    if entry.pointer("/message/role").and_then(Value::as_str) == Some("user") {
        return entry
            .pointer("/message/content")
            .and_then(message_content_text)
            .and_then(|text| normalize_prompt(&text));
    }

    None
}

fn determine_droid_status(entry: &Value) -> Option<AgentStatus> {
    match entry.get("hook_event_name").and_then(Value::as_str) {
        Some("UserPromptSubmit") => return Some(AgentStatus::Running),
        Some("Stop" | "SessionEnd") => return Some(AgentStatus::Done),
        Some("Notification") => return Some(AgentStatus::Waiting),
        _ => {}
    }

    match entry.get("role").and_then(Value::as_str) {
        Some("user") => Some(AgentStatus::Running),
        Some("assistant") => Some(AgentStatus::Done),
        _ => match entry.pointer("/message/role").and_then(Value::as_str) {
            Some("user") => Some(AgentStatus::Running),
            Some("assistant") => Some(AgentStatus::Done),
            _ => None,
        },
    }
}

fn is_codex_tool_call_entry(entry: &Value) -> bool {
    matches!(
        entry.get("type").and_then(Value::as_str),
        Some("function_call")
    ) || entry.get("type").and_then(Value::as_str) == Some("response_item")
        && entry.pointer("/payload/type").and_then(Value::as_str) == Some("function_call")
}

fn is_codex_internal_prompt(candidate: &str) -> bool {
    candidate.starts_with('<')
        || candidate.starts_with('{')
        || candidate.starts_with("# AGENTS.md")
        || candidate.starts_with("# Context from my IDE")
        || candidate.starts_with("# Files mentioned by the user")
}

fn normalize_thread_name(text: &str) -> Option<String> {
    let line = first_non_empty_line(text)?;
    Some(line.chars().take(THREAD_NAME_MAX).collect())
}

fn normalize_prompt(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_string())
}

fn first_non_empty_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).find(|line| !line.is_empty())
}

fn content_text(content: Option<&Value>) -> Option<String> {
    match content? {
        Value::String(text) => Some(text.clone()),
        Value::Array(items) => items
            .iter()
            .find(|item| {
                item.get("type").and_then(Value::as_str) == Some("text")
                    && item.get("text").is_some()
            })
            .and_then(|item| item.get("text").and_then(Value::as_str))
            .map(ToString::to_string),
        _ => None,
    }
}

fn message_content_text(content: &Value) -> Option<String> {
    match content {
        Value::String(text) => Some(text.clone()),
        Value::Array(items) => {
            let text = items
                .iter()
                .filter(|item| item.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|item| item.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n");
            (!text.trim().is_empty()).then_some(text)
        }
        _ => None,
    }
}

fn content_has_type(content: Option<&Value>, target_type: &str) -> bool {
    content.and_then(Value::as_array).is_some_and(|items| {
        items
            .iter()
            .any(|item| item.get("type").and_then(Value::as_str) == Some(target_type))
    })
}

fn find_uuid_suffix(name: &str) -> Option<&str> {
    let bytes = name.as_bytes();
    let len = bytes.len();
    if len < 36 {
        return None;
    }
    for start in (0..=len - 36).rev() {
        // Slice only on char boundaries: a non-ASCII file name must not
        // panic the scanner (a UUID is ASCII, so such slices cannot match).
        let Some(candidate) = name.get(start..start + 36) else {
            continue;
        };
        if is_uuid(candidate) {
            return Some(candidate);
        }
    }
    None
}

fn is_uuid(candidate: &str) -> bool {
    candidate.char_indices().all(|(idx, ch)| match idx {
        8 | 13 | 18 | 23 => ch == '-',
        _ => ch.is_ascii_hexdigit(),
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn scratch_home(name: &str) -> PathBuf {
        let home = std::env::temp_dir().join(format!(
            "opensessions-claude-dirs-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).expect("create scratch home");
        home
    }

    #[test]
    fn uuid_suffix_search_tolerates_non_ascii_file_names() {
        let name = "rollout-2026-09-12T10-00-00-会話ノート記録テスト用の長い名前-01234567-89ab-cdef-0123-456789abcdef";
        assert_eq!(
            find_uuid_suffix(name),
            Some("01234567-89ab-cdef-0123-456789abcdef")
        );
        assert_eq!(
            find_uuid_suffix("会話ノート会話ノート会話ノート会話ノート会話ノート会話ノート"),
            None
        );
    }

    #[test]
    fn claude_projects_dirs_include_the_config_dir_override_and_siblings() {
        let home = scratch_home("siblings");
        fs::create_dir_all(home.join(".claude/projects")).unwrap();
        fs::create_dir_all(home.join(".claude-personal/projects")).unwrap();
        fs::create_dir_all(home.join(".claude-empty")).unwrap();
        fs::write(home.join(".claude.json"), "{}").unwrap();
        fs::create_dir_all(home.join(".not-claude/projects")).unwrap();

        let dirs = claude_code_projects_dirs_from(&home, Some(OsStr::new("~/.claude-work")));

        assert_eq!(
            dirs,
            vec![
                home.join(".claude/projects"),
                home.join(".claude-work/projects"),
                home.join(".claude-personal/projects"),
            ]
        );
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn claude_projects_dirs_deduplicate_override_that_is_also_a_sibling() {
        let home = scratch_home("dedupe");
        fs::create_dir_all(home.join(".claude-personal/projects")).unwrap();

        let dirs =
            claude_code_projects_dirs_from(&home, Some(home.join(".claude-personal").as_os_str()));

        assert_eq!(
            dirs,
            vec![
                home.join(".claude/projects"),
                home.join(".claude-personal/projects"),
            ]
        );
        assert_eq!(
            claude_code_projects_dirs_from(&home, Some(OsStr::new(""))),
            vec![
                home.join(".claude/projects"),
                home.join(".claude-personal/projects"),
            ]
        );
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn claude_code_snapshot_tracks_latest_real_user_prompt() {
        let raw = r#"
{"type":"user","message":{"role":"user","content":[{"type":"text","text":"Implement auth"}]}}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Done"}],"stop_reason":"end_turn"}}
{"type":"user","message":{"role":"user","content":[{"type":"text","text":"Add tests too"}]}}
"#;
        let snapshot = claude_code_snapshot_from_jsonl("thread", "/repo", raw, 1_000, 1_100)
            .expect("snapshot");
        assert_eq!(snapshot.thread_name.as_deref(), Some("Implement auth"));
        assert_eq!(snapshot.last_user_prompt.as_deref(), Some("Add tests too"));
        assert_eq!(snapshot.status, AgentStatus::Running);
    }

    #[test]
    fn claude_state_skips_meta_entries_prefers_last_prompt_records_and_reads_cwd() {
        let raw = r#"
{"type":"user","cwd":"/repo/sub","sessionId":"s1","isMeta":true,"message":{"role":"user","content":"<local-command-caveat>noise"}}
{"type":"user","cwd":"/repo/sub","sessionId":"s1","message":{"role":"user","content":"Implement auth"}}
{"type":"last-prompt","sessionId":"s1","lastPrompt":"Implement auth"}
{"type":"assistant","cwd":"/repo/sub","message":{"role":"assistant","content":[{"type":"tool_use","name":"Bash"}],"stop_reason":"tool_use"}}
{"type":"user","cwd":"/repo/sub","message":{"role":"user","content":[{"type":"tool_result","content":"ok"}]}}
{"type":"user","cwd":"/repo/sub","isCompactSummary":true,"message":{"role":"user","content":"This session is being continued from a previous conversation"}}
{"type":"custom-title","sessionId":"s1","customTitle":"auth work"}
{"type":"assistant","cwd":"/repo/sub","message":{"role":"assistant","content":[{"type":"text","text":"Done"}],"stop_reason":"end_turn"}}
"#;
        let mut state = ClaudeTranscriptState::default();
        for line in raw.lines().filter(|line| !line.trim().is_empty()) {
            state.apply_line(line);
        }

        assert_eq!(state.cwd.as_deref(), Some("/repo/sub"));
        assert_eq!(state.session_id.as_deref(), Some("s1"));
        let hint = state.hint();
        assert_eq!(hint.thread_name.as_deref(), Some("auth work"));
        assert_eq!(hint.last_user_prompt.as_deref(), Some("Implement auth"));
        assert_eq!(hint.last_entry, LastEntry::AssistantEndTurn);

        let snapshot = state
            .snapshot("s1", "/decoded/fallback", 1_000, 1_100)
            .expect("snapshot");
        assert_eq!(snapshot.project_dir.as_deref(), Some("/repo/sub"));
        assert_eq!(snapshot.thread_name.as_deref(), Some("auth work"));
        assert_eq!(snapshot.status, AgentStatus::Done);
    }

    #[test]
    fn claude_state_classifies_the_last_entry_for_registry_refinement() {
        let mut state = ClaudeTranscriptState::default();
        let apply = |state: &mut ClaudeTranscriptState, line: &str| state.apply_line(line);

        apply(
            &mut state,
            r#"{"type":"user","message":{"role":"user","content":"Fix it"}}"#,
        );
        assert_eq!(state.hint().last_entry, LastEntry::UserPrompt);

        apply(
            &mut state,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"hmm"}]}}"#,
        );
        assert_eq!(state.hint().last_entry, LastEntry::Other);

        apply(
            &mut state,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Agent"}],"stop_reason":"tool_use"}}"#,
        );
        assert_eq!(state.hint().last_entry, LastEntry::AssistantToolUse);

        apply(
            &mut state,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"done"}]}}"#,
        );
        assert_eq!(state.hint().last_entry, LastEntry::ToolResult);

        // Interleaved metadata does not disturb the classification.
        apply(
            &mut state,
            r#"{"type":"file-history-snapshot","messageId":"m"}"#,
        );
        apply(
            &mut state,
            r#"{"type":"system","subtype":"turn_duration","durationMs":5}"#,
        );
        assert_eq!(state.hint().last_entry, LastEntry::ToolResult);

        apply(
            &mut state,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]}}"#,
        );
        assert_eq!(state.hint().last_entry, LastEntry::Interrupted);
        assert_eq!(
            state.snapshot("s", "/r", 1, 2).unwrap().status,
            AgentStatus::Interrupted
        );

        apply(
            &mut state,
            r#"{"type":"assistant","isApiErrorMessage":true,"message":{"role":"assistant","content":[{"type":"text","text":"API Error: overloaded"}]}}"#,
        );
        assert_eq!(state.hint().last_entry, LastEntry::ApiError);
        assert_eq!(
            state.snapshot("s", "/r", 1, 2).unwrap().status,
            AgentStatus::Error
        );

        apply(
            &mut state,
            r#"{"type":"user","message":{"role":"user","content":"<command-name>/exit</command-name>"}}"#,
        );
        assert_eq!(
            state.snapshot("s", "/r", 1, 2).unwrap().status,
            AgentStatus::Done
        );
    }

    #[test]
    fn claude_state_promotes_silence_to_waiting_and_stale_only_for_the_fallback_snapshot() {
        let mut state = ClaudeTranscriptState::default();
        state.apply_line(r#"{"type":"user","message":{"role":"user","content":"go"}}"#);
        state.apply_line(r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Bash"}]}}"#);

        assert_eq!(
            state.snapshot("s", "/r", 1_000, 2_000).unwrap().status,
            AgentStatus::Running
        );
        assert_eq!(
            state.snapshot("s", "/r", 1_000, 5_000).unwrap().status,
            AgentStatus::Waiting
        );
        assert_eq!(
            state.snapshot("s", "/r", 1_000, 20_000).unwrap().status,
            AgentStatus::Stale
        );
        assert_eq!(state.hint().last_entry, LastEntry::AssistantToolUse);
    }

    #[test]
    fn codex_state_rejects_injected_context_and_subagent_rollouts() {
        let raw = r###"
{"type":"session_meta","payload":{"id":"abc","cwd":"/repo"}}
{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"# Context from my IDE setup:\nlots of stuff"}]}}
{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<recommended_plugins>x</recommended_plugins>"}]}}
{"type":"event_msg","payload":{"type":"user_message","message":"<environment_context>\n</environment_context>\n## My request for Codex:\n\nFix the flaky watcher"}}
{"type":"event_msg","payload":{"type":"task_complete"}}
"###;
        let mut state = CodexTranscriptState::default();
        for line in raw.lines().filter(|line| !line.trim().is_empty()) {
            state.apply_line(line);
        }
        let snapshot = state.snapshot("abc", None, 1_000, 1_100).expect("snapshot");
        assert_eq!(
            snapshot.thread_name.as_deref(),
            Some("Fix the flaky watcher")
        );
        assert_eq!(
            snapshot.last_user_prompt.as_deref(),
            Some("Fix the flaky watcher")
        );
        assert_eq!(snapshot.project_dir.as_deref(), Some("/repo"));
        assert_eq!(snapshot.status, AgentStatus::Done);

        let mut subagent = CodexTranscriptState::default();
        subagent.apply_line(r#"{"type":"session_meta","payload":{"id":"sub","cwd":"/repo","source":{"subagent":{"thread_spawn":{}}}}}"#);
        subagent.apply_line(r#"{"type":"event_msg","payload":{"type":"task_started"}}"#);
        assert!(subagent.snapshot("sub", None, 1_000, 1_100).is_none());
    }

    #[test]
    fn codex_snapshot_strips_request_prefix_and_ignores_internal_prompts() {
        let raw = r###"
{"type":"session_meta","payload":{"id":"abc","cwd":"/repo"}}
{"type":"event_msg","payload":{"type":"user_message","message":"<codex reminder>ignore"}}
{"type":"event_msg","payload":{"type":"user_message","message":"## My request for Codex:\nFix the flaky watcher"}}
{"type":"event_msg","payload":{"type":"task_complete"}}
"###;
        let snapshot = codex_snapshot_from_jsonl("abc", raw, None, 1_000, 1_100).expect("snapshot");
        assert_eq!(
            snapshot.thread_name.as_deref(),
            Some("Fix the flaky watcher")
        );
        assert_eq!(
            snapshot.last_user_prompt.as_deref(),
            Some("Fix the flaky watcher")
        );
        assert_eq!(snapshot.status, AgentStatus::Done);
    }

    #[test]
    fn pi_snapshot_reads_session_header_name_and_latest_user_prompt() {
        let raw = r#"
{"type":"session","version":3,"id":"pi-session","cwd":"/repo"}
{"type":"session_info","id":"n","parentId":null,"name":"Nice title"}
{"type":"message","id":"u1","parentId":null,"message":{"role":"user","content":[{"type":"text","text":"First task"}]}}
{"type":"message","id":"a1","parentId":"u1","message":{"role":"assistant","stopReason":"stop","content":[{"type":"text","text":"Done"}]}}
{"type":"message","id":"u2","parentId":"a1","message":{"role":"user","content":[{"type":"text","text":"Follow up"}]}}
"#;
        let snapshot = pi_snapshot_from_jsonl("file", raw, 1_000, 1_100).expect("snapshot");
        assert_eq!(snapshot.thread_id.as_deref(), Some("pi-session"));
        assert_eq!(snapshot.project_dir.as_deref(), Some("/repo"));
        assert_eq!(snapshot.thread_name.as_deref(), Some("Nice title"));
        assert_eq!(snapshot.last_user_prompt.as_deref(), Some("Follow up"));
        assert_eq!(snapshot.status, AgentStatus::Running);
    }

    #[test]
    fn droid_snapshot_reads_hook_prompt_and_stop_status() {
        let raw = r#"
{"session_id":"droid-session","transcript_path":"/tmp/session.jsonl","cwd":"/repo","hook_event_name":"SessionStart","source":"startup"}
{"session_id":"droid-session","cwd":"/repo","hook_event_name":"UserPromptSubmit","prompt":"Write a migration"}
{"session_id":"droid-session","cwd":"/repo","hook_event_name":"Stop","stop_hook_active":false}
"#;
        let snapshot = droid_snapshot_from_jsonl("file", raw, 1_000, 1_100).expect("snapshot");
        assert_eq!(snapshot.agent, "droid");
        assert_eq!(snapshot.thread_id.as_deref(), Some("droid-session"));
        assert_eq!(snapshot.project_dir.as_deref(), Some("/repo"));
        assert_eq!(snapshot.thread_name.as_deref(), Some("Write a migration"));
        assert_eq!(
            snapshot.last_user_prompt.as_deref(),
            Some("Write a migration")
        );
        assert_eq!(snapshot.status, AgentStatus::Done);
    }

    #[test]
    fn opencode_snapshot_reads_v2_user_prompt_json() {
        let last_message = r#"{"role":"assistant","finish":"stop"}"#;
        let last_user = r#"{"text":"Ship this sidebar"}"#;
        let snapshot = opencode_snapshot_from_row(
            "ses_1",
            Some("Sidebar"),
            "/repo",
            1_000,
            last_message,
            Some(last_user),
            1_100,
        )
        .expect("snapshot");
        assert_eq!(
            snapshot.last_user_prompt.as_deref(),
            Some("Ship this sidebar")
        );
        assert_eq!(snapshot.status, AgentStatus::Done);
    }
}

use std::collections::{HashMap, HashSet};
use std::process::Command;
use std::sync::Arc;

use crate::mux::{
    ActiveWindow, AgentPane, ClientFocus, MuxPane, MuxProvider, MuxSessionInfo, SidebarPane,
    SidebarPosition,
};
use crate::tmux_scripting::{
    delayed_http_hook_command, hook_context_format, hook_slot, http_hook_command,
    owned_hook_indices, pane_died_hook_command, pane_exited_hook_command,
    sidebar_width_repair_hook_command,
};

const SEP: &str = "\t";
const STASH_SESSION: &str = "_os_stash";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl CommandOutput {
    pub fn ok(&self) -> bool {
        self.exit_code == 0
    }
}

pub trait CommandRunner: Send + Sync {
    fn run(&self, args: &[String]) -> CommandOutput;
}

#[derive(Debug, Clone)]
pub struct StdCommandRunner {
    binary: String,
}

impl StdCommandRunner {
    pub fn new(binary: impl Into<String>) -> Self {
        Self {
            binary: binary.into(),
        }
    }
}

impl Default for StdCommandRunner {
    fn default() -> Self {
        Self::new("tmux")
    }
}

impl CommandRunner for StdCommandRunner {
    fn run(&self, args: &[String]) -> CommandOutput {
        match Command::new(&self.binary).args(args).output() {
            Ok(output) => CommandOutput {
                exit_code: output.status.code().unwrap_or(1),
                stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            },
            Err(err) => CommandOutput {
                exit_code: 1,
                stdout: String::new(),
                stderr: err.to_string(),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub id: String,
    pub name: String,
    pub created_at: u64,
    pub attached_clients: u32,
    pub window_count: u32,
    pub dir: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowInfo {
    pub id: String,
    pub session_id: String,
    pub session_name: String,
    pub index: u32,
    pub name: String,
    pub active: bool,
    pub pane_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneInfo {
    pub id: String,
    pub session_name: String,
    pub window_id: String,
    pub window_index: u32,
    pub index: u32,
    pub active: bool,
    pub tty: String,
    pub pid: u32,
    pub cwd: String,
    pub command: String,
    pub title: String,
    pub width: u16,
    pub height: u16,
    pub left: u16,
    pub right: u16,
    /// The pane's process has exited but the pane is kept by `remain-on-exit`.
    pub dead: bool,
    /// The pane's window is the current window of its session.
    pub window_active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientInfo {
    pub name: String,
    pub tty: String,
    pub pid: u32,
    pub session_name: String,
    pub width: u16,
    pub height: u16,
}

#[derive(Clone)]
pub struct TmuxClient {
    runner: Arc<dyn CommandRunner>,
}

impl TmuxClient {
    pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self { runner }
    }

    pub fn run(&self, args: &[&str]) -> CommandOutput {
        let args = args
            .iter()
            .map(|arg| (*arg).to_string())
            .collect::<Vec<_>>();
        self.runner.run(&args)
    }

    pub fn list_sessions(&self) -> Vec<SessionInfo> {
        parse_sessions(&self.run(&["list-sessions", "-F", session_format()]).stdout)
    }

    pub fn list_windows(&self) -> Vec<WindowInfo> {
        parse_windows(
            &self
                .run(&["list-windows", "-a", "-F", window_format()])
                .stdout,
        )
    }

    pub fn list_clients(&self) -> Vec<ClientInfo> {
        parse_clients(&self.run(&["list-clients", "-F", client_format()]).stdout)
    }

    /// The current pane of every attached client: `#{pane_id}` in a
    /// `list-clients` format expands to the active pane of the client's
    /// current window.
    pub fn list_client_focus(&self) -> Vec<ClientFocus> {
        parse_client_focus(
            &self
                .run(&[
                    "list-clients",
                    "-F",
                    "#{client_tty}\t#{session_name}\t#{window_id}\t#{pane_id}",
                ])
                .stdout,
        )
    }

    pub fn list_panes(&self, scope: PaneScope<'_>) -> Vec<PaneInfo> {
        let session_target = match scope {
            PaneScope::Session(name) => exact_session_window_target(name),
            _ => String::new(),
        };
        let session_target = session_target.as_str();
        let mut args = vec!["list-panes"];
        match scope {
            PaneScope::All => args.push("-a"),
            PaneScope::Session(_) => {
                args.push("-s");
                args.push("-t");
                args.push(session_target);
            }
            PaneScope::Window(target) => {
                args.push("-t");
                args.push(target);
            }
        }
        args.push("-F");
        args.push(pane_format());
        parse_panes(&self.run(&args).stdout)
    }

    pub fn switch_client(&self, target: &str, client_tty: Option<&str>) {
        let mut args = vec!["switch-client"];
        if let Some(client_tty) = client_tty {
            args.push("-c");
            args.push(client_tty);
        }
        let target = exact_session_target(target);
        args.push("-t");
        args.push(&target);
        self.run(&args);
    }

    pub fn select_sidebar_pane_for_session(&self, session_name: &str) {
        let Some(window_id) = self
            .list_windows()
            .into_iter()
            .find(|window| window.session_name == session_name && window.active)
            .map(|window| window.id)
        else {
            return;
        };
        let Some(sidebar_pane) = self
            .list_panes(PaneScope::Window(&window_id))
            .into_iter()
            .find(|pane| pane.title == "opensessions-sidebar")
            .map(|pane| pane.id)
        else {
            return;
        };
        self.select_pane(&sidebar_pane);
    }

    pub fn new_session(&self, name: Option<&str>, cwd: Option<&str>) -> String {
        let mut args = vec!["new-session", "-d"];
        if let Some(name) = name {
            args.push("-s");
            args.push(name);
        }
        if let Some(cwd) = cwd {
            args.push("-c");
            args.push(cwd);
        }
        args.extend(["-P", "-F", "#{session_name}"]);
        self.run(&args).stdout
    }

    pub fn kill_session(&self, target: &str) {
        self.run(&["kill-session", "-t", &exact_session_target(target)]);
    }

    pub fn kill_pane(&self, target: &str) {
        self.run(&["kill-pane", "-t", target]);
    }

    pub fn select_window(&self, target: &str) {
        self.run(&["select-window", "-t", target]);
    }

    pub fn select_pane(&self, target: &str) {
        self.run(&["select-pane", "-t", target]);
    }

    pub fn flash_pane(&self, target: &str) {
        self.run(&["select-pane", "-t", target, "-P", "bg=colour238"]);
        let quoted = shell_quote(target);
        self.run(&[
            "run-shell",
            "-b",
            &format!("sleep 0.18; tmux select-pane -t {quoted} -P default"),
        ]);
    }

    pub fn set_pane_title(&self, target: &str, title: &str) {
        self.run(&["select-pane", "-t", target, "-T", title]);
    }

    pub fn resize_pane_width(&self, target: &str, width: u16) {
        self.run(&["resize-pane", "-t", target, "-x", &width.to_string()]);
    }

    /// Turn `remain-on-exit` on for a window that hosts a sidebar, or release
    /// it again. Releasing unsets the window-level value rather than forcing
    /// `off`, so a user's own global `remain-on-exit on` is not overridden.
    pub fn set_window_remain_on_exit(&self, target: &str, enabled: bool) {
        if enabled {
            self.run(&["set-window-option", "-t", target, "remain-on-exit", "on"]);
        } else {
            self.run(&["set-window-option", "-u", "-t", target, "remain-on-exit"]);
        }
    }

    pub fn set_remain_on_exit_for_sidebar_windows(&self, enabled: bool) {
        let mut seen_windows = HashSet::new();
        for pane in self.list_panes(PaneScope::All) {
            if pane.title == "opensessions-sidebar" && seen_windows.insert(pane.window_id.clone()) {
                self.set_window_remain_on_exit(&pane.window_id, enabled);
            }
        }
    }

    pub fn split_sidebar_pane(
        &self,
        target: &str,
        before: bool,
        width: u16,
        env: &[(&str, &str)],
        command: &str,
    ) -> Option<PaneInfo> {
        let size = width.to_string();
        let side = if before { "-hb" } else { "-h" };
        let mut args = vec!["split-window", side, "-f", "-l", &size];
        // `-e` hands the pane its environment directly, so per-pane values
        // never pass through the user's shell (tmux >= 3.0).
        let env_args = env
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>();
        for assignment in &env_args {
            args.push("-e");
            args.push(assignment);
        }
        args.extend(["-t", target, "-P", "-F", pane_format(), command]);
        let output = self.run(&args);
        if !output.ok() || output.stdout.is_empty() {
            return None;
        }
        parse_panes(&output.stdout).into_iter().next()
    }

    pub fn display(&self, format: &str, target: Option<&str>) -> String {
        let mut args = vec!["display-message"];
        if let Some(target) = target {
            args.push("-t");
            args.push(target);
        }
        args.push("-p");
        args.push(format);
        self.run(&args).stdout
    }

    pub fn display_for_client(&self, format: &str, client_tty: Option<&str>) -> String {
        let mut args = vec!["display-message"];
        if let Some(client_tty) = client_tty.filter(|client_tty| !client_tty.is_empty()) {
            args.push("-c");
            args.push(client_tty);
        }
        args.push("-p");
        args.push(format);
        self.run(&args).stdout
    }

    pub fn get_current_session(&self) -> Option<String> {
        let session_name = self.display("#{session_name}", None);
        if !session_name.is_empty() && !session_name.contains('/') {
            return Some(session_name);
        }
        self.list_clients()
            .into_iter()
            .find(|client| !client.tty.is_empty())
            .and_then(|client| (!client.session_name.is_empty()).then_some(client.session_name))
    }

    pub fn get_client_tty(&self) -> String {
        self.display("#{client_tty}", None)
    }

    pub fn get_current_window_id(&self) -> Option<String> {
        let window_id = self.display("#{window_id}", None);
        (!window_id.is_empty()).then_some(window_id)
    }

    pub fn get_current_pane_id(&self) -> Option<String> {
        let pane_id = self.display("#{pane_id}", None);
        (!pane_id.is_empty()).then_some(pane_id)
    }

    pub fn get_client_focus(&self, client_tty: Option<&str>) -> Option<ClientFocus> {
        let raw = self.display_for_client(
            "#{client_tty}\t#{session_name}\t#{window_id}\t#{pane_id}",
            client_tty,
        );
        let parts = raw.split(SEP).collect::<Vec<_>>();
        if parts.len() < 4 || parts[1].is_empty() || parts[2].is_empty() || parts[3].is_empty() {
            return None;
        }
        Some(ClientFocus {
            client_tty: (!parts[0].is_empty()).then(|| parts[0].to_string()),
            session_name: parts[1].to_string(),
            window_id: parts[2].to_string(),
            pane_id: parts[3].to_string(),
        })
    }

    pub fn get_session_dir(&self, target: &str) -> String {
        self.display(
            "#{pane_current_path}",
            Some(&exact_session_window_target(target)),
        )
    }

    pub fn get_pane_count(&self, target: &str) -> u32 {
        self.list_panes(PaneScope::Session(target)).len() as u32
    }

    pub fn get_all_pane_counts(&self) -> HashMap<String, u32> {
        let mut counts = HashMap::new();
        for pane in self.list_panes(PaneScope::All) {
            *counts.entry(pane.session_name).or_insert(0) += 1;
        }
        counts
    }

    pub fn get_active_session_dirs(&self) -> HashMap<String, String> {
        let output = self.run(&[
            "list-panes",
            "-a",
            "-f",
            "#{&&:#{window_active},#{!=:#{pane_title},opensessions-sidebar}}",
            "-F",
            "#{session_name}\t#{pane_current_path}",
        ]);
        let mut dirs = HashMap::new();
        for line in output.stdout.lines() {
            let Some((session, cwd)) = line.split_once(SEP) else {
                continue;
            };
            dirs.entry(session.to_string())
                .or_insert_with(|| cwd.to_string());
        }
        dirs
    }

    /// Install `command` as opensessions' entry for `name`, in its own array
    /// slot so hooks other plugins registered on the same event survive.
    pub fn set_global_hook(&self, name: &str, command: &str) {
        self.remove_owned_hook_entries(name);
        let slot = hook_slot(name);
        let output = self.run(&["set-hook", "-g", &slot, command]);
        if !output.ok() {
            eprintln!(
                "opensessions: failed to install tmux hook {slot}: status={} stderr={} command={command}",
                output.exit_code, output.stderr,
            );
        }
    }

    pub fn unset_global_hook(&self, name: &str) {
        self.remove_owned_hook_entries(name);
        self.run(&["set-hook", "-gu", &hook_slot(name)]);
    }

    /// Remove every entry of `name` that opensessions wrote, in whatever slot.
    /// Releases before hooks moved to a fixed slot replaced the whole array,
    /// so their entry sits at index 0; leaving it behind after an upgrade
    /// would keep a hook pointing at a dead server port. Other plugins'
    /// entries are untouched.
    fn remove_owned_hook_entries(&self, name: &str) {
        let listing = self.run(&["show-hooks", "-g", name]);
        if !listing.ok() {
            return;
        }
        for index in owned_hook_indices(name, &listing.stdout) {
            self.run(&["set-hook", "-gu", &format!("{name}[{index}]")]);
        }
    }

    pub fn set_global_option(&self, name: &str, value: &str) {
        self.run(&["set-option", "-gq", name, value]);
    }

    pub fn unset_global_option(&self, name: &str) {
        self.run(&["set-option", "-gu", name]);
    }
}

pub enum PaneScope<'a> {
    All,
    Session(&'a str),
    Window(&'a str),
}

#[derive(Clone)]
pub struct TmuxProvider {
    name: String,
    client: TmuxClient,
}

impl TmuxProvider {
    pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self {
            name: "tmux".to_string(),
            client: TmuxClient::new(runner),
        }
    }
}

impl MuxProvider for TmuxProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn list_sessions(&self) -> Vec<MuxSessionInfo> {
        let active_dirs = self.client.get_active_session_dirs();
        self.client
            .list_sessions()
            .into_iter()
            .filter(|session| session.name != STASH_SESSION)
            .map(|session| MuxSessionInfo {
                name: session.name.clone(),
                created_at: session.created_at,
                dir: active_dirs
                    .get(&session.name)
                    .cloned()
                    .unwrap_or(session.dir),
                windows: session.window_count,
            })
            .collect()
    }

    fn switch_session(&self, name: &str, client_tty: Option<&str>) {
        self.client.switch_client(name, client_tty);
    }

    fn get_current_session(&self) -> Option<String> {
        self.client.get_current_session()
    }

    fn get_session_dir(&self, name: &str) -> String {
        self.client.get_session_dir(name)
    }

    fn get_session_pane_pids(&self, name: &str) -> Vec<u32> {
        self.client
            .list_panes(PaneScope::Session(name))
            .into_iter()
            .map(|pane| pane.pid)
            .filter(|pid| *pid > 0)
            .collect()
    }

    fn get_pane_count(&self, name: &str) -> u32 {
        self.client.get_pane_count(name)
    }

    fn get_client_tty(&self) -> String {
        self.client.get_client_tty()
    }

    fn create_session(&self, name: Option<&str>, dir: Option<&str>) {
        self.client.new_session(name, dir);
    }

    fn kill_session(&self, name: &str) {
        self.client.kill_session(name);
    }

    fn cleanup_sidebar(&self) {
        self.client.kill_session(STASH_SESSION);
    }

    fn setup_hooks(&self, server_host: &str, server_port: u16) {
        let base = format!("http://{server_host}:{server_port}");
        let hook_context = hook_context_format();
        let focus_cmd = http_hook_command(&base, "/focus", Some(hook_context), true);
        let refresh_cmd = http_hook_command(&base, "/refresh", None, true);
        let ensure_cmd = http_hook_command(&base, "/ensure-sidebar", Some(hook_context), true);
        let pane_exited_cmd = pane_exited_hook_command(&base);
        let pane_died_cmd = pane_died_hook_command(&base);
        let repair_sidebar_width_cmd = sidebar_width_repair_hook_command();
        let client_resized_cmd = format!(
            "{repair_sidebar_width_cmd} ; {}",
            delayed_http_hook_command(&base, "/client-resized"),
        );
        let pane_layout_changed_cmd = format!(
            "{repair_sidebar_width_cmd} ; {}",
            delayed_http_hook_command(&base, "/pane-layout-changed"),
        );

        self.client.set_global_hook(
            "client-session-changed",
            &format!("{focus_cmd} ; {ensure_cmd}"),
        );
        self.client.set_global_hook("after-select-pane", &focus_cmd);
        self.client.set_global_hook("session-created", &refresh_cmd);
        self.client.set_global_hook("session-closed", &refresh_cmd);
        self.client
            .set_global_hook("after-select-window", &ensure_cmd);
        self.client.set_global_hook("after-new-window", &ensure_cmd);
        self.client
            .set_global_hook("client-resized", &client_resized_cmd);
        self.client
            .set_global_hook("after-kill-pane", &pane_exited_cmd);
        self.client.set_global_hook("pane-exited", &pane_exited_cmd);
        self.client.set_global_hook("pane-died", &pane_died_cmd);
        self.client
            .set_global_hook("after-resize-pane", &repair_sidebar_width_cmd);
        self.client
            .set_global_hook("after-resize-window", &pane_layout_changed_cmd);
        self.client.set_remain_on_exit_for_sidebar_windows(true);
    }

    fn cleanup_hooks(&self) {
        self.client.set_remain_on_exit_for_sidebar_windows(false);
        for hook in [
            "client-session-changed",
            "after-select-pane",
            "session-created",
            "session-closed",
            "after-select-window",
            "after-new-window",
            "client-resized",
            "after-kill-pane",
            "pane-exited",
            "pane-died",
            "after-resize-pane",
            "after-resize-window",
        ] {
            self.client.unset_global_hook(hook);
        }
        self.client.unset_global_option("@opensessions_width");
    }

    fn set_sidebar_width_hint(&self, width: u16) {
        self.client
            .set_global_option("@opensessions_width", &width.to_string());
    }

    fn is_window_capable(&self) -> bool {
        true
    }

    fn is_sidebar_capable(&self) -> bool {
        true
    }

    fn is_batch_capable(&self) -> bool {
        true
    }

    fn list_active_windows(&self) -> Vec<ActiveWindow> {
        let mut windows = Vec::<ActiveWindow>::new();
        for window in self
            .client
            .list_windows()
            .into_iter()
            .filter(|window| window.session_name != STASH_SESSION)
        {
            let next = ActiveWindow {
                id: window.id,
                session_name: window.session_name,
                active: window.active,
            };
            if let Some(current) = windows.iter_mut().find(|current| current.id == next.id) {
                if !current.active && next.active {
                    *current = next;
                }
            } else {
                windows.push(next);
            }
        }

        windows
    }

    fn get_current_window_id(&self) -> Option<String> {
        self.client.get_current_window_id()
    }

    fn get_current_pane_id(&self) -> Option<String> {
        self.client.get_current_pane_id()
    }

    fn get_client_focus(&self, client_tty: Option<&str>) -> Option<ClientFocus> {
        self.client.get_client_focus(client_tty)
    }

    fn list_sidebar_panes(&self, session_name: Option<&str>) -> Vec<SidebarPane> {
        let panes = match session_name {
            Some(session_name) => self.client.list_panes(PaneScope::Session(session_name)),
            None => self.client.list_panes(PaneScope::All),
        };
        let mut window_widths = HashMap::new();
        for pane in &panes {
            let width = pane.right.saturating_add(1);
            window_widths
                .entry(pane.window_id.clone())
                .and_modify(|current: &mut u16| *current = (*current).max(width))
                .or_insert(width);
        }

        let mut seen_pane_ids = HashSet::new();
        panes
            .into_iter()
            .filter(|pane| {
                pane.title == "opensessions-sidebar" && pane.session_name != STASH_SESSION
            })
            .filter(|pane| seen_pane_ids.insert(pane.id.clone()))
            .map(|pane| SidebarPane {
                pane_id: pane.id,
                session_name: pane.session_name,
                window_id: pane.window_id.clone(),
                width: Some(pane.width),
                window_width: window_widths.get(&pane.window_id).copied(),
            })
            .collect()
    }

    fn list_agent_panes(&self, session_name: &str) -> Vec<AgentPane> {
        agent_panes_from(
            &self
                .list_all_panes()
                .into_iter()
                .filter(|pane| pane.session_name == session_name)
                .collect::<Vec<_>>(),
        )
    }

    fn list_all_panes(&self) -> Vec<MuxPane> {
        self.client
            .list_panes(PaneScope::All)
            .into_iter()
            .filter(|pane| pane.session_name != STASH_SESSION && !is_sidebar_pane(pane))
            .map(|pane| MuxPane {
                pane_id: pane.id,
                session_name: pane.session_name,
                window_id: pane.window_id,
                window_active: pane.window_active,
                active: pane.active,
                pid: pane.pid,
                command: pane.command,
                title: pane.title,
                dead: pane.dead,
            })
            .collect()
    }

    fn list_client_focus(&self) -> Vec<ClientFocus> {
        self.client
            .list_client_focus()
            .into_iter()
            .filter(|focus| focus.session_name != STASH_SESSION)
            .collect()
    }

    fn hide_sidebar(&self, pane_id: &str) {
        // The window only needed remain-on-exit while it hosted a sidebar;
        // release it first so a later shell exit closes the pane normally.
        let window_id = self.client.display("#{window_id}", Some(pane_id));
        if !window_id.is_empty() {
            self.client.set_window_remain_on_exit(&window_id, false);
        }
        self.client.kill_pane(pane_id);
    }

    fn kill_sidebar_pane(&self, pane_id: &str) {
        self.client.kill_pane(pane_id);
    }

    fn prepare_sidebar_window(&self, window_id: &str) {
        self.client.set_window_remain_on_exit(window_id, true);
    }

    fn focus_pane(&self, pane_id: &str) {
        let window_id = self.client.display("#{window_id}", Some(pane_id));
        if !window_id.is_empty() {
            self.client.select_window(&window_id);
        }
        self.client.select_pane(pane_id);
        self.client.flash_pane(pane_id);
    }

    fn kill_pane(&self, pane_id: &str) {
        self.client.kill_pane(pane_id);
    }

    fn resolve_agent_pane_id(
        &self,
        session: &str,
        agent: &str,
        _thread_id: Option<&str>,
        thread_name: Option<&str>,
    ) -> Option<String> {
        let panes = self
            .client
            .list_panes(PaneScope::Session(session))
            .into_iter()
            .filter(|pane| pane.title != "opensessions-sidebar")
            .collect::<Vec<_>>();

        if agent == "amp"
            && let Some(thread_name) = thread_name
        {
            let matches = panes
                .iter()
                .filter(|pane| {
                    pane.title.to_lowercase().starts_with("amp - ")
                        && pane.title.contains(thread_name)
                })
                .collect::<Vec<_>>();
            if matches.len() == 1 {
                return Some(matches[0].id.clone());
            }
        }

        let patterns = match agent {
            "amp" => &["amp"][..],
            "claude-code" => &["claude"][..],
            "codex" => &["codex"][..],
            "opencode" => &["opencode"][..],
            _ => return None,
        };
        panes
            .into_iter()
            .find(|pane| {
                let title = pane.title.to_lowercase();
                patterns.iter().any(|pattern| title.contains(pattern))
            })
            .map(|pane| pane.id)
    }

    fn resize_sidebar_pane(&self, pane_id: &str, width: u16) {
        self.client.resize_pane_width(pane_id, width);
    }

    fn kill_orphaned_sidebar_panes(&self) {
        self.kill_orphaned_sidebar_panes_with_fallbacks(&HashMap::new());
    }

    fn kill_orphaned_sidebar_panes_with_fallbacks(
        &self,
        fallback_sessions: &HashMap<String, String>,
    ) {
        let panes = self.client.list_panes(PaneScope::All);
        let windows_by_session = self
            .client
            .list_sessions()
            .into_iter()
            .map(|session| (session.name, session.window_count))
            .collect::<HashMap<_, _>>();
        let mut window_pane_counts: HashMap<String, u32> = HashMap::new();
        let mut sidebars_by_window: HashMap<String, Vec<String>> = HashMap::new();
        let mut session_by_window: HashMap<String, String> = HashMap::new();
        let mut seen_pane_ids = HashSet::new();

        for pane in panes {
            if pane.session_name == STASH_SESSION || !seen_pane_ids.insert(pane.id.clone()) {
                continue;
            }
            session_by_window
                .entry(pane.window_id.clone())
                .or_insert_with(|| pane.session_name.clone());
            *window_pane_counts
                .entry(pane.window_id.clone())
                .or_insert(0) += 1;
            if pane.title == "opensessions-sidebar" {
                sidebars_by_window
                    .entry(pane.window_id)
                    .or_default()
                    .push(pane.id);
            }
        }

        for (window_id, sidebars) in sidebars_by_window {
            if window_pane_counts.get(&window_id) == Some(&1) {
                if let Some(session_name) = session_by_window.get(&window_id)
                    && windows_by_session.get(session_name).copied().unwrap_or(1) <= 1
                    && let Some(fallback_session) = fallback_sessions.get(session_name)
                {
                    for client in self.client.list_clients() {
                        if client.session_name == *session_name {
                            self.client
                                .switch_client(fallback_session, Some(&client.tty));
                        }
                    }
                }
                for pane_id in sidebars {
                    self.client.kill_pane(&pane_id);
                }
                continue;
            }
            for pane_id in sidebars.into_iter().skip(1) {
                self.client.kill_pane(&pane_id);
            }
        }
    }

    fn spawn_sidebar(
        &self,
        _session_name: &str,
        window_id: &str,
        width: u16,
        position: SidebarPosition,
        scripts_dir: &str,
    ) -> Option<String> {
        let panes = self.client.list_panes(PaneScope::Window(window_id));
        let target = match position {
            SidebarPosition::Left => panes.iter().min_by_key(|pane| pane.left),
            SidebarPosition::Right => panes.iter().max_by_key(|pane| pane.right),
        }?;
        let command = sidebar_launch_command(scripts_dir);
        let env = [
            ("OPENSESSIONS_SESSION_NAME", target.session_name.as_str()),
            ("OPENSESSIONS_WINDOW_ID", window_id),
            ("REFOCUS_WINDOW", window_id),
        ];
        let new_pane = self.client.split_sidebar_pane(
            &target.id,
            position == SidebarPosition::Left,
            width,
            &env,
            &command,
        )?;
        self.client
            .set_pane_title(&new_pane.id, "opensessions-sidebar");
        self.client.set_window_remain_on_exit(window_id, true);
        Some(new_pane.id)
    }

    fn get_all_pane_counts(&self) -> HashMap<String, u32> {
        self.client.get_all_pane_counts()
    }

    fn mux_server_id(&self) -> Option<String> {
        let output = self.client.run(&["display-message", "-p", "#{pid}"]);
        let pid = output.stdout.trim();
        (output.ok() && !pid.is_empty()).then(|| pid.to_string())
    }

    fn resolve_session_id(&self, id: &str) -> Option<String> {
        self.client
            .list_sessions()
            .into_iter()
            .find(|session| session.id == id)
            .map(|session| session.name)
    }

    fn nudge_dead_panes(&self) -> bool {
        if !self
            .client
            .list_panes(PaneScope::All)
            .iter()
            .any(|pane| pane.dead)
        {
            return false;
        }
        // tmux <= 3.5a built with utempter can lose the SIGCHLD of an exiting
        // pane process (tmux issue #4559): the pane shows as dead, but tmux
        // never reaps the child, so `pane-died` (and our cleanup hook) never
        // fires. Spawning any trivial job hands tmux a fresh SIGCHLD; its
        // reaper then collects the zombie and the notification goes out.
        self.client.run(&["run-shell", "-b", "true"]);
        true
    }
}

/// Build the pane command that launches the sidebar.
///
/// tmux runs pane commands through the user's `default-shell`, which may be a
/// non-POSIX shell such as fish that cannot parse `${VAR:-default}` or
/// `NAME=value cmd` prefixes. The user's shell therefore only ever sees the
/// fixed string `sh -c '...'`; the POSIX expansion runs inside `sh`. The path
/// is resolved against `$OPENSESSIONS_DIR` (exported into tmux's global
/// environment by `opensessions.tmux`) so the pane works even when the parent
/// pane's cwd is unrelated to the plugin checkout. Per-pane values such as the
/// session name are passed with `split-window -e`, never through a shell.
fn sidebar_launch_command(scripts_dir: &str) -> String {
    debug_assert!(
        !scripts_dir.contains('\''),
        "scripts_dir must not contain single quotes"
    );
    format!("sh -c 'exec \"${{OPENSESSIONS_DIR:-.}}\"/{scripts_dir}/start.sh'")
}

fn session_format() -> &'static str {
    "#{session_id}\t#{session_name}\t#{session_created}\t#{session_attached}\t#{session_windows}\t#{session_path}"
}

fn window_format() -> &'static str {
    "#{window_id}\t#{session_id}\t#{session_name}\t#{window_index}\t#{window_name}\t#{window_active}\t#{window_panes}"
}

fn client_format() -> &'static str {
    "#{client_name}\t#{client_tty}\t#{client_pid}\t#{session_name}\t#{client_width}\t#{client_height}"
}

fn pane_format() -> &'static str {
    "#{pane_id}\t#{session_name}\t#{window_id}\t#{window_index}\t#{pane_index}\t#{pane_active}\t#{pane_tty}\t#{pane_pid}\t#{pane_current_path}\t#{pane_current_command}\t#{pane_title}\t#{pane_width}\t#{pane_height}\t#{pane_left}\t#{pane_right}\t#{pane_dead}\t#{window_active}"
}

/// tmux resolves a bare `-t <name>` on window/pane commands (`list-panes -s`,
/// `display-message`, ...) as a window of the *current* session first, so a
/// session called `1` or `api` can silently target another session's panes.
/// `=name:` forces an exact session match with its active window.
fn exact_session_window_target(name: &str) -> String {
    format!("={name}:")
}

/// Exact session match for session-target commands (`switch-client`,
/// `kill-session`), which otherwise accept prefix and glob matches.
fn exact_session_target(name: &str) -> String {
    format!("={name}")
}

fn is_sidebar_pane(pane: &PaneInfo) -> bool {
    pane.title == "opensessions-sidebar"
}

/// Agent-looking panes among `panes`, detected from process name and title.
/// This is Agent Pane Presence (see CONTEXT.md): it may bind a pane to an
/// agent row, it never authors agent status.
pub fn agent_panes_from(panes: &[MuxPane]) -> Vec<AgentPane> {
    panes
        .iter()
        .filter_map(|pane| {
            let agent = detect_agent(&pane.title, &pane.command)?;
            Some(AgentPane {
                thread_name: thread_name_from_title(&pane.title, &agent),
                agent,
                pane_id: pane.pane_id.clone(),
                active: pane.active,
                thread_id: None,
            })
        })
        .collect()
}

fn agent_from_pane(pane: &PaneInfo) -> Option<String> {
    detect_agent(&pane.title, &pane.command)
}

/// Which agent CLI, if any, a pane with this title and current command looks
/// like it is running.
pub fn detect_agent(title: &str, command: &str) -> Option<String> {
    let title = title.to_lowercase();
    let command = command.to_lowercase();
    if title == "pi" || title.starts_with("pi ") || title.starts_with('π') || command == "pi" {
        return Some("pi".to_string());
    }
    // Match whole words only: a substring match let `amp` claim any pane whose
    // title contained "example" and `claude` claim "claude-docs".
    let tokens = format!("{title} {command}")
        .split(|ch: char| !(ch.is_alphanumeric() || ch == '-' || ch == '_'))
        .filter(|token| !token.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    for (agent, aliases) in AGENT_ALIASES {
        if tokens.iter().any(|token| aliases.contains(&token.as_str())) {
            return Some((*agent).to_string());
        }
    }
    None
}

// Keep this broad and process/title based for zero-config agent
// awareness. Transcript/file watchers still provide richer status where we
// have native integrations; this path makes panes from other popular CLIs show
// up immediately instead of disappearing from the sidebar.
const AGENT_ALIASES: &[(&str, &[&str])] = &[
    ("amp", &["amp", "amp-local"]),
    ("claude-code", &["claude", "claude-code"]),
    ("codex", &["codex"]),
    ("gemini", &["gemini"]),
    ("cursor", &["cursor", "cursor-agent"]),
    ("antigravity", &["agy", "antigravity", "antigravity-cli"]),
    ("cline", &["cline"]),
    ("opencode", &["opencode", "open-code"]),
    ("github-copilot", &["copilot", "github-copilot", "ghcs"]),
    ("kimi", &["kimi", "kimi-code"]),
    ("kiro", &["kiro", "kiro-cli"]),
    ("droid", &["droid"]),
    ("grok", &["grok", "grok-build"]),
    ("hermes", &["hermes", "hermes-agent"]),
    ("qodercli", &["qodercli", "qoderclicn", "qoder", "qodercn"]),
];

fn thread_name_from_title(title: &str, agent: &str) -> Option<String> {
    let title = title.trim();
    if agent == "amp"
        && let Some((thread_name, _)) = title.split_once(" - amp - ")
    {
        let thread_name = thread_name.trim();
        if !thread_name.is_empty() {
            return Some(thread_name.to_string());
        }
    }
    None
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn parse_sessions(raw: &str) -> Vec<SessionInfo> {
    raw.lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let parts = split(line);
            SessionInfo {
                id: part(&parts, 0),
                name: part(&parts, 1),
                created_at: parse_u64(&parts, 2),
                attached_clients: parse_u32(&parts, 3),
                window_count: parse_u32(&parts, 4),
                dir: part(&parts, 5),
            }
        })
        .collect()
}

fn parse_windows(raw: &str) -> Vec<WindowInfo> {
    raw.lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let parts = split(line);
            WindowInfo {
                id: part(&parts, 0),
                session_id: part(&parts, 1),
                session_name: part(&parts, 2),
                index: parse_u32(&parts, 3),
                name: part(&parts, 4),
                active: part(&parts, 5) == "1",
                pane_count: parse_u32(&parts, 6),
            }
        })
        .collect()
}

fn parse_clients(raw: &str) -> Vec<ClientInfo> {
    raw.lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let parts = split(line);
            ClientInfo {
                name: part(&parts, 0),
                tty: part(&parts, 1),
                pid: parse_u32(&parts, 2),
                session_name: part(&parts, 3),
                width: parse_u16(&parts, 4),
                height: parse_u16(&parts, 5),
            }
        })
        .collect()
}

fn parse_panes(raw: &str) -> Vec<PaneInfo> {
    raw.lines()
        .filter(|line| !line.is_empty())
        .filter_map(|line| {
            let parts = split(line);
            if parts.len() < 15 {
                return None;
            }
            Some(PaneInfo {
                id: part(&parts, 0),
                session_name: part(&parts, 1),
                window_id: part(&parts, 2),
                window_index: parse_u32(&parts, 3),
                index: parse_u32(&parts, 4),
                active: part(&parts, 5) == "1",
                tty: part(&parts, 6),
                pid: parse_u32(&parts, 7),
                cwd: part(&parts, 8),
                command: part(&parts, 9),
                title: part(&parts, 10),
                width: parse_u16(&parts, 11),
                height: parse_u16(&parts, 12),
                left: parse_u16(&parts, 13),
                right: parse_u16(&parts, 14),
                dead: part(&parts, 15) == "1",
                window_active: part(&parts, 16) == "1",
            })
        })
        .collect()
}

fn parse_client_focus(raw: &str) -> Vec<ClientFocus> {
    raw.lines()
        .filter(|line| !line.is_empty())
        .filter_map(|line| {
            let parts = split(line);
            let session_name = part(&parts, 1);
            let window_id = part(&parts, 2);
            let pane_id = part(&parts, 3);
            if session_name.is_empty() || window_id.is_empty() || pane_id.is_empty() {
                return None;
            }
            let client_tty = part(&parts, 0);
            Some(ClientFocus {
                client_tty: (!client_tty.is_empty()).then_some(client_tty),
                session_name,
                window_id,
                pane_id,
            })
        })
        .collect()
}

fn split(line: &str) -> Vec<&str> {
    line.split(SEP).collect()
}

fn part(parts: &[&str], index: usize) -> String {
    parts.get(index).copied().unwrap_or_default().to_string()
}

fn parse_u16(parts: &[&str], index: usize) -> u16 {
    parts
        .get(index)
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or_default()
}

fn parse_u32(parts: &[&str], index: usize) -> u32 {
    parts
        .get(index)
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or_default()
}

fn parse_u64(parts: &[&str], index: usize) -> u64 {
    parts
        .get(index)
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct RecordingRunner {
        calls: Mutex<Vec<Vec<String>>>,
    }

    impl CommandRunner for RecordingRunner {
        fn run(&self, args: &[String]) -> CommandOutput {
            self.calls.lock().unwrap().push(args.to_vec());
            CommandOutput {
                exit_code: 0,
                stdout: "/dev/ttys001\topensessions\t@0\t%186".to_string(),
                stderr: String::new(),
            }
        }
    }

    /// Runner that answers each tmux subcommand with a canned stdout.
    struct ScriptedRunner {
        calls: Mutex<Vec<Vec<String>>>,
        replies: HashMap<&'static str, String>,
    }

    impl ScriptedRunner {
        fn new(replies: HashMap<&'static str, String>) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                replies,
            }
        }

        fn call(&self, subcommand: &str) -> Vec<String> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .find(|call| call.first().map(String::as_str) == Some(subcommand))
                .cloned()
                .unwrap_or_else(|| panic!("expected a `{subcommand}` call"))
        }
    }

    impl CommandRunner for ScriptedRunner {
        fn run(&self, args: &[String]) -> CommandOutput {
            self.calls.lock().unwrap().push(args.to_vec());
            let stdout = args
                .first()
                .and_then(|subcommand| self.replies.get(subcommand.as_str()))
                .cloned()
                .unwrap_or_default();
            CommandOutput {
                exit_code: 0,
                stdout,
                stderr: String::new(),
            }
        }
    }

    fn pane_row(id: &str, session: &str, title: &str, left: u16, right: u16) -> String {
        format!(
            "{id}\t{session}\t@1\t0\t0\t1\t/dev/ttys1\t123\t/repo\tbash\t{title}\t80\t24\t{left}\t{right}"
        )
    }

    fn full_pane_row(
        id: &str,
        session: &str,
        window: &str,
        active: bool,
        pid: u32,
        command: &str,
        title: &str,
        dead: bool,
        window_active: bool,
    ) -> String {
        format!(
            "{id}\t{session}\t{window}\t0\t0\t{}\t/dev/ttys1\t{pid}\t/repo\t{command}\t{title}\t80\t24\t0\t79\t{}\t{}",
            u8::from(active),
            u8::from(dead),
            u8::from(window_active),
        )
    }

    #[test]
    fn list_all_panes_reports_every_content_pane_with_its_process_in_one_call() {
        let runner = Arc::new(ScriptedRunner::new(HashMap::from([(
            "list-panes",
            [
                full_pane_row("%1", "work", "@1", true, 4242, "claude", "✳ work", false, true),
                full_pane_row("%2", "work", "@2", false, 4300, "bash", "shell", true, false),
                full_pane_row("%3", "work", "@1", false, 4301, "opensessions-sidebar", "opensessions-sidebar", false, true),
                full_pane_row("%4", "_os_stash", "@9", true, 4302, "bash", "stash", false, true),
            ]
            .join("\n"),
        )])));
        let provider = TmuxProvider::new(runner.clone());

        let panes = provider.list_all_panes();

        assert_eq!(
            panes,
            vec![
                MuxPane {
                    pane_id: "%1".to_string(),
                    session_name: "work".to_string(),
                    window_id: "@1".to_string(),
                    window_active: true,
                    active: true,
                    pid: 4242,
                    command: "claude".to_string(),
                    title: "✳ work".to_string(),
                    dead: false,
                },
                MuxPane {
                    pane_id: "%2".to_string(),
                    session_name: "work".to_string(),
                    window_id: "@2".to_string(),
                    window_active: false,
                    active: false,
                    pid: 4300,
                    command: "bash".to_string(),
                    title: "shell".to_string(),
                    dead: true,
                },
            ]
        );
        let calls = runner.calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "one tmux call lists every pane");
        assert_eq!(calls[0][..2], ["list-panes", "-a"]);
    }

    #[test]
    fn list_client_focus_reports_the_current_pane_of_every_attached_client() {
        let runner = Arc::new(ScriptedRunner::new(HashMap::from([(
            "list-clients",
            "/dev/pts/8\twork\t@1\t%1\n/dev/pts/9\tdocs\t@4\t%7\n/dev/pts/3\t\t\t\n/dev/pts/2\t_os_stash\t@9\t%4"
                .to_string(),
        )])));
        let provider = TmuxProvider::new(runner);

        assert_eq!(
            provider.list_client_focus(),
            vec![
                ClientFocus {
                    client_tty: Some("/dev/pts/8".to_string()),
                    session_name: "work".to_string(),
                    window_id: "@1".to_string(),
                    pane_id: "%1".to_string(),
                },
                ClientFocus {
                    client_tty: Some("/dev/pts/9".to_string()),
                    session_name: "docs".to_string(),
                    window_id: "@4".to_string(),
                    pane_id: "%7".to_string(),
                },
            ]
        );
    }

    #[test]
    fn agent_panes_are_derived_from_the_pane_list_without_extra_tmux_calls() {
        let runner = Arc::new(ScriptedRunner::new(HashMap::from([(
            "list-panes",
            [
                full_pane_row("%1", "work", "@1", true, 1, "claude", "✳ work", false, true),
                full_pane_row("%2", "work", "@1", false, 2, "node", "Fix focus - amp - T1", false, true),
                full_pane_row("%3", "docs", "@2", true, 3, "vim", "notes", false, true),
            ]
            .join("\n"),
        )])));
        let provider = TmuxProvider::new(runner.clone());

        let agents = agent_panes_from(&provider.list_all_panes());

        assert_eq!(
            agents,
            vec![
                AgentPane {
                    agent: "claude-code".to_string(),
                    pane_id: "%1".to_string(),
                    active: true,
                    thread_id: None,
                    thread_name: None,
                },
                AgentPane {
                    agent: "amp".to_string(),
                    pane_id: "%2".to_string(),
                    active: false,
                    thread_id: None,
                    thread_name: Some("Fix focus".to_string()),
                },
            ]
        );
        assert_eq!(runner.calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn spawn_sidebar_passes_pane_values_via_env_flags_not_the_shell() {
        // A session name full of shell metacharacters must reach the pane
        // untouched, and the command handed to the user's (possibly non-POSIX)
        // shell must be the fixed `sh -c` launcher with nothing interpolated.
        let evil = r#"x"; touch /tmp/pwned; $(id)`whoami` o'brien"#;
        let runner = Arc::new(ScriptedRunner::new(HashMap::from([
            ("list-panes", pane_row("%1", evil, "main", 0, 79)),
            (
                "split-window",
                pane_row("%9", evil, "opensessions-sidebar", 0, 25),
            ),
        ])));
        let provider = TmuxProvider::new(runner.clone());

        let pane = provider.spawn_sidebar(
            "ignored",
            "@1",
            26,
            SidebarPosition::Left,
            "apps/tui/scripts",
        );

        assert_eq!(pane.as_deref(), Some("%9"));
        let split = runner.call("split-window");
        assert_eq!(
            split,
            vec![
                "split-window",
                "-hb",
                "-f",
                "-l",
                "26",
                "-e",
                &format!("OPENSESSIONS_SESSION_NAME={evil}"),
                "-e",
                "OPENSESSIONS_WINDOW_ID=@1",
                "-e",
                "REFOCUS_WINDOW=@1",
                "-t",
                "%1",
                "-P",
                "-F",
                pane_format(),
                r#"sh -c 'exec "${OPENSESSIONS_DIR:-.}"/apps/tui/scripts/start.sh'"#,
            ]
        );
        assert_eq!(
            runner.call("select-pane"),
            vec!["select-pane", "-t", "%9", "-T", "opensessions-sidebar"]
        );
    }

    #[test]
    fn spawn_sidebar_splits_to_the_right_of_the_rightmost_pane_when_positioned_right() {
        let runner = Arc::new(ScriptedRunner::new(HashMap::from([
            (
                "list-panes",
                format!(
                    "{}\n{}",
                    pane_row("%1", "alpha", "main", 0, 39),
                    pane_row("%2", "alpha", "main", 40, 79)
                ),
            ),
            (
                "split-window",
                pane_row("%9", "alpha", "opensessions-sidebar", 54, 79),
            ),
        ])));
        let provider = TmuxProvider::new(runner.clone());

        provider.spawn_sidebar(
            "alpha",
            "@1",
            26,
            SidebarPosition::Right,
            "apps/tui/scripts",
        );

        let split = runner.call("split-window");
        assert_eq!(split[1], "-h");
        assert_eq!(
            split[split.iter().position(|arg| arg == "-t").unwrap() + 1],
            "%2"
        );
    }

    #[test]
    fn hooks_are_installed_in_their_own_slot_and_legacy_entries_are_cleared() {
        let ours = "run-shell -b \"curl -s -o /dev/null -m 0.2 --connect-timeout 0.1 -X POST http://127.0.0.1:24500/refresh >/dev/null 2>&1 || true\"";
        let listing = format!(
            "session-created[0] {ours}\nsession-created[7] run-shell \"~/.tmux/plugins/other/save.sh\"\n"
        );
        let runner = Arc::new(ScriptedRunner::new(HashMap::from([(
            "show-hooks",
            listing,
        )])));
        let client = TmuxClient::new(runner.clone());

        client.set_global_hook("session-created", ours);
        client.unset_global_hook("session-created");

        let calls = runner.calls.lock().unwrap().clone();
        let slot = format!("session-created[{}]", crate::tmux_scripting::HOOK_SLOT);
        assert_eq!(
            calls,
            vec![
                vec!["show-hooks", "-g", "session-created"],
                vec!["set-hook", "-gu", "session-created[0]"],
                vec!["set-hook", "-g", slot.as_str(), ours],
                vec!["show-hooks", "-g", "session-created"],
                vec!["set-hook", "-gu", "session-created[0]"],
                vec!["set-hook", "-gu", slot.as_str()],
            ]
        );
        assert!(
            !calls
                .iter()
                .any(|call| call == &vec!["set-hook", "-gu", "session-created"]
                    || call == &vec!["set-hook", "-gu", "session-created[7]"]),
            "must never wipe the whole array or another plugin's slot"
        );
    }

    #[test]
    fn session_scoped_commands_use_exact_session_targets() {
        let runner = Arc::new(ScriptedRunner::new(HashMap::new()));
        let client = TmuxClient::new(runner.clone());

        client.list_panes(PaneScope::Session("1"));
        client.get_session_dir("api");
        client.switch_client("api", None);
        client.kill_session("api");

        let calls = runner.calls.lock().unwrap().clone();
        assert_eq!(calls[0][..4], ["list-panes", "-s", "-t", "=1:"]);
        assert_eq!(calls[1][..3], ["display-message", "-t", "=api:"]);
        assert_eq!(calls[2], vec!["switch-client", "-t", "=api"]);
        assert_eq!(calls[3], vec!["kill-session", "-t", "=api"]);
    }

    #[test]
    fn agent_detection_matches_whole_words_only() {
        let pane = |title: &str, command: &str| PaneInfo {
            id: "%1".into(),
            session_name: "s".into(),
            window_id: "@1".into(),
            window_index: 0,
            index: 0,
            active: false,
            tty: String::new(),
            pid: 0,
            cwd: String::new(),
            command: command.into(),
            title: title.into(),
            width: 80,
            height: 24,
            left: 0,
            right: 79,
            dead: false,
            window_active: true,
        };
        assert_eq!(
            agent_from_pane(&pane("host", "claude")).as_deref(),
            Some("claude-code")
        );
        assert_eq!(
            agent_from_pane(&pane("Fix focus - amp - T1", "node")).as_deref(),
            Some("amp")
        );
        assert_eq!(agent_from_pane(&pane("example", "bash")), None);
        assert_eq!(agent_from_pane(&pane("claude-docs", "bash")), None);
        assert_eq!(agent_from_pane(&pane("sample notes", "vim")), None);
    }

    #[test]
    fn client_focus_uses_tmux_target_client_not_window_active_state() {
        let runner = Arc::new(RecordingRunner::default());
        let provider = TmuxProvider::new(runner.clone());

        let focus = provider
            .get_client_focus(Some("/dev/ttys001"))
            .expect("client focus");

        assert_eq!(focus.client_tty.as_deref(), Some("/dev/ttys001"));
        assert_eq!(focus.session_name, "opensessions");
        assert_eq!(focus.window_id, "@0");
        assert_eq!(focus.pane_id, "%186");
        assert_eq!(
            runner.calls.lock().unwrap()[0],
            vec![
                "display-message".to_string(),
                "-c".to_string(),
                "/dev/ttys001".to_string(),
                "-p".to_string(),
                "#{client_tty}\t#{session_name}\t#{window_id}\t#{pane_id}".to_string(),
            ],
        );
    }
}

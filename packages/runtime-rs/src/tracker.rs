//! Agent Thread State (see CONTEXT.md): the rows the sidebar shows per
//! session, keyed by `agent:threadId`.
//!
//! Three sources feed the tracker, in decreasing authority:
//!
//! - the Claude Code session registry (`claude_registry`), which names the
//!   pane and the status outright;
//! - `/api/agent-event` posts, which may name a pane (`paneId`);
//! - transcript watchers, which know a thread and a directory but no pane.
//!
//! A pane binding is *explicit* when a source named the pane and
//! *heuristic* when Agent Pane Presence (an agent-looking pane, see
//! `tmux_provider::agent_panes_from`) was matched to a pane-less row. Panes
//! bind rows; they never author status. Liveness is observed from the mux on
//! every sync: a bound pane that disappears (or a registry process that
//! exits) makes the row *gone*, and gone rows are reaped after a short grace
//! period whatever their status.

use std::collections::{BTreeSet, HashMap, HashSet};

use crate::claude_registry::{CLAUDE_CODE_AGENT, RegistryStatus};
use crate::mux::{AgentPane, ClientFocus, MuxPane};
use crate::protocol::{AgentEvent, AgentLiveness, AgentStatus};
use crate::tmux_provider::agent_panes_from;

const MAX_EVENT_TIMESTAMPS: usize = 30;
/// How long a seen terminal entry with no known pane lingers before it is
/// reaped.
pub const TERMINAL_PRUNE_MS: u64 = 5 * 60 * 1000;
/// Unseen terminal entries stay longer so a finished agent's marker is not
/// reaped before the user had a chance to notice it.
pub const UNSEEN_TERMINAL_PRUNE_MS: u64 = 30 * 60 * 1000;
/// Grace period after an agent's pane or process disappears before its row
/// is dropped. Long enough to absorb a brief listing flap, short enough that
/// closing an agent pane visibly clears it from the sidebar.
pub const EXITED_PRUNE_MS: u64 = 10 * 1000;
/// Non-terminal entries that have no known pane and have not reported for
/// this long are treated as abandoned (an external agent that crashed
/// without sending a terminal event).
pub const STUCK_PRUNE_MS: u64 = 30 * 60 * 1000;

/// Where a row's facts come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Registry,
    Http,
    Transcript,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PaneBinding {
    None,
    Explicit(String),
    Heuristic(String),
}

impl PaneBinding {
    fn pane_id(&self) -> Option<&str> {
        match self {
            Self::None => None,
            Self::Explicit(pane) | Self::Heuristic(pane) => Some(pane),
        }
    }
}

/// One live Claude Code registry record already resolved to a pane of this
/// mux (see `claude_registry::RegistryResolver`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryInput {
    pub thread_id: String,
    pub session: String,
    pub pane_id: String,
    pub pid: u32,
    pub status: RegistryStatus,
    pub waiting_for: Option<String>,
    pub status_updated_at: Option<u64>,
    pub started_at: Option<u64>,
    pub name: Option<String>,
    /// Teammate agent name, when the process was spawned by another session.
    pub agent_name: Option<String>,
}

/// What the tail of a transcript says, used to refine registry-sourced rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LastEntry {
    UserPrompt,
    ToolResult,
    AssistantToolUse,
    AssistantEndTurn,
    Interrupted,
    ApiError,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptHint {
    pub thread_name: Option<String>,
    pub last_user_prompt: Option<String>,
    pub last_entry: LastEntry,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RegistryObservation {
    status: RegistryStatus,
    status_updated_at: Option<u64>,
    pid: u32,
}

#[derive(Debug, Clone)]
struct TrackedAgent {
    event: AgentEvent,
    source: Source,
    binding: PaneBinding,
    /// When the row's pane or process was first observed gone.
    gone_since: Option<u64>,
    registry: Option<RegistryObservation>,
    hint: Option<TranscriptHint>,
    /// Name supplied by the registry, which outranks transcript titles.
    registry_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct InstanceId {
    session: String,
    key: String,
}

#[derive(Debug, Clone, Default)]
pub struct AgentTracker {
    instances: HashMap<InstanceId, TrackedAgent>,
    event_timestamps: HashMap<String, Vec<u64>>,
    unseen: HashSet<InstanceId>,
}

impl AgentTracker {
    pub fn new() -> Self {
        Self::default()
    }

    // ----- reads -----------------------------------------------------------

    pub fn get_state(&self, session: &str) -> Option<AgentEvent> {
        self.instances
            .iter()
            .filter(|(id, _)| id.session == session)
            .map(|(id, tracked)| self.serialize(id, tracked))
            .max_by_key(|event| status_priority(event.status))
    }

    pub fn get_agents(&self, session: &str) -> Vec<AgentEvent> {
        let mut agents = self
            .instances
            .iter()
            .filter(|(id, _)| id.session == session)
            .map(|(id, tracked)| self.serialize(id, tracked))
            .collect::<Vec<_>>();
        agents.sort_by(|a, b| b.ts.cmp(&a.ts).then_with(|| a.thread_id.cmp(&b.thread_id)));
        agents
    }

    pub fn get_event_timestamps(&self, session: &str) -> Vec<u64> {
        self.event_timestamps
            .get(session)
            .cloned()
            .unwrap_or_default()
    }

    pub fn is_unseen(&self, session: &str) -> bool {
        self.unseen.iter().any(|id| id.session == session)
    }

    pub fn get_unseen(&self) -> Vec<String> {
        self.unseen
            .iter()
            .map(|id| id.session.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    /// Pane a row is bound to, if that pane is currently believed alive.
    pub fn live_pane_for(&self, session: &str, agent: &str, thread_id: &str) -> Option<String> {
        let id = InstanceId {
            session: session.to_string(),
            key: instance_key(agent, Some(thread_id)),
        };
        let tracked = self.instances.get(&id)?;
        if tracked.gone_since.is_some() {
            return None;
        }
        tracked.binding.pane_id().map(str::to_string)
    }

    fn serialize(&self, id: &InstanceId, tracked: &TrackedAgent) -> AgentEvent {
        let mut event = tracked.event.clone();
        event.unseen = self.unseen.contains(id).then_some(true);
        event
    }

    // ----- seen state ------------------------------------------------------

    pub fn mark_seen(&mut self, session: &str) -> bool {
        let before = self.unseen.len();
        self.unseen.retain(|id| id.session != session);
        self.unseen.len() != before
    }

    pub fn mark_agent_seen(
        &mut self,
        session: &str,
        agent: &str,
        thread_id: Option<&str>,
        pane_id: Option<&str>,
    ) -> bool {
        let ids = self
            .instances
            .iter()
            .filter(|(id, tracked)| {
                id.session == session
                    && tracked.event.agent == agent
                    && (thread_id.is_some_and(|thread_id| {
                        tracked.event.thread_id.as_deref() == Some(thread_id)
                    }) || pane_id
                        .is_some_and(|pane_id| tracked.binding.pane_id() == Some(pane_id))
                        || (thread_id.is_none() && tracked.event.thread_id.is_none()))
            })
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        let mut changed = false;
        for id in ids {
            changed = self.unseen.remove(&id) || changed;
        }
        changed
    }

    /// The user is looking at `pane_id` in `session`: every row bound to it
    /// is seen.
    pub fn mark_pane_seen(&mut self, session: &str, pane_id: &str) -> bool {
        let ids = self
            .instances
            .iter()
            .filter(|(id, tracked)| {
                id.session == session && tracked.binding.pane_id() == Some(pane_id)
            })
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        let mut changed = false;
        for id in ids {
            changed = self.unseen.remove(&id) || changed;
        }
        changed
    }

    pub fn dismiss(&mut self, session: &str, agent: &str, thread_id: Option<&str>) -> bool {
        let id = InstanceId {
            session: session.to_string(),
            key: instance_key(agent, thread_id),
        };
        self.remove(&id)
    }

    // ----- sources ---------------------------------------------------------

    /// Apply an `/api/agent-event` post.
    pub fn apply_event(&mut self, event: AgentEvent) {
        self.apply_event_from(event, Source::Http);
    }

    /// Apply an event from `source`. Transcript events never override a
    /// registry-sourced row: the registry already knows more than the
    /// transcript, which only contributes a `TranscriptHint`.
    pub fn apply_event_from(&mut self, mut event: AgentEvent, source: Source) -> bool {
        let id = InstanceId {
            session: event.session.clone(),
            key: instance_key(&event.agent, event.thread_id.as_deref()),
        };
        if source == Source::Transcript
            && self
                .find_by_key(&id.key)
                .is_some_and(|(_, tracked)| tracked.source == Source::Registry)
        {
            return false;
        }
        event.unseen = None;
        let terminal = is_terminal_status(event.status);
        let explicit_pane = event.pane_id.clone();
        let previous = self.instances.remove(&id).or_else(|| {
            // A row that moved sessions (HTTP events name the session on each
            // post) keeps its history.
            let moved = self
                .find_by_key(&id.key)
                .filter(|(other, tracked)| {
                    other.session != id.session && tracked.event.thread_id.is_some()
                })
                .map(|(other, _)| other.clone());
            moved.and_then(|other| {
                let unseen = self.unseen.remove(&other);
                let tracked = self.instances.remove(&other);
                if unseen {
                    self.unseen.insert(id.clone());
                }
                tracked
            })
        });

        let mut tracked = match previous {
            Some(mut previous) => {
                if event.thread_name.is_none() {
                    event.thread_name = previous.event.thread_name.take();
                }
                if event.last_user_prompt.is_none() {
                    event.last_user_prompt = previous.event.last_user_prompt.take();
                }
                previous.event = event;
                previous.source = source;
                previous
            }
            None => TrackedAgent {
                event,
                source,
                binding: PaneBinding::None,
                gone_since: None,
                registry: None,
                hint: None,
                registry_name: None,
            },
        };
        if let Some(pane_id) = explicit_pane {
            // The source vouches for the pane; `sync_panes` confirms it is
            // still there before the row counts as alive.
            tracked.binding = PaneBinding::Explicit(pane_id);
            tracked.gone_since = None;
        }
        tracked.event.pane_id = tracked.binding.pane_id().map(str::to_string);
        tracked.event.liveness = tracked
            .binding
            .pane_id()
            .map(|_| AgentLiveness::Alive)
            .or(tracked.event.liveness);
        if tracked.source != Source::Registry {
            tracked.registry = None;
        }
        let ts = tracked.event.ts;
        let session = id.session.clone();
        self.instances.insert(id.clone(), tracked);
        self.record_timestamp(&session, ts);
        if terminal {
            self.unseen.insert(id);
        } else {
            self.unseen.remove(&id);
        }
        true
    }

    /// Apply this tick's live Claude Code registry records. Registry rows
    /// absent from `inputs` have lost their process (or their pane is no
    /// longer ours) and become gone.
    pub fn apply_registry(&mut self, inputs: Vec<RegistryInput>, now: u64) -> bool {
        let mut changed = false;
        let mut seen_keys = HashSet::new();

        // Two records for one session id (a resumed session still shown by
        // its old process): the newest process wins.
        let mut by_thread = HashMap::<String, RegistryInput>::new();
        for input in inputs {
            match by_thread.get(&input.thread_id) {
                Some(existing) if existing.started_at >= input.started_at => {}
                _ => {
                    by_thread.insert(input.thread_id.clone(), input);
                }
            }
        }

        for (_, input) in by_thread {
            let key = instance_key(CLAUDE_CODE_AGENT, Some(&input.thread_id));
            seen_keys.insert(key.clone());
            let id = InstanceId {
                session: input.session.clone(),
                key: key.clone(),
            };

            // The same process now runs a different session (`/clear`): the
            // old row is not coming back.
            let replaced = self
                .instances
                .iter()
                .filter(|(other, tracked)| {
                    other.key != key
                        && tracked
                            .registry
                            .is_some_and(|observation| observation.pid == input.pid)
                })
                .map(|(other, _)| other.clone())
                .collect::<Vec<_>>();
            for other in replaced {
                changed = self.remove(&other) || changed;
            }

            let existing = self.find_by_key(&key).map(|(other, _)| other.clone());
            let previous = match existing {
                Some(other) if other != id => {
                    // The pane moved to another session: relocate, keeping
                    // the unseen marker.
                    let unseen = self.unseen.remove(&other);
                    let tracked = self.instances.remove(&other);
                    if unseen {
                        self.unseen.insert(id.clone());
                    }
                    changed = true;
                    tracked
                }
                Some(other) => self.instances.remove(&other),
                None => None,
            };

            let ts = input.status_updated_at.map(|ts| ts.min(now)).unwrap_or(now);
            let observation = RegistryObservation {
                status: input.status,
                status_updated_at: input.status_updated_at,
                pid: input.pid,
            };
            let name = input.name.clone().or_else(|| input.agent_name.clone());

            let mut tracked = match previous {
                Some(mut tracked) => {
                    let before = tracked.registry;
                    if let Some(transition) = registry_transition(before, observation) {
                        if transition.status != tracked.event.status
                            || transition.detail != tracked.event.detail
                        {
                            changed = true;
                        }
                        tracked.event.status = transition.status;
                        tracked.event.detail = transition.detail;
                        tracked.event.ts = ts;
                        if !is_terminal_status(transition.status) {
                            self.unseen.remove(&id);
                        } else if transition.turn_ended {
                            self.unseen.insert(id.clone());
                        }
                        self.record_timestamp(&id.session, ts);
                    }
                    if input.status == RegistryStatus::Waiting
                        && tracked.event.detail != input.waiting_for
                    {
                        tracked.event.detail = input.waiting_for.clone();
                        changed = true;
                    }
                    tracked
                }
                None => {
                    let Transition { status, detail, .. } = first_registry_status(observation);
                    let event = AgentEvent {
                        agent: CLAUDE_CODE_AGENT.to_string(),
                        session: input.session.clone(),
                        status,
                        ts,
                        thread_id: Some(input.thread_id.clone()),
                        thread_name: None,
                        last_user_prompt: None,
                        unseen: None,
                        pane_id: None,
                        liveness: None,
                        detail,
                    };
                    self.record_timestamp(&id.session, ts);
                    changed = true;
                    TrackedAgent {
                        event,
                        source: Source::Registry,
                        binding: PaneBinding::None,
                        gone_since: None,
                        registry: None,
                        hint: None,
                        registry_name: None,
                    }
                }
            };
            if input.status == RegistryStatus::Waiting {
                tracked.event.detail = input.waiting_for.clone();
            }
            tracked.registry = Some(observation);
            tracked.source = Source::Registry;
            if tracked.binding != PaneBinding::Explicit(input.pane_id.clone()) {
                tracked.binding = PaneBinding::Explicit(input.pane_id.clone());
                changed = true;
            }
            if tracked.gone_since.is_some() {
                tracked.gone_since = None;
                changed = true;
            }
            tracked.event.pane_id = Some(input.pane_id.clone());
            tracked.event.liveness = Some(AgentLiveness::Alive);
            if tracked.registry_name != name {
                tracked.registry_name = name;
                changed = true;
            }
            tracked.event.session = input.session.clone();
            refine(&mut tracked);
            self.instances.insert(id, tracked);
        }

        // Registry rows without a live record this tick are gone.
        for tracked in self.instances.values_mut() {
            if tracked.source == Source::Registry
                && tracked.event.thread_id.as_deref().is_some_and(|thread_id| {
                    !seen_keys.contains(&instance_key(CLAUDE_CODE_AGENT, Some(thread_id)))
                })
                && tracked.gone_since.is_none()
            {
                mark_gone(tracked, now);
                changed = true;
            }
        }
        changed
    }

    /// Refine a registry row with what its transcript tail says: the name
    /// and prompt for display, and the running/done sub-state.
    pub fn apply_transcript_hint(&mut self, thread_id: &str, hint: TranscriptHint) -> bool {
        let key = instance_key(CLAUDE_CODE_AGENT, Some(thread_id));
        let Some((id, _)) = self.find_by_key(&key) else {
            return false;
        };
        let id = id.clone();
        let tracked = self.instances.get_mut(&id).expect("row exists");
        if tracked.source != Source::Registry {
            return false;
        }
        if tracked.hint.as_ref() == Some(&hint) {
            return false;
        }
        tracked.hint = Some(hint);
        let before = tracked.event.clone();
        refine(tracked);
        let terminal_now = is_terminal_status(tracked.event.status);
        let changed = tracked.event != before;
        if changed && terminal_now != is_terminal_status(before.status) {
            if terminal_now {
                self.unseen.insert(id);
            } else {
                self.unseen.remove(&id);
            }
        }
        changed
    }

    // ----- mux sync --------------------------------------------------------

    /// Reconcile rows with what the mux shows right now: pane liveness,
    /// heuristic pane binding for pane-less rows, the seen rule, and
    /// bookkeeping for sessions that no longer exist.
    pub fn sync_panes(
        &mut self,
        panes: &[MuxPane],
        client_focus: &[ClientFocus],
        live_sessions: &HashSet<String>,
        now: u64,
    ) -> bool {
        let mut changed = false;
        let pane_sessions = panes
            .iter()
            .map(|pane| (pane.pane_id.as_str(), pane.session_name.as_str()))
            .collect::<HashMap<_, _>>();

        // 1. Liveness of bound panes; rows of vanished sessions are gone.
        let mut relocations = Vec::new();
        for (id, tracked) in self.instances.iter_mut() {
            let session_alive = live_sessions.contains(&id.session);
            match tracked.binding.pane_id() {
                Some(pane_id) => match pane_sessions.get(pane_id) {
                    Some(session) if *session != id.session => {
                        relocations.push((id.clone(), (*session).to_string()));
                    }
                    Some(_) => {
                        // A registry row is alive only while its process
                        // is: the pane outliving the process (a shell prompt
                        // after /exit) does not revive it.
                        if tracked.gone_since.is_some() && tracked.source != Source::Registry {
                            tracked.gone_since = None;
                            changed = true;
                        }
                        if tracked.gone_since.is_none()
                            && tracked.event.liveness != Some(AgentLiveness::Alive)
                        {
                            tracked.event.liveness = Some(AgentLiveness::Alive);
                            changed = true;
                        }
                    }
                    None => {
                        if tracked.gone_since.is_none() {
                            mark_gone(tracked, now);
                            if matches!(tracked.binding, PaneBinding::Heuristic(_)) {
                                tracked.binding = PaneBinding::None;
                            }
                            changed = true;
                        }
                    }
                },
                None => {
                    if !session_alive && tracked.gone_since.is_none() {
                        mark_gone(tracked, now);
                        changed = true;
                    }
                }
            }
        }
        for (id, session) in relocations {
            let unseen = self.unseen.remove(&id);
            if let Some(mut tracked) = self.instances.remove(&id) {
                tracked.event.session = session.clone();
                let new_id = InstanceId {
                    session,
                    key: id.key,
                };
                if unseen {
                    self.unseen.insert(new_id.clone());
                }
                self.instances.insert(new_id, tracked);
                changed = true;
            }
        }
        self.event_timestamps
            .retain(|session, _| live_sessions.contains(session));

        // 2. Heuristic binding for pane-less rows.
        changed = self.bind_heuristically(panes) || changed;

        // 3. Seen rule: rows bound to a pane an attached client is looking at.
        for focus in client_focus {
            changed = self.mark_pane_seen(&focus.session_name, &focus.pane_id) || changed;
        }
        changed
    }

    fn bind_heuristically(&mut self, panes: &[MuxPane]) -> bool {
        let mut changed = false;
        let sessions = panes
            .iter()
            .map(|pane| pane.session_name.clone())
            .collect::<BTreeSet<_>>();
        for session in sessions {
            let session_panes = panes
                .iter()
                .filter(|pane| pane.session_name == session)
                .cloned()
                .collect::<Vec<_>>();
            let agent_panes = agent_panes_from(&session_panes);
            if agent_panes.is_empty() {
                continue;
            }
            let claimed = self
                .instances
                .iter()
                .filter(|(id, _)| id.session == session)
                .filter_map(|(_, tracked)| tracked.binding.pane_id().map(str::to_string))
                .collect::<HashSet<_>>();
            let agents = agent_panes
                .iter()
                .map(|pane| pane.agent.clone())
                .collect::<BTreeSet<_>>();
            for agent in agents {
                let free_panes = agent_panes
                    .iter()
                    .filter(|pane| pane.agent == agent && !claimed.contains(&pane.pane_id))
                    .collect::<Vec<_>>();
                if free_panes.is_empty() {
                    continue;
                }
                let candidates = self
                    .instances
                    .iter()
                    .filter(|(id, tracked)| {
                        id.session == session
                            && tracked.event.agent == agent
                            && tracked.source != Source::Registry
                            && tracked.binding == PaneBinding::None
                            && tracked.gone_since.is_none()
                    })
                    .map(|(id, tracked)| (id.clone(), tracked.event.thread_name.clone()))
                    .collect::<Vec<_>>();
                if candidates.is_empty() {
                    continue;
                }
                let mut bound = Vec::new();
                // Panes that carry a thread name (Amp titles) bind by name.
                for pane in &free_panes {
                    let Some(pane_name) = pane.thread_name.as_deref() else {
                        continue;
                    };
                    let named = candidates
                        .iter()
                        .filter(|(_, name)| name.as_deref() == Some(pane_name))
                        .map(|(id, _)| id.clone())
                        .collect::<Vec<_>>();
                    if named.len() == 1 {
                        bound.push((named[0].clone(), pane.pane_id.clone()));
                    }
                }
                let bound_ids = bound
                    .iter()
                    .map(|(id, _)| id.clone())
                    .collect::<HashSet<_>>();
                let bound_panes = bound
                    .iter()
                    .map(|(_, pane)| pane.clone())
                    .collect::<HashSet<_>>();
                let remaining_panes = free_panes
                    .iter()
                    .filter(|pane| !bound_panes.contains(&pane.pane_id))
                    .collect::<Vec<_>>();
                let remaining_candidates = candidates
                    .iter()
                    .filter(|(id, _)| !bound_ids.contains(id))
                    .collect::<Vec<_>>();
                // Otherwise only an unambiguous 1:1 match binds (F036).
                if remaining_panes.len() == 1 && remaining_candidates.len() == 1 {
                    bound.push((
                        remaining_candidates[0].0.clone(),
                        remaining_panes[0].pane_id.clone(),
                    ));
                }
                for (id, pane_id) in bound {
                    if let Some(tracked) = self.instances.get_mut(&id) {
                        tracked.binding = PaneBinding::Heuristic(pane_id.clone());
                        tracked.event.pane_id = Some(pane_id);
                        tracked.event.liveness = Some(AgentLiveness::Alive);
                        changed = true;
                    }
                }
            }
        }
        changed
    }

    // ----- pruning ---------------------------------------------------------

    /// Reap rows that are gone or abandoned. Returns whether anything was
    /// removed.
    pub fn prune(&mut self, now: u64) -> bool {
        let doomed = self
            .instances
            .iter()
            .filter(|(id, tracked)| {
                if let Some(gone_since) = tracked.gone_since {
                    return now.saturating_sub(gone_since) > EXITED_PRUNE_MS;
                }
                if tracked.binding.pane_id().is_some() {
                    return false;
                }
                let silence = now.saturating_sub(tracked.event.ts);
                if is_terminal_status(tracked.event.status) {
                    let ttl = if self.unseen.contains(id) {
                        UNSEEN_TERMINAL_PRUNE_MS
                    } else {
                        TERMINAL_PRUNE_MS
                    };
                    silence > ttl
                } else {
                    silence > STUCK_PRUNE_MS
                }
            })
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        let changed = !doomed.is_empty();
        for id in doomed {
            self.remove(&id);
        }
        changed
    }

    // ----- helpers ---------------------------------------------------------

    fn find_by_key(&self, key: &str) -> Option<(&InstanceId, &TrackedAgent)> {
        self.instances.iter().find(|(id, _)| id.key == key)
    }

    fn remove(&mut self, id: &InstanceId) -> bool {
        let removed = self.instances.remove(id).is_some();
        self.unseen.remove(id);
        removed
    }

    fn record_timestamp(&mut self, session: &str, ts: u64) {
        let timestamps = self
            .event_timestamps
            .entry(session.to_string())
            .or_default();
        timestamps.push(ts);
        if timestamps.len() > MAX_EVENT_TIMESTAMPS {
            timestamps.drain(0..timestamps.len() - MAX_EVENT_TIMESTAMPS);
        }
    }
}

fn mark_gone(tracked: &mut TrackedAgent, now: u64) {
    tracked.gone_since = Some(now);
    tracked.event.liveness = Some(AgentLiveness::Exited);
    tracked.event.pane_id = None;
}

const SHELL_DETAIL: &str = "shell running";

/// A status change derived from registry observations.
struct Transition {
    status: AgentStatus,
    detail: Option<String>,
    /// A turn just ended: the row deserves the attention marker. False for
    /// changes that only refine an already-ended turn.
    turn_ended: bool,
}

impl Transition {
    fn to(status: AgentStatus, detail: Option<&str>, turn_ended: bool) -> Self {
        Self {
            status,
            detail: detail.map(str::to_string),
            turn_ended,
        }
    }
}

/// Status of a row seen for the first time: nothing is "done" yet, the
/// user has not been kept waiting for anything they have not seen.
fn first_registry_status(observation: RegistryObservation) -> Transition {
    match observation.status {
        RegistryStatus::Busy => Transition::to(AgentStatus::Running, None, false),
        RegistryStatus::Waiting => Transition::to(AgentStatus::Waiting, None, false),
        RegistryStatus::Idle => Transition::to(AgentStatus::Idle, None, false),
        RegistryStatus::Shell => Transition::to(AgentStatus::Idle, Some(SHELL_DETAIL), false),
    }
}

/// The status change implied by two consecutive registry observations, or
/// `None` when nothing happened.
fn registry_transition(
    before: Option<RegistryObservation>,
    after: RegistryObservation,
) -> Option<Transition> {
    let Some(before) = before else {
        return Some(first_registry_status(after));
    };
    let turn_was_active = matches!(
        before.status,
        RegistryStatus::Busy | RegistryStatus::Waiting
    );
    // A `busy → idle → …` round trip between two polls still moves
    // `statusUpdatedAt`, so a same-status observation with a newer stamp is
    // a transition too.
    let stamp_advanced = match (before.status_updated_at, after.status_updated_at) {
        (Some(before), Some(after)) => after > before,
        _ => false,
    };
    match after.status {
        RegistryStatus::Busy => (before.status != RegistryStatus::Busy)
            .then(|| Transition::to(AgentStatus::Running, None, false)),
        RegistryStatus::Waiting => (before.status != RegistryStatus::Waiting)
            .then(|| Transition::to(AgentStatus::Waiting, None, false)),
        RegistryStatus::Idle => {
            if turn_was_active || (before.status == RegistryStatus::Idle && stamp_advanced) {
                Some(Transition::to(AgentStatus::Done, None, true))
            } else if before.status == RegistryStatus::Shell {
                // The turn ended when the row became `shell`; only the
                // background command finished now.
                Some(Transition::to(AgentStatus::Done, None, false))
            } else {
                None
            }
        }
        RegistryStatus::Shell => {
            if turn_was_active || before.status == RegistryStatus::Idle {
                // From idle, a background command can only have been started
                // by a turn that ran entirely between two polls.
                Some(Transition::to(AgentStatus::Done, Some(SHELL_DETAIL), true))
            } else {
                None
            }
        }
    }
}

/// Apply the transcript hint on top of the registry status.
fn refine(tracked: &mut TrackedAgent) {
    if let Some(name) = tracked.registry_name.clone() {
        tracked.event.thread_name = Some(name);
    } else if let Some(name) = tracked
        .hint
        .as_ref()
        .and_then(|hint| hint.thread_name.clone())
    {
        tracked.event.thread_name = Some(name);
    }
    let Some(hint) = tracked.hint.as_ref() else {
        return;
    };
    if let Some(prompt) = hint.last_user_prompt.clone() {
        tracked.event.last_user_prompt = Some(prompt);
    }
    let Some(observation) = tracked.registry else {
        return;
    };
    match observation.status {
        RegistryStatus::Busy => match hint.last_entry {
            LastEntry::AssistantToolUse => {
                tracked.event.status = AgentStatus::ToolRunning;
                tracked.event.detail = None;
            }
            LastEntry::AssistantEndTurn => {
                tracked.event.status = AgentStatus::Running;
                tracked.event.detail = Some("delegating".to_string());
            }
            _ => {
                tracked.event.status = AgentStatus::Running;
                tracked.event.detail = None;
            }
        },
        RegistryStatus::Idle | RegistryStatus::Shell
            if matches!(
                tracked.event.status,
                AgentStatus::Done | AgentStatus::Interrupted | AgentStatus::Error
            ) =>
        {
            tracked.event.status = match hint.last_entry {
                LastEntry::Interrupted => AgentStatus::Interrupted,
                LastEntry::ApiError => AgentStatus::Error,
                _ => AgentStatus::Done,
            };
        }
        _ => {}
    }
}

pub fn instance_key(agent: &str, thread_id: Option<&str>) -> String {
    match thread_id {
        Some(thread_id) => format!("{agent}:{thread_id}"),
        None => agent.to_string(),
    }
}

fn is_terminal_status(status: AgentStatus) -> bool {
    matches!(
        status,
        AgentStatus::Done | AgentStatus::Error | AgentStatus::Interrupted | AgentStatus::Stale
    )
}

fn status_priority(status: AgentStatus) -> u8 {
    match status {
        AgentStatus::ToolRunning => 7,
        AgentStatus::Running => 6,
        AgentStatus::Error => 5,
        AgentStatus::Stale => 4,
        AgentStatus::Interrupted => 3,
        AgentStatus::Waiting => 2,
        AgentStatus::Done => 1,
        AgentStatus::Idle => 0,
    }
}

/// Agent Pane Presence for one session, for callers that still need the
/// per-session view (Enter/x routing).
pub fn agent_panes_in(panes: &[MuxPane], session: &str) -> Vec<AgentPane> {
    agent_panes_from(
        &panes
            .iter()
            .filter(|pane| pane.session_name == session)
            .cloned()
            .collect::<Vec<_>>(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_789_315_528_128;

    fn event(
        agent: &str,
        session: &str,
        thread_id: Option<&str>,
        thread_name: Option<&str>,
    ) -> AgentEvent {
        AgentEvent {
            agent: agent.to_string(),
            session: session.to_string(),
            status: AgentStatus::Running,
            ts: 1,
            thread_id: thread_id.map(str::to_string),
            thread_name: thread_name.map(str::to_string),
            last_user_prompt: None,
            unseen: None,
            pane_id: None,
            liveness: None,
            detail: None,
        }
    }

    fn terminal_event(
        agent: &str,
        session: &str,
        thread_id: Option<&str>,
        thread_name: Option<&str>,
        pane_id: Option<&str>,
    ) -> AgentEvent {
        let mut event = event(agent, session, thread_id, thread_name);
        event.status = AgentStatus::Done;
        event.pane_id = pane_id.map(str::to_string);
        event
    }

    fn pane(pane_id: &str, session: &str, command: &str, title: &str) -> MuxPane {
        MuxPane {
            pane_id: pane_id.to_string(),
            session_name: session.to_string(),
            window_id: "@1".to_string(),
            window_active: true,
            active: false,
            pid: 100,
            command: command.to_string(),
            title: title.to_string(),
            dead: false,
        }
    }

    fn shell(pane_id: &str, session: &str) -> MuxPane {
        pane(pane_id, session, "bash", "shell")
    }

    fn focus(session: &str, pane_id: &str) -> ClientFocus {
        ClientFocus {
            client_tty: Some("/dev/pts/1".to_string()),
            session_name: session.to_string(),
            window_id: "@1".to_string(),
            pane_id: pane_id.to_string(),
        }
    }

    fn sessions(names: &[&str]) -> HashSet<String> {
        names.iter().map(|name| (*name).to_string()).collect()
    }

    fn registry(
        thread_id: &str,
        session: &str,
        pane_id: &str,
        status: RegistryStatus,
        stamp: u64,
    ) -> RegistryInput {
        RegistryInput {
            thread_id: thread_id.to_string(),
            session: session.to_string(),
            pane_id: pane_id.to_string(),
            pid: 4242,
            status,
            waiting_for: None,
            status_updated_at: Some(stamp),
            started_at: Some(NOW - 60_000),
            name: Some("app".to_string()),
            agent_name: None,
        }
    }

    fn only_agent(tracker: &AgentTracker, session: &str) -> AgentEvent {
        let agents = tracker.get_agents(session);
        assert_eq!(agents.len(), 1, "expected one row, got {agents:?}");
        agents.into_iter().next().unwrap()
    }

    // ----- registry -----

    #[test]
    fn registry_busy_record_is_a_running_row_bound_to_its_pane() {
        let mut tracker = AgentTracker::new();

        tracker.apply_registry(
            vec![registry(
                "s1",
                "work",
                "%7",
                RegistryStatus::Busy,
                NOW - 5_000,
            )],
            NOW,
        );

        let row = only_agent(&tracker, "work");
        assert_eq!(row.agent, "claude-code");
        assert_eq!(row.thread_id.as_deref(), Some("s1"));
        assert_eq!(row.thread_name.as_deref(), Some("app"));
        assert_eq!(row.status, AgentStatus::Running);
        assert_eq!(row.pane_id.as_deref(), Some("%7"));
        assert_eq!(row.liveness, Some(AgentLiveness::Alive));
        assert_eq!(row.ts, NOW - 5_000);
        assert_eq!(row.unseen, None);
        assert_eq!(
            tracker
                .live_pane_for("work", "claude-code", "s1")
                .as_deref(),
            Some("%7")
        );
    }

    #[test]
    fn registry_first_observation_of_an_idle_session_is_a_seen_idle_row() {
        let mut tracker = AgentTracker::new();

        tracker.apply_registry(
            vec![registry(
                "s1",
                "work",
                "%7",
                RegistryStatus::Idle,
                NOW - 60_000,
            )],
            NOW,
        );

        let row = only_agent(&tracker, "work");
        assert_eq!(row.status, AgentStatus::Idle);
        assert_eq!(row.unseen, None);
        assert!(!tracker.is_unseen("work"));
    }

    #[test]
    fn registry_busy_to_idle_is_done_and_unseen_until_the_pane_is_looked_at() {
        let mut tracker = AgentTracker::new();
        tracker.apply_registry(
            vec![registry(
                "s1",
                "work",
                "%7",
                RegistryStatus::Busy,
                NOW - 5_000,
            )],
            NOW,
        );

        let changed = tracker.apply_registry(
            vec![registry(
                "s1",
                "work",
                "%7",
                RegistryStatus::Idle,
                NOW - 1_000,
            )],
            NOW,
        );

        assert!(changed);
        let row = only_agent(&tracker, "work");
        assert_eq!(row.status, AgentStatus::Done);
        assert_eq!(row.unseen, Some(true));
        assert_eq!(row.ts, NOW - 1_000);
        assert!(tracker.is_unseen("work"));

        // Looking at another pane changes nothing; looking at its pane does.
        tracker.sync_panes(
            &[shell("%7", "work"), shell("%8", "work")],
            &[focus("work", "%8")],
            &sessions(&["work"]),
            NOW,
        );
        assert!(tracker.is_unseen("work"));
        tracker.sync_panes(
            &[shell("%7", "work"), shell("%8", "work")],
            &[focus("work", "%7")],
            &sessions(&["work"]),
            NOW,
        );
        assert!(!tracker.is_unseen("work"));
        assert_eq!(only_agent(&tracker, "work").unseen, None);
        assert_eq!(only_agent(&tracker, "work").status, AgentStatus::Done);
    }

    #[test]
    fn registry_transition_missed_between_polls_is_detected_by_the_status_stamp() {
        let mut tracker = AgentTracker::new();
        tracker.apply_registry(
            vec![registry(
                "s1",
                "work",
                "%7",
                RegistryStatus::Idle,
                NOW - 60_000,
            )],
            NOW,
        );

        // busy → idle happened entirely between two polls.
        tracker.apply_registry(
            vec![registry(
                "s1",
                "work",
                "%7",
                RegistryStatus::Idle,
                NOW - 400,
            )],
            NOW,
        );

        let row = only_agent(&tracker, "work");
        assert_eq!(row.status, AgentStatus::Done);
        assert_eq!(row.unseen, Some(true));

        // The same stamp again is not a new transition.
        let changed = tracker.apply_registry(
            vec![registry(
                "s1",
                "work",
                "%7",
                RegistryStatus::Idle,
                NOW - 400,
            )],
            NOW + 500,
        );
        assert!(!changed);
        assert_eq!(only_agent(&tracker, "work").status, AgentStatus::Done);
    }

    #[test]
    fn registry_waiting_carries_its_reason_as_detail() {
        let mut tracker = AgentTracker::new();
        tracker.apply_registry(
            vec![registry(
                "s1",
                "work",
                "%7",
                RegistryStatus::Busy,
                NOW - 5_000,
            )],
            NOW,
        );

        let mut waiting = registry("s1", "work", "%7", RegistryStatus::Waiting, NOW - 1_000);
        waiting.waiting_for = Some("dialog open".to_string());
        tracker.apply_registry(vec![waiting], NOW);

        let row = only_agent(&tracker, "work");
        assert_eq!(row.status, AgentStatus::Waiting);
        assert_eq!(row.detail.as_deref(), Some("dialog open"));
        assert_eq!(row.unseen, None);

        // Approval given: the turn continues without any attention marker.
        tracker.apply_registry(
            vec![registry(
                "s1",
                "work",
                "%7",
                RegistryStatus::Busy,
                NOW - 500,
            )],
            NOW,
        );
        let row = only_agent(&tracker, "work");
        assert_eq!(row.status, AgentStatus::Running);
        assert_eq!(row.detail, None);
    }

    #[test]
    fn registry_shell_after_a_turn_is_done_with_a_shell_detail() {
        let mut tracker = AgentTracker::new();
        tracker.apply_registry(
            vec![registry(
                "s1",
                "work",
                "%7",
                RegistryStatus::Busy,
                NOW - 5_000,
            )],
            NOW,
        );

        tracker.apply_registry(
            vec![registry(
                "s1",
                "work",
                "%7",
                RegistryStatus::Shell,
                NOW - 1_000,
            )],
            NOW,
        );
        let row = only_agent(&tracker, "work");
        assert_eq!(row.status, AgentStatus::Done);
        assert_eq!(row.detail.as_deref(), Some("shell running"));
        assert_eq!(row.unseen, Some(true));

        // The user looks, then the background command finishes: still seen.
        tracker.mark_pane_seen("work", "%7");
        tracker.apply_registry(
            vec![registry(
                "s1",
                "work",
                "%7",
                RegistryStatus::Idle,
                NOW - 200,
            )],
            NOW,
        );
        let row = only_agent(&tracker, "work");
        assert_eq!(row.status, AgentStatus::Done);
        assert_eq!(row.detail, None);
        assert_eq!(row.unseen, None);
    }

    #[test]
    fn registry_row_whose_record_disappears_is_gone_and_reaped_after_the_grace_period() {
        let mut tracker = AgentTracker::new();
        tracker.apply_registry(
            vec![registry(
                "s1",
                "work",
                "%7",
                RegistryStatus::Busy,
                NOW - 5_000,
            )],
            NOW,
        );

        let changed = tracker.apply_registry(vec![], NOW);

        assert!(changed);
        let row = only_agent(&tracker, "work");
        assert_eq!(row.liveness, Some(AgentLiveness::Exited));
        assert_eq!(row.pane_id, None);
        assert_eq!(row.status, AgentStatus::Running, "status is not guessed");
        assert!(!tracker.prune(NOW + EXITED_PRUNE_MS));
        assert!(tracker.prune(NOW + EXITED_PRUNE_MS + 1));
        assert!(tracker.get_agents("work").is_empty());
    }

    #[test]
    fn registry_row_stays_gone_while_its_pane_outlives_the_process() {
        let mut tracker = AgentTracker::new();
        tracker.apply_registry(
            vec![registry(
                "s1",
                "work",
                "%7",
                RegistryStatus::Busy,
                NOW - 5_000,
            )],
            NOW,
        );

        // /exit: the record is swept, the pane is back at a shell prompt.
        tracker.apply_registry(vec![], NOW);
        let changed =
            tracker.sync_panes(&[shell("%7", "work")], &[], &sessions(&["work"]), NOW + 500);

        assert!(!changed, "the surviving pane must not revive the row");
        assert_eq!(
            only_agent(&tracker, "work").liveness,
            Some(AgentLiveness::Exited)
        );
        assert!(
            !tracker.apply_registry(vec![], NOW + 1_000),
            "nothing new to report"
        );
        assert!(tracker.prune(NOW + EXITED_PRUNE_MS + 1));
        assert!(tracker.get_agents("work").is_empty());
    }

    #[test]
    fn registry_record_flapping_for_one_tick_does_not_lose_the_row() {
        let mut tracker = AgentTracker::new();
        tracker.apply_registry(
            vec![registry(
                "s1",
                "work",
                "%7",
                RegistryStatus::Busy,
                NOW - 5_000,
            )],
            NOW,
        );
        tracker.apply_registry(vec![], NOW);

        tracker.apply_registry(
            vec![registry(
                "s1",
                "work",
                "%7",
                RegistryStatus::Busy,
                NOW - 5_000,
            )],
            NOW + 500,
        );

        let row = only_agent(&tracker, "work");
        assert_eq!(row.liveness, Some(AgentLiveness::Alive));
        assert_eq!(row.pane_id.as_deref(), Some("%7"));
        assert!(!tracker.prune(NOW + EXITED_PRUNE_MS + 1_000));
    }

    #[test]
    fn registry_same_process_with_a_new_session_id_replaces_the_old_row_immediately() {
        let mut tracker = AgentTracker::new();
        tracker.apply_registry(
            vec![registry(
                "s1",
                "work",
                "%7",
                RegistryStatus::Busy,
                NOW - 5_000,
            )],
            NOW,
        );

        // `/clear`: same pid and pane, fresh session id.
        tracker.apply_registry(
            vec![registry(
                "s2",
                "work",
                "%7",
                RegistryStatus::Idle,
                NOW - 100,
            )],
            NOW,
        );

        let row = only_agent(&tracker, "work");
        assert_eq!(row.thread_id.as_deref(), Some("s2"));
        assert_eq!(row.status, AgentStatus::Idle);
        assert!(!tracker.is_unseen("work"));
    }

    #[test]
    fn registry_row_follows_its_pane_to_another_session_keeping_the_unseen_marker() {
        let mut tracker = AgentTracker::new();
        tracker.apply_registry(
            vec![registry(
                "s1",
                "work",
                "%7",
                RegistryStatus::Busy,
                NOW - 5_000,
            )],
            NOW,
        );
        tracker.apply_registry(
            vec![registry(
                "s1",
                "work",
                "%7",
                RegistryStatus::Idle,
                NOW - 1_000,
            )],
            NOW,
        );
        assert!(tracker.is_unseen("work"));

        tracker.apply_registry(
            vec![registry(
                "s1",
                "docs",
                "%7",
                RegistryStatus::Idle,
                NOW - 1_000,
            )],
            NOW,
        );

        assert!(tracker.get_agents("work").is_empty());
        let row = only_agent(&tracker, "docs");
        assert_eq!(row.session, "docs");
        assert_eq!(row.unseen, Some(true));
        assert!(!tracker.is_unseen("work"));
        assert!(tracker.is_unseen("docs"));
    }

    #[test]
    fn registry_names_outrank_transcript_titles_and_teammates_use_their_agent_name() {
        let mut tracker = AgentTracker::new();
        tracker.apply_registry(
            vec![registry(
                "s1",
                "work",
                "%7",
                RegistryStatus::Busy,
                NOW - 5_000,
            )],
            NOW,
        );
        tracker.apply_transcript_hint(
            "s1",
            TranscriptHint {
                thread_name: Some("first prompt".to_string()),
                last_user_prompt: Some("first prompt".to_string()),
                last_entry: LastEntry::UserPrompt,
            },
        );
        assert_eq!(
            only_agent(&tracker, "work").thread_name.as_deref(),
            Some("app")
        );
        assert_eq!(
            only_agent(&tracker, "work").last_user_prompt.as_deref(),
            Some("first prompt")
        );

        let mut unnamed = registry("s1", "work", "%7", RegistryStatus::Busy, NOW - 5_000);
        unnamed.name = None;
        tracker.apply_registry(vec![unnamed], NOW);
        assert_eq!(
            only_agent(&tracker, "work").thread_name.as_deref(),
            Some("first prompt")
        );

        let mut teammate = registry("s2", "work", "%8", RegistryStatus::Busy, NOW - 5_000);
        teammate.pid = 4300;
        teammate.name = None;
        teammate.agent_name = Some("researcher".to_string());
        let mut unnamed = registry("s1", "work", "%7", RegistryStatus::Busy, NOW - 5_000);
        unnamed.name = None;
        tracker.apply_registry(vec![unnamed, teammate], NOW);
        let names = tracker
            .get_agents("work")
            .into_iter()
            .filter_map(|row| row.thread_name)
            .collect::<BTreeSet<_>>();
        assert_eq!(
            names,
            BTreeSet::from(["first prompt".to_string(), "researcher".to_string()])
        );
    }

    #[test]
    fn transcript_hint_refines_running_into_tool_running_or_delegating() {
        let mut tracker = AgentTracker::new();
        tracker.apply_registry(
            vec![registry(
                "s1",
                "work",
                "%7",
                RegistryStatus::Busy,
                NOW - 5_000,
            )],
            NOW,
        );

        let hint = |last_entry| TranscriptHint {
            thread_name: None,
            last_user_prompt: None,
            last_entry,
        };
        tracker.apply_transcript_hint("s1", hint(LastEntry::AssistantToolUse));
        assert_eq!(
            only_agent(&tracker, "work").status,
            AgentStatus::ToolRunning
        );

        tracker.apply_transcript_hint("s1", hint(LastEntry::AssistantEndTurn));
        let row = only_agent(&tracker, "work");
        assert_eq!(row.status, AgentStatus::Running);
        assert_eq!(row.detail.as_deref(), Some("delegating"));

        tracker.apply_transcript_hint("s1", hint(LastEntry::UserPrompt));
        let row = only_agent(&tracker, "work");
        assert_eq!(row.status, AgentStatus::Running);
        assert_eq!(row.detail, None);

        // A later registry poll keeps the refinement.
        tracker.apply_transcript_hint("s1", hint(LastEntry::AssistantToolUse));
        tracker.apply_registry(
            vec![registry(
                "s1",
                "work",
                "%7",
                RegistryStatus::Busy,
                NOW - 5_000,
            )],
            NOW + 500,
        );
        assert_eq!(
            only_agent(&tracker, "work").status,
            AgentStatus::ToolRunning
        );
    }

    #[test]
    fn transcript_hint_refines_done_into_interrupted_or_error() {
        let mut tracker = AgentTracker::new();
        tracker.apply_registry(
            vec![registry(
                "s1",
                "work",
                "%7",
                RegistryStatus::Busy,
                NOW - 5_000,
            )],
            NOW,
        );
        tracker.apply_registry(
            vec![registry(
                "s1",
                "work",
                "%7",
                RegistryStatus::Idle,
                NOW - 1_000,
            )],
            NOW,
        );

        let hint = |last_entry| TranscriptHint {
            thread_name: None,
            last_user_prompt: None,
            last_entry,
        };
        tracker.apply_transcript_hint("s1", hint(LastEntry::Interrupted));
        let row = only_agent(&tracker, "work");
        assert_eq!(row.status, AgentStatus::Interrupted);
        assert_eq!(row.unseen, Some(true));

        tracker.apply_transcript_hint("s1", hint(LastEntry::ApiError));
        assert_eq!(only_agent(&tracker, "work").status, AgentStatus::Error);

        // While a new turn runs the hint does not turn the row terminal.
        tracker.apply_registry(
            vec![registry(
                "s1",
                "work",
                "%7",
                RegistryStatus::Busy,
                NOW - 500,
            )],
            NOW,
        );
        assert_eq!(only_agent(&tracker, "work").status, AgentStatus::Running);
        assert!(!tracker.is_unseen("work"));
    }

    #[test]
    fn transcript_events_never_override_a_registry_row() {
        let mut tracker = AgentTracker::new();
        tracker.apply_registry(
            vec![registry(
                "s1",
                "work",
                "%7",
                RegistryStatus::Busy,
                NOW - 5_000,
            )],
            NOW,
        );

        let mut stale = event("claude-code", "work", Some("s1"), Some("guessed"));
        stale.status = AgentStatus::Stale;
        let applied = tracker.apply_event_from(stale, Source::Transcript);

        assert!(!applied);
        let row = only_agent(&tracker, "work");
        assert_eq!(row.status, AgentStatus::Running);
        assert_eq!(row.thread_name.as_deref(), Some("app"));
        assert!(!tracker.is_unseen("work"));
    }

    // ----- explicit HTTP bindings -----

    #[test]
    fn explicit_pane_binding_survives_alias_absence_and_ends_when_the_pane_dies() {
        let mut tracker = AgentTracker::new();
        tracker.apply_event(terminal_event(
            "my-agent",
            "work",
            Some("task-1"),
            Some("Deploy"),
            Some("%7"),
        ));

        // The pane runs a plain shell as far as tmux can tell.
        tracker.sync_panes(&[shell("%7", "work")], &[], &sessions(&["work"]), NOW);
        let row = only_agent(&tracker, "work");
        assert_eq!(row.pane_id.as_deref(), Some("%7"));
        assert_eq!(row.liveness, Some(AgentLiveness::Alive));
        assert!(!tracker.prune(NOW + UNSEEN_TERMINAL_PRUNE_MS + 1));

        tracker.sync_panes(&[shell("%8", "work")], &[], &sessions(&["work"]), NOW);
        let row = only_agent(&tracker, "work");
        assert_eq!(row.liveness, Some(AgentLiveness::Exited));
        assert!(!tracker.prune(NOW + EXITED_PRUNE_MS));
        assert!(tracker.prune(NOW + EXITED_PRUNE_MS + 1));
    }

    #[test]
    fn a_pane_less_event_does_not_revive_a_row_whose_pane_is_gone() {
        let mut tracker = AgentTracker::new();
        tracker.apply_event(terminal_event(
            "claude-code",
            "work",
            Some("t1"),
            Some("Task"),
            Some("%7"),
        ));
        tracker.sync_panes(&[], &[], &sessions(&["work"]), NOW);
        assert_eq!(
            only_agent(&tracker, "work").liveness,
            Some(AgentLiveness::Exited)
        );

        let mut later = event("claude-code", "work", Some("t1"), None);
        later.status = AgentStatus::Stale;
        later.ts = NOW + 5_000;
        tracker.apply_event_from(later, Source::Transcript);

        assert!(tracker.prune(NOW + EXITED_PRUNE_MS + 1));
        assert!(tracker.get_agents("work").is_empty());
    }

    #[test]
    fn an_event_naming_a_live_pane_revives_a_gone_row() {
        let mut tracker = AgentTracker::new();
        tracker.apply_event(terminal_event(
            "my-agent",
            "work",
            Some("t1"),
            None,
            Some("%7"),
        ));
        tracker.sync_panes(&[], &[], &sessions(&["work"]), NOW);

        let mut again = event("my-agent", "work", Some("t1"), None);
        again.pane_id = Some("%9".to_string());
        tracker.apply_event(again);
        tracker.sync_panes(&[shell("%9", "work")], &[], &sessions(&["work"]), NOW + 100);

        let row = only_agent(&tracker, "work");
        assert_eq!(row.pane_id.as_deref(), Some("%9"));
        assert_eq!(row.liveness, Some(AgentLiveness::Alive));
        assert!(!tracker.prune(NOW + EXITED_PRUNE_MS + 1_000));
    }

    // ----- seen rule -----

    #[test]
    fn done_in_a_background_session_stays_unseen_until_its_pane_is_the_focused_pane() {
        let mut tracker = AgentTracker::new();
        tracker.apply_event(terminal_event(
            "amp",
            "background",
            Some("T-1"),
            Some("Fix focus"),
            Some("%7"),
        ));
        let panes = [shell("%7", "background"), shell("%1", "front")];

        // The client sits in another session; it once looked at %7 but is
        // not looking now.
        tracker.sync_panes(
            &panes,
            &[focus("front", "%1")],
            &sessions(&["front", "background"]),
            NOW,
        );
        assert!(tracker.is_unseen("background"));

        tracker.sync_panes(
            &panes,
            &[focus("background", "%7")],
            &sessions(&["front", "background"]),
            NOW,
        );
        assert!(!tracker.is_unseen("background"));
    }

    #[test]
    fn focusing_agent_pane_clears_only_matching_unseen_agent() {
        let mut tracker = AgentTracker::new();
        tracker.apply_event(terminal_event(
            "amp",
            "work",
            Some("T-1"),
            Some("Fix focus"),
            Some("%7"),
        ));
        tracker.apply_event(terminal_event(
            "codex",
            "work",
            Some("C-1"),
            Some("Polish UI"),
            Some("%8"),
        ));

        assert!(tracker.mark_agent_seen("work", "amp", Some("T-1"), Some("%7")));

        let agents = tracker.get_agents("work");
        let amp = agents.iter().find(|agent| agent.agent == "amp").unwrap();
        let codex = agents.iter().find(|agent| agent.agent == "codex").unwrap();
        assert_eq!(amp.unseen, None);
        assert_eq!(codex.unseen, Some(true));
        assert!(tracker.is_unseen("work"));
    }

    #[test]
    fn focusing_one_pane_preserves_unseen_for_other_threads_of_same_agent() {
        let mut tracker = AgentTracker::new();
        tracker.apply_event(terminal_event("amp", "work", Some("T-1"), None, Some("%7")));
        tracker.apply_event(terminal_event("amp", "work", Some("T-2"), None, Some("%8")));

        assert!(tracker.mark_pane_seen("work", "%7"));

        let agents = tracker.get_agents("work");
        let first = agents
            .iter()
            .find(|agent| agent.thread_id.as_deref() == Some("T-1"))
            .unwrap();
        let second = agents
            .iter()
            .find(|agent| agent.thread_id.as_deref() == Some("T-2"))
            .unwrap();
        assert_eq!(first.unseen, None);
        assert_eq!(second.unseen, Some(true));
    }

    #[test]
    fn session_mark_seen_clears_the_session_and_every_row_flag() {
        let mut tracker = AgentTracker::new();
        tracker.apply_event(terminal_event("amp", "work", Some("T-1"), None, Some("%7")));
        tracker.apply_event(terminal_event("amp", "work", Some("T-2"), None, None));

        assert!(tracker.mark_seen("work"));

        assert!(!tracker.is_unseen("work"));
        assert!(tracker.get_unseen().is_empty());
        assert!(
            tracker
                .get_agents("work")
                .iter()
                .all(|agent| agent.unseen.is_none())
        );
        assert_eq!(tracker.get_state("work").unwrap().unseen, None);
        assert!(!tracker.mark_seen("work"));
    }

    #[test]
    fn aggregate_state_reports_same_unseen_flag_as_agent_list() {
        let mut tracker = AgentTracker::new();
        tracker.apply_event(terminal_event(
            "amp",
            "work",
            Some("T-1"),
            Some("Fix focus"),
            Some("%7"),
        ));

        assert_eq!(tracker.get_state("work").unwrap().unseen, Some(true));
        assert_eq!(tracker.get_agents("work")[0].unseen, Some(true));

        assert!(tracker.mark_pane_seen("work", "%7"));

        assert_eq!(tracker.get_state("work").unwrap().unseen, None);
        assert_eq!(tracker.get_agents("work")[0].unseen, None);
    }

    #[test]
    fn focusing_pane_does_not_guess_for_unattached_logical_events() {
        let mut tracker = AgentTracker::new();
        tracker.apply_event(terminal_event("amp", "work", Some("T-1"), None, None));
        tracker.apply_event(terminal_event("amp", "work", Some("T-2"), None, None));
        tracker.sync_panes(
            &[pane("%7", "work", "node", "Amp running here")],
            &[focus("work", "%7")],
            &sessions(&["work"]),
            NOW,
        );

        assert!(!tracker.mark_pane_seen("work", "%7"));
        let agents = tracker.get_agents("work");
        assert_eq!(agents.len(), 2);
        assert!(agents.iter().all(|agent| agent.unseen == Some(true)));
        assert!(agents.iter().all(|agent| agent.pane_id.is_none()));
    }

    #[test]
    fn terminal_event_in_the_focused_session_still_becomes_unseen_until_its_pane_is_focused() {
        let mut tracker = AgentTracker::new();
        tracker.apply_event(terminal_event(
            "amp",
            "work",
            Some("T-1"),
            Some("Fix focus"),
            Some("%7"),
        ));
        tracker.sync_panes(
            &[shell("%7", "work"), shell("%1", "work")],
            &[focus("work", "%1")],
            &sessions(&["work"]),
            NOW,
        );

        assert!(tracker.is_unseen("work"));
        assert_eq!(tracker.get_agents("work")[0].unseen, Some(true));
    }

    // ----- heuristic binding (F036) -----

    #[test]
    fn heuristic_binding_needs_exactly_one_free_pane_and_one_candidate() {
        let mut tracker = AgentTracker::new();
        tracker.apply_event(event("codex", "work", Some("c1"), None));
        tracker.apply_event(event("codex", "work", Some("c2"), None));

        tracker.sync_panes(
            &[pane("%7", "work", "codex", "codex")],
            &[],
            &sessions(&["work"]),
            NOW,
        );
        assert!(
            tracker
                .get_agents("work")
                .iter()
                .all(|row| row.pane_id.is_none()),
            "two candidates for one pane: nobody binds"
        );

        tracker.dismiss("work", "codex", Some("c2"));
        tracker.sync_panes(
            &[pane("%7", "work", "codex", "codex")],
            &[],
            &sessions(&["work"]),
            NOW,
        );
        assert_eq!(only_agent(&tracker, "work").pane_id.as_deref(), Some("%7"));

        // A second codex pane appears: the existing binding stays put and
        // the new pane stays free.
        tracker.apply_event(event("codex", "work", Some("c3"), None));
        tracker.sync_panes(
            &[
                pane("%7", "work", "codex", "codex"),
                pane("%9", "work", "codex", "codex"),
            ],
            &[],
            &sessions(&["work"]),
            NOW,
        );
        let agents = tracker.get_agents("work");
        let c1 = agents
            .iter()
            .find(|row| row.thread_id.as_deref() == Some("c1"))
            .unwrap();
        let c3 = agents
            .iter()
            .find(|row| row.thread_id.as_deref() == Some("c3"))
            .unwrap();
        assert_eq!(c1.pane_id.as_deref(), Some("%7"));
        assert_eq!(c3.pane_id.as_deref(), Some("%9"));
    }

    #[test]
    fn heuristic_binding_prefers_matching_amp_titles() {
        let mut tracker = AgentTracker::new();
        tracker.apply_event(event("amp", "work", Some("T-1"), Some("Fix focus")));
        tracker.apply_event(event("amp", "work", Some("T-2"), Some("Review PR")));

        tracker.sync_panes(
            &[
                pane("%7", "work", "node", "Review PR - amp - T-2"),
                pane("%8", "work", "node", "Fix focus - amp - T-1"),
            ],
            &[],
            &sessions(&["work"]),
            NOW,
        );

        let agents = tracker.get_agents("work");
        let first = agents
            .iter()
            .find(|row| row.thread_id.as_deref() == Some("T-1"))
            .unwrap();
        let second = agents
            .iter()
            .find(|row| row.thread_id.as_deref() == Some("T-2"))
            .unwrap();
        assert_eq!(first.pane_id.as_deref(), Some("%8"));
        assert_eq!(second.pane_id.as_deref(), Some("%7"));
        assert_eq!(first.liveness, Some(AgentLiveness::Alive));
    }

    #[test]
    fn heuristic_binding_ends_when_the_pane_disappears() {
        let mut tracker = AgentTracker::new();
        tracker.apply_event(event("codex", "work", Some("c1"), None));
        tracker.sync_panes(
            &[pane("%7", "work", "codex", "codex")],
            &[],
            &sessions(&["work"]),
            NOW,
        );
        assert_eq!(only_agent(&tracker, "work").pane_id.as_deref(), Some("%7"));

        tracker.sync_panes(&[], &[], &sessions(&["work"]), NOW + 1_000);

        let row = only_agent(&tracker, "work");
        assert_eq!(row.pane_id, None);
        assert_eq!(row.liveness, Some(AgentLiveness::Exited));
        assert!(tracker.prune(NOW + 1_000 + EXITED_PRUNE_MS + 1));
    }

    #[test]
    fn heuristic_binding_never_touches_registry_rows() {
        let mut tracker = AgentTracker::new();
        tracker.apply_registry(
            vec![registry(
                "s1",
                "work",
                "%7",
                RegistryStatus::Busy,
                NOW - 5_000,
            )],
            NOW,
        );

        tracker.sync_panes(
            &[
                pane("%7", "work", "claude", "✳ app"),
                pane("%8", "work", "claude", "✳ other"),
            ],
            &[],
            &sessions(&["work"]),
            NOW,
        );

        assert_eq!(only_agent(&tracker, "work").pane_id.as_deref(), Some("%7"));
    }

    // ----- pruning -----

    fn aged_done_event(session: &str, thread_id: &str, ts: u64) -> AgentEvent {
        let mut event = event("codex", session, Some(thread_id), Some("Task"));
        event.status = AgentStatus::Done;
        event.ts = ts;
        event
    }

    #[test]
    fn prune_reaps_seen_pane_less_terminal_rows_after_ttl_but_keeps_unseen_longer() {
        let mut tracker = AgentTracker::new();
        tracker.apply_event(aged_done_event("work", "seen", NOW));
        tracker.apply_event(aged_done_event("work", "unseen", NOW));
        tracker.mark_agent_seen("work", "codex", Some("seen"), None);

        assert!(!tracker.prune(NOW + TERMINAL_PRUNE_MS));
        assert!(tracker.prune(NOW + TERMINAL_PRUNE_MS + 1));
        assert_eq!(
            only_agent(&tracker, "work").thread_id.as_deref(),
            Some("unseen")
        );

        assert!(!tracker.prune(NOW + UNSEEN_TERMINAL_PRUNE_MS));
        assert!(tracker.prune(NOW + UNSEEN_TERMINAL_PRUNE_MS + 1));
        assert!(tracker.get_agents("work").is_empty());
    }

    #[test]
    fn prune_reaps_silent_pane_less_rows_whatever_their_status() {
        let mut tracker = AgentTracker::new();
        let mut waiting = event("my-agent", "work", Some("w"), None);
        waiting.status = AgentStatus::Waiting;
        waiting.ts = NOW;
        tracker.apply_event(waiting);
        let mut idle = event("my-agent", "work", Some("i"), None);
        idle.status = AgentStatus::Idle;
        idle.ts = NOW;
        tracker.apply_event(idle);
        let mut running = event("my-agent", "work", Some("r"), None);
        running.ts = NOW;
        tracker.apply_event(running);

        assert!(!tracker.prune(NOW + STUCK_PRUNE_MS));
        assert!(tracker.prune(NOW + STUCK_PRUNE_MS + 1));
        assert!(tracker.get_agents("work").is_empty());
    }

    #[test]
    fn prune_keeps_rows_whose_pane_is_alive_regardless_of_age() {
        let mut tracker = AgentTracker::new();
        let mut done = aged_done_event("work", "t1", NOW);
        done.pane_id = Some("%7".to_string());
        tracker.apply_event(done);
        tracker.sync_panes(&[shell("%7", "work")], &[], &sessions(&["work"]), NOW);

        assert!(!tracker.prune(NOW + UNSEEN_TERMINAL_PRUNE_MS * 10));
        assert_eq!(tracker.get_agents("work").len(), 1);
    }

    #[test]
    fn prune_reaps_a_gone_waiting_row_after_the_grace_period() {
        let mut tracker = AgentTracker::new();
        let mut waiting = event("my-agent", "work", Some("w"), None);
        waiting.status = AgentStatus::Waiting;
        waiting.pane_id = Some("%7".to_string());
        tracker.apply_event(waiting);
        tracker.sync_panes(&[shell("%7", "work")], &[], &sessions(&["work"]), NOW);

        tracker.sync_panes(&[], &[], &sessions(&["work"]), NOW + 1_000);

        assert!(!tracker.prune(NOW + 1_000 + EXITED_PRUNE_MS));
        assert!(tracker.prune(NOW + 1_000 + EXITED_PRUNE_MS + 1));
    }

    #[test]
    fn rows_of_a_killed_session_are_gone_and_its_bookkeeping_is_released() {
        let mut tracker = AgentTracker::new();
        tracker.apply_event(event("my-agent", "gone", Some("t1"), None));
        tracker.apply_event(terminal_event("amp", "gone", Some("T-1"), None, Some("%7")));
        assert!(!tracker.get_event_timestamps("gone").is_empty());

        tracker.sync_panes(&[shell("%1", "work")], &[], &sessions(&["work"]), NOW);

        assert!(
            tracker
                .get_agents("gone")
                .iter()
                .all(|row| row.liveness == Some(AgentLiveness::Exited))
        );
        assert!(tracker.get_event_timestamps("gone").is_empty());
        assert!(tracker.prune(NOW + EXITED_PRUNE_MS + 1));
        assert!(tracker.get_agents("gone").is_empty());
        assert!(tracker.get_unseen().is_empty());
    }

    #[test]
    fn http_events_keep_thread_names_and_prompts_across_updates() {
        let mut tracker = AgentTracker::new();
        let mut first = event("my-agent", "work", Some("t1"), Some("Deploy"));
        first.last_user_prompt = Some("ship it".to_string());
        tracker.apply_event(first);

        let mut update = event("my-agent", "work", Some("t1"), None);
        update.status = AgentStatus::Done;
        update.ts = 2;
        tracker.apply_event(update);

        let row = only_agent(&tracker, "work");
        assert_eq!(row.thread_name.as_deref(), Some("Deploy"));
        assert_eq!(row.last_user_prompt.as_deref(), Some("ship it"));
        assert_eq!(row.status, AgentStatus::Done);
        assert_eq!(row.unseen, Some(true));
        assert_eq!(tracker.get_event_timestamps("work"), vec![1, 2]);
    }

    #[test]
    fn dismiss_removes_the_row_and_its_unseen_marker() {
        let mut tracker = AgentTracker::new();
        tracker.apply_event(terminal_event("amp", "work", Some("T-1"), None, Some("%7")));

        assert!(tracker.dismiss("work", "amp", Some("T-1")));

        assert!(tracker.get_agents("work").is_empty());
        assert!(!tracker.is_unseen("work"));
        assert!(!tracker.dismiss("work", "amp", Some("T-1")));
    }
}

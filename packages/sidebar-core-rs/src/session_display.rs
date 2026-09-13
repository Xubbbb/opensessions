use std::collections::{HashMap, HashSet};

use crate::generated::protocol::{AgentEvent, AgentStatus, SessionData};

#[derive(Debug, Clone, Default)]
pub struct GroupSummary {
    pub running_agents: usize,
    pub unseen: usize,
    pub insertions: u32,
    pub deletions: u32,
    pub first_port: Option<u32>,
    pub extra_ports: usize,
}

#[derive(Debug, Clone)]
pub enum DisplaySessionEntry<'a> {
    Group {
        key: String,
        label: String,
        count: usize,
        collapsed: bool,
        summary: GroupSummary,
    },
    Session {
        index: usize,
        session: &'a SessionData,
        indented: bool,
    },
}

impl DisplaySessionEntry<'_> {
    pub fn row_height(&self) -> usize {
        match self {
            Self::Group { .. } => 1,
            Self::Session { .. } => 3,
        }
    }
}

pub(crate) fn session_display_entries<'a>(
    sessions: Vec<&'a SessionData>,
    collapsed_groups: &HashSet<String>,
) -> Vec<DisplaySessionEntry<'a>> {
    let grouped_keys = grouped_worktree_keys(&sessions);
    let mut emitted = HashSet::<String>::new();
    let mut entries = Vec::with_capacity(sessions.len());
    let mut display_index = 0;

    for session in sessions.iter().copied() {
        let Some(key) = worktree_group_key(session).filter(|key| grouped_keys.contains(key)) else {
            display_index += 1;
            entries.push(DisplaySessionEntry::Session {
                index: display_index,
                session,
                indented: false,
            });
            continue;
        };

        if !emitted.insert(key.clone()) {
            continue;
        }

        let members = sessions
            .iter()
            .copied()
            .filter(|candidate| worktree_group_key(candidate).as_deref() == Some(key.as_str()))
            .collect::<Vec<_>>();
        entries.push(DisplaySessionEntry::Group {
            key: key.clone(),
            label: group_label(&key),
            count: members.len(),
            collapsed: collapsed_groups.contains(&key),
            summary: group_summary(&members),
        });
        if collapsed_groups.contains(&key) {
            continue;
        }
        for member in members {
            display_index += 1;
            entries.push(DisplaySessionEntry::Session {
                index: display_index,
                session: member,
                indented: true,
            });
        }
    }

    entries
}

fn group_summary(sessions: &[&SessionData]) -> GroupSummary {
    let mut summary = GroupSummary::default();
    for session in sessions {
        if session_has_active_agent(session) {
            summary.running_agents += 1;
        }
        if session.unseen {
            summary.unseen += 1;
        }
        summary.insertions = summary.insertions.saturating_add(session.insertions);
        summary.deletions = summary.deletions.saturating_add(session.deletions);
        for port in &session.ports {
            if summary.first_port.is_none() {
                summary.first_port = Some(*port);
            } else {
                summary.extra_ports += 1;
            }
        }
    }
    summary
}

pub(crate) fn session_has_active_agent(session: &SessionData) -> bool {
    session
        .agent_state
        .iter()
        .chain(session.agents.iter())
        .any(|agent| is_active_agent(agent))
}

fn is_active_agent(agent: &AgentEvent) -> bool {
    matches!(
        agent.status,
        AgentStatus::Running | AgentStatus::ToolRunning | AgentStatus::Waiting
    )
}

fn grouped_worktree_keys(sessions: &[&SessionData]) -> HashSet<String> {
    let mut counts = HashMap::<String, usize>::new();
    for session in sessions {
        if let Some(key) = worktree_group_key(session) {
            *counts.entry(key).or_default() += 1;
        }
    }
    counts
        .into_iter()
        .filter_map(|(key, count)| (count >= 2).then_some(key))
        .collect()
}

pub(crate) fn worktree_group_key(session: &SessionData) -> Option<String> {
    if !session.is_worktree {
        return None;
    }
    let dir = session.dir.trim_end_matches('/');
    let (parent, _) = dir.rsplit_once('/')?;
    (!parent.is_empty()).then(|| parent.to_string())
}

fn group_label(key: &str) -> String {
    key.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(key)
        .to_string()
}

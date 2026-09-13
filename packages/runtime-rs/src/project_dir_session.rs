//! Attribution of a project directory to a mux session, for agent sources
//! that know where they run but not in which pane (transcript watchers,
//! `projectDir` on `/api/agent-event`).

use std::collections::{BTreeMap, BTreeSet, HashSet};

pub type DirSessionMap = BTreeMap<String, Vec<String>>;

pub fn build_dir_session_map(
    sessions: impl IntoIterator<Item = (String, String)>,
) -> DirSessionMap {
    let mut map = BTreeMap::new();
    for (name, dir) in sessions {
        if dir.is_empty() {
            continue;
        }
        let names: &mut Vec<String> = map.entry(dir).or_default();
        if !names.contains(&name) {
            names.push(name);
        }
    }
    map
}

/// Resolve `project_dir` to one session: an exact directory match wins,
/// then sessions rooted below it together with the sessions of its deepest
/// ancestor directory, then the encoded-folder fallback. A stage that
/// matches several sessions resolves to none of them.
pub fn resolve_session_for_project_dir(
    project_dir: &str,
    dir_session_map: &DirSessionMap,
) -> Option<String> {
    resolve_session_for_project_dir_preferring(project_dir, dir_session_map, &HashSet::new())
}

/// Like `resolve_session_for_project_dir`, but a stage that matches several
/// sessions is settled by `preferred` (sessions with a live pane of the
/// agent in question) when exactly one of the candidates is preferred.
pub fn resolve_session_for_project_dir_preferring(
    project_dir: &str,
    dir_session_map: &DirSessionMap,
    preferred: &HashSet<String>,
) -> Option<String> {
    let exact_matches = dir_session_map
        .get(project_dir)
        .map(|sessions| sessions.iter().cloned().collect::<BTreeSet<_>>())
        .unwrap_or_default();
    if !exact_matches.is_empty() {
        return settle(exact_matches, preferred);
    }

    let mut related_matches = BTreeSet::new();
    let mut deepest_ancestor: Option<(&String, &Vec<String>)> = None;
    for (dir, sessions) in dir_session_map {
        if dir.starts_with(&format!("{project_dir}/")) {
            related_matches.extend(sessions.iter().cloned());
        } else if project_dir.starts_with(&format!("{dir}/"))
            && deepest_ancestor.is_none_or(|(deepest, _)| dir.len() > deepest.len())
        {
            deepest_ancestor = Some((dir, sessions));
        }
    }
    if let Some((_, sessions)) = deepest_ancestor {
        related_matches.extend(sessions.iter().cloned());
    }
    if !related_matches.is_empty() {
        return settle(related_matches, preferred);
    }

    let encoded = project_dir.strip_prefix("__encoded__:")?;

    let mut encoded_matches = BTreeSet::new();
    for (dir, sessions) in dir_session_map {
        if encode_project_dir(dir) != encoded {
            continue;
        }
        encoded_matches.extend(sessions.iter().cloned());
    }
    settle(encoded_matches, preferred)
}

fn settle(matches: BTreeSet<String>, preferred: &HashSet<String>) -> Option<String> {
    if matches.len() == 1 {
        return matches.into_iter().next();
    }
    let mut preferred_matches = matches
        .into_iter()
        .filter(|session| preferred.contains(session));
    let winner = preferred_matches.next()?;
    preferred_matches.next().is_none().then_some(winner)
}

fn encode_project_dir(dir: &str) -> String {
    dir.chars()
        .map(|ch| {
            if matches!(ch, '/' | '.' | '_') {
                '-'
            } else {
                ch
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(entries: &[(&str, &str)]) -> DirSessionMap {
        build_dir_session_map(
            entries
                .iter()
                .map(|(name, dir)| ((*name).to_string(), (*dir).to_string())),
        )
    }

    fn preferred(names: &[&str]) -> HashSet<String> {
        names.iter().map(|name| (*name).to_string()).collect()
    }

    #[test]
    fn exact_directory_match_wins_over_related_directories() {
        let sessions = map(&[("home", "/home/me"), ("app", "/home/me/code/app")]);
        assert_eq!(
            resolve_session_for_project_dir("/home/me/code/app", &sessions).as_deref(),
            Some("app")
        );
    }

    #[test]
    fn the_deepest_ancestor_session_wins_over_shallower_ones() {
        // A session at $HOME no longer makes every nested project ambiguous.
        let sessions = map(&[("home", "/home/me"), ("app", "/home/me/code/app")]);
        assert_eq!(
            resolve_session_for_project_dir("/home/me/code/app/packages/core", &sessions)
                .as_deref(),
            Some("app")
        );
        assert_eq!(
            resolve_session_for_project_dir("/home/me/docs", &sessions).as_deref(),
            Some("home")
        );
    }

    #[test]
    fn descendant_sessions_and_the_deepest_ancestor_are_pooled_and_must_be_unique() {
        let sessions = map(&[
            ("app", "/home/me/code/app"),
            ("lib", "/home/me/code/lib"),
            ("home", "/home/me"),
        ]);
        assert_eq!(
            resolve_session_for_project_dir("/home/me/code", &sessions),
            None,
            "two descendants plus the ancestor: ambiguous"
        );
        let sessions = map(&[("app", "/home/me/code/app")]);
        assert_eq!(
            resolve_session_for_project_dir("/home/me/code", &sessions).as_deref(),
            Some("app")
        );
    }

    #[test]
    fn a_tie_is_settled_by_the_session_with_a_live_agent_pane() {
        let sessions = map(&[("work", "/repo"), ("work-2", "/repo")]);
        assert_eq!(resolve_session_for_project_dir("/repo", &sessions), None);
        assert_eq!(
            resolve_session_for_project_dir_preferring("/repo", &sessions, &preferred(&["work-2"]))
                .as_deref(),
            Some("work-2")
        );
        assert_eq!(
            resolve_session_for_project_dir_preferring(
                "/repo",
                &sessions,
                &preferred(&["work", "work-2"])
            ),
            None,
            "both have agent panes: still ambiguous"
        );
        assert_eq!(
            resolve_session_for_project_dir_preferring("/repo", &sessions, &preferred(&["other"])),
            None
        );
    }

    #[test]
    fn encoded_fallback_matches_the_dashed_folder_name() {
        let sessions = map(&[("app", "/home/me/my_app.v2")]);
        assert_eq!(
            resolve_session_for_project_dir("__encoded__:-home-me-my-app-v2", &sessions).as_deref(),
            Some("app")
        );
        assert_eq!(
            resolve_session_for_project_dir("__encoded__:-home-me-other", &sessions),
            None
        );
    }
}

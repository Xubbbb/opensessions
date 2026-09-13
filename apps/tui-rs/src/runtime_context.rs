#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneIdentity {
    pub pane_id: String,
    pub session_name: String,
    pub window_id: Option<String>,
}

pub fn pane_identity_from_env<F>(env: F) -> Option<PaneIdentity>
where
    F: Fn(&str) -> Option<String>,
{
    pane_identity_resolve(env, |_, _| None)
}

/// Resolve the running pane's identity, mirroring
/// tmux display-message fallback used by the live sidebar.
///
/// `env` reads process environment variables. `tmux_query` invokes
/// `tmux display-message -p -t <target> <format>` and returns the trimmed
/// stdout. Tmux is only consulted when the corresponding `OPENSESSIONS_*`
/// env vars are absent.
pub fn pane_identity_resolve<F, T>(env: F, tmux_query: T) -> Option<PaneIdentity>
where
    F: Fn(&str) -> Option<String>,
    T: Fn(&str, &str) -> Option<String>,
{
    let pane_id = env("TMUX_PANE")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())?;

    let session_name = env("OPENSESSIONS_SESSION_NAME")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| {
            tmux_query("#{session_name}", &pane_id)
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })?;

    let window_id = env("OPENSESSIONS_WINDOW_ID")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| {
            tmux_query("#{window_id}", &pane_id)
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        });

    Some(PaneIdentity {
        pane_id,
        session_name,
        window_id,
    })
}

/// Plan describing which tmux pane should receive focus after the sidebar
/// finishes capability detection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefocusPlan {
    pub select_pane: String,
}

/// Refocus-main-pane planning as a pure function.
///
/// `tmux_query` is invoked with the argv slice that should be passed to
/// `tmux <args>`. It must return the trimmed stdout when the command succeeds
/// or `None` when it fails. The function is decoupled from process spawning to
/// keep red/green TDD on the sidebar refocus rules straightforward.
pub fn refocus_plan<F>(
    pane_id: &str,
    refocus_window_env: Option<&str>,
    tmux_query: F,
) -> Option<RefocusPlan>
where
    F: Fn(&[&str]) -> Option<String>,
{
    let window_id = match refocus_window_env.map(str::trim).filter(|s| !s.is_empty()) {
        Some(window) => window.to_string(),
        None => tmux_query(&["display-message", "-t", pane_id, "-p", "#{window_id}"])
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())?,
    };

    let panes = tmux_query(&[
        "list-panes",
        "-t",
        &window_id,
        "-F",
        "#{pane_id}\t#{pane_last}\t#{pane_title}",
    ])?;
    let candidates = panes
        .lines()
        .filter_map(|line| {
            let mut parts = line.trim().splitn(3, '\t');
            let id = parts.next()?.trim();
            let last = parts.next()?.trim() == "1";
            let title = parts.next().unwrap_or_default();
            (!id.is_empty() && title != "opensessions-sidebar").then_some((id.to_string(), last))
        })
        .collect::<Vec<_>>();
    // Spawning the sidebar made it the active pane; the pane the user was
    // in is now tmux's "last" pane. Go back there rather than to whichever
    // content pane happens to be listed first.
    let main_pane = candidates
        .iter()
        .find(|(_, last)| *last)
        .or_else(|| candidates.first())
        .map(|(id, _)| id.clone())?;

    Some(RefocusPlan {
        select_pane: main_pane,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmux(args: &[&str]) -> Option<String> {
        match args.first().copied() {
            Some("list-panes") => {
                Some("%5\t0\topensessions-sidebar\n%1\t0\tbash\n%3\t1\tnvim main.rs\n".to_string())
            }
            _ => None,
        }
    }

    #[test]
    fn refocus_returns_to_the_previously_active_pane() {
        let plan = refocus_plan("%5", Some("@1"), tmux).expect("plan");
        assert_eq!(plan.select_pane, "%3");
    }

    #[test]
    fn refocus_falls_back_to_first_content_pane_without_a_last_pane() {
        let plan = refocus_plan("%5", Some("@1"), |args| match args.first().copied() {
            Some("list-panes") => Some("%5\t0\topensessions-sidebar\n%1\t0\tbash\n".to_string()),
            _ => None,
        })
        .expect("plan");
        assert_eq!(plan.select_pane, "%1");
    }
}

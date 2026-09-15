use crate::app::{App, Modal, PanelFocus};
use crate::renderer::{THEME_NAMES, compute_hit_target, detail_separator_row};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiKey {
    Char(char),
    Up,
    Down,
    Left,
    Right,
    Tab { shift: bool },
    Enter,
    Esc,
    Backspace,
    CtrlJ,
    CtrlK,
    AltUp,
    AltDown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiMouse {
    ScrollUp {
        x: u16,
        y: u16,
        width: u16,
        height: u16,
    },
    ScrollDown {
        x: u16,
        y: u16,
        width: u16,
        height: u16,
    },
    Click {
        x: u16,
        y: u16,
        width: u16,
        height: u16,
    },
    Move {
        x: u16,
        y: u16,
        width: u16,
        height: u16,
    },
    Drag {
        y: u16,
    },
    DragEnd,
}

pub fn apply_ui_key(app: &mut App, key: UiKey) {
    if app.is_modal_open() {
        apply_modal_key(app, key);
        return;
    }

    match key {
        UiKey::AltUp => app.reorder_focused_session(-1),
        UiKey::AltDown => app.reorder_focused_session(1),
        UiKey::CtrlJ => app.focus_agents_panel(),
        UiKey::CtrlK => app.focus_sessions_panel(),
        UiKey::Down => {
            if app.panel_focus == PanelFocus::Agents {
                app.move_agent_focus(1);
            } else {
                app.move_focus(1);
            }
        }
        UiKey::Up => {
            if app.panel_focus == PanelFocus::Agents {
                app.move_agent_focus(-1);
            } else {
                app.move_focus(-1);
            }
        }
        UiKey::Left => {
            if app.panel_focus == PanelFocus::Sessions {
                app.resize_detail_panel(-1);
            } else {
                app.focus_sessions_panel();
            }
        }
        UiKey::Right => {
            if app.panel_focus == PanelFocus::Sessions {
                let agent_count = app
                    .focused_session_name()
                    .and_then(|name| app.sessions.iter().find(|s| s.name == name))
                    .map(|s| s.agents.len())
                    .unwrap_or(0);
                if agent_count > 0 {
                    app.focus_agents_panel();
                } else {
                    app.resize_detail_panel(1);
                }
            }
        }
        UiKey::Tab { shift } => app.handle_tab(shift),
        UiKey::Enter => app.activate_focused_item(),
        UiKey::Esc => app.focus_sessions_panel(),
        UiKey::Backspace => {}
        UiKey::Char(ch) => app.handle_key_char(ch),
    }
}

fn apply_modal_key(app: &mut App, key: UiKey) {
    match &app.modal {
        Modal::ThemePicker { .. } => apply_theme_picker_key(app, key),
        Modal::WidthSlider { .. } => apply_width_slider_key(app, key),
        Modal::KillConfirm { .. } => apply_kill_confirm_key(app, key),
        Modal::None => {}
    }
}

fn filtered_theme_names(query: &str) -> Vec<&'static str> {
    let query_lower = query.to_lowercase();
    THEME_NAMES
        .iter()
        .copied()
        .filter(|name| query_lower.is_empty() || name.contains(&query_lower))
        .collect()
}

fn apply_theme_picker_key(app: &mut App, key: UiKey) {
    match key {
        UiKey::Esc => {
            app.close_theme_picker();
        }
        UiKey::Enter => {
            app.confirm_theme_picker();
        }
        UiKey::Up => {
            if let Modal::ThemePicker {
                query, selected, ..
            } = &mut app.modal
            {
                let names = filtered_theme_names(query);
                if !names.is_empty() && *selected > 0 {
                    *selected -= 1;
                    app.theme = Some(names[*selected].to_string());
                }
            }
        }
        UiKey::Down => {
            if let Modal::ThemePicker {
                query, selected, ..
            } = &mut app.modal
            {
                let names = filtered_theme_names(query);
                if !names.is_empty() && *selected + 1 < names.len() {
                    *selected += 1;
                    app.theme = Some(names[*selected].to_string());
                }
            }
        }
        UiKey::Backspace => {
            if let Modal::ThemePicker {
                query, selected, ..
            } = &mut app.modal
            {
                query.pop();
                let names = filtered_theme_names(query);
                *selected = (*selected).min(names.len().saturating_sub(1));
                if let Some(name) = names.get(*selected) {
                    app.theme = Some(name.to_string());
                }
            }
        }
        UiKey::Char(ch) => {
            if let Modal::ThemePicker {
                query, selected, ..
            } = &mut app.modal
            {
                query.push(ch);
                let names = filtered_theme_names(query);
                *selected = 0;
                if let Some(name) = names.first() {
                    app.theme = Some(name.to_string());
                }
            }
        }
        _ => {}
    }
}

fn apply_kill_confirm_key(app: &mut App, key: UiKey) {
    match key {
        UiKey::Char('y') => {
            if matches!(app.modal, Modal::KillConfirm { .. }) {
                app.confirm_kill_target();
            }
        }
        _ => {
            app.modal = Modal::None;
        }
    }
}

fn apply_width_slider_key(app: &mut App, key: UiKey) {
    match key {
        UiKey::Left | UiKey::Down => app.adjust_width_slider(-1),
        UiKey::Right | UiKey::Up => app.adjust_width_slider(1),
        UiKey::Char('h') => app.adjust_width_slider(-1),
        UiKey::Char('l') => app.adjust_width_slider(1),
        UiKey::Char('H') => app.adjust_width_slider(-5),
        UiKey::Char('L') => app.adjust_width_slider(5),
        UiKey::Enter => app.confirm_width_slider(),
        UiKey::Esc => app.close_width_slider(),
        _ => {}
    }
}

pub fn apply_ui_mouse(app: &mut App, event: UiMouse) {
    // A modal owns the input; nothing may reach the rows behind it.
    if app.is_modal_open() {
        if matches!(event, UiMouse::DragEnd) {
            app.resize_drag_state = None;
        }
        return;
    }
    match event {
        UiMouse::ScrollUp {
            x: _,
            y,
            width,
            height,
        } => {
            let separator_row = detail_separator_row(app, width, height);
            let session_rows = separator_row.saturating_sub(3) as usize;
            if y < separator_row {
                app.scroll_sessions(-1, session_rows);
            } else if app.panel_focus == PanelFocus::Agents {
                app.move_agent_focus(-1);
            } else {
                app.scroll_sessions(-1, session_rows);
            }
        }
        UiMouse::ScrollDown {
            x: _,
            y,
            width,
            height,
        } => {
            let separator_row = detail_separator_row(app, width, height);
            let session_rows = separator_row.saturating_sub(3) as usize;
            if y < separator_row {
                app.scroll_sessions(1, session_rows);
            } else if app.panel_focus == PanelFocus::Agents {
                app.move_agent_focus(1);
            } else {
                app.scroll_sessions(1, session_rows);
            }
        }
        UiMouse::Click {
            x,
            y,
            width,
            height,
        } => {
            if app.ignores_clicks_at(std::time::Instant::now()) {
                return;
            }
            // Check if clicking on the separator row to start a drag resize
            if y == detail_separator_row(app, width, height) {
                app.resize_drag_state = Some((y, app.detail_panel_height));
                return;
            }

            let target = compute_hit_target(app, x, y, width, height);
            if let Some(target) = target {
                app.activate_hit_target(target);
            }
        }
        UiMouse::Move {
            x,
            y,
            width,
            height,
        } => {
            app.set_hover_target(compute_hit_target(app, x, y, width, height));
        }
        UiMouse::Drag { y } => {
            if let Some((start_y, start_height)) = app.resize_drag_state {
                let delta = start_y as i16 - y as i16;
                let new_height = (start_height as i16 + delta).max(4) as usize;
                app.set_detail_panel_height(new_height);
            }
        }
        UiMouse::DragEnd => {
            app.resize_drag_state = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;
    use crate::app::{Modal, SidebarFocus};
    use crate::generated::protocol::{
        AgentPanelScope, ClientCommand, ServerMessage, ServerState, SessionData,
    };

    fn session(name: &str) -> SessionData {
        SessionData {
            name: name.to_string(),
            id: None,
            created_at: 0,
            dir: format!("/tmp/{name}"),
            branch: String::new(),
            dirty: false,
            changed_files: 0,
            insertions: 0,
            deletions: 0,
            is_worktree: false,
            unseen: false,
            panes: 1,
            ports: Vec::new(),
            local_links: Vec::new(),
            windows: 1,
            uptime: String::new(),
            agent_state: None,
            agents: Vec::new(),
            event_timestamps: Vec::new(),
            metadata: None,
        }
    }

    fn app() -> App {
        let mut app = App::from_state(ServerState {
            sessions: vec![session("work"), session("docs"), session("notes")],
            focused_session: None,
            current_session: Some("work".to_string()),
            theme: None,
            session_filter: None,
            agent_panel_scope: AgentPanelScope::Current,
            sidebar_width: 40,
            detail_panel_height: 10,
            initializing: false,
            init_label: None,
            collapsed_worktree_groups: Vec::new(),
            ts: 0,
        });
        app.set_pane_identity("%1".to_string(), "work".to_string(), Some("@1".to_string()));
        app
    }

    /// Screen row of a session's name line (header, blank, then 3 rows each).
    fn row_of(index: usize) -> u16 {
        2 + (index as u16) * 3
    }

    #[test]
    fn a_click_on_a_session_row_switches_to_it() {
        let mut app = app();

        apply_ui_mouse(
            &mut app,
            UiMouse::Click {
                x: 6,
                y: row_of(1),
                width: 40,
                height: 40,
            },
        );

        assert_eq!(app.focused_session_name(), Some("docs"));
        assert_eq!(
            app.drain_commands(),
            vec![ClientCommand::SwitchSession {
                name: "docs".to_string(),
                client_tty: None,
            }]
        );
    }

    #[test]
    fn mouse_is_inert_while_a_modal_is_open() {
        let mut app = app();
        app.set_sidebar_focus(SidebarFocus::Session("docs".to_string()));
        app.open_kill_confirm_for_focus();
        assert!(matches!(app.modal, Modal::KillConfirm { .. }));

        apply_ui_mouse(
            &mut app,
            UiMouse::Click {
                x: 6,
                y: row_of(0),
                width: 40,
                height: 40,
            },
        );
        apply_ui_mouse(
            &mut app,
            UiMouse::ScrollDown {
                x: 6,
                y: row_of(0),
                width: 40,
                height: 40,
            },
        );

        assert!(app.drain_commands().is_empty());
        assert!(matches!(app.modal, Modal::KillConfirm { .. }));
        assert_eq!(app.focused_session_name(), Some("docs"));
    }

    #[test]
    fn clicks_right_after_a_chosen_arrival_are_the_double_clicks_tail() {
        let mut app = app();
        app.apply_server_message(ServerMessage::ActivateSession {
            name: "work".to_string(),
            source_pane_id: Some("%99".to_string()),
            chosen: true,
        });

        assert!(app.ignores_clicks_at(Instant::now()));
        apply_ui_mouse(
            &mut app,
            UiMouse::Click {
                x: 6,
                y: row_of(2),
                width: 40,
                height: 40,
            },
        );
        assert!(app.drain_commands().is_empty());
        assert_eq!(app.focused_session_name(), Some("work"));

        assert!(!app.ignores_clicks_at(Instant::now() + Duration::from_millis(400)));
    }
}

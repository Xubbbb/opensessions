use serde_json::Value;

use crate::protocol::AgentStatus;

pub fn map_amp_state(amp_state: &str) -> Option<AgentStatus> {
    match amp_state {
        "working" | "streaming" | "running_tools" => Some(AgentStatus::Running),
        "tool_use" => Some(AgentStatus::ToolRunning),
        "awaiting_approval" => Some(AgentStatus::Waiting),
        "idle" => Some(AgentStatus::Done),
        "error" => Some(AgentStatus::Error),
        _ => None,
    }
}

pub fn determine_amp_message_status(last_msg: &Value) -> AgentStatus {
    let Some(role) = last_msg.get("role").and_then(Value::as_str) else {
        return AgentStatus::Idle;
    };

    match role {
        "user" => {
            if content_has_type_with_run_status(
                last_msg.get("content"),
                "tool_result",
                "in-progress",
            ) {
                AgentStatus::ToolRunning
            } else {
                AgentStatus::Running
            }
        }
        "assistant" => {
            let state_type = last_msg.pointer("/state/type").and_then(Value::as_str);
            match state_type {
                None => AgentStatus::Running,
                Some("streaming") => AgentStatus::Running,
                Some("cancelled") => AgentStatus::Interrupted,
                Some("complete") => match last_msg
                    .pointer("/state/stopReason")
                    .and_then(Value::as_str)
                {
                    Some("tool_use") => AgentStatus::Running,
                    Some("end_turn") => AgentStatus::Done,
                    _ => AgentStatus::Error,
                },
                _ => AgentStatus::Running,
            }
        }
        _ => AgentStatus::Idle,
    }
}

pub fn determine_codex_status(entry: &Value) -> Option<AgentStatus> {
    match entry.get("type").and_then(Value::as_str) {
        Some("event_msg") => match entry.pointer("/payload/type").and_then(Value::as_str) {
            Some("task_complete") => Some(AgentStatus::Done),
            Some("turn_aborted") => Some(AgentStatus::Interrupted),
            Some("task_started" | "user_message") => Some(AgentStatus::Running),
            Some("agent_message") => {
                match entry.pointer("/payload/phase").and_then(Value::as_str) {
                    Some("final_answer") => Some(AgentStatus::Done),
                    _ => Some(AgentStatus::Running),
                }
            }
            _ => None,
        },
        Some("response_item") => match entry.pointer("/payload/type").and_then(Value::as_str) {
            Some("message") => match entry.pointer("/payload/role").and_then(Value::as_str) {
                Some("developer") => None,
                Some("user") => Some(AgentStatus::Running),
                Some("assistant") => {
                    match entry.pointer("/payload/phase").and_then(Value::as_str) {
                        Some("final_answer") => Some(AgentStatus::Done),
                        _ => Some(AgentStatus::Running),
                    }
                }
                _ => None,
            },
            Some(
                "function_call"
                | "function_call_output"
                | "reasoning"
                | "custom_tool_call"
                | "custom_tool_call_output"
                | "web_search_call",
            ) => Some(AgentStatus::Running),
            _ => None,
        },
        Some("message") => match entry.get("role").and_then(Value::as_str) {
            Some("user" | "assistant") => Some(AgentStatus::Running),
            _ => None,
        },
        Some("function_call" | "function_call_output" | "reasoning") => Some(AgentStatus::Running),
        _ => None,
    }
}

pub fn determine_opencode_status(msg: &Value) -> AgentStatus {
    let Some(role) = msg.get("role").and_then(Value::as_str) else {
        return AgentStatus::Idle;
    };

    if let Some(error_name) = msg.pointer("/error/name").and_then(Value::as_str) {
        return if error_name == "MessageAbortedError" {
            AgentStatus::Interrupted
        } else {
            AgentStatus::Error
        };
    }

    match role {
        "assistant" => match msg.get("finish").and_then(Value::as_str) {
            Some("tool-calls") => AgentStatus::Running,
            Some("stop") => AgentStatus::Done,
            Some("error") => AgentStatus::Error,
            Some("unknown") => AgentStatus::Done,
            _ if msg
                .pointer("/time/completed")
                .and_then(Value::as_u64)
                .is_some() =>
            {
                AgentStatus::Done
            }
            _ => AgentStatus::Running,
        },
        "user" => AgentStatus::Running,
        _ => AgentStatus::Idle,
    }
}

pub fn determine_pi_status(entry: &Value) -> AgentStatus {
    if entry.get("type").and_then(Value::as_str) != Some("message") {
        return AgentStatus::Idle;
    }
    let Some(role) = entry.pointer("/message/role").and_then(Value::as_str) else {
        return AgentStatus::Idle;
    };

    match role {
        "user" | "toolResult" => AgentStatus::Running,
        "assistant" => match entry.pointer("/message/stopReason").and_then(Value::as_str) {
            Some("toolUse") => AgentStatus::Running,
            Some("stop") => AgentStatus::Done,
            Some("error") => AgentStatus::Error,
            Some("cancelled" | "aborted" | "interrupted") => AgentStatus::Interrupted,
            _ => AgentStatus::Waiting,
        },
        _ => AgentStatus::Idle,
    }
}

fn content_has_type_with_run_status(
    content: Option<&Value>,
    target_type: &str,
    run_status: &str,
) -> bool {
    content.and_then(Value::as_array).is_some_and(|items| {
        items.iter().any(|item| {
            item.get("type").and_then(Value::as_str) == Some(target_type)
                && item.pointer("/run/status").and_then(Value::as_str) == Some(run_status)
        })
    })
}

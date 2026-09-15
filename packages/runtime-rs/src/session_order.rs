use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io;
use std::path::PathBuf;

use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone, Default)]
pub struct SessionOrder {
    order: Vec<String>,
    hidden: Vec<String>,
    persist_path: Option<PathBuf>,
    /// Last known name per mux session id, so a renamed session keeps its
    /// place (and its hidden state) instead of reappearing at the end.
    /// Not persisted: ids are per mux server.
    names_by_id: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct PersistedSessionOrder {
    order: Option<Value>,
    hidden: Option<Value>,
}

impl SessionOrder {
    pub fn new(persist_path: Option<PathBuf>) -> Self {
        let mut session_order = Self {
            order: Vec::new(),
            hidden: Vec::new(),
            persist_path,
            names_by_id: HashMap::new(),
        };
        session_order.load();
        session_order
    }

    /// Reconcile with the live session list: `(id, name)` pairs, id `None`
    /// when the mux has no stable ids. A session whose id is known under
    /// another name was renamed and keeps its position and hidden state.
    pub fn sync(&mut self, sessions: impl IntoIterator<Item = (Option<String>, String)>) {
        let sessions = sessions.into_iter().collect::<Vec<_>>();
        let mut renamed = false;
        for (id, name) in &sessions {
            let Some(id) = id else {
                continue;
            };
            if let Some(previous) = self.names_by_id.insert(id.clone(), name.clone())
                && previous != *name
                && !sessions.iter().any(|(_, other)| *other == previous)
            {
                for slot in self
                    .order
                    .iter_mut()
                    .chain(self.hidden.iter_mut())
                    .filter(|slot| **slot == previous)
                {
                    *slot = name.clone();
                    renamed = true;
                }
            }
        }
        let name_set = sessions
            .iter()
            .map(|(_, name)| name.clone())
            .collect::<BTreeSet<_>>();
        self.names_by_id.retain(|_, name| name_set.contains(name));
        self.order.retain(|name| name_set.contains(name));
        self.hidden.retain(|name| name_set.contains(name));
        for (_, name) in sessions {
            if !self.order.contains(&name) {
                self.order.push(name);
            }
        }
        if renamed {
            let _ = self.save();
        }
    }

    pub fn set_visible_order(&mut self, visible_names: Vec<String>) {
        let visible_set = visible_names.iter().cloned().collect::<BTreeSet<_>>();
        let mut order = visible_names;
        order.extend(
            self.order
                .iter()
                .filter(|name| !visible_set.contains(*name))
                .cloned(),
        );
        self.order = order;
        let _ = self.save();
    }

    pub fn hide(&mut self, name: &str) {
        if !self.order.iter().any(|candidate| candidate == name)
            || self.hidden.iter().any(|candidate| candidate == name)
        {
            return;
        }
        self.hidden.push(name.to_string());
        let _ = self.save();
    }

    pub fn show(&mut self, name: &str) {
        let len_before = self.hidden.len();
        self.hidden.retain(|candidate| candidate != name);
        if self.hidden.len() == len_before {
            return;
        }
        if !self.order.iter().any(|candidate| candidate == name) {
            self.order.push(name.to_string());
        }
        let _ = self.save();
    }

    pub fn show_all(&mut self) {
        if self.hidden.is_empty() {
            return;
        }
        self.hidden.clear();
        let _ = self.save();
    }

    pub fn apply(&self, names: impl IntoIterator<Item = String>) -> Vec<String> {
        let mut names = names
            .into_iter()
            .filter(|name| !self.hidden.contains(name))
            .collect::<Vec<_>>();
        names.sort_by(|a, b| {
            let pa = self
                .order
                .iter()
                .position(|name| name == a)
                .unwrap_or(usize::MAX);
            let pb = self
                .order
                .iter()
                .position(|name| name == b)
                .unwrap_or(usize::MAX);
            pa.cmp(&pb)
        });
        names
    }

    fn load(&mut self) {
        let Some(path) = &self.persist_path else {
            return;
        };
        let Ok(raw) = fs::read_to_string(path) else {
            return;
        };
        let Ok(parsed) = serde_json::from_str::<Value>(&raw) else {
            return;
        };

        if let Value::Array(values) = parsed {
            self.order = strings_from_value_array(values);
            return;
        }

        let Ok(persisted) = serde_json::from_value::<PersistedSessionOrder>(parsed) else {
            return;
        };
        if let Some(Value::Array(order)) = persisted.order {
            self.order = strings_from_value_array(order);
        }
        if let Some(Value::Array(hidden)) = persisted.hidden {
            self.hidden = strings_from_value_array(hidden);
        }
    }

    fn save(&self) -> io::Result<()> {
        let Some(path) = &self.persist_path else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let value = if self.hidden.is_empty() {
            serde_json::json!(self.order)
        } else {
            serde_json::json!({ "order": self.order, "hidden": self.hidden })
        };
        let encoded = serde_json::to_string(&value).map_err(io::Error::other)?;
        fs::write(path, format!("{encoded}\n"))
    }
}

fn strings_from_value_array(values: Vec<Value>) -> Vec<String> {
    values
        .into_iter()
        .filter_map(|value| value.as_str().map(str::to_string))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(sessions: &[(&str, &str)]) -> Vec<(Option<String>, String)> {
        sessions
            .iter()
            .map(|(id, name)| (Some((*id).to_string()), (*name).to_string()))
            .collect()
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| (*name).to_string()).collect()
    }

    #[test]
    fn a_renamed_session_keeps_its_place_and_its_hidden_state() {
        let mut order = SessionOrder::new(None);
        order.sync(ids(&[("$1", "a"), ("$2", "b"), ("$3", "c")]));
        order.set_visible_order(names(&["c", "b", "a"]));
        order.hide("b");

        order.sync(ids(&[("$1", "a"), ("$2", "b2"), ("$3", "c")]));

        assert_eq!(order.apply(names(&["a", "b2", "c"])), names(&["c", "a"]));
        order.show("b2");
        assert_eq!(
            order.apply(names(&["a", "b2", "c"])),
            names(&["c", "b2", "a"])
        );
    }

    #[test]
    fn sessions_without_ids_still_append_and_drop_by_name() {
        let mut order = SessionOrder::new(None);
        order.sync(vec![(None, "a".to_string()), (None, "b".to_string())]);
        order.sync(vec![(None, "b".to_string()), (None, "c".to_string())]);

        assert_eq!(order.apply(names(&["b", "c"])), names(&["b", "c"]));
    }
}

use anyhow::Result;
use crossterm::event::KeyCode;

use super::App;
use crate::tui_state::parse_bypass_entries;

impl App {
    pub(super) fn open_benchmark_filter_modal(&mut self) {
        self.filter_input = Some(self.benchmark_filter.clone());
        self.flash = None;
    }

    pub(super) fn open_bypass_modal(&mut self) {
        self.bypass_input = Some(self.bypass_entries.join(","));
        self.flash = None;
    }

    pub(super) fn handle_filter_input_key(&mut self, code: KeyCode) -> Result<bool> {
        let Some(buffer) = self.filter_input.as_mut() else {
            return Ok(true);
        };
        match code {
            KeyCode::Esc | KeyCode::Char(' ') => {
                self.filter_input = None;
                self.set_status_only("Latency filter edit canceled");
            }
            KeyCode::Enter => {
                let value = buffer.trim().to_string();
                self.filter_input = None;
                self.apply_benchmark_filter(value)?;
            }
            KeyCode::Backspace => {
                buffer.pop();
            }
            KeyCode::Char(ch) => buffer.push(ch),
            _ => {}
        }
        Ok(true)
    }

    pub(super) fn handle_bypass_input_key(&mut self, code: KeyCode) -> Result<bool> {
        let Some(buffer) = self.bypass_input.as_mut() else {
            return Ok(true);
        };
        match code {
            KeyCode::Esc | KeyCode::Char(' ') => {
                self.bypass_input = None;
                self.set_status_only("Bypass edit canceled");
            }
            KeyCode::Enter => {
                let value = buffer.clone();
                self.bypass_input = None;
                self.apply_bypass_entries(parse_bypass_entries(&value))?;
            }
            KeyCode::Backspace => {
                buffer.pop();
            }
            KeyCode::Char(ch) => buffer.push(ch),
            _ => {}
        }
        Ok(true)
    }

    pub(super) fn apply_benchmark_filter(&mut self, value: String) -> Result<()> {
        self.benchmark_filter = value;
        self.sync_selection_to_displayed_members();
        self.last_auto_select_benchmark = None;
        if self.benchmark_filter.is_empty() {
            self.set_status_only("Latency filter cleared");
        } else {
            self.set_status_only(format!("Latency filter set to '{}'", self.benchmark_filter));
        }
        self.save_runtime_state()?;
        self.ensure_auto_pick_background_worker_after_state_change()?;
        Ok(())
    }

    pub(super) fn apply_bypass_entries(&mut self, entries: Vec<String>) -> Result<()> {
        self.bypass_entries = entries;
        self.save_runtime_state()?;
        self.save_bypass_rule_set()?;
        if self.bypass_entries.is_empty() {
            self.set_status_only("Bypass list cleared");
        } else {
            self.set_status_only(format!(
                "Bypass list saved ({} entries)",
                self.bypass_entries.len()
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyCode;

    use super::super::test_support::{test_app, test_bypass_rule_set_path, test_state_path};
    use crate::tui_state::TuiStateStore;

    #[test]
    fn filter_modal_edits_submits_and_cancels() {
        let mut app = test_app();
        app.benchmark_filter = "hk".to_string();
        app.handle_key(KeyCode::Char('/')).unwrap();
        assert_eq!(app.filter_input.as_deref(), Some("hk"));
        app.handle_key(KeyCode::Char('x')).unwrap();
        app.handle_key(KeyCode::Esc).unwrap();
        assert_eq!(app.benchmark_filter, "hk");
        assert_eq!(app.status, "Latency filter edit canceled");

        app.handle_key(KeyCode::Char('/')).unwrap();
        app.handle_key(KeyCode::Backspace).unwrap();
        app.handle_key(KeyCode::Backspace).unwrap();
        app.handle_key(KeyCode::Char('u')).unwrap();
        app.handle_key(KeyCode::Char('s')).unwrap();
        app.handle_key(KeyCode::Enter).unwrap();
        assert_eq!(app.benchmark_filter, "us");
        assert_eq!(app.status, "Latency filter set to 'us'");
    }

    #[test]
    fn applying_filter_resynchronizes_visible_selection() {
        let mut app = test_app();
        app.groups[0].members = vec!["hk-1".into(), "us-x2".into(), "hk-2".into()];
        app.member_index = 1;
        app.apply_benchmark_filter("hk,!x2".to_string()).unwrap();
        assert_eq!(app.displayed_members(), vec!["hk-1", "hk-2"]);
        assert_eq!(app.member_index, 0);
        assert_eq!(app.displayed_member_index(), Some(0));
    }

    #[test]
    fn bypass_modal_persists_state_and_rule_set() {
        let state_path = test_state_path();
        let rule_set_path = test_bypass_rule_set_path();
        let mut app = test_app();
        app.state_store = Some(TuiStateStore::new(&state_path));
        app.bypass_rule_set_store = Some(crate::tui_state::BypassRuleSetStore::new(&rule_set_path));
        app.handle_key(KeyCode::Char('b')).unwrap();
        for ch in "example.com,10.0.0.0/8".chars() {
            app.handle_key(KeyCode::Char(ch)).unwrap();
        }
        app.handle_key(KeyCode::Enter).unwrap();
        let state = TuiStateStore::new(&state_path).load().unwrap();
        assert_eq!(state.bypass_entries, vec!["example.com", "10.0.0.0/8"]);
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&rule_set_path).unwrap()).unwrap();
        assert_eq!(
            value["rules"][0]["domain_suffix"],
            serde_json::json!(["example.com"])
        );
        assert_eq!(
            value["rules"][1]["ip_cidr"],
            serde_json::json!(["10.0.0.0/8"])
        );
        let _ = std::fs::remove_file(state_path);
        let _ = std::fs::remove_file(rule_set_path);
    }
}

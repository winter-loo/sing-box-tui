use std::time::Duration;

use anyhow::{Context, Result, bail};

use super::view::{Focus, LeftPaneSection, NodeViewPanel};
use super::{App, DIRECT_CLASH_MODE, GLOBAL_CLASH_MODE, REFRESH_DEBOUNCE, RULE_CLASH_MODE};

fn next_clash_mode(current: Option<&str>, mode_list: &[String]) -> String {
    let modes = if mode_list.is_empty() {
        [GLOBAL_CLASH_MODE, DIRECT_CLASH_MODE, RULE_CLASH_MODE]
            .into_iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
    } else {
        mode_list.to_vec()
    };
    let current_index = current.and_then(|value| modes.iter().position(|mode| mode == value));
    modes
        .get(current_index.map_or(0, |index| (index + 1) % modes.len()))
        .cloned()
        .unwrap_or_else(|| RULE_CLASH_MODE.to_string())
}

#[cfg(test)]
#[path = "tui_selection_navigation_tests.rs"]
mod tests;

impl App {
    pub(super) fn move_next(&mut self) {
        match self.focus {
            Focus::Groups => match self.left_pane_section {
                LeftPaneSection::Internet => {
                    let group_count = self.displayed_group_names().len();
                    if self.displayed_group_index() + 1 < group_count {
                        if self.implicit_root_mode() {
                            self.internet_route_index += 1;
                        } else {
                            self.group_index += 1;
                        }
                        self.sync_member_selection_to_current();
                    } else if self.private_access.is_configured() {
                        self.left_pane_section = LeftPaneSection::Intranet;
                        self.private_access.focused_index = 0;
                        self.intranet_detail_scroll = 0;
                    }
                }
                LeftPaneSection::Intranet => {
                    if self.private_access.focused_index + 1 < self.private_access.profiles.len() {
                        self.private_access.focused_index += 1;
                        self.intranet_detail_scroll = 0;
                    }
                }
            },
            Focus::Members => {
                if self.showing_intranet_details() {
                    let max_scroll = self.intranet_detail_line_count().saturating_sub(1) as u16;
                    self.intranet_detail_scroll = self
                        .intranet_detail_scroll
                        .saturating_add(1)
                        .min(max_scroll);
                    return;
                }
                let members = self.displayed_members();
                if members.is_empty() {
                    return;
                }
                match self.displayed_member_index() {
                    Some(current_index) if current_index + 1 < members.len() => {
                        self.sync_selection_to_member_name(&members[current_index + 1]);
                    }
                    None => self.sync_selection_to_member_name(&members[0]),
                    _ => {}
                }
            }
        }
    }

    pub(super) fn move_previous(&mut self) {
        match self.focus {
            Focus::Groups => match self.left_pane_section {
                LeftPaneSection::Internet => {
                    if self.displayed_group_index() > 0 {
                        if self.implicit_root_mode() {
                            self.internet_route_index -= 1;
                        } else {
                            self.group_index -= 1;
                        }
                        self.sync_member_selection_to_current();
                    }
                }
                LeftPaneSection::Intranet => {
                    if self.private_access.focused_index > 0 {
                        self.private_access.focused_index -= 1;
                        self.intranet_detail_scroll = 0;
                    } else if !self.displayed_group_names().is_empty() {
                        self.left_pane_section = LeftPaneSection::Internet;
                        self.intranet_detail_scroll = 0;
                        if self.implicit_root_mode() {
                            self.internet_route_index =
                                self.displayed_group_names().len().saturating_sub(1);
                        } else {
                            self.group_index = self.groups.len().saturating_sub(1);
                        }
                        self.sync_member_selection_to_current();
                    }
                }
            },
            Focus::Members => {
                if self.showing_intranet_details() {
                    self.intranet_detail_scroll = self.intranet_detail_scroll.saturating_sub(1);
                    return;
                }
                let members = self.displayed_members();
                if members.is_empty() {
                    return;
                }
                match self.displayed_member_index() {
                    Some(current_index) if current_index > 0 => {
                        self.sync_selection_to_member_name(&members[current_index - 1]);
                    }
                    None => self.sync_selection_to_member_name(&members[0]),
                    _ => {}
                }
            }
        }
    }

    pub(super) fn move_first(&mut self) {
        match self.focus {
            Focus::Groups => {
                self.left_pane_section = LeftPaneSection::Internet;
                if self.implicit_root_mode() {
                    self.internet_route_index = 0;
                } else {
                    self.group_index = 0;
                }
                self.sync_member_selection_to_current();
            }
            Focus::Members => {
                if self.showing_intranet_details() {
                    self.intranet_detail_scroll = 0;
                    return;
                }
                if let Some(first) = self.displayed_members().first().cloned() {
                    self.sync_selection_to_member_name(&first);
                }
            }
        }
    }

    pub(super) fn move_last(&mut self) {
        match self.focus {
            Focus::Groups => {
                if self.private_access.is_configured() {
                    self.left_pane_section = LeftPaneSection::Intranet;
                    self.private_access.focused_index =
                        self.private_access.profiles.len().saturating_sub(1);
                    self.intranet_detail_scroll = 0;
                } else if self.implicit_root_mode() {
                    let groups = self.displayed_group_names();
                    if !groups.is_empty() {
                        self.internet_route_index = groups.len() - 1;
                        self.sync_member_selection_to_current();
                    }
                } else if !self.groups.is_empty() {
                    self.group_index = self.groups.len() - 1;
                    self.sync_member_selection_to_current();
                }
            }
            Focus::Members => {
                if self.showing_intranet_details() {
                    self.intranet_detail_scroll =
                        self.intranet_detail_line_count().saturating_sub(1) as u16;
                    return;
                }
                if let Some(last) = self.displayed_members().last().cloned() {
                    self.sync_selection_to_member_name(&last);
                }
            }
        }
    }

    pub(super) fn activate_selection(&mut self) -> Result<()> {
        if self.showing_intranet_details() {
            let profile_id = self.private_access.focused().id.clone();
            self.focus = Focus::Members;
            self.set_status_only(format!(
                "Showing Intranet Proxy details for {profile_id}; press V to connect or disconnect"
            ));
            return Ok(());
        }
        if self.focus == Focus::Groups {
            if self.implicit_root_mode() {
                self.activate_root_choice()?;
            } else {
                self.focus = Focus::Members;
            }
            return Ok(());
        }

        let Some(group) = self.selected_member_panel_group() else {
            bail!("no selector group available");
        };
        let group_name = group.name.clone();
        let group_members = group.members.clone();
        if self.implicit_root_mode() && !self.selected_member_panel_is_manual_selector() {
            self.activate_root_choice()?;
            return Ok(());
        }
        let Some(member) = self.selected_member_name() else {
            self.set_status_only("No node is available in the active node view");
            return Ok(());
        };
        let quality_lease = if self.node_view_panel != NodeViewPanel::CurrentSelector {
            Some(self.benchmark_workflow.acquire_quality_read_lease()?)
        } else {
            None
        };
        if let (Some(manifest_id), Some(lease)) =
            (self.active_usability_manifest_id(), quality_lease.as_ref())
        {
            // Manual selection starts from a render cache, but that cache cannot authorize a PUT.
            // Re-check Included membership under the lease kept alive through both writes so a
            // new run or reconciliation cannot turn a stale visible row into an invalid switch.
            let projection = self.leased_custom_node_view_projection(
                lease,
                &manifest_id,
                &group_name,
                &group_members,
            )?;
            if projection.membership(&member)
                != crate::automatic_selection::PanelMembership::Included
            {
                self.set_status_only(format!(
                    "Selection deferred: {member} is no longer included in {}",
                    projection.label
                ));
                return Ok(());
            }
        }
        let parent_switch = if self.implicit_root_mode() {
            self.selected_root_choice_name().and_then(|choice| {
                self.implicit_root_group()
                    .map(|root| (root.name.clone(), choice))
            })
        } else {
            None
        };
        self.client
            .switch_proxy(&group_name, &member)
            .with_context(|| format!("failed to switch {} to {}", group_name, member))?;
        if let Some((parent, route_group)) = parent_switch {
            self.client
                .switch_proxy(&parent, &route_group)
                .with_context(|| format!("failed to switch {} to {}", parent, route_group))?;
        }
        // The lease must cover membership validation and every selector write, but refresh uses
        // unleased projection reads that acquire the same cross-process lock. Release it before
        // refresh so a custom candidate-panel selection cannot deadlock itself re-entering that
        // lock on the UI thread.
        drop(quality_lease);
        if REFRESH_DEBOUNCE > Duration::ZERO {
            std::thread::sleep(REFRESH_DEBOUNCE);
        }
        self.refresh()?;
        self.save_runtime_state()?;
        self.set_switch_status(&group_name, &member);
        Ok(())
    }

    pub(super) fn cycle_clash_mode(&mut self) -> Result<()> {
        let current = self.clash_mode.as_deref();
        let next = next_clash_mode(current, &self.clash_modes);
        self.client
            .set_mode(&next)
            .with_context(|| format!("failed to switch Clash mode to {next}"))?;
        self.clash_mode = Some(next.clone());
        self.set_status_only(format!("Switched Clash mode to {next}"));
        Ok(())
    }

    fn activate_root_choice(&mut self) -> Result<()> {
        let Some(root) = self.implicit_root_group() else {
            bail!("no implicit root selector available");
        };
        let root_name = root.name.clone();
        let Some(choice) = self.selected_root_choice_name() else {
            bail!("no selectable choice available");
        };
        self.client
            .switch_proxy(&root_name, &choice)
            .with_context(|| format!("failed to switch {} to {}", root_name, choice))?;
        if REFRESH_DEBOUNCE > Duration::ZERO {
            std::thread::sleep(REFRESH_DEBOUNCE);
        }
        self.refresh()?;
        self.save_runtime_state()?;
        self.set_switch_status(&root_name, &choice);
        Ok(())
    }

    pub(super) fn refresh(&mut self) -> Result<()> {
        let previous_group_name = self.selected_group().map(|group| group.name.clone());
        let previous_choice_name = self.selected_root_choice_name();
        let config = self.client.fetch_config()?;
        let groups = self.client.fetch_selector_groups()?;
        if groups.is_empty() {
            bail!("no selector groups returned by controller");
        }
        self.clash_mode = config.mode;
        self.clash_modes = config.mode_list;
        self.groups = groups;
        if self.implicit_root_mode() {
            let choices = self.displayed_group_names();
            self.internet_route_index = previous_choice_name
                .as_ref()
                .and_then(|name| choices.iter().position(|choice| choice == name))
                .or_else(|| {
                    self.implicit_root_group()
                        .and_then(|root| root.current.as_deref())
                        .and_then(|current| choices.iter().position(|choice| choice == current))
                })
                .unwrap_or(0);
        } else {
            self.group_index = previous_group_name
                .and_then(|name| self.groups.iter().position(|group| group.name == name))
                .unwrap_or(0);
            self.internet_route_index = 0;
        }
        self.sync_member_selection_to_current();
        self.refresh_usability_probe_projection_cache();
        self.status = format!("Loaded {} selector groups", self.groups.len());
        Ok(())
    }

    pub(super) fn sync_member_selection_to_current(&mut self) {
        let next_index =
            self.selected_member_panel_group()
                .and_then(|group| {
                    group.current.as_deref().and_then(|current| {
                        group.members.iter().position(|member| member == current)
                    })
                })
                .unwrap_or(0);
        self.member_index = next_index;
        self.sync_selection_to_displayed_members();
    }
}

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use super::App;
use super::view::{
    IntranetDetailSection, IntranetDetailView, LeftPaneSection, NodeViewPanel,
    private_access_detail_view,
};
use crate::controller::{ProxyGroup, matches_filter};
use crate::defaults::DEFAULT_SELECTOR_TAG;
use crate::private_access_session::PrivateAccessProfileRuntime;

impl App {
    pub(super) fn selected_group(&self) -> Option<&ProxyGroup> {
        if self.implicit_root_mode() {
            return self
                .selected_root_choice_name()
                .and_then(|name| self.group_by_name(&name));
        }
        self.groups.get(self.group_index)
    }

    pub(super) fn internet_outbound_context(&self) -> Option<String> {
        let mut current = self
            .implicit_root_group()
            .or_else(|| self.selected_group())?;
        let mut chain = vec![current.name.clone()];
        let mut visited = BTreeSet::new();
        visited.insert(current.name.clone());
        while let Some(selected) = current.current.as_deref() {
            chain.push(selected.to_string());
            let Some(next) = self.group_by_name(selected) else {
                break;
            };
            if !visited.insert(next.name.clone()) {
                chain.push("(cycle)".to_string());
                break;
            }
            current = next;
        }
        (!chain.is_empty()).then(|| chain.join(" -> "))
    }

    pub(super) fn internet_outbound_root_selector(&self) -> Option<String> {
        self.implicit_root_group()
            .or_else(|| self.selected_group())
            .map(|group| group.name.clone())
    }

    pub(super) fn group_by_name(&self, name: &str) -> Option<&ProxyGroup> {
        self.groups.iter().find(|group| group.name == name)
    }

    pub(super) fn implicit_root_group(&self) -> Option<&ProxyGroup> {
        let root = self.group_by_name(DEFAULT_SELECTOR_TAG)?;
        let internet_route_group_count = root
            .members
            .iter()
            .filter(|member| self.is_internet_route_child_group(member))
            .count();
        if internet_route_group_count >= 1 {
            Some(root)
        } else {
            None
        }
    }

    pub(super) fn implicit_root_mode(&self) -> bool {
        self.implicit_root_group().is_some()
    }

    pub(super) fn displayed_group_names(&self) -> Vec<String> {
        if let Some(root) = self.implicit_root_group() {
            return self.internet_route_child_group_names(root);
        }
        self.groups.iter().map(|group| group.name.clone()).collect()
    }

    pub(super) fn displayed_group_index(&self) -> usize {
        if self.implicit_root_mode() {
            self.internet_route_index
        } else {
            self.group_index
        }
    }

    pub(super) fn showing_intranet_details(&self) -> bool {
        self.left_pane_section == LeftPaneSection::Intranet && self.private_access.is_configured()
    }

    pub(super) fn intranet_detail_section_key(
        profile_id: &str,
        section: IntranetDetailSection,
    ) -> String {
        format!("{profile_id}:{}", section.key())
    }

    pub(super) fn intranet_detail_view(
        &self,
        profile: &PrivateAccessProfileRuntime,
    ) -> IntranetDetailView {
        private_access_detail_view(profile, |section| {
            self.expanded_intranet_sections
                .contains(&Self::intranet_detail_section_key(&profile.id, section))
        })
    }

    pub(super) fn intranet_detail_line_count(&self) -> usize {
        self.private_access
            .focused_opt()
            .map(|profile| self.intranet_detail_view(profile).lines.len())
            .unwrap_or(0)
    }

    pub(super) fn toggle_intranet_detail_section(&mut self) {
        let Some(profile) = self.private_access.focused_opt() else {
            return;
        };
        let profile_id = profile.id.clone();
        let view = self.intranet_detail_view(profile);
        let cursor = self.intranet_detail_scroll as usize;
        let Some(range) = view
            .sections
            .iter()
            .find(|range| range.foldable && cursor >= range.start && cursor < range.end)
            .or_else(|| {
                view.sections
                    .iter()
                    .find(|range| range.foldable && range.start >= cursor)
            })
            .or_else(|| view.sections.iter().rev().find(|range| range.foldable))
            .copied()
        else {
            self.set_status_only("No detail section has more than 10 items");
            return;
        };
        let key = Self::intranet_detail_section_key(&profile_id, range.section);
        let expanded = if self.expanded_intranet_sections.remove(&key) {
            false
        } else {
            self.expanded_intranet_sections.insert(key);
            true
        };
        self.intranet_detail_scroll = range.start as u16;
        self.set_status_only(format!(
            "{} {} section for {}",
            if expanded { "Expanded" } else { "Folded" },
            range.section.key(),
            profile_id
        ));
    }

    pub(super) fn selected_root_choice_name(&self) -> Option<String> {
        self.implicit_root_group().and_then(|root| {
            self.internet_route_child_group_names(root)
                .into_iter()
                .nth(self.internet_route_index)
        })
    }

    pub(super) fn internet_route_child_group_names(&self, root: &ProxyGroup) -> Vec<String> {
        root.members
            .iter()
            .filter(|member| self.is_internet_route_child_group(member))
            .cloned()
            .collect()
    }

    pub(super) fn is_internet_route_child_group(&self, member: &str) -> bool {
        self.group_by_name(member)
            .is_some_and(|group| group.kind.eq_ignore_ascii_case("selector"))
    }

    pub(super) fn implicit_root_parent_switch_for_group(
        &self,
        group_name: &str,
    ) -> Option<(String, String)> {
        let root = self.implicit_root_group()?;
        if root.current.as_deref() == Some(group_name) {
            return None;
        }
        if self
            .internet_route_child_group_names(root)
            .iter()
            .any(|route_group| route_group == group_name)
        {
            return Some((root.name.clone(), group_name.to_string()));
        }
        None
    }

    pub(super) fn selected_member_panel_group(&self) -> Option<&ProxyGroup> {
        if self.showing_intranet_details() {
            return None;
        }
        if self.implicit_root_mode() {
            let choice = self.selected_root_choice_name()?;
            return self.group_by_name(&choice);
        }
        self.selected_group()
    }

    pub(super) fn selected_member_panel_is_manual_selector(&self) -> bool {
        self.selected_member_panel_group()
            .is_some_and(|group| group.kind.eq_ignore_ascii_case("selector"))
    }

    pub(super) fn member_matches_filter(&self, member: &str) -> bool {
        matches_filter(member, &self.benchmark_filter)
    }

    pub(super) fn benchmark_candidates_for_group(&self, group: &ProxyGroup) -> Vec<String> {
        group
            .members
            .iter()
            .filter(|member| self.member_matches_filter(member))
            .cloned()
            .collect()
    }

    pub(super) fn displayed_members(&self) -> Vec<String> {
        let Some(group) = self.selected_group() else {
            return Vec::new();
        };
        let group = self.selected_member_panel_group().unwrap_or(group);
        if let Some(manifest_id) = self.active_usability_manifest_id() {
            if let Some((pending, results)) =
                self.custom_usability_live_projection(&manifest_id, &group.name)
            {
                return live_usability_members(
                    &group.members,
                    pending.as_ref().map(|(node, _)| node.as_str()),
                    &results,
                );
            }
            let projection =
                self.cached_custom_node_view_projection(&manifest_id, &group.name, &group.members);
            let mut displayed = group
                .members
                .iter()
                .filter(|member| {
                    projection.membership(member)
                        == crate::automatic_selection::PanelMembership::Included
                })
                .cloned()
                .collect::<Vec<_>>();
            let facts = self
                .benchmark_workflow
                .node_quality_facts_from_cache(&group.name, &group.members);
            let facts = facts
                .iter()
                .map(|facts| (facts.node.as_str(), facts))
                .collect::<BTreeMap<_, _>>();
            displayed.sort_by(|left, right| {
                match (facts.get(left.as_str()), facts.get(right.as_str())) {
                    (Some(left), Some(right)) => crate::automatic_selection::compare_node_quality(
                        projection.ranking_policy,
                        left,
                        right,
                    )
                    .reverse(),
                    (Some(_), None) => Ordering::Less,
                    (None, Some(_)) => Ordering::Greater,
                    (None, None) => Ordering::Equal,
                }
            });
            return displayed;
        }
        group.members.clone()
    }

    pub(super) fn active_usability_manifest_id(
        &self,
    ) -> Option<crate::automatic_selection::NodeViewId> {
        match &self.node_view_panel {
            NodeViewPanel::Streaming => Some(crate::automatic_selection::NodeViewId::streaming()),
            NodeViewPanel::Custom(id) => Some(id.clone()),
            NodeViewPanel::CurrentSelector => None,
        }
    }

    pub(super) fn visible_usability_manifests(
        &self,
    ) -> impl Iterator<Item = &crate::usability_probe::UsabilityProbeManifest> {
        self.usability_probe_manifests
            .iter()
            .filter(|manifest| manifest.visible)
    }

    pub(super) fn node_view_id_is_visible(
        &self,
        id: &crate::automatic_selection::NodeViewId,
    ) -> bool {
        if id == &crate::automatic_selection::NodeViewId::current_selector() {
            return true;
        }
        self.usability_probe_manifests
            .iter()
            .find(|manifest| &manifest.id == id)
            .map(|manifest| manifest.visible)
            // A missing plug-in can be installed again later. Preserve its stable persisted ID;
            // only a manifest that is present and explicitly hidden should be suppressed.
            .unwrap_or(true)
    }

    #[cfg(test)]
    pub(super) fn node_view_counts(&self) -> (usize, usize) {
        let Some(group) = self.selected_member_panel_group() else {
            return (0, 0);
        };
        (
            group.members.len(),
            self.cached_custom_node_view_projection(
                &crate::automatic_selection::NodeViewId::streaming(),
                &group.name,
                &group.members,
            )
            .members
            .values()
            .filter(|membership| {
                **membership == crate::automatic_selection::PanelMembership::Included
            })
            .count(),
        )
    }

    pub(super) fn move_node_view_next(&mut self) {
        let ids = self.node_view_ids();
        let next = ids
            .iter()
            .position(|id| id == &self.node_view_panel.id())
            .map_or(0, |index| (index + 1) % ids.len());
        self.node_view_panel = self.node_view_panel_from_index(next);
        self.finish_node_view_change();
    }

    pub(super) fn move_node_view_previous(&mut self) {
        let ids = self.node_view_ids();
        let previous = ids
            .iter()
            .position(|id| id == &self.node_view_panel.id())
            .map_or(ids.len() - 1, |index| (index + ids.len() - 1) % ids.len());
        self.node_view_panel = self.node_view_panel_from_index(previous);
        self.finish_node_view_change();
    }

    fn finish_node_view_change(&mut self) {
        self.sync_selection_to_displayed_members();
        let status = format!("Node view: {}", self.active_node_view_label());
        self.auto_select_node_view = self.node_view_panel.id();
        self.auto_select_ranking_policy = self.active_node_view_ranking_policy();
        // A panel is a candidate boundary. Never carry confirmation or traffic evidence across a
        // boundary change, even when both panels currently happen to show the same nodes.
        self.automatic_selection_state = Default::default();
        self.active_node_traffic = Default::default();
        self.last_auto_selection_explanation = None;
        self.last_auto_select_benchmark = None;
        if let Err(error) = self
            .save_runtime_state()
            .and_then(|_| self.ensure_auto_pick_background_worker_after_state_change())
        {
            self.set_status_with_flash(format!("{status}; failed to persist node view: {error:#}"));
        } else {
            self.set_status_only(status);
        }
    }

    fn node_view_ids(&self) -> Vec<crate::automatic_selection::NodeViewId> {
        std::iter::once(crate::automatic_selection::NodeViewId::current_selector())
            .chain(
                self.visible_usability_manifests()
                    .map(|manifest| manifest.id.clone()),
            )
            .collect()
    }

    fn node_view_panel_from_index(&self, index: usize) -> NodeViewPanel {
        self.node_view_ids()
            .get(index)
            .map(NodeViewPanel::from_id)
            .unwrap_or_default()
    }

    pub(super) fn active_node_view_label(&self) -> String {
        match &self.node_view_panel {
            NodeViewPanel::CurrentSelector => "current selector".to_string(),
            NodeViewPanel::Streaming => self
                .usability_probe_manifests
                .iter()
                .find(|manifest| manifest.id == crate::automatic_selection::NodeViewId::streaming())
                .map(|manifest| manifest.label.clone())
                .unwrap_or_else(|| "Streaming".to_string()),
            NodeViewPanel::Custom(id) => self
                .usability_probe_manifests
                .iter()
                .find(|manifest| &manifest.id == id)
                .map(|manifest| manifest.label.clone())
                .unwrap_or_else(|| format!("unavailable custom criterion ({id})")),
        }
    }

    pub(super) fn active_node_view_ranking_policy(
        &self,
    ) -> crate::automatic_selection::RankingPolicy {
        self.active_usability_manifest_id()
            .and_then(|id| {
                self.usability_probe_manifests
                    .iter()
                    .find(|manifest| manifest.id == id)
                    .map(|manifest| manifest.ranking_policy)
            })
            .or_else(|| self.node_view_panel.builtin_ranking_policy())
            .unwrap_or(crate::automatic_selection::RankingPolicy::Balanced)
    }

    pub(super) fn displayed_member_index(&self) -> Option<usize> {
        let members = self.displayed_members();
        let current = self
            .selected_member_panel_group()?
            .members
            .get(self.member_index)?;
        members.iter().position(|member| member == current)
    }

    pub(super) fn sync_selection_to_member_name(&mut self, name: &str) {
        if let Some(group) = self.selected_member_panel_group()
            && let Some(index) = group.members.iter().position(|member| member == name)
        {
            self.member_index = index;
        }
    }

    pub(super) fn sync_selection_to_displayed_members(&mut self) {
        let displayed = self.displayed_members();
        if displayed.is_empty() {
            return;
        }

        let current = self
            .selected_member_panel_group()
            .and_then(|group| group.members.get(self.member_index))
            .cloned();
        if current
            .as_ref()
            .is_some_and(|member| displayed.iter().any(|item| item == member))
        {
            return;
        }

        if let Some(first) = displayed.first() {
            let next = first.clone();
            self.sync_selection_to_member_name(&next);
        }
    }
}

pub(super) fn live_usability_members(
    selector_members: &[String],
    current_node: Option<&str>,
    results: &BTreeMap<String, crate::usability_probe::UsabilityProbeNodeResult>,
) -> Vec<String> {
    selector_members
        .iter()
        .filter(|member| {
            current_node == Some(member.as_str())
                || results
                    .get(member.as_str())
                    .is_some_and(|result| result.usable)
        })
        .cloned()
        .collect()
}

#[cfg(test)]
#[path = "tui_selection_model_tests.rs"]
mod tests;

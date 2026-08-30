use std::time::Duration;

use anyhow::Result;

use super::network_mode::apply_internet_tun_persistence;
use super::private_access_workflow::private_access_process_exists;
use super::settings::normalize_optional_setting;
use super::view::{LeftPaneSection, NodeViewPanel, truncate_for_width};
use super::{App, REFRESH_DEBOUNCE, process_exists};
use crate::config::{
    TailscaleConfigOptions, config_has_china_ip_routing, inspect_tailscale_config,
    set_china_ip_routing, set_tailscale_config,
};
use crate::tui_state::TuiRuntimeState;

#[cfg(test)]
#[path = "tui_runtime_state_tests.rs"]
mod tests;

impl App {
    pub(super) fn reconcile_persisted_tun_mode(
        &mut self,
        runtime_state: &mut TuiRuntimeState,
    ) -> Result<()> {
        let state_store = self.state_store.clone();
        let mut persisted_runtime = self.runtime_state();
        self.internet_tun.reconcile(|tun| {
            apply_internet_tun_persistence(&mut persisted_runtime, tun);
            if let Some(store) = &state_store {
                store.save(&persisted_runtime)?;
            }
            Ok(())
        })?;
        apply_internet_tun_persistence(runtime_state, self.internet_tun.persisted());
        Ok(())
    }

    /// Re-applies an explicit China IP routing choice to the config when it drifted, e.g. after a
    /// subscription refresh regenerated the config without the geoip/geosite rule-sets.
    pub(super) fn reconcile_persisted_china_ip_routing(&self) -> Result<()> {
        if !self.china_ip_routing_explicit || !self.system_proxy_config_path.exists() {
            return Ok(());
        }
        let in_config =
            config_has_china_ip_routing(&self.system_proxy_config_path).unwrap_or(false);
        if in_config != self.china_ip_routing_enabled {
            set_china_ip_routing(
                &self.system_proxy_config_path,
                self.china_ip_routing_enabled,
            )?;
        }
        Ok(())
    }

    pub(super) fn reconcile_persisted_tailscale(&self) -> Result<()> {
        if !self.tailscale_explicit || !self.system_proxy_config_path.exists() {
            return Ok(());
        }
        let current = inspect_tailscale_config(&self.system_proxy_config_path).unwrap_or_default();
        let desired_domain = self.tailscale_tailnet_domain.trim();
        let desired_hostname = normalize_optional_setting(Some(self.tailscale_hostname.clone()));
        if current.enabled != self.tailscale_enabled
            || (self.tailscale_enabled
                && (current.tailnet_domain.as_deref() != Some(desired_domain)
                    || current.hostname != desired_hostname))
        {
            let options = self.tailscale_enabled.then(|| TailscaleConfigOptions {
                tailnet_domain: desired_domain.to_string(),
                hostname: desired_hostname,
            });
            set_tailscale_config(&self.system_proxy_config_path, options)?;
        }
        Ok(())
    }

    /// Prompts for sudo credentials before the first elevated sing-box restart. `start_managed_sing_box`
    /// uses `sudo -n`, which never prompts, so a config that already has a TUN inbound would fail to
    /// launch sing-box once the cached sudo timestamp expires. Running `sudo -v` here re-authorizes
    /// interactively while the terminal is still in its normal (non-raw) mode.
    pub(super) fn apply_runtime_state(&mut self, state: TuiRuntimeState) -> Result<()> {
        self.private_access
            .apply_state(&state, private_access_process_exists)?;
        if !self.private_access.is_configured() {
            self.left_pane_section = LeftPaneSection::Internet;
            self.intranet_detail_scroll = 0;
        }
        self.benchmark_filter = state.benchmark_filter;
        self.auto_select_enabled = state.auto_pick_enabled;
        self.auto_select_selector = state.auto_pick_selector;
        let active_node_view = state.active_node_view.unwrap_or_default();
        self.node_view_panel = NodeViewPanel::from_id(&active_node_view);
        // Preserve the stable ID even when its manifest is temporarily missing. Manifest ordering
        // is presentation only; falling back by index could silently authorize another panel.
        self.auto_select_node_view = active_node_view;
        self.background_probe_enabled = state.background_probe_enabled;
        self.background_probe_selectors = state.background_probe_selectors;
        // Persisted user consent is necessary but never sufficient. Revalidate the manifest's
        // current permission and selector target on every startup so a removed/edited manifest
        // cannot keep a stale paid probe authorization alive.
        self.background_probe_enabled.retain(|id| {
            self.usability_probe_manifests
                .iter()
                .any(|manifest| &manifest.id == id && manifest.background)
                && self
                    .background_probe_selectors
                    .get(id)
                    .is_some_and(|selector| self.groups.iter().any(|group| &group.name == selector))
        });
        self.background_probe_selectors
            .retain(|id, _| self.background_probe_enabled.contains(id));
        self.auto_select_ranking_policy = self.active_node_view_ranking_policy();
        self.bypass_entries = state.bypass_entries;
        if let Some(value) = state.benchmark_url.filter(|value| !value.trim().is_empty()) {
            self.benchmark_url = value;
        }
        if let Some(value) = state
            .sustained_target_url
            .filter(|value| !value.trim().is_empty())
        {
            crate::sustained_quality::validate_sustained_target(&value)?;
            self.sustained_target_url = value;
        }
        self.benchmark_workflow
            .activate_sustained_target(&self.sustained_target_url)?;
        if let Some(value) = state.benchmark_timeout_ms.filter(|value| *value > 0) {
            self.benchmark_timeout_ms = value;
        }
        if let Some(value) = state.benchmark_request_timeout.filter(|value| *value > 0.0) {
            self.benchmark_request_timeout = value;
        }
        if let Some(value) = state.benchmark_max_concurrency.filter(|value| *value > 0) {
            self.benchmark_max_concurrency = value;
        }
        if let Some(value) = normalize_optional_setting(state.verify_targets) {
            self.verify_targets = value;
        }
        if let Some(value) = state.auto_select_interval_secs.filter(|value| *value > 0) {
            self.auto_select_interval = Duration::from_secs(value);
        }
        if let Some(value) = state
            .system_proxy_server
            .clone()
            .filter(|value| !value.trim().is_empty())
        {
            self.system_proxy
                .restore_server(value, state.system_proxy_server_override);
        }
        self.system_proxy
            .restore_enabled_intent(state.system_proxy_enabled);
        if let Some(value) = state.china_ip_routing_enabled {
            self.china_ip_routing_enabled = value;
            self.china_ip_routing_explicit = true;
        }
        if let Some(value) = state.tailscale_enabled {
            self.tailscale_enabled = value;
            self.tailscale_explicit = true;
        }
        if let Some(value) = normalize_optional_setting(state.tailscale_tailnet_domain) {
            self.tailscale_tailnet_domain = value;
        }
        if let Some(value) = state.tailscale_hostname {
            self.tailscale_hostname = value;
        }
        self.last_auto_select_benchmark = None;
        // Confirmation and traffic windows are observational process state. Restoring either
        // would turn pre-restart evidence into a false consecutive round or a false idle window.
        self.automatic_selection_state = Default::default();
        self.active_node_traffic = Default::default();
        self.last_auto_selection_explanation = None;
        if let Some(group) = self.selected_group()
            && let Some(node) = state.current_selected_nodes.get(&group.name)
        {
            let node = node.clone();
            self.sync_selection_to_member_name(&node);
        }
        self.sync_selection_to_displayed_members();
        Ok(())
    }

    pub(super) fn runtime_state(&self) -> TuiRuntimeState {
        let persisted_tun = self.internet_tun.persisted();
        TuiRuntimeState {
            benchmark_filter: self.benchmark_filter.clone(),
            auto_pick_enabled: self.auto_select_enabled,
            auto_pick_selector: self.auto_select_selector.clone(),
            active_node_view: Some(self.auto_select_node_view.clone()),
            background_probe_enabled: self.background_probe_enabled.clone(),
            background_probe_selectors: self.background_probe_selectors.clone(),
            current_selected_nodes: self
                .groups
                .iter()
                .filter_map(|group| {
                    group
                        .current
                        .as_ref()
                        .map(|current| (group.name.clone(), current.clone()))
                })
                .collect(),
            bypass_entries: self.bypass_entries.clone(),
            onboarding_complete: self.onboarding_complete,
            benchmark_url: Some(self.benchmark_url.clone()),
            sustained_target_url: Some(self.sustained_target_url.clone()),
            benchmark_timeout_ms: Some(self.benchmark_timeout_ms),
            benchmark_request_timeout: Some(self.benchmark_request_timeout),
            benchmark_max_concurrency: Some(self.benchmark_max_concurrency),
            verify_targets: normalize_optional_setting(Some(self.verify_targets.clone())),
            auto_select_interval_secs: Some(self.auto_select_interval.as_secs()),
            system_proxy_server: Some(self.system_proxy.server().to_string()),
            system_proxy_server_override: self.system_proxy.server_is_overridden(),
            system_proxy_enabled: self.system_proxy.persisted_enabled(),
            tun_enabled: persisted_tun.enabled(),
            tun_auto_detect_interface_before_enable: persisted_tun.restore_auto_detect_interface(),
            china_ip_routing_enabled: self
                .china_ip_routing_explicit
                .then_some(self.china_ip_routing_enabled),
            tailscale_enabled: self.tailscale_explicit.then_some(self.tailscale_enabled),
            tailscale_tailnet_domain: normalize_optional_setting(Some(
                self.tailscale_tailnet_domain.clone(),
            )),
            tailscale_hostname: normalize_optional_setting(Some(self.tailscale_hostname.clone())),
            private_access_profiles: self.private_access.runtime_states(process_exists),
        }
    }

    pub(super) fn save_runtime_state(&self) -> Result<()> {
        let Some(store) = &self.state_store else {
            return Ok(());
        };
        store.save(&self.runtime_state())
    }

    pub(super) fn persisted_selection_restore_plan(
        &self,
        state: &TuiRuntimeState,
    ) -> Vec<(String, String)> {
        self.groups
            .iter()
            .filter(|group| group.kind.eq_ignore_ascii_case("selector"))
            .filter_map(|group| {
                let node = state.current_selected_nodes.get(&group.name)?;
                if group.current.as_ref() == Some(node) {
                    return None;
                }
                if !group.members.iter().any(|member| member == node) {
                    return None;
                }
                Some((group.name.clone(), node.clone()))
            })
            .collect()
    }

    pub(super) fn restore_persisted_selections(&mut self, state: &TuiRuntimeState) -> Result<()> {
        let plan = self.persisted_selection_restore_plan(state);
        if plan.is_empty() {
            return Ok(());
        }

        let mut restored = 0usize;
        let mut failures = Vec::new();
        for (group, node) in plan {
            match self.client.switch_proxy(&group, &node) {
                Ok(()) => restored += 1,
                Err(error) => failures.push(format!("{group} -> {node}: {error}")),
            }
        }

        if restored > 0 {
            if REFRESH_DEBOUNCE > Duration::ZERO {
                std::thread::sleep(REFRESH_DEBOUNCE);
            }
            self.refresh()?;
        }

        if failures.is_empty() {
            if restored > 0 {
                self.set_status_only(format!("Restored {restored} saved selector selection(s)"));
            }
        } else {
            let detail = truncate_for_width(&failures.join("; "), 90);
            self.set_status_only(format!(
                "Restored {restored} saved selector selection(s); failed: {detail}"
            ));
        }

        Ok(())
    }

    pub(super) fn save_bypass_rule_set(&self) -> Result<()> {
        let Some(store) = &self.bypass_rule_set_store else {
            return Ok(());
        };
        store.save(&self.bypass_entries)
    }
}

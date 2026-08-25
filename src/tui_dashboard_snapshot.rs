use super::App;
use super::settings::{settings_field_display_value, visible_settings_fields};
use super::view::{
    CandidateRow, CandidateTone, ConnectionsPanelSnapshot, DashboardSnapshot, Focus, InternetRow,
    IntranetDetailSnapshot, IntranetRow, SettingRow, SettingsPanelSnapshot, StatusFooter,
    StatusSnapshot, node_order_badge, pick_mode_badge, settings_field_label,
};

impl App {
    pub(super) fn view_snapshot(&mut self) -> DashboardSnapshot<'_> {
        let flash = self.flash_message();
        let implicit_root_mode = self.implicit_root_mode();
        let implicit_current = self
            .implicit_root_group()
            .and_then(|root| root.current.as_deref());
        let internet_rows = self
            .displayed_group_names()
            .into_iter()
            .map(|name| {
                let current = self
                    .group_by_name(&name)
                    .and_then(|group| group.current.as_deref())
                    .unwrap_or("unset")
                    .to_string();
                let is_current = implicit_root_mode && implicit_current == Some(name.as_str());
                InternetRow {
                    name,
                    current,
                    is_current,
                }
            })
            .collect();
        let intranet_rows = self
            .private_access
            .profiles
            .iter()
            .map(|profile| IntranetRow {
                id: profile.id.clone(),
                state: profile.state.clone(),
                background: profile.background_pid.is_some(),
            })
            .collect();

        let displayed_members = self.displayed_members();
        let selected_group = self.selected_member_panel_group();
        let selected_benchmark = self.selected_benchmark();
        let candidate_rows = selected_group
            .map(|group| {
                displayed_members
                    .iter()
                    .map(|member| {
                        let result =
                            selected_benchmark.and_then(|summary| summary.find_result(member));
                        let (marker, tone) = match result {
                            Some(result) if !result.completed => {
                                (result.display_delay(), CandidateTone::Pending)
                            }
                            Some(result) if result.delay.is_some() => {
                                (result.display_delay(), CandidateTone::Success)
                            }
                            Some(result) => (result.display_delay(), CandidateTone::Error),
                            None => ("-".to_string(), CandidateTone::Missing),
                        };
                        CandidateRow {
                            name: member.clone(),
                            is_current: group.current.as_deref() == Some(member.as_str()),
                            marker,
                            tone,
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        let candidate_title = selected_group
            .map(|group| {
                format!(
                    "Candidates for {} [{}]",
                    group.name,
                    node_order_badge(self.benchmark_workflow.latency_order())
                )
            })
            .unwrap_or_else(|| {
                format!(
                    "Candidates [{}]",
                    node_order_badge(self.benchmark_workflow.latency_order())
                )
            });

        let showing_intranet_details = self.showing_intranet_details();
        let intranet_detail = if showing_intranet_details {
            self.private_access
                .focused_opt()
                .map(|profile| IntranetDetailSnapshot {
                    profile,
                    expanded_sections: &self.expanded_intranet_sections,
                    scroll: self.intranet_detail_scroll,
                    active: self.focus == Focus::Members,
                })
        } else {
            None
        };

        let mut selection_context = format!(
            "clash={}  Pick={}  filter='{}'",
            self.clash_mode_label(),
            pick_mode_badge(self.auto_select_enabled),
            self.benchmark_filter
        );
        if showing_intranet_details {
            selection_context.push_str("  Intranet details are shown in the right panel");
        }
        let footer = if let Some(input) = self.filter_input.as_ref() {
            StatusFooter::Filter(input.clone())
        } else if let Some(input) = self.bypass_input.as_ref() {
            StatusFooter::Bypass(input.clone())
        } else {
            StatusFooter::Status(self.status_line())
        };
        let status = StatusSnapshot {
            system_proxy_enabled: self.system_proxy.enabled(),
            tun_enabled: self.internet_tun.is_enabled(),
            selection_context,
            connections: self.connections_summary_line(),
            subscription: self.subscription_summary_line(),
            sing_box: self.sing_box_summary_line(),
            footer,
        };

        let connections = self.show_connections.then(|| ConnectionsPanelSnapshot {
            summary: self.connections_summary_line(),
            connections: &self.connections,
            error: self.connection_error.as_deref(),
        });
        let settings = self.show_settings.then(|| {
            let fields = visible_settings_fields(self);
            let rows = fields
                .iter()
                .map(|field| SettingRow {
                    label: settings_field_label(*field),
                    value: settings_field_display_value(self, *field),
                })
                .collect::<Vec<_>>();
            let editing = self
                .settings_edit
                .as_ref()
                .map(|edit| (settings_field_label(edit.field), edit.input.clone()));
            let error = self
                .settings_edit
                .as_ref()
                .and_then(|edit| edit.error.clone())
                .or_else(|| self.settings_error.clone());
            SettingsPanelSnapshot {
                selected: self.settings_index.min(rows.len().saturating_sub(1)),
                rows,
                editing,
                error,
            }
        });

        DashboardSnapshot {
            focus: self.focus,
            left_pane_section: self.left_pane_section,
            internet_rows,
            internet_selected: self.displayed_group_index(),
            intranet_rows,
            intranet_selected: self.private_access.focused_index,
            candidate_title,
            candidate_rows,
            candidate_selected: self.displayed_member_index(),
            intranet_detail,
            status,
            flash,
            latency_chart: self.latency_chart.as_ref(),
            connections,
            help_index: self.show_help.then_some(self.help_index),
            settings,
            onboarding: self.onboarding.as_ref(),
            private_access_progress: self.private_access_progress.as_ref(),
            private_access_auth: self.private_access_auth.as_ref(),
        }
    }
}

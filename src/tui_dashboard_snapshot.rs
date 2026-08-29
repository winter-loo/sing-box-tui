use super::App;
use super::settings::{settings_field_display_value, visible_settings_fields};
use super::view::{
    CandidateRow, CandidateTone, ConnectionsPanelSnapshot, DashboardSnapshot, Focus, InternetRow,
    IntranetDetailSnapshot, IntranetRow, NodeViewPanel, NodeViewTab, SettingRow,
    SettingsPanelSnapshot, StatusFooter, StatusSnapshot, node_order_badge, pick_mode_badge,
    settings_field_label,
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

        let selected_group = self.selected_member_panel_group();
        let streaming_projection = selected_group
            .map(|group| {
                self.benchmark_workflow
                    .streaming_projection(&group.name, &group.members)
            })
            .unwrap_or_default();
        // Streaming names and row evidence must come from the same leased projection. Re-reading
        // fenced state per row would turn a normal cross-process generation change into a panic.
        let displayed_members = if self.node_view_panel == NodeViewPanel::Streaming {
            streaming_projection
                .iter()
                .map(|projection| projection.name.clone())
                .collect()
        } else {
            self.displayed_members()
        };
        let selected_benchmark = self.selected_benchmark();
        let candidate_rows = selected_group
            .map(|group| {
                if self.node_view_panel == NodeViewPanel::Streaming {
                    return streaming_projection
                        .iter()
                        .map(|projection| {
                            let (reachability, _) = split_reachability_evidence(
                                &projection.assessment.compact_evidence(),
                            );
                            let p95 = projection
                                .quick_history
                                .p95_ms
                                .map(|value| format!("p95 {value}ms"))
                                .unwrap_or_else(|| "p95 -".to_string());
                            let cold = projection
                                .quick_history
                                .cold_start_ms
                                .map(|value| format!("cold {value}ms"))
                                .unwrap_or_else(|| "cold -".to_string());
                            let throughput = format!(
                                "{:.1} MiB/s",
                                projection.completion.throughput_bytes_per_second as f64
                                    / (1024.0 * 1024.0)
                            );
                            CandidateRow {
                                name: projection.name.clone(),
                                is_current: group.current.as_deref()
                                    == Some(projection.name.as_str()),
                                reachability,
                                marker: format!(
                                    "{throughput} · {}/{} sustained · {p95} · {cold}",
                                    projection.sustained_stats.successes,
                                    projection.sustained_stats.attempts,
                                ),
                                compact_marker: throughput.replace(" MiB/s", "M/s"),
                                tone: CandidateTone::Success,
                            }
                        })
                        .collect();
                }
                displayed_members
                    .iter()
                    .map(|member| {
                        let assessment = self
                            .benchmark_workflow
                            .reachability_assessment(&group.name, member);
                        let result =
                            selected_benchmark.and_then(|summary| summary.find_result(member));
                        let (reachability, marker, tone) = if let Some(assessment) = assessment {
                            let tone = match assessment.assessment {
                                Some(
                                    crate::controller::ReachabilityAssessment::StableReachable,
                                )
                                | Some(crate::controller::ReachabilityAssessment::Reachable) => {
                                    CandidateTone::Success
                                }
                                Some(crate::controller::ReachabilityAssessment::Degraded)
                                | Some(crate::controller::ReachabilityAssessment::Unreachable) => {
                                    CandidateTone::Error
                                }
                                None => CandidateTone::Missing,
                            };
                            let (reachability, marker) =
                                split_reachability_evidence(&assessment.compact_evidence());
                            (reachability, marker, tone)
                        } else {
                            match result {
                                Some(result) if !result.completed => {
                                    ("-/3".into(), result.display_delay(), CandidateTone::Pending)
                                }
                                Some(result) if result.delay.is_some() => {
                                    ("-/3".into(), result.display_delay(), CandidateTone::Success)
                                }
                                Some(result) => {
                                    ("-/3".into(), result.display_delay(), CandidateTone::Error)
                                }
                                None => ("-/3".into(), "-".to_string(), CandidateTone::Missing),
                            }
                        };
                        CandidateRow {
                            name: member.clone(),
                            is_current: group.current.as_deref() == Some(member.as_str()),
                            reachability,
                            marker,
                            compact_marker: String::new(),
                            tone,
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        let candidate_title = selected_group
            .map(|group| {
                let order = match self.node_view_panel {
                    NodeViewPanel::CurrentSelector => {
                        node_order_badge(self.benchmark_workflow.latency_order())
                    }
                    NodeViewPanel::Streaming => "THROUGHPUT",
                };
                format!("Candidates for {} [{order}]", group.name)
            })
            .unwrap_or_else(|| {
                format!(
                    "Candidates [{}]",
                    node_order_badge(self.benchmark_workflow.latency_order())
                )
            });
        let all_count = selected_group.map_or(0, |group| group.members.len());
        let streaming_count = streaming_projection.len();
        let node_view_tabs = vec![
            NodeViewTab {
                label: "Current selector",
                count: all_count,
            },
            NodeViewTab {
                label: "Streaming",
                count: streaming_count,
            },
        ];

        let candidate_selected = if self.node_view_panel == NodeViewPanel::Streaming {
            selected_group
                .and_then(|group| group.members.get(self.member_index))
                .and_then(|current| {
                    displayed_members
                        .iter()
                        .position(|member| member == current)
                })
        } else {
            self.displayed_member_index()
        };
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
            node_view_tabs,
            active_node_view_tab: self.node_view_panel.index(),
            candidate_rows,
            candidate_selected,
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

fn split_reachability_evidence(value: &str) -> (String, String) {
    value
        .split_once(' ')
        .map(|(ratio, detail)| (ratio.to_string(), detail.to_string()))
        .unwrap_or_else(|| (value.to_string(), String::new()))
}

#[cfg(test)]
mod tests {
    use super::super::test_support::test_app;
    use super::super::view::CandidateTone;
    use crate::controller::{NodeReachabilityAssessment, ProbeOutcome, ReachabilityAssessment};

    #[test]
    fn application_snapshot_exposes_compact_reachability_evidence() {
        let mut app = test_app();
        app.benchmark_filter.clear();
        app.benchmark_workflow.set_reachability_assessment(
            "select",
            NodeReachabilityAssessment {
                name: "node-a".into(),
                attempts: vec![
                    ProbeOutcome::Reachable { delay_ms: 40 },
                    ProbeOutcome::Reachable { delay_ms: 50 },
                    ProbeOutcome::Timeout,
                ],
                assessment: Some(ReachabilityAssessment::Reachable),
            },
        );

        let snapshot = app.view_snapshot();
        assert_eq!(snapshot.candidate_rows[0].reachability, "2/3");
        assert_eq!(snapshot.candidate_rows[0].marker, "reachable");
        assert_eq!(snapshot.candidate_rows[0].tone, CandidateTone::Success);
        assert_eq!(snapshot.candidate_rows.len(), app.groups[0].members.len());
    }
}

use super::App;
use super::settings::{settings_field_display_value, visible_settings_fields};
use super::view::{
    CandidateRow, CandidateTone, ConnectionsPanelSnapshot, DashboardSnapshot, Focus, InternetRow,
    IntranetDetailSnapshot, IntranetRow, NodeViewPanel, NodeViewTab, SettingRow,
    SettingsPanelSnapshot, StatusFooter, StatusSnapshot, pick_mode_badge, settings_field_label,
    truncate_for_width,
};
use crate::benchmark_workflow::{ActiveQuickProbe, BenchmarkWorkflow};

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
        let custom_run = match (&self.node_view_panel, selected_group) {
            (NodeViewPanel::Custom(id), Some(group)) => {
                self.custom_usability_run(id, &group.name, &group.members)
            }
            _ => None,
        };
        // Streaming names and row evidence must come from the same leased projection. Re-reading
        // fenced state per row would turn a normal cross-process generation change into a panic.
        let displayed_members = match &self.node_view_panel {
            NodeViewPanel::Streaming => streaming_projection
                .iter()
                .map(|projection| projection.name.clone())
                .collect(),
            NodeViewPanel::Custom(_) | NodeViewPanel::CurrentSelector => self.displayed_members(),
        };
        let candidate_rows = selected_group
            .map(|group| {
                if self.node_view_panel == NodeViewPanel::Streaming {
                    return streaming_projection
                        .iter()
                        .map(|projection| {
                            let (reachability, _) = split_reachability_evidence(
                                &projection.assessment.compact_evidence(),
                            );
                            let active = active_quick_overlay(
                                &self.benchmark_workflow,
                                &group.name,
                                &projection.name,
                            );
                            let reachability = active
                                .as_ref()
                                .map(|(reachability, _)| reachability.clone())
                                .unwrap_or(reachability);
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
                                tone: if active.is_some() {
                                    CandidateTone::Pending
                                } else {
                                    CandidateTone::Success
                                },
                            }
                        })
                        .collect();
                }
                if let NodeViewPanel::Custom(_) = &self.node_view_panel {
                    let results = custom_run
                        .as_ref()
                        .map(|run| {
                            run.results
                                .iter()
                                .map(|result| (result.node.as_str(), result))
                                .collect::<std::collections::BTreeMap<_, _>>()
                        })
                        .unwrap_or_default();
                    return displayed_members
                        .iter()
                        .filter_map(|member| {
                            let result = results.get(member.as_str())?;
                            let assessment = self
                                .benchmark_workflow
                                .reachability_assessment(&group.name, member);
                            let reachability = assessment
                                .map(|assessment| {
                                    split_reachability_evidence(&assessment.compact_evidence()).0
                                })
                                .unwrap_or_else(|| "-/3".to_string());
                            let active =
                                active_quick_overlay(&self.benchmark_workflow, &group.name, member);
                            let reachability = active
                                .as_ref()
                                .map(|(reachability, _)| reachability.clone())
                                .unwrap_or(reachability);
                            let marker = result
                                .detail
                                .clone()
                                .unwrap_or_else(|| "criterion accepted".to_string());
                            Some(CandidateRow {
                                name: member.clone(),
                                is_current: group.current.as_deref() == Some(member.as_str()),
                                reachability,
                                compact_marker: "usable".to_string(),
                                marker,
                                tone: if active.is_some() {
                                    CandidateTone::Pending
                                } else {
                                    CandidateTone::Success
                                },
                            })
                        })
                        .collect();
                }
                displayed_members
                    .iter()
                    .map(|member| {
                        let stored_assessment = self
                            .benchmark_workflow
                            .reachability_assessment(&group.name, member);
                        // WHY: an active run is the current observation. It must cover stored
                        // evidence so reruns cannot present an old result as live progress.
                        let active =
                            active_quick_overlay(&self.benchmark_workflow, &group.name, member);
                        let (reachability, marker, tone) = if let Some((reachability, marker)) =
                            active
                        {
                            (
                                reachability,
                                marker.unwrap_or_else(|| "...".to_string()),
                                CandidateTone::Pending,
                            )
                        } else if let Some(assessment) = stored_assessment {
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
                            ("-/3".into(), "-".to_string(), CandidateTone::Missing)
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
                let order = match &self.node_view_panel {
                    NodeViewPanel::CurrentSelector => "SELECTOR ORDER",
                    NodeViewPanel::Streaming => "THROUGHPUT",
                    NodeViewPanel::Custom(id) => self
                        .usability_probe_manifests
                        .iter()
                        .find(|manifest| &manifest.id == id)
                        .map(|manifest| manifest.ranking_policy.badge())
                        .unwrap_or("CUSTOM"),
                };
                let progress = match &self.node_view_panel {
                    NodeViewPanel::Custom(id) => {
                        if let Some((received, total)) =
                            self.custom_usability_probe_progress(id, &group.name)
                        {
                            format!(" · PROBING {received}/{total}")
                        } else {
                            let Some(run) = custom_run.as_ref() else {
                                return format!(
                                    "Candidates for {} [{order}] · UNTESTED",
                                    group.name
                                );
                            };
                            let mut state = Vec::new();
                            if self.custom_usability_run_is_expired(run) {
                                state.push("EXPIRED".to_string());
                            }
                            if let Some(failure) = self.custom_usability_latest_failure(run) {
                                state.push(format!("FAILED {}", truncate_for_width(&failure, 48)));
                            }
                            if state.is_empty() {
                                String::new()
                            } else {
                                format!(" · {}", state.join(" · "))
                            }
                        }
                    }
                    _ => String::new(),
                };
                format!("Candidates for {} [{order}]{progress}", group.name)
            })
            .unwrap_or_else(|| "Candidates [SELECTOR ORDER]".to_string());
        let all_count = selected_group.map_or(0, |group| group.members.len());
        let streaming_count = streaming_projection.len();
        let mut node_view_tabs = vec![
            NodeViewTab {
                label: "Current selector".to_string(),
                count: all_count,
            },
            NodeViewTab {
                label: "Streaming".to_string(),
                count: streaming_count,
            },
        ];
        if let Some(group) = selected_group {
            node_view_tabs.extend(self.usability_probe_manifests.iter().map(|manifest| {
                let projection = self.cached_custom_node_view_projection(
                    &manifest.id,
                    &group.name,
                    &group.members,
                );
                NodeViewTab {
                    label: manifest.label.clone(),
                    count: projection
                        .members
                        .values()
                        .filter(|membership| {
                            **membership == crate::automatic_selection::PanelMembership::Included
                        })
                        .count(),
                }
            }));
        } else {
            node_view_tabs.extend(self.usability_probe_manifests.iter().map(|manifest| {
                NodeViewTab {
                    label: manifest.label.clone(),
                    count: 0,
                }
            }));
        }
        let active_node_view_tab = match &self.node_view_panel {
            NodeViewPanel::CurrentSelector => 0,
            NodeViewPanel::Streaming => 1,
            NodeViewPanel::Custom(id) => self
                .usability_probe_manifests
                .iter()
                .position(|manifest| &manifest.id == id)
                .map(|index| index + 2)
                .unwrap_or_else(|| {
                    node_view_tabs.push(NodeViewTab {
                        label: format!("Unavailable ({id})"),
                        count: 0,
                    });
                    node_view_tabs.len() - 1
                }),
        };

        let candidate_selected = if self.node_view_panel != NodeViewPanel::CurrentSelector {
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
            active_node_view_tab,
            candidate_rows,
            candidate_selected,
            intranet_detail,
            status,
            flash,
            node_quality_detail: self.node_quality_detail.as_ref(),
            connections,
            help_index: self.show_help.then_some(self.help_index),
            usability_probe_diagnostics: &self.usability_probe_diagnostics,
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

fn active_quick_overlay(
    workflow: &BenchmarkWorkflow,
    group: &str,
    node: &str,
) -> Option<(String, Option<String>)> {
    workflow
        .active_quick_probe(group, node)
        .map(|progress| match progress {
            ActiveQuickProbe::Pending => ("-/3".to_string(), None),
            ActiveQuickProbe::Assessment(assessment) => {
                let (reachability, marker) =
                    split_reachability_evidence(&assessment.compact_evidence());
                (reachability, Some(marker))
            }
        })
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

    #[test]
    fn active_reachability_progress_covers_stored_evidence() {
        let mut app = test_app();
        app.benchmark_workflow.set_reachability_assessment(
            "select",
            NodeReachabilityAssessment {
                name: "node-a".into(),
                attempts: vec![
                    ProbeOutcome::Reachable { delay_ms: 20 },
                    ProbeOutcome::Reachable { delay_ms: 21 },
                    ProbeOutcome::Reachable { delay_ms: 22 },
                ],
                assessment: Some(ReachabilityAssessment::StableReachable),
            },
        );
        app.benchmark_workflow
            .add_pending_job_for_test("select", "node-a");
        app.benchmark_workflow
            .set_active_reachability_assessment_for_test(
                "select",
                NodeReachabilityAssessment {
                    name: "node-a".into(),
                    attempts: vec![ProbeOutcome::Timeout],
                    assessment: Some(ReachabilityAssessment::Degraded),
                },
            );

        let snapshot = app.view_snapshot();
        let row = snapshot
            .candidate_rows
            .iter()
            .find(|row| row.name == "node-a")
            .expect("active node row");

        assert_eq!(row.reachability, "0/3");
        assert_eq!(row.marker, "degraded");
        assert_eq!(row.tone, CandidateTone::Pending);
    }

    #[test]
    fn active_probe_without_progress_hides_stored_evidence() {
        let mut app = test_app();
        app.benchmark_workflow.set_reachability_assessment(
            "select",
            NodeReachabilityAssessment {
                name: "node-a".into(),
                attempts: vec![
                    ProbeOutcome::Reachable { delay_ms: 20 },
                    ProbeOutcome::Reachable { delay_ms: 21 },
                    ProbeOutcome::Reachable { delay_ms: 22 },
                ],
                assessment: Some(ReachabilityAssessment::StableReachable),
            },
        );
        app.benchmark_workflow
            .add_pending_job_for_test("select", "node-a");

        let snapshot = app.view_snapshot();
        let row = snapshot
            .candidate_rows
            .iter()
            .find(|row| row.name == "node-a")
            .expect("pending node row");

        assert_eq!(row.reachability, "-/3");
        assert_eq!(row.marker, "...");
        assert_eq!(row.tone, CandidateTone::Pending);
    }
}

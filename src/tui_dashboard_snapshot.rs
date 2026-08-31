const BRAILLE_SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

use super::App;
use super::settings::{settings_field_display_value, visible_settings_fields};
use super::view::{
    CandidateNotice, CandidateRow, CandidateTone, ConnectionsPanelSnapshot, DashboardSnapshot,
    Focus, InternetRow, IntranetDetailSnapshot, IntranetRow, NodeViewPanel, NodeViewTab,
    SettingRow, SettingsPanelSnapshot, StatusFooter, StatusSnapshot, pick_mode_badge,
    settings_field_label,
};
use crate::automatic_selection::NodeViewId;
use crate::benchmark_workflow::{ActiveQuickProbe, BenchmarkWorkflow};
use crate::sustained_quality::{NodeSustainedQuality, SustainedProbeOutcome};

fn extract_candidate_brief_marker(
    manifest_id: Option<&NodeViewId>,
    sustained: Option<&NodeSustainedQuality>,
    detail: Option<&str>,
) -> (String, String) {
    if let Some(sustained) = sustained {
        if let SustainedProbeOutcome::Completed(completion) = &sustained.outcome {
            let speed = completion.throughput_bytes_per_second as f64 / (1024.0 * 1024.0);
            let marker = format!("{:.1} MiB/s", speed);
            let compact = format!("{:.1}M/s", speed);
            return (marker, compact);
        }
    }
    if manifest_id == Some(&NodeViewId::streaming()) {
        if let Some(detail_str) = detail {
            if let Some(idx) = detail_str.find("MiB/s") {
                let prefix = &detail_str[..idx + 5];
                if let Some(start) = prefix.rfind(' ') {
                    let speed = &prefix[start + 1..];
                    return (speed.to_string(), speed.to_string());
                }
            }
        }
    }
    if let Some(detail_str) = detail {
        if let Some(idx) = detail_str.find("succeeded in ") {
            let rest = &detail_str[idx + "succeeded in ".len()..];
            return (rest.to_string(), rest.to_string());
        }
        if let Some(idx) = detail_str.find("response in ") {
            let rest = &detail_str[idx + "response in ".len()..];
            return (rest.to_string(), rest.to_string());
        }
        if let Some(idx) = detail_str.find("banner in ") {
            let rest = &detail_str[idx + "banner in ".len()..];
            return (rest.to_string(), rest.to_string());
        }
        if unicode_width::UnicodeWidthStr::width(detail_str) <= 24 {
            return (detail_str.to_string(), detail_str.to_string());
        }
    }
    ("usable".to_string(), "usable".to_string())
}

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
        let usability_manifest_id = self.active_usability_manifest_id();
        let custom_run = usability_manifest_id.as_ref().and_then(|id| {
            selected_group
                .and_then(|group| self.custom_usability_run(id, &group.name, &group.members))
        });
        let custom_live = usability_manifest_id.as_ref().and_then(|id| {
            selected_group.and_then(|group| self.custom_usability_live_projection(id, &group.name))
        });
        let displayed_members = self.displayed_members();
        let candidate_rows = selected_group
            .map(|group| {
                if usability_manifest_id.is_some() {
                    if let Some((pending, live_results)) = custom_live.as_ref() {
                        return displayed_members
                            .iter()
                            .filter_map(|member| {
                                let is_pending =
                                    pending.as_ref().is_some_and(|(node, _)| node == member);
                                let result = live_results.get(member);
                                if !is_pending && !result.is_some_and(|result| result.usable) {
                                    return None;
                                }
                                let (marker, compact_marker) = if is_pending {
                                    let elapsed = pending.as_ref().map(|(_, elapsed)| *elapsed).unwrap_or(0);
                                    let m = pending_candidate_marker(elapsed);
                                    let c = format!("Working ({elapsed}s)");
                                    (m, c)
                                } else {
                                    let sustained = result
                                        .and_then(|r| r.sustained_quality.as_ref())
                                        .or_else(|| {
                                            self.benchmark_workflow
                                                .sustained_quality(&group.name, member)
                                        });
                                    extract_candidate_brief_marker(
                                        usability_manifest_id.as_ref(),
                                        sustained,
                                        result.and_then(|r| r.detail.as_deref()),
                                    )
                                };
                                Some(CandidateRow {
                                    name: member.clone(),
                                    is_current: group.current.as_deref() == Some(member.as_str()),
                                    reachability: String::new(),
                                    compact_marker,
                                    marker,
                                    tone: if is_pending {
                                        CandidateTone::Pending
                                    } else {
                                        CandidateTone::Success
                                    },
                                })
                            })
                            .collect();
                    }
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
                            let active =
                                active_quick_overlay(&self.benchmark_workflow, &group.name, member);
                            let quick_probe_pending = active
                                .as_ref()
                                .is_some_and(|(_, _, assessment)| assessment.is_none());
                            let sustained = self
                                .benchmark_workflow
                                .sustained_quality(&group.name, member);
                            let (marker, compact_marker) = extract_candidate_brief_marker(
                                usability_manifest_id.as_ref(),
                                sustained,
                                result.detail.as_deref(),
                            );
                            Some(CandidateRow {
                                name: member.clone(),
                                is_current: group.current.as_deref() == Some(member.as_str()),
                                reachability: String::new(),
                                compact_marker,
                                marker,
                                tone: if quick_probe_pending {
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
                        let (reachability, marker, tone) = if let Some((
                            reachability,
                            marker,
                            assessment,
                        )) = active
                        {
                            let tone = match assessment {
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
                                None => CandidateTone::Pending,
                            };
                            (
                                reachability,
                                marker.unwrap_or_else(|| "...".to_string()),
                                tone,
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
                    NodeViewPanel::Streaming | NodeViewPanel::Custom(_) => usability_manifest_id
                        .as_ref()
                        .and_then(|id| {
                            self.usability_probe_manifests
                                .iter()
                                .find(|manifest| &manifest.id == id)
                                .map(|manifest| manifest.ranking_policy.badge())
                        })
                        .unwrap_or("CUSTOM"),
                };
                let progress = match usability_manifest_id.as_ref() {
                    Some(id) => {
                        if self
                            .custom_usability_probe_progress(id, &group.name)
                            .is_some()
                        {
                            String::new()
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
                                let _ = failure;
                                state.push("FAILED".to_string());
                            }
                            if state.is_empty() {
                                String::new()
                            } else {
                                format!(" · {}", state.join(" · "))
                            }
                        }
                    }
                    None => String::new(),
                };
                format!("Candidates for {} [{order}]{progress}", group.name)
            })
            .unwrap_or_else(|| "Candidates [SELECTOR ORDER]".to_string());
        let candidate_notice = match (usability_manifest_id.as_ref(), selected_group) {
            (Some(id), Some(group)) => {
                if let Some(message) = self.custom_usability_probe_stage(id, &group.name) {
                    let progress = self
                        .custom_usability_probe_progress(id, &group.name)
                        .map(|(received, _total, metrics)| {
                            format_custom_probe_progress(metrics.as_ref(), received)
                        })
                        .unwrap_or_else(|| "Probe running".to_string());
                    Some(CandidateNotice {
                        title: "Probe progress".to_string(),
                        message: format!("{progress}\n{message}"),
                        error: false,
                    })
                } else {
                    custom_run.as_ref().and_then(|run| {
                        self.custom_usability_latest_failure(run)
                            .map(|message| CandidateNotice {
                                title: "Probe error".to_string(),
                                message,
                                error: true,
                            })
                    })
                }
            }
            _ => None,
        };
        let spinner_frame = BRAILLE_SPINNER_FRAMES[
            (self.animation_started.elapsed().as_millis() / 80) as usize % BRAILLE_SPINNER_FRAMES.len()
        ];
        let all_count = selected_group.map_or(0, |group| group.members.len());
        let mut node_view_tabs = vec![NodeViewTab {
            label: "Current selector".to_string(),
            count: all_count,
            spinner: None,
        }];
        if let Some(group) = selected_group {
            node_view_tabs.extend(self.visible_usability_manifests().map(|manifest| {
                let is_probing = self.is_usability_probe_active_for(&manifest.id, Some(&group.name));
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
                    spinner: is_probing.then(|| spinner_frame.to_string()),
                }
            }));
        } else {
            node_view_tabs.extend(
                self.visible_usability_manifests()
                    .map(|manifest| {
                        let is_probing = self.is_usability_probe_active_for(&manifest.id, None);
                        NodeViewTab {
                            label: manifest.label.clone(),
                            count: 0,
                            spinner: is_probing.then(|| spinner_frame.to_string()),
                        }
                    }),
            );
        }
        let active_node_view_tab = match &self.node_view_panel {
            NodeViewPanel::CurrentSelector => 0,
            NodeViewPanel::Streaming | NodeViewPanel::Custom(_) => {
                let id = self.node_view_panel.id();
                self.visible_usability_manifests()
                    .position(|manifest| manifest.id == id)
                    .map(|index| index + 1)
                    .unwrap_or_else(|| {
                        node_view_tabs.push(NodeViewTab {
                            label: format!("Unavailable ({id})"),
                            count: 0,
                            spinner: None,
                        });
                        node_view_tabs.len() - 1
                    })
            }
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
            candidate_notice,
            node_view_tabs,
            active_node_view_tab,
            candidate_rows,
            candidate_selected,
            pending_animation_tick: (self.animation_started.elapsed().as_millis() / 80) as usize,
            pending_animation_bright: (self.animation_started.elapsed().as_millis() / 500) % 2 == 0,
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
) -> Option<(
    String,
    Option<String>,
    Option<crate::controller::ReachabilityAssessment>,
)> {
    workflow
        .active_quick_probe(group, node)
        .map(|progress| match progress {
            ActiveQuickProbe::Pending => ("-/3".to_string(), None, None),
            ActiveQuickProbe::IncompleteAssessment(assessment) => {
                let (reachability, marker) =
                    split_reachability_evidence(&assessment.compact_evidence());
                (reachability, Some(marker), None)
            }
            ActiveQuickProbe::CompleteAssessment(assessment) => {
                let (reachability, marker) =
                    split_reachability_evidence(&assessment.compact_evidence());
                (reachability, Some(marker), assessment.assessment)
            }
        })
}

fn format_custom_probe_progress(
    metrics: Option<&crate::usability_probe::UsabilityProbeProgress>,
    received: usize,
) -> String {
    metrics.map_or_else(
        || format!("Results received {received}"),
        |metrics| {
            format!(
                "{} {}/{} · {} {}/{} · accepted {}",
                metrics.stage_one_label,
                metrics.stage_one_completed,
                metrics.stage_one_total,
                metrics.stage_two_label,
                metrics.stage_two_completed,
                metrics.stage_two_total,
                metrics.accepted
            )
        },
    )
}

fn pending_candidate_marker(elapsed_seconds: u64) -> String {
    format!("• Working ({elapsed_seconds}s)")
}

#[cfg(test)]
mod tests {
    use super::super::test_support::test_app;
    use super::super::view::CandidateTone;
    use super::{format_custom_probe_progress, pending_candidate_marker};
    use crate::controller::{NodeReachabilityAssessment, ProbeOutcome, ReachabilityAssessment};

    #[test]
    fn custom_probe_title_uses_explicit_stage_metrics() {
        let metrics = crate::usability_probe::UsabilityProbeProgress {
            stage_one_completed: 44,
            stage_one_total: 108,
            stage_two_completed: 3,
            stage_two_total: 17,
            stage_one_label: "HTTPS".to_string(),
            stage_two_label: "TCP 22".to_string(),
            accepted: 2,
        };

        assert_eq!(
            format_custom_probe_progress(Some(&metrics), 0),
            "HTTPS 44/108 · TCP 22 3/17 · accepted 2"
        );
    }

    #[test]
    fn pending_tcp_candidate_marker_matches_working_animation_copy() {
        assert_eq!(pending_candidate_marker(3), "• Working (3s)");
    }

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
    fn completed_node_stops_pending_animation_while_quick_batch_remains_active() {
        let mut app = test_app();
        app.benchmark_workflow
            .add_pending_job_for_test("select", "node-a");
        app.benchmark_workflow
            .set_active_reachability_assessment_for_test(
                "select",
                NodeReachabilityAssessment::from_attempts(
                    "node-a".into(),
                    vec![
                        ProbeOutcome::Reachable { delay_ms: 20 },
                        ProbeOutcome::Reachable { delay_ms: 21 },
                        ProbeOutcome::Reachable { delay_ms: 22 },
                    ],
                ),
            );

        let snapshot = app.view_snapshot();
        let row = snapshot
            .candidate_rows
            .iter()
            .find(|row| row.name == "node-a")
            .expect("completed node row");

        assert_eq!(row.reachability, "3/3");
        assert_eq!(row.marker, "stable reachable");
        assert_eq!(row.tone, CandidateTone::Success);
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

use std::time::Instant;

use anyhow::Result;

use super::view::{
    LatencyChartState, UsabilityCriterionDetail, latency_chart_evidence_line_count,
    latency_chart_window_label, latency_chart_zoom_in, latency_chart_zoom_out,
};
use super::{App, LATENCY_CHART_DEFAULT_WINDOW, LATENCY_CHART_REFRESH_INTERVAL};

impl App {
    pub(super) fn open_latency_chart(&mut self) -> Result<()> {
        if self.showing_intranet_details() {
            self.set_status_only("Latency history is available for Internet Proxy nodes only");
            return Ok(());
        }
        let Some(group_name) = self
            .selected_member_panel_group()
            .map(|group| group.name.clone())
        else {
            self.set_status_only("No selector group available for latency history");
            return Ok(());
        };
        let Some(node) = self.selected_member_name() else {
            self.set_status_only("No node selected for latency history");
            return Ok(());
        };
        let Some(samples) =
            self.benchmark_workflow
                .node_latency_history(&group_name, &node, 200)?
        else {
            self.set_status_only("SQLite latency history is unavailable");
            return Ok(());
        };
        let reachability_assessment = self
            .benchmark_workflow
            .reachability_assessment(&group_name, &node)
            .cloned();
        let sustained_quality = self
            .benchmark_workflow
            .sustained_quality(&group_name, &node)
            .cloned();
        let auto_selection_detail = self
            .last_auto_selection_explanation
            .as_ref()
            .filter(|explanation| explanation.matches(&group_name, &self.node_view_panel.id()))
            .map(|explanation| explanation.detail.clone());
        let selector_members = self
            .selected_member_panel_group()
            .map(|group| group.members.clone())
            .unwrap_or_default();
        let usability_details = self
            .usability_probe_manifests
            .iter()
            .filter_map(|manifest| {
                self.custom_usability_run(&manifest.id, &group_name, &selector_members)
                    .and_then(|run| {
                        run.results
                            .into_iter()
                            .find(|result| result.node == node)
                            .map(|result| UsabilityCriterionDetail {
                                label: manifest.label.clone(),
                                usable: result.usable,
                                detail: result.detail,
                            })
                    })
            })
            .collect::<Vec<_>>();
        if samples.iter().all(|sample| sample.delay_ms.is_none())
            && reachability_assessment.is_none()
            && sustained_quality.is_none()
            && auto_selection_detail.is_none()
            && usability_details.is_empty()
        {
            self.set_status_only(format!("No latency history for {}", node));
            return Ok(());
        }
        let count = samples.len();
        self.latency_chart = Some(LatencyChartState {
            selector: group_name,
            node: node.clone(),
            samples,
            window: LATENCY_CHART_DEFAULT_WINDOW,
            threshold_ms: self.auto_select_threshold_ms,
            last_refresh: Instant::now(),
            reachability_assessment,
            sustained_quality,
            auto_selection_detail,
            usability_details,
            evidence_scroll: 0,
        });
        self.set_status_only(format!("Showing {} latency samples for {}", count, node));
        Ok(())
    }

    pub(super) fn zoom_latency_chart_in(&mut self) {
        let Some(chart) = self.latency_chart.as_mut() else {
            return;
        };
        chart.window = latency_chart_zoom_in(chart.window);
        let label = latency_chart_window_label(chart.window);
        self.set_status_only(format!("Latency chart window: {label}"));
    }

    pub(super) fn zoom_latency_chart_out(&mut self) {
        let Some(chart) = self.latency_chart.as_mut() else {
            return;
        };
        chart.window = latency_chart_zoom_out(chart.window);
        let label = latency_chart_window_label(chart.window);
        self.set_status_only(format!("Latency chart window: {label}"));
    }

    pub(super) fn scroll_latency_chart_evidence_down(&mut self) {
        let Some(chart) = self.latency_chart.as_mut() else {
            return;
        };
        // The evidence pane is ten rows including borders, leaving eight visible evidence lines.
        // Keeping every criterion in the model and scrolling the viewport preserves rejected
        // facts without letting dozens of manifests crowd the latency graph out of the modal.
        let max_scroll = latency_chart_evidence_line_count(chart).saturating_sub(8) as u16;
        chart.evidence_scroll = chart.evidence_scroll.saturating_add(1).min(max_scroll);
    }

    pub(super) fn scroll_latency_chart_evidence_up(&mut self) {
        if let Some(chart) = self.latency_chart.as_mut() {
            chart.evidence_scroll = chart.evidence_scroll.saturating_sub(1);
        }
    }

    pub(super) fn maybe_refresh_latency_chart(&mut self) -> Result<()> {
        let active_panel = self.node_view_panel.id();
        let explanation = self.last_auto_selection_explanation.clone();
        let Some(chart) = self.latency_chart.as_ref() else {
            return Ok(());
        };
        if chart.last_refresh.elapsed() < LATENCY_CHART_REFRESH_INTERVAL {
            return Ok(());
        }
        let selector = chart.selector.clone();
        let node = chart.node.clone();
        let selector_members = self
            .groups
            .iter()
            .find(|group| group.name == selector)
            .map(|group| group.members.clone())
            .unwrap_or_default();
        let usability_details = self
            .usability_probe_manifests
            .iter()
            .filter_map(|manifest| {
                self.custom_usability_run(&manifest.id, &selector, &selector_members)
                    .and_then(|run| {
                        run.results
                            .into_iter()
                            .find(|result| result.node == node)
                            .map(|result| UsabilityCriterionDetail {
                                label: manifest.label.clone(),
                                usable: result.usable,
                                detail: result.detail,
                            })
                    })
            })
            .collect::<Vec<_>>();
        let Some(samples) = self
            .benchmark_workflow
            .node_latency_history(&selector, &node, 200)?
        else {
            return Ok(());
        };
        let reachability_assessment = self
            .benchmark_workflow
            .reachability_assessment(&selector, &node)
            .cloned();
        let sustained_quality = self
            .benchmark_workflow
            .sustained_quality(&selector, &node)
            .cloned();
        let auto_selection_detail = explanation
            .as_ref()
            .filter(|explanation| explanation.matches(&selector, &active_panel))
            .map(|explanation| explanation.detail.clone());
        let chart = self
            .latency_chart
            .as_mut()
            .expect("latency chart remained open during refresh");
        chart.samples = samples;
        chart.reachability_assessment = reachability_assessment;
        chart.sustained_quality = sustained_quality;
        chart.auto_selection_detail = auto_selection_detail;
        chart.usability_details = usability_details;
        chart.last_refresh = Instant::now();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use crossterm::event::KeyCode;

    use super::super::AUTO_SELECT_THRESHOLD_MS;
    use super::super::test_support::{test_app, test_db_path};
    use super::{LATENCY_CHART_DEFAULT_WINDOW, LATENCY_CHART_REFRESH_INTERVAL, LatencyChartState};
    use crate::automatic_selection::{AutoSelectionExplanation, NodeViewId, RankingPolicy};
    use crate::controller::{NodeReachabilityAssessment, ProbeOutcome, ReachabilityAssessment};
    use crate::storage::{
        BenchmarkRecord, BenchmarkStore, NodeLatencySample, StoredUsabilityProbeRun,
        UsabilityProbeFactRecord,
    };
    use crate::usability_probe::{UsabilityProbeManifest, UsabilityProbeSource};

    fn bind_test_nodes(store: &BenchmarkStore, nodes: &[&str]) {
        let mut outbounds = vec![serde_json::json!({
            "type":"selector", "tag":"select", "outbounds":nodes
        })];
        outbounds.extend(
            nodes
                .iter()
                .map(|node| serde_json::json!({"type":"direct", "tag":node})),
        );
        store
            .reconcile_node_history(&serde_json::json!({"outbounds":outbounds}))
            .expect("bind latency-chart test identities");
    }

    #[test]
    fn pressing_i_opens_latency_chart_for_selected_node() {
        let path = test_db_path();
        let mut app = test_app();
        app.auto_select_enabled = true;
        app.auto_select_selector = Some("select".to_string());
        app.groups[0].members = vec!["node-a".to_string(), "node-b".to_string()];
        app.member_index = 1;
        let store = BenchmarkStore::open(&path).expect("open benchmark store");
        bind_test_nodes(&store, &["node-a", "node-b"]);
        store
            .record_benchmark(&BenchmarkRecord {
                selector: "select",
                node: "node-b",
                filter: "美国",
                delay_ms: Some(93),
                completed: true,
                job_kind: "single",
            })
            .expect("record benchmark");
        app.benchmark_workflow.replace_store(Some(store));
        app.apply_background_auto_selection_explanation(Some(AutoSelectionExplanation {
            selector: "select".to_string(),
            panel: NodeViewId::current_selector(),
            detail: "node-b leads; awaiting confirmation 1/2".to_string(),
        }));

        app.handle_key(KeyCode::Char('i')).expect("open chart");

        let chart = app.latency_chart.as_ref().expect("latency chart");
        assert_eq!(chart.selector, "select");
        assert_eq!(chart.node, "node-b");
        assert_eq!(chart.samples.len(), 1);
        assert_eq!(chart.samples[0].delay_ms, Some(93));
        assert_eq!(
            chart.auto_selection_detail.as_deref(),
            Some("node-b leads; awaiting confirmation 1/2")
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn custom_panel_detail_exposes_the_criterion_result() {
        let path = test_db_path();
        let mut app = test_app();
        app.benchmark_filter.clear();
        app.usability_probe_manifests.push(UsabilityProbeManifest {
            id: NodeViewId::new("github-web").unwrap(),
            label: "GitHub Web".to_string(),
            ranking_policy: RankingPolicy::LowLatency,
            source: UsabilityProbeSource::Url("https://github.com/".to_string()),
            source_path: std::path::PathBuf::from("github-web.json"),
        });
        app.usability_probe_projection_cache.insert(
            (NodeViewId::new("github-web").unwrap(), "select".to_string()),
            StoredUsabilityProbeRun {
                run_id: 9,
                completed_at_ms: 100,
                summary: Some("complete".to_string()),
                results: vec![UsabilityProbeFactRecord {
                    node: "node-a".to_string(),
                    usable: true,
                    detail: Some("HTTP 404 is still a valid response".to_string()),
                }],
            },
        );
        app.node_view_panel =
            super::super::NodeViewPanel::Custom(NodeViewId::new("github-web").unwrap());
        let store = BenchmarkStore::open(&path).expect("open benchmark store");
        bind_test_nodes(&store, &["node-a"]);
        store
            .record_benchmark(&BenchmarkRecord {
                selector: "select",
                node: "node-a",
                filter: "",
                delay_ms: Some(55),
                completed: true,
                job_kind: "single",
            })
            .expect("record benchmark");
        app.benchmark_workflow.replace_store(Some(store));

        app.handle_key(KeyCode::Char('i'))
            .expect("open custom criterion detail");

        let detail = app
            .latency_chart
            .as_ref()
            .and_then(|chart| chart.usability_details.first())
            .expect("criterion detail is visible");
        assert_eq!(detail.label, "GitHub Web");
        assert!(detail.usable);
        assert_eq!(
            detail.detail.as_deref(),
            Some("HTTP 404 is still a valid response")
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn current_selector_detail_keeps_rejected_custom_facts_reachable() {
        let path = test_db_path();
        let mut app = test_app();
        app.benchmark_filter.clear();
        app.groups[0].members = vec!["node-a".to_string(), "node-b".to_string()];
        app.usability_probe_manifests.push(UsabilityProbeManifest {
            id: NodeViewId::new("github-web").unwrap(),
            label: "GitHub Web".to_string(),
            ranking_policy: RankingPolicy::LowLatency,
            source: UsabilityProbeSource::Url("https://github.com/".to_string()),
            source_path: std::path::PathBuf::from("github-web.json"),
        });
        app.usability_probe_projection_cache.insert(
            (NodeViewId::new("github-web").unwrap(), "select".to_string()),
            StoredUsabilityProbeRun {
                run_id: 10,
                completed_at_ms: 101,
                summary: Some("complete".to_string()),
                results: vec![
                    UsabilityProbeFactRecord {
                        node: "node-a".to_string(),
                        usable: false,
                        detail: Some("application login failed".to_string()),
                    },
                    UsabilityProbeFactRecord {
                        node: "node-b".to_string(),
                        usable: true,
                        detail: Some("application accepted".to_string()),
                    },
                ],
            },
        );
        app.node_view_panel =
            super::super::NodeViewPanel::Custom(NodeViewId::new("github-web").unwrap());
        assert_eq!(app.displayed_members(), ["node-b"]);

        app.node_view_panel = super::super::NodeViewPanel::CurrentSelector;
        app.member_index = 0;
        let store = BenchmarkStore::open(&path).expect("open benchmark store");
        bind_test_nodes(&store, &["node-a", "node-b"]);
        app.benchmark_workflow.replace_store(Some(store));
        app.handle_key(KeyCode::Char('i'))
            .expect("open rejected criterion detail from current selector");

        let detail = app
            .latency_chart
            .as_ref()
            .and_then(|chart| chart.usability_details.first())
            .expect("rejected fact remains visible");
        assert_eq!(detail.label, "GitHub Web");
        assert!(!detail.usable);
        assert_eq!(detail.detail.as_deref(), Some("application login failed"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn z_and_shift_z_zoom_latency_chart() {
        let mut app = test_app();
        app.latency_chart = Some(LatencyChartState {
            selector: "select".to_string(),
            node: "node-a".to_string(),
            samples: vec![NodeLatencySample {
                recorded_at_ms: 1_000,
                delay_ms: Some(90),
            }],
            window: LATENCY_CHART_DEFAULT_WINDOW,
            threshold_ms: AUTO_SELECT_THRESHOLD_MS,
            last_refresh: Instant::now(),
            reachability_assessment: None,
            sustained_quality: None,
            auto_selection_detail: None,
            usability_details: Vec::new(),
            evidence_scroll: 0,
        });
        app.handle_key(KeyCode::Char('z')).expect("zoom in");
        assert_eq!(
            app.latency_chart.as_ref().unwrap().window,
            Duration::from_secs(30 * 60)
        );
        app.handle_key(KeyCode::Char('Z')).expect("zoom out");
        assert_eq!(
            app.latency_chart.as_ref().unwrap().window,
            LATENCY_CHART_DEFAULT_WINDOW
        );
    }

    #[test]
    fn latency_chart_refreshes_from_sqlite() {
        let path = test_db_path();
        let mut app = test_app();
        let store = BenchmarkStore::open(&path).expect("open benchmark store");
        bind_test_nodes(&store, &["node-a"]);
        store
            .record_benchmark(&BenchmarkRecord {
                selector: "select",
                node: "node-a",
                filter: "美国",
                delay_ms: Some(77),
                completed: true,
                job_kind: "auto",
            })
            .unwrap();
        app.benchmark_workflow.replace_store(Some(store));
        app.latency_chart = Some(LatencyChartState {
            selector: "select".to_string(),
            node: "node-a".to_string(),
            samples: Vec::new(),
            window: LATENCY_CHART_DEFAULT_WINDOW,
            threshold_ms: AUTO_SELECT_THRESHOLD_MS,
            last_refresh: Instant::now() - LATENCY_CHART_REFRESH_INTERVAL,
            reachability_assessment: None,
            sustained_quality: None,
            auto_selection_detail: None,
            usability_details: Vec::new(),
            evidence_scroll: 0,
        });
        app.maybe_refresh_latency_chart().expect("refresh chart");
        assert_eq!(
            app.latency_chart.as_ref().unwrap().samples[0].delay_ms,
            Some(77)
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn pressing_i_without_history_updates_status() {
        let path = test_db_path();
        let mut app = test_app();
        app.benchmark_workflow
            .replace_store(Some(BenchmarkStore::open(&path).unwrap()));
        app.handle_key(KeyCode::Char('i')).expect("open chart");
        assert!(app.latency_chart.is_none());
        assert_eq!(app.status, "No latency history for node-a");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn pressing_i_exposes_persisted_probe_attempts_without_latency_history() {
        let path = test_db_path();
        let mut app = test_app();
        app.benchmark_filter.clear();
        let store = BenchmarkStore::open(&path).unwrap();
        bind_test_nodes(&store, &["node-a"]);
        store
            .record_reachability_assessment(
                "select",
                &NodeReachabilityAssessment {
                    name: "node-a".into(),
                    attempts: vec![
                        ProbeOutcome::Reachable { delay_ms: 42 },
                        ProbeOutcome::Timeout,
                        ProbeOutcome::Reachable { delay_ms: 51 },
                    ],
                    assessment: Some(ReachabilityAssessment::Reachable),
                },
            )
            .unwrap();
        app.benchmark_workflow.replace_store(Some(store));

        app.handle_key(KeyCode::Char('i'))
            .expect("open quality detail");

        let detail = app.latency_chart.as_ref().expect("node-quality detail");
        assert_eq!(
            detail
                .reachability_assessment
                .as_ref()
                .unwrap()
                .attempts
                .len(),
            3
        );
        let _ = std::fs::remove_file(path);
    }
}

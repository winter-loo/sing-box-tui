use std::time::Instant;

use anyhow::Result;

use super::view::{
    NodeQualityDetailState, UsabilityCriterionDetail, node_quality_detail_max_scroll,
};
use super::{ActiveView, App, NODE_QUALITY_DETAIL_REFRESH_INTERVAL};

impl App {
    pub(super) fn open_node_quality_detail(&mut self) -> Result<()> {
        if self.showing_intranet_details() {
            self.set_status_only("Node quality is available for Internet Proxy nodes only");
            return Ok(());
        }
        let (group_name, node, selector_members) = if self.active_view == ActiveView::NodeDashboard
        {
            let Some((_, route_group, route_node)) = self.current_route_target() else {
                self.set_status_only("No current route available for node quality");
                return Ok(());
            };
            (
                route_group.name.clone(),
                route_node.to_string(),
                route_group.members.clone(),
            )
        } else {
            let Some(group) = self.selected_member_panel_group() else {
                self.set_status_only("No selector group available for node quality");
                return Ok(());
            };
            let group_name = group.name.clone();
            let selector_members = group.members.clone();
            let Some(node) = self.selected_member_name() else {
                self.set_status_only("No node selected for node quality");
                return Ok(());
            };
            (group_name, node, selector_members)
        };
        let (latency_history, throughput_history) =
            self.node_quality_history(&group_name, &node)?;
        self.node_quality_detail = Some(NodeQualityDetailState {
            selector: group_name.clone(),
            node: node.clone(),
            last_refresh: Instant::now(),
            reachability_assessment: self
                .benchmark_workflow
                .reachability_assessment(&group_name, &node)
                .cloned(),
            quick_history: self.benchmark_workflow.quick_history(&group_name, &node),
            sustained_quality: self
                .benchmark_workflow
                .sustained_quality(&group_name, &node)
                .cloned(),
            latency_history,
            throughput_history,
            auto_selection_detail: self
                .last_auto_selection_explanation
                .as_ref()
                .filter(|explanation| explanation.matches(&group_name, &self.node_view_panel.id()))
                .map(|explanation| explanation.detail.clone()),
            usability_details: self.node_usability_details(&group_name, &node, &selector_members),
            evidence_scroll: 0,
        });
        self.set_status_only(format!("Showing node quality for {node}"));
        Ok(())
    }

    pub(super) fn scroll_node_quality_detail_down(&mut self) {
        let Some(detail) = self.node_quality_detail.as_mut() else {
            return;
        };
        let max_scroll = node_quality_detail_max_scroll(detail, self.last_frame_area);
        detail.evidence_scroll = detail.evidence_scroll.saturating_add(1).min(max_scroll);
    }

    pub(super) fn scroll_node_quality_detail_up(&mut self) {
        if let Some(detail) = self.node_quality_detail.as_mut() {
            detail.evidence_scroll = detail.evidence_scroll.saturating_sub(1);
        }
    }

    pub(super) fn maybe_refresh_node_quality_detail(&mut self) -> Result<()> {
        let Some(detail) = self.node_quality_detail.as_ref() else {
            return Ok(());
        };
        if detail.last_refresh.elapsed() < NODE_QUALITY_DETAIL_REFRESH_INTERVAL {
            return Ok(());
        }
        let selector = detail.selector.clone();
        let node = detail.node.clone();
        let selector_members = self
            .groups
            .iter()
            .find(|group| group.name == selector)
            .map(|group| group.members.clone())
            .unwrap_or_default();
        let reachability_assessment = self
            .benchmark_workflow
            .reachability_assessment(&selector, &node)
            .cloned();
        let quick_history = self.benchmark_workflow.quick_history(&selector, &node);
        let sustained_quality = self
            .benchmark_workflow
            .sustained_quality(&selector, &node)
            .cloned();
        let (latency_history, throughput_history) = self.node_quality_history(&selector, &node)?;
        let auto_selection_detail = self
            .last_auto_selection_explanation
            .as_ref()
            .filter(|explanation| explanation.matches(&selector, &self.node_view_panel.id()))
            .map(|explanation| explanation.detail.clone());
        let usability_details = self.node_usability_details(&selector, &node, &selector_members);
        let detail = self
            .node_quality_detail
            .as_mut()
            .expect("node-quality detail remained open during refresh");
        detail.reachability_assessment = reachability_assessment;
        detail.quick_history = quick_history;
        detail.sustained_quality = sustained_quality;
        detail.latency_history = latency_history;
        detail.throughput_history = throughput_history;
        detail.auto_selection_detail = auto_selection_detail;
        detail.usability_details = usability_details;
        detail.last_refresh = Instant::now();
        Ok(())
    }

    fn node_quality_history(
        &self,
        selector: &str,
        node: &str,
    ) -> Result<(Vec<crate::tui::metrics::LatencySample>, Vec<(i64, u64)>)> {
        let latency = self.metric_store.as_ref().map_or_else(Vec::new, |store| {
            store
                .latency_samples()
                .iter()
                .filter(|sample| sample.selector == selector && sample.node_name == node)
                .cloned()
                .collect()
        });
        let cutoff_ms = crate::tui::metrics::now_unix_ms()
            .saturating_sub(crate::tui::metrics::METRIC_RETENTION_WINDOW_MS);
        let throughput = self
            .benchmark_workflow
            .sustained_throughput_history(selector, node, cutoff_ms)?;
        Ok((latency, throughput))
    }

    fn node_usability_details(
        &self,
        selector: &str,
        node: &str,
        selector_members: &[String],
    ) -> Vec<UsabilityCriterionDetail> {
        self.usability_probe_manifests
            .iter()
            .filter_map(|manifest| {
                self.custom_usability_run(&manifest.id, selector, selector_members)
                    .and_then(|run| {
                        let expired = self.custom_usability_run_is_expired(&run);
                        let latest_failure = self.custom_usability_latest_failure(&run);
                        let result = run.results.into_iter().find(|result| result.node == node);
                        // WHY: a failed attempt is criterion-level audit evidence. Preserve it in
                        // node detail even when no complete run has ever published a node result.
                        (result.is_some() || latest_failure.is_some()).then(|| {
                            UsabilityCriterionDetail {
                                label: manifest.label.clone(),
                                usable: result.as_ref().map(|result| result.usable),
                                detail: result.and_then(|result| result.detail),
                                expired,
                                latest_failure,
                            }
                        })
                    })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyCode;

    use super::super::test_support::test_app;
    use crate::automatic_selection::{NodeViewId, RankingPolicy};
    use crate::storage::{StoredUsabilityProbeAttempt, StoredUsabilityProbeRun};
    use crate::usability_probe::{UsabilityProbeManifest, UsabilityProbeSource};

    #[test]
    fn pressing_i_opens_quality_detail_even_when_node_is_untested() {
        let mut app = test_app();
        app.handle_key(KeyCode::Char('i'))
            .expect("open quality detail");
        let detail = app
            .node_quality_detail
            .as_ref()
            .expect("node-quality detail");
        assert_eq!(detail.node, "node-a");
        assert_eq!(app.status, "Showing node quality for node-a");
    }

    #[test]
    fn quality_detail_uses_current_route_when_another_node_is_browsed() {
        let mut app = test_app();
        app.groups[0].members = vec!["node-a".into(), "node-b".into()];
        app.groups[0].current = Some("node-a".into());
        app.member_index = 1;
        assert_eq!(app.selected_member_name().as_deref(), Some("node-b"));
        app.active_view = super::super::ActiveView::NodeDashboard;

        app.open_node_quality_detail().unwrap();

        let detail = app.node_quality_detail.as_ref().unwrap();
        assert_eq!(detail.selector, "select");
        assert_eq!(detail.node, "node-a");
    }

    #[test]
    fn node_list_quality_detail_uses_the_browsed_node() {
        let mut app = test_app();
        app.groups[0].members = vec!["node-a".into(), "node-b".into()];
        app.groups[0].current = Some("node-a".into());
        app.member_index = 1;

        app.open_node_quality_detail().unwrap();

        assert_eq!(app.node_quality_detail.as_ref().unwrap().node, "node-b");
    }

    #[test]
    fn compact_quality_dialog_scrolls_to_last_evidence_row() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        for (width, height) in [(80, 24), (120, 30), (137, 35), (100, 40)] {
            let mut app = test_app();
            app.open_node_quality_detail().unwrap();
            let detail = app.node_quality_detail.as_mut().unwrap();
            detail.usability_details = (0..12)
                .map(|index| super::UsabilityCriterionDetail {
                    label: format!("Criterion {index}"),
                    usable: None,
                    detail: None,
                    expired: false,
                    latest_failure: Some(if index == 11 {
                        "TAIL MARKER".into()
                    } else {
                        format!("failure {index}")
                    }),
                })
                .collect();
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| super::super::draw(frame, &mut app))
                .unwrap();
            for _ in 0..60 {
                app.handle_key(KeyCode::Down).unwrap();
            }
            terminal
                .draw(|frame| super::super::draw(frame, &mut app))
                .unwrap();
            let buffer = terminal.backend().buffer();
            let text = (0..buffer.area.height)
                .map(|y| {
                    (0..buffer.area.width)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                text.contains("TAIL") && text.contains("MARKER"),
                "{width}x{height}\n{text}"
            );
        }
    }

    #[test]
    fn pressing_i_again_closes_quality_detail() {
        let mut app = test_app();
        app.handle_key(KeyCode::Char('i')).unwrap();
        app.handle_key(KeyCode::Char('i')).unwrap();
        assert!(app.node_quality_detail.is_none());
        assert_eq!(app.status, "Node quality detail closed");
    }

    #[test]
    fn failed_first_attempt_is_visible_without_a_node_result() {
        let mut app = test_app();
        app.usability_probe_manifests.push(UsabilityProbeManifest {
            id: NodeViewId::new("agy").unwrap(),
            label: "Agy".to_string(),
            ranking_policy: RankingPolicy::Balanced,
            source: UsabilityProbeSource::Url("https://example.test/".to_string()),
            background: false,
            interval: None,
            result_ttl: None,
            timeout: std::time::Duration::from_secs(60),
            source_path: std::path::PathBuf::from("agy.json"),
            visible: true,
        });
        app.usability_probe_projection_cache.insert(
            (NodeViewId::new("agy").unwrap(), "select".to_string()),
            StoredUsabilityProbeRun {
                run_id: 14,
                completed_at_ms: 200,
                expires_at_ms: None,
                summary: None,
                results: Vec::new(),
                latest_attempt: Some(StoredUsabilityProbeAttempt {
                    run_id: 14,
                    completed_at_ms: 200,
                    complete: false,
                    diagnostic: Some("authentication failed".to_string()),
                }),
            },
        );

        let details = app.node_usability_details(
            "select",
            "node-a",
            &["node-a".to_string(), "node-b".to_string()],
        );

        assert_eq!(details.len(), 1);
        assert_eq!(details[0].usable, None);
        assert_eq!(
            details[0].latest_failure.as_deref(),
            Some("run #14 failed: authentication failed")
        );
    }
}

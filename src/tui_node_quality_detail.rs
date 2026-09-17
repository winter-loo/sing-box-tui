use std::time::Instant;

use anyhow::Result;

use super::view::{
    NodeQualityDetailState, UsabilityCriterionDetail, node_quality_detail_line_count,
};
use super::{App, NODE_QUALITY_DETAIL_REFRESH_INTERVAL};

impl App {
    pub(super) fn open_node_quality_detail(&mut self) -> Result<()> {
        if self.showing_intranet_details() {
            self.set_status_only("Node quality is available for Internet Proxy nodes only");
            return Ok(());
        }
        let Some(group_name) = self
            .selected_member_panel_group()
            .map(|group| group.name.clone())
        else {
            self.set_status_only("No selector group available for node quality");
            return Ok(());
        };
        let Some(node) = self.selected_member_name() else {
            self.set_status_only("No node selected for node quality");
            return Ok(());
        };
        let selector_members = self
            .selected_member_panel_group()
            .map(|group| group.members.clone())
            .unwrap_or_default();
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
        let max_scroll = node_quality_detail_line_count(detail).saturating_sub(8) as u16;
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
        detail.auto_selection_detail = auto_selection_detail;
        detail.usability_details = usability_details;
        detail.last_refresh = Instant::now();
        Ok(())
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

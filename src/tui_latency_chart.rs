use std::time::Instant;

use anyhow::Result;

use super::view::{
    LatencyChartState, latency_chart_window_label, latency_chart_zoom_in, latency_chart_zoom_out,
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
        if samples.iter().all(|sample| sample.delay_ms.is_none())
            && reachability_assessment.is_none()
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

    pub(super) fn maybe_refresh_latency_chart(&mut self) -> Result<()> {
        let Some(chart) = self.latency_chart.as_mut() else {
            return Ok(());
        };
        if chart.last_refresh.elapsed() < LATENCY_CHART_REFRESH_INTERVAL {
            return Ok(());
        }
        let Some(samples) =
            self.benchmark_workflow
                .node_latency_history(&chart.selector, &chart.node, 200)?
        else {
            return Ok(());
        };
        chart.samples = samples;
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
    use crate::controller::{NodeReachabilityAssessment, ProbeOutcome, ReachabilityAssessment};
    use crate::storage::{BenchmarkRecord, BenchmarkStore, NodeLatencySample};

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

        app.handle_key(KeyCode::Char('i')).expect("open chart");

        let chart = app.latency_chart.as_ref().expect("latency chart");
        assert_eq!(chart.selector, "select");
        assert_eq!(chart.node, "node-b");
        assert_eq!(chart.samples.len(), 1);
        assert_eq!(chart.samples[0].delay_ms, Some(93));
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

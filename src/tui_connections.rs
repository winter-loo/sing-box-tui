use std::collections::BTreeMap;
use std::time::Instant;

use super::view::{format_bytes_opt, truncate_for_width};
use super::{App, CONNECTION_REFRESH_INTERVAL};
use crate::controller::ConnectionInfo;

fn connection_is_direct(connection: &ConnectionInfo) -> bool {
    connection
        .chains
        .iter()
        .any(|chain| is_direct_chain_name(chain))
}

fn is_direct_chain_name(value: &str) -> bool {
    value.eq_ignore_ascii_case("direct") || value == "国内直连"
}

fn connection_totals_for_node(
    connections: &crate::controller::ConnectionsSnapshot,
    node_name: &str,
) -> BTreeMap<String, u64> {
    connections
        .connections
        .iter()
        .filter(|connection| connection.chains.iter().any(|chain| chain == node_name))
        .map(|connection| {
            (
                connection.id.clone(),
                connection.download.saturating_add(connection.upload),
            )
        })
        .collect()
}

impl App {
    pub(super) fn connections_summary_line(&self) -> String {
        if let Some(error) = &self.connection_error {
            return format!("connections unavailable: {}", truncate_for_width(error, 80));
        }
        let direct_count = self
            .connections
            .connections
            .iter()
            .filter(|connection| connection_is_direct(connection))
            .count();
        let active_count = self.connections.connections.len();
        let proxied_count = active_count.saturating_sub(direct_count);
        format!(
            "connections active={} proxy={} direct={} up={} down={}",
            active_count,
            proxied_count,
            direct_count,
            format_bytes_opt(self.connections.upload_total),
            format_bytes_opt(self.connections.download_total)
        )
    }

    pub(super) fn maybe_refresh_connections(&mut self) {
        if self.last_connection_refresh.elapsed() < CONNECTION_REFRESH_INTERVAL {
            return;
        }
        self.last_connection_refresh = Instant::now();
        match self.client.fetch_connections() {
            Ok(connections) => {
                let now = Instant::now();
                let now_ms = crate::tui::metrics::now_unix_ms();
                let current_route = self
                    .current_route_target()
                    .map(|(_, group, node)| (group.name.clone(), node.to_string()));
                if let Some((selector, node_name)) = current_route {
                    let totals_by_connection = connection_totals_for_node(&connections, &node_name);
                    if let Some(previous) = &self.active_route_connection_baseline {
                        if previous.selector == selector && previous.node_name == node_name {
                            let elapsed = now
                                .saturating_duration_since(previous.observed_at)
                                .as_secs_f64();
                            if elapsed > 0.1 {
                                let growth_bytes = totals_by_connection
                                    .iter()
                                    .map(|(id, total)| {
                                        previous
                                            .totals_by_connection
                                            .get(id)
                                            .map_or(0, |old| total.saturating_sub(*old))
                                    })
                                    .sum::<u64>();
                                let bytes_per_sec = (growth_bytes as f64 / elapsed) as u64;
                                if let Some(store) = &mut self.metric_store {
                                    let _ = store.record_node_throughput(
                                        now_ms,
                                        &selector,
                                        &node_name,
                                        bytes_per_sec,
                                    );
                                }
                            }
                        }
                    }
                    self.active_route_connection_baseline = Some(
                        super::ActiveRouteConnectionBaseline {
                            observed_at: now,
                            selector,
                            node_name,
                            totals_by_connection,
                        },
                    );
                } else {
                    self.active_route_connection_baseline = None;
                }
                if let (Some(down_total), Some(up_total)) = (connections.download_total, connections.upload_total) {
                    if let Some((prev_time, prev_down, prev_up)) = self.last_traffic_totals {
                        let elapsed = now.saturating_duration_since(prev_time).as_secs_f64();
                        if elapsed > 0.1 {
                            let down_delta = down_total.saturating_sub(prev_down);
                            let up_delta = up_total.saturating_sub(prev_up);
                            let down_bps = (down_delta as f64 / elapsed) as u64;
                            let up_bps = (up_delta as f64 / elapsed) as u64;

                            self.last_active_traffic_rate = (
                                format!("{:.1}M/s", down_bps as f64 / 1_048_576.0),
                                format!("{:.1}M/s", up_bps as f64 / 1_048_576.0),
                            );

                            if let Some(store) = &mut self.metric_store {
                                let _ = store.record_traffic(now_ms, down_bps, up_bps);
                            }
                        }
                    }
                    self.last_traffic_totals = Some((now, down_total, up_total));
                }

                if let Some(scope) = self.active_auto_selection_scope() {
                    self.active_node_traffic
                        .observe(scope, Instant::now(), &connections);
                }
                self.connections = connections;
                self.connection_error = None;
            }
            Err(error) => {
                let detail = error.to_string();
                self.active_node_traffic.mark_unavailable(detail.clone());
                self.connection_error = Some(detail);
            }
        }
    }

    pub(super) fn open_connections_panel(&mut self) {
        self.show_connections = true;
        self.set_status_only("Showing active connections");
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyCode;

    use super::super::test_support::test_app;
    use super::connection_is_direct;
    use super::connection_totals_for_node;
    use crate::controller::{ConnectionInfo, ConnectionMetadata, ConnectionsSnapshot};

    fn test_connection(host: &str, chains: Vec<&str>) -> ConnectionInfo {
        ConnectionInfo {
            id: "conn-1".to_string(),
            download: 0,
            upload: 0,
            start: None,
            chains: chains.into_iter().map(ToString::to_string).collect(),
            rule: Some("clash_mode=规则 => route(手动选择)".to_string()),
            rule_payload: None,
            metadata: ConnectionMetadata {
                network: Some("tcp".to_string()),
                kind: Some("tun/tun-in".to_string()),
                source_ip: Some("172.19.0.1".to_string()),
                destination_ip: Some("1.1.1.1".to_string()),
                host: Some(host.to_string()),
                destination_port: Some("443".to_string()),
                source_port: None,
                process_path: None,
            },
        }
    }

    #[test]
    fn summary_counts_proxy_and_direct_connections() {
        let mut app = test_app();
        app.connections = ConnectionsSnapshot {
            upload_total: Some(1536),
            download_total: Some(2 * 1024 * 1024),
            memory: None,
            connections: vec![
                test_connection("www.google.com", vec!["node-a", "airtcp", "手动选择"]),
                test_connection("example.cn", vec!["国内直连"]),
            ],
        };
        assert_eq!(
            app.connections_summary_line(),
            "connections active=2 proxy=1 direct=1 up=1.5KiB down=2.0MiB"
        );
        assert!(connection_is_direct(&app.connections.connections[1]));
        assert!(!connection_is_direct(&app.connections.connections[0]));
    }

    #[test]
    fn pressing_c_opens_connection_details() {
        let mut app = test_app();
        app.handle_key(KeyCode::Char('c')).unwrap();
        assert!(app.show_connections);
        assert_eq!(app.status, "Showing active connections");
    }

    #[test]
    fn node_throughput_totals_only_include_exact_current_node_chain() {
        let mut current = test_connection("www.google.com", vec!["node-a", "select"]);
        current.id = "current".to_string();
        current.download = 3_000;
        current.upload = 2_000;
        let mut other = test_connection("example.com", vec!["node-a-backup", "select"]);
        other.id = "other".to_string();
        other.download = 9_000;
        let snapshot = ConnectionsSnapshot {
            connections: vec![current, other],
            ..ConnectionsSnapshot::default()
        };

        assert_eq!(
            connection_totals_for_node(&snapshot, "node-a"),
            [("current".to_string(), 5_000)].into_iter().collect()
        );
    }
}

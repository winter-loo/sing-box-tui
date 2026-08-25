use std::time::Instant;

use super::presentation::{format_bytes_opt, truncate_for_width};
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
                self.connections = connections;
                self.connection_error = None;
            }
            Err(error) => self.connection_error = Some(error.to_string()),
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
}

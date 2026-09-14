use super::*;

pub(crate) struct ConnectionsPanelSnapshot<'a> {
    pub(crate) summary: String,
    pub(crate) connections: &'a ConnectionsSnapshot,
    pub(crate) error: Option<&'a str>,
}

fn format_connection_rate(connection: &ConnectionInfo) -> String {
    format!("↓{} ↑{}", format_bytes(connection.download), format_bytes(connection.upload))
}

fn format_connection_line(connection: &ConnectionInfo, max_width: usize) -> String {
    let source = format_connection_source(connection);
    let target = format_connection_target(connection);
    let rule = connection.rule.as_deref().unwrap_or("-");
    let rate = format_connection_rate(connection);
    let chain = if connection.chains.is_empty() {
        "-".to_string()
    } else {
        connection.chains.join(" -> ")
    };
    truncate_for_width(
        &format!("{source:<12} {target:<30} {rule:<16} {rate:<18} {chain}"),
        max_width,
    )
}

fn format_connection_source(connection: &ConnectionInfo) -> String {
    let kind = connection.metadata.kind.as_deref().unwrap_or("-");
    let network = connection.metadata.network.as_deref().unwrap_or("-");
    format!("{kind}/{network}")
}

fn format_connection_target(connection: &ConnectionInfo) -> String {
    let target = connection
        .metadata
        .host
        .as_deref()
        .filter(|value| !value.is_empty())
        .or(connection.metadata.destination_ip.as_deref())
        .unwrap_or("-");
    match connection.metadata.destination_port.as_deref() {
        Some(port) if !port.is_empty() => format!("{target}:{port}"),
        _ => target.to_string(),
    }
}

pub(crate) fn format_bytes_opt(bytes: Option<u64>) -> String {
    bytes.map(format_bytes).unwrap_or_else(|| "-".to_string())
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit_index = 0;
    while value >= 1024.0 && unit_index + 1 < UNITS.len() {
        value /= 1024.0;
        unit_index += 1;
    }
    if unit_index == 0 {
        format!("{bytes}B")
    } else {
        format!("{value:.1}{}", UNITS[unit_index])
    }
}

use crate::tui::ds::{render_dialog_frame, Theme};

pub(crate) fn draw_connections_panel(frame: &mut Frame, snapshot: &ConnectionsPanelSnapshot<'_>) {
    let area = frame.area();
    let theme = Theme::detect();
    render_dialog_frame(
        frame,
        area,
        &theme,
        " ACTIVE CONNECTIONS (c) ",
        96,
        24,
        |frame, inner_area| {
            if inner_area.height == 0 || inner_area.width == 0 {
                return;
            }

            let (content_area, footer_area) = if inner_area.height >= 3 {
                let [c, f] = Layout::vertical([
                    Constraint::Min(1),
                    Constraint::Length(1),
                ])
                .areas(inner_area);
                (c, Some(f))
            } else {
                (inner_area, None)
            };

            let inner_width = content_area.width as usize;
            let max_rows = content_area.height.saturating_sub(2) as usize;

            let mut lines = vec![
                Line::from(Span::styled(
                    snapshot.summary.as_str(),
                    theme.style_base(),
                )),
                Line::from(vec![
                    Span::styled(format!("{:<12}", "Source"), theme.style_breadcrumb()),
                    Span::raw(" "),
                    Span::styled(format!("{:<30}", "Destination"), theme.style_breadcrumb()),
                    Span::raw(" "),
                    Span::styled(format!("{:<16}", "Rule"), theme.style_breadcrumb()),
                    Span::raw(" "),
                    Span::styled(format!("{:<18}", "Rate (↓ / ↑)"), theme.style_breadcrumb()),
                    Span::raw(" "),
                    Span::styled("Chain", theme.style_breadcrumb()),
                ]),
            ];

            if let Some(error) = snapshot.error {
                lines.push(Line::from(vec![
                    Span::styled("Error: ", theme.style_error()),
                    Span::styled(
                        truncate_for_width(error, inner_width.saturating_sub(7)),
                        theme.style_error(),
                    ),
                ]));
            } else if snapshot.connections.connections.is_empty() {
                lines.push(Line::from(Span::styled(
                    "No active connections",
                    theme.style_muted(),
                )));
            } else {
                for connection in snapshot.connections.connections.iter().take(max_rows) {
                    lines.push(Line::from(Span::styled(
                        format_connection_line(connection, inner_width),
                        theme.style_base(),
                    )));
                }
                let hidden = snapshot
                    .connections
                    .connections
                    .len()
                    .saturating_sub(max_rows);
                if hidden > 0 {
                    lines.push(Line::from(Span::styled(
                        format!("... {hidden} more connections"),
                        theme.style_muted(),
                    )));
                }
            }

            frame.render_widget(Paragraph::new(lines).style(theme.style_base()), content_area);

            if let Some(footer) = footer_area {
                let footer_line = Line::from(vec![
                    Span::styled("[Esc/c]", theme.style_footer_keys()),
                    Span::raw(" "),
                    Span::styled("Close", theme.style_muted()),
                    Span::raw("  "),
                    Span::styled("[r]", theme.style_footer_keys()),
                    Span::raw(" "),
                    Span::styled("Refresh", theme.style_muted()),
                ]);
                frame.render_widget(Paragraph::new(footer_line).style(theme.style_base()), footer);
            }
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::{ConnectionInfo, ConnectionMetadata};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    #[test]
    fn connection_and_byte_values_are_formatted_for_the_panel() {
        let connection = ConnectionInfo {
            id: "connection-1".to_string(),
            upload: 512,
            download: 2048,
            start: None,
            chains: vec!["node-a".to_string(), "airtcp".to_string()],
            rule: Some("route(select)".to_string()),
            rule_payload: None,
            metadata: ConnectionMetadata {
                network: Some("tcp".to_string()),
                kind: Some("tun/tun-in".to_string()),
                source_ip: Some("172.19.0.1".to_string()),
                destination_ip: Some("1.1.1.1".to_string()),
                host: Some("www.google.com".to_string()),
                destination_port: Some("443".to_string()),
                source_port: None,
                process_path: None,
            },
        };

        assert_eq!(format_bytes(512), "512B");
        assert_eq!(format_bytes(2048), "2.0KiB");
        let formatted = format_connection_line(&connection, 120);
        assert!(formatted.contains("www.google.com:443"));
        assert!(formatted.contains("route(select)"));
        assert!(formatted.contains("↓2.0KiB ↑512B"));
        assert!(formatted.contains("node-a -> airtcp"));
    }

    #[test]
    fn connections_panel_renders_dialog_frame_and_footer_hints() {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        let connections_data = ConnectionsSnapshot::default();
        let snapshot = ConnectionsPanelSnapshot {
            summary: "Active connections: 0 (proxy: 0, direct: 0)".to_string(),
            connections: &connections_data,
            error: None,
        };

        terminal
            .draw(|f| {
                draw_connections_panel(f, &snapshot);
            })
            .unwrap();

        let mut text = String::new();
        let buf = terminal.backend().buffer();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                text.push_str(buf[(x, y)].symbol());
            }
            text.push('\n');
        }

        assert!(text.contains("ACTIVE CONNECTIONS (c)"));
        assert!(text.contains("Active connections: 0"));
        assert!(text.contains("Source"));
        assert!(text.contains("Destination"));
        assert!(text.contains("Rule"));
        assert!(text.contains("Rate (↓ / ↑)"));
        assert!(text.contains("Chain"));
        assert!(text.contains("No active connections"));
        assert!(text.contains("[Esc/c] Close  [r] Refresh"));
    }
}

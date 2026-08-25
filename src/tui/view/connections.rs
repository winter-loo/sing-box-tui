use super::*;

pub(crate) struct ConnectionsPanelSnapshot<'a> {
    pub(crate) summary: String,
    pub(crate) connections: &'a ConnectionsSnapshot,
    pub(crate) error: Option<&'a str>,
}

fn format_connection_line(connection: &ConnectionInfo, max_width: usize) -> String {
    let source = format_connection_source(connection);
    let target = format_connection_target(connection);
    let chain = if connection.chains.is_empty() {
        "-".to_string()
    } else {
        connection.chains.join(" -> ")
    };
    let rule = connection.rule.as_deref().unwrap_or("-");
    truncate_for_width(
        &format!("{source:<14} {target:<28} {chain}  {rule}"),
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

pub(crate) fn draw_connections_panel(frame: &mut Frame, snapshot: &ConnectionsPanelSnapshot<'_>) {
    let frame_area = frame.area();
    let width = frame_area.width.saturating_sub(4).min(120);
    let height = frame_area.height.saturating_sub(4).min(24);
    let area = centered_rect(width.max(20), height.max(8), frame_area);
    frame.render_widget(Clear, area);

    let inner_width = area.width.saturating_sub(4) as usize;
    let max_rows = area.height.saturating_sub(6) as usize;
    let mut lines = vec![
        Line::from(snapshot.summary.as_str()),
        Line::from(vec![
            Span::styled("Source", Style::default().fg(Color::Cyan)),
            Span::raw("  "),
            Span::styled("Target", Style::default().fg(Color::Cyan)),
            Span::raw("  "),
            Span::styled("Chain", Style::default().fg(Color::Cyan)),
        ]),
    ];

    if let Some(error) = snapshot.error {
        lines.push(Line::from(format!(
            "error: {}",
            truncate_for_width(error, inner_width.saturating_sub(7))
        )));
    } else if snapshot.connections.connections.is_empty() {
        lines.push(Line::from("No active connections"));
    } else {
        lines.extend(
            snapshot
                .connections
                .connections
                .iter()
                .take(max_rows)
                .map(|connection| Line::from(format_connection_line(connection, inner_width))),
        );
        let hidden = snapshot
            .connections
            .connections
            .len()
            .saturating_sub(max_rows);
        if hidden > 0 {
            lines.push(Line::from(format!("... {hidden} more connections")));
        }
    }

    let widget = Paragraph::new(lines).block(
        Block::default()
            .title("Active Connections (c/Esc close, r refresh)")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan)),
    );
    frame.render_widget(widget, area);
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::{ConnectionInfo, ConnectionMetadata};

    #[test]
    fn connection_and_byte_values_are_formatted_for_the_panel() {
        let connection = ConnectionInfo {
            id: "connection-1".to_string(),
            upload: 0,
            download: 0,
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
        assert!(format_connection_line(&connection, 120).contains("www.google.com:443"));
        assert!(format_connection_line(&connection, 120).contains("node-a -> airtcp"));
    }
}

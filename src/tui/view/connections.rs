use super::*;

pub(crate) struct ConnectionsPanelSnapshot<'a> {
    pub(crate) summary: String,
    pub(crate) connections: &'a ConnectionsSnapshot,
    pub(crate) error: Option<&'a str>,
    pub(crate) last_success_age: Option<Duration>,
    pub(crate) scroll_offset: usize,
}

fn format_connection_transfer(connection: &ConnectionInfo) -> String {
    format!(
        "↓{} / ↑{}",
        format_bytes(connection.download),
        format_bytes(connection.upload)
    )
}

fn format_connection_chain_rule(connection: &ConnectionInfo) -> String {
    let chain = if connection.chains.is_empty() {
        "-".to_string()
    } else {
        connection.chains.join(" -> ")
    };
    let rule = connection.rule.as_deref().unwrap_or("-");
    match connection
        .rule_payload
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        Some(payload) => format!("{chain} · {rule} / {payload}"),
        None => format!("{chain} · {rule}"),
    }
}

fn format_connection_source(connection: &ConnectionInfo) -> String {
    let kind = connection.metadata.kind.as_deref().unwrap_or("-");
    let network = connection.metadata.network.as_deref().unwrap_or("-");
    format!("{kind}/{network}")
}

pub(crate) fn format_connection_target(connection: &ConnectionInfo) -> String {
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
    const UNITS: [&str; 7] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
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

use ratatui::layout::{Alignment, Rect};

use crate::tui::ds::{Theme, dialog_content_area, render_dialog_frame};

pub(crate) fn draw_connections_panel(frame: &mut Frame, snapshot: &ConnectionsPanelSnapshot<'_>) {
    let area = frame.area();
    let theme = Theme::detect();
    render_dialog_frame(
        frame,
        area,
        &theme,
        " ACTIVE CONNECTIONS ",
        u16::MAX,
        u16::MAX,
        |frame, inner_area| {
            if inner_area.height == 0 || inner_area.width == 0 {
                return;
            }

            let content_area = dialog_content_area(inner_area);
            let (toolbar_area, table_area, footer_area) = if content_area.height >= 5 {
                let gap = u16::from(content_area.height >= 14);
                let [toolbar, _, table, footer] = Layout::vertical([
                    Constraint::Length(1),
                    Constraint::Length(gap),
                    Constraint::Min(1),
                    Constraint::Length(1),
                ])
                .areas(content_area);
                (Some(toolbar), table, Some(footer))
            } else {
                (None, content_area, None)
            };

            if let Some(toolbar) = toolbar_area {
                let refresh_width = 11.min(toolbar.width);
                let [summary_area, refresh_area] =
                    Layout::horizontal([Constraint::Min(1), Constraint::Length(refresh_width)])
                        .areas(toolbar);
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        truncate_for_width(snapshot.summary.as_str(), summary_area.width as usize),
                        theme.style_breadcrumb(),
                    )))
                    .style(theme.style_base()),
                    summary_area,
                );
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled("r refresh", theme.style_success())))
                        .alignment(Alignment::Right)
                        .style(theme.style_base()),
                    refresh_area,
                );
            }

            let table_block = Block::default()
                .borders(Borders::ALL)
                .border_style(theme.style_muted())
                .style(theme.style_base());
            let table_inner = table_block.inner(table_area);
            frame.render_widget(table_block, table_area);
            if table_inner.width == 0 || table_inner.height == 0 {
                return;
            }

            let [header_area, rows_area] =
                Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(table_inner);
            let columns = connection_columns(header_area, snapshot.connections);
            render_connection_headers(frame, &theme, &columns);

            if let Some(error) = snapshot.error {
                let last_success = snapshot.last_success_age.map_or_else(
                    || "Last successful refresh: never".to_string(),
                    |age| format!("Last successful refresh: {} ago", format_age(age.as_secs())),
                );
                frame.render_widget(
                    Paragraph::new(vec![
                        Line::from(vec![
                            Span::styled("Error: ", theme.style_error()),
                            Span::styled(
                                truncate_for_width(
                                    error,
                                    rows_area.width.saturating_sub(7) as usize,
                                ),
                                theme.style_error(),
                            ),
                        ]),
                        Line::from(Span::styled(last_success, theme.style_muted())),
                    ]),
                    rows_area,
                );
            } else if snapshot.connections.connections.is_empty() {
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        "No active connections",
                        theme.style_muted(),
                    ))),
                    rows_area,
                );
            } else {
                let scroll_offset = snapshot
                    .scroll_offset
                    .min(snapshot.connections.connections.len().saturating_sub(1));
                let data_area = if scroll_offset > 0 && rows_area.height > 1 {
                    frame.render_widget(
                        Paragraph::new(format!("↑ {scroll_offset} earlier"))
                            .style(theme.style_muted()),
                        Rect::new(rows_area.x, rows_area.y, rows_area.width, 1),
                    );
                    Rect::new(
                        rows_area.x,
                        rows_area.y + 1,
                        rows_area.width,
                        rows_area.height - 1,
                    )
                } else {
                    rows_area
                };
                let row_stride = if data_area.height >= 10 { 2 } else { 1 };
                let max_rows = (data_area.height / row_stride).max(1) as usize;
                for (index, connection) in snapshot
                    .connections
                    .connections
                    .iter()
                    .skip(scroll_offset)
                    .take(max_rows)
                    .enumerate()
                {
                    let y = data_area.y.saturating_add(index as u16 * row_stride);
                    render_connection_row(frame, &theme, &columns, y, connection);
                }
                let hidden = snapshot
                    .connections
                    .connections
                    .len()
                    .saturating_sub(scroll_offset + max_rows);
                if hidden > 0 {
                    let y = data_area.bottom().saturating_sub(1);
                    frame.render_widget(
                        Paragraph::new(Line::from(Span::styled(
                            format!("… {hidden} more connections"),
                            theme.style_muted(),
                        ))),
                        Rect::new(rows_area.x, y, rows_area.width, 1),
                    );
                }
            }

            if let Some(footer) = footer_area {
                let footer_line = Line::from(vec![
                    Span::styled("[Esc/c/Enter]", theme.style_footer_keys()),
                    Span::raw(" "),
                    Span::styled("Close", theme.style_muted()),
                    Span::raw("  "),
                    Span::styled("[r]", theme.style_footer_keys()),
                    Span::raw(" "),
                    Span::styled("Refresh", theme.style_muted()),
                    Span::raw("  "),
                    Span::styled("[j/k]", theme.style_footer_keys()),
                    Span::raw(" "),
                    Span::styled("Scroll", theme.style_muted()),
                ]);
                frame.render_widget(
                    Paragraph::new(footer_line).style(theme.style_base()),
                    footer,
                );
            }
        },
    );
}

#[derive(Clone, Copy)]
struct ConnectionColumns {
    source: Rect,
    destination: Rect,
    chain_rule: Rect,
    transfer: Rect,
    age: Rect,
}

fn connection_columns(area: Rect, connections: &ConnectionsSnapshot) -> ConnectionColumns {
    let source_width = if area.width >= 90 { 12 } else { 9 };
    let transfer_width = connections
        .connections
        .iter()
        .map(|connection| {
            unicode_width::UnicodeWidthStr::width(format_connection_transfer(connection).as_str())
        })
        .max()
        .unwrap_or(0)
        .max("Down / Up".len()) as u16;
    let age_width = 7;
    let gaps = 4;
    let flexible = area
        .width
        .saturating_sub(source_width + transfer_width + age_width + gaps);
    let destination_width = (flexible * 45 / 100).max(8);
    let chain_width = flexible.saturating_sub(destination_width).max(8);
    let mut constraints = vec![
        Constraint::Length(source_width),
        Constraint::Length(1),
        Constraint::Length(destination_width),
        Constraint::Length(1),
        Constraint::Length(chain_width),
        Constraint::Length(1),
        Constraint::Length(transfer_width),
    ];
    constraints.push(Constraint::Length(1));
    constraints.push(Constraint::Length(age_width));
    let parts = Layout::horizontal(constraints).split(area);
    ConnectionColumns {
        source: parts[0],
        destination: parts[2],
        chain_rule: parts[4],
        transfer: parts[6],
        age: parts[8],
    }
}

fn render_connection_headers(frame: &mut Frame, theme: &Theme, columns: &ConnectionColumns) {
    for (area, label) in [
        (columns.source, "Source"),
        (columns.destination, "Destination"),
        (columns.chain_rule, "Chain / Rule"),
        (columns.transfer, "Down / Up"),
    ] {
        frame.render_widget(
            Paragraph::new(truncate_for_width(label, area.width as usize))
                .style(theme.style_breadcrumb()),
            area,
        );
    }
    frame.render_widget(
        Paragraph::new("Age").style(theme.style_breadcrumb()),
        columns.age,
    );
}

fn render_connection_row(
    frame: &mut Frame,
    theme: &Theme,
    columns: &ConnectionColumns,
    y: u16,
    connection: &ConnectionInfo,
) {
    let row_area = |column: Rect| Rect::new(column.x, y, column.width, 1);
    let values = [
        (
            columns.source,
            format_connection_source(connection),
            theme.style_base(),
        ),
        (
            columns.destination,
            format_connection_target(connection),
            theme.style_base(),
        ),
        (
            columns.chain_rule,
            format_connection_chain_rule(connection),
            theme.style_muted(),
        ),
        (
            columns.transfer,
            format_connection_transfer(connection),
            theme.style_success(),
        ),
    ];
    for (area, value, style) in values {
        frame.render_widget(
            Paragraph::new(truncate_for_width(&value, area.width as usize)).style(style),
            row_area(area),
        );
    }
    frame.render_widget(
        Paragraph::new(format_connection_age(connection.start.as_deref()))
            .style(theme.style_muted()),
        row_area(columns.age),
    );
}

fn format_connection_age(start: Option<&str>) -> String {
    format_connection_age_at(start, time::OffsetDateTime::now_utc())
}

fn format_connection_age_at(start: Option<&str>, now: time::OffsetDateTime) -> String {
    use time::format_description::well_known::Rfc3339;

    let Some(started_at) =
        start.and_then(|value| time::OffsetDateTime::parse(value, &Rfc3339).ok())
    else {
        return "—".to_string();
    };
    format_age((now - started_at).whole_seconds().max(0) as u64)
}

fn format_age(seconds: u64) -> String {
    match seconds {
        0..=59 => format!("{seconds}s"),
        60..=3_599 => format!("{}m", seconds / 60),
        3_600..=86_399 => format!("{}h", seconds / 3_600),
        _ => format!("{}d", seconds / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::{ConnectionInfo, ConnectionMetadata};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

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
        assert_eq!(format_connection_target(&connection), "www.google.com:443");
        assert_eq!(
            format_connection_chain_rule(&connection),
            "node-a -> airtcp · route(select)"
        );
        assert_eq!(format_connection_transfer(&connection), "↓2.0KiB / ↑512B");
        let now = time::OffsetDateTime::parse(
            "2026-09-19T09:12:11Z",
            &time::format_description::well_known::Rfc3339,
        )
        .unwrap();
        assert_eq!(
            format_connection_age_at(Some("2026-09-19T09:10:11Z"), now),
            "2m"
        );
        assert_eq!(format_connection_age_at(Some("invalid"), now), "—");
    }

    #[test]
    fn connections_panel_renders_dialog_frame_and_footer_hints() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        let connections_data = ConnectionsSnapshot::default();
        let snapshot = ConnectionsPanelSnapshot {
            summary: "Active connections: 0 (proxy: 0, direct: 0)".to_string(),
            connections: &connections_data,
            error: None,
            last_success_age: None,
            scroll_offset: 0,
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

        assert!(text.contains("ACTIVE CONNECTIONS"));
        assert!(text.contains("Active connections: 0"));
        assert!(text.contains("Source"));
        assert!(text.contains("Destination"));
        assert!(text.contains("Chain / Rule"));
        assert!(text.contains("Down / Up"));
        assert!(text.contains("Age"));
        assert!(text.contains("No active connections"));
        assert!(text.contains("[Esc/c/Enter] Close  [r] Refresh"));
    }

    #[test]
    fn refresh_error_keeps_last_success_age_visible_without_stale_rows() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let connections_data = ConnectionsSnapshot {
            connections: vec![ConnectionInfo {
                id: "stale".into(),
                metadata: ConnectionMetadata {
                    host: Some("must-not-render.example".into()),
                    ..ConnectionMetadata::default()
                },
                upload: 0,
                download: 0,
                start: None,
                chains: Vec::new(),
                rule: None,
                rule_payload: None,
            }],
            ..ConnectionsSnapshot::default()
        };
        let snapshot = ConnectionsPanelSnapshot {
            summary: "refresh failed".into(),
            connections: &connections_data,
            error: Some("controller unavailable"),
            last_success_age: Some(Duration::from_secs(125)),
            scroll_offset: 0,
        };

        terminal
            .draw(|frame| draw_connections_panel(frame, &snapshot))
            .unwrap();
        let text = buffer_to_text(terminal.backend().buffer());

        assert!(text.contains("Error: controller unavailable"));
        assert!(text.contains("Last successful refresh: 2m ago"));
        assert!(!text.contains("must-not-render.example"));
    }

    #[test]
    fn connection_scroll_exposes_rows_beyond_the_initial_window() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let connection = |id: &str, host: &str| ConnectionInfo {
            id: id.into(),
            upload: 1,
            download: 2,
            start: None,
            chains: vec!["node-a".into()],
            rule: None,
            rule_payload: None,
            metadata: ConnectionMetadata {
                host: Some(host.into()),
                ..ConnectionMetadata::default()
            },
        };
        let connections_data = ConnectionsSnapshot {
            connections: vec![
                connection("one", "one.example"),
                connection("two", "two.example"),
                connection("three", "three.example"),
            ],
            ..ConnectionsSnapshot::default()
        };
        let snapshot = ConnectionsPanelSnapshot {
            summary: "connections active=3".into(),
            connections: &connections_data,
            error: None,
            last_success_age: None,
            scroll_offset: 2,
        };

        terminal
            .draw(|frame| draw_connections_panel(frame, &snapshot))
            .unwrap();
        let text = buffer_to_text(terminal.backend().buffer());

        assert!(text.contains("↑ 2 earlier"));
        assert!(text.contains("three.example"));
        assert!(!text.contains("one.example"));
    }

    #[test]
    fn compact_connections_keep_transfer_values_visible_beside_long_grapheme_labels() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let connections_data = ConnectionsSnapshot {
            connections: vec![ConnectionInfo {
                id: "long".into(),
                upload: 512,
                download: 2048,
                start: Some("2026-09-19T09:10:11Z".into()),
                chains: vec!["selector-👩‍💻-超长节点名".into()],
                rule: Some("rule-set/very-long-policy".into()),
                rule_payload: None,
                metadata: ConnectionMetadata {
                    network: Some("tcp".into()),
                    kind: Some("tun".into()),
                    host: Some("👩‍💻-超长目标-destination.example.test".into()),
                    destination_port: Some("443".into()),
                    ..ConnectionMetadata::default()
                },
            }],
            ..ConnectionsSnapshot::default()
        };
        let snapshot = ConnectionsPanelSnapshot {
            summary: "connections active=1 proxy=1 direct=0".into(),
            connections: &connections_data,
            error: None,
            last_success_age: None,
            scroll_offset: 0,
        };

        terminal
            .draw(|f| draw_connections_panel(f, &snapshot))
            .unwrap();

        let text = buffer_to_text(terminal.backend().buffer());
        assert!(text.contains("↓2.0KiB / ↑512B"));
        assert!(text.contains("👩‍💻"));
        assert!(!text.contains('\u{fffd}'));
        assert!(text.contains("[Esc/c/Enter] Close"));
    }

    #[test]
    fn compact_connections_keep_large_transfer_values_visible() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let connections_data = ConnectionsSnapshot {
            connections: vec![ConnectionInfo {
                id: "large".into(),
                upload: u64::MAX,
                download: u64::MAX,
                start: None,
                chains: vec!["very-long-👩‍💻-超长-node-name".into()],
                rule: Some("MATCH".into()),
                rule_payload: None,
                metadata: ConnectionMetadata {
                    host: Some("very-long-👩‍💻-超长-destination.example".into()),
                    ..ConnectionMetadata::default()
                },
            }],
            ..ConnectionsSnapshot::default()
        };
        let snapshot = ConnectionsPanelSnapshot {
            summary: "connections active=1".into(),
            connections: &connections_data,
            error: None,
            last_success_age: None,
            scroll_offset: 0,
        };

        terminal
            .draw(|frame| draw_connections_panel(frame, &snapshot))
            .unwrap();
        let text = buffer_to_text(terminal.backend().buffer());
        assert!(text.contains("↓16.0EiB / ↑16.0EiB"), "{text}");
    }

    #[test]
    fn connections_dialog_fills_supported_reference_and_non_standard_viewports() {
        for (width, height) in [(80, 24), (120, 30), (137, 35)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            let connections_data = ConnectionsSnapshot::default();
            let snapshot = ConnectionsPanelSnapshot {
                summary: "connections active=0 proxy=0 direct=0".into(),
                connections: &connections_data,
                error: None,
                last_success_age: None,
                scroll_offset: 0,
            };

            terminal
                .draw(|f| draw_connections_panel(f, &snapshot))
                .unwrap();
            let buffer = terminal.backend().buffer();

            assert_eq!(buffer[(0, 0)].symbol(), "┌");
            assert_eq!(buffer[(width - 1, 0)].symbol(), "┐");
            assert_eq!(buffer[(0, height - 1)].symbol(), "└");
            assert_eq!(buffer[(width - 1, height - 1)].symbol(), "┘");
            assert!(row_text(buffer, height - 2).contains("[Esc/c/Enter] Close"));
        }
    }

    fn buffer_to_text(buffer: &ratatui::buffer::Buffer) -> String {
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        text
    }

    fn row_text(buffer: &ratatui::buffer::Buffer, y: u16) -> String {
        (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect()
    }
}

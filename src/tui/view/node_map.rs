use std::collections::BTreeMap;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};

use super::shared::{centered_rect, truncate_for_width};
use crate::node_map::{NodeLocation, NodeMapState, NodeTone, is_land_grid, project_coords};

pub(crate) fn draw_node_map_panel(frame: &mut Frame, state: &NodeMapState) {
    let frame_area = frame.area();
    let width = frame_area.width.saturating_sub(4).min(136).max(70);
    let height = frame_area.height.saturating_sub(2).min(40).max(22);
    let area = centered_rect(width, height, frame_area);
    frame.render_widget(Clear, area);

    let stats = state.stats();
    let filter_text = if state.filter_reachable_only {
        "Filter: Reachable Only (f)"
    } else {
        "Filter: All Nodes (f)"
    };

    let resolving_text = if state.is_resolving {
        " [Resolving GeoIP...]"
    } else {
        ""
    };

    let title = Line::from(vec![
        Span::styled(
            " 🌍 Node Physical Location World Map ",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                "(Nodes: {} | Plotted: {} | Reachable: {} | Countries: {}{}) ",
                stats.total_nodes,
                stats.plotted_nodes,
                stats.reachable_nodes,
                stats.unique_countries,
                resolving_text
            ),
            Style::default().fg(Color::Gray),
        ),
    ]);

    let outer_block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    let inner = outer_block.inner(area);
    frame.render_widget(outer_block, area);

    let [content_area, footer_area] =
        Layout::vertical([Constraint::Min(16), Constraint::Length(1)]).areas(inner);

    let [map_pane, info_pane] =
        Layout::horizontal([Constraint::Percentage(66), Constraint::Percentage(34)])
            .areas(content_area);

    draw_map_canvas(frame, map_pane, state);
    draw_info_pane(frame, info_pane, state);

    // Footer instructions
    let footer_line = Line::from(vec![
        Span::styled(" j/k or ↑/↓", Style::default().fg(Color::Cyan)),
        Span::raw(" Select  "),
        Span::styled("g/G", Style::default().fg(Color::Cyan)),
        Span::raw(" First/Last  "),
        Span::styled("f", Style::default().fg(Color::Cyan)),
        Span::styled(format!(" {filter_text}  "), Style::default().fg(Color::Yellow)),
        Span::styled("r", Style::default().fg(Color::Cyan)),
        Span::raw(" Refresh  "),
        Span::styled("Esc/Enter/M", Style::default().fg(Color::Cyan)),
        Span::raw(" Close"),
    ]);
    frame.render_widget(Paragraph::new(footer_line), footer_area);
}

fn draw_map_canvas(frame: &mut Frame, area: Rect, state: &NodeMapState) {
    let [map_rect, legend_rect] =
        Layout::vertical([Constraint::Min(12), Constraint::Length(1)]).areas(area);

    let map_block = Block::default()
        .title(" World Map (Equirectangular Projection) ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    let canvas_area = map_block.inner(map_rect);
    frame.render_widget(map_block, map_rect);

    let width = canvas_area.width as usize;
    let height = canvas_area.height as usize;
    if width < 10 || height < 6 {
        return;
    }

    // Equator and Prime Meridian rows/cols
    let eq_y = height / 2;
    let pm_x = width / 2;

    // Cluster nodes by projected (x, y)
    let filtered_indices = state.filtered_indices();
    let selected_filtered_pos = state
        .selected_index
        .min(filtered_indices.len().saturating_sub(1));
    let selected_orig_idx = filtered_indices.get(selected_filtered_pos).copied();

    let mut clusters: BTreeMap<(usize, usize), Vec<(usize, &NodeLocation)>> = BTreeMap::new();
    for &idx in &filtered_indices {
        if let Some(node) = state.nodes.get(idx) {
            if let Some(loc) = &node.location {
                let (nx, ny) = project_coords(loc.latitude, loc.longitude, width, height);
                clusters.entry((nx, ny)).or_default().push((idx, node));
            }
        }
    }

    // Build map lines character by character
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(height);
    for y in 0..height {
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(width);
        for x in 0..width {
            if let Some(cluster) = clusters.get(&(x, y)) {
                let contains_selected = selected_orig_idx
                    .map(|sel| cluster.iter().any(|(i, _)| *i == sel))
                    .unwrap_or(false);

                if contains_selected {
                    spans.push(Span::styled(
                        "⦿",
                        Style::default()
                            .fg(Color::Yellow)
                            .bg(Color::Rgb(160, 40, 40))
                            .add_modifier(Modifier::BOLD),
                    ));
                } else if cluster.len() > 1 {
                    let count_char = if cluster.len() <= 9 {
                        char::from_digit(cluster.len() as u32, 10).unwrap_or('+')
                    } else {
                        '+'
                    };
                    spans.push(Span::styled(
                        count_char.to_string(),
                        Style::default()
                            .fg(Color::LightCyan)
                            .bg(Color::Rgb(20, 50, 70))
                            .add_modifier(Modifier::BOLD),
                    ));
                } else {
                    let (_, node) = cluster[0];
                    let (marker_glyph, marker_color) = match node.tone {
                        NodeTone::Success => ("●", Color::LightGreen),
                        NodeTone::Error => ("●", Color::LightRed),
                        NodeTone::Pending => ("●", Color::Yellow),
                        NodeTone::Missing => ("●", Color::DarkGray),
                    };
                    spans.push(Span::styled(
                        marker_glyph,
                        Style::default()
                            .fg(marker_color)
                            .add_modifier(Modifier::BOLD),
                    ));
                }
            } else if is_land_grid(x, y, width, height) {
                spans.push(Span::styled(
                    "░",
                    Style::default().fg(Color::Rgb(55, 80, 65)),
                ));
            } else {
                // Ocean
                if y == eq_y && x % 4 == 0 {
                    spans.push(Span::styled(
                        "·",
                        Style::default().fg(Color::Rgb(35, 55, 75)),
                    ));
                } else if x == pm_x && y % 2 == 0 {
                    spans.push(Span::styled(
                        "·",
                        Style::default().fg(Color::Rgb(35, 55, 75)),
                    ));
                } else {
                    spans.push(Span::raw(" "));
                }
            }
        }
        lines.push(Line::from(spans));
    }

    frame.render_widget(Paragraph::new(lines), canvas_area);

    // Map Legend
    let legend = Line::from(vec![
        Span::styled("● ", Style::default().fg(Color::LightGreen)),
        Span::raw("Reachable  "),
        Span::styled("● ", Style::default().fg(Color::Yellow)),
        Span::raw("Pending  "),
        Span::styled("● ", Style::default().fg(Color::LightRed)),
        Span::raw("Unreachable  "),
        Span::styled("⦿ ", Style::default().fg(Color::Yellow)),
        Span::raw("Selected  "),
        Span::styled("2+ ", Style::default().fg(Color::LightCyan)),
        Span::raw("Cluster"),
    ]);
    frame.render_widget(Paragraph::new(legend), legend_rect);
}

fn draw_info_pane(frame: &mut Frame, area: Rect, state: &NodeMapState) {
    let [detail_area, list_area] =
        Layout::vertical([Constraint::Length(10), Constraint::Min(6)]).areas(area);

    // Detail card for currently selected node
    let detail_block = Block::default()
        .title(" Selected Node Details ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    let detail_inner = detail_block.inner(detail_area);
    frame.render_widget(detail_block, detail_area);

    let selected_node = state.selected_node();
    let detail_lines = if let Some(node) = selected_node {
        let latency_str = node
            .latency_ms
            .map(|ms| format!("{ms} ms"))
            .unwrap_or_else(|| "--".to_string());
        let current_star = if node.is_current { "  *" } else { "" };
        let available_w = detail_inner.width.saturating_sub(12) as usize;

        vec![
            Line::from(vec![
                Span::styled("Tag:      ", Style::default().fg(Color::Cyan)),
                Span::styled(
                    truncate_for_width(&node.tag, available_w),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(current_star, Style::default().fg(Color::Green)),
            ]),
            Line::from(vec![
                Span::styled("Type:     ", Style::default().fg(Color::Cyan)),
                Span::styled(node.outbound_type.as_str(), Style::default().fg(Color::Yellow)),
            ]),
            Line::from(vec![
                Span::styled("Host:     ", Style::default().fg(Color::Cyan)),
                Span::styled(
                    truncate_for_width(&node.server_host, available_w),
                    Style::default().fg(Color::Gray),
                ),
            ]),
            Line::from(vec![
                Span::styled("IP:       ", Style::default().fg(Color::Cyan)),
                Span::styled(
                    node.ip.as_deref().unwrap_or("Resolving..."),
                    Style::default().fg(Color::LightYellow),
                ),
            ]),
            Line::from(vec![
                Span::styled("Location: ", Style::default().fg(Color::Cyan)),
                Span::styled(
                    truncate_for_width(&node.display_location(), available_w),
                    Style::default().fg(Color::LightCyan),
                ),
            ]),
            Line::from(vec![
                Span::styled("Coords:   ", Style::default().fg(Color::Cyan)),
                Span::styled(node.display_coordinates(), Style::default().fg(Color::LightGreen)),
            ]),
            Line::from(vec![
                Span::styled("Quality:  ", Style::default().fg(Color::Cyan)),
                Span::styled(
                    format!("{latency_str} ({})", node.reachability),
                    match node.tone {
                        NodeTone::Success => Style::default().fg(Color::LightGreen),
                        NodeTone::Error => Style::default().fg(Color::LightRed),
                        _ => Style::default().fg(Color::Yellow),
                    },
                ),
            ]),
        ]
    } else {
        vec![Line::from("No node selected")]
    };

    frame.render_widget(Paragraph::new(detail_lines), detail_inner);

    // List of nodes
    let filtered_indices = state.filtered_indices();
    let selected_pos = state
        .selected_index
        .min(filtered_indices.len().saturating_sub(1));

    let list_items: Vec<ListItem> = filtered_indices
        .iter()
        .map(|&orig_idx| {
            let node = &state.nodes[orig_idx];
            let (status_dot, dot_color) = match node.tone {
                NodeTone::Success => ("●", Color::LightGreen),
                NodeTone::Error => ("●", Color::LightRed),
                NodeTone::Pending => ("●", Color::Yellow),
                NodeTone::Missing => ("●", Color::DarkGray),
            };

            let country_badge = node
                .location
                .as_ref()
                .map(|l| format!("[{}]", l.country_code))
                .unwrap_or_else(|| "[--]".to_string());

            let current_indicator = if node.is_current { " *" } else { "" };
            let max_name_len = list_area.width.saturating_sub(16) as usize;
            let display_name = truncate_for_width(&node.tag, max_name_len);

            ListItem::new(Line::from(vec![
                Span::styled(format!("{status_dot} "), Style::default().fg(dot_color)),
                Span::styled(
                    format!("{country_badge:<5} "),
                    Style::default().fg(Color::Cyan),
                ),
                Span::raw(display_name),
                Span::styled(current_indicator, Style::default().fg(Color::Green)),
            ]))
        })
        .collect();

    let list_block = Block::default()
        .title(format!(" Nodes ({}/{}) ", filtered_indices.len(), state.nodes.len()))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    let list_widget = List::new(list_items)
        .block(list_block)
        .highlight_style(
            Style::default()
                .fg(Color::White)
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");

    let mut list_state = ListState::default().with_selected(Some(selected_pos));
    frame.render_stateful_widget(list_widget, list_area, &mut list_state);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use crate::node_map::{GeoLocation, NodeLocation, NodeMapState, NodeTone};

    #[test]
    fn node_map_panel_renders_without_panicking() {
        let nodes = vec![
            NodeLocation {
                tag: "US-LA-01".to_string(),
                outbound_type: "vless".to_string(),
                server_host: "us.example.com".to_string(),
                server_port: Some(443),
                ip: Some("1.2.3.4".to_string()),
                location: Some(GeoLocation {
                    country: "United States".to_string(),
                    country_code: "US".to_string(),
                    region: "CA".to_string(),
                    city: "Los Angeles".to_string(),
                    latitude: 34.05,
                    longitude: -118.24,
                }),
                is_current: true,
                latency_ms: Some(135),
                reachability: "stable reachable".to_string(),
                tone: NodeTone::Success,
            },
            NodeLocation {
                tag: "JP-Tokyo-01".to_string(),
                outbound_type: "vmess".to_string(),
                server_host: "jp.example.com".to_string(),
                server_port: Some(443),
                ip: Some("5.6.7.8".to_string()),
                location: Some(GeoLocation {
                    country: "Japan".to_string(),
                    country_code: "JP".to_string(),
                    region: "Kanto".to_string(),
                    city: "Tokyo".to_string(),
                    latitude: 35.68,
                    longitude: 139.65,
                }),
                is_current: false,
                latency_ms: Some(68),
                reachability: "reachable".to_string(),
                tone: NodeTone::Success,
            },
        ];

        let state = NodeMapState::new(nodes);
        let backend = TestBackend::new(120, 36);
        let mut terminal = Terminal::new(backend).expect("terminal initializes");
        terminal
            .draw(|frame| draw_node_map_panel(frame, &state))
            .expect("draws node map panel");

        let buffer = terminal.backend().buffer().clone();
        let rendered_text = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(rendered_text.contains("World Map"));
        assert!(rendered_text.contains("Selected Node Details"));
        assert!(rendered_text.contains("US-LA-01"));
        assert!(rendered_text.contains("1.2.3.4"));
        assert!(rendered_text.contains("Los Angeles"));
    }
}


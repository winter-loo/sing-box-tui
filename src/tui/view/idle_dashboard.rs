use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::canvas::{Canvas, Line as CanvasLine, Points};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};

use crate::tui::ds::{Theme, ViewportMode, render_breadcrumb, render_footer, render_unsupported_guard};
use crate::tui::metrics::{
    LatencySample, METRIC_RETENTION_WINDOW_MS, MetricStore, RouteInterval, TrafficSample,
    now_unix_ms,
};

#[derive(Clone, Debug)]
pub(crate) struct ActiveNodeQualitySnapshot<'a> {
    pub(crate) node_name: &'a str,
    pub(crate) current_latency_ms: Option<u64>,
    pub(crate) warm_median_ms: Option<u64>,
    pub(crate) p95_ms: Option<u64>,
    pub(crate) cold_start_ms: Option<u64>,
    pub(crate) sustained_speed_label: Option<String>,
    pub(crate) reachability_label: &'a str,
}

#[derive(Clone, Debug)]
pub(crate) struct ActiveConnectionSummary<'a> {
    pub(crate) destination: &'a str,
    pub(crate) rate_label: String,
    pub(crate) rule: &'a str,
}

pub(crate) struct IdleDashboardSnapshot<'a> {
    pub(crate) active_provider: &'a str,
    pub(crate) active_node: &'a str,
    pub(crate) status_text: &'a str,
    pub(crate) current_down_rate: &'a str,
    pub(crate) current_up_rate: &'a str,
    pub(crate) traffic_samples: &'a [TrafficSample],
    pub(crate) latency_samples: &'a [LatencySample],
    pub(crate) route_intervals: &'a [RouteInterval],
    pub(crate) node_quality: Option<ActiveNodeQualitySnapshot<'a>>,
    pub(crate) active_connections: Vec<ActiveConnectionSummary<'a>>,
}

/// Main entry point for rendering the Idle Dashboard (120x30 Canonical or 80x24 Compact)
pub(crate) fn render_idle_dashboard(frame: &mut Frame, snapshot: &IdleDashboardSnapshot<'_>) {
    let area = frame.area();
    let theme = Theme::default();
    let mode = ViewportMode::determine(area);

    if mode.is_unsupported() {
        render_unsupported_guard(frame, area, &theme);
        return;
    }

    // Fill entire screen with canvas background
    frame.render_widget(Block::default().style(theme.style_base()), area);

    // Root vertical layout: Breadcrumb (Row 1), Main Panels (Remaining), Footer (Row 30 or 24)
    let [header_area, main_area, footer_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(10),
        Constraint::Length(1),
    ])
    .areas(area);

    // 1. Breadcrumb
    render_breadcrumb(
        frame,
        header_area,
        &theme,
        &["DASHBOARD", snapshot.active_provider, snapshot.active_node],
    );

    // 2. Main content panels
    match mode {
        ViewportMode::Standard => {
            let right_width = 38.min(main_area.width.saturating_sub(60));
            let [charts_area, side_area] = Layout::horizontal([
                Constraint::Min(60),
                Constraint::Length(right_width),
            ])
            .areas(main_area);

            render_core_history_panel(frame, charts_area, &theme, snapshot);
            render_side_panels(frame, side_area, &theme, snapshot);
        }
        ViewportMode::Compact => {
            render_core_history_panel(frame, main_area, &theme, snapshot);
        }
        ViewportMode::Unsupported { .. } => unreachable!(),
    }

    // 3. Footer
    let shortcuts = match mode {
        ViewportMode::Standard => [
            ("Ctrl+K", "menu"),
            ("c", "connections"),
            ("i", "quality"),
            ("o", "settings"),
            ("?", "help"),
        ],
        ViewportMode::Compact => [
            ("Ctrl+K", "menu"),
            ("c", "conn"),
            ("i", "quality"),
            ("o", "settings"),
            ("?", "help"),
        ],
        _ => unreachable!(),
    };

    render_footer(
        frame,
        footer_area,
        &theme,
        &shortcuts,
        snapshot.status_text,
        Some((snapshot.current_down_rate, snapshot.current_up_rate)),
    );
}

/// Renders the 30-min Core History panel with Route Switch legend and double-stacked charts
fn render_core_history_panel(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    snapshot: &IdleDashboardSnapshot<'_>,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.style_muted())
        .title(" CORE HISTORY · 30 MIN ")
        .style(theme.style_base());

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height < 6 {
        return;
    }

    // Layout inside Core History:
    let [latency_area, traffic_area] = Layout::vertical([
        Constraint::Percentage(50),
        Constraint::Percentage(50),
    ])
    .areas(inner);

    render_route_latency_chart(frame, latency_area, theme, snapshot);
    render_core_traffic_chart(frame, traffic_area, theme, snapshot);
}

/// Renders the Route Latency Chart (Canvas + Braille)
fn render_route_latency_chart(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    snapshot: &IdleDashboardSnapshot<'_>,
) {
    let now_ms = now_unix_ms();
    let cutoff_ms = now_ms.saturating_sub(METRIC_RETENTION_WINDOW_MS);

    // Compute max latency for Y-axis
    let mut max_ms: f64 = 60.0;
    for s in snapshot.latency_samples {
        if s.latency_ms as f64 > max_ms {
            max_ms = s.latency_ms as f64;
        }
    }
    let y_max = (max_ms * 1.25).min(2000.0);

    let current_latency_label = snapshot
        .latency_samples
        .last()
        .map_or("--".to_string(), |s| format!("{} ms", s.latency_ms));

    let title = format!("ROUTE LATENCY · ms                        {}", current_latency_label);
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(theme.style_muted())
        .title(title);

    let intervals = snapshot.route_intervals;
    let samples = snapshot.latency_samples;

    let canvas = Canvas::default()
        .block(block)
        .marker(Marker::Braille)
        .x_bounds([0.0, 1800.0])
        .y_bounds([0.0, y_max])
        .paint(move |ctx| {
            if samples.is_empty() {
                return;
            }

            for i in 0..samples.len() {
                let s_curr = &samples[i];
                let x2 = ((s_curr.recorded_at_ms - cutoff_ms) as f64 / 1000.0).clamp(0.0, 1800.0);
                let y2 = s_curr.latency_ms as f64;

                // Determine route color at this timestamp
                let mut color = theme.text_accent();
                for interval in intervals {
                    if s_curr.recorded_at_ms >= interval.started_at_ms
                        && interval.ended_at_ms.map_or(true, |end| s_curr.recorded_at_ms <= end)
                    {
                        color = theme.route_color(interval.interval_index);
                        break;
                    }
                }

                if i > 0 {
                    let s_prev = &samples[i - 1];
                    // If no gap, draw line segment
                    if !MetricStore::has_gap(s_prev.recorded_at_ms, s_curr.recorded_at_ms) {
                        let x1 = ((s_prev.recorded_at_ms - cutoff_ms) as f64 / 1000.0).clamp(0.0, 1800.0);
                        let y1 = s_prev.latency_ms as f64;
                        ctx.draw(&CanvasLine { x1, y1, x2, y2, color });
                    } else {
                        // Point only across gap
                        ctx.draw(&Points { coords: &[(x2, y2)], color });
                    }
                } else {
                    ctx.draw(&Points { coords: &[(x2, y2)], color });
                }
            }
        });

    frame.render_widget(canvas, area);
}

/// Renders the Core Traffic Chart (Canvas + Braille, DOWN solid, UP dotted)
fn render_core_traffic_chart(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    snapshot: &IdleDashboardSnapshot<'_>,
) {
    let now_ms = now_unix_ms();
    let cutoff_ms = now_ms.saturating_sub(METRIC_RETENTION_WINDOW_MS);

    // Compute peak throughput for Y-axis (min 1 MiB/s)
    let mut max_bytes: f64 = 1_048_576.0;
    for s in snapshot.traffic_samples {
        if s.down_bytes_per_sec as f64 > max_bytes {
            max_bytes = s.down_bytes_per_sec as f64;
        }
        if s.up_bytes_per_sec as f64 > max_bytes {
            max_bytes = s.up_bytes_per_sec as f64;
        }
    }
    let y_max = max_bytes * 1.2;
    let y_max_label = format!("{:.1}M", y_max / 1_048_576.0);

    let title = format!("CORE TRAFFIC · MiB/s                  DOWN ──  UP ╌╌  (max: {})", y_max_label);
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(theme.style_muted())
        .title(title);

    let intervals = snapshot.route_intervals;
    let samples = snapshot.traffic_samples;

    let canvas = Canvas::default()
        .block(block)
        .marker(Marker::Braille)
        .x_bounds([0.0, 1800.0])
        .y_bounds([0.0, y_max])
        .paint(move |ctx| {
            if samples.is_empty() {
                return;
            }

            // 1. Draw DOWN traffic (solid line)
            for i in 0..samples.len() {
                let s_curr = &samples[i];
                let x2 = ((s_curr.recorded_at_ms - cutoff_ms) as f64 / 1000.0).clamp(0.0, 1800.0);
                let y2 = s_curr.down_bytes_per_sec as f64;

                let mut color = theme.text_accent();
                for interval in intervals {
                    if s_curr.recorded_at_ms >= interval.started_at_ms
                        && interval.ended_at_ms.map_or(true, |end| s_curr.recorded_at_ms <= end)
                    {
                        color = theme.route_color(interval.interval_index);
                        break;
                    }
                }

                if i > 0 {
                    let s_prev = &samples[i - 1];
                    if !MetricStore::has_gap(s_prev.recorded_at_ms, s_curr.recorded_at_ms) {
                        let x1 = ((s_prev.recorded_at_ms - cutoff_ms) as f64 / 1000.0).clamp(0.0, 1800.0);
                        let y1 = s_prev.down_bytes_per_sec as f64;
                        ctx.draw(&CanvasLine { x1, y1, x2, y2, color });
                    } else {
                        ctx.draw(&Points { coords: &[(x2, y2)], color });
                    }
                } else {
                    ctx.draw(&Points { coords: &[(x2, y2)], color });
                }
            }

            // 2. Draw UP traffic (dotted line: alternating points with interval color)
            for i in 0..samples.len() {
                let s_curr = &samples[i];
                let x2 = ((s_curr.recorded_at_ms - cutoff_ms) as f64 / 1000.0).clamp(0.0, 1800.0);
                let y2 = s_curr.up_bytes_per_sec as f64;

                let mut color = theme.text_accent();
                for interval in intervals {
                    if s_curr.recorded_at_ms >= interval.started_at_ms
                        && interval.ended_at_ms.map_or(true, |end| s_curr.recorded_at_ms <= end)
                    {
                        color = theme.route_color(interval.interval_index);
                        break;
                    }
                }
                // Dotted effect: render point at every sample
                ctx.draw(&Points { coords: &[(x2, y2)], color });
            }
        });

    frame.render_widget(canvas, area);
}

/// Renders the two side panels on the right side of the 120x30 standard layout:
/// 1. Node Quality
/// 2. Active Connections
fn render_side_panels(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    snapshot: &IdleDashboardSnapshot<'_>,
) {
    let [quality_area, conn_area] = Layout::vertical([
        Constraint::Length(12),
        Constraint::Min(8),
    ])
    .areas(area);

    // --- Panel 1: Node Quality ---
    let quality_title = format!(" NODE QUALITY · {} ", snapshot.active_node);
    let quality_block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.style_muted())
        .title(quality_title)
        .style(theme.style_base());

    let quality_inner = quality_block.inner(quality_area);
    frame.render_widget(quality_block, quality_area);

    let mut quality_lines = Vec::new();
    if let Some(q) = &snapshot.node_quality {
        quality_lines.push(Line::from(vec![
            Span::styled("Node:         ", theme.style_muted()),
            Span::styled(q.node_name, theme.style_breadcrumb()),
        ]));
        quality_lines.push(Line::from(vec![
            Span::styled("Reachability: ", theme.style_muted()),
            Span::styled(q.reachability_label, theme.style_success()),
        ]));
        if let Some(lat) = q.current_latency_ms {
            quality_lines.push(Line::from(vec![
                Span::styled("Current:      ", theme.style_muted()),
                Span::styled(format!("{} ms", lat), theme.style_breadcrumb()),
            ]));
        }
        quality_lines.push(Line::from(vec![
            Span::styled("Median / P95: ", theme.style_muted()),
            Span::styled(
                format!(
                    "{} / {}",
                    q.warm_median_ms.map_or("--".to_string(), |v| format!("{}ms", v)),
                    q.p95_ms.map_or("--".to_string(), |v| format!("{}ms", v))
                ),
                theme.style_warning(),
            ),
        ]));
        quality_lines.push(Line::from(vec![
            Span::styled("Cold Start:   ", theme.style_muted()),
            Span::styled(
                q.cold_start_ms.map_or("--".to_string(), |v| format!("{} ms", v)),
                theme.style_muted(),
            ),
        ]));
        if let Some(speed) = &q.sustained_speed_label {
            quality_lines.push(Line::from(vec![
                Span::styled("Sustained:    ", theme.style_muted()),
                Span::styled(speed, theme.style_success()),
            ]));
        }
    } else {
        quality_lines.push(Line::from(Span::styled("No probe data for node", theme.style_muted())));
    }
    frame.render_widget(Paragraph::new(quality_lines), quality_inner);

    // --- Panel 2: Active Connections ---
    let conn_title = format!(" ACTIVE CONNECTIONS · {} ", snapshot.active_connections.len());
    let conn_block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.style_muted())
        .title(conn_title)
        .style(theme.style_base());

    let conn_inner = conn_block.inner(conn_area);
    frame.render_widget(conn_block, conn_area);

    let items: Vec<ListItem> = snapshot
        .active_connections
        .iter()
        .take(conn_inner.height.saturating_sub(1) as usize)
        .map(|conn| {
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{:<15}", conn.destination),
                    theme.style_base(),
                ),
                Span::styled(
                    format!(" {:>7}", conn.rate_label),
                    theme.style_muted(),
                ),
                Span::styled(
                    format!(" {}", conn.rule),
                    theme.style_warning(),
                ),
            ]))
        })
        .collect();

    frame.render_widget(List::new(items), conn_inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    #[test]
    fn test_render_idle_dashboard_standard_120x30() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        let traffic = vec![
            TrafficSample {
                recorded_at_ms: now_unix_ms() - 100_000,
                down_bytes_per_sec: 2_000_000,
                up_bytes_per_sec: 1_000_000,
            },
            TrafficSample {
                recorded_at_ms: now_unix_ms(),
                down_bytes_per_sec: 3_900_000,
                up_bytes_per_sec: 2_100_000,
            },
        ];

        let latency = vec![
            LatencySample {
                recorded_at_ms: now_unix_ms() - 100_000,
                selector: "Proxy".to_string(),
                node_name: "JP-Edge-03".to_string(),
                latency_ms: 28,
            },
            LatencySample {
                recorded_at_ms: now_unix_ms(),
                selector: "Proxy".to_string(),
                node_name: "JP-Edge-03".to_string(),
                latency_ms: 26,
            },
        ];

        let intervals = vec![
            RouteInterval {
                id: 1,
                selector: "Proxy".to_string(),
                node_name: "JP-Edge-03".to_string(),
                started_at_ms: now_unix_ms() - 200_000,
                ended_at_ms: None,
                interval_index: 0,
            },
        ];

        let snapshot = IdleDashboardSnapshot {
            active_provider: "AirTCP",
            active_node: "JP-Edge-03",
            status_text: "GLOBAL NET  STABLE",
            current_down_rate: "3.9M/s",
            current_up_rate: "2.1M/s",
            traffic_samples: &traffic,
            latency_samples: &latency,
            route_intervals: &intervals,
            node_quality: Some(ActiveNodeQualitySnapshot {
                node_name: "JP-Edge-03",
                current_latency_ms: Some(26),
                warm_median_ms: Some(28),
                p95_ms: Some(35),
                cold_start_ms: Some(42),
                sustained_speed_label: Some("12.4 MiB/s".to_string()),
                reachability_label: "Stable Reachable",
            }),
            active_connections: vec![
                ActiveConnectionSummary {
                    destination: "chat.openai.com",
                    rate_label: "1.2M/s".to_string(),
                    rule: "Proxy",
                },
            ],
        };

        terminal.draw(|f| render_idle_dashboard(f, &snapshot)).unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer.area.width, 120);
        assert_eq!(buffer.area.height, 30);
    }

    #[test]
    fn test_render_idle_dashboard_compact_80x24() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        let snapshot = IdleDashboardSnapshot {
            active_provider: "AirTCP",
            active_node: "JP-Edge-03",
            status_text: "GLOBAL NET  STABLE",
            current_down_rate: "3.9M/s",
            current_up_rate: "2.1M/s",
            traffic_samples: &[],
            latency_samples: &[],
            route_intervals: &[],
            node_quality: None,
            active_connections: vec![],
        };

        terminal.draw(|f| render_idle_dashboard(f, &snapshot)).unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer.area.width, 80);
        assert_eq!(buffer.area.height, 24);
    }

    #[test]
    fn test_render_idle_dashboard_unsupported_guard() {
        let backend = TestBackend::new(70, 20);
        let mut terminal = Terminal::new(backend).unwrap();

        let snapshot = IdleDashboardSnapshot {
            active_provider: "AirTCP",
            active_node: "JP-Edge-03",
            status_text: "GLOBAL NET  STABLE",
            current_down_rate: "0.0M/s",
            current_up_rate: "0.0M/s",
            traffic_samples: &[],
            latency_samples: &[],
            route_intervals: &[],
            node_quality: None,
            active_connections: vec![],
        };

        terminal.draw(|f| render_idle_dashboard(f, &snapshot)).unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer.area.width, 70);
        assert_eq!(buffer.area.height, 20);
    }
}

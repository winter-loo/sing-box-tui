use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::canvas::{Canvas, Line as CanvasLine, Points};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::tui::ds::{Theme, ViewportMode, render_breadcrumb, render_footer, render_unsupported_guard};
use crate::tui::metrics::{
    LatencySample, METRIC_RETENTION_WINDOW_MS, MetricStore, RouteInterval, TrafficSample,
    now_unix_ms,
};

#[derive(Clone, Debug)]
pub(crate) struct ActiveNodeQualitySnapshot<'a> {
    pub(crate) node_name: &'a str,
    pub(crate) current_latency_ms: Option<u64>,
    #[allow(dead_code)]
    pub(crate) warm_median_ms: Option<u64>,
    #[allow(dead_code)]
    pub(crate) p95_ms: Option<u64>,
    #[allow(dead_code)]
    pub(crate) cold_start_ms: Option<u64>,
    pub(crate) sustained_speed_label: Option<String>,
    #[allow(dead_code)]
    pub(crate) reachability_label: &'a str,
    pub(crate) latency_points: Vec<(f64, f64)>,
    pub(crate) sustained_points: Vec<(f64, f64)>,
    pub(crate) latest_latency: Option<String>,
    pub(crate) latency_sample_age: Option<String>,
    pub(crate) latest_sustained_speed: Option<String>,
    pub(crate) sustained_sample_age: Option<String>,
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

impl<'a> IdleDashboardSnapshot<'a> {
    #[allow(dead_code)]
    pub(crate) fn latency_history_points(&self) -> &[(f64, f64)] {
        self.node_quality.as_ref().map_or(&[], |q| &q.latency_points)
    }

    #[allow(dead_code)]
    pub(crate) fn sustained_speed_history_points(&self) -> &[(f64, f64)] {
        self.node_quality.as_ref().map_or(&[], |q| &q.sustained_points)
    }

    #[allow(dead_code)]
    pub(crate) fn latest_latency(&self) -> Option<&str> {
        self.node_quality.as_ref().and_then(|q| q.latest_latency.as_deref())
    }

    #[allow(dead_code)]
    pub(crate) fn latency_sample_age(&self) -> Option<&str> {
        self.node_quality.as_ref().and_then(|q| q.latency_sample_age.as_deref())
    }

    #[allow(dead_code)]
    pub(crate) fn latest_sustained_speed(&self) -> Option<&str> {
        self.node_quality.as_ref().and_then(|q| q.latest_sustained_speed.as_deref())
    }

    #[allow(dead_code)]
    pub(crate) fn sustained_sample_age(&self) -> Option<&str> {
        self.node_quality.as_ref().and_then(|q| q.sustained_sample_age.as_deref())
    }
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

    // 2. Main content panels (Responsive breakpoints: >=120 all, 96..119 quality only, 80..95 full charts)
    if area.width >= 120 && area.height >= 30 {
        let right_width = 38.min(main_area.width.saturating_sub(60));
        let [charts_area, side_area] = Layout::horizontal([
            Constraint::Min(60),
            Constraint::Length(right_width),
        ])
        .areas(main_area);

        render_core_history_panel(frame, charts_area, &theme, snapshot);
        render_side_panels(frame, side_area, &theme, snapshot);
    } else if area.width >= 96 {
        let right_width = 30.min(main_area.width.saturating_sub(60));
        let [charts_area, side_area] = Layout::horizontal([
            Constraint::Min(60),
            Constraint::Length(right_width),
        ])
        .areas(main_area);

        render_core_history_panel(frame, charts_area, &theme, snapshot);
        let [quality_area, _] = Layout::vertical([
            Constraint::Length(12),
            Constraint::Min(0),
        ])
        .areas(side_area);
        render_node_quality_panel(frame, quality_area, &theme, snapshot);
    } else {
        render_core_history_panel(frame, main_area, &theme, snapshot);
    }

    // 3. Footer
    let shortcuts = if area.width >= 120 && area.height >= 30 {
        [
            ("Ctrl+K", "menu"),
            ("c", "connections"),
            ("i", "quality"),
            ("o", "settings"),
            ("?", "help"),
        ]
    } else {
        [
            ("Ctrl+K", "menu"),
            ("c", "conn"),
            ("i", "quality"),
            ("o", "settings"),
            ("?", "help"),
        ]
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

const PINK: Color = Color::Rgb(229, 137, 245);
const GREEN: Color = Color::Rgb(98, 230, 167);

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

    render_node_quality_panel(frame, quality_area, theme, snapshot);
    render_active_connections_panel(frame, conn_area, theme, snapshot);
}

/// Renders the Node Quality panel with 3-row mini Braille sparklines
fn render_node_quality_panel(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    snapshot: &IdleDashboardSnapshot<'_>,
) {
    let quality_title = format!(" NODE QUALITY · {} ", snapshot.active_node);
    let quality_block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.style_muted())
        .title(quality_title)
        .style(theme.style_base());

    let inner = quality_block.inner(area);
    frame.render_widget(quality_block, area);

    if inner.height < 10 || inner.width < 10 {
        if let Some(q) = &snapshot.node_quality {
            let line = Line::from(Span::styled(format!("Node: {}", q.node_name), theme.style_muted()));
            frame.render_widget(Paragraph::new(line), inner);
        }
        return;
    }

    let Some(q) = &snapshot.node_quality else {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled("No probe data for node", theme.style_muted()))),
            inner,
        );
        return;
    };

    // Row 0: Latency header (pink value + muted sample age)
    let latency_raw = q
        .latest_latency
        .as_deref()
        .map(|s| s.to_string())
        .or_else(|| q.current_latency_ms.map(|v| format!("{v} ms")))
        .unwrap_or_else(|| "--".to_string());
    let latency_label = if latency_raw.starts_with("延迟") || latency_raw.starts_with("Latency") {
        latency_raw
    } else {
        format!("延迟 {}", latency_raw)
    };
    let latency_age = q.latency_sample_age.as_deref().unwrap_or("");
    let pad_latency = inner.width.saturating_sub(
        UnicodeWidthStr::width(latency_label.as_str()) as u16 + UnicodeWidthStr::width(latency_age) as u16,
    );
    let latency_header = Line::from(vec![
        Span::styled(latency_label, Style::default().fg(PINK)),
        Span::raw(" ".repeat(pad_latency as usize)),
        Span::styled(latency_age, theme.style_muted()),
    ]);
    frame.render_widget(
        Paragraph::new(latency_header),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );

    // Rows 1..=3: Latency 3-row mini Braille sparkline (range 0..120 ms) with bounds labels "0" and "120ms"
    let bounds_w = 6.min(inner.width.saturating_sub(4));
    let latency_bounds = vec![
        Line::from(Span::styled("120ms", theme.style_muted())),
        Line::from(""),
        Line::from(Span::styled("0", theme.style_muted())),
    ];
    frame.render_widget(
        Paragraph::new(latency_bounds),
        Rect::new(inner.x, inner.y + 1, bounds_w, 3),
    );

    let plot_w = inner.width.saturating_sub(bounds_w);
    let points = &q.latency_points;
    let canvas_latency = Canvas::default()
        .marker(Marker::Braille)
        .x_bounds([0.0, 30.0])
        .y_bounds([0.0, 120.0])
        .paint(move |ctx| {
            for (i, &(x, y)) in points.iter().enumerate() {
                let clamped_x = x.clamp(0.0, 30.0);
                let clamped_y = y.clamp(0.0, 120.0);
                ctx.draw(&Points {
                    coords: &[(clamped_x, clamped_y)],
                    color: PINK,
                });
                if i > 0 {
                    let (prev_x, prev_y) = points[i - 1];
                    // Follow ADR 0003 & docs: sparse samples remain gaps without interpolation.
                    // Only connect if samples are contiguous (<= 1.0 min apart).
                    if (clamped_x - prev_x).abs() <= 1.0 {
                        ctx.draw(&CanvasLine {
                            x1: prev_x.clamp(0.0, 30.0),
                            y1: prev_y.clamp(0.0, 120.0),
                            x2: clamped_x,
                            y2: clamped_y,
                            color: PINK,
                        });
                    }
                }
            }
        });
    frame.render_widget(
        canvas_latency,
        Rect::new(inner.x + bounds_w, inner.y + 1, plot_w, 3),
    );

    // Row 4: Sustained speed header (green value + muted sample age)
    let speed_raw = q
        .latest_sustained_speed
        .as_deref()
        .map(|s| s.to_string())
        .or_else(|| q.sustained_speed_label.clone())
        .unwrap_or_else(|| "--".to_string());
    let speed_label = if speed_raw.starts_with("实测") || speed_raw.starts_with("Speed") {
        speed_raw
    } else {
        format!("实测 {}", speed_raw)
    };
    let speed_age = q.sustained_sample_age.as_deref().unwrap_or("");
    let pad_speed = inner.width.saturating_sub(
        UnicodeWidthStr::width(speed_label.as_str()) as u16 + UnicodeWidthStr::width(speed_age) as u16,
    );
    let speed_header = Line::from(vec![
        Span::styled(speed_label, Style::default().fg(GREEN)),
        Span::raw(" ".repeat(pad_speed as usize)),
        Span::styled(speed_age, theme.style_muted()),
    ]);
    frame.render_widget(
        Paragraph::new(speed_header),
        Rect::new(inner.x, inner.y + 4, inner.width, 1),
    );

    // Rows 5..=7: Sustained speed 3-row mini Braille sparkline (range 0..10 MiB/s) with bounds labels "0" and "10M"
    let speed_bounds = vec![
        Line::from(Span::styled("10M", theme.style_muted())),
        Line::from(""),
        Line::from(Span::styled("0", theme.style_muted())),
    ];
    frame.render_widget(
        Paragraph::new(speed_bounds),
        Rect::new(inner.x, inner.y + 5, bounds_w, 3),
    );

    let s_points = &q.sustained_points;
    let canvas_speed = Canvas::default()
        .marker(Marker::Braille)
        .x_bounds([0.0, 30.0])
        .y_bounds([0.0, 10.0])
        .paint(move |ctx| {
            for (i, &(x, y)) in s_points.iter().enumerate() {
                let clamped_x = x.clamp(0.0, 30.0);
                let clamped_y = y.clamp(0.0, 10.0);
                ctx.draw(&Points {
                    coords: &[(clamped_x, clamped_y)],
                    color: GREEN,
                });
                if i > 0 {
                    let (prev_x, prev_y) = s_points[i - 1];
                    if (clamped_x - prev_x).abs() <= 1.0 {
                        ctx.draw(&CanvasLine {
                            x1: prev_x.clamp(0.0, 30.0),
                            y1: prev_y.clamp(0.0, 10.0),
                            x2: clamped_x,
                            y2: clamped_y,
                            color: GREEN,
                        });
                    }
                }
            }
        });
    frame.render_widget(
        canvas_speed,
        Rect::new(inner.x + bounds_w, inner.y + 5, plot_w, 3),
    );

    // Row 8: Common time axis (-30m ... 现在)
    let right_label = "现在";
    let pad_axis = plot_w.saturating_sub(
        UnicodeWidthStr::width("-30m") as u16 + UnicodeWidthStr::width(right_label) as u16,
    );
    let axis_line = Line::from(vec![
        Span::raw(" ".repeat(bounds_w as usize)),
        Span::styled("-30m", theme.style_muted()),
        Span::raw(" ".repeat(pad_axis as usize)),
        Span::styled(right_label, theme.style_muted()),
    ]);
    frame.render_widget(
        Paragraph::new(axis_line),
        Rect::new(inner.x, inner.y + 8, inner.width, 1),
    );

    // Row 9: Status note
    let note_line = Line::from(Span::styled("仅显示已有采样", theme.style_muted()));
    frame.render_widget(
        Paragraph::new(note_line),
        Rect::new(inner.x, inner.y + 9, inner.width, 1),
    );
}

/// Renders the Active Connections panel
fn render_active_connections_panel(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    snapshot: &IdleDashboardSnapshot<'_>,
) {
    let conn_title = format!(" ACTIVE CONNECTIONS · {} ", snapshot.active_connections.len());
    let conn_block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.style_muted())
        .title(conn_title)
        .style(theme.style_base());

    let conn_inner = conn_block.inner(area);
    frame.render_widget(conn_block, area);

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


    fn buffer_to_text(b: &ratatui::buffer::Buffer) -> String {
        let mut out = String::new();
        for y in 0..b.area.height {
            let mut x = 0;
            while x < b.area.width {
                let s = b[(x, y)].symbol();
                out.push_str(s);
                x += unicode_width::UnicodeWidthStr::width(s).max(1) as u16;
            }
            out.push('\n');
        }
        out
    }

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
                current_latency_ms: Some(28),
                warm_median_ms: Some(28),
                p95_ms: Some(35),
                cold_start_ms: Some(42),
                sustained_speed_label: Some("8.0 MiB/s".to_string()),
                reachability_label: "Stable Reachable",
                latency_points: vec![
                    (2.0, 45.0),
                    (9.0, 55.0),
                    (21.0, 35.0),
                    (29.0, 50.0),
                    (30.0, 28.0),
                ],
                sustained_points: vec![(3.0, 7.0), (13.0, 5.0), (28.0, 8.0)],
                latest_latency: Some("28 ms".to_string()),
                latency_sample_age: Some("刚测".to_string()),
                latest_sustained_speed: Some("8.0 MiB/s".to_string()),
                sustained_sample_age: Some("2分钟前".to_string()),
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

        let t = buffer_to_text(buffer);
        assert!(t.contains("CORE HISTORY · 30 MIN"));
        assert!(t.contains("NODE QUALITY · JP-Edge-03"));
        assert!(t.contains("ACTIVE CONNECTIONS · 1"));
        assert!(t.contains("chat.openai.com"));
        assert!(t.contains("120ms"));
        assert!(t.contains("10M"));
        assert!(t.contains("28 ms"));
        assert!(t.contains("8.0 MiB/s"));
        assert!(t.contains("刚测"));
        assert!(t.contains("2分钟前"));
        assert!(t.contains("仅显示已有采样"));
    }

    #[test]
    fn test_render_idle_dashboard_breakpoint_96x30() {
        let backend = TestBackend::new(96, 30);
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
            node_quality: Some(ActiveNodeQualitySnapshot {
                node_name: "JP-Edge-03",
                current_latency_ms: Some(28),
                warm_median_ms: Some(28),
                p95_ms: Some(35),
                cold_start_ms: Some(42),
                sustained_speed_label: Some("8.0 MiB/s".to_string()),
                reachability_label: "Stable Reachable",
                latency_points: vec![
                    (2.0, 45.0),
                    (9.0, 55.0),
                    (21.0, 35.0),
                    (29.0, 50.0),
                    (30.0, 28.0),
                ],
                sustained_points: vec![(3.0, 7.0), (13.0, 5.0), (28.0, 8.0)],
                latest_latency: Some("28 ms".to_string()),
                latency_sample_age: Some("刚测".to_string()),
                latest_sustained_speed: Some("8.0 MiB/s".to_string()),
                sustained_sample_age: Some("2分钟前".to_string()),
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
        assert_eq!(buffer.area.width, 96);
        assert_eq!(buffer.area.height, 30);

        let t = buffer_to_text(buffer);
        // Core history is present
        assert!(t.contains("CORE HISTORY · 30 MIN"));
        // Node quality is present (30 cols)
        assert!(t.contains("NODE QUALITY · JP-Edge-03"));
        assert!(t.contains("120ms"));
        assert!(t.contains("10M"));
        assert!(t.contains("28 ms"));
        assert!(t.contains("8.0 MiB/s"));
        // Connections table is omitted
        assert!(!t.contains("ACTIVE CONNECTIONS"));
        assert!(!t.contains("chat.openai.com"));

        // Helper methods on snapshot verify exposure
        assert_eq!(snapshot.latency_history_points().len(), 5);
        assert_eq!(snapshot.sustained_speed_history_points().len(), 3);
        assert_eq!(snapshot.latest_latency(), Some("28 ms"));
        assert_eq!(snapshot.latency_sample_age(), Some("刚测"));
        assert_eq!(snapshot.latest_sustained_speed(), Some("8.0 MiB/s"));
        assert_eq!(snapshot.sustained_sample_age(), Some("2分钟前"));
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
            node_quality: Some(ActiveNodeQualitySnapshot {
                node_name: "JP-Edge-03",
                current_latency_ms: Some(28),
                warm_median_ms: Some(28),
                p95_ms: Some(35),
                cold_start_ms: Some(42),
                sustained_speed_label: Some("8.0 MiB/s".to_string()),
                reachability_label: "Stable Reachable",
                latency_points: vec![(2.0, 45.0), (30.0, 28.0)],
                sustained_points: vec![(28.0, 8.0)],
                latest_latency: Some("28 ms".to_string()),
                latency_sample_age: Some("刚测".to_string()),
                latest_sustained_speed: Some("8.0 MiB/s".to_string()),
                sustained_sample_age: Some("2分钟前".to_string()),
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
        assert_eq!(buffer.area.width, 80);
        assert_eq!(buffer.area.height, 24);

        let t = buffer_to_text(buffer);
        // Aggregate charts are present
        assert!(t.contains("CORE HISTORY · 30 MIN"));
        // Both node quality AND active connections are omitted
        assert!(!t.contains("NODE QUALITY"));
        assert!(!t.contains("ACTIVE CONNECTIONS"));
        assert!(!t.contains("chat.openai.com"));
        assert!(!t.contains("8.0 MiB/s"));
    }

    #[test]
    fn test_render_idle_dashboard_unsupported_guard() {
        for (w, h) in [(70, 20), (79, 24), (80, 23)] {
            let backend = TestBackend::new(w, h);
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
            assert_eq!(buffer.area.width, w);
            assert_eq!(buffer.area.height, h);

            let t = buffer_to_text(buffer);
            assert!(t.contains("Resize terminal to at least") || t.contains("80x24"));
            assert!(t.contains("q") && t.contains("?"));
        }
    }
}

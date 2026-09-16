use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Axis, Block, Borders, Chart, Dataset, GraphType, Paragraph};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::tui::ds::{Theme, render_breadcrumb, render_unsupported_guard};
use crate::tui::metrics::{
    LatencySample, METRIC_RETENTION_WINDOW_MS, MetricStore, RouteInterval, TrafficSample,
    now_unix_ms,
};

#[derive(Clone, Debug)]
pub(crate) struct ActiveNodeQualitySnapshot<'a> {
    #[allow(dead_code)]
    pub(crate) node_name: &'a str,
    pub(crate) current_latency_ms: Option<u64>,
    #[allow(dead_code)]
    pub(crate) warm_median_ms: Option<u64>,
    #[allow(dead_code)]
    pub(crate) p95_ms: Option<u64>,
    #[allow(dead_code)]
    pub(crate) cold_start_ms: Option<u64>,
    #[allow(dead_code)]
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
    #[allow(dead_code)]
    pub(crate) rule: &'a str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GlobalNetworkStatus {
    Stable,
    Idle,
}

impl GlobalNetworkStatus {
    fn label(self) -> &'static str {
        match self {
            Self::Stable => "STABLE",
            Self::Idle => "IDLE",
        }
    }

    fn color(self, theme: &Theme) -> Color {
        match self {
            Self::Stable => theme.text_status_active(),
            Self::Idle => theme.text_muted(),
        }
    }
}

pub(crate) struct IdleDashboardSnapshot<'a> {
    pub(crate) active_provider: &'a str,
    pub(crate) active_node: &'a str,
    pub(crate) network_status: GlobalNetworkStatus,
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
        self.node_quality
            .as_ref()
            .map_or(&[], |q| &q.latency_points)
    }

    #[allow(dead_code)]
    pub(crate) fn sustained_speed_history_points(&self) -> &[(f64, f64)] {
        self.node_quality
            .as_ref()
            .map_or(&[], |q| &q.sustained_points)
    }

    #[allow(dead_code)]
    pub(crate) fn latest_latency(&self) -> Option<&str> {
        self.node_quality
            .as_ref()
            .and_then(|q| q.latest_latency.as_deref())
    }

    #[allow(dead_code)]
    pub(crate) fn latency_sample_age(&self) -> Option<&str> {
        self.node_quality
            .as_ref()
            .and_then(|q| q.latency_sample_age.as_deref())
    }

    #[allow(dead_code)]
    pub(crate) fn latest_sustained_speed(&self) -> Option<&str> {
        self.node_quality
            .as_ref()
            .and_then(|q| q.latest_sustained_speed.as_deref())
    }

    #[allow(dead_code)]
    pub(crate) fn sustained_sample_age(&self) -> Option<&str> {
        self.node_quality
            .as_ref()
            .and_then(|q| q.sustained_sample_age.as_deref())
    }
}

fn fit(s: &str, width: u16) -> String {
    if s.width() <= usize::from(width) {
        return s.into();
    }
    let budget = usize::from(width).saturating_sub(3);
    let mut out = String::new();
    for g in s.graphemes(true) {
        if out.width() + g.width() > budget {
            break;
        }
        out.push_str(g);
    }
    out.push_str(&"..."[..usize::from(width).min(3)]);
    out
}

fn label(f: &mut Frame, x: u16, y: u16, w: u16, s: &str, color: Color) {
    if w == 0 {
        return;
    }
    f.render_widget(
        Paragraph::new(fit(s, w)).style(Style::default().fg(color)),
        Rect::new(x, y, w, 1),
    );
}

fn panel(f: &mut Frame, r: Rect, title: &str, theme: &Theme) {
    if r.width == 0 || r.height == 0 {
        return;
    }
    surface(f, r, theme);
    f.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.border_default()))
            .title(title),
        r,
    );
}

fn surface(f: &mut Frame, r: Rect, theme: &Theme) {
    if r.width == 0 || r.height == 0 {
        return;
    }
    f.render_widget(
        Block::default().style(
            Style::default()
                .bg(theme.bg_surface())
                .fg(theme.text_primary()),
        ),
        r,
    );
}

fn plot(
    f: &mut Frame,
    r: Rect,
    points: &[Vec<(f64, f64)>],
    colors: &[Color],
    kinds: &[GraphType],
    max: f64,
) {
    if r.width == 0 || r.height == 0 || points.is_empty() {
        return;
    }
    let sets = points
        .iter()
        .enumerate()
        .map(|(i, p)| {
            Dataset::default()
                .data(p)
                .graph_type(kinds[i])
                .marker(if kinds[i] == GraphType::Scatter {
                    Marker::Dot
                } else {
                    Marker::Braille
                })
                .style(Style::default().fg(colors[i]))
        })
        .collect::<Vec<_>>();
    f.render_widget(
        Chart::new(sets)
            .x_axis(Axis::default().bounds([0., 30.]))
            .y_axis(Axis::default().bounds([0., max]))
            .legend_position(None),
        r,
    );
}

const SPARKLINE_OBSERVATION_GAP_MINUTES: f64 = 2.5;

fn segment_sparkline_series(
    points: &[(f64, f64)],
    gap_minutes: f64,
) -> Vec<(Vec<(f64, f64)>, GraphType)> {
    let mut segments = Vec::new();
    let mut cur_seg: Vec<(f64, f64)> = Vec::new();

    for &pt in points {
        if let Some(&last_pt) = cur_seg.last() {
            if (pt.0 - last_pt.0).abs() > gap_minutes {
                let kind = if cur_seg.len() == 1 {
                    GraphType::Scatter
                } else {
                    GraphType::Line
                };
                segments.push((std::mem::take(&mut cur_seg), kind));
            }
        }
        cur_seg.push(pt);
    }
    if !cur_seg.is_empty() {
        let kind = if cur_seg.len() == 1 {
            GraphType::Scatter
        } else {
            GraphType::Line
        };
        segments.push((cur_seg, kind));
    }
    segments
}

fn plot_braille_sparklines(f: &mut Frame, r: Rect, points: &[(f64, f64)], color: Color, max: f64) {
    if r.width == 0 || r.height == 0 || points.is_empty() {
        return;
    }
    let segments = segment_sparkline_series(points, SPARKLINE_OBSERVATION_GAP_MINUTES);
    let sets = segments
        .iter()
        .map(|(seg, kind)| {
            Dataset::default()
                .data(seg)
                .graph_type(*kind)
                .marker(Marker::Braille)
                .style(Style::default().fg(color))
        })
        .collect::<Vec<_>>();

    f.render_widget(
        Chart::new(sets)
            .x_axis(Axis::default().bounds([0., 30.]))
            .y_axis(Axis::default().bounds([0., max]))
            .legend_position(None),
        r,
    );
}

fn render_node_panel(f: &mut Frame, r: Rect, snapshot: &IdleDashboardSnapshot<'_>, theme: &Theme) {
    surface(f, r, theme);
    let x = r.x + 1;
    let w = r.width.saturating_sub(2);
    let y = r.y;

    label(
        f,
        x,
        y,
        w,
        &format!("节点质量 · {}", snapshot.active_node),
        theme.text_secondary(),
    );

    let Some(q) = &snapshot.node_quality else {
        label(f, x, y + 2, w, "无节点数据", theme.text_muted());
        return;
    };

    let latency_val = q
        .latest_latency
        .clone()
        .or_else(|| q.current_latency_ms.map(|ms| format!("{ms} ms")))
        .unwrap_or_else(|| "—".to_string());
    let latency_title = if latency_val.starts_with("延迟") {
        latency_val
    } else {
        format!("延迟 {latency_val}")
    };
    let latency_age = q.latency_sample_age.as_deref().unwrap_or("—");
    label(
        f,
        x,
        y + 2,
        w.saturating_sub(7),
        &latency_title,
        theme.text_latency(),
    );
    label(
        f,
        x + w.saturating_sub(7),
        y + 2,
        7,
        latency_age,
        theme.text_muted(),
    );
    label(f, x, y + 3, 4, "120", theme.text_muted());
    label(f, x, y + 6, 4, "0", theme.text_muted());

    plot_braille_sparklines(
        f,
        Rect::new(x + 5, y + 3, w.saturating_sub(5), 4),
        &q.latency_points,
        theme.text_latency(),
        120.,
    );
    label(f, x + 5, y + 7, 4, "-30m", theme.text_muted());
    label(
        f,
        x + w.saturating_sub(4),
        y + 7,
        4,
        "现在",
        theme.text_muted(),
    );

    let speed_raw = q
        .latest_sustained_speed
        .as_deref()
        .or(q.sustained_speed_label.as_deref())
        .unwrap_or("—");
    let speed_title = if speed_raw.starts_with("实测") {
        speed_raw.to_string()
    } else {
        format!("实测 {speed_raw}")
    };
    let speed_age = q.sustained_sample_age.as_deref().unwrap_or("—");
    label(
        f,
        x,
        y + 9,
        w.saturating_sub(7),
        &speed_title,
        theme.text_success(),
    );
    label(
        f,
        x + w.saturating_sub(7),
        y + 9,
        7,
        speed_age,
        theme.text_muted(),
    );
    label(f, x, y + 10, 4, "10", theme.text_muted());
    label(f, x, y + 13, 4, "0", theme.text_muted());

    plot_braille_sparklines(
        f,
        Rect::new(x + 5, y + 10, w.saturating_sub(5), 4),
        &q.sustained_points,
        theme.text_success(),
        10.,
    );

    label(f, x + 5, y + 14, 4, "-30m", theme.text_muted());
    label(
        f,
        x + w.saturating_sub(4),
        y + 14,
        4,
        "现在",
        theme.text_muted(),
    );
}

fn render_connections_panel(
    f: &mut Frame,
    r: Rect,
    snapshot: &IdleDashboardSnapshot<'_>,
    theme: &Theme,
) {
    let title = format!(" 活动连接 · {} ", snapshot.active_connections.len());
    panel(f, r, &title, theme);
    let x = r.x + 1;
    let w = r.width.saturating_sub(2);
    label(
        f,
        x,
        r.y + 1,
        w.saturating_sub(6),
        "目标",
        theme.text_muted(),
    );
    label(
        f,
        x + w.saturating_sub(6),
        r.y + 1,
        6,
        "MiB/s",
        theme.text_muted(),
    );

    let max_rows = usize::from(r.height.saturating_sub(4));
    let mut shown = 0;
    for (i, conn) in snapshot
        .active_connections
        .iter()
        .take(max_rows)
        .enumerate()
    {
        shown += 1;
        label(
            f,
            x,
            r.y + 2 + i as u16,
            w.saturating_sub(7),
            conn.destination,
            theme.text_primary(),
        );
        label(
            f,
            x + w.saturating_sub(6),
            r.y + 2 + i as u16,
            6,
            &conn.rate_label,
            theme.text_primary(),
        );
    }
    let remaining = snapshot.active_connections.len().saturating_sub(shown);
    if remaining > 0 && r.height >= 5 {
        label(
            f,
            x,
            r.y + 2 + shown as u16,
            w,
            &format!("另 {remaining} 条"),
            theme.text_muted(),
        );
    }
}

fn render_aggregate_panel(
    f: &mut Frame,
    r: Rect,
    snapshot: &IdleDashboardSnapshot<'_>,
    theme: &Theme,
) {
    surface(f, r, theme);
    let x = r.x + 2;
    let w = r.width.saturating_sub(4);
    let top = r.y;
    if r.height < 20 || w < 12 {
        return;
    }
    label(f, x, top, w, "核心历史 · 30 分钟", theme.text_secondary());

    let plot_height = (r.height - 12) / 2;
    let plot_x = x + 4;
    let plot_width = w.saturating_sub(4);
    let latency_title_y = top + 4;
    let latency_plot_y = latency_title_y + 1;
    let latency_axis_y = latency_plot_y + plot_height;
    let traffic_title_y = latency_axis_y + 3;
    let traffic_plot_y = traffic_title_y + 1;
    let traffic_axis_y = traffic_plot_y + plot_height;

    let now_ms = now_unix_ms();
    let cutoff_ms = now_ms.saturating_sub(METRIC_RETENTION_WINDOW_MS);

    // Route interval headers
    if !snapshot.route_intervals.is_empty() {
        for interval in snapshot.route_intervals {
            let start_ms = interval.started_at_ms.max(cutoff_ms);
            let minute = ((start_ms - cutoff_ms) as f64 / 60_000.0).clamp(0.0, 30.0);
            let color = theme.route_color(interval.interval_index);
            let bx = plot_x
                + ((f64::from(plot_width.saturating_sub(1)) * (minute / 30.0)).round()
                    as u16);
            let rem_w = (plot_x + plot_width).saturating_sub(bx).min(14);
            if rem_w > 0 {
                label(f, bx, top + 2, rem_w, &interval.node_name, color);
            }
        }
    } else {
        label(
            f,
            plot_x,
            top + 2,
            plot_width.min(14),
            snapshot.active_node,
            theme.route_color(0),
        );
    }

    label(f, x, latency_title_y, w, "延迟 · ms", theme.text_muted());
    let latest_latency = snapshot
        .latency_samples
        .last()
        .map(|sample| sample.latency_ms)
        .or_else(|| snapshot.node_quality.as_ref()?.current_latency_ms);
    if let Some(latency_ms) = latest_latency {
        let value = latency_ms.to_string();
        label(
            f,
            x + w.saturating_sub(value.width() as u16),
            latency_title_y,
            value.width() as u16,
            &value,
            theme.text_muted(),
        );
    }
    label(f, x, latency_plot_y, 3, "120", theme.text_muted());
    label(
        f,
        x + 2,
        latency_plot_y + plot_height - 1,
        1,
        "0",
        theme.text_muted(),
    );
    label(
        f,
        x,
        traffic_title_y,
        w,
        "核心流量 · MiB/s",
        theme.text_muted(),
    );
    label(
        f,
        x + w.saturating_sub(12),
        traffic_title_y,
        12,
        "↓ 线   ↑ 点",
        theme.text_muted(),
    );
    label(f, x + 1, traffic_plot_y, 2, "10", theme.text_muted());
    label(
        f,
        x + 2,
        traffic_plot_y + plot_height - 1,
        1,
        "0",
        theme.text_muted(),
    );

    // Group latency samples into continuous segments
    let mut latency_series: Vec<Vec<(f64, f64)>> = Vec::new();
    let mut latency_colors: Vec<Color> = Vec::new();
    let mut latency_kinds: Vec<GraphType> = Vec::new();

    if !snapshot.latency_samples.is_empty() {
        let mut cur_seg: Vec<(f64, f64)> = Vec::new();
        let mut cur_idx = 0;
        let mut last_ts = 0;

        for s in snapshot.latency_samples {
            if s.recorded_at_ms < cutoff_ms {
                continue;
            }
            let minute = ((s.recorded_at_ms - cutoff_ms) as f64 / 60_000.0).clamp(0.0, 30.0);
            let lat = (s.latency_ms as f64).clamp(0.0, 120.0);

            let mut idx = 0;
            for iv in snapshot.route_intervals {
                if s.recorded_at_ms >= iv.started_at_ms
                    && iv.ended_at_ms.map_or(true, |end| s.recorded_at_ms <= end)
                {
                    idx = iv.interval_index;
                    break;
                }
            }

            let gap = last_ts > 0 && MetricStore::has_latency_gap(last_ts, s.recorded_at_ms);
            let switched = last_ts > 0 && idx != cur_idx;

            if (gap || switched) && !cur_seg.is_empty() {
                latency_colors.push(theme.route_color(cur_idx));
                latency_kinds.push(if cur_seg.len() == 1 {
                    GraphType::Scatter
                } else {
                    GraphType::Line
                });
                latency_series.push(std::mem::take(&mut cur_seg));
            }

            cur_idx = idx;
            last_ts = s.recorded_at_ms;
            cur_seg.push((minute, lat));
        }

        if !cur_seg.is_empty() {
            latency_colors.push(theme.route_color(cur_idx));
            latency_kinds.push(if cur_seg.len() == 1 {
                GraphType::Scatter
            } else {
                GraphType::Line
            });
            latency_series.push(cur_seg);
        }
    }

    if !latency_series.is_empty() {
        plot(
            f,
            Rect::new(plot_x, latency_plot_y, plot_width, plot_height),
            &latency_series,
            &latency_colors,
            &latency_kinds,
            120.,
        );
    }

    // Group traffic samples into down (line) and up (scatter)
    let mut traffic_series: Vec<Vec<(f64, f64)>> = Vec::new();
    let mut traffic_colors: Vec<Color> = Vec::new();
    let mut traffic_kinds: Vec<GraphType> = Vec::new();

    if !snapshot.traffic_samples.is_empty() {
        let mut down_seg: Vec<(f64, f64)> = Vec::new();
        let mut up_seg: Vec<(f64, f64)> = Vec::new();
        let mut cur_idx = 0;
        let mut last_ts = 0;

        for s in snapshot.traffic_samples {
            if s.recorded_at_ms < cutoff_ms {
                continue;
            }
            let minute = ((s.recorded_at_ms - cutoff_ms) as f64 / 60_000.0).clamp(0.0, 30.0);
            let down_mib = (s.down_bytes_per_sec as f64 / (1024.0 * 1024.0)).clamp(0.0, 10.0);
            let up_mib = (s.up_bytes_per_sec as f64 / (1024.0 * 1024.0)).clamp(0.0, 10.0);

            let mut idx = 0;
            for iv in snapshot.route_intervals {
                if s.recorded_at_ms >= iv.started_at_ms
                    && iv.ended_at_ms.map_or(true, |end| s.recorded_at_ms <= end)
                {
                    idx = iv.interval_index;
                    break;
                }
            }

            let gap = last_ts > 0 && MetricStore::has_traffic_gap(last_ts, s.recorded_at_ms);
            let switched = last_ts > 0 && idx != cur_idx;

            if (gap || switched) && !down_seg.is_empty() {
                let col = theme.route_color(cur_idx);
                traffic_colors.push(col);
                traffic_kinds.push(if down_seg.len() == 1 {
                    GraphType::Scatter
                } else {
                    GraphType::Line
                });
                traffic_series.push(std::mem::take(&mut down_seg));

                traffic_colors.push(col);
                traffic_kinds.push(GraphType::Scatter);
                traffic_series.push(std::mem::take(&mut up_seg));
            }

            cur_idx = idx;
            last_ts = s.recorded_at_ms;
            down_seg.push((minute, down_mib));
            up_seg.push((minute, up_mib));
        }

        if !down_seg.is_empty() {
            let col = theme.route_color(cur_idx);
            traffic_colors.push(col);
            traffic_kinds.push(if down_seg.len() == 1 {
                GraphType::Scatter
            } else {
                GraphType::Line
            });
            traffic_series.push(down_seg);

            traffic_colors.push(col);
            traffic_kinds.push(GraphType::Scatter);
            traffic_series.push(up_seg);
        }
    }

    if !traffic_series.is_empty() {
        plot(
            f,
            Rect::new(plot_x, traffic_plot_y, plot_width, plot_height),
            &traffic_series,
            &traffic_colors,
            &traffic_kinds,
            10.,
        );
    }

    if plot_width > 0 {
        for axis in [latency_axis_y, traffic_axis_y] {
            label(
                f,
                plot_x,
                axis,
                plot_width,
                &"─".repeat(usize::from(plot_width)),
                theme.text_muted(),
            );
            for (fraction, text) in [(0., "-30m"), (0.5, "-15m"), (1., "现在")] {
                let offset =
                    ((f64::from(plot_width.saturating_sub(1)) * fraction).round() as u16)
                        .min(plot_width.saturating_sub(text.width() as u16));
                label(
                    f,
                    plot_x + offset,
                    axis + 1,
                    text.width() as u16,
                    text,
                    theme.text_muted(),
                );
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct IdleDashboardLayout {
    node: Option<Rect>,
    connections: Option<Rect>,
    aggregate: Rect,
}

impl IdleDashboardLayout {
    const HEADER_HEIGHT: u16 = 2;
    const FOOTER_HEIGHT: u16 = 2;
    const MAX_CONTENT_WIDTH: u16 = 144;
    const MAX_CONTENT_HEIGHT: u16 = 30;

    fn from_area(area: Rect) -> Self {
        let body_y = area.y + Self::HEADER_HEIGHT;
        let body_height = area
            .height
            .saturating_sub(Self::HEADER_HEIGHT + Self::FOOTER_HEIGHT);
        let content_width = area.width.saturating_sub(2).min(Self::MAX_CONTENT_WIDTH);
        let content_height = body_height.min(Self::MAX_CONTENT_HEIGHT);
        let content = Rect::new(
            area.x + (area.width.saturating_sub(content_width)) / 2,
            body_y + (body_height.saturating_sub(content_height)) / 2,
            content_width,
            content_height,
        );

        let full = content.width >= 118 && content.height >= 26;
        let with_node = content.width >= 94 && content.height >= 20;
        if !with_node {
            return Self {
                node: None,
                connections: None,
                aggregate: content,
            };
        }

        let left_width = if full {
            ((u32::from(content.width) * 37 + 59) / 118) as u16
        } else {
            30
        }
        .clamp(30, 42);
        let aggregate = Rect::new(
            content.x + left_width + 1,
            content.y,
            content.width.saturating_sub(left_width + 1),
            content.height,
        );
        let node = Some(Rect::new(content.x, content.y, left_width, 15));
        let connections = full.then(|| {
            Rect::new(
                content.x,
                content.y + 16,
                left_width,
                content.height.saturating_sub(16),
            )
        });

        Self {
            node,
            connections,
            aggregate,
        }
    }
}

/// Main entry point for rendering the Idle Dashboard from the available terminal-cell budget.
pub(crate) fn render_idle_dashboard(frame: &mut Frame, snapshot: &IdleDashboardSnapshot<'_>) {
    let r = frame.area();
    let theme = Theme::detect();
    frame.render_widget(Block::default().style(theme.style_base()), r);

    if r.width < 80 || r.height < 24 {
        render_unsupported_guard(frame, r, &theme);
        return;
    }

    let provider = if snapshot.active_provider.is_empty() {
        "—"
    } else {
        snapshot.active_provider
    };
    let node = if snapshot.active_node.is_empty() {
        "—"
    } else {
        snapshot.active_node
    };

    // Canonical Figma Breadcrumb navigation (node 830:4 / 1025:3):
    // Unbordered, 1 row (row 0: text), slash-delimited with optical padding
    render_breadcrumb(
        frame,
        Rect::new(0, 0, r.width, 1),
        &theme,
        &["DASHBOARD", provider, node],
    );
    label(
        frame,
        0,
        1,
        r.width,
        &"─".repeat(usize::from(r.width)),
        theme.border_default(),
    );

    let layout = IdleDashboardLayout::from_area(r);
    if let Some(node_area) = layout.node {
        render_node_panel(frame, node_area, snapshot, &theme);
    }
    if let Some(connections_area) = layout.connections {
        render_connections_panel(frame, connections_area, snapshot, &theme);
    }
    render_aggregate_panel(frame, layout.aggregate, snapshot, &theme);

    let footer_rule_y = r.height - 2;
    let footer_y = r.height - 1;
    label(
        frame,
        0,
        footer_rule_y,
        r.width,
        &"─".repeat(usize::from(r.width)),
        theme.border_default(),
    );

    let rates_str = format!(
        "↓{}  ↑{}",
        snapshot.current_down_rate, snapshot.current_up_rate
    );
    let status_str = format!("GLOBAL NET  {}  {}", snapshot.network_status.label(), rates_str);
    let status_width = status_str.width() as u16;
    let shortcut_candidates = [
        "Ctrl+K 导航   c 连接   i 节点   o 设置   ? 帮助   q 退出",
        "Ctrl+K  c 连接  i 节点  o 设置  ?  q",
        "Ctrl+K  c  i  o  ?  q",
    ];
    let shortcut_budget = r.width.saturating_sub(status_width + 3);
    let shortcuts = shortcut_candidates
        .into_iter()
        .find(|candidate| candidate.width() as u16 <= shortcut_budget)
        .unwrap_or("Ctrl+K  ?  q");
    let shortcuts_width = shortcuts.width() as u16;
    label(
        frame,
        1,
        footer_y,
        shortcuts_width,
        shortcuts,
        theme.text_accent(),
    );
    if r.width >= shortcuts_width + status_width + 3 {
        let mut status_spans = Vec::new();
        status_spans.push(Span::styled("GLOBAL NET  ", theme.style_muted()));
        status_spans.push(Span::styled(
            snapshot.network_status.label(),
            Style::default().fg(snapshot.network_status.color(&theme)),
        ));
        status_spans.push(Span::raw("  "));
        status_spans.push(Span::styled(
            format!("↓{}", snapshot.current_down_rate),
            Style::default().fg(theme.text_transfer()),
        ));
        status_spans.push(Span::raw("  "));
        status_spans.push(Span::styled(
            format!("↑{}", snapshot.current_up_rate),
            Style::default().fg(theme.text_transfer()),
        ));
        frame.render_widget(
            Paragraph::new(Line::from(status_spans)),
            Rect::new(
                r.width.saturating_sub(status_width + 1),
                footer_y,
                status_width,
                1,
            ),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

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
                recorded_at_ms: now_unix_ms() - 25 * 60_000,
                down_bytes_per_sec: 2_000_000,
                up_bytes_per_sec: 1_000_000,
            },
            TrafficSample {
                recorded_at_ms: now_unix_ms() - 5 * 60_000,
                down_bytes_per_sec: 3_900_000,
                up_bytes_per_sec: 2_100_000,
            },
        ];

        let latency = vec![
            LatencySample {
                recorded_at_ms: now_unix_ms() - 25 * 60_000,
                selector: "Proxy".to_string(),
                node_name: "JP-Edge-03".to_string(),
                latency_ms: 28,
            },
            LatencySample {
                recorded_at_ms: now_unix_ms() - 5 * 60_000,
                selector: "Proxy".to_string(),
                node_name: "JP-Edge-03".to_string(),
                latency_ms: 26,
            },
        ];

        let intervals = vec![RouteInterval {
            id: 1,
            selector: "Proxy".to_string(),
            node_name: "JP-Edge-03".to_string(),
            started_at_ms: now_unix_ms() - 200_000,
            ended_at_ms: None,
            interval_index: 0,
        }];

        let snapshot = IdleDashboardSnapshot {
            active_provider: "AirTCP",
            active_node: "JP-Edge-03",
            network_status: GlobalNetworkStatus::Stable,
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
            active_connections: vec![ActiveConnectionSummary {
                destination: "chat.openai.com",
                rate_label: "1.2M/s".to_string(),
                rule: "Proxy",
            }],
        };

        terminal
            .draw(|f| render_idle_dashboard(f, &snapshot))
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer.area.width, 120);
        assert_eq!(buffer.area.height, 30);

        let t = buffer_to_text(buffer);
        assert!(!t.contains("监控"));
        assert!(t.contains("DASHBOARD / AirTCP / JP-Edge-03"));
        assert!(t.contains("核心历史 · 30 分钟"));
        assert!(t.contains("节点质量"));
        assert!(t.contains("活动连接 · 1"));
        assert!(t.contains("chat.openai.com"));
        assert!(t.contains("120"));
        assert!(t.contains("10"));
        assert!(t.contains("28 ms"));
        assert!(t.contains("8.0 MiB/s"));
        assert!(t.contains("刚测"));
        assert!(t.contains("2分钟前"));
        assert!(t.contains("Ctrl+K 导航"));
        assert!(t.contains("c 连接"));
        assert!(t.contains("i 节点"));
        assert!(t.contains("o 设置"));
        assert!(t.contains("? 帮助"));
        assert!(t.contains("q 退出"));
        assert!(!t.contains("历史有缺测"));
        assert!(!t.contains("探测流量"));
        assert!(t.contains("GLOBAL NET  STABLE  ↓3.9M/s  ↑2.1M/s"));

        // Figma 1025:2 maps its 8x16 design grid to terminal cells: the body has
        // one-cell outer gutters, a 37-cell left rail, a one-cell panel gap,
        // and an 80-cell global-history surface.
        assert_eq!(buffer[(0, 0)].symbol(), " ");
        assert_eq!(buffer[(1, 0)].symbol(), "D");
        assert_eq!(buffer[(0, 2)].symbol(), " ");
        assert_ne!(buffer[(1, 2)].symbol(), "┌");
        assert_ne!(buffer[(39, 2)].symbol(), "┌");
        assert_eq!(buffer[(41, 2)].symbol(), "核");
        assert_eq!(buffer[(1, 18)].symbol(), "┌");
        assert_eq!(buffer[(38, 18)].symbol(), " ");
        assert_eq!(buffer[(119, 18)].symbol(), " ");

        // Verify that 3-row mini Braille sparklines are rendered in node quality panel
        let mut node_panel_has_braille = false;
        for y in 4..17 {
            for x in 7..37 {
                let s = buffer[(x, y)].symbol();
                if s.chars().any(|ch| ('\u{2800}'..='\u{28FF}').contains(&ch)) {
                    node_panel_has_braille = true;
                    break;
                }
            }
        }
        assert!(
            node_panel_has_braille,
            "Node quality panel must render mini Braille sparklines"
        );

        // Verify gap between sample at minute 2 and minute 9 remains blank without interpolation
        for y in 5..8 {
            for x in 11..15 {
                let s = buffer[(x, y)].symbol();
                assert_eq!(
                    s, " ",
                    "Expected blank gap without interpolated line at x={}, y={}",
                    x, y
                );
            }
        }

        // The two global samples are twenty minutes apart. Both charts must
        // retain the missing interval instead of drawing across it.
        for y in (7..14).chain(18..25) {
            for x in 75..91 {
                let symbol = buffer[(x, y)].symbol();
                assert!(
                    !symbol
                        .chars()
                        .any(|ch| ('\u{2800}'..='\u{28FF}').contains(&ch)),
                    "Expected a missing-data gap at x={x}, y={y}, got {symbol:?}"
                );
            }
        }
    }

    #[test]
    fn test_render_idle_dashboard_breakpoint_96x30() {
        let backend = TestBackend::new(96, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        let snapshot = IdleDashboardSnapshot {
            active_provider: "AirTCP",
            active_node: "JP-Edge-03",
            network_status: GlobalNetworkStatus::Stable,
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
            active_connections: vec![ActiveConnectionSummary {
                destination: "chat.openai.com",
                rate_label: "1.2M/s".to_string(),
                rule: "Proxy",
            }],
        };

        terminal
            .draw(|f| render_idle_dashboard(f, &snapshot))
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer.area.width, 96);
        assert_eq!(buffer.area.height, 30);

        let t = buffer_to_text(buffer);
        assert!(!t.contains("监控"));
        assert!(t.contains("DASHBOARD / AirTCP / JP-Edge-03"));
        assert!(t.contains("核心历史 · 30 分钟"));
        assert!(t.contains("节点质量"));
        assert!(t.contains("28 ms"));
        assert!(t.contains("8.0 MiB/s"));
        // Active connections table is omitted at width 96
        assert!(!t.contains("活动连接"));
        assert!(!t.contains("chat.openai.com"));
        assert!(t.contains("GLOBAL NET  STABLE  ↓3.9M/s  ↑2.1M/s"));

        // One-cell gutters and one-cell inter-panel gap remain at the
        // intermediate breakpoint; connections are hidden before quality.
        assert_eq!(buffer[(0, 0)].symbol(), " ");
        assert_eq!(buffer[(1, 0)].symbol(), "D");
        assert_eq!(buffer[(0, 2)].symbol(), " ");
        assert_ne!(buffer[(1, 2)].symbol(), "┌");
        assert_eq!(buffer[(31, 2)].symbol(), " ");
        assert_ne!(buffer[(32, 2)].symbol(), "┌");

        // Verify 3-row mini Braille sparklines are rendered in 30-column node quality panel
        let mut node_panel_has_braille = false;
        for y in 4..17 {
            for x in 7..30 {
                let s = buffer[(x, y)].symbol();
                if s.chars().any(|ch| ('\u{2800}'..='\u{28FF}').contains(&ch)) {
                    node_panel_has_braille = true;
                    break;
                }
            }
        }
        assert!(
            node_panel_has_braille,
            "96x30 node quality panel must render mini Braille sparklines"
        );

        // Verify connections panel is not rendered in left column below node panel
        for y in 2..28 {
            let s = buffer[(1, y)].symbol();
            assert_ne!(s, "┌", "Active connections panel must be omitted at 96x30");
        }

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
            network_status: GlobalNetworkStatus::Stable,
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
            active_connections: vec![ActiveConnectionSummary {
                destination: "chat.openai.com",
                rate_label: "1.2M/s".to_string(),
                rule: "Proxy",
            }],
        };

        terminal
            .draw(|f| render_idle_dashboard(f, &snapshot))
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer.area.width, 80);
        assert_eq!(buffer.area.height, 24);

        let t = buffer_to_text(buffer);
        assert!(!t.contains("监控"));
        assert!(t.contains("DASHBOARD / AirTCP / JP-Edge-03"));
        assert!(t.contains("核心历史 · 30 分钟"));
        // Both node quality and active connections are omitted at width 80
        assert!(!t.contains("节点质量"));
        assert!(!t.contains("活动连接"));
        assert!(!t.contains("chat.openai.com"));
        assert!(!t.contains("8.0 MiB/s"));
        assert!(t.contains("GLOBAL NET  STABLE  ↓3.9M/s  ↑2.1M/s"));
        assert!(t.contains("延迟 · ms"));
        assert!(t.contains("核心流量 · MiB/s"));
        assert!(t.contains("-30m") && t.contains("-15m") && t.contains("现在"));

        // Compact Figma 1027:16 keeps a one-cell outer gutter around the
        // global-history surface and moves the footer to the final two rows.
        assert_eq!(buffer[(0, 0)].symbol(), " ");
        assert_eq!(buffer[(1, 0)].symbol(), "D");
        assert_eq!(buffer[(0, 2)].symbol(), " ");
        assert_ne!(buffer[(1, 2)].symbol(), "┌");
        assert_eq!(buffer[(3, 2)].symbol(), "核");
        assert_eq!(buffer[(79, 2)].symbol(), " ");
        assert!(
            buffer_to_text(buffer)
                .lines()
                .nth(23)
                .unwrap()
                .contains("GLOBAL NET  STABLE")
        );
    }

    #[test]
    fn test_render_idle_dashboard_caps_and_centers_large_windows_terminal_compositions() {
        let snapshot = IdleDashboardSnapshot {
            active_provider: "AirTCP",
            active_node: "JP-Edge-03",
            network_status: GlobalNetworkStatus::Stable,
            current_down_rate: "3.9M/s",
            current_up_rate: "2.1M/s",
            traffic_samples: &[],
            latency_samples: &[],
            route_intervals: &[],
            node_quality: None,
            active_connections: vec![],
        };

        for (width, height, content_x, content_y, aggregate_title_x) in
            [(132, 36, 1, 3, 45), (160, 45, 8, 7, 53)]
        {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|f| render_idle_dashboard(f, &snapshot))
                .unwrap();
            let buffer = terminal.backend().buffer();

            assert_eq!(buffer[(content_x + 1, content_y)].symbol(), "节");
            assert_eq!(buffer[(aggregate_title_x, content_y)].symbol(), "核");
            assert_eq!(buffer[(content_x, content_y + 16)].symbol(), "┌");
            if content_x > 0 {
                assert_eq!(buffer[(content_x - 1, content_y)].symbol(), " ");
            }
            if content_y > 2 {
                assert_eq!(buffer[(content_x + 1, content_y - 1)].symbol(), " ");
            }

            let footer = buffer_to_text(buffer);
            let footer = footer.lines().nth(usize::from(height - 1)).unwrap();
            assert!(footer.starts_with(' '));
            assert!(footer.contains("Ctrl+K"));
            assert!(footer.ends_with("GLOBAL NET  STABLE  ↓3.9M/s  ↑2.1M/s "));
        }

        let backend = TestBackend::new(150, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render_idle_dashboard(f, &snapshot))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let text = buffer_to_text(buffer);
        assert_eq!(buffer[(4, 2)].symbol(), "节");
        assert!(text.contains("节点质量"));
        assert!(!text.contains("活动连接"));
        assert!(text.lines().nth(23).unwrap().contains("GLOBAL NET  STABLE"));

        let backend = TestBackend::new(120, 29);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render_idle_dashboard(f, &snapshot))
            .unwrap();
        let text = buffer_to_text(terminal.backend().buffer());
        assert!(text.contains("节点质量"));
        assert!(!text.contains("活动连接"));
        assert!(text.contains("核心历史 · 30 分钟"));
    }

    #[test]
    fn test_render_idle_dashboard_does_not_fabricate_missing_quality_measurements() {
        let backend = TestBackend::new(96, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let snapshot = IdleDashboardSnapshot {
            active_provider: "AirTCP",
            active_node: "JP-Edge-03",
            network_status: GlobalNetworkStatus::Idle,
            current_down_rate: "0B/s",
            current_up_rate: "0B/s",
            traffic_samples: &[],
            latency_samples: &[],
            route_intervals: &[],
            node_quality: Some(ActiveNodeQualitySnapshot {
                node_name: "JP-Edge-03",
                current_latency_ms: None,
                warm_median_ms: None,
                p95_ms: None,
                cold_start_ms: None,
                sustained_speed_label: None,
                reachability_label: "Untested",
                latency_points: vec![],
                sustained_points: vec![],
                latest_latency: None,
                latency_sample_age: None,
                latest_sustained_speed: None,
                sustained_sample_age: None,
            }),
            active_connections: vec![],
        };

        terminal
            .draw(|f| render_idle_dashboard(f, &snapshot))
            .unwrap();
        let text = buffer_to_text(terminal.backend().buffer());
        assert!(!text.contains("28 ms"));
        assert!(!text.contains("8.0 MiB/s"));
        assert!(!text.contains("刚测"));
        assert!(!text.contains("2分钟前"));
        assert!(text.contains("延迟 —"));
        assert!(text.contains("实测 —"));

        let footer = text.lines().nth(29).unwrap();
        let idle_offset = footer.find("IDLE").unwrap();
        let transfer_offset = footer.find('↓').unwrap();
        let idle_x = unicode_width::UnicodeWidthStr::width(&footer[..idle_offset]) as u16;
        let transfer_x = unicode_width::UnicodeWidthStr::width(&footer[..transfer_offset]) as u16;
        let theme = Theme::detect();
        assert_eq!(terminal.backend().buffer()[(idle_x, 29)].fg, theme.text_muted());
        assert_eq!(
            terminal.backend().buffer()[(transfer_x, 29)].fg,
            theme.text_transfer()
        );
    }

    #[test]
    fn test_render_idle_dashboard_unsupported_guard() {
        for (w, h) in [(70, 20), (79, 24), (80, 23)] {
            let backend = TestBackend::new(w, h);
            let mut terminal = Terminal::new(backend).unwrap();

            let snapshot = IdleDashboardSnapshot {
                active_provider: "AirTCP",
                active_node: "JP-Edge-03",
                network_status: GlobalNetworkStatus::Stable,
                current_down_rate: "0.0M/s",
                current_up_rate: "0.0M/s",
                traffic_samples: &[],
                latency_samples: &[],
                route_intervals: &[],
                node_quality: None,
                active_connections: vec![],
            };

            terminal
                .draw(|f| render_idle_dashboard(f, &snapshot))
                .unwrap();
            let buffer = terminal.backend().buffer();
            assert_eq!(buffer.area.width, w);
            assert_eq!(buffer.area.height, h);

            let t = buffer_to_text(buffer);
            assert!(t.contains("Resize terminal to at least 80x24"));
            assert!(t.contains("q to quit") && t.contains("? for help"));
        }
    }
}

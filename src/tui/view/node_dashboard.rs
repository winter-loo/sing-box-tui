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
    pub(crate) destination: String,
    pub(crate) transfer_label: String,
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

pub(crate) struct NodeDashboardSnapshot<'a> {
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

impl<'a> NodeDashboardSnapshot<'a> {
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

fn emphasized_panel(f: &mut Frame, r: Rect, title: &str, theme: &Theme) -> Rect {
    if r.width < 2 || r.height < 2 {
        return Rect::new(r.x, r.y, 0, 0);
    }
    surface(f, r, theme);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.text_secondary()))
        .title(title);
    let inner = block.inner(r);
    f.render_widget(block, r);
    inner
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
    min: f64,
    max: f64,
) {
    if r.width == 0 || r.height == 0 {
        return;
    }
    f.render_widget(
        Block::default().style(Style::default().bg(Color::Black)),
        r,
    );
    if points.is_empty() {
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
            .style(Style::default().bg(Color::Black))
            .x_axis(Axis::default().bounds([0., 30.]))
            .y_axis(Axis::default().bounds([min, max]))
            .legend_position(None),
        r,
    );
}

const SPARKLINE_OBSERVATION_GAP_MINUTES: f64 = 2.5;

fn separated_traffic_minutes(minute: f64, plot_width: u16) -> (f64, f64) {
    let drawable_columns = f64::from(plot_width.saturating_sub(1).max(1));
    let half_column_minutes = 30.0 / drawable_columns;
    (
        (minute - half_column_minutes).clamp(0.0, 30.0),
        (minute + half_column_minutes).clamp(0.0, 30.0),
    )
}

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
    if r.width == 0 || r.height == 0 {
        return;
    }
    f.render_widget(
        Block::default().style(Style::default().bg(Color::Black)),
        r,
    );
    if points.is_empty() {
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
            .style(Style::default().bg(Color::Black))
            .x_axis(Axis::default().bounds([0., 30.]))
            .y_axis(Axis::default().bounds([0., max]))
            .legend_position(None),
        r,
    );
}

fn render_node_metric(
    f: &mut Frame,
    area: Rect,
    title: &str,
    age: &str,
    top_value: &str,
    points: &[(f64, f64)],
    color: Color,
    max: f64,
    theme: &Theme,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    label(
        f,
        area.x,
        area.y,
        area.width.saturating_sub(7),
        title,
        color,
    );
    label(
        f,
        area.x + area.width.saturating_sub(7),
        area.y,
        7,
        age,
        theme.text_muted(),
    );
    if area.height < 3 {
        return;
    }

    let plot_height = area.height.saturating_sub(2);
    label(f, area.x, area.y + 1, 4, top_value, theme.text_muted());
    label(f, area.x, area.y + plot_height, 4, "0", theme.text_muted());
    plot_braille_sparklines(
        f,
        Rect::new(
            area.x + 5,
            area.y + 1,
            area.width.saturating_sub(5),
            plot_height,
        ),
        points,
        color,
        max,
    );
    let axis_y = area.y + area.height - 1;
    label(f, area.x + 5, axis_y, 4, "-30m", theme.text_muted());
    label(
        f,
        area.x + area.width.saturating_sub(4),
        axis_y,
        4,
        "现在",
        theme.text_muted(),
    );
}

fn node_metric_axis_max(points: &[(f64, f64)], minimum: f64, step: f64) -> f64 {
    let observed_max = points
        .iter()
        .map(|(_, value)| *value)
        .filter(|value| value.is_finite())
        .fold(0.0_f64, f64::max);
    let padded = observed_max * 1.1;
    minimum.max((padded / step).ceil() * step)
}

fn render_node_panel(f: &mut Frame, r: Rect, snapshot: &NodeDashboardSnapshot<'_>, theme: &Theme) {
    let inner = emphasized_panel(
        f,
        r,
        &format!(" 节点质量 · {} ", snapshot.active_node),
        theme,
    );
    let x = inner.x;
    let w = inner.width;
    let y = inner.y;

    let Some(q) = &snapshot.node_quality else {
        label(f, x, y + 1, w, "无节点数据", theme.text_muted());
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
    let speed_raw = q
        .latest_sustained_speed
        .as_deref()
        .or(q.sustained_speed_label.as_deref())
        .unwrap_or("—");
    let speed_title = if speed_raw.starts_with("吞吐量") {
        speed_raw.to_string()
    } else {
        format!("吞吐量 {speed_raw}")
    };
    let speed_age = q.sustained_sample_age.as_deref().unwrap_or("—");
    let latency_max = node_metric_axis_max(&q.latency_points, 120.0, 10.0);
    let speed_max = node_metric_axis_max(&q.sustained_points, 10.0, 1.0);
    let latency_top = format!("{latency_max:.0}");
    let speed_top = format!("{speed_max:.0}");
    let latency_height = inner.height.div_ceil(2);
    render_node_metric(
        f,
        Rect::new(x, y, w, latency_height),
        &latency_title,
        latency_age,
        &latency_top,
        &q.latency_points,
        theme.text_latency(),
        latency_max,
        theme,
    );
    render_node_metric(
        f,
        Rect::new(
            x,
            y + latency_height,
            w,
            inner.height.saturating_sub(latency_height),
        ),
        &speed_title,
        speed_age,
        &speed_top,
        &q.sustained_points,
        theme.text_success(),
        speed_max,
        theme,
    );
}

fn render_connections_panel(
    f: &mut Frame,
    r: Rect,
    snapshot: &NodeDashboardSnapshot<'_>,
    theme: &Theme,
) {
    let title = format!(" 活动连接 · {} ", snapshot.active_connections.len());
    let inner = emphasized_panel(f, r, &title, theme);
    let x = inner.x;
    let w = inner.width;
    let desired_transfer_width = snapshot
        .active_connections
        .iter()
        .map(|connection| connection.transfer_label.width() as u16)
        .max()
        .unwrap_or(0)
        .max("传输".width() as u16);
    let transfer_width = desired_transfer_width.min(w.saturating_sub(8));
    let destination_width = w.saturating_sub(transfer_width + 1);
    let transfer_x = x + destination_width + 1;
    label(f, x, r.y + 1, destination_width, "目标", theme.text_muted());
    label(
        f,
        transfer_x,
        r.y + 1,
        transfer_width,
        "传输",
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
            destination_width,
            &conn.destination,
            theme.text_primary(),
        );
        label(
            f,
            transfer_x,
            r.y + 2 + i as u16,
            transfer_width,
            &conn.transfer_label,
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
    snapshot: &NodeDashboardSnapshot<'_>,
    theme: &Theme,
) {
    surface(f, r, theme);
    if r.height < 20 || r.width < 16 {
        return;
    }
    let panels_height = r.height;
    let latency_height = panels_height / 2;
    let latency_area = Rect::new(r.x, r.y, r.width, latency_height);
    let traffic_area = Rect::new(
        r.x,
        latency_area.y + latency_area.height,
        r.width,
        panels_height.saturating_sub(latency_height),
    );
    let latency_inner = emphasized_panel(f, latency_area, " 延迟 · ms ", theme);
    let traffic_inner = emphasized_panel(f, traffic_area, " 总吞吐量 · MiB/s ", theme);

    let plot_x = latency_inner.x + 4;
    let plot_width = latency_inner.width.saturating_sub(4);
    let plot_height = latency_inner.height.saturating_sub(3);
    let latency_plot_y = latency_inner.y + 1;
    let latency_axis_y = latency_plot_y + plot_height;
    let traffic_plot_x = traffic_inner.x + 4;
    let traffic_plot_width = traffic_inner.width.saturating_sub(4);
    let traffic_plot_height = traffic_inner.height.saturating_sub(3);
    let traffic_plot_y = traffic_inner.y + 1;
    let traffic_axis_y = traffic_plot_y + traffic_plot_height;

    let now_ms = now_unix_ms();
    let cutoff_ms = now_ms.saturating_sub(METRIC_RETENTION_WINDOW_MS);
    let aggregate_latency_points = snapshot
        .latency_samples
        .iter()
        .map(|sample| (0.0, sample.latency_ms as f64))
        .chain(
            snapshot
                .node_quality
                .iter()
                .flat_map(|quality| quality.latency_points.iter().copied()),
        )
        .collect::<Vec<_>>();
    let latency_max = node_metric_axis_max(&aggregate_latency_points, 120.0, 10.0);
    let latency_top = format!("{latency_max:.0}");

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
                label(
                    f,
                    bx,
                    latency_inner.y,
                    rem_w,
                    &interval.node_name,
                    color,
                );
            }
        }
    } else {
        label(
            f,
            plot_x,
            latency_inner.y,
            plot_width.min(14),
            snapshot.active_node,
            theme.route_color(0),
        );
    }

    let latest_latency = snapshot
        .latency_samples
        .last()
        .map(|sample| sample.latency_ms)
        .or_else(|| snapshot.node_quality.as_ref()?.current_latency_ms);
    if let Some(latency_ms) = latest_latency {
        let value = latency_ms.to_string();
        label(
            f,
            latency_inner.x + latency_inner.width.saturating_sub(value.width() as u16),
            latency_inner.y,
            value.width() as u16,
            &value,
            theme.text_muted(),
        );
    }
    label(
        f,
        latency_inner.x,
        latency_plot_y,
        latency_top.width() as u16,
        &latency_top,
        theme.text_muted(),
    );
    label(
        f,
        latency_inner.x + 2,
        latency_plot_y + plot_height - 1,
        1,
        "0",
        theme.text_muted(),
    );
    let traffic_legend = Line::from(vec![
        Span::styled("↓下载", Style::default().fg(theme.text_accent())),
        Span::styled("  ", Style::default().fg(theme.text_muted())),
        Span::styled("↑上传", Style::default().fg(theme.text_warning())),
    ]);
    f.render_widget(
        Paragraph::new(traffic_legend),
        Rect::new(
            traffic_inner.x + traffic_inner.width.saturating_sub(12),
            traffic_inner.y,
            12,
            1,
        ),
    );
    label(
        f,
        traffic_inner.x + 1,
        traffic_plot_y,
        2,
        "10",
        theme.text_muted(),
    );
    label(
        f,
        traffic_inner.x + 2,
        traffic_plot_y + traffic_plot_height - 1,
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
            let lat = s.latency_ms as f64;

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
    } else if let Some(quality) = &snapshot.node_quality {
        let segments = segment_sparkline_series(
            &quality.latency_points,
            SPARKLINE_OBSERVATION_GAP_MINUTES,
        );
        for (segment, kind) in segments {
            latency_series.push(segment);
            latency_colors.push(theme.route_color(0));
            latency_kinds.push(kind);
        }
    }

    plot(
        f,
        Rect::new(plot_x, latency_plot_y, plot_width, plot_height),
        &latency_series,
        &latency_colors,
        &latency_kinds,
        0.0,
        latency_max,
    );

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
            let (down_minute, up_minute) =
                separated_traffic_minutes(minute, traffic_plot_width);
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
                traffic_colors.push(theme.text_accent());
                traffic_kinds.push(if down_seg.len() == 1 {
                    GraphType::Scatter
                } else {
                    GraphType::Line
                });
                traffic_series.push(std::mem::take(&mut down_seg));

                traffic_colors.push(theme.text_warning());
                traffic_kinds.push(if up_seg.len() == 1 {
                    GraphType::Scatter
                } else {
                    GraphType::Line
                });
                traffic_series.push(std::mem::take(&mut up_seg));
            }

            cur_idx = idx;
            last_ts = s.recorded_at_ms;
            down_seg.push((down_minute, down_mib));
            up_seg.push((up_minute, up_mib));
        }

        if !down_seg.is_empty() {
            traffic_colors.push(theme.text_accent());
            traffic_kinds.push(if down_seg.len() == 1 {
                GraphType::Scatter
            } else {
                GraphType::Line
            });
            traffic_series.push(down_seg);

            traffic_colors.push(theme.text_warning());
            traffic_kinds.push(if up_seg.len() == 1 {
                GraphType::Scatter
            } else {
                GraphType::Line
            });
            traffic_series.push(up_seg);
        }
    }

    plot(
        f,
        Rect::new(
            traffic_plot_x,
            traffic_plot_y,
            traffic_plot_width,
            traffic_plot_height,
        ),
        &traffic_series,
        &traffic_colors,
        &traffic_kinds,
        -1.0,
        10.,
    );

    for (axis_x, axis_y, axis_width) in [
        (plot_x, latency_axis_y, plot_width),
        (traffic_plot_x, traffic_axis_y, traffic_plot_width),
    ] {
        if axis_width > 0 {
            label(
                f,
                axis_x,
                axis_y,
                axis_width,
                &"─".repeat(usize::from(axis_width)),
                theme.text_muted(),
            );
            for (fraction, text) in [(0., "-30m"), (0.5, "-15m"), (1., "现在")] {
                let offset =
                    ((f64::from(axis_width.saturating_sub(1)) * fraction).round() as u16)
                        .min(axis_width.saturating_sub(text.width() as u16));
                label(
                    f,
                    axis_x + offset,
                    axis_y + 1,
                    text.width() as u16,
                    text,
                    theme.text_muted(),
                );
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct NodeDashboardLayout {
    node: Option<Rect>,
    connections: Option<Rect>,
    aggregate: Rect,
}

impl NodeDashboardLayout {
    const HEADER_HEIGHT: u16 = 2;
    const FOOTER_HEIGHT: u16 = 2;

    fn from_area(area: Rect) -> Self {
        let body_y = area.y + Self::HEADER_HEIGHT;
        let body_height = area
            .height
            .saturating_sub(Self::HEADER_HEIGHT + Self::FOOTER_HEIGHT);
        let content = Rect::new(
            area.x + 1,
            body_y,
            area.width.saturating_sub(2),
            body_height,
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
        .max(30);
        let aggregate = Rect::new(
            content.x + left_width + 1,
            content.y,
            content.width.saturating_sub(left_width + 1),
            content.height,
        );
        let node_height = if full { content.height / 2 } else { 15 };
        let node = Some(Rect::new(content.x, content.y, left_width, node_height));
        let connections = full.then(|| {
            Rect::new(
                content.x,
                content.y + node_height,
                left_width,
                content.height.saturating_sub(node_height),
            )
        });

        Self {
            node,
            connections,
            aggregate,
        }
    }
}

/// Main entry point for rendering the Node Dashboard from the available terminal-cell budget.
pub(crate) fn render_node_dashboard(frame: &mut Frame, snapshot: &NodeDashboardSnapshot<'_>) {
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
        &["NODE DASHBOARD", provider, node],
    );
    label(
        frame,
        0,
        1,
        r.width,
        &"─".repeat(usize::from(r.width)),
        theme.border_default(),
    );

    let layout = NodeDashboardLayout::from_area(r);
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
    use crate::tui::ds::ColorCapability;
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

    fn plot_glyph_count(buffer: &ratatui::buffer::Buffer, area: Rect) -> usize {
        let mut count = 0;
        for y in area.y..area.y + area.height {
            for x in area.x..area.x + area.width {
                if !buffer[(x, y)].symbol().trim().is_empty() {
                    count += 1;
                }
            }
        }
        count
    }

    fn assert_visible_border(buffer: &ratatui::buffer::Buffer, area: Rect) {
        let right = area.x + area.width - 1;
        let bottom = area.y + area.height - 1;
        assert_eq!(buffer[(area.x, area.y)].symbol(), "┌");
        assert_eq!(buffer[(right, area.y)].symbol(), "┐");
        assert_eq!(buffer[(area.x, bottom)].symbol(), "└");
        assert_eq!(buffer[(right, bottom)].symbol(), "┘");
    }

    #[test]
    fn close_download_and_upload_values_keep_distinct_colors() {
        let backend = TestBackend::new(20, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = Theme::new(ColorCapability::TrueColor);
        let separated_points = (0..=30)
            .map(|minute| separated_traffic_minutes(f64::from(minute), 20))
            .collect::<Vec<_>>();
        let download_points = separated_points
            .iter()
            .map(|(down_minute, _)| (*down_minute, 1.0))
            .collect::<Vec<_>>();
        let upload_points = separated_points
            .iter()
            .map(|(_, up_minute)| (*up_minute, 1.1))
            .collect::<Vec<_>>();

        terminal
            .draw(|frame| {
                plot(
                    frame,
                    Rect::new(0, 0, 20, 6),
                    &[download_points.clone(), upload_points.clone()],
                    &[theme.text_accent(), theme.text_warning()],
                    &[GraphType::Line, GraphType::Line],
                    -1.0,
                    10.0,
                );
            })
            .unwrap();

        let visible_colors = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .filter(|cell| !cell.symbol().trim().is_empty())
            .map(|cell| cell.fg)
            .collect::<Vec<_>>();
        let download_cells = visible_colors
            .iter()
            .filter(|&&color| color == theme.text_accent())
            .count();
        let upload_cells = visible_colors
            .iter()
            .filter(|&&color| color == theme.text_warning())
            .count();
        assert!(download_cells > 0, "the download line must remain cyan");
        assert!(upload_cells > 0, "the upload line must remain yellow");
    }

    #[test]
    fn test_render_node_dashboard_standard_120x30() {
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
                latency_ms: 386,
            },
            LatencySample {
                recorded_at_ms: now_unix_ms() - 5 * 60_000,
                selector: "Proxy".to_string(),
                node_name: "JP-Edge-03".to_string(),
                latency_ms: 420,
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

        let snapshot = NodeDashboardSnapshot {
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
                current_latency_ms: Some(386),
                warm_median_ms: Some(28),
                p95_ms: Some(35),
                cold_start_ms: Some(42),
                sustained_speed_label: Some("18.0 MiB/s".to_string()),
                reachability_label: "Stable Reachable",
                latency_points: vec![(30.0, 386.0)],
                sustained_points: vec![(28.0, 18.0)],
                latest_latency: Some("386 ms".to_string()),
                latency_sample_age: Some("刚测".to_string()),
                latest_sustained_speed: Some("18.0 MiB/s".to_string()),
                sustained_sample_age: Some("2分钟前".to_string()),
            }),
            active_connections: vec![ActiveConnectionSummary {
                destination: "chat.openai.com".to_string(),
                transfer_label: "152.3MiB".to_string(),
                rule: "Proxy",
            }],
        };

        terminal
            .draw(|f| render_node_dashboard(f, &snapshot))
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer.area.width, 120);
        assert_eq!(buffer.area.height, 30);
        assert_visible_border(buffer, Rect::new(1, 2, 37, 13));
        assert_visible_border(buffer, Rect::new(1, 15, 37, 13));
        assert_visible_border(buffer, Rect::new(39, 2, 80, 13));
        assert_visible_border(buffer, Rect::new(39, 15, 80, 13));
        assert_eq!(buffer[(1, 15)].fg, buffer[(39, 15)].fg);
        assert!(plot_glyph_count(buffer, Rect::new(7, 4, 30, 4)) > 0);
        assert!(plot_glyph_count(buffer, Rect::new(7, 10, 30, 3)) > 0);
        assert!(plot_glyph_count(buffer, Rect::new(45, 7, 72, 7)) > 0);
        assert!(plot_glyph_count(buffer, Rect::new(45, 18, 72, 7)) > 0);
        let traffic_area = Rect::new(45, 18, 72, 7);
        let traffic_glyph_colors = traffic_area.rows().flat_map(|row| {
            row.columns().filter_map(|position| {
                let cell = &buffer[position];
                (!cell.symbol().trim().is_empty()).then_some(cell.fg)
            })
        });
        let traffic_glyph_colors = traffic_glyph_colors.collect::<Vec<_>>();
        let theme = Theme::detect();
        assert!(
            traffic_glyph_colors.contains(&theme.text_accent()),
            "download series must use the Figma cyan token"
        );
        assert!(
            traffic_glyph_colors.contains(&theme.text_warning()),
            "upload series must use the Figma yellow token"
        );

        let t = buffer_to_text(buffer);
        assert!(!t.contains("监控"));
        assert!(t.contains("NODE DASHBOARD / AirTCP / JP-Edge-03"));
        assert!(!t.contains("核心历史 · 30 分钟"));
        assert!(t.contains("节点质量"));
        assert!(t.contains("活动连接 · 1"));
        assert!(t.contains("chat.openai.com"));
        assert!(t.contains("152.3MiB"));
        assert!(t.contains("430"));
        assert!(t.contains("470"));
        assert!(t.contains("10"));
        assert!(t.contains("386 ms"));
        assert!(t.contains("18.0 MiB/s"));
        assert!(t.contains("↓下载  ↑上传"));
        assert!(!t.contains("↑ 点"));
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

        // The body keeps one-cell outer gutters, a 37-cell left rail, a one-cell
        // panel gap, and an 80-cell global-history surface. Each visible group
        // now owns an explicit border inside that budget.
        assert_eq!(buffer[(0, 0)].symbol(), " ");
        assert_eq!(buffer[(1, 0)].symbol(), "N");
        assert_eq!(buffer[(0, 2)].symbol(), " ");
        assert_eq!(buffer[(1, 2)].symbol(), "┌");
        assert_eq!(buffer[(39, 2)].symbol(), "┌");
        assert_eq!(buffer[(1, 15)].symbol(), "┌");
        assert_eq!(buffer[(38, 15)].symbol(), " ");
        assert_eq!(buffer[(119, 15)].symbol(), " ");

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
    fn test_render_node_dashboard_breakpoint_96x30() {
        let backend = TestBackend::new(96, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        let snapshot = NodeDashboardSnapshot {
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
                destination: "chat.openai.com".to_string(),
                transfer_label: "1.2MiB".to_string(),
                rule: "Proxy",
            }],
        };

        terminal
            .draw(|f| render_node_dashboard(f, &snapshot))
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer.area.width, 96);
        assert_eq!(buffer.area.height, 30);

        let t = buffer_to_text(buffer);
        assert!(!t.contains("监控"));
        assert!(t.contains("NODE DASHBOARD / AirTCP / JP-Edge-03"));
        assert!(!t.contains("核心历史 · 30 分钟"));
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
        assert_eq!(buffer[(1, 0)].symbol(), "N");
        assert_eq!(buffer[(0, 2)].symbol(), " ");
        assert_eq!(buffer[(1, 2)].symbol(), "┌");
        assert_eq!(buffer[(31, 2)].symbol(), " ");
        assert_eq!(buffer[(32, 2)].symbol(), "┌");

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
        for y in 17..28 {
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
    fn test_render_node_dashboard_compact_80x24() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        let snapshot = NodeDashboardSnapshot {
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
                destination: "chat.openai.com".to_string(),
                transfer_label: "1.2MiB".to_string(),
                rule: "Proxy",
            }],
        };

        terminal
            .draw(|f| render_node_dashboard(f, &snapshot))
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer.area.width, 80);
        assert_eq!(buffer.area.height, 24);

        let t = buffer_to_text(buffer);
        assert!(!t.contains("监控"));
        assert!(t.contains("NODE DASHBOARD / AirTCP / JP-Edge-03"));
        assert!(!t.contains("核心历史 · 30 分钟"));
        // Both node quality and active connections are omitted at width 80
        assert!(!t.contains("节点质量"));
        assert!(!t.contains("活动连接"));
        assert!(!t.contains("chat.openai.com"));
        assert!(!t.contains("8.0 MiB/s"));
        assert!(t.contains("GLOBAL NET  STABLE  ↓3.9M/s  ↑2.1M/s"));
        assert!(t.contains("延迟 · ms"));
        assert!(t.contains("总吞吐量 · MiB/s"));
        assert!(t.contains("-30m") && t.contains("-15m") && t.contains("现在"));

        // The compact layout keeps a one-cell outer gutter around the
        // aggregate charts and moves the footer to the final two rows.
        assert_eq!(buffer[(0, 0)].symbol(), " ");
        assert_eq!(buffer[(1, 0)].symbol(), "N");
        assert_eq!(buffer[(0, 2)].symbol(), " ");
        assert_eq!(buffer[(1, 2)].symbol(), "┌");
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
    fn test_render_node_dashboard_fills_large_window_body() {
        let snapshot = NodeDashboardSnapshot {
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

        for (width, height, left_width) in [(132, 36, 41), (160, 45, 50), (206, 48, 64)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|f| render_node_dashboard(f, &snapshot))
                .unwrap();
            let buffer = terminal.backend().buffer();

            assert_eq!(buffer[(3, 2)].symbol(), "节");
            assert_eq!(buffer[(left_width + 2, 2)].symbol(), "┌");
            let left_content_height = height - 4;
            let node_height = left_content_height / 2;
            let connections_y = 2 + node_height;
            assert_visible_border(buffer, Rect::new(1, 2, left_width, node_height));
            assert_visible_border(
                buffer,
                Rect::new(
                    1,
                    connections_y,
                    left_width,
                    left_content_height - node_height,
                ),
            );
            assert_eq!(buffer[(0, 2)].symbol(), " ");
            assert_eq!(buffer[(width - 1, 2)].symbol(), " ");
            assert_eq!(buffer[(width - 2, 2)].bg, Theme::detect().bg_surface());

            let footer = buffer_to_text(buffer);
            let footer = footer.lines().nth(usize::from(height - 1)).unwrap();
            assert!(footer.starts_with(' '));
            assert!(footer.contains("Ctrl+K"));
            assert!(footer.ends_with("GLOBAL NET  STABLE  ↓3.9M/s  ↑2.1M/s "));
        }

        let backend = TestBackend::new(150, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render_node_dashboard(f, &snapshot))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let text = buffer_to_text(buffer);
        assert_eq!(buffer[(3, 2)].symbol(), "节");
        assert!(text.contains("节点质量"));
        assert!(!text.contains("活动连接"));
        assert!(text.lines().nth(23).unwrap().contains("GLOBAL NET  STABLE"));

        let backend = TestBackend::new(120, 29);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render_node_dashboard(f, &snapshot))
            .unwrap();
        let text = buffer_to_text(terminal.backend().buffer());
        assert!(text.contains("节点质量"));
        assert!(!text.contains("活动连接"));
        assert!(!text.contains("核心历史 · 30 分钟"));
    }

    #[test]
    fn test_render_node_dashboard_does_not_fabricate_missing_quality_measurements() {
        let backend = TestBackend::new(96, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let snapshot = NodeDashboardSnapshot {
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
                latency_points: vec![(28.0, 50.0)],
                sustained_points: vec![],
                latest_latency: None,
                latency_sample_age: None,
                latest_sustained_speed: None,
                sustained_sample_age: None,
            }),
            active_connections: vec![],
        };

        terminal
            .draw(|f| render_node_dashboard(f, &snapshot))
            .unwrap();
        let text = buffer_to_text(terminal.backend().buffer());
        assert!(!text.contains("28 ms"));
        assert!(!text.contains("8.0 MiB/s"));
        assert!(!text.contains("刚测"));
        assert!(!text.contains("2分钟前"));
        assert!(text.contains("延迟 —"));
        assert!(text.contains("吞吐量 —"));

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(10, 5)].bg, Color::Black);
        assert_eq!(buffer[(10, 12)].bg, Color::Black);

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
    fn test_render_node_dashboard_unsupported_guard() {
        for (w, h) in [(70, 20), (79, 24), (80, 23)] {
            let backend = TestBackend::new(w, h);
            let mut terminal = Terminal::new(backend).unwrap();

            let snapshot = NodeDashboardSnapshot {
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
                .draw(|f| render_node_dashboard(f, &snapshot))
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

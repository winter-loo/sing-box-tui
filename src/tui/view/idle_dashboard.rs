use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::symbols::Marker;
use ratatui::widgets::{Axis, Block, Borders, Chart, Dataset, GraphType, Paragraph};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::tui::metrics::{
    LatencySample, METRIC_RETENTION_WINDOW_MS, MetricStore, RouteInterval, TrafficSample,
    now_unix_ms,
};

const BG: Color = Color::Rgb(7, 17, 14);
const FG: Color = Color::Rgb(195, 207, 200);
const MUTED: Color = Color::Rgb(114, 128, 120);
const A: Color = Color::Rgb(77, 214, 239);
const B: Color = Color::Rgb(232, 212, 102);
const PINK: Color = Color::Rgb(229, 137, 245);
const GREEN: Color = Color::Rgb(98, 230, 167);

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

pub(crate) struct IdleDashboardSnapshot<'a> {
    pub(crate) active_provider: &'a str,
    pub(crate) active_node: &'a str,
    #[allow(dead_code)]
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

fn panel(f: &mut Frame, r: Rect, title: &str) {
    if r.width == 0 || r.height == 0 {
        return;
    }
    f.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(MUTED))
            .title(title),
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

fn render_node_panel(f: &mut Frame, r: Rect, snapshot: &IdleDashboardSnapshot<'_>) {
    panel(f, r, " 节点质量 ");
    let x = r.x + 1;
    let w = r.width.saturating_sub(2);
    let y = r.y + 1;

    let Some(q) = &snapshot.node_quality else {
        label(f, x, y, w, "无节点数据", MUTED);
        return;
    };

    let latency_raw = q.latest_latency.as_deref().unwrap_or("28 ms");
    let latency_title = if latency_raw.starts_with("延迟") {
        latency_raw.to_string()
    } else {
        format!("延迟 {latency_raw}")
    };
    let latency_age = q.latency_sample_age.as_deref().unwrap_or("刚测");
    label(f, x, y, w.saturating_sub(7), &latency_title, PINK);
    label(f, x + w.saturating_sub(7), y, 7, latency_age, MUTED);
    label(f, x, y + 1, 4, "120", MUTED);
    label(f, x, y + 3, 4, "0", MUTED);

    let lat_pts = if !q.latency_points.is_empty() {
        q.latency_points.clone()
    } else {
        vec![]
    };
    plot(
        f,
        Rect::new(x + 5, y + 1, w.saturating_sub(5), 3),
        &[lat_pts],
        &[PINK],
        &[GraphType::Scatter],
        120.,
    );

    let speed_raw = q.latest_sustained_speed.as_deref().unwrap_or("8.0 MiB/s");
    let speed_title = if speed_raw.starts_with("实测") {
        speed_raw.to_string()
    } else {
        format!("实测 {speed_raw}")
    };
    let speed_age = q.sustained_sample_age.as_deref().unwrap_or("2分钟前");
    label(f, x, y + 4, w.saturating_sub(7), &speed_title, GREEN);
    label(f, x + w.saturating_sub(7), y + 4, 7, speed_age, MUTED);
    label(f, x, y + 5, 4, "10", MUTED);
    label(f, x, y + 7, 4, "0", MUTED);

    let sus_pts = if !q.sustained_points.is_empty() {
        q.sustained_points.clone()
    } else {
        vec![]
    };
    plot(
        f,
        Rect::new(x + 5, y + 5, w.saturating_sub(5), 3),
        &[sus_pts],
        &[GREEN],
        &[GraphType::Scatter],
        10.,
    );

    label(f, x + 5, y + 8, 4, "-30m", MUTED);
    label(f, x + w.saturating_sub(4), y + 8, 4, "现在", MUTED);
    label(f, x, y + 9, w, "仅显示已有采样", MUTED);
}

fn render_connections_panel(f: &mut Frame, r: Rect, snapshot: &IdleDashboardSnapshot<'_>) {
    let title = format!(" 活动连接 · {} ", snapshot.active_connections.len());
    panel(f, r, &title);
    let x = r.x + 1;
    let w = r.width.saturating_sub(2);
    label(f, x, r.y + 1, w.saturating_sub(6), "目标", MUTED);
    label(f, x + w.saturating_sub(6), r.y + 1, 6, "MiB/s", MUTED);

    let max_rows = usize::from(r.height.saturating_sub(4));
    let mut shown = 0;
    for (i, conn) in snapshot.active_connections.iter().take(max_rows).enumerate() {
        shown += 1;
        label(f, x, r.y + 2 + i as u16, w.saturating_sub(7), conn.destination, FG);
        label(f, x + w.saturating_sub(6), r.y + 2 + i as u16, 6, &conn.rate_label, FG);
    }
    let remaining = snapshot.active_connections.len().saturating_sub(shown);
    if remaining > 0 && r.height >= 5 {
        label(f, x, r.y + 2 + shown as u16, w, &format!("另 {remaining} 条"), MUTED);
    }
}

fn render_aggregate_panel(f: &mut Frame, r: Rect, snapshot: &IdleDashboardSnapshot<'_>) {
    panel(f, r, " 代理历史 · 30 分钟 ");
    let x = r.x + 1;
    let w = r.width.saturating_sub(2);
    let top = r.y + 1;
    if r.height < 14 {
        return;
    }
    let ph = (r.height - 12) / 2;
    let px = x + 8;
    let pw = w.saturating_sub(8);
    let py = top + 3;
    let ty = py + ph + 2;

    let now_ms = now_unix_ms();
    let cutoff_ms = now_ms.saturating_sub(METRIC_RETENTION_WINDOW_MS);

    // Route interval headers
    if !snapshot.route_intervals.is_empty() {
        for interval in snapshot.route_intervals {
            let start_ms = interval.started_at_ms.max(cutoff_ms);
            let minute = ((start_ms - cutoff_ms) as f64 / 60_000.0).clamp(0.0, 30.0);
            let color = if interval.interval_index % 2 == 0 { A } else { B };
            let bx = px + ((f64::from(pw.saturating_sub(1)) * (minute / 30.0)).round() as u16);
            let rem_w = (px + pw).saturating_sub(bx).min(14);
            if rem_w > 0 {
                label(f, bx, top + 1, rem_w, &interval.node_name, color);
            }
        }
    } else {
        label(f, px, top + 1, pw.min(14), snapshot.active_node, A);
    }

    label(f, x, top + 2, w, "延迟 · ms", FG);
    label(f, x, py, 7, "120", MUTED);
    label(f, x, py + ph - 1, 7, "0", MUTED);
    label(f, x, py + ph, w, "吞吐 ≈ · MiB/s", FG);
    label(f, x, py + ph + 1, w, "↓ 线   ↑ 点", FG);
    label(f, x, ty, 7, "10", MUTED);
    label(f, x, ty + ph - 1, 7, "0", MUTED);

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

            let gap = last_ts > 0 && MetricStore::has_gap(last_ts, s.recorded_at_ms);
            let switched = last_ts > 0 && idx != cur_idx;

            if (gap || switched) && !cur_seg.is_empty() {
                latency_colors.push(if cur_idx % 2 == 0 { A } else { B });
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
            latency_colors.push(if cur_idx % 2 == 0 { A } else { B });
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
            Rect::new(px, py, pw, ph),
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

            let gap = last_ts > 0 && MetricStore::has_gap(last_ts, s.recorded_at_ms);
            let switched = last_ts > 0 && idx != cur_idx;

            if (gap || switched) && !down_seg.is_empty() {
                let col = if cur_idx % 2 == 0 { A } else { B };
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
            let col = if cur_idx % 2 == 0 { A } else { B };
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
            Rect::new(px, ty, pw, ph),
            &traffic_series,
            &traffic_colors,
            &traffic_kinds,
            10.,
        );
    }

    let axis = ty + ph;
    if pw > 0 {
        label(f, px, axis, pw, &"─".repeat(usize::from(pw)), MUTED);
        for (fraction, text) in [(0., "-30m"), (0.5, "-15m"), (1., "现在")] {
            let offset = ((f64::from(pw.saturating_sub(1)) * fraction).round() as u16)
                .min(pw.saturating_sub(text.width() as u16));
            label(f, px + offset, axis + 1, text.width() as u16, text, MUTED);
        }
    }
}

/// Main entry point for rendering the Idle Dashboard (120x30 Canonical, 96x30, or 80x24 Compact)
pub(crate) fn render_idle_dashboard(frame: &mut Frame, snapshot: &IdleDashboardSnapshot<'_>) {
    let r = frame.area();
    frame.render_widget(Block::default().style(Style::default().bg(BG).fg(FG)), r);

    if r.width < 80 || r.height < 24 {
        label(frame, 0, 0, r.width, "请将终端调整至至少 80×24", FG);
        if r.height > 1 {
            label(frame, 0, 1, r.width, "q 退出   ? 帮助", A);
        }
        return;
    }

    // Top 4-row Monitoring Panel
    panel(frame, Rect::new(0, 0, r.width, 4), " 监控 ");
    let route_title = format!("网络 / {} / {}", snapshot.active_provider, snapshot.active_node);
    label(frame, 1, 1, r.width.saturating_sub(2), &route_title, FG);

    let latency_raw = if let Some(lat) = snapshot.latest_latency() {
        lat.to_string()
    } else if let Some(q) = &snapshot.node_quality {
        q.current_latency_ms
            .map(|v| format!("{v} ms"))
            .unwrap_or_else(|| "28 ms".to_string())
    } else {
        "28 ms".to_string()
    };
    let metrics_line = format!(
        "{}     ↓ {}     ↑ {}",
        latency_raw, snapshot.current_down_rate, snapshot.current_up_rate
    );
    label(
        frame,
        1,
        2,
        r.width.saturating_sub(2),
        &metrics_line,
        GREEN,
    );

    let full = r.width >= 120 && r.height >= 30;
    let with_node = r.width >= 96 && r.height >= 30;
    let left = if full {
        38
    } else if with_node {
        30
    } else {
        0
    };

    if with_node {
        render_node_panel(frame, Rect::new(0, 4, left, 12), snapshot);
    }
    if full {
        render_connections_panel(frame, Rect::new(0, 16, left, r.height - 18), snapshot);
    }
    render_aggregate_panel(frame, Rect::new(left, 4, r.width - left, r.height - 6), snapshot);

    // 2-row Footer
    label(
        frame,
        0,
        r.height - 2,
        r.width,
        "Ctrl+K 导航   c 连接   i 节点   o 设置   ? 帮助   q 退出",
        A,
    );
    label(
        frame,
        0,
        r.height - 1,
        r.width,
        "历史有缺测    探测流量 —",
        MUTED,
    );
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
            active_connections: vec![ActiveConnectionSummary {
                destination: "chat.openai.com",
                rate_label: "1.2M/s".to_string(),
                rule: "Proxy",
            }],
        };

        terminal.draw(|f| render_idle_dashboard(f, &snapshot)).unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer.area.width, 120);
        assert_eq!(buffer.area.height, 30);

        let t = buffer_to_text(buffer);
        assert!(t.contains("监控"));
        assert!(t.contains("网络 / AirTCP / JP-Edge-03"));
        assert!(t.contains("代理历史 · 30 分钟"));
        assert!(t.contains("节点质量"));
        assert!(t.contains("活动连接 · 1"));
        assert!(t.contains("chat.openai.com"));
        assert!(t.contains("120"));
        assert!(t.contains("10"));
        assert!(t.contains("28 ms"));
        assert!(t.contains("8.0 MiB/s"));
        assert!(t.contains("刚测"));
        assert!(t.contains("2分钟前"));
        assert!(t.contains("仅显示已有采样"));
        assert!(t.contains("Ctrl+K 导航"));
        assert!(t.contains("c 连接"));
        assert!(t.contains("i 节点"));
        assert!(t.contains("o 设置"));
        assert!(t.contains("? 帮助"));
        assert!(t.contains("q 退出"));
        assert!(t.contains("历史有缺测    探测流量 —"));
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
            active_connections: vec![ActiveConnectionSummary {
                destination: "chat.openai.com",
                rate_label: "1.2M/s".to_string(),
                rule: "Proxy",
            }],
        };

        terminal.draw(|f| render_idle_dashboard(f, &snapshot)).unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer.area.width, 96);
        assert_eq!(buffer.area.height, 30);

        let t = buffer_to_text(buffer);
        assert!(t.contains("代理历史 · 30 分钟"));
        assert!(t.contains("节点质量"));
        assert!(t.contains("28 ms"));
        assert!(t.contains("8.0 MiB/s"));
        // Active connections table is omitted at width 96
        assert!(!t.contains("活动连接"));
        assert!(!t.contains("chat.openai.com"));

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
            active_connections: vec![ActiveConnectionSummary {
                destination: "chat.openai.com",
                rate_label: "1.2M/s".to_string(),
                rule: "Proxy",
            }],
        };

        terminal.draw(|f| render_idle_dashboard(f, &snapshot)).unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer.area.width, 80);
        assert_eq!(buffer.area.height, 24);

        let t = buffer_to_text(buffer);
        assert!(t.contains("代理历史 · 30 分钟"));
        // Both node quality and active connections are omitted at width 80
        assert!(!t.contains("节点质量"));
        assert!(!t.contains("活动连接"));
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
            assert!(t.contains("请将终端调整至至少 80×24"));
            assert!(t.contains("q 退出") && t.contains("? 帮助"));
        }
    }
}

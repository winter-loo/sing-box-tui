use super::*;

const LATENCY_CHART_MIN_WINDOW: Duration = Duration::from_secs(5 * 60);
const LATENCY_CHART_MAX_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LatencyChartTimeUnit {
    Minutes,
    Hours,
}

#[derive(Clone, Debug)]
pub(crate) struct LatencyChartState {
    pub(crate) selector: String,
    pub(crate) node: String,
    pub(crate) samples: Vec<NodeLatencySample>,
    pub(crate) window: Duration,
    pub(crate) threshold_ms: u64,
    pub(crate) last_refresh: Instant,
    pub(crate) reachability_assessment: Option<NodeReachabilityAssessment>,
    pub(crate) sustained_quality: Option<NodeSustainedQuality>,
    pub(crate) auto_selection_detail: Option<String>,
    pub(crate) usability_details: Vec<UsabilityCriterionDetail>,
    pub(crate) evidence_scroll: u16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct UsabilityCriterionDetail {
    pub(crate) label: String,
    pub(crate) usable: Option<bool>,
    pub(crate) detail: Option<String>,
    pub(crate) expired: bool,
    pub(crate) latest_failure: Option<String>,
}

fn latency_chart_time_unit(window: Duration) -> LatencyChartTimeUnit {
    if window >= Duration::from_secs(2 * 60 * 60) {
        LatencyChartTimeUnit::Hours
    } else {
        LatencyChartTimeUnit::Minutes
    }
}

pub(crate) fn latency_chart_window_label(window: Duration) -> String {
    if window >= Duration::from_secs(60 * 60) {
        format!("{}h", window.as_secs() / 3600)
    } else {
        format!("{}m", window.as_secs() / 60)
    }
}

pub(crate) fn latency_chart_zoom_in(window: Duration) -> Duration {
    (window / 2).max(LATENCY_CHART_MIN_WINDOW)
}

pub(crate) fn latency_chart_zoom_out(window: Duration) -> Duration {
    (window * 2).min(LATENCY_CHART_MAX_WINDOW)
}

fn latency_chart_latest_ms(samples: &[NodeLatencySample]) -> Option<u64> {
    samples.iter().map(|sample| sample.recorded_at_ms).max()
}

fn latency_chart_window_start_ms(samples: &[NodeLatencySample], window: Duration) -> Option<u64> {
    let latest = latency_chart_latest_ms(samples)?;
    Some(latest.saturating_sub(window.as_millis() as u64))
}

fn latency_chart_windowed_samples(
    samples: &[NodeLatencySample],
    window: Duration,
) -> Vec<NodeLatencySample> {
    let Some(start) = latency_chart_window_start_ms(samples, window) else {
        return Vec::new();
    };
    samples
        .iter()
        .filter(|sample| sample.recorded_at_ms >= start)
        .cloned()
        .collect()
}

pub(crate) fn draw_latency_chart(frame: &mut Frame, chart: &LatencyChartState) {
    let area = centered_rect(90, 20, frame.area());
    frame.render_widget(Clear, area);
    let [quality_area, area] = if chart.reachability_assessment.is_some()
        || chart.sustained_quality.is_some()
        || chart.auto_selection_detail.is_some()
        || !chart.usability_details.is_empty()
    {
        Layout::vertical([Constraint::Length(10), Constraint::Min(6)]).areas(area)
    } else {
        Layout::vertical([Constraint::Length(0), Constraint::Min(8)]).areas(area)
    };
    if chart.reachability_assessment.is_some()
        || chart.sustained_quality.is_some()
        || chart.auto_selection_detail.is_some()
        || !chart.usability_details.is_empty()
    {
        let mut lines = Vec::new();
        if let Some(detail) = &chart.auto_selection_detail {
            lines.push(Line::from(format!(
                "Auto-selection: {}",
                truncate_for_width(detail, 96)
            )));
        }
        if let Some(assessment) = &chart.reachability_assessment {
            lines.push(Line::from(format!(
                "Assessment: {}",
                assessment.compact_evidence()
            )));
            for (index, outcome) in assessment.attempts.iter().enumerate() {
                lines.push(Line::from(format!(
                    "Attempt {}: {}",
                    index + 1,
                    probe_outcome_label(outcome)
                )));
            }
        }
        if let Some(sustained) = &chart.sustained_quality {
            match &sustained.outcome {
                SustainedProbeOutcome::Completed(completion) => {
                    lines.push(Line::from(format!(
                        "Sustained: {:.1} MiB/s, {} bytes",
                        completion.throughput_bytes_per_second as f64 / (1024.0 * 1024.0),
                        completion.bytes_read
                    )));
                    lines.push(Line::from(format!(
                        "First byte: {}ms  Completion: {}ms",
                        completion.first_byte_ms, completion.completion_ms
                    )));
                }
                SustainedProbeOutcome::TransferFailed { detail } => {
                    lines.push(Line::from(format!(
                        "Sustained: transfer failed ({})",
                        truncate_for_width(detail, 72)
                    )));
                }
                SustainedProbeOutcome::RuntimeFailed { detail } => {
                    lines.push(Line::from(format!(
                        "Sustained: runtime failed ({})",
                        truncate_for_width(detail, 72)
                    )));
                }
                SustainedProbeOutcome::Cancelled => {
                    lines.push(Line::from("Sustained: cancelled"));
                }
            }
        }
        for criterion in &chart.usability_details {
            if let Some(usable) = criterion.usable {
                lines.push(Line::from(format!(
                    "{} criterion: {}{}",
                    criterion.label,
                    if usable { "usable" } else { "rejected" },
                    criterion
                        .detail
                        .as_deref()
                        .map(|detail| format!(" ({})", truncate_for_width(detail, 56)))
                        .unwrap_or_default()
                )));
                if criterion.expired {
                    lines.push(Line::from(format!(
                        "{} criterion result: expired (excluded from candidates)",
                        criterion.label
                    )));
                }
            }
            if let Some(failure) = &criterion.latest_failure {
                lines.push(Line::from(format!(
                    "{} criterion latest attempt: {}",
                    criterion.label,
                    truncate_for_width(failure, 72)
                )));
            }
        }
        frame.render_widget(
            Paragraph::new(lines)
                .scroll((chart.evidence_scroll, 0))
                .block(
                    Block::default()
                        .title("Node quality evidence (j/k scroll)")
                        .borders(Borders::ALL),
                ),
            quality_area,
        );
    }

    let visible_samples = latency_chart_windowed_samples(&chart.samples, chart.window);
    let segments = latency_chart_segments(&visible_samples);
    let Some(start_ms) = latency_chart_window_start_ms(&chart.samples, chart.window) else {
        frame.render_widget(
            Paragraph::new("No latency history")
                .block(Block::default().title("Latency").borders(Borders::ALL)),
            area,
        );
        return;
    };
    let time_unit = latency_chart_time_unit(chart.window);
    let scale = match time_unit {
        LatencyChartTimeUnit::Minutes => 60_000.0,
        LatencyChartTimeUnit::Hours => 3_600_000.0,
    };
    let segment_data = segments
        .iter()
        .map(|segment| {
            segment
                .iter()
                .map(|point| {
                    (
                        point.0.saturating_sub(start_ms) as f64 / scale,
                        point.1 as f64,
                    )
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();

    if segment_data.iter().all(Vec::is_empty) {
        frame.render_widget(
            Paragraph::new("No successful latency samples in this window")
                .block(Block::default().title("Latency").borders(Borders::ALL)),
            area,
        );
        return;
    }

    let (min_y, max_y) = segment_data
        .iter()
        .flatten()
        .fold((f64::MAX, f64::MIN), |(min_y, max_y), (_, y)| {
            (min_y.min(*y), max_y.max(*y))
        });
    let x_max = chart.window.as_millis() as f64 / scale;
    let x_bounds = [0.0, x_max.max(1.0)];
    let y_bounds = latency_chart_y_bounds(min_y, max_y, chart.threshold_ms);
    let title = format!(
        "Latency: {} / {} ({} samples, window {}, z/Z zoom)",
        chart.selector,
        truncate_for_width(&chart.node, 36),
        visible_samples.len(),
        latency_chart_window_label(chart.window)
    );
    let mut datasets = segment_data
        .iter()
        .enumerate()
        .map(|(index, data)| {
            Dataset::default()
                .name(format!("latency-{index}"))
                .marker(symbols::Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::default().fg(Color::Magenta))
                .data(data)
        })
        .collect::<Vec<_>>();
    let threshold_data = latency_chart_threshold_line(x_bounds[1], chart.threshold_ms);
    datasets.push(
        Dataset::default()
            .name(format!("{}ms limit", chart.threshold_ms))
            .marker(symbols::Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(Color::Yellow))
            .data(&threshold_data),
    );
    let chart_widget = Chart::new(datasets)
        .block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        )
        .x_axis(
            Axis::default()
                .title(match time_unit {
                    LatencyChartTimeUnit::Minutes => "time (minutes)",
                    LatencyChartTimeUnit::Hours => "time (hours)",
                })
                .style(Style::default().fg(Color::Gray))
                .bounds(x_bounds)
                .labels(vec![
                    Span::raw(format!("{} ago", latency_chart_window_label(chart.window))),
                    Span::raw("now"),
                ]),
        )
        .y_axis(
            Axis::default()
                .title("latency (ms)")
                .style(Style::default().fg(Color::Gray))
                .bounds(y_bounds)
                .labels(vec![
                    Span::raw(format!("{:.0}", y_bounds[0])),
                    Span::raw(format!("{:.0}", y_bounds[1])),
                ]),
        );
    frame.render_widget(chart_widget, area);
}

pub(crate) fn latency_chart_evidence_line_count(chart: &LatencyChartState) -> usize {
    let quick = chart
        .reachability_assessment
        .as_ref()
        .map_or(0, |assessment| 1 + assessment.attempts.len());
    let sustained = chart.sustained_quality.as_ref().map_or(0, |quality| {
        usize::from(matches!(
            &quality.outcome,
            SustainedProbeOutcome::Completed(_)
        )) + 1
    });
    usize::from(chart.auto_selection_detail.is_some())
        + quick
        + sustained
        + chart.usability_details.len()
}

fn probe_outcome_label(outcome: &ProbeOutcome) -> String {
    match outcome {
        ProbeOutcome::Reachable { delay_ms } => format!("reachable ({delay_ms}ms)"),
        ProbeOutcome::Timeout => "timeout".to_string(),
        ProbeOutcome::TransportFailure { detail } => format!("transport failure ({detail})"),
        ProbeOutcome::ControllerFailure { status } => format!("controller failure (HTTP {status})"),
        ProbeOutcome::InvalidMeasurement => "invalid measurement".to_string(),
        ProbeOutcome::Cancelled => "cancelled".to_string(),
    }
}

fn latency_chart_segments(samples: &[NodeLatencySample]) -> Vec<Vec<(u64, u64)>> {
    let mut segments = Vec::new();
    let mut current = Vec::new();
    for sample in samples {
        if let Some(delay_ms) = sample.delay_ms {
            current.push((sample.recorded_at_ms, delay_ms));
        } else if !current.is_empty() {
            segments.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        segments.push(current);
    }
    segments
}

fn latency_chart_threshold_line(x_max: f64, threshold_ms: u64) -> Vec<(f64, f64)> {
    vec![
        (0.0, threshold_ms as f64),
        (x_max.max(1.0), threshold_ms as f64),
    ]
}

fn latency_chart_y_bounds(min_y: f64, max_y: f64, threshold_ms: u64) -> [f64; 2] {
    let min_y = min_y.min(threshold_ms as f64);
    let max_y = max_y.max(threshold_ms as f64);
    let padding = ((max_y - min_y) * 0.05).max(10.0);
    [0.0_f64.max(min_y - padding), max_y + padding]
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latency_chart_helpers_define_the_visible_series() {
        let samples = vec![
            NodeLatencySample {
                recorded_at_ms: 0,
                delay_ms: Some(90),
            },
            NodeLatencySample {
                recorded_at_ms: 1,
                delay_ms: None,
            },
            NodeLatencySample {
                recorded_at_ms: 45 * 60 * 1000,
                delay_ms: Some(120),
            },
            NodeLatencySample {
                recorded_at_ms: 60 * 60 * 1000,
                delay_ms: Some(80),
            },
        ];

        assert_eq!(
            latency_chart_segments(&samples),
            vec![
                vec![(0, 90)],
                vec![(45 * 60 * 1000, 120), (60 * 60 * 1000, 80)]
            ]
        );
        assert_eq!(
            latency_chart_time_unit(Duration::from_secs(30 * 60)),
            LatencyChartTimeUnit::Minutes
        );
        assert_eq!(
            latency_chart_time_unit(Duration::from_secs(3 * 60 * 60)),
            LatencyChartTimeUnit::Hours
        );
        assert_eq!(
            latency_chart_zoom_in(Duration::from_secs(60 * 60)),
            Duration::from_secs(30 * 60)
        );
        assert_eq!(
            latency_chart_zoom_out(Duration::from_secs(60 * 60)),
            Duration::from_secs(2 * 60 * 60)
        );
        assert_eq!(
            latency_chart_threshold_line(30.0, 600),
            vec![(0.0, 600.0), (30.0, 600.0)]
        );

        let low_bounds = latency_chart_y_bounds(80.0, 120.0, 600);
        assert!(low_bounds[0] <= 80.0 && low_bounds[1] > 600.0);
        let high_bounds = latency_chart_y_bounds(700.0, 900.0, 600);
        assert!(high_bounds[0] < 600.0 && high_bounds[1] >= 900.0);

        let visible = latency_chart_windowed_samples(&samples, Duration::from_secs(30 * 60));
        assert_eq!(visible.len(), 2);
        assert_eq!(visible[0].recorded_at_ms, 45 * 60 * 1000);
        assert_eq!(visible[1].recorded_at_ms, 60 * 60 * 1000);
    }
}

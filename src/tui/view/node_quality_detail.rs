use super::*;
use crate::controller::ReachabilityAssessment;
use crate::storage::NodeQuickHistory;
use crate::tui::metrics::LatencySample;

#[derive(Clone, Debug)]
pub(crate) struct NodeQualityDetailState {
    pub(crate) selector: String,
    pub(crate) node: String,
    pub(crate) last_refresh: Instant,
    pub(crate) reachability_assessment: Option<NodeReachabilityAssessment>,
    pub(crate) quick_history: NodeQuickHistory,
    pub(crate) sustained_quality: Option<NodeSustainedQuality>,
    pub(crate) latency_history: Vec<LatencySample>,
    pub(crate) throughput_history: Vec<(i64, u64)>,
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

use ratatui::layout::{Alignment, Rect};

use crate::tui::ds::{Theme, dialog_content_area, render_dialog_frame};

pub(crate) fn draw_node_quality_detail(frame: &mut Frame, detail: &NodeQualityDetailState) {
    let area = frame.area();
    let theme = Theme::detect();
    let title = format!(
        " NODE QUALITY · {} ",
        truncate_for_width(&detail.node, area.width.saturating_sub(22) as usize)
    );
    render_dialog_frame(
        frame,
        area,
        &theme,
        &title,
        u16::MAX,
        u16::MAX,
        |frame, inner_area| {
            if inner_area.height == 0 || inner_area.width == 0 {
                return;
            }

            let content_area = dialog_content_area(inner_area);
            let (header_area, body_area, footer_area) = if content_area.height >= 5 {
                let gap = u16::from(content_area.height >= 14);
                let [h, _, b, f] = Layout::vertical([
                    Constraint::Length(1),
                    Constraint::Length(gap),
                    Constraint::Min(1),
                    Constraint::Length(1),
                ])
                .areas(content_area);
                (Some(h), b, Some(f))
            } else {
                (None, content_area, None)
            };

            if let Some(header) = header_area {
                let refresh = measurement_age_label(detail);
                let refresh_width = (unicode_width::UnicodeWidthStr::width(refresh.as_str())
                    as u16)
                    .min(header.width);
                let [selector_area, refresh_area] =
                    Layout::horizontal([Constraint::Min(1), Constraint::Length(refresh_width)])
                        .areas(header);
                frame.render_widget(
                    Paragraph::new(Line::from(vec![
                        Span::styled("Internet Proxy · ", theme.style_muted()),
                        Span::styled(
                            truncate_for_width(
                                &detail.selector,
                                selector_area.width.saturating_sub(17) as usize,
                            ),
                            theme.style_breadcrumb(),
                        ),
                    ]))
                    .style(theme.style_base()),
                    selector_area,
                );
                frame.render_widget(
                    Paragraph::new(refresh)
                        .alignment(Alignment::Right)
                        .style(theme.style_muted()),
                    refresh_area,
                );
            }

            render_node_quality_body(frame, &theme, body_area, detail);

            if let Some(footer) = footer_area {
                let footer_line = Line::from(vec![
                    Span::styled("[Esc/i/Enter]", theme.style_footer_keys()),
                    Span::raw(" "),
                    Span::styled("Close", theme.style_muted()),
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

fn measurement_age_label(detail: &NodeQualityDetailState) -> String {
    let latest = detail
        .latency_history
        .iter()
        .map(|sample| sample.recorded_at_ms)
        .chain(detail.throughput_history.iter().map(|sample| sample.0))
        .max();
    if let Some(recorded_at_ms) = latest {
        let age_seconds = crate::tui::metrics::now_unix_ms()
            .saturating_sub(recorded_at_ms)
            .max(0) as u64
            / 1_000;
        let age = match age_seconds {
            0..=59 => format!("{age_seconds}s"),
            60..=3_599 => format!("{}m", age_seconds / 60),
            _ => format!("{}h", age_seconds / 3_600),
        };
        format!("MEASURED · {age} AGO")
    } else if detail.reachability_assessment.is_some() || detail.sustained_quality.is_some() {
        "MEASURED · AGE UNAVAILABLE".to_string()
    } else {
        "NOT MEASURED".to_string()
    }
}

fn render_node_quality_body(
    frame: &mut Frame,
    theme: &Theme,
    area: Rect,
    detail: &NodeQualityDetailState,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let left_width = (area.width * 35 / 100).clamp(24, 38).min(area.width);
    let [left, _, right] = Layout::horizontal([
        Constraint::Length(left_width),
        Constraint::Length(u16::from(area.width >= 50)),
        Constraint::Min(0),
    ])
    .areas(area);

    let reachability_height = left.height.min(7);
    let sustained_height = left.height.saturating_sub(reachability_height + 1).min(7);
    let [reachability, _, sustained, remaining] = Layout::vertical([
        Constraint::Length(reachability_height),
        Constraint::Length(u16::from(left.height > reachability_height)),
        Constraint::Length(sustained_height),
        Constraint::Min(0),
    ])
    .areas(left);
    render_reachability_card(frame, theme, reachability, detail);
    if sustained.height > 0 {
        render_sustained_card(frame, theme, sustained, detail);
    }
    if remaining.height >= 3 {
        render_detail_evidence(frame, theme, remaining, detail);
    }
    render_latency_chart(frame, theme, right, detail);
}

fn quality_block<'a>(title: &'a str, theme: &Theme) -> Block<'a> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(theme.style_muted())
        .title(Span::styled(format!(" {title} "), theme.style_breadcrumb()))
        .style(theme.style_base())
}

fn render_reachability_card(
    frame: &mut Frame,
    theme: &Theme,
    area: Rect,
    detail: &NodeQualityDetailState,
) {
    let block = quality_block("REACHABILITY", theme);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 {
        return;
    }
    let (assessment, style) = match detail
        .reachability_assessment
        .as_ref()
        .and_then(|a| a.assessment)
    {
        Some(ReachabilityAssessment::StableReachable) => {
            ("STABLE REACHABLE", theme.style_success())
        }
        Some(ReachabilityAssessment::Reachable) => ("REACHABLE", theme.style_success()),
        Some(ReachabilityAssessment::Degraded) => ("DEGRADED", theme.style_warning()),
        Some(ReachabilityAssessment::Unreachable) => ("UNREACHABLE", theme.style_error()),
        None => ("NOT MEASURED", theme.style_muted()),
    };
    let attempts = detail
        .reachability_assessment
        .as_ref()
        .map_or(0, |assessment| assessment.attempts.len());
    let completed = detail
        .reachability_assessment
        .as_ref()
        .map_or(0, |assessment| {
            assessment
                .attempts
                .iter()
                .filter(|outcome| matches!(outcome, ProbeOutcome::Reachable { .. }))
                .count()
        });
    let lines = vec![
        Line::from(Span::styled(assessment, style)),
        metric_row(
            "Assessment",
            format!("{completed} / {attempts} complete"),
            theme,
            inner.width,
        ),
        metric_row(
            "Cold-start med.",
            spaced_metric(detail.quick_history.cold_start_ms),
            theme,
            inner.width,
        ),
        metric_row(
            "Warm median",
            spaced_metric(detail.quick_history.warm_median_ms),
            theme,
            inner.width,
        ),
        metric_row(
            "P95",
            spaced_metric(detail.quick_history.p95_ms),
            theme,
            inner.width,
        ),
    ];
    frame.render_widget(Paragraph::new(lines).style(theme.style_base()), inner);
}

fn render_sustained_card(
    frame: &mut Frame,
    theme: &Theme,
    area: Rect,
    detail: &NodeQualityDetailState,
) {
    let block = quality_block("SUSTAINED QUALITY", theme);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 {
        return;
    }
    let lines = match detail
        .sustained_quality
        .as_ref()
        .map(|quality| &quality.outcome)
    {
        Some(SustainedProbeOutcome::Completed(completion)) => vec![
            Line::from(Span::styled("COMPLETED", theme.style_success())),
            metric_row(
                "First byte",
                format!("{} ms", completion.first_byte_ms),
                theme,
                inner.width,
            ),
            metric_row(
                "Completion",
                format!("{} ms", completion.completion_ms),
                theme,
                inner.width,
            ),
            metric_row(
                "Throughput",
                format!(
                    "{:.1} MiB/s",
                    completion.throughput_bytes_per_second as f64 / 1_048_576.0
                ),
                theme,
                inner.width,
            ),
            metric_row(
                "Transferred",
                format_bytes_opt(Some(completion.bytes_read)),
                theme,
                inner.width,
            ),
        ],
        Some(SustainedProbeOutcome::TransferFailed { detail }) => vec![
            Line::from(Span::styled("TRANSFER FAILED", theme.style_error())),
            Line::from(Span::styled(
                truncate_for_width(detail, inner.width as usize),
                theme.style_muted(),
            )),
        ],
        Some(SustainedProbeOutcome::RuntimeFailed { detail }) => vec![
            Line::from(Span::styled("RUNTIME FAILED", theme.style_error())),
            Line::from(Span::styled(
                truncate_for_width(detail, inner.width as usize),
                theme.style_muted(),
            )),
        ],
        Some(SustainedProbeOutcome::Cancelled) => {
            vec![Line::from(Span::styled("CANCELLED", theme.style_warning()))]
        }
        None => vec![Line::from(Span::styled(
            "NOT MEASURED",
            theme.style_muted(),
        ))],
    };
    frame.render_widget(Paragraph::new(lines).style(theme.style_base()), inner);
}

fn metric_row(label: &str, value: String, theme: &Theme, width: u16) -> Line<'static> {
    let value_width = unicode_width::UnicodeWidthStr::width(value.as_str());
    let label_width = (width as usize).saturating_sub(value_width);
    let label = truncate_for_width(&format!("{label}:"), label_width.saturating_sub(1));
    let padding = label_width.saturating_sub(unicode_width::UnicodeWidthStr::width(label.as_str()));
    Line::from(vec![
        Span::styled(
            format!("{label}{}", " ".repeat(padding)),
            theme.style_muted(),
        ),
        Span::styled(value, theme.style_base()),
    ])
}

fn spaced_metric(value: Option<u64>) -> String {
    value
        .map(|milliseconds| format!("{milliseconds} ms"))
        .unwrap_or_else(|| "—".to_string())
}

fn render_latency_chart(
    frame: &mut Frame,
    theme: &Theme,
    area: Rect,
    detail: &NodeQualityDetailState,
) {
    let block = quality_block("30 MIN HISTORY · LATENCY ms / THROUGHPUT MiB/s", theme);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 {
        return;
    }
    if inner.height < 5 || inner.width < 20 {
        frame.render_widget(
            Paragraph::new("History needs more space").style(theme.style_muted()),
            inner,
        );
        return;
    }

    let now_ms = crate::tui::metrics::now_unix_ms();
    let cutoff_ms = now_ms.saturating_sub(crate::tui::metrics::METRIC_RETENTION_WINDOW_MS);
    let latency = history_series(
        &detail.latency_history,
        cutoff_ms,
        |sample| sample.recorded_at_ms,
        |sample| sample.latency_ms as f64,
        crate::tui::metrics::LATENCY_OBSERVATION_GAP_THRESHOLD_MS,
    );
    let throughput = history_series(
        &detail.throughput_history,
        cutoff_ms,
        |sample| sample.0,
        |sample| sample.1 as f64 / 1_048_576.0,
        crate::tui::metrics::TRAFFIC_OBSERVATION_GAP_THRESHOLD_MS,
    );
    if latency.is_empty() && throughput.is_empty() {
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    "No recorded 30-minute history",
                    theme.style_muted(),
                )),
                Line::from(Span::styled(
                    "Latest probe evidence is shown below",
                    theme.style_muted(),
                )),
            ])
            .style(theme.style_base()),
            inner,
        );
        return;
    }

    render_history_plot(frame, theme, inner, &latency, &throughput);
}

fn history_series<T>(
    samples: &[T],
    cutoff_ms: i64,
    timestamp: impl Fn(&T) -> i64,
    value: impl Fn(&T) -> f64,
    gap_threshold_ms: i64,
) -> Vec<Vec<(f64, f64)>> {
    let mut series = Vec::new();
    let mut current = Vec::new();
    let mut previous_at = None;
    for sample in samples {
        let recorded_at_ms = timestamp(sample);
        if recorded_at_ms < cutoff_ms {
            continue;
        }
        if previous_at.is_some_and(|previous| recorded_at_ms - previous > gap_threshold_ms)
            && !current.is_empty()
        {
            series.push(std::mem::take(&mut current));
        }
        current.push((
            ((recorded_at_ms - cutoff_ms) as f64 / 60_000.0).clamp(0.0, 30.0),
            value(sample),
        ));
        previous_at = Some(recorded_at_ms);
    }
    if !current.is_empty() {
        series.push(current);
    }
    series
}

fn render_history_plot(
    frame: &mut Frame,
    theme: &Theme,
    area: Rect,
    latency: &[Vec<(f64, f64)>],
    throughput: &[Vec<(f64, f64)>],
) {
    use ratatui::style::Style;
    use ratatui::symbols::Marker;
    use ratatui::widgets::{Axis, Chart, Dataset, GraphType};

    if area.width == 0 || area.height == 0 {
        return;
    }
    let latency_max = latency
        .iter()
        .flatten()
        .map(|(_, value)| *value)
        .fold(120.0_f64, f64::max);
    let throughput_max = throughput
        .iter()
        .flatten()
        .map(|(_, value)| *value)
        .fold(1.0_f64, f64::max);
    let normalized_latency = normalize_history_series(latency, latency_max);
    let normalized_throughput = normalize_history_series(throughput, throughput_max);
    let mut datasets = normalized_latency
        .iter()
        .map(|points| {
            Dataset::default()
                .data(points)
                .graph_type(if points.len() > 1 {
                    GraphType::Line
                } else {
                    GraphType::Scatter
                })
                .marker(Marker::Braille)
                .style(Style::default().fg(theme.text_latency()))
        })
        .collect::<Vec<_>>();
    datasets.extend(normalized_throughput.iter().map(|points| {
        Dataset::default()
            .data(points)
            .graph_type(if points.len() > 1 {
                GraphType::Line
            } else {
                GraphType::Scatter
            })
            .marker(Marker::Braille)
            .style(Style::default().fg(theme.text_success()))
    }));
    let chart_area = Rect::new(
        area.x.saturating_add(6),
        area.y,
        area.width.saturating_sub(14),
        area.height.saturating_sub(1),
    );
    frame.render_widget(
        Chart::new(datasets)
            .x_axis(Axis::default().bounds([0.0, 30.0]))
            .y_axis(Axis::default().bounds([0.0, 1.0]))
            .style(Style::default()),
        chart_area,
    );
    frame.render_widget(
        Paragraph::new(format!("{latency_max:.0} ms"))
            .style(Style::default().fg(theme.text_latency())),
        Rect::new(area.x, area.y, 6.min(area.width), 1),
    );
    frame.render_widget(
        Paragraph::new(format!("{throughput_max:.1}"))
            .alignment(Alignment::Right)
            .style(Style::default().fg(theme.text_success())),
        Rect::new(area.right().saturating_sub(7), area.y, 7.min(area.width), 1),
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("latency", Style::default().fg(theme.text_latency())),
            Span::raw("  "),
            Span::styled("throughput", Style::default().fg(theme.text_success())),
        ]))
        .alignment(Alignment::Center),
        Rect::new(chart_area.x, area.y, chart_area.width, 1),
    );
    frame.render_widget(
        Paragraph::new("-30m              -15m               now")
            .alignment(Alignment::Center)
            .style(theme.style_muted()),
        Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1),
    );
}

fn normalize_history_series(series: &[Vec<(f64, f64)>], maximum: f64) -> Vec<Vec<(f64, f64)>> {
    series
        .iter()
        .map(|points| {
            points
                .iter()
                .map(|(minute, value)| (*minute, (*value / maximum).clamp(0.0, 1.0)))
                .collect()
        })
        .collect()
}

fn render_detail_evidence(
    frame: &mut Frame,
    theme: &Theme,
    area: Rect,
    detail: &NodeQualityDetailState,
) {
    let block = quality_block("EVIDENCE", theme);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height > 0 {
        frame.render_widget(
            Paragraph::new(node_quality_evidence_lines(detail))
                .scroll((detail.evidence_scroll, 0))
                .wrap(Wrap { trim: false })
                .style(theme.style_base()),
            inner,
        );
    }
}

fn node_quality_evidence_lines(detail: &NodeQualityDetailState) -> Vec<Line<'static>> {
    let theme = Theme::detect();
    let mut lines = Vec::new();
    if let Some(explanation) = &detail.auto_selection_detail {
        lines.push(Line::from(vec![
            Span::styled("Automatic selection: ", theme.style_breadcrumb()),
            Span::styled(truncate_for_width(explanation, 96), theme.style_base()),
        ]));
    }
    if let Some(assessment) = &detail.reachability_assessment {
        let (assess_text, assess_style) = match assessment.assessment {
            Some(ReachabilityAssessment::StableReachable)
            | Some(ReachabilityAssessment::Reachable) => {
                (assessment.compact_evidence(), theme.style_success())
            }
            Some(ReachabilityAssessment::Degraded) => {
                (assessment.compact_evidence(), theme.style_warning())
            }
            Some(ReachabilityAssessment::Unreachable) => {
                (assessment.compact_evidence(), theme.style_danger())
            }
            None => (assessment.compact_evidence(), theme.style_muted()),
        };
        lines.push(Line::from(vec![
            Span::styled("Reachability assessment: ", theme.style_breadcrumb()),
            Span::styled(assess_text, assess_style),
        ]));
        for (index, outcome) in assessment.attempts.iter().enumerate() {
            let (label, outcome_style) = match outcome {
                ProbeOutcome::Reachable { .. } => {
                    (probe_outcome_label(outcome), theme.style_success())
                }
                ProbeOutcome::Timeout => (probe_outcome_label(outcome), theme.style_warning()),
                ProbeOutcome::TransportFailure { .. } => {
                    (probe_outcome_label(outcome), theme.style_danger())
                }
                ProbeOutcome::Cancelled
                | ProbeOutcome::ControllerFailure { .. }
                | ProbeOutcome::InvalidMeasurement => {
                    (probe_outcome_label(outcome), theme.style_warning())
                }
            };
            lines.push(Line::from(vec![
                Span::styled(
                    format!("Probe attempt {}: ", index + 1),
                    theme.style_muted(),
                ),
                Span::styled(label, outcome_style),
            ]));
        }
    } else {
        lines.push(Line::from(vec![
            Span::styled("Reachability assessment: ", theme.style_breadcrumb()),
            Span::styled("untested", theme.style_muted()),
        ]));
    }
    if detail.quick_history.rounds == 0 {
        lines.push(Line::from(vec![
            Span::styled("Recent quick success: ", theme.style_muted()),
            Span::styled("untested", theme.style_muted()),
        ]));
    } else {
        lines.push(Line::from(vec![
            Span::styled("Recent quick success: ", theme.style_muted()),
            Span::styled(
                format!(
                    "{}/{} rounds",
                    detail.quick_history.successful_rounds, detail.quick_history.rounds
                ),
                theme.style_base(),
            ),
        ]));
        lines.push(Line::from(vec![
            Span::styled("Quick history: ", theme.style_breadcrumb()),
            Span::styled(
                format!(
                    "warm median {}  P95 {}  cold-start {}",
                    metric_label(detail.quick_history.warm_median_ms),
                    metric_label(detail.quick_history.p95_ms),
                    metric_label(detail.quick_history.cold_start_ms),
                ),
                theme.style_base(),
            ),
        ]));
    }
    if let Some(sustained) = &detail.sustained_quality {
        match &sustained.outcome {
            SustainedProbeOutcome::Completed(completion) => {
                lines.push(Line::from(vec![
                    Span::styled("Sustained quality: ", theme.style_breadcrumb()),
                    Span::styled(
                        format!(
                            "{:.1} MiB/s, {} bytes",
                            completion.throughput_bytes_per_second as f64 / (1024.0 * 1024.0),
                            completion.bytes_read
                        ),
                        theme.style_success(),
                    ),
                ]));
                lines.push(Line::from(vec![Span::styled(
                    format!(
                        "First byte: {}ms  Completion: {}ms",
                        completion.first_byte_ms, completion.completion_ms
                    ),
                    theme.style_base(),
                )]));
            }
            SustainedProbeOutcome::TransferFailed { detail } => {
                lines.push(Line::from(vec![
                    Span::styled("Sustained quality: ", theme.style_breadcrumb()),
                    Span::styled(
                        format!("transfer failed ({})", truncate_for_width(detail, 72)),
                        theme.style_danger(),
                    ),
                ]));
            }
            SustainedProbeOutcome::RuntimeFailed { detail } => {
                lines.push(Line::from(vec![
                    Span::styled("Sustained quality: ", theme.style_breadcrumb()),
                    Span::styled(
                        format!("runtime failed ({})", truncate_for_width(detail, 72)),
                        theme.style_danger(),
                    ),
                ]));
            }
            SustainedProbeOutcome::Cancelled => {
                lines.push(Line::from(vec![
                    Span::styled("Sustained quality: ", theme.style_breadcrumb()),
                    Span::styled("cancelled", theme.style_warning()),
                ]));
            }
        }
    } else {
        lines.push(Line::from(vec![
            Span::styled("Sustained quality: ", theme.style_breadcrumb()),
            Span::styled("untested", theme.style_muted()),
        ]));
    }
    for criterion in &detail.usability_details {
        if let Some(usable) = criterion.usable {
            let (status_text, status_style) = if usable {
                ("usable", theme.style_success())
            } else {
                ("rejected", theme.style_danger())
            };
            let mut spans = vec![
                Span::styled(
                    format!("{} usability criterion: ", criterion.label),
                    theme.style_breadcrumb(),
                ),
                Span::styled(status_text, status_style),
            ];
            if let Some(value) = criterion.detail.as_deref() {
                spans.push(Span::styled(
                    format!(" ({})", truncate_for_width(value, 56)),
                    theme.style_muted(),
                ));
            }
            lines.push(Line::from(spans));
            if criterion.expired {
                lines.push(Line::from(vec![Span::styled(
                    format!(
                        "{} result: expired (excluded from candidates)",
                        criterion.label
                    ),
                    theme.style_warning(),
                )]));
            }
        }
        if let Some(failure) = &criterion.latest_failure {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{} latest probe attempt: ", criterion.label),
                    theme.style_muted(),
                ),
                Span::styled(truncate_for_width(failure, 72), theme.style_danger()),
            ]));
        }
    }
    lines
}

pub(crate) fn node_quality_detail_line_count(detail: &NodeQualityDetailState) -> usize {
    node_quality_evidence_lines(detail)
        .iter()
        .map(|line| {
            let width = unicode_width::UnicodeWidthStr::width(line.to_string().as_str());
            width.max(1).div_ceil(22)
        })
        .sum()
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

fn metric_label(value: Option<u64>) -> String {
    value
        .map(|milliseconds| format!("{milliseconds}ms"))
        .unwrap_or_else(|| "unavailable".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::{ProbeOutcome, ReachabilityAssessment};

    #[test]
    fn history_series_breaks_lines_across_observation_gaps() {
        let samples = [0_i64, 10_000, 30_001];
        let series = history_series(&samples, 0, |sample| *sample, |_| 1.0, 15_000);

        assert_eq!(series.len(), 2);
        assert_eq!(series[0].len(), 2);
        assert_eq!(series[1].len(), 1);
    }

    #[test]
    fn detail_uses_node_quality_vocabulary_and_keeps_every_probe_outcome() {
        let detail = NodeQualityDetailState {
            selector: "select".into(),
            node: "node-a".into(),
            last_refresh: Instant::now(),
            reachability_assessment: Some(NodeReachabilityAssessment {
                name: "node-a".into(),
                attempts: vec![
                    ProbeOutcome::Reachable { delay_ms: 40 },
                    ProbeOutcome::Timeout,
                ],
                assessment: Some(ReachabilityAssessment::Degraded),
            }),
            quick_history: NodeQuickHistory {
                successful_rounds: 4,
                rounds: 5,
                warm_median_ms: Some(40),
                p95_ms: Some(90),
                cold_start_ms: Some(75),
            },
            sustained_quality: None,
            latency_history: vec![
                LatencySample {
                    recorded_at_ms: crate::tui::metrics::now_unix_ms() - 60_000,
                    selector: "select".into(),
                    node_name: "node-a".into(),
                    latency_ms: 40,
                },
                LatencySample {
                    recorded_at_ms: crate::tui::metrics::now_unix_ms(),
                    selector: "select".into(),
                    node_name: "node-a".into(),
                    latency_ms: 51,
                },
            ],
            throughput_history: vec![(crate::tui::metrics::now_unix_ms(), 2 * 1_048_576)],
            auto_selection_detail: None,
            usability_details: Vec::new(),
            evidence_scroll: 0,
        };
        let lines = node_quality_evidence_lines(&detail)
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>();
        assert!(
            lines
                .iter()
                .any(|line| line.contains("Reachability assessment"))
        );
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.contains("Probe attempt"))
                .count(),
            2
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("Sustained quality: untested"))
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("Recent quick success: 4/5 rounds"))
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("P95 90ms") && line.contains("cold-start 75ms"))
        );
    }

    #[test]
    fn node_quality_detail_renders_dialog_frame_and_hints() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        let detail = NodeQualityDetailState {
            selector: "select".into(),
            node: "node-a".into(),
            last_refresh: Instant::now(),
            reachability_assessment: Some(NodeReachabilityAssessment {
                name: "node-a".into(),
                attempts: vec![
                    ProbeOutcome::Reachable { delay_ms: 40 },
                    ProbeOutcome::Timeout,
                ],
                assessment: Some(ReachabilityAssessment::Degraded),
            }),
            quick_history: NodeQuickHistory {
                successful_rounds: 4,
                rounds: 5,
                warm_median_ms: Some(40),
                p95_ms: Some(90),
                cold_start_ms: Some(75),
            },
            sustained_quality: None,
            latency_history: vec![
                LatencySample {
                    recorded_at_ms: crate::tui::metrics::now_unix_ms() - 60_000,
                    selector: "select".into(),
                    node_name: "node-a".into(),
                    latency_ms: 40,
                },
                LatencySample {
                    recorded_at_ms: crate::tui::metrics::now_unix_ms(),
                    selector: "select".into(),
                    node_name: "node-a".into(),
                    latency_ms: 51,
                },
            ],
            throughput_history: vec![(crate::tui::metrics::now_unix_ms(), 2 * 1_048_576)],
            auto_selection_detail: None,
            usability_details: Vec::new(),
            evidence_scroll: 0,
        };

        terminal
            .draw(|f| {
                draw_node_quality_detail(f, &detail);
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

        assert!(text.contains("NODE QUALITY · node-a"));
        assert!(text.contains("Internet Proxy · select"));
        assert!(text.contains("REACHABILITY"));
        assert!(text.contains("SUSTAINED QUALITY"));
        assert!(text.contains("30 MIN HISTORY"));
        assert!(text.contains("LAT"));
        assert!(text.contains("MiB/s"));
        assert!(text.contains("120 ms"));
        assert!(text.contains("2.0"));
        assert!(text.contains("MEASURED"));
        assert!(text.contains("Probe attempt 1: reachable (40ms)"));
        assert!(text.contains("Probe attempt 2: timeout"));
        assert!(text.contains("P95"));
        assert!(text.contains("75 ms"));
        assert!(text.contains("[Esc/i/Enter] Close"));
    }

    #[test]
    fn compact_quality_layout_keeps_metrics_and_footer_clear_of_long_node_label() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let detail = NodeQualityDetailState {
            selector: "selector-超长-👩‍💻".into(),
            node: "👩‍💻-超长节点名-that-must-not-displace-values".into(),
            last_refresh: Instant::now(),
            reachability_assessment: Some(NodeReachabilityAssessment {
                name: "node-a".into(),
                attempts: vec![ProbeOutcome::Reachable { delay_ms: 54 }],
                assessment: Some(ReachabilityAssessment::Reachable),
            }),
            quick_history: NodeQuickHistory {
                successful_rounds: 3,
                rounds: 3,
                warm_median_ms: Some(54),
                p95_ms: Some(68),
                cold_start_ms: Some(412),
            },
            sustained_quality: None,
            latency_history: Vec::new(),
            throughput_history: Vec::new(),
            auto_selection_detail: None,
            usability_details: Vec::new(),
            evidence_scroll: 0,
        };

        terminal
            .draw(|f| draw_node_quality_detail(f, &detail))
            .unwrap();
        let text = buffer_to_text(terminal.backend().buffer());

        assert!(text.contains("👩‍💻"));
        assert!(!text.contains('\u{fffd}'));
        assert!(text.contains("54 ms"));
        assert!(text.contains("68 ms"));
        assert!(text.contains("412 ms"));
        assert!(text.contains("[Esc/i/Enter] Close"));
    }

    #[test]
    fn compact_quality_card_reserves_space_for_large_metric_values() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let detail = NodeQualityDetailState {
            selector: "select".into(),
            node: "node-a".into(),
            last_refresh: Instant::now(),
            reachability_assessment: None,
            quick_history: NodeQuickHistory {
                cold_start_ms: Some(123_456),
                warm_median_ms: Some(2_345),
                p95_ms: Some(67),
                ..NodeQuickHistory::default()
            },
            sustained_quality: None,
            latency_history: Vec::new(),
            throughput_history: Vec::new(),
            auto_selection_detail: None,
            usability_details: Vec::new(),
            evidence_scroll: 0,
        };

        terminal
            .draw(|frame| draw_node_quality_detail(frame, &detail))
            .unwrap();
        let text = buffer_to_text(terminal.backend().buffer());
        assert!(text.contains("123456 ms"), "{text}");
        assert!(text.contains("2345 ms"), "{text}");
        assert!(text.contains("67 ms"), "{text}");
        let metric_rows = ["123456 ms", "2345 ms", "67 ms"].map(|metric| {
            let row = text.lines().find(|row| row.contains(metric)).unwrap();
            let end = row.find(metric).unwrap() + metric.len();
            unicode_width::UnicodeWidthStr::width(&row[..end])
        });
        assert_eq!(metric_rows, [metric_rows[0]; 3]);
    }

    #[test]
    fn unmeasured_quality_state_keeps_units_and_empty_state_vocabulary() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let backend = TestBackend::new(137, 35);
        let mut terminal = Terminal::new(backend).unwrap();
        let detail = NodeQualityDetailState {
            selector: "select".into(),
            node: "node-a".into(),
            last_refresh: Instant::now(),
            reachability_assessment: None,
            quick_history: NodeQuickHistory::default(),
            sustained_quality: None,
            latency_history: Vec::new(),
            throughput_history: Vec::new(),
            auto_selection_detail: None,
            usability_details: Vec::new(),
            evidence_scroll: 0,
        };

        terminal
            .draw(|f| draw_node_quality_detail(f, &detail))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let text = buffer_to_text(buffer);

        assert!(text.contains("NOT MEASURED"));
        assert!(text.contains("30 MIN HISTORY"));
        assert!(text.contains("No recorded 30-minute history"));
        assert_eq!(buffer[(0, 0)].symbol(), "┌");
        assert_eq!(buffer[(136, 34)].symbol(), "┘");
        assert!(row_text(buffer, 33).contains("[Esc/i/Enter] Close"));
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

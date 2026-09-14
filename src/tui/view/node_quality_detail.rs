use super::*;
use crate::controller::ReachabilityAssessment;
use crate::storage::NodeQuickHistory;

#[derive(Clone, Debug)]
pub(crate) struct NodeQualityDetailState {
    pub(crate) selector: String,
    pub(crate) node: String,
    pub(crate) last_refresh: Instant,
    pub(crate) reachability_assessment: Option<NodeReachabilityAssessment>,
    pub(crate) quick_history: NodeQuickHistory,
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

use crate::tui::ds::{render_dialog_frame, Theme};

pub(crate) fn draw_node_quality_detail(frame: &mut Frame, detail: &NodeQualityDetailState) {
    let area = frame.area();
    let theme = Theme::detect();
    render_dialog_frame(
        frame,
        area,
        &theme,
        " NODE QUALITY DETAIL (i) ",
        90,
        22,
        |frame, inner_area| {
            if inner_area.height == 0 || inner_area.width == 0 {
                return;
            }

            let (header_area, evidence_area, footer_area) = if inner_area.height >= 4 {
                let [h, e, f] = Layout::vertical([
                    Constraint::Length(1),
                    Constraint::Min(1),
                    Constraint::Length(1),
                ])
                .areas(inner_area);
                (Some(h), e, Some(f))
            } else if inner_area.height >= 2 {
                let [e, f] = Layout::vertical([
                    Constraint::Min(1),
                    Constraint::Length(1),
                ])
                .areas(inner_area);
                (None, e, Some(f))
            } else {
                (None, inner_area, None)
            };

            if let Some(header) = header_area {
                let header_line = Line::from(vec![
                    Span::styled("Node: ", theme.style_muted()),
                    Span::styled(
                        truncate_for_width(&detail.node, 36),
                        theme.style_breadcrumb(),
                    ),
                    Span::raw("   "),
                    Span::styled("Selector: ", theme.style_muted()),
                    Span::styled(&detail.selector, theme.style_breadcrumb()),
                ]);
                frame.render_widget(
                    Paragraph::new(header_line).style(theme.style_base()),
                    header,
                );
            }

            frame.render_widget(
                Paragraph::new(node_quality_evidence_lines(detail))
                    .scroll((detail.evidence_scroll, 0))
                    .style(theme.style_base()),
                evidence_area,
            );

            if let Some(footer) = footer_area {
                let footer_line = Line::from(vec![
                    Span::styled("[Esc/i]", theme.style_footer_keys()),
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
            Some(ReachabilityAssessment::StableReachable) | Some(ReachabilityAssessment::Reachable) => {
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
                ProbeOutcome::Reachable { .. } => (probe_outcome_label(outcome), theme.style_success()),
                ProbeOutcome::Timeout => (probe_outcome_label(outcome), theme.style_warning()),
                ProbeOutcome::TransportFailure { .. } => (probe_outcome_label(outcome), theme.style_danger()),
                ProbeOutcome::Cancelled
                | ProbeOutcome::ControllerFailure { .. }
                | ProbeOutcome::InvalidMeasurement => (probe_outcome_label(outcome), theme.style_warning()),
            };
            lines.push(Line::from(vec![
                Span::styled(format!("Probe attempt {}: ", index + 1), theme.style_muted()),
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
                format!("{}/{} rounds", detail.quick_history.successful_rounds, detail.quick_history.rounds),
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
                lines.push(Line::from(vec![
                    Span::styled(
                        format!(
                            "First byte: {}ms  Completion: {}ms",
                            completion.first_byte_ms, completion.completion_ms
                        ),
                        theme.style_base(),
                    ),
                ]));
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
                Span::styled(format!("{} usability criterion: ", criterion.label), theme.style_breadcrumb()),
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
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{} result: expired (excluded from candidates)", criterion.label),
                        theme.style_warning(),
                    ),
                ]));
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
    node_quality_evidence_lines(detail).len()
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
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let backend = TestBackend::new(100, 30);
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

        assert!(text.contains("NODE QUALITY DETAIL (i)"));
        assert!(text.contains("Node: node-a"));
        assert!(text.contains("Selector: select"));
        assert!(text.contains("Probe attempt 1: reachable (40ms)"));
        assert!(text.contains("Probe attempt 2: timeout"));
        assert!(text.contains("cold-start 75ms"));
        assert!(text.contains("[Esc/i] Close"));
    }
}

use super::*;

#[derive(Clone, Debug)]
pub(crate) struct NodeQualityDetailState {
    pub(crate) selector: String,
    pub(crate) node: String,
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

pub(crate) fn draw_node_quality_detail(frame: &mut Frame, detail: &NodeQualityDetailState) {
    let area = centered_rect(90, 20, frame.area());
    frame.render_widget(Clear, area);
    let title = format!(
        "Node quality: {} / {} (j/k scroll)",
        detail.selector,
        truncate_for_width(&detail.node, 36)
    );
    frame.render_widget(
        Paragraph::new(node_quality_evidence_lines(detail))
            .scroll((detail.evidence_scroll, 0))
            .block(
                Block::default()
                    .title(title)
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan)),
            ),
        area,
    );
}

fn node_quality_evidence_lines(detail: &NodeQualityDetailState) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(explanation) = &detail.auto_selection_detail {
        lines.push(Line::from(format!(
            "Automatic selection: {}",
            truncate_for_width(explanation, 96)
        )));
    }
    if let Some(assessment) = &detail.reachability_assessment {
        lines.push(Line::from(format!(
            "Reachability assessment: {}",
            assessment.compact_evidence()
        )));
        for (index, outcome) in assessment.attempts.iter().enumerate() {
            lines.push(Line::from(format!(
                "Probe attempt {}: {}",
                index + 1,
                probe_outcome_label(outcome)
            )));
        }
    } else {
        lines.push(Line::from("Reachability assessment: untested"));
    }
    if let Some(sustained) = &detail.sustained_quality {
        match &sustained.outcome {
            SustainedProbeOutcome::Completed(completion) => {
                lines.push(Line::from(format!(
                    "Sustained quality: {:.1} MiB/s, {} bytes",
                    completion.throughput_bytes_per_second as f64 / (1024.0 * 1024.0),
                    completion.bytes_read
                )));
                lines.push(Line::from(format!(
                    "First byte: {}ms  Completion: {}ms",
                    completion.first_byte_ms, completion.completion_ms
                )));
            }
            SustainedProbeOutcome::TransferFailed { detail } => lines.push(Line::from(format!(
                "Sustained quality: transfer failed ({})",
                truncate_for_width(detail, 72)
            ))),
            SustainedProbeOutcome::RuntimeFailed { detail } => lines.push(Line::from(format!(
                "Sustained quality: runtime failed ({})",
                truncate_for_width(detail, 72)
            ))),
            SustainedProbeOutcome::Cancelled => {
                lines.push(Line::from("Sustained quality: cancelled"));
            }
        }
    } else {
        lines.push(Line::from("Sustained quality: untested"));
    }
    for criterion in &detail.usability_details {
        if let Some(usable) = criterion.usable {
            lines.push(Line::from(format!(
                "{} usability criterion: {}{}",
                criterion.label,
                if usable { "usable" } else { "rejected" },
                criterion
                    .detail
                    .as_deref()
                    .map(|value| format!(" ({})", truncate_for_width(value, 56)))
                    .unwrap_or_default()
            )));
            if criterion.expired {
                lines.push(Line::from(format!(
                    "{} result: expired (excluded from candidates)",
                    criterion.label
                )));
            }
        }
        if let Some(failure) = &criterion.latest_failure {
            lines.push(Line::from(format!(
                "{} latest probe attempt: {}",
                criterion.label,
                truncate_for_width(failure, 72)
            )));
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
    }
}

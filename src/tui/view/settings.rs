use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SettingsField {
    BenchmarkUrl,
    BenchmarkTimeoutMs,
    RequestTimeoutSec,
    MaxConcurrency,
    VerifyTargets,
    AutoPickThresholdMs,
    AutoPickIntervalSec,
    SystemProxyServer,
    ChinaIpRouting,
    PrivateAccessProfile,
    PrivateAccessManifestPath,
    PrivateAccessMode,
    PrivateAccessServer,
    PrivateAccessPort,
    PrivateAccessUsername,
    PrivateAccessPassword,
    PrivateAccessPasswordEnv,
    PrivateAccessBridgeListen,
    PrivateAccessUseInternetProxy,
    PrivateAccessTlsVerify,
}

pub(crate) const SETTINGS_FIELDS: &[SettingsField] = &[
    SettingsField::BenchmarkUrl,
    SettingsField::BenchmarkTimeoutMs,
    SettingsField::RequestTimeoutSec,
    SettingsField::MaxConcurrency,
    SettingsField::VerifyTargets,
    SettingsField::AutoPickThresholdMs,
    SettingsField::AutoPickIntervalSec,
    SettingsField::SystemProxyServer,
    SettingsField::ChinaIpRouting,
    SettingsField::PrivateAccessProfile,
    SettingsField::PrivateAccessManifestPath,
    SettingsField::PrivateAccessMode,
    SettingsField::PrivateAccessServer,
    SettingsField::PrivateAccessPort,
    SettingsField::PrivateAccessUsername,
    SettingsField::PrivateAccessPassword,
    SettingsField::PrivateAccessPasswordEnv,
    SettingsField::PrivateAccessBridgeListen,
    SettingsField::PrivateAccessUseInternetProxy,
    SettingsField::PrivateAccessTlsVerify,
];

pub(crate) struct SettingsEditState {
    pub(crate) field: SettingsField,
    pub(crate) input: String,
    pub(crate) error: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SettingRow {
    pub(crate) label: &'static str,
    pub(crate) value: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SettingsPanelSnapshot {
    pub(crate) rows: Vec<SettingRow>,
    pub(crate) selected: usize,
    pub(crate) editing: Option<(&'static str, String)>,
    pub(crate) error: Option<String>,
}

pub(crate) fn draw_settings_panel(frame: &mut Frame, settings: &SettingsPanelSnapshot) {
    let frame_area = frame.area();
    let area = centered_rect(96, 26, frame_area);
    frame.render_widget(Clear, area);
    let selected = settings.selected.min(settings.rows.len().saturating_sub(1));
    let mut lines = Vec::new();
    lines.push(Line::from(vec![
        Span::styled("Enter", Style::default().fg(Color::Cyan)),
        Span::raw(" edit  "),
        Span::styled("Esc", Style::default().fg(Color::Cyan)),
        Span::raw(" close"),
    ]));
    lines.push(Line::raw(""));
    for (index, row) in settings.rows.iter().enumerate() {
        let marker = if index == selected { "> " } else { "  " };
        let style = if index == selected {
            Style::default().bg(Color::Blue)
        } else {
            Style::default()
        };
        lines.push(
            Line::from(vec![
                Span::raw(marker),
                Span::styled(row.label, Style::default().fg(Color::Cyan)),
                Span::raw("  "),
                Span::raw(row.value.as_str()),
            ])
            .style(style),
        );
    }
    if let Some((label, input)) = &settings.editing {
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::styled("Editing ", Style::default().fg(Color::Yellow)),
            Span::raw(*label),
            Span::raw(": "),
            Span::raw(input.as_str()),
        ]));
    }
    if let Some(error) = settings.error.as_deref() {
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::styled(
                "Error: ",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                truncate_for_width(error, area.width.saturating_sub(12) as usize),
                Style::default().fg(Color::Red),
            ),
        ]));
    }

    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .title("Settings")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Green)),
        ),
        area,
    );
}

pub(crate) fn settings_field_label(field: SettingsField) -> &'static str {
    match field {
        SettingsField::BenchmarkUrl => "Latency URL",
        SettingsField::BenchmarkTimeoutMs => "Latency timeout ms",
        SettingsField::RequestTimeoutSec => "Request timeout sec",
        SettingsField::MaxConcurrency => "Max concurrency",
        SettingsField::VerifyTargets => "Verification targets",
        SettingsField::AutoPickThresholdMs => "Auto-pick threshold ms",
        SettingsField::AutoPickIntervalSec => "Auto-pick interval sec",
        SettingsField::SystemProxyServer => "System proxy server",
        SettingsField::ChinaIpRouting => "China IP routing",
        SettingsField::PrivateAccessProfile => "Private Access profile",
        SettingsField::PrivateAccessManifestPath => "Private Access service manifest",
        SettingsField::PrivateAccessMode => "Private Access mode",
        SettingsField::PrivateAccessServer => "Private Access server",
        SettingsField::PrivateAccessPort => "Private Access port",
        SettingsField::PrivateAccessUsername => "Private Access username",
        SettingsField::PrivateAccessPassword => "Private Access password",
        SettingsField::PrivateAccessPasswordEnv => "Private Access password env",
        SettingsField::PrivateAccessBridgeListen => "Private Access bridge listen",
        SettingsField::PrivateAccessUseInternetProxy => "SonicWall use Internet proxy",
        SettingsField::PrivateAccessTlsVerify => "Private Access TLS verify",
    }
}

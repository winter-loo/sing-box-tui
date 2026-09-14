use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SettingsField {
    BenchmarkUrl,
    SustainedTargetUrl,
    BenchmarkTimeoutMs,
    RequestTimeoutSec,
    MaxConcurrency,
    VerifyTargets,
    AutoPickIntervalSec,
    SystemProxyServer,
    ChinaIpRouting,
    TailscaleEnabled,
    TailscaleTailnetDomain,
    TailscaleHostname,
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
    SettingsField::SustainedTargetUrl,
    SettingsField::BenchmarkTimeoutMs,
    SettingsField::RequestTimeoutSec,
    SettingsField::MaxConcurrency,
    SettingsField::VerifyTargets,
    SettingsField::AutoPickIntervalSec,
    SettingsField::SystemProxyServer,
    SettingsField::ChinaIpRouting,
    SettingsField::TailscaleEnabled,
    SettingsField::TailscaleTailnetDomain,
    SettingsField::TailscaleHostname,
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

use crate::tui::ds::{render_dialog_frame, Theme};

pub(crate) fn draw_settings_panel(frame: &mut Frame, settings: &SettingsPanelSnapshot) {
    let area = frame.area();
    let theme = Theme::detect();
    render_dialog_frame(
        frame,
        area,
        &theme,
        " SETTINGS (s) ",
        80,
        20,
        |frame, inner_area| {
            if inner_area.height == 0 || inner_area.width == 0 {
                return;
            }

            let (main_area, footer_area) = if inner_area.height >= 3 {
                let [m, f] = Layout::vertical([
                    Constraint::Min(1),
                    Constraint::Length(1),
                ])
                .areas(inner_area);
                (m, Some(f))
            } else {
                (inner_area, None)
            };

            let total_lines = main_area.height as usize;
            let mut reserved = 1; // Header line
            if settings.editing.is_some() {
                reserved += 1;
            }
            if settings.error.is_some() {
                reserved += 1;
            }
            let visible_rows = total_lines.saturating_sub(reserved).max(1);
            let selected = settings.selected.min(settings.rows.len().saturating_sub(1));
            let first = if settings.rows.len() <= visible_rows {
                0
            } else if selected >= visible_rows {
                selected.saturating_sub(visible_rows.saturating_sub(1))
            } else {
                0
            };
            let last = (first + visible_rows).min(settings.rows.len());

            let mut lines = Vec::new();
            lines.push(Line::from(vec![
                Span::styled("Settings", theme.style_breadcrumb()),
                Span::raw(" "),
                Span::styled("· Runtime Configuration", theme.style_muted()),
            ]));

            for index in first..last {
                let row = &settings.rows[index];
                let is_selected = index == selected;
                let marker = if is_selected { "> " } else { "  " };
                let (marker_style, label_style, value_style) = if is_selected {
                    (
                        theme.style_selected_marker(),
                        theme.style_focused_row(),
                        theme.style_focused_row(),
                    )
                } else {
                    (
                        theme.style_base(),
                        theme.style_breadcrumb(),
                        theme.style_base(),
                    )
                };
                lines.push(
                    Line::from(vec![
                        Span::styled(marker, marker_style),
                        Span::styled(row.label, label_style),
                        Span::raw("  "),
                        Span::styled(row.value.as_str(), value_style),
                    ])
                    .style(if is_selected {
                        theme.style_focused_row()
                    } else {
                        theme.style_base()
                    }),
                );
            }

            if let Some((label, input)) = &settings.editing {
                lines.push(Line::from(vec![
                    Span::styled("Editing ", theme.style_warning()),
                    Span::styled(*label, theme.style_breadcrumb()),
                    Span::raw(": "),
                    Span::styled(input.as_str(), theme.style_base()),
                    Span::styled("█", theme.style_selected_marker()),
                ]));
            }

            if let Some(error) = settings.error.as_deref() {
                lines.push(Line::from(vec![
                    Span::styled(
                        "Error: ",
                        theme.style_error().add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        truncate_for_width(error, main_area.width.saturating_sub(10) as usize),
                        theme.style_error(),
                    ),
                ]));
            }

            frame.render_widget(Paragraph::new(lines).style(theme.style_base()), main_area);

            if let Some(footer) = footer_area {
                let footer_line = Line::from(vec![
                    Span::styled("[Esc/s]", theme.style_footer_keys()),
                    Span::raw(" "),
                    Span::styled("Close", theme.style_muted()),
                    Span::raw("  "),
                    Span::styled("[Enter]", theme.style_footer_keys()),
                    Span::raw(" "),
                    Span::styled("Save", theme.style_muted()),
                ]);
                frame.render_widget(Paragraph::new(footer_line).style(theme.style_base()), footer);
            }
        },
    );
}

pub(crate) fn settings_field_label(field: SettingsField) -> &'static str {
    match field {
        SettingsField::BenchmarkUrl => "Quick probe HTTPS target",
        SettingsField::SustainedTargetUrl => "Sustained HTTPS target",
        SettingsField::BenchmarkTimeoutMs => "Quick attempt timeout ms",
        SettingsField::RequestTimeoutSec => "Request timeout sec",
        SettingsField::MaxConcurrency => "Max concurrency",
        SettingsField::VerifyTargets => "Verification targets",
        SettingsField::AutoPickIntervalSec => "Automatic-selection interval sec",
        SettingsField::SystemProxyServer => "System proxy server",
        SettingsField::ChinaIpRouting => "China IP routing",
        SettingsField::TailscaleEnabled => "Tailscale endpoint",
        SettingsField::TailscaleTailnetDomain => "Tailscale tailnet domain",
        SettingsField::TailscaleHostname => "Tailscale hostname",
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

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    #[test]
    fn settings_panel_renders_dialog_frame_cursor_and_footer_hints() {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        let snapshot = SettingsPanelSnapshot {
            rows: vec![
                SettingRow {
                    label: "Quick probe HTTPS target",
                    value: "https://example.test/ping".to_string(),
                },
                SettingRow {
                    label: "China IP routing",
                    value: "enabled".to_string(),
                },
            ],
            selected: 0,
            editing: None,
            error: None,
        };

        terminal
            .draw(|f| {
                draw_settings_panel(f, &snapshot);
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

        assert!(text.contains("SETTINGS (s)"));
        assert!(text.contains("Settings"));
        assert!(text.contains("> Quick probe HTTPS target"));
        assert!(text.contains("https://example.test/ping"));
        assert!(text.contains("China IP routing"));
        assert!(text.contains("[Esc/s] Close  [Enter] Save"));
    }
}

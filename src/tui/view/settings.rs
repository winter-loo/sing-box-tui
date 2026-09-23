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
    pub(crate) field: SettingsField,
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

use crate::tui::ds::Theme;

fn settings_category(field: SettingsField) -> usize {
    match field {
        SettingsField::BenchmarkUrl
        | SettingsField::SustainedTargetUrl
        | SettingsField::BenchmarkTimeoutMs
        | SettingsField::RequestTimeoutSec
        | SettingsField::MaxConcurrency
        | SettingsField::VerifyTargets
        | SettingsField::AutoPickIntervalSec => 0,
        SettingsField::SystemProxyServer | SettingsField::ChinaIpRouting => 1,
        SettingsField::TailscaleEnabled
        | SettingsField::TailscaleTailnetDomain
        | SettingsField::TailscaleHostname => 2,
        SettingsField::PrivateAccessProfile
        | SettingsField::PrivateAccessManifestPath
        | SettingsField::PrivateAccessMode
        | SettingsField::PrivateAccessServer
        | SettingsField::PrivateAccessPort
        | SettingsField::PrivateAccessUsername
        | SettingsField::PrivateAccessPassword
        | SettingsField::PrivateAccessPasswordEnv
        | SettingsField::PrivateAccessBridgeListen
        | SettingsField::PrivateAccessUseInternetProxy
        | SettingsField::PrivateAccessTlsVerify => 3,
    }
}

pub(crate) fn draw_settings_panel(frame: &mut Frame, settings: &SettingsPanelSnapshot) {
    use ratatui::layout::Rect;
    let theme = Theme::detect();
    let selected = settings.selected.min(settings.rows.len().saturating_sub(1));
    let active_category = settings
        .rows
        .get(selected)
        .map(|row| settings_category(row.field))
        .unwrap_or(0);
    let (body, helper) = render_workbench_shell(
        frame,
        "SETTINGS",
        &format!(
            "{:02} / {:02}",
            selected + usize::from(!settings.rows.is_empty()),
            settings.rows.len()
        ),
        "Esc close   Enter edit   j/k navigate",
        &theme,
    );
    if body.width == 0 || body.height == 0 {
        return;
    }
    let rail_width = if body.width >= 100 {
        25
    } else {
        18.min(body.width / 3)
    };
    let gap = if body.width >= 100 { 2 } else { 1 };
    let rail = Rect::new(body.x, body.y, rail_width, body.height);
    let fields = Rect::new(
        body.x + rail_width + gap,
        body.y,
        body.width.saturating_sub(rail_width + gap),
        body.height,
    );
    for pane in [rail, fields] {
        frame.render_widget(
            Block::default()
                .borders(Borders::ALL)
                .border_style(theme.style_muted())
                .style(theme.style_base()),
            pane,
        );
    }
    let categories = ["PROBES", "NETWORK", "TAILSCALE", "PRIVATE ACCESS"];
    let counts: Vec<usize> = (0..4)
        .map(|category| {
            settings
                .rows
                .iter()
                .filter(|row| settings_category(row.field) == category)
                .count()
        })
        .collect();
    if rail.width > 2 {
        frame.render_widget(
            Paragraph::new("CATEGORIES").style(theme.style_muted()),
            Rect::new(rail.x + 2, rail.y + 1, rail.width.saturating_sub(3), 1),
        );
        for (index, category) in categories.iter().enumerate() {
            let y = rail.y + 3 + (index as u16) * 2;
            if y >= rail.bottom().saturating_sub(1) {
                break;
            }
            let marker = if index == active_category { "*" } else { " " };
            let label = truncate_for_width(category, rail.width.saturating_sub(7) as usize);
            let line = format!("{marker} {label}  {:02}", counts[index]);
            frame.render_widget(
                Paragraph::new(line).style(if index == active_category {
                    theme.style_focused_row()
                } else {
                    theme.style_base()
                }),
                Rect::new(rail.x + 1, y, rail.width.saturating_sub(2), 1),
            );
        }
    }
    if fields.width > 2 {
        let field_inner = Rect::new(
            fields.x + 2,
            fields.y + 1,
            fields.width.saturating_sub(4),
            fields.height.saturating_sub(2),
        );
        let rows: Vec<(usize, &SettingRow)> = settings
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| settings_category(row.field) == active_category)
            .collect();
        let reserved =
            usize::from(settings.editing.is_some()) + usize::from(settings.error.is_some());
        let visible = field_inner
            .height
            .saturating_sub(3)
            .saturating_sub(reserved as u16) as usize;
        let selected_in_category = rows
            .iter()
            .position(|(index, _)| *index == selected)
            .unwrap_or(0);
        let first = selected_in_category.saturating_sub(visible.saturating_sub(1));
        let remaining = rows.len().saturating_sub(first + visible);
        let heading = format!(
            "{}  ·  {} SHOWN · SCROLL FOR {}",
            categories[active_category],
            visible.min(rows.len().saturating_sub(first)),
            remaining
        );
        frame.render_widget(
            Paragraph::new(truncate_for_width(&heading, field_inner.width as usize))
                .style(theme.style_footer_keys()),
            Rect::new(field_inner.x, field_inner.y, field_inner.width, 1),
        );
        if rows.is_empty() {
            frame.render_widget(
                Paragraph::new("No settings available").style(theme.style_muted()),
                Rect::new(field_inner.x, field_inner.y + 2, field_inner.width, 1),
            );
        }
        let label_width = if field_inner.width >= 58 {
            27
        } else {
            (field_inner.width / 2).max(12)
        };
        for (display_index, (index, row)) in rows.iter().skip(first).take(visible).enumerate() {
            let y = field_inner.y + 2 + display_index as u16;
            let value_width = field_inner.width.saturating_sub(label_width + 2) as usize;
            let label = truncate_for_width(&row.label.to_ascii_uppercase(), label_width as usize);
            let value = truncate_for_width(&row.value, value_width);
            let line = Line::from(vec![
                Span::styled(
                    format!("{label:<width$}", width = label_width as usize),
                    theme.style_muted(),
                ),
                Span::raw("  "),
                Span::styled(value, theme.style_base()),
            ])
            .style(if *index == selected {
                theme.style_focused_row()
            } else {
                theme.style_base()
            });
            frame.render_widget(
                Paragraph::new(line),
                Rect::new(field_inner.x, y, field_inner.width, 1),
            );
        }
        let mut status_y = field_inner.bottom().saturating_sub(reserved as u16);
        if let Some((label, input)) = &settings.editing {
            let label = truncate_for_width(
                label,
                (field_inner.width as usize / 2).saturating_sub(6).max(8),
            );
            let prefix = format!("EDIT {label}: ");
            let input_width = (field_inner.width as usize)
                .saturating_sub(unicode_width::UnicodeWidthStr::width(prefix.as_str()) + 1);
            let text = format!("{prefix}{}█", tail_for_width(input, input_width));
            frame.render_widget(
                Paragraph::new(text).style(theme.style_warning()),
                Rect::new(field_inner.x, status_y, field_inner.width, 1),
            );
            status_y += 1;
        }
        if let Some(error) = &settings.error {
            frame.render_widget(
                Paragraph::new(truncate_for_width(
                    &format!("Error: {error}"),
                    field_inner.width as usize,
                ))
                .style(theme.style_error()),
                Rect::new(field_inner.x, status_y, field_inner.width, 1),
            );
        }
    }
    let hint = if settings.editing.is_some() {
        "Enter save   Esc cancel edit   j/k navigate"
    } else {
        "Enter edit   j/k navigate   Esc close / return to the same screen"
    };
    frame.render_widget(Paragraph::new(hint).style(theme.style_muted()), helper);
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

    fn buffer_text(buffer: &ratatui::buffer::Buffer) -> String {
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        text
    }

    #[test]
    fn settings_reference_layout_keeps_categories_fields_and_dismissal_visible() {
        for (width, height) in [(120, 30), (80, 24), (100, 40)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let snapshot = SettingsPanelSnapshot {
                rows: SETTINGS_FIELDS
                    .iter()
                    .map(|field| SettingRow {
                        field: *field,
                        label: settings_field_label(*field),
                        value: "value".into(),
                    })
                    .collect(),
                selected: SETTINGS_FIELDS.len() - 1,
                editing: None,
                error: None,
            };
            terminal
                .draw(|f| draw_settings_panel(f, &snapshot))
                .unwrap();
            let buffer = terminal.backend().buffer();
            let row = |y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            };
            assert!(row(0).contains("SETTINGS"));
            assert!(row(0).contains("Esc CLOSE"));
            assert!(row(4).contains("CATEGORIES"));
            assert!(row(4).contains("PRIVATE ACCESS"));
            assert!(row(height - 3).contains("Esc"));
            assert!(row(height - 1).contains("Esc close"));
            assert!((0..height).any(|y| row(y).contains("PRIVATE ACCESS TLS VERIFY")));
        }
    }

    #[test]
    fn settings_empty_and_long_editing_states_keep_input_and_footer() {
        for (width, height) in [(80, 24), (120, 30)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let mut snapshot = SettingsPanelSnapshot {
                rows: vec![],
                selected: 0,
                editing: None,
                error: None,
            };
            terminal
                .draw(|f| draw_settings_panel(f, &snapshot))
                .unwrap();
            let text = buffer_text(terminal.backend().buffer());
            assert!(text.contains("No settings available"));

            snapshot.rows.push(SettingRow {
                field: SettingsField::BenchmarkUrl,
                label: "Quick probe HTTPS target",
                value: format!("https://example.test/{}", "long/".repeat(30)),
            });
            snapshot.editing = Some((
                "Quick probe HTTPS target",
                format!("{}東京", "long/".repeat(30)),
            ));
            snapshot.error = Some("Invalid target".into());
            terminal
                .draw(|f| draw_settings_panel(f, &snapshot))
                .unwrap();
            let text = buffer_text(terminal.backend().buffer());
            assert!(text.contains("EDIT Quick probe"));
            assert!(text.lines().any(|line| line.contains("EDIT")
                && line.contains('東')
                && line.contains('京')
                && line.contains('█')));
            assert!(text.contains("Error: Invalid target"));
            assert!(text.contains("Enter save"));
            assert!(text.contains("Esc close"));
        }
    }

    #[test]
    fn settings_panel_renders_dialog_frame_cursor_and_footer_hints() {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        let snapshot = SettingsPanelSnapshot {
            rows: vec![
                SettingRow {
                    field: SettingsField::BenchmarkUrl,
                    label: "Quick probe HTTPS target",
                    value: "https://example.test/ping".to_string(),
                },
                SettingRow {
                    field: SettingsField::ChinaIpRouting,
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

        assert!(text.contains("SETTINGS"));
        assert!(text.contains("QUICK PROBE HTTPS TARGET"));
        assert!(text.contains("https://example.test/ping"));
        assert!(text.contains("NETWORK"));
        assert!(text.contains("Enter edit"));
    }
}

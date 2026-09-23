use super::*;

use crate::tui::ds::Theme;

#[derive(Clone, Copy)]
struct HelpBinding {
    key: &'static str,
    category: &'static str,
    section: HelpSection,
    summary: &'static str,
    detail: &'static str,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HelpSection {
    Navigation,
    Probes,
    Panels,
    NetworkApp,
    General,
}

impl HelpSection {
    fn label(self) -> &'static str {
        match self {
            Self::Navigation => "NAVIGATION",
            Self::Probes => "PROBES",
            Self::Panels => "PANELS",
            Self::NetworkApp => "NETWORK / APP",
            Self::General => "GENERAL",
        }
    }
}

const HELP_BINDINGS: &[HelpBinding] = &[
    HelpBinding {
        key: "up",
        category: "Navigation",
        section: HelpSection::Navigation,
        summary: "Move up / scroll details",
        detail: "Move the highlighted row up, or scroll Intranet Proxy details when the right pane is focused.",
    },
    HelpBinding {
        key: "k",
        category: "Navigation",
        section: HelpSection::Navigation,
        summary: "Move selection up",
        detail: "Vim-style shortcut for moving the highlighted row up.",
    },
    HelpBinding {
        key: "down",
        category: "Navigation",
        section: HelpSection::Navigation,
        summary: "Move down / scroll details",
        detail: "Move the highlighted row down, crossing from Internet Proxy into Intranet Proxy, or scroll right-pane details.",
    },
    HelpBinding {
        key: "j",
        category: "Navigation",
        section: HelpSection::Navigation,
        summary: "Move selection down",
        detail: "Vim-style shortcut for moving the highlighted row down.",
    },
    HelpBinding {
        key: "tab",
        category: "Navigation",
        section: HelpSection::Navigation,
        summary: "Switch pane",
        detail: "Move focus between the selector/group pane and the candidate node pane.",
    },
    HelpBinding {
        key: "h",
        category: "Navigation",
        section: HelpSection::Navigation,
        summary: "Switch to left pane",
        detail: "Move focus to the pane on the left.",
    },
    HelpBinding {
        key: "l",
        category: "Navigation",
        section: HelpSection::Navigation,
        summary: "Switch to right pane",
        detail: "Move focus to the pane on the right.",
    },
    HelpBinding {
        key: "left/right",
        category: "Navigation",
        section: HelpSection::Navigation,
        summary: "Switch node-view tab",
        detail: "When the candidate pane is focused, move between built-in and discovered custom views without changing the live selector.",
    },
    HelpBinding {
        key: "g",
        category: "Navigation",
        section: HelpSection::Navigation,
        summary: "Move to first item",
        detail: "Jump to the first item in the focused list.",
    },
    HelpBinding {
        key: "G",
        category: "Navigation",
        section: HelpSection::Navigation,
        summary: "Move to last item",
        detail: "Jump to the last item in the focused list.",
    },
    HelpBinding {
        key: "space",
        category: "Actions",
        section: HelpSection::Navigation,
        summary: "Activate selection",
        detail: "Apply an Internet Proxy selection, or open the selected Intranet Proxy profile details.",
    },
    HelpBinding {
        key: "m",
        category: "Actions",
        section: HelpSection::NetworkApp,
        summary: "Cycle Clash mode",
        detail: "Switch the controller between available Clash modes.",
    },
    HelpBinding {
        key: "T",
        category: "Actions",
        section: HelpSection::Probes,
        summary: "Quick-assess current scope",
        detail: "Run three live-controller reachability attempts for each node in the active selector or node-view scope.",
    },
    HelpBinding {
        key: "t",
        category: "Actions",
        section: HelpSection::Probes,
        summary: "Fully assess selected node",
        detail: "Run quick eligibility and then one bounded sustained transfer through an isolated node runtime.",
    },
    HelpBinding {
        key: "U",
        category: "Actions",
        section: HelpSection::Probes,
        summary: "Run active usability probe",
        detail: "Manually run the criterion declared by the active usability tab, including the built-in Streaming probe. Discovery alone never starts a probe.",
    },
    HelpBinding {
        key: "P",
        category: "Actions",
        section: HelpSection::Probes,
        summary: "Toggle probe schedule",
        detail: "Explicitly allow or deny scheduled background execution for the active usability tab and selected selector. Its manifest must also permit background execution.",
    },
    HelpBinding {
        key: "/",
        category: "Actions",
        section: HelpSection::Probes,
        summary: "Edit node-name filter",
        detail: "Open the filter editor. Comma-separated values include matches; prefix with ! or - to exclude.",
    },
    HelpBinding {
        key: "a",
        category: "Actions",
        section: HelpSection::Probes,
        summary: "Toggle auto-pick",
        detail: "Rank the active node-view panel periodically; switch after two complete wins, a same-tier 20% material improvement, and an idle current-node traffic window.",
    },
    HelpBinding {
        key: "i",
        category: "Actions",
        section: HelpSection::Panels,
        summary: "Open node quality detail",
        detail: "Show probe outcomes, reachability assessment, sustained quality, usability criteria, and the latest automatic-selection explanation.",
    },
    HelpBinding {
        key: "c",
        category: "Actions",
        section: HelpSection::Panels,
        summary: "Show active connections",
        detail: "Open a panel with active connection targets, outbound chains, and matched rules.",
    },
    HelpBinding {
        key: "b",
        category: "Actions",
        section: HelpSection::Panels,
        summary: "Edit bypass rules",
        detail: "Edit direct-bypass domains, IPs, and CIDRs written to the local rule-set.",
    },
    HelpBinding {
        key: "B",
        category: "Actions",
        section: HelpSection::Panels,
        summary: "Keep sing-box running",
        detail: "Exit the TUI while leaving sing-box, auto-pick, and active Private Access sessions running in the background.",
    },
    HelpBinding {
        key: "p",
        category: "Actions",
        section: HelpSection::NetworkApp,
        summary: "Toggle system proxy",
        detail: "Enable or disable the OS system proxy for the detected sing-box mixed inbound.",
    },
    HelpBinding {
        key: "\\",
        category: "Actions",
        section: HelpSection::NetworkApp,
        summary: "Toggle TUN mode",
        detail: "Add or remove the sing-box TUN inbound and restart sing-box to capture system traffic. Needs administrator/root privileges on macOS and Linux.",
    },
    HelpBinding {
        key: "u",
        category: "Actions",
        section: HelpSection::NetworkApp,
        summary: "Update subscriptions",
        detail: "Force a background subscription refresh when subscription refresh is configured.",
    },
    HelpBinding {
        key: "v",
        category: "Actions",
        section: HelpSection::NetworkApp,
        summary: "Verify network",
        detail: "Run configured connectivity checks in the background.",
    },
    HelpBinding {
        key: "V",
        category: "Actions",
        section: HelpSection::NetworkApp,
        summary: "Toggle Private Access",
        detail: "Connect or disconnect the selected Intranet Proxy profile.",
    },
    HelpBinding {
        key: "o",
        category: "Actions",
        section: HelpSection::Panels,
        summary: "Open settings",
        detail: "Edit quick assessment, sustained quality, automatic selection, and system proxy settings.",
    },
    HelpBinding {
        key: "r",
        category: "Actions",
        section: HelpSection::NetworkApp,
        summary: "Refresh groups",
        detail: "Reload selector groups, mode, and connection state from the controller.",
    },
    HelpBinding {
        key: "?",
        category: "General",
        section: HelpSection::General,
        summary: "Close help",
        detail: "Close this keybindings panel.",
    },
    HelpBinding {
        key: "esc",
        category: "General",
        section: HelpSection::General,
        summary: "Close help",
        detail: "Close this keybindings panel.",
    },
    HelpBinding {
        key: "enter",
        category: "General",
        section: HelpSection::General,
        summary: "Close help",
        detail: "Close this keybindings panel.",
    },
    HelpBinding {
        key: "q",
        category: "General",
        section: HelpSection::General,
        summary: "Quit",
        detail: "Exit the TUI. When help is open, q still exits the application.",
    },
];

pub(crate) fn help_item_count(diagnostic_count: usize) -> usize {
    HELP_BINDINGS.len().saturating_add(diagnostic_count)
}

#[cfg(test)]
pub(crate) fn has_help_binding(key: &str, summary: &str) -> bool {
    HELP_BINDINGS
        .iter()
        .any(|binding| binding.key == key && binding.summary == summary)
}

pub(crate) fn draw_help_panel(
    frame: &mut Frame,
    help_index: usize,
    diagnostics: &[ManifestDiagnostic],
) {
    use ratatui::layout::Rect;
    let theme = Theme::detect();
    let selected = help_index.min(help_item_count(diagnostics.len()).saturating_sub(1));
    let (body, helper) = render_workbench_shell(
        frame,
        "KEYBINDINGS",
        &format!("{} COMMANDS", HELP_BINDINGS.len()),
        "Esc/? close   j/k navigate   Ctrl+K actions",
        &theme,
    );
    if body.width == 0 || body.height == 0 {
        return;
    }
    let left_width = if body.width >= 95 {
        body.width * 2 / 3
    } else {
        body.width * 3 / 5
    };
    let gap = if body.width >= 95 { 2 } else { 1 };
    let command_pane = Rect::new(body.x, body.y, left_width, body.height);
    let detail_pane = Rect::new(
        body.x + left_width + gap,
        body.y,
        body.width.saturating_sub(left_width + gap),
        body.height,
    );
    for pane in [command_pane, detail_pane] {
        frame.render_widget(
            Block::default()
                .borders(Borders::ALL)
                .border_style(theme.style_muted())
                .style(theme.style_base()),
            pane,
        );
    }
    let command_inner = Rect::new(
        command_pane.x + 2,
        command_pane.y + 1,
        command_pane.width.saturating_sub(4),
        command_pane.height.saturating_sub(2),
    );
    let command_heading = if diagnostics.is_empty() {
        "COMMANDS BY CONTEXT".to_string()
    } else {
        format!(
            "COMMANDS BY CONTEXT  ·  invalid usability manifests ({})",
            diagnostics.len()
        )
    };
    frame.render_widget(
        Paragraph::new(command_heading).style(theme.style_footer_keys()),
        Rect::new(command_inner.x, command_inner.y, command_inner.width, 1),
    );
    let two_columns = command_inner.width >= 60;
    let column_count = if two_columns { 2 } else { 1 };
    let column_width = if two_columns {
        (command_inner.width - 2) / 2
    } else {
        command_inner.width
    };
    for column in 0..column_count {
        let x = command_inner.x + column as u16 * (column_width + 2);
        let mut rows: Vec<(Option<usize>, String)> = Vec::new();
        if two_columns {
            let sections: &[HelpSection] = if column == 0 {
                &[
                    HelpSection::Navigation,
                    HelpSection::Panels,
                    HelpSection::General,
                ]
            } else {
                &[HelpSection::Probes, HelpSection::NetworkApp]
            };
            for section in sections {
                rows.push((None, section.label().to_string()));
                for (index, binding) in HELP_BINDINGS
                    .iter()
                    .enumerate()
                    .filter(|(_, binding)| binding.section == *section)
                {
                    rows.push((
                        Some(index),
                        format!("{:<10} {}", binding.key, binding.summary),
                    ));
                }
            }
        } else {
            let mut previous_category = "";
            for (index, binding) in HELP_BINDINGS.iter().enumerate() {
                if binding.category != previous_category {
                    rows.push((None, binding.category.to_ascii_uppercase()));
                    previous_category = binding.category;
                }
                rows.push((
                    Some(index),
                    format!("{:<10} {}", binding.key, binding.summary),
                ));
            }
        }
        if column == column_count - 1 && !diagnostics.is_empty() {
            rows.push((None, "DIAGNOSTICS".into()));
            for (index, diagnostic) in diagnostics.iter().enumerate() {
                rows.push((
                    Some(HELP_BINDINGS.len() + index),
                    format!("! {}: {}", diagnostic.path, diagnostic.message),
                ));
            }
        }
        let visible = command_inner.height.saturating_sub(2) as usize;
        let selected_position = rows.iter().position(|(index, _)| *index == Some(selected));
        let first = selected_position
            .map(|position| position.saturating_sub(visible.saturating_sub(1)))
            .unwrap_or(0);
        for (display, (index, text)) in rows.iter().skip(first).take(visible).enumerate() {
            let y = command_inner.y + 2 + display as u16;
            let style = if *index == Some(selected) {
                theme.style_focused_row()
            } else if index.is_none() {
                theme.style_muted()
            } else {
                theme.style_base()
            };
            frame.render_widget(
                Paragraph::new(truncate_for_width(text, column_width as usize)).style(style),
                Rect::new(x, y, column_width, 1),
            );
        }
    }
    let detail_inner = Rect::new(
        detail_pane.x + 2,
        detail_pane.y + 1,
        detail_pane.width.saturating_sub(4),
        detail_pane.height.saturating_sub(2),
    );
    frame.render_widget(
        Paragraph::new("SELECTED COMMAND").style(theme.style_muted()),
        Rect::new(detail_inner.x, detail_inner.y, detail_inner.width, 1),
    );
    if let Some(binding) = HELP_BINDINGS.get(selected) {
        frame.render_widget(
            Paragraph::new(truncate_for_width(
                &format!("{}  {}", binding.key, binding.summary.to_ascii_uppercase()),
                detail_inner.width as usize,
            ))
            .style(theme.style_footer_keys()),
            Rect::new(detail_inner.x, detail_inner.y + 2, detail_inner.width, 1),
        );
        frame.render_widget(
            Paragraph::new(binding.detail)
                .style(theme.style_base())
                .wrap(Wrap { trim: false }),
            Rect::new(
                detail_inner.x,
                detail_inner.y + 4,
                detail_inner.width,
                detail_inner.height.saturating_sub(9).min(8),
            ),
        );
        let available_y = detail_inner.bottom().saturating_sub(3);
        frame.render_widget(
            Paragraph::new("AVAILABLE IN").style(theme.style_muted()),
            Rect::new(detail_inner.x, available_y, detail_inner.width, 1),
        );
        frame.render_widget(
            Paragraph::new(binding.category).style(theme.style_success()),
            Rect::new(detail_inner.x, available_y + 1, detail_inner.width, 1),
        );
    } else if diagnostics
        .get(selected.saturating_sub(HELP_BINDINGS.len()))
        .is_some()
    {
        frame.render_widget(
            Paragraph::new(format!(
                "INVALID MANIFEST {}/{}",
                selected - HELP_BINDINGS.len() + 1,
                diagnostics.len()
            ))
            .style(theme.style_error()),
            Rect::new(detail_inner.x, detail_inner.y + 2, detail_inner.width, 1),
        );
        let all_diagnostics = diagnostics
            .iter()
            .map(|diagnostic| {
                let name = diagnostic
                    .path
                    .rsplit(['/', '\\'])
                    .next()
                    .unwrap_or(&diagnostic.path);
                format!("{}\n{}", name, diagnostic.message)
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        frame.render_widget(
            Paragraph::new(all_diagnostics)
                .style(theme.style_warning())
                .wrap(Wrap { trim: false }),
            Rect::new(
                detail_inner.x,
                detail_inner.y + 4,
                detail_inner.width,
                detail_inner.height.saturating_sub(5),
            ),
        );
    }
    frame.render_widget(
        Paragraph::new("Ctrl+K actions   j/k navigate   Esc/? close").style(theme.style_muted()),
        helper,
    );
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
    fn help_reference_layout_keeps_sections_detail_and_close_visible() {
        for (width, height) in [(120, 30), (80, 24), (140, 26)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|f| draw_help_panel(f, 19, &[])).unwrap();
            let buffer = terminal.backend().buffer();
            let row = |y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            };
            assert!(row(0).contains("KEYBINDINGS"));
            assert!(row(0).contains("Esc CLOSE"));
            assert!(row(4).contains("COMMANDS BY CONTEXT"));
            assert!((0..height).any(|y| row(y).contains("SELECTED COMMAND")));
            assert!(row(height - 3).contains("Ctrl+K"));
            assert!(row(height - 1).contains("Esc/? close"));
            if width == 120 {
                let probes_y = (0..height).find(|&y| row(y).contains("PROBES")).unwrap();
                let network_y = (0..height)
                    .find(|&y| row(y).contains("NETWORK / APP"))
                    .unwrap();
                assert!(probes_y < network_y);
                assert!((0..height).any(|y| row(y).contains("space")));
            }
        }
    }

    #[test]
    fn help_diagnostics_remain_readable_at_narrow_and_reference_widths() {
        let diagnostics = [
            ManifestDiagnostic {
                path: "usability-probes/bad-one.json".into(),
                message: "manifest id is invalid".into(),
            },
            ManifestDiagnostic {
                path: "usability-probes/bad-two.json".into(),
                message: "URL must use https".into(),
            },
        ];
        for (width, height) in [(80, 24), (120, 30)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|f| draw_help_panel(f, help_item_count(diagnostics.len()) - 1, &diagnostics))
                .unwrap();
            let text = buffer_text(terminal.backend().buffer());
            for expected in [
                "bad-one.json",
                "manifest id is invalid",
                "bad-two.json",
                "URL must use https",
                "Esc/? close",
            ] {
                assert!(text.contains(expected));
            }
        }
    }

    #[test]
    fn help_panel_renders_dialog_frame_categories_and_footer_hints() {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|f| {
                draw_help_panel(f, 0, &[]);
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

        assert!(text.contains("KEYBINDINGS"));
        assert!(text.contains("COMMANDS BY CONTEXT"));
        assert!(text.contains("NAVIGATION"));
        assert!(text.contains("SELECTED COMMAND"));
        assert!(text.contains("Esc/? close"));
    }
}

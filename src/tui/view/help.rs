use super::*;

use ratatui::layout::Alignment;
use crate::tui::ds::{render_dialog_frame, Theme};

#[derive(Clone, Copy)]
struct HelpBinding {
    key: &'static str,
    category: &'static str,
    summary: &'static str,
    detail: &'static str,
}

const HELP_BINDINGS: &[HelpBinding] = &[
    HelpBinding {
        key: "up",
        category: "Navigation",
        summary: "Move up / scroll details",
        detail: "Move the highlighted row up, or scroll Intranet Proxy details when the right pane is focused.",
    },
    HelpBinding {
        key: "k",
        category: "Navigation",
        summary: "Move selection up",
        detail: "Vim-style shortcut for moving the highlighted row up.",
    },
    HelpBinding {
        key: "down",
        category: "Navigation",
        summary: "Move down / scroll details",
        detail: "Move the highlighted row down, crossing from Internet Proxy into Intranet Proxy, or scroll right-pane details.",
    },
    HelpBinding {
        key: "j",
        category: "Navigation",
        summary: "Move selection down",
        detail: "Vim-style shortcut for moving the highlighted row down.",
    },
    HelpBinding {
        key: "tab",
        category: "Navigation",
        summary: "Switch pane",
        detail: "Move focus between the selector/group pane and the candidate node pane.",
    },
    HelpBinding {
        key: "h",
        category: "Navigation",
        summary: "Switch to left pane",
        detail: "Move focus to the pane on the left.",
    },
    HelpBinding {
        key: "l",
        category: "Navigation",
        summary: "Switch to right pane",
        detail: "Move focus to the pane on the right.",
    },
    HelpBinding {
        key: "left/right",
        category: "Navigation",
        summary: "Switch node-view tab",
        detail: "When the candidate pane is focused, move between built-in and discovered custom views without changing the live selector.",
    },
    HelpBinding {
        key: "g",
        category: "Navigation",
        summary: "Move to first item",
        detail: "Jump to the first item in the focused list.",
    },
    HelpBinding {
        key: "G",
        category: "Navigation",
        summary: "Move to last item",
        detail: "Jump to the last item in the focused list.",
    },
    HelpBinding {
        key: "space",
        category: "Actions",
        summary: "Activate selection",
        detail: "Apply an Internet Proxy selection, or open the selected Intranet Proxy profile details.",
    },
    HelpBinding {
        key: "m",
        category: "Actions",
        summary: "Cycle Clash mode",
        detail: "Switch the controller between available Clash modes.",
    },
    HelpBinding {
        key: "T",
        category: "Actions",
        summary: "Quick-assess current scope",
        detail: "Run three live-controller reachability attempts for each node in the active selector or node-view scope.",
    },
    HelpBinding {
        key: "t",
        category: "Actions",
        summary: "Fully assess selected node",
        detail: "Run quick eligibility and then one bounded sustained transfer through an isolated node runtime.",
    },
    HelpBinding {
        key: "U",
        category: "Actions",
        summary: "Run active usability probe",
        detail: "Manually run the criterion declared by the active usability tab, including the built-in Streaming probe. Discovery alone never starts a probe.",
    },
    HelpBinding {
        key: "P",
        category: "Actions",
        summary: "Toggle probe schedule",
        detail: "Explicitly allow or deny scheduled background execution for the active usability tab and selected selector. Its manifest must also permit background execution.",
    },
    HelpBinding {
        key: "/",
        category: "Actions",
        summary: "Edit node-name filter",
        detail: "Open the filter editor. Comma-separated values include matches; prefix with ! or - to exclude.",
    },
    HelpBinding {
        key: "a",
        category: "Actions",
        summary: "Toggle auto-pick",
        detail: "Rank the active node-view panel periodically; switch after two complete wins, a same-tier 20% material improvement, and an idle current-node traffic window.",
    },
    HelpBinding {
        key: "i",
        category: "Actions",
        summary: "Open node quality detail",
        detail: "Show probe outcomes, reachability assessment, sustained quality, usability criteria, and the latest automatic-selection explanation.",
    },
    HelpBinding {
        key: "c",
        category: "Actions",
        summary: "Show active connections",
        detail: "Open a panel with active connection targets, outbound chains, and matched rules.",
    },
    HelpBinding {
        key: "b",
        category: "Actions",
        summary: "Edit bypass rules",
        detail: "Edit direct-bypass domains, IPs, and CIDRs written to the local rule-set.",
    },
    HelpBinding {
        key: "B",
        category: "Actions",
        summary: "Keep sing-box running",
        detail: "Exit the TUI while leaving sing-box, auto-pick, and active Private Access sessions running in the background.",
    },
    HelpBinding {
        key: "p",
        category: "Actions",
        summary: "Toggle system proxy",
        detail: "Enable or disable the OS system proxy for the detected sing-box mixed inbound.",
    },
    HelpBinding {
        key: "\\",
        category: "Actions",
        summary: "Toggle TUN mode",
        detail: "Add or remove the sing-box TUN inbound and restart sing-box to capture system traffic. Needs administrator/root privileges on macOS and Linux.",
    },
    HelpBinding {
        key: "u",
        category: "Actions",
        summary: "Update subscriptions",
        detail: "Force a background subscription refresh when subscription refresh is configured.",
    },
    HelpBinding {
        key: "v",
        category: "Actions",
        summary: "Verify network",
        detail: "Run configured connectivity checks in the background.",
    },
    HelpBinding {
        key: "V",
        category: "Actions",
        summary: "Toggle Private Access",
        detail: "Connect or disconnect the selected Intranet Proxy profile.",
    },
    HelpBinding {
        key: "o",
        category: "Actions",
        summary: "Open settings",
        detail: "Edit quick assessment, sustained quality, automatic selection, and system proxy settings.",
    },
    HelpBinding {
        key: "r",
        category: "Actions",
        summary: "Refresh groups",
        detail: "Reload selector groups, mode, and connection state from the controller.",
    },
    HelpBinding {
        key: "?",
        category: "General",
        summary: "Close help",
        detail: "Close this keybindings panel.",
    },
    HelpBinding {
        key: "esc",
        category: "General",
        summary: "Close help",
        detail: "Close this keybindings panel.",
    },
    HelpBinding {
        key: "enter",
        category: "General",
        summary: "Close help",
        detail: "Close this keybindings panel.",
    },
    HelpBinding {
        key: "q",
        category: "General",
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
    let area = frame.area();
    let theme = Theme::detect();
    render_dialog_frame(
        frame,
        area,
        &theme,
        " KEYBOARD SHORTCUTS & HELP (?) ",
        84,
        24,
        |frame, inner_area| {
            if inner_area.height == 0 || inner_area.width == 0 {
                return;
            }

            let (header_area, list_area, detail_area, footer_area) = if inner_area.height >= 12 {
                let [h, l, d, f] = Layout::vertical([
                    Constraint::Length(1),
                    Constraint::Min(6),
                    Constraint::Length(4),
                    Constraint::Length(1),
                ])
                .areas(inner_area);
                (Some(h), l, Some(d), Some(f))
            } else if inner_area.height >= 4 {
                let [l, f] = Layout::vertical([
                    Constraint::Min(2),
                    Constraint::Length(1),
                ])
                .areas(inner_area);
                (None, l, None, Some(f))
            } else {
                (None, inner_area, None, None)
            };

            if let Some(header) = header_area {
                let subheader = if diagnostics.is_empty() {
                    Line::from(vec![
                        Span::styled("Categories: ", theme.style_muted()),
                        Span::styled("Navigation", theme.style_breadcrumb()),
                        Span::styled(" · ", theme.style_muted()),
                        Span::styled("Actions", theme.style_breadcrumb()),
                        Span::styled(" · ", theme.style_muted()),
                        Span::styled("General", theme.style_breadcrumb()),
                    ])
                } else {
                    Line::from(vec![
                        Span::styled("Categories: ", theme.style_muted()),
                        Span::styled("Navigation", theme.style_breadcrumb()),
                        Span::styled(" · ", theme.style_muted()),
                        Span::styled("Actions", theme.style_breadcrumb()),
                        Span::styled(" · ", theme.style_muted()),
                        Span::styled("General", theme.style_breadcrumb()),
                        Span::styled(" · ", theme.style_muted()),
                        Span::styled(
                            format!("invalid usability manifests ({})", diagnostics.len()),
                            theme.style_warning(),
                        ),
                    ])
                };
                frame.render_widget(Paragraph::new(subheader).style(theme.style_base()), header);
            }

            let item_count = help_item_count(diagnostics.len());
            let selected = help_index.min(item_count.saturating_sub(1));
            let visible_rows = list_area.height as usize;
            let first = selected.saturating_sub(visible_rows.saturating_sub(1));

            let lines = (first..item_count.min(first.saturating_add(visible_rows)))
                .map(|index| {
                    if let Some(binding) = HELP_BINDINGS.get(index) {
                        help_binding(*binding, index == selected, &theme)
                    } else {
                        let diagnostic_index = index - HELP_BINDINGS.len();
                        help_manifest_diagnostic(
                            &diagnostics[diagnostic_index],
                            diagnostic_index,
                            diagnostics.len(),
                            index == selected,
                            &theme,
                        )
                    }
                })
                .collect::<Vec<_>>();

            frame.render_widget(Paragraph::new(lines).style(theme.style_base()), list_area);

            if let Some(detail_box) = detail_area {
                let detail_text = HELP_BINDINGS.get(selected).map_or_else(
                    || {
                        diagnostics
                            .get(selected.saturating_sub(HELP_BINDINGS.len()))
                            .map(|diagnostic| format!("{}: {}", diagnostic.path, diagnostic.message))
                            .unwrap_or_default()
                    },
                    |binding| binding.detail.to_string(),
                );
                let detail_widget = Paragraph::new(detail_text)
                    .style(theme.style_muted())
                    .wrap(Wrap { trim: false })
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_style(theme.style_muted())
                            .title(Span::styled(" Details ", theme.style_muted())),
                    );
                frame.render_widget(detail_widget, detail_box);
            }

            if let Some(footer) = footer_area {
                let count = format!("{} of {}", selected + 1, item_count);
                let count_len = count.len() as u16 + 2;
                let [hint_area, count_area] = Layout::horizontal([
                    Constraint::Min(1),
                    Constraint::Length(count_len),
                ])
                .areas(footer);

                let footer_line = Line::from(vec![
                    Span::styled("[Esc/?]", theme.style_footer_keys()),
                    Span::raw(" "),
                    Span::styled("Close", theme.style_muted()),
                    Span::raw("  "),
                    Span::styled("[j/k]", theme.style_footer_keys()),
                    Span::raw(" "),
                    Span::styled("Navigate", theme.style_muted()),
                ]);
                frame.render_widget(Paragraph::new(footer_line).style(theme.style_base()), hint_area);
                frame.render_widget(
                    Paragraph::new(count)
                        .alignment(Alignment::Right)
                        .style(theme.style_muted()),
                    count_area,
                );
            }
        },
    );
}

fn help_manifest_diagnostic(
    diagnostic: &ManifestDiagnostic,
    index: usize,
    count: usize,
    selected: bool,
    theme: &Theme,
) -> Line<'static> {
    let line_style = if selected {
        theme.style_focused_row()
    } else {
        theme.style_base()
    };
    Line::from(vec![
        Span::raw(" "),
        Span::styled(
            format!("! {:>3}/{:<3}", index + 1, count),
            if selected {
                theme.style_focused_row()
            } else {
                theme.style_error()
            },
        ),
        Span::raw("  "),
        Span::styled(
            "[Diagnostic]",
            if selected {
                theme.style_focused_row()
            } else {
                theme.style_warning()
            },
        ),
        Span::raw(" "),
        Span::styled(
            format!("{}: {}", diagnostic.path, diagnostic.message),
            if selected {
                theme.style_focused_row()
            } else {
                theme.style_warning()
            },
        ),
    ])
    .style(line_style)
}

fn help_binding(binding: HelpBinding, selected: bool, theme: &Theme) -> Line<'static> {
    let line_style = if selected {
        theme.style_focused_row()
    } else {
        theme.style_base()
    };
    Line::from(vec![
        Span::raw(" "),
        Span::styled(
            format!("{:>10}", binding.key),
            if selected {
                theme.style_focused_row()
            } else {
                theme.style_footer_keys()
            },
        ),
        Span::raw("  "),
        Span::styled(
            format!("[{:<10}]", binding.category),
            if selected {
                theme.style_focused_row()
            } else {
                theme.style_muted()
            },
        ),
        Span::raw(" "),
        Span::styled(
            binding.summary,
            if selected {
                theme.style_focused_row()
            } else {
                theme.style_base()
            },
        ),
    ])
    .style(line_style)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

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

        assert!(text.contains("KEYBOARD SHORTCUTS & HELP (?)"));
        assert!(text.contains("Categories:"));
        assert!(text.contains("Navigation"));
        assert!(text.contains("Actions"));
        assert!(text.contains("General"));
        assert!(text.contains("[Esc/?] Close"));
    }
}

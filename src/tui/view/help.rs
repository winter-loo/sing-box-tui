use super::*;

#[derive(Clone, Copy)]
struct HelpBinding {
    key: &'static str,
    summary: &'static str,
    detail: &'static str,
}

const HELP_BINDINGS: &[HelpBinding] = &[
    HelpBinding {
        key: "up",
        summary: "Move up / scroll details",
        detail: "Move the highlighted row up, or scroll Intranet Proxy details when the right pane is focused.",
    },
    HelpBinding {
        key: "k",
        summary: "Move selection up",
        detail: "Vim-style shortcut for moving the highlighted row up.",
    },
    HelpBinding {
        key: "down",
        summary: "Move down / scroll details",
        detail: "Move the highlighted row down, crossing from Internet Proxy into Intranet Proxy, or scroll right-pane details.",
    },
    HelpBinding {
        key: "j",
        summary: "Move selection down",
        detail: "Vim-style shortcut for moving the highlighted row down.",
    },
    HelpBinding {
        key: "tab",
        summary: "Switch pane",
        detail: "Move focus between the selector/group pane and the candidate node pane.",
    },
    HelpBinding {
        key: "h",
        summary: "Switch to left pane",
        detail: "Move focus to the pane on the left.",
    },
    HelpBinding {
        key: "l",
        summary: "Switch to right pane",
        detail: "Move focus to the pane on the right.",
    },
    HelpBinding {
        key: "left/right",
        summary: "Switch node-view tab",
        detail: "When the candidate pane is focused, move between built-in and discovered custom views without changing the live selector.",
    },
    HelpBinding {
        key: "g",
        summary: "Move to first item",
        detail: "Jump to the first item in the focused list.",
    },
    HelpBinding {
        key: "G",
        summary: "Move to last item",
        detail: "Jump to the last item in the focused list.",
    },
    HelpBinding {
        key: "space",
        summary: "Activate selection",
        detail: "Apply an Internet Proxy selection, or open the selected Intranet Proxy profile details.",
    },
    HelpBinding {
        key: "m",
        summary: "Cycle Clash mode",
        detail: "Switch the controller between available Clash modes.",
    },
    HelpBinding {
        key: "T",
        summary: "Quick-assess current scope",
        detail: "Run three live-controller reachability attempts for each node in the active selector or node-view scope.",
    },
    HelpBinding {
        key: "t",
        summary: "Fully assess selected node",
        detail: "Run quick eligibility and then one bounded sustained transfer through an isolated node runtime.",
    },
    HelpBinding {
        key: "U",
        summary: "Run active usability probe",
        detail: "Manually run the criterion declared by the active usability tab, including the built-in Streaming probe. Discovery alone never starts a probe.",
    },
    HelpBinding {
        key: "P",
        summary: "Toggle probe schedule",
        detail: "Explicitly allow or deny scheduled background execution for the active usability tab and selected selector. Its manifest must also permit background execution.",
    },
    HelpBinding {
        key: "/",
        summary: "Edit node-name filter",
        detail: "Open the filter editor. Comma-separated values include matches; prefix with ! or - to exclude.",
    },
    HelpBinding {
        key: "a",
        summary: "Toggle auto-pick",
        detail: "Rank the active node-view panel periodically; switch after two complete wins, a same-tier 20% material improvement, and an idle current-node traffic window.",
    },
    HelpBinding {
        key: "i",
        summary: "Open node quality detail",
        detail: "Show probe outcomes, reachability assessment, sustained quality, usability criteria, and the latest automatic-selection explanation.",
    },
    HelpBinding {
        key: "c",
        summary: "Show active connections",
        detail: "Open a panel with active connection targets, outbound chains, and matched rules.",
    },
    HelpBinding {
        key: "b",
        summary: "Edit bypass rules",
        detail: "Edit direct-bypass domains, IPs, and CIDRs written to the local rule-set.",
    },
    HelpBinding {
        key: "B",
        summary: "Keep sing-box running",
        detail: "Exit the TUI while leaving sing-box, auto-pick, and active Private Access sessions running in the background.",
    },
    HelpBinding {
        key: "p",
        summary: "Toggle system proxy",
        detail: "Enable or disable the OS system proxy for the detected sing-box mixed inbound.",
    },
    HelpBinding {
        key: "\\",
        summary: "Toggle TUN mode",
        detail: "Add or remove the sing-box TUN inbound and restart sing-box to capture system traffic. Needs administrator/root privileges on macOS and Linux.",
    },
    HelpBinding {
        key: "u",
        summary: "Update subscriptions",
        detail: "Force a background subscription refresh when subscription refresh is configured.",
    },
    HelpBinding {
        key: "v",
        summary: "Verify network",
        detail: "Run configured connectivity checks in the background.",
    },
    HelpBinding {
        key: "V",
        summary: "Toggle Private Access",
        detail: "Connect or disconnect the selected Intranet Proxy profile.",
    },
    HelpBinding {
        key: "o",
        summary: "Open settings",
        detail: "Edit quick assessment, sustained quality, automatic selection, and system proxy settings.",
    },
    HelpBinding {
        key: "r",
        summary: "Refresh groups",
        detail: "Reload selector groups, mode, and connection state from the controller.",
    },
    HelpBinding {
        key: "?",
        summary: "Close help",
        detail: "Close this keybindings panel.",
    },
    HelpBinding {
        key: "esc",
        summary: "Close help",
        detail: "Close this keybindings panel.",
    },
    HelpBinding {
        key: "enter",
        summary: "Close help",
        detail: "Close this keybindings panel.",
    },
    HelpBinding {
        key: "q",
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
    let frame_area = frame.area();
    let width = frame_area.width.saturating_sub(4).min(108);
    let height = frame_area.height.saturating_sub(4).min(34);
    let area = centered_rect(width.max(56), height.max(18), frame_area);
    frame.render_widget(Clear, area);
    let [list_area, detail_area] =
        Layout::vertical([Constraint::Min(10), Constraint::Length(7)]).areas(area);
    let item_count = help_item_count(diagnostics.len());
    let selected = help_index.min(item_count.saturating_sub(1));
    let visible_rows = list_area.height.saturating_sub(2).max(1) as usize;
    let first = selected.saturating_sub(visible_rows.saturating_sub(1));

    let lines = (first..item_count.min(first.saturating_add(visible_rows)))
        .map(|index| {
            if let Some(binding) = HELP_BINDINGS.get(index) {
                help_binding(*binding, index == selected)
            } else {
                let diagnostic_index = index - HELP_BINDINGS.len();
                help_manifest_diagnostic(
                    &diagnostics[diagnostic_index],
                    diagnostic_index,
                    diagnostics.len(),
                    index == selected,
                )
            }
        })
        .collect::<Vec<_>>();

    let widget = Paragraph::new(lines).block(
        Block::default()
            .title(if diagnostics.is_empty() {
                "Keybindings"
            } else {
                "Keybindings + invalid usability manifests"
            })
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Green)),
    );
    frame.render_widget(widget, list_area);

    let count = format!("{} of {}", selected + 1, item_count);
    let count_width = unicode_width::UnicodeWidthStr::width(count.as_str()) as u16;
    let count_area = ratatui::layout::Rect {
        x: list_area.x + list_area.width.saturating_sub(count_width + 1),
        y: list_area.y + list_area.height.saturating_sub(1),
        width: count_width,
        height: 1,
    };
    frame.render_widget(
        Paragraph::new(count).style(Style::default().fg(Color::Green)),
        count_area,
    );

    let detail = HELP_BINDINGS.get(selected).map_or_else(
        || {
            diagnostics
                .get(selected.saturating_sub(HELP_BINDINGS.len()))
                .map(|diagnostic| format!("{}: {}", diagnostic.path, diagnostic.message))
                .unwrap_or_default()
        },
        |binding| binding.detail.to_string(),
    );
    frame.render_widget(
        Paragraph::new(detail)
            .style(Style::default().fg(Color::Gray))
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL)),
        detail_area,
    );
}

fn help_manifest_diagnostic(
    diagnostic: &ManifestDiagnostic,
    index: usize,
    count: usize,
    selected: bool,
) -> Line<'static> {
    let line_style = if selected {
        Style::default().bg(Color::Blue)
    } else {
        Style::default()
    };
    Line::from(vec![
        Span::raw("  "),
        Span::styled(
            format!("! {:>3}/{:<3}", index + 1, count),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            format!("{}: {}", diagnostic.path, diagnostic.message),
            Style::default().fg(Color::Yellow),
        ),
    ])
    .style(line_style)
}

fn help_binding(binding: HelpBinding, selected: bool) -> Line<'static> {
    let line_style = if selected {
        Style::default().bg(Color::Blue)
    } else {
        Style::default()
    };
    Line::from(vec![
        Span::raw("  "),
        Span::styled(
            format!("{:>7}", binding.key),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(binding.summary, Style::default().fg(Color::Gray)),
    ])
    .style(line_style)
}

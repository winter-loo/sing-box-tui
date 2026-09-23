use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::tui::ds::{render_dialog_frame, Theme};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandItem {
    pub id: &'static str,
    pub title: String,
    pub category: &'static str,
    pub shortcut: Option<&'static str>,
}

impl CommandItem {
    pub fn new(
        id: &'static str,
        title: impl Into<String>,
        category: &'static str,
        shortcut: Option<&'static str>,
    ) -> Self {
        Self {
            id,
            title: title.into(),
            category,
            shortcut,
        }
    }
}

pub const CMD_SWITCH_INTERNET: &str = "switch_internet";
pub const CMD_SWITCH_PRIVATE_ACCESS: &str = "switch_private_access";
pub const CMD_SWITCH_PROVIDER: &str = "switch_provider";
pub const CMD_SWITCH_PROFILE: &str = "switch_profile";
#[allow(dead_code)]
pub const CMD_SWITCH_SUBSCRIPTIONS: &str = CMD_SWITCH_PROVIDER;
pub const CMD_TOGGLE_TUN: &str = "toggle_tun";
pub const CMD_TOGGLE_SYSTEM_PROXY: &str = "toggle_system_proxy";
pub const CMD_TRIGGER_USABILITY_PROBES: &str = "trigger_usability_probes";
pub const CMD_REFRESH_SUBSCRIPTIONS: &str = "refresh_subscriptions";
pub const CMD_REFRESH_CONNECTIONS: &str = "refresh_connections";
pub const CMD_VIEW_CONNECTIONS: &str = "view_connections";
pub const CMD_VIEW_NODE_QUALITY: &str = "view_node_quality";
pub const CMD_OPEN_SETTINGS: &str = "open_settings";
pub const CMD_OPEN_HELP: &str = "open_help";
pub const CMD_ENTER_NODE_DASHBOARD: &str = "enter_node_dashboard";
pub const CMD_QUIT: &str = "quit";

pub fn builtin_commands() -> Vec<CommandItem> {
    vec![
        CommandItem::new(
            CMD_SWITCH_INTERNET,
            "Switch to Internet Workspace",
            "Navigation",
            Some("Tab"),
        ),
        CommandItem::new(
            CMD_SWITCH_PRIVATE_ACCESS,
            "Switch to Private Access Workspace",
            "Navigation",
            Some("Tab"),
        ),
        CommandItem::new(
            CMD_SWITCH_PROVIDER,
            "Switch Proxy Provider",
            "Navigation",
            Some("p"),
        ),
        CommandItem::new(
            CMD_SWITCH_PROFILE,
            "Switch Private Access Profile",
            "Navigation",
            Some("p"),
        ),
        CommandItem::new(
            CMD_TOGGLE_TUN,
            "Toggle Internet TUN Mode",
            "Network capture",
            Some("\\"),
        ),
        CommandItem::new(
            CMD_TOGGLE_SYSTEM_PROXY,
            "Toggle System Proxy",
            "Network capture",
            Some("x"),
        ),
        CommandItem::new(
            CMD_TRIGGER_USABILITY_PROBES,
            "Trigger Usability Probes",
            "Diagnostics & Usability",
            Some("u"),
        ),
        CommandItem::new(
            CMD_REFRESH_SUBSCRIPTIONS,
            "Refresh Subscriptions",
            "Diagnostics & Usability",
            Some("U"),
        ),
        CommandItem::new(
            CMD_REFRESH_CONNECTIONS,
            "Refresh Active Connections",
            "Diagnostics & Usability",
            Some("r"),
        ),
        CommandItem::new(
            CMD_VIEW_CONNECTIONS,
            "View Active Connections",
            "Inspection Overlays",
            Some("c"),
        ),
        CommandItem::new(
            CMD_VIEW_NODE_QUALITY,
            "View Current Node Quality",
            "Inspection Overlays",
            Some("i"),
        ),
        CommandItem::new(
            CMD_OPEN_SETTINGS,
            "Open Settings",
            "Inspection Overlays",
            Some("s"),
        ),
        CommandItem::new(
            CMD_OPEN_HELP,
            "Open Help & Diagnostics",
            "Inspection Overlays",
            Some("?"),
        ),
        CommandItem::new(
            CMD_ENTER_NODE_DASHBOARD,
            "Enter Node Dashboard",
            "General",
            None,
        ),
        CommandItem::new(CMD_QUIT, "Quit sing-box-tui", "General", Some("q")),
    ]
}

pub fn filter_commands(items: &[CommandItem], query: &str) -> Vec<CommandItem> {
    let query_trimmed = query.trim();
    if query_trimmed.is_empty() {
        return items.to_vec();
    }

    let q_lower = query_trimmed.to_lowercase();
    let mut direct_matches = Vec::new();

    for item in items {
        let title_lower = item.title.to_lowercase();
        let cat_lower = item.category.to_lowercase();

        if title_lower.contains(&q_lower)
            || cat_lower.contains(&q_lower)
            || item
                .shortcut
                .map_or(false, |s| s.eq_ignore_ascii_case(&q_lower))
        {
            direct_matches.push(item.clone());
        }
    }

    if !direct_matches.is_empty() {
        return direct_matches;
    }

    let mut fuzzy_matches = Vec::new();
    for item in items {
        let title_lower = item.title.to_lowercase();
        if fuzzy_subsequence(&title_lower, &q_lower) {
            fuzzy_matches.push(item.clone());
        }
    }

    fuzzy_matches
}

fn fuzzy_subsequence(target: &str, query: &str) -> bool {
    let mut target_chars = target.chars();
    for qc in query.chars() {
        if qc.is_whitespace() {
            continue;
        }
        if !target_chars.any(|tc| tc == qc) {
            return false;
        }
    }
    true
}

pub fn render_command_palette(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    query: &str,
    selected_index: usize,
    items: &[CommandItem],
) {
    use unicode_width::UnicodeWidthStr;
    let height = (items.len() as u16 + 7).clamp(8, 18);
    render_dialog_frame(frame, area, theme, "", 48, height, |frame, inner| {
        let inner = crate::tui::ds::dialog_content_area(inner);
        if inner.width == 0 || inner.height < 4 {
            return;
        }
        let title = format!("MENU  ·  {} COMMANDS", items.len());
        frame.render_widget(
            Paragraph::new(title).style(theme.style_muted()),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );
        let input = Rect::new(inner.x, inner.y + 2, inner.width, 1);
        let max_query_width = input.width.saturating_sub(4) as usize;
        let tail = super::tail_for_width(query, max_query_width);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("> ", theme.style_footer_keys()),
                Span::styled(tail, theme.style_base()),
                Span::styled("█", theme.style_selected_marker()),
            ]))
            .style(theme.style_base()),
            input,
        );

        let list = Rect::new(
            inner.x,
            inner.y + 4,
            inner.width,
            inner.height.saturating_sub(5),
        );
        if items.is_empty() {
            frame.render_widget(
                Paragraph::new("No matching commands").style(theme.style_muted()),
                list,
            );
        } else {
            let visible = list.height as usize;
            let selected = selected_index.min(items.len().saturating_sub(1));
            let first = selected.saturating_sub(visible.saturating_sub(1));
            for (display, (index, item)) in items
                .iter()
                .enumerate()
                .skip(first)
                .take(visible)
                .enumerate()
            {
                let badge = item.shortcut.map(|s| format!("  {s}")).unwrap_or_default();
                let badge_width = UnicodeWidthStr::width(badge.as_str());
                let title_width = list.width.saturating_sub(badge_width as u16 + 2) as usize;
                let title = super::truncate_for_width(&item.title, title_width);
                let selected_row = index == selected;
                let style = if selected_row {
                    theme.style_focused_row()
                } else {
                    theme.style_base()
                };
                let marker = if selected_row { "› " } else { "  " };
                let used = UnicodeWidthStr::width(title.as_str()) + badge_width + 2;
                let spaces = " ".repeat((list.width as usize).saturating_sub(used));
                let line = Line::from(vec![
                    Span::styled(marker, style),
                    Span::styled(title, style),
                    Span::raw(spaces),
                    Span::styled(
                        badge,
                        if selected_row {
                            style
                        } else {
                            theme.style_footer_keys()
                        },
                    ),
                ])
                .style(style);
                frame.render_widget(
                    Paragraph::new(line),
                    Rect::new(list.x, list.y + display as u16, list.width, 1),
                );
            }
        }
        let footer = Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1);
        frame.render_widget(
            Paragraph::new("↑ ↓ select   Enter execute   Esc close").style(theme.style_muted()),
            footer,
        );
    });
    super::render_context_footer_hint(frame, "Ctrl+K close   Enter execute   ↑↓ select", theme);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    #[test]
    fn palette_centers_search_results_and_dismissal_at_multiple_sizes() {
        let items = builtin_commands();
        for (width, height) in [(120, 30), (80, 24), (140, 26)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let theme = Theme::default();
            terminal
                .draw(|f| render_command_palette(f, f.area(), &theme, "node", 0, &items))
                .unwrap();
            let lines: Vec<String> = (0..height)
                .map(|y| {
                    (0..width)
                        .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                        .collect()
                })
                .collect();
            let menu = lines
                .iter()
                .position(|line| line.contains("COMMANDS"))
                .unwrap();
            assert!(menu > 0 && menu < height as usize / 2);
            assert!(!lines[menu].contains("Ctrl+K"));
            assert!(lines.iter().any(|line| line.contains("> node█")));
            assert!(lines.iter().any(|line| line.contains("Esc close")));
            assert!(lines
                .iter()
                .any(|line| line.contains("Switch to Internet Workspace")));
        }
    }

    #[test]
    fn palette_keeps_long_unicode_query_cursor_and_last_result_visible() {
        for (width, height) in [(80, 24), (120, 30)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let theme = Theme::default();
            let items = builtin_commands();
            let query = format!("{}東京", "x".repeat(100));
            terminal
                .draw(|f| {
                    render_command_palette(f, f.area(), &theme, &query, items.len() - 1, &items)
                })
                .unwrap();
            let text = buffer_to_text(terminal.backend().buffer());
            assert!(text
                .lines()
                .any(|line| line.contains('東') && line.contains('京') && line.contains('█')));
            assert!(text.contains("Quit sing-box-tui"));
            assert!(text.contains("Esc close"));
        }
    }

    #[test]
    fn palette_footer_replaces_page_shortcuts_and_preserves_global_status() {
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        let theme = Theme::default();
        terminal
            .draw(|frame| {
                frame.render_widget(
                    Paragraph::new("c connections   i quality"),
                    Rect::new(0, 29, 60, 1),
                );
                frame.render_widget(
                    Paragraph::new("GLOBAL NET STABLE  ↓0.0M/s  ↑0.0M/s"),
                    Rect::new(80, 29, 40, 1),
                );
                render_command_palette(frame, frame.area(), &theme, "", 0, &builtin_commands());
            })
            .unwrap();
        let footer = (0..120)
            .map(|x| terminal.backend().buffer()[(x, 29)].symbol())
            .collect::<String>();
        assert!(footer.contains("Ctrl+K close"));
        assert!(!footer.contains("c connections"));
        assert!(footer.contains("GLOBAL NET STABLE"));
    }

    fn buffer_to_text(buffer: &ratatui::buffer::Buffer) -> String {
        let mut text = String::new();
        for y in 0..buffer.area().height {
            for x in 0..buffer.area().width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        text
    }

    #[test]
    fn test_builtin_commands_structure() {
        let cmds = builtin_commands();
        assert_eq!(cmds.len(), 15);

        let ids: Vec<&str> = cmds.iter().map(|c| c.id).collect();
        assert!(ids.contains(&CMD_SWITCH_INTERNET));
        assert!(ids.contains(&CMD_SWITCH_PRIVATE_ACCESS));
        assert!(ids.contains(&CMD_SWITCH_PROVIDER));
        assert!(ids.contains(&CMD_SWITCH_PROFILE));
        assert!(ids.contains(&CMD_TOGGLE_TUN));
        assert!(ids.contains(&CMD_TOGGLE_SYSTEM_PROXY));
        assert!(ids.contains(&CMD_TRIGGER_USABILITY_PROBES));
        assert!(ids.contains(&CMD_REFRESH_SUBSCRIPTIONS));
        assert!(ids.contains(&CMD_REFRESH_CONNECTIONS));
        assert!(ids.contains(&CMD_VIEW_CONNECTIONS));
        assert!(ids.contains(&CMD_VIEW_NODE_QUALITY));
        assert!(ids.contains(&CMD_OPEN_SETTINGS));
        assert!(ids.contains(&CMD_OPEN_HELP));
        assert!(ids.contains(&CMD_ENTER_NODE_DASHBOARD));
        assert!(ids.contains(&CMD_QUIT));

        let tun_cmd = cmds.iter().find(|c| c.id == CMD_TOGGLE_TUN).unwrap();
        assert_eq!(tun_cmd.title, "Toggle Internet TUN Mode");
        assert_eq!(tun_cmd.category, "Network capture");
        assert_eq!(tun_cmd.shortcut, Some("\\"));

        let dashboard_cmd = cmds
            .iter()
            .find(|c| c.id == CMD_ENTER_NODE_DASHBOARD)
            .unwrap();
        assert_eq!(dashboard_cmd.title, "Enter Node Dashboard");
        assert_eq!(dashboard_cmd.category, "General");
        assert_eq!(dashboard_cmd.shortcut, None);
    }

    #[test]
    fn test_filter_commands_empty_query() {
        let cmds = builtin_commands();
        let filtered = filter_commands(&cmds, "");
        assert_eq!(filtered.len(), cmds.len());

        let filtered_ws = filter_commands(&cmds, "   ");
        assert_eq!(filtered_ws.len(), cmds.len());
    }

    #[test]
    fn test_filter_commands_substring_matching() {
        let cmds = builtin_commands();

        // Substring on title
        let filtered = filter_commands(&cmds, "tun");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, CMD_TOGGLE_TUN);

        // Case insensitivity
        let filtered_upper = filter_commands(&cmds, "TUN");
        assert_eq!(filtered_upper.len(), 1);
        assert_eq!(filtered_upper[0].id, CMD_TOGGLE_TUN);

        // Substring on category
        let filtered_cat = filter_commands(&cmds, "navigation");
        assert_eq!(filtered_cat.len(), 4);
        assert_eq!(filtered_cat[0].id, CMD_SWITCH_INTERNET);
        assert_eq!(filtered_cat[1].id, CMD_SWITCH_PRIVATE_ACCESS);
        assert_eq!(filtered_cat[2].id, CMD_SWITCH_PROVIDER);
        assert_eq!(filtered_cat[3].id, CMD_SWITCH_PROFILE);

        // Substring on shortcut
        let filtered_sc = filter_commands(&cmds, "Tab");
        assert_eq!(filtered_sc.len(), 2);

        let filtered_slash = filter_commands(&cmds, "\\");
        assert_eq!(filtered_slash.len(), 1);
        assert_eq!(filtered_slash[0].id, CMD_TOGGLE_TUN);
    }

    #[test]
    fn test_filter_commands_fuzzy_matching() {
        let cmds = builtin_commands();

        // Subsequence: "swint" matches "Switch to Internet Workspace"
        let filtered = filter_commands(&cmds, "swint");
        assert!(!filtered.is_empty());
        assert_eq!(filtered[0].id, CMD_SWITCH_INTERNET);

        // Direct phrase: "node dashboard" matches "Enter Node Dashboard"
        let filtered_dashboard = filter_commands(&cmds, "node dashboard");
        assert_eq!(filtered_dashboard[0].id, CMD_ENTER_NODE_DASHBOARD);
    }

    #[test]
    fn test_filter_commands_no_match() {
        let cmds = builtin_commands();
        let filtered = filter_commands(&cmds, "xyz_not_found_12345");
        assert!(filtered.is_empty());
    }

    #[test]
    fn test_render_command_palette_basic() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = Theme::default();
        let cmds = builtin_commands();

        terminal
            .draw(|frame| {
                render_command_palette(frame, frame.area(), &theme, "", 0, &cmds);
            })
            .unwrap();

        let text = buffer_to_text(terminal.backend().buffer());
        assert!(text.contains("COMMANDS"));
        assert!(text.contains("> █"));
        assert!(text.contains("Switch to Internet Workspace"));
        assert!(text.contains("Tab"));
        assert!(text.contains("Enter execute   Esc close"));
    }

    #[test]
    fn test_render_command_palette_with_query_and_empty_state() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = Theme::default();

        terminal
            .draw(|frame| {
                render_command_palette(frame, frame.area(), &theme, "unknown", 0, &[]);
            })
            .unwrap();

        let text = buffer_to_text(terminal.backend().buffer());
        assert!(text.contains("COMMANDS"));
        assert!(text.contains("> unknown█"));
        assert!(text.contains("No matching commands"));
        assert!(text.contains("Enter execute   Esc close"));
    }
}

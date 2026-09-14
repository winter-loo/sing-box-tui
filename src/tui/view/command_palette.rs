use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

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
pub const CMD_ENTER_IDLE_DASHBOARD: &str = "enter_idle_dashboard";
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
            CMD_ENTER_IDLE_DASHBOARD,
            "Enter Idle Dashboard",
            "General",
            None,
        ),
        CommandItem::new(
            CMD_QUIT,
            "Quit sing-box-tui",
            "General",
            Some("q"),
        ),
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
    render_dialog_frame(
        frame,
        area,
        theme,
        " COMMAND PALETTE (Ctrl+K) ",
        74,
        18,
        |frame, inner_area| {
            if inner_area.height == 0 || inner_area.width == 0 {
                return;
            }

            let (input_area, list_area, footer_area) = if inner_area.height >= 3 {
                let [input, list, footer] = Layout::vertical([
                    Constraint::Length(1),
                    Constraint::Min(1),
                    Constraint::Length(1),
                ])
                .areas(inner_area);
                (input, list, Some(footer))
            } else {
                (inner_area, inner_area, None)
            };

            // First row: text input box `> {query}` with active cursor `█`
            let input_line = Line::from(vec![
                Span::styled("> ", theme.style_breadcrumb()),
                Span::styled(query, theme.style_base()),
                Span::styled("█", theme.style_selected_marker()),
            ]);
            frame.render_widget(Paragraph::new(input_line).style(theme.style_base()), input_area);

            // Below: list of filtered command items
            let mut lines = Vec::new();
            if items.is_empty() {
                lines.push(Line::from(vec![
                    Span::styled("  No matching commands", theme.style_muted()),
                ]));
            } else {
                let visible_rows = list_area.height as usize;
                let selected = selected_index.min(items.len().saturating_sub(1));
                let first = if items.len() <= visible_rows {
                    0
                } else if selected >= visible_rows {
                    selected.saturating_sub(visible_rows.saturating_sub(1))
                } else {
                    0
                };
                let last = (first + visible_rows).min(items.len());

                for index in first..last {
                    let item = &items[index];
                    let is_selected = index == selected;

                    let marker = if is_selected { "> " } else { "  " };
                    let cat_badge = format!("[{}]", item.category);
                    let cat_col = format!("{:<25}", cat_badge);
                    let title = &item.title;
                    let shortcut_badge = item
                        .shortcut
                        .map(|s| format!("[{s}]"))
                        .unwrap_or_default();
                    let shortcut_len = shortcut_badge.chars().count();

                    let prefix_len = marker.chars().count()
                        + cat_col.chars().count()
                        + 1
                        + title.chars().count();
                    let total_width = list_area.width as usize;

                    let padding = total_width.saturating_sub(prefix_len + shortcut_len);
                    let spaces = " ".repeat(padding.max(1));

                    let spans = if is_selected {
                        vec![
                            Span::styled(marker, theme.style_focused_row()),
                            Span::styled(cat_col, theme.style_focused_row()),
                            Span::styled(" ", theme.style_focused_row()),
                            Span::styled(title.clone(), theme.style_focused_row()),
                            Span::styled(spaces, theme.style_focused_row()),
                            Span::styled(shortcut_badge, theme.style_focused_row()),
                        ]
                    } else {
                        vec![
                            Span::styled(marker, theme.style_muted()),
                            Span::styled(cat_col, theme.style_muted()),
                            Span::raw(" "),
                            Span::styled(title.clone(), theme.style_base()),
                            Span::raw(spaces),
                            Span::styled(shortcut_badge, theme.style_footer_keys()),
                        ]
                    };

                    lines.push(Line::from(spans).style(if is_selected {
                        theme.style_focused_row()
                    } else {
                        theme.style_base()
                    }));
                }
            }

            frame.render_widget(Paragraph::new(lines).style(theme.style_base()), list_area);

            // Footer hint: `[Enter] Execute  [Esc] Dismiss`
            if let Some(footer) = footer_area {
                let footer_line = Line::from(vec![
                    Span::styled("[Enter]", theme.style_footer_keys()),
                    Span::raw(" "),
                    Span::styled("Execute", theme.style_muted()),
                    Span::raw("  "),
                    Span::styled("[Esc]", theme.style_footer_keys()),
                    Span::raw(" "),
                    Span::styled("Dismiss", theme.style_muted()),
                ]);
                frame.render_widget(Paragraph::new(footer_line).style(theme.style_base()), footer);
            }
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

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
        assert!(ids.contains(&CMD_ENTER_IDLE_DASHBOARD));
        assert!(ids.contains(&CMD_QUIT));

        let tun_cmd = cmds.iter().find(|c| c.id == CMD_TOGGLE_TUN).unwrap();
        assert_eq!(tun_cmd.title, "Toggle Internet TUN Mode");
        assert_eq!(tun_cmd.category, "Network capture");
        assert_eq!(tun_cmd.shortcut, Some("\\"));

        let idle_cmd = cmds.iter().find(|c| c.id == CMD_ENTER_IDLE_DASHBOARD).unwrap();
        assert_eq!(idle_cmd.title, "Enter Idle Dashboard");
        assert_eq!(idle_cmd.category, "General");
        assert_eq!(idle_cmd.shortcut, None);
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

        // Subsequence: "idle" matches "Enter Idle Dashboard"
        let filtered_idle = filter_commands(&cmds, "idle");
        assert_eq!(filtered_idle[0].id, CMD_ENTER_IDLE_DASHBOARD);
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
        assert!(text.contains("COMMAND PALETTE (Ctrl+K)"));
        assert!(text.contains("> █"));
        assert!(text.contains("Switch to Internet Workspace"));
        assert!(text.contains("[Tab]"));
        assert!(text.contains("[Enter] Execute  [Esc] Dismiss"));
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
        assert!(text.contains("COMMAND PALETTE (Ctrl+K)"));
        assert!(text.contains("> unknown█"));
        assert!(text.contains("No matching commands"));
        assert!(text.contains("[Enter] Execute  [Esc] Dismiss"));
    }
}

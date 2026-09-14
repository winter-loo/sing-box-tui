use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph};

use crate::tui::ds::widgets::render_dialog_frame;
use crate::tui::ds::Theme;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProviderItem {
    pub(crate) name: String,
    pub(crate) is_current: bool,
}

/// Renders the centered Selector / Provider switching modal dialog.
/// Matches Figma 796:352 (Internet / Provider selection popup) and 998:40 (Intranet profile selection popup).
/// Standard dimensions: 50 cols x 10 rows (400x160 px in 8x16 terminal grid).
pub(crate) fn render_provider_modal(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    title: &str,
    providers: &[ProviderItem],
    selected_index: usize,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    // Figma 796:352: 400px = 50 columns in standard 8px/col grid
    let dialog_width = 50.min(area.width.saturating_sub(4)).max(30);
    let dialog_height = (providers.len() as u16 + 4)
        .min(area.height.saturating_sub(2))
        .max(6);

    let modal_title = if title.is_empty() {
        "INTERNET PROXY PROVIDER"
    } else {
        title
    };

    render_dialog_frame(
        frame,
        area,
        theme,
        modal_title,
        dialog_width,
        dialog_height,
        |frame, inner_area| {
            let [list_area, footer_area] = Layout::vertical([
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .areas(inner_area);

            let items = providers
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    let is_selected = i == selected_index;
                    // Handoff 1014:2:
                    // '*' marks the applied item; highlighted row marks keyboard focus.
                    // '>' may reinforce focus in chooser lists. Moving focus alone must not apply a selection.
                    let (marker_symbol, marker_style) = if p.is_current {
                        (
                            "*",
                            if is_selected {
                                theme.style_focused_row()
                            } else {
                                theme.style_success()
                            },
                        )
                    } else {
                        (" ", theme.style_muted())
                    };

                    let cursor_prefix = if is_selected { "> " } else { "  " };

                    let spans = vec![
                        Span::styled(marker_symbol, marker_style),
                        Span::styled(
                            cursor_prefix,
                            if is_selected {
                                theme.style_focused_row()
                            } else {
                                theme.style_muted()
                            },
                        ),
                        Span::styled(
                            p.name.clone(),
                            if is_selected {
                                theme.style_focused_row()
                            } else {
                                theme.style_base()
                            },
                        ),
                    ];
                    let line = Line::from(spans);
                    if is_selected {
                        ListItem::new(line).style(theme.style_focused_row())
                    } else {
                        ListItem::new(line)
                    }
                })
                .collect::<Vec<_>>();

            let list = List::new(items).highlight_style(theme.style_focused_row());
            let mut state = ListState::default().with_selected(Some(selected_index));
            frame.render_stateful_widget(list, list_area, &mut state);

            let footer_spans = vec![
                Span::styled("[Enter]", theme.style_footer_keys()),
                Span::raw(" "),
                Span::styled("Select", theme.style_muted()),
                Span::raw("  "),
                Span::styled("[Esc]", theme.style_footer_keys()),
                Span::raw(" "),
                Span::styled("Close", theme.style_muted()),
            ];
            frame.render_widget(Paragraph::new(Line::from(footer_spans)), footer_area);
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn rendered_provider_modal_lines_at(
        title: &str,
        providers: &[ProviderItem],
        selected_index: usize,
        width: u16,
        height: u16,
    ) -> Vec<String> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let theme = Theme::default();
        terminal
            .draw(|frame| {
                render_provider_modal(frame, frame.area(), &theme, title, providers, selected_index)
            })
            .expect("provider modal renders");
        terminal
            .backend()
            .buffer()
            .content
            .chunks(width as usize)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect()
    }

    #[test]
    fn provider_modal_renders_internet_proxy_provider_with_focus_and_applied_markers() {
        let providers = vec![
            ProviderItem {
                name: "AirTCP".to_string(),
                is_current: true,
            },
            ProviderItem {
                name: "宝贝云".to_string(),
                is_current: false,
            },
        ];

        let lines = rendered_provider_modal_lines_at("INTERNET PROXY PROVIDER", &providers, 1, 120, 30);
        let text = lines.join("\n");
        assert!(text.contains("INTERNET PROXY PROVIDER"));
        assert!(text.contains("AirTCP"));
        assert!(text.contains("*"));
        assert!(text.contains("> 宝") && text.contains("云"));
        assert!(text.contains("[Enter]"));
        assert!(text.contains("Select"));
        assert!(text.contains("[Esc]"));
        assert!(text.contains("Close"));
    }

    #[test]
    fn provider_modal_renders_intranet_profile() {
        let profiles = vec![
            ProviderItem {
                name: "Corp-Production".to_string(),
                is_current: true,
            },
            ProviderItem {
                name: "Corp-Staging".to_string(),
                is_current: false,
            },
        ];

        let lines = rendered_provider_modal_lines_at("INTRANET PROFILE", &profiles, 0, 100, 25);
        let text = lines.join("\n");
        assert!(text.contains("INTRANET PROFILE"));
        assert!(text.contains("*> Corp-Production"));
        assert!(text.contains("Corp-Staging"));
        assert!(text.contains("[Enter]"));
        assert!(text.contains("Select"));
    }

    #[test]
    fn provider_modal_zero_area_does_not_panic() {
        let backend = TestBackend::new(0, 0);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let theme = Theme::default();
        let providers = vec![ProviderItem {
            name: "AirTCP".to_string(),
            is_current: true,
        }];
        terminal
            .draw(|frame| {
                render_provider_modal(frame, frame.area(), &theme, "", &providers, 0)
            })
            .expect("renders zero area safely");
    }
}


use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

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

    // Figma 796:352: 400x160px = 50x10 cells in the reference 8x16 grid.
    // Each option gets a content row plus one rhythm row when the viewport permits it.
    let dialog_width = 50;
    let dialog_height = (providers.len() as u16).saturating_mul(2).saturating_add(4);

    let modal_title = if title.is_empty() {
        "INTERNET PROXY PROVIDER"
    } else {
        title
    };

    render_dialog_frame(
        frame,
        area,
        theme,
        "",
        dialog_width,
        dialog_height,
        |frame, inner_area| {
            if inner_area.width == 0 || inner_area.height == 0 {
                return;
            }

            let content_area = Rect {
                x: inner_area.x.saturating_add(1),
                y: inner_area.y,
                width: inner_area.width.saturating_sub(2),
                height: inner_area.height,
            };
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    modal_title,
                    theme.style_breadcrumb(),
                ))),
                Rect { height: 1, ..content_area },
            );

            if inner_area.height < 2 {
                return;
            }

            let footer_y = inner_area.y.saturating_add(inner_area.height - 1);
            let option_rows = inner_area.height.saturating_sub(2);
            let spaced_options = option_rows >= (providers.len() as u16).saturating_mul(2);
            let first_option_offset = u16::from(spaced_options);
            let option_stride = if spaced_options { 2 } else { 1 };

            for (index, provider) in providers.iter().enumerate() {
                let row_offset = first_option_offset
                    .saturating_add((index as u16).saturating_mul(option_stride));
                if row_offset >= option_rows {
                    break;
                }
                let row_area = Rect {
                    y: inner_area.y.saturating_add(1).saturating_add(row_offset),
                    height: 1,
                    ..content_area
                };
                let is_focused = index == selected_index;
                let applied_marker = if provider.is_current { "*" } else { " " };
                let focus_marker = if is_focused { ">" } else { " " };
                let line = Line::from(vec![
                    Span::styled(
                        applied_marker,
                        if provider.is_current {
                            theme.style_success()
                        } else {
                            theme.style_muted()
                        },
                    ),
                    Span::styled(
                        focus_marker,
                        if is_focused {
                            theme.style_selected_marker()
                        } else {
                            theme.style_muted()
                        },
                    ),
                    Span::raw(" "),
                    Span::styled(
                        provider.name.clone(),
                        if is_focused {
                            theme.style_focused_row()
                        } else {
                            theme.style_base()
                        },
                    ),
                ]);
                frame.render_widget(
                    Paragraph::new(line).style(if is_focused {
                        theme.style_focused_row()
                    } else {
                        theme.style_base()
                    }),
                    row_area,
                );
            }

            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("Esc", theme.style_footer_keys()),
                    Span::raw(" "),
                    Span::styled("close", theme.style_muted()),
                ])),
                Rect {
                    y: footer_y,
                    height: 1,
                    ..content_area
                },
            );
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

    fn first_non_blank_column(line: &str) -> usize {
        line.chars()
            .position(|character| character != ' ')
            .expect("rendered row has content")
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
            ProviderItem {
                name: "白嫖机场".to_string(),
                is_current: false,
            },
        ];

        let lines = rendered_provider_modal_lines_at("INTERNET PROXY PROVIDER", &providers, 1, 120, 30);
        let text = lines.join("\n");
        assert!(text.contains("INTERNET PROXY PROVIDER"));
        assert!(text.contains("AirTCP"));
        assert!(text.contains("*"));
        assert!(lines
            .iter()
            .any(|line| line.contains(" > 宝") && line.contains('云')));
        assert!(lines
            .iter()
            .any(|line| line.contains('白')
                && line.contains('嫖')
                && line.contains('机')
                && line.contains('场')));
        assert!(text.contains("Esc close"));

        let top = lines
            .iter()
            .position(|line| line.contains('┌'))
            .expect("dialog top border");
        let bottom = lines
            .iter()
            .position(|line| line.contains('└'))
            .expect("dialog bottom border");
        let left = first_non_blank_column(&lines[top]);
        let right = lines[top]
            .chars()
            .collect::<Vec<_>>()
            .iter()
            .rposition(|character| *character != ' ')
            .expect("dialog right border");

        assert_eq!(bottom - top + 1, 10, "canonical dialog height");
        assert_eq!(right - left + 1, 50, "canonical dialog width");
        assert!(!lines[top].contains("INTERNET PROXY PROVIDER"));
        assert!(lines[top + 1].contains("INTERNET PROXY PROVIDER"));

        let option_rows = ["AirTCP", "宝", "白"]
            .iter()
            .map(|provider| {
                lines
                    .iter()
                    .position(|line| line.contains(provider))
                    .expect("provider row")
            })
            .collect::<Vec<_>>();
        assert_eq!(option_rows, vec![top + 3, top + 5, top + 7]);
        assert!(lines[top + 8].contains("Esc close"));
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
        assert!(text.contains("Esc close"));

        let top = lines.iter().position(|line| line.contains('┌')).unwrap();
        let bottom = lines.iter().position(|line| line.contains('└')).unwrap();
        assert_eq!(bottom - top + 1, 8);
        assert!(lines[top + 1].contains("INTRANET PROFILE"));
        assert!(lines[top + 3].contains("Corp-Production"));
        assert!(lines[top + 5].contains("Corp-Staging"));
        assert!(lines[top + 6].contains("Esc close"));
    }

    #[test]
    fn provider_modal_clamps_to_a_compact_viewport_without_losing_its_border() {
        let providers = vec![
            ProviderItem {
                name: "AirTCP".to_string(),
                is_current: true,
            },
            ProviderItem {
                name: "宝贝云".to_string(),
                is_current: false,
            },
            ProviderItem {
                name: "白嫖机场".to_string(),
                is_current: false,
            },
        ];

        let lines = rendered_provider_modal_lines_at("INTERNET PROXY PROVIDER", &providers, 2, 32, 9);
        let top = lines
            .iter()
            .position(|line| line.contains('┌'))
            .expect("compact top border");
        let bottom = lines
            .iter()
            .position(|line| line.contains('└'))
            .expect("compact bottom border");

        assert!(bottom > top);
        assert!(lines[top + 1].contains("INTERNET PROXY"));
        assert!(lines
            .iter()
            .any(|line| line.contains('白')
                && line.contains('嫖')
                && line.contains('机')
                && line.contains('场')));
        assert!(lines.iter().any(|line| line.contains("Esc close")));
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


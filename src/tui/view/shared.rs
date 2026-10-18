use super::*;

pub(crate) fn network_mode_footer_spans(
    system_proxy_enabled: bool,
    tun_enabled: bool,
    down_rate: &str,
    up_rate: &str,
    theme: &crate::tui::ds::Theme,
) -> Vec<Span<'static>> {
    let mode_style = |enabled| {
        if enabled {
            Style::default()
                .fg(theme.text_status_active())
                .add_modifier(Modifier::BOLD)
        } else {
            theme.style_muted()
        }
    };
    vec![
        Span::styled("SYSTEM PROXY", mode_style(system_proxy_enabled)),
        Span::raw("  "),
        Span::styled("TUN", mode_style(tun_enabled)),
        Span::raw("  "),
        Span::styled(
            format!("↓{down_rate}"),
            Style::default().fg(theme.text_transfer()),
        ),
        Span::raw("  "),
        Span::styled(
            format!("↑{up_rate}"),
            Style::default().fg(theme.text_transfer()),
        ),
    ]
}

#[cfg(test)]
mod footer_tests {
    use super::*;

    #[test]
    fn footer_labels_each_network_mode_and_highlights_only_enabled_modes() {
        let theme = crate::tui::ds::Theme::detect();
        for (proxy, tun) in [(false, false), (true, false), (false, true), (true, true)] {
            let spans = network_mode_footer_spans(proxy, tun, "8.4M/s", "612K/s", &theme);
            let text = spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            assert_eq!(text, "SYSTEM PROXY  TUN  ↓8.4M/s  ↑612K/s");
            for (span, enabled) in [(&spans[0], proxy), (&spans[2], tun)] {
                assert_eq!(
                    span.style.fg,
                    Some(if enabled {
                        theme.text_status_active()
                    } else {
                        theme.text_muted()
                    })
                );
                assert_eq!(span.style.add_modifier.contains(Modifier::BOLD), enabled);
            }
        }
    }
}

/// Full-width workbench overlay used by the Settings and Help reference frames.
/// The last terminal row belongs to the underlying global footer.
pub(crate) fn render_workbench_shell(
    frame: &mut Frame,
    title: &str,
    meta: &str,
    footer_hint: &str,
    theme: &crate::tui::ds::Theme,
) -> (ratatui::layout::Rect, ratatui::layout::Rect) {
    use ratatui::layout::Rect;
    let area = frame.area();
    let shell = Rect {
        height: area.height.saturating_sub(1),
        ..area
    };
    frame.render_widget(Clear, shell);
    frame.render_widget(
        Block::default()
            .style(theme.style_base())
            .borders(Borders::ALL)
            .border_style(theme.style_muted()),
        shell,
    );
    let inset = if shell.width >= 96 { 5 } else { 2 };
    let left = shell.x.saturating_add(inset);
    let content_width = shell.width.saturating_sub(inset * 2);
    let header = Rect::new(left, shell.y, content_width, 1.min(shell.height));
    let close_width = 9.min(content_width);
    let meta_width = content_width.saturating_sub(close_width);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(title.to_owned(), theme.style_success()),
            Span::styled(format!("    {meta}"), theme.style_muted()),
        ])),
        Rect {
            width: meta_width,
            ..header
        },
    );
    if close_width > 0 {
        frame.render_widget(
            Paragraph::new("Esc CLOSE").style(theme.style_footer_keys()),
            Rect {
                x: left + content_width - close_width,
                width: close_width,
                ..header
            },
        );
    }
    let body = Rect::new(
        left,
        shell.y.saturating_add(3),
        content_width,
        shell.height.saturating_sub(6),
    );
    let helper = Rect::new(
        left,
        shell.bottom().saturating_sub(2),
        content_width,
        1.min(shell.height),
    );
    render_context_footer_hint(frame, footer_hint, theme);
    (body, helper)
}

pub(crate) fn render_context_footer_hint(
    frame: &mut Frame,
    hint: &str,
    theme: &crate::tui::ds::Theme,
) {
    use ratatui::layout::Rect;
    let area = frame.area();
    if area.height == 0 {
        return;
    }
    let status_start = {
        let buffer = frame.buffer_mut();
        let marker = ["S", "Y", "S", "T", "E", "M", " ", "P", "R", "O", "X", "Y"];
        (area.x..area.right().saturating_sub(11))
            .find(|&x| {
                marker.iter().enumerate().all(|(offset, symbol)| {
                    buffer[(x + offset as u16, area.bottom() - 1)].symbol() == *symbol
                })
            })
            .unwrap_or_else(|| area.right().saturating_sub(50))
    };
    let footer = Rect::new(
        area.x,
        area.bottom() - 1,
        status_start.saturating_sub(area.x),
        1,
    );
    frame.render_widget(Clear, footer);
    frame.render_widget(
        Paragraph::new(truncate_for_width(hint, footer.width as usize))
            .style(theme.style_footer_keys()),
        footer,
    );
}

pub(crate) fn centered_rect(
    width: u16,
    height: u16,
    area: ratatui::layout::Rect,
) -> ratatui::layout::Rect {
    let [vertical] = Layout::vertical([Constraint::Length(height)])
        .flex(ratatui::layout::Flex::Center)
        .areas(area);
    let [horizontal] = Layout::horizontal([Constraint::Length(width)])
        .flex(ratatui::layout::Flex::Center)
        .areas(vertical);
    horizontal
}

pub(crate) fn truncate_for_width(value: &str, max_width: usize) -> String {
    use unicode_segmentation::UnicodeSegmentation;

    if max_width == 0 {
        return String::new();
    }
    let width = unicode_width::UnicodeWidthStr::width(value);
    if width <= max_width {
        return value.to_string();
    }
    let mut output = String::new();
    let mut current_width = 0;
    for grapheme in value.graphemes(true) {
        let grapheme_width = unicode_width::UnicodeWidthStr::width(grapheme);
        if current_width + grapheme_width + 1 > max_width {
            break;
        }
        output.push_str(grapheme);
        current_width += grapheme_width;
    }
    output.push('…');
    output
}

pub(crate) fn tail_for_width(value: &str, max_width: usize) -> String {
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;
    let mut tail = String::new();
    let mut width = 0;
    for grapheme in value.graphemes(true).rev() {
        let grapheme_width = UnicodeWidthStr::width(grapheme);
        if width + grapheme_width > max_width {
            break;
        }
        tail.insert_str(0, grapheme);
        width += grapheme_width;
    }
    tail
}

#[cfg(test)]
mod tests {
    use super::truncate_for_width;

    #[test]
    fn truncation_keeps_zwj_emoji_graphemes_intact() {
        assert_eq!(truncate_for_width("alpha 👩‍💻 beta", 9), "alpha 👩‍💻…");
        assert_eq!(truncate_for_width("👩‍💻-service", 5), "👩‍💻-s…");
    }
}

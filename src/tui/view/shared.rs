use super::*;

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
    let footer = Rect::new(
        area.x,
        area.bottom() - 1,
        area.width.saturating_sub(50).min(area.width / 2),
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

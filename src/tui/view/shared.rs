use super::*;

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

#[cfg(test)]
mod tests {
    use super::truncate_for_width;

    #[test]
    fn truncation_keeps_zwj_emoji_graphemes_intact() {
        assert_eq!(truncate_for_width("alpha 👩‍💻 beta", 9), "alpha 👩‍💻…");
        assert_eq!(truncate_for_width("👩‍💻-service", 5), "👩‍💻-s…");
    }
}

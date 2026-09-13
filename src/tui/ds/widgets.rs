use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use super::theme::Theme;

/// Renders the canonical breadcrumb header according to Figma Components (828:2)
/// e.g. "DASHBOARD / AirTCP / JP-Edge-03"
pub(crate) fn render_breadcrumb(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    segments: &[&str],
) {
    let mut spans = Vec::new();
    for (i, seg) in segments.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" / ", theme.style_muted()));
        }
        spans.push(Span::styled(*seg, theme.style_breadcrumb()));
    }
    let p = Paragraph::new(Line::from(spans)).style(theme.style_base());
    frame.render_widget(p, area);
}

/// Renders the one-line footer: shortcuts left-aligned, status right-aligned
/// e.g. "Ctrl+K menu   c connections   i quality   o settings   ? help" (left)
///      "GLOBAL NET STABLE  ↓3.9M/s  ↑2.1M/s" (right)
pub(crate) fn render_footer(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    shortcuts: &[(&str, &str)],
    status_text: &str,
    traffic_rates: Option<(&str, &str)>, // (down, up)
) {
    let mut left_spans = Vec::new();
    for (i, (key, label)) in shortcuts.iter().enumerate() {
        if i > 0 {
            left_spans.push(Span::raw("   "));
        }
        left_spans.push(Span::styled(*key, theme.style_footer_keys()));
        left_spans.push(Span::raw(" "));
        left_spans.push(Span::styled(*label, theme.style_muted()));
    }
    let left_line = Line::from(left_spans);

    let mut right_spans = Vec::new();
    right_spans.push(Span::styled(status_text, theme.style_footer_status()));
    if let Some((down, up)) = traffic_rates {
        right_spans.push(Span::raw("  "));
        right_spans.push(Span::styled(format!("↓{}", down), theme.style_muted()));
        right_spans.push(Span::raw("  "));
        right_spans.push(Span::styled(format!("↑{}", up), theme.style_muted()));
    }
    let right_line = Line::from(right_spans);

    let left_width = area.width.saturating_sub(35);
    let [left_area, right_area] = Layout::horizontal([
        Constraint::Length(left_width),
        Constraint::Min(35),
    ])
    .areas(area);

    frame.render_widget(Paragraph::new(left_line).style(theme.style_base()), left_area);
    frame.render_widget(
        Paragraph::new(right_line)
            .alignment(Alignment::Right)
            .style(theme.style_base()),
        right_area,
    );
}

/// Renders the Terminal Contract Unsupported Guard if window is smaller than 80x24
pub(crate) fn render_unsupported_guard(frame: &mut Frame, area: Rect, theme: &Theme) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.style_error())
        .title(" Terminal Geometry ")
        .style(theme.style_base());

    let text = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("Resize terminal to at least ", theme.style_warning()),
            Span::styled("80x24", theme.style_success()),
        ]),
        Line::from(vec![
            Span::styled(format!("Current viewport: {} cols x {} rows", area.width, area.height), theme.style_muted()),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("Press ", theme.style_muted()),
            Span::styled("q", theme.style_footer_keys()),
            Span::styled(" to quit, ", theme.style_muted()),
            Span::styled("?", theme.style_footer_keys()),
            Span::styled(" for help", theme.style_muted()),
        ]),
    ];

    let p = Paragraph::new(text)
        .block(block)
        .alignment(Alignment::Center);

    let [_, center_row, _] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(8),
        Constraint::Fill(1),
    ])
    .areas(area);

    let [_, center_box, _] = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(area.width.min(54)),
        Constraint::Fill(1),
    ])
    .areas(center_row);

    frame.render_widget(Clear, center_box);
    frame.render_widget(p, center_box);
}

/// Centered modal dialog wrapper with backdrop clearing and Figma dialog borders
#[allow(dead_code)]
pub(crate) fn render_dialog_frame<F>(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    title: &str,
    width: u16,
    height: u16,
    render_inner: F,
) where
    F: FnOnce(&mut Frame, Rect),
{
    let target_width = width.min(area.width.saturating_sub(4));
    let target_height = height.min(area.height.saturating_sub(2));

    let [_, center_row, _] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(target_height),
        Constraint::Fill(1),
    ])
    .areas(area);

    let [_, dialog_area, _] = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(target_width),
        Constraint::Fill(1),
    ])
    .areas(center_row);

    frame.render_widget(Clear, dialog_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.style_focused_row())
        .title(Span::styled(format!(" {} ", title), theme.style_breadcrumb()))
        .style(theme.style_base());

    let inner = block.inner(dialog_area);
    frame.render_widget(block, dialog_area);

    render_inner(frame, inner);
}

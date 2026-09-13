use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use super::theme::Theme;
use crate::tui_state::OperationalWorkspace;

/// Renders the operational top header navigation bar
/// Layout:
/// - Left: SING-BOX TUI · <WORKSPACE> · <SELECTOR_NAME>
/// - Center / Hints: [Tab] Switch Workspace  [Ctrl+K] Actions
/// - Right: badges for TUN: [ON/OFF]  SYS PROXY: [ON/OFF]  CLASH: [RULE/GLOBAL/DIRECT]
pub(crate) fn render_top_header(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    workspace: OperationalWorkspace,
    selector_name: &str,
    tun_enabled: bool,
    system_proxy_enabled: bool,
    clash_mode: &str,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    frame.render_widget(Clear, area);
    frame.render_widget(Block::default().style(theme.style_base()), area);

    let mut left_spans = Vec::new();
    left_spans.push(Span::styled("SING-BOX TUI", theme.style_breadcrumb()));
    left_spans.push(Span::styled(" · ", theme.style_muted()));
    left_spans.push(Span::styled(workspace.header_label(), theme.style_breadcrumb()));
    left_spans.push(Span::styled(" · ", theme.style_muted()));
    let sel = if selector_name.is_empty() { "—" } else { selector_name };
    left_spans.push(Span::styled(sel, theme.style_breadcrumb()));

    let mut hint_spans = Vec::new();
    hint_spans.push(Span::styled("[Tab]", theme.style_footer_keys()));
    hint_spans.push(Span::raw(" "));
    hint_spans.push(Span::styled("Switch Workspace", theme.style_muted()));
    hint_spans.push(Span::raw("  "));
    hint_spans.push(Span::styled("[Ctrl+K]", theme.style_footer_keys()));
    hint_spans.push(Span::raw(" "));
    hint_spans.push(Span::styled("Actions", theme.style_muted()));

    let mut right_spans = Vec::new();
    right_spans.push(Span::styled("TUN: ", theme.style_muted()));
    if tun_enabled {
        right_spans.push(Span::styled("[ON]", theme.style_success()));
    } else {
        right_spans.push(Span::styled("[OFF]", theme.style_muted()));
    }
    right_spans.push(Span::raw("  "));

    right_spans.push(Span::styled("SYS PROXY: ", theme.style_muted()));
    if system_proxy_enabled {
        right_spans.push(Span::styled("[ON]", theme.style_success()));
    } else {
        right_spans.push(Span::styled("[OFF]", theme.style_muted()));
    }
    right_spans.push(Span::raw("  "));

    right_spans.push(Span::styled("CLASH: ", theme.style_muted()));
    let (clash_label, clash_style) = match clash_mode.to_ascii_lowercase().as_str() {
        "rule" | "规则" => ("RULE", theme.style_success()),
        "global" | "全局" => ("GLOBAL", theme.style_warning()),
        "direct" | "直连" => ("DIRECT", theme.style_muted()),
        _ if clash_mode.is_empty() || clash_mode == "—" => ("DIRECT", theme.style_muted()),
        _ => (clash_mode, theme.style_muted()),
    };
    right_spans.push(Span::styled(format!("[{}]", clash_label), clash_style));

    let right_width = 44.min(area.width);
    let left_width = area.width.saturating_sub(right_width);

    if area.height >= 2 && area.width < 90 {
        let [row0, row1] = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(area);
        let ctrl_k_width = 18.min(area.width);
        let row0_left_width = area.width.saturating_sub(ctrl_k_width);
        let [row0_left, row0_right] = Layout::horizontal([
            Constraint::Length(row0_left_width),
            Constraint::Min(ctrl_k_width),
        ]).areas(row0);

        frame.render_widget(Paragraph::new(Line::from(left_spans)).style(theme.style_base()), row0_left);
        let ctrl_k_spans = vec![
            Span::styled("[Ctrl+K]", theme.style_footer_keys()),
            Span::raw(" "),
            Span::styled("Actions", theme.style_muted()),
        ];
        frame.render_widget(
            Paragraph::new(Line::from(ctrl_k_spans))
                .alignment(Alignment::Right)
                .style(theme.style_base()),
            row0_right,
        );

        let [row1_left, row1_right] = Layout::horizontal([
            Constraint::Length(left_width),
            Constraint::Min(right_width),
        ]).areas(row1);

        let tab_spans = vec![
            Span::styled("[Tab]", theme.style_footer_keys()),
            Span::raw(" "),
            Span::styled("Switch Workspace", theme.style_muted()),
        ];
        frame.render_widget(Paragraph::new(Line::from(tab_spans)).style(theme.style_base()), row1_left);
        frame.render_widget(
            Paragraph::new(Line::from(right_spans))
                .alignment(Alignment::Right)
                .style(theme.style_base()),
            row1_right,
        );
    } else if area.height >= 2 {
        let [row0, row1] = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(area);
        let [row0_left, row0_right] = Layout::horizontal([
            Constraint::Length(left_width),
            Constraint::Min(right_width),
        ])
        .areas(row0);

        frame.render_widget(Paragraph::new(Line::from(left_spans)).style(theme.style_base()), row0_left);
        frame.render_widget(
            Paragraph::new(Line::from(right_spans))
                .alignment(Alignment::Right)
                .style(theme.style_base()),
            row0_right,
        );
        frame.render_widget(Paragraph::new(Line::from(hint_spans)).style(theme.style_base()), row1);
    } else {
        if left_width >= 72 {
            left_spans.push(Span::raw("   "));
            left_spans.extend(hint_spans);
        }
        let [left_area, right_area] = Layout::horizontal([
            Constraint::Length(left_width),
            Constraint::Min(right_width),
        ])
        .areas(area);

        frame.render_widget(Paragraph::new(Line::from(left_spans)).style(theme.style_base()), left_area);
        frame.render_widget(
            Paragraph::new(Line::from(right_spans))
                .alignment(Alignment::Right)
                .style(theme.style_base()),
            right_area,
        );
    }
}

/// Renders the canonical breadcrumb header according to Figma Components (828:2)
/// e.g. "DASHBOARD / AirTCP / JP-Edge-03"
#[allow(dead_code)]
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
#[allow(dead_code)]
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
#[allow(dead_code)]
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

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn buffer_to_text(buffer: &ratatui::buffer::Buffer) -> String {
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        text
    }

    #[test]
    fn top_header_renders_left_breadcrumb_badges_and_hints_120x1() {
        let backend = TestBackend::new(120, 1);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = Theme::default();

        terminal
            .draw(|f| {
                render_top_header(
                    f,
                    f.area(),
                    &theme,
                    OperationalWorkspace::Internet,
                    "Proxy",
                    true,
                    false,
                    "RULE",
                );
            })
            .unwrap();

        let t = buffer_to_text(terminal.backend().buffer());
        assert!(t.contains("SING-BOX TUI · INTERNET · Proxy"));
        assert!(t.contains("[Tab] Switch Workspace"));
        assert!(t.contains("[Ctrl+K] Actions"));
        assert!(t.contains("TUN: [ON]"));
        assert!(t.contains("SYS PROXY: [OFF]"));
        assert!(t.contains("CLASH: [RULE]"));
    }

    #[test]
    fn top_header_renders_private_access_and_2_row_layout() {
        let backend = TestBackend::new(80, 2);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = Theme::default();

        terminal
            .draw(|f| {
                render_top_header(
                    f,
                    f.area(),
                    &theme,
                    OperationalWorkspace::PrivateAccess,
                    "GLOBAL",
                    false,
                    true,
                    "GLOBAL",
                );
            })
            .unwrap();

        let t = buffer_to_text(terminal.backend().buffer());
        assert!(t.contains("SING-BOX TUI · PRIVATE ACCESS · GLOBAL"));
        assert!(t.contains("[Tab] Switch Workspace"));
        assert!(t.contains("[Ctrl+K] Actions"));
        assert!(t.contains("TUN: [OFF]"));
        assert!(t.contains("SYS PROXY: [ON]"));
        assert!(t.contains("CLASH: [GLOBAL]"));
    }
}


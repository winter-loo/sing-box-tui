use super::*;

pub(crate) struct OnboardingState {
    pub(crate) input: String,
    pub(crate) message: String,
}

pub(crate) fn draw_onboarding_panel(frame: &mut Frame, onboarding: &OnboardingState) {
    let frame_area = frame.area();
    let area = centered_rect(86, 13, frame_area);
    frame.render_widget(Clear, area);
    let lines = vec![
        Line::from("First run setup"),
        Line::raw(""),
        Line::from("Paste one sing-box subscription URL and press Enter to save it to .suburl."),
        Line::from("Press s to skip, or Esc to keep this wizard for next time."),
        Line::raw(""),
        Line::from(vec![
            Span::styled("URL: ", Style::default().fg(Color::Cyan)),
            Span::raw(onboarding.input.as_str()),
        ]),
        Line::raw(""),
        Line::from(onboarding.message.as_str()),
    ];
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .title("Welcome")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Green)),
        ),
        area,
    );
}

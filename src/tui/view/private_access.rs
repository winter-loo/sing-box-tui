use super::*;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum IntranetDetailSection {
    Dns,
    Routes,
    Domains,
}

impl IntranetDetailSection {
    pub(crate) fn key(self) -> &'static str {
        match self {
            Self::Dns => "dns",
            Self::Routes => "routes",
            Self::Domains => "domains",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct IntranetDetailSectionRange {
    pub(crate) section: IntranetDetailSection,
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) foldable: bool,
}

pub(crate) struct IntranetDetailView {
    pub(crate) lines: Vec<Line<'static>>,
    pub(crate) sections: Vec<IntranetDetailSectionRange>,
}

#[derive(Clone, Debug)]
pub(crate) struct PrivateAccessProgressModal {
    pub(crate) profile_index: usize,
    pub(crate) title: String,
    pub(crate) entries: Vec<PrivateAccessProgressEntry>,
    pub(crate) done: bool,
}

pub(crate) struct PrivateAccessAuthModal {
    pub(crate) profile_index: usize,
    pub(crate) service: String,
    pub(crate) session_id: String,
    pub(crate) challenge_id: String,
    pub(crate) title: String,
    pub(crate) message: String,
    pub(crate) fields: Vec<PrivateAccessAuthField>,
    pub(crate) buttons: Vec<String>,
    pub(crate) inputs: Vec<String>,
    pub(crate) field_index: usize,
    pub(crate) error: Option<String>,
}

impl Drop for PrivateAccessAuthModal {
    fn drop(&mut self) {
        self.session_id.zeroize();
        self.challenge_id.zeroize();
        self.inputs.zeroize();
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PrivateAccessProgressEntry {
    pub(crate) tone: PrivateAccessProgressTone,
    pub(crate) text: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PrivateAccessProgressTone {
    Info,
    Success,
    Error,
}

impl PrivateAccessProgressTone {
    fn prefix(self) -> &'static str {
        match self {
            Self::Info => "[..] ",
            Self::Success => "[OK] ",
            Self::Error => "[ERR] ",
        }
    }

    fn style(self) -> Style {
        match self {
            Self::Info => Style::default().fg(Color::Cyan),
            Self::Success => Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
            Self::Error => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        }
    }
}

pub(crate) struct IntranetDetailSnapshot<'a> {
    pub(crate) profile: &'a PrivateAccessProfileRuntime,
    pub(crate) expanded_sections: &'a BTreeSet<String>,
    pub(crate) scroll: u16,
    pub(crate) active: bool,
}

pub(crate) fn private_access_progress_title(profile: &PrivateAccessProfileRuntime) -> String {
    format!(
        "Private Access - {} ({})",
        profile.id,
        profile.mode.as_str()
    )
}

pub(crate) fn private_access_state_badge(state: PrivateAccessState) -> &'static str {
    match state {
        PrivateAccessState::Disabled => "DISABLED",
        PrivateAccessState::Disconnected => "DISCONNECTED",
        PrivateAccessState::Connecting => "CONNECTING",
        PrivateAccessState::Connected => "CONNECTED",
        PrivateAccessState::Disconnecting => "DISCONNECTING",
        PrivateAccessState::Error => "ERROR",
    }
}

pub(crate) fn private_access_state_style(state: &PrivateAccessState) -> Style {
    match state {
        PrivateAccessState::Connected => Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD),
        PrivateAccessState::Connecting | PrivateAccessState::Disconnecting => Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
        PrivateAccessState::Error => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        PrivateAccessState::Disabled | PrivateAccessState::Disconnected => {
            Style::default().fg(Color::DarkGray)
        }
    }
}

pub(crate) fn private_access_detail_view(
    profile: &PrivateAccessProfileRuntime,
    is_expanded: impl Fn(IntranetDetailSection) -> bool,
) -> IntranetDetailView {
    let state_label = if profile.background_pid.is_some() {
        "BACKGROUND"
    } else {
        private_access_state_badge(profile.state.clone())
    };
    let gateway = if profile.server.trim().is_empty() {
        "not configured".to_string()
    } else {
        format!("{}:{}", profile.server, profile.port)
    };
    let data_plane = match profile.mode {
        PrivateAccessMode::Tun => "TUN".to_string(),
        PrivateAccessMode::Bridge => profile
            .bridge
            .as_ref()
            .map(|bridge| format!("{} at {}", bridge.kind, bridge.listen))
            .unwrap_or_else(|| format!("HTTP bridge at {}", profile.bridge_listen)),
    };
    let capabilities = [
        profile
            .manifest
            .capabilities
            .pushed_routes
            .then_some("routes"),
        profile.manifest.capabilities.pushed_dns.then_some("DNS"),
        profile
            .manifest
            .capabilities
            .local_http_bridge
            .then_some("HTTP bridge"),
        profile
            .manifest
            .capabilities
            .graceful_disconnect
            .then_some("graceful disconnect"),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(", ");

    let mut lines = vec![
        Line::from(vec![
            Span::styled("State: ", Style::default().fg(Color::DarkGray)),
            Span::styled(state_label, private_access_state_style(&profile.state)),
        ]),
        private_access_detail_line("Service", &profile.manifest.name),
        private_access_detail_line(
            "Protocol",
            &format!(
                "{} (service v{})",
                profile.manifest.protocol, profile.manifest.version
            ),
        ),
        private_access_detail_line("Gateway", &gateway),
        private_access_detail_line(
            "TLS verification",
            if profile.tls_verify {
                "enabled"
            } else {
                "disabled"
            },
        ),
        private_access_detail_line("Data plane", &data_plane),
    ];
    if !capabilities.is_empty() {
        lines.push(private_access_detail_line("Capabilities", &capabilities));
    }
    if let Some(pid) = profile.background_pid {
        lines.push(private_access_detail_line(
            "Process",
            &format!("background pid {pid}"),
        ));
    } else if profile.owns_process() {
        lines.push(private_access_detail_line("Process", "owned by this TUI"));
    }

    let mut sections = Vec::new();
    append_private_access_detail_section(
        &mut lines,
        &mut sections,
        IntranetDetailSection::Dns,
        "DNS servers",
        profile.dns.clone(),
        "No DNS servers have been pushed.",
        is_expanded(IntranetDetailSection::Dns),
    );
    append_private_access_detail_section(
        &mut lines,
        &mut sections,
        IntranetDetailSection::Routes,
        "Routes",
        profile
            .routes
            .iter()
            .map(|route| route.cidr.clone())
            .collect(),
        "No routes have been pushed.",
        is_expanded(IntranetDetailSection::Routes),
    );
    let domains = profile
        .domains
        .iter()
        .map(|domain| format!("exact  {domain}"))
        .chain(
            profile
                .domain_suffixes
                .iter()
                .map(|domain| format!("suffix *.{domain}")),
        )
        .collect();
    append_private_access_detail_section(
        &mut lines,
        &mut sections,
        IntranetDetailSection::Domains,
        "Internal domains",
        domains,
        "No internal domains have been pushed.",
        is_expanded(IntranetDetailSection::Domains),
    );

    if let Some(error) = profile.last_error.as_deref() {
        lines.push(Line::default());
        lines.push(private_access_detail_heading("Last error"));
        lines.push(Line::from(Span::styled(
            error.to_string(),
            Style::default().fg(Color::Red),
        )));
    }

    IntranetDetailView { lines, sections }
}

fn append_private_access_detail_section(
    lines: &mut Vec<Line<'static>>,
    sections: &mut Vec<IntranetDetailSectionRange>,
    section: IntranetDetailSection,
    label: &str,
    items: Vec<String>,
    empty_message: &str,
    expanded: bool,
) {
    const FOLDED_ITEM_LIMIT: usize = 10;

    lines.push(Line::default());
    let start = lines.len();
    let item_count = items.len();
    let foldable = item_count > FOLDED_ITEM_LIMIT;
    let visible_count = if foldable && !expanded {
        FOLDED_ITEM_LIMIT
    } else {
        item_count
    };
    let heading = match (foldable, expanded) {
        (true, true) => format!("▼ {label} ({item_count}) [Enter to fold]"),
        (true, false) => {
            format!("▶ {label} ({item_count}) [showing {FOLDED_ITEM_LIMIT}; Enter to expand]")
        }
        (false, _) => format!("{label} ({item_count})"),
    };
    lines.push(private_access_detail_heading(heading));
    if items.is_empty() {
        lines.push(private_access_detail_empty(empty_message));
    } else {
        lines.extend(
            items
                .into_iter()
                .take(visible_count)
                .map(|item| Line::from(format!("  {item}"))),
        );
        if foldable && !expanded {
            lines.push(Line::from(Span::styled(
                format!("  … {} more item(s)", item_count - visible_count),
                Style::default().fg(Color::DarkGray),
            )));
        }
    }
    sections.push(IntranetDetailSectionRange {
        section,
        start,
        end: lines.len(),
        foldable,
    });
}

fn private_access_detail_line(label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label}: "), Style::default().fg(Color::DarkGray)),
        Span::raw(value.to_string()),
    ])
}

fn private_access_detail_heading(value: impl Into<String>) -> Line<'static> {
    Line::from(Span::styled(
        value.into(),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ))
}

fn private_access_detail_empty(value: &str) -> Line<'static> {
    Line::from(Span::styled(
        format!("  {value}"),
        Style::default().fg(Color::DarkGray),
    ))
}

pub(crate) fn draw_private_access_progress_panel(
    frame: &mut Frame,
    progress: &PrivateAccessProgressModal,
) {
    let frame_area = frame.area();
    let width = frame_area.width.saturating_sub(6).clamp(56, 88);
    let height = (progress.entries.len() as u16 + 4)
        .min(frame_area.height.saturating_sub(4))
        .max(8);
    let area = centered_rect(width, height, frame_area);
    frame.render_widget(Clear, area);

    let max_entries = area.height.saturating_sub(4) as usize;
    let start = progress.entries.len().saturating_sub(max_entries);
    let mut lines = progress
        .entries
        .iter()
        .skip(start)
        .map(|entry| {
            Line::from(vec![
                Span::styled(entry.tone.prefix(), entry.tone.style()),
                Span::raw(truncate_for_width(
                    &entry.text,
                    area.width.saturating_sub(8) as usize,
                )),
            ])
        })
        .collect::<Vec<_>>();
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        if progress.done {
            "Enter/Esc close"
        } else {
            "Private Access is running..."
        },
        Style::default().fg(Color::DarkGray),
    )));

    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .title(progress.title.clone())
                .borders(Borders::ALL)
                .border_style(if progress.done {
                    Style::default().fg(Color::Green)
                } else {
                    Style::default().fg(Color::Cyan)
                }),
        ),
        area,
    );
}

pub(crate) fn draw_private_access_auth_panel(frame: &mut Frame, auth: &PrivateAccessAuthModal) {
    let frame_area = frame.area();
    let width = frame_area.width.saturating_sub(6).clamp(52, 82);
    let message_rows = usize::from(!auth.message.trim().is_empty());
    let height = (auth.fields.len() + message_rows + 6) as u16;
    let area = centered_rect(
        width,
        height.min(frame_area.height.saturating_sub(4)).max(9),
        frame_area,
    );
    frame.render_widget(Clear, area);

    let mut lines = vec![Line::from(vec![
        Span::styled("Enter", Style::default().fg(Color::Cyan)),
        Span::raw(" next/submit  "),
        Span::styled("Esc", Style::default().fg(Color::Cyan)),
        Span::raw(" cancel"),
    ])];
    if !auth.message.trim().is_empty() {
        lines.push(Line::from(auth.message.as_str()));
    }
    lines.push(Line::raw(""));
    let field_start = lines.len();
    for (index, field) in auth.fields.iter().enumerate() {
        let selected = index == auth.field_index;
        let marker = if selected { "> " } else { "  " };
        let value = private_access_auth_display_value(field, &auth.inputs[index]);
        let style = if selected {
            Style::default().bg(Color::Blue)
        } else {
            Style::default()
        };
        lines.push(
            Line::from(vec![
                Span::raw(marker),
                Span::styled(field.label.as_str(), Style::default().fg(Color::Cyan)),
                Span::raw("  "),
                Span::raw(value),
            ])
            .style(style),
        );
    }
    if let Some(error) = auth.error.as_deref() {
        lines.push(Line::raw(""));
        lines.push(Line::styled(error, Style::default().fg(Color::Red)));
    }
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .title(auth.title.as_str())
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Green)),
        ),
        area,
    );

    if let Some(field) = auth.fields.get(auth.field_index) {
        let display = private_access_auth_display_value(field, &auth.inputs[auth.field_index]);
        let cursor_x = area
            .x
            .saturating_add(1)
            .saturating_add(2)
            .saturating_add(unicode_width::UnicodeWidthStr::width(field.label.as_str()) as u16)
            .saturating_add(2)
            .saturating_add(unicode_width::UnicodeWidthStr::width(display.as_str()) as u16)
            .min(area.x.saturating_add(area.width.saturating_sub(2)));
        let cursor_y = area
            .y
            .saturating_add(1)
            .saturating_add(field_start as u16)
            .saturating_add(auth.field_index as u16)
            .min(area.y.saturating_add(area.height.saturating_sub(2)));
        frame.set_cursor_position((cursor_x, cursor_y));
    }
}

pub(crate) fn private_access_auth_display_value(
    _field: &PrivateAccessAuthField,
    input: &str,
) -> String {
    input.to_string()
}

pub(crate) fn private_access_auth_initial_value(
    profile: &PrivateAccessProfileRuntime,
    field: &PrivateAccessAuthField,
) -> String {
    if let Some(option) = field.options.first() {
        return option.value.clone();
    }
    if profile.manifest.id != "sonicwall" {
        return String::new();
    }

    let has_kind_marker = |expected: &str| {
        field
            .kind
            .split(|character: char| {
                character.is_ascii_whitespace() || matches!(character, ',' | ';')
            })
            .any(|marker| marker.eq_ignore_ascii_case(expected))
    };
    if has_kind_marker("is-username") {
        return profile.username.clone();
    }
    if has_kind_marker("is-password") {
        if !profile.password.is_empty() {
            return profile.password.clone();
        }
        if !profile.password_env.is_empty() {
            return env::var(&profile.password_env).unwrap_or_default();
        }
    }

    // A generic sensitive/password field may be an OTP or another dynamic reply.
    // Only the gateway's explicit is-password marker is safe to prefill.
    String::new()
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::private_access::PrivateAccessRoute;

    #[test]
    fn large_intranet_sections_are_folded_by_the_view_interface() {
        let mut profile =
            PrivateAccessProfileRuntime::default_hillstone().expect("Hillstone profile");
        profile.routes = (0..103)
            .map(|index| PrivateAccessRoute {
                cidr: format!("10.20.{index}.0/24"),
            })
            .collect();

        let collapsed = private_access_detail_view(&profile, |_| false);
        let route_range = collapsed
            .sections
            .iter()
            .find(|range| range.section == IntranetDetailSection::Routes)
            .expect("routes section");
        let collapsed_text = collapsed
            .lines
            .into_iter()
            .map(|line| {
                line.spans
                    .into_iter()
                    .map(|span| span.content.into_owned())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(route_range.foldable);
        assert!(collapsed_text.contains("▶ Routes (103)"));
        assert!(collapsed_text.contains("… 93 more item(s)"));
        assert!(collapsed_text.contains("10.20.9.0/24"));
        assert!(!collapsed_text.contains("10.20.10.0/24"));

        let expanded = private_access_detail_view(&profile, |section| {
            section == IntranetDetailSection::Routes
        });
        assert_eq!(expanded.lines.len(), collapsed_text.lines().count() + 92);
    }
}

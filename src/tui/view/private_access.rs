use super::*;
use crate::tui::ds::theme::Theme;
use crate::tui::ds::widgets::{dialog_content_area, render_dialog_frame};
use ratatui::layout::Rect;

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

    fn style(self, theme: &Theme) -> Style {
        match self {
            Self::Info => theme.style_breadcrumb(),
            Self::Success => theme.style_success().add_modifier(Modifier::BOLD),
            Self::Error => theme.style_danger().add_modifier(Modifier::BOLD),
        }
    }
}

pub(crate) struct IntranetDetailSnapshot<'a> {
    pub(crate) profile: &'a PrivateAccessProfileRuntime,
    pub(crate) expanded_sections: &'a BTreeSet<String>,
    pub(crate) focused_section: IntranetDetailSection,
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
    let theme = Theme::detect();
    match state {
        PrivateAccessState::Connected => theme.style_success().add_modifier(Modifier::BOLD),
        PrivateAccessState::Connecting | PrivateAccessState::Disconnecting => {
            theme.style_warning().add_modifier(Modifier::BOLD)
        }
        PrivateAccessState::Error => theme.style_danger().add_modifier(Modifier::BOLD),
        PrivateAccessState::Disabled | PrivateAccessState::Disconnected => theme.style_muted(),
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

    let theme = Theme::detect();
    let mut lines = vec![
        Line::from(vec![
            Span::styled("State: ", theme.style_muted()),
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
            theme.style_danger(),
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
            let theme = Theme::detect();
            lines.push(Line::from(Span::styled(
                format!("  … {} more item(s)", item_count - visible_count),
                theme.style_muted(),
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
    let theme = Theme::detect();
    Line::from(vec![
        Span::styled(format!("{label}: "), theme.style_muted()),
        Span::styled(value.to_string(), theme.style_base()),
    ])
}

fn private_access_detail_heading(value: impl Into<String>) -> Line<'static> {
    let theme = Theme::detect();
    Line::from(Span::styled(
        value.into(),
        theme.style_breadcrumb().add_modifier(Modifier::BOLD),
    ))
}

fn private_access_detail_empty(value: &str) -> Line<'static> {
    let theme = Theme::detect();
    Line::from(Span::styled(
        format!("  {value}"),
        theme.style_muted(),
    ))
}

pub(crate) fn draw_private_access_progress_panel(
    frame: &mut Frame,
    progress: &PrivateAccessProgressModal,
) {
    let theme = Theme::detect();
    let frame_area = frame.area();
    let width = frame_area.width.saturating_sub(6).clamp(56, 88);
    let height = (progress.entries.len() as u16 + 5)
        .min(frame_area.height.saturating_sub(4))
        .max(8);

    render_dialog_frame(
        frame,
        frame_area,
        &theme,
        &format!(" PRIVATE ACCESS · {} ", progress.title),
        width,
        height,
        |frame, inner_area| {
            let max_entries = inner_area.height.saturating_sub(2) as usize;
            let start = progress.entries.len().saturating_sub(max_entries);
            let mut lines = progress
                .entries
                .iter()
                .skip(start)
                .map(|entry| {
                    Line::from(vec![
                        Span::styled(entry.tone.prefix(), entry.tone.style(&theme)),
                        Span::raw(truncate_for_width(
                            &entry.text,
                            inner_area.width.saturating_sub(6) as usize,
                        )),
                    ])
                })
                .collect::<Vec<_>>();
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                if progress.done {
                    "[Enter/Esc] Close"
                } else {
                    "Private Access is running..."
                },
                theme.style_muted(),
            )));

            frame.render_widget(Paragraph::new(lines), inner_area);
        },
    );
}

pub(crate) fn draw_private_access_auth_panel(frame: &mut Frame, auth: &PrivateAccessAuthModal) {
    let theme = Theme::detect();
    let frame_area = frame.area();

    render_dialog_frame(
        frame,
        frame_area,
        &theme,
        "",
        82,
        20,
        |frame, inner_area| {
            if inner_area.width == 0 || inner_area.height == 0 {
                return;
            }

            let content_area = dialog_content_area(inner_area);
            frame.render_widget(
                Paragraph::new(Line::styled(
                    format!("{} / AUTHENTICATION", auth.title),
                    theme.style_breadcrumb(),
                )),
                Rect { height: 1, ..content_area },
            );

            let error_row = inner_area.height.saturating_sub(2);
            let fields_height = auth.fields.len().min(u16::MAX as usize) as u16;
            let latest_field_start = error_row.saturating_sub(fields_height);
            let field_start = 6.min(latest_field_start).max(1);
            let controls_row = if field_start >= 4 { 2 } else { 1 };
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("Enter", theme.style_footer_keys()),
                    Span::raw(" next/submit   "),
                    Span::styled("Tab", theme.style_footer_keys()),
                    Span::raw(" field   "),
                    Span::styled("Esc", theme.style_footer_keys()),
                    Span::raw(" cancel"),
                ])),
                Rect {
                    y: inner_area.y.saturating_add(controls_row),
                    height: 1,
                    ..content_area
                },
            );

            if !auth.message.trim().is_empty() && field_start >= 3 {
                let message_row = field_start.saturating_sub(2);
                frame.render_widget(
                    Paragraph::new(Line::from(truncate_for_width(
                        auth.message.as_str(),
                        content_area.width as usize,
                    ))),
                    Rect {
                        y: inner_area.y.saturating_add(message_row),
                        height: 1,
                        ..content_area
                    },
                );
            }

            let max_label_width = auth
                .fields
                .iter()
                .map(|field| unicode_width::UnicodeWidthStr::width(field.label.as_str()))
                .max()
                .unwrap_or(0);
            let label_width = max_label_width.min(content_area.width.saturating_sub(5) as usize);
            let value_offset = 2usize.saturating_add(label_width).saturating_add(2);

            for (index, field) in auth.fields.iter().enumerate() {
                let row = field_start.saturating_add(index as u16);
                if row >= error_row {
                    break;
                }
                let is_focused = index == auth.field_index;
                let input = auth.inputs.get(index).map(String::as_str).unwrap_or_default();
                let display = private_access_auth_display_value(field, input);
                let label = truncate_for_width(field.label.as_str(), label_width);
                let label_display_width = unicode_width::UnicodeWidthStr::width(label.as_str());
                let label_padding = " ".repeat(label_width.saturating_sub(label_display_width));
                let value_width = content_area.width.saturating_sub(value_offset as u16) as usize;
                let value = truncate_for_width(display.as_str(), value_width);
                let style = if is_focused {
                    theme.style_focused_row()
                } else {
                    Style::default()
                };
                frame.render_widget(
                    Paragraph::new(Line::from(vec![
                        Span::raw("  "),
                        Span::styled(label, theme.style_breadcrumb()),
                        Span::raw(label_padding),
                        Span::raw("  "),
                        Span::raw(value),
                    ]))
                    .style(style),
                    Rect {
                        y: inner_area.y.saturating_add(row),
                        height: 1,
                        ..content_area
                    },
                );
            }

            if let Some(error) = auth.error.as_deref() {
                frame.render_widget(
                    Paragraph::new(Line::styled(
                        truncate_for_width(error, content_area.width as usize),
                        theme.style_danger(),
                    )),
                    Rect {
                        y: inner_area.y.saturating_add(error_row),
                        height: 1,
                        ..content_area
                    },
                );
            }

            if let Some(field) = auth.fields.get(auth.field_index) {
                let input = auth
                    .inputs
                    .get(auth.field_index)
                    .map(String::as_str)
                    .unwrap_or_default();
                let display = private_access_auth_display_value(field, input);
                let cursor_x = content_area
                    .x
                    .saturating_add(value_offset as u16)
                    .saturating_add(unicode_width::UnicodeWidthStr::width(display.as_str()) as u16)
                    .min(content_area.x.saturating_add(content_area.width.saturating_sub(1)));
                let cursor_y = inner_area
                    .y
                    .saturating_add(field_start)
                    .saturating_add(auth.field_index as u16)
                    .min(inner_area.y.saturating_add(inner_area.height.saturating_sub(1)));
                frame.set_cursor_position((cursor_x, cursor_y));
            }
        },
    );
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
    use crate::private_access::{PrivateAccessAuthField, PrivateAccessRoute};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn auth_field(label: &str) -> PrivateAccessAuthField {
        PrivateAccessAuthField {
            id: label.to_ascii_lowercase().replace(' ', "-"),
            label: label.to_string(),
            kind: "text".to_string(),
            sensitive: false,
            required: true,
            options: Vec::new(),
        }
    }

    fn render_auth(width: u16, height: u16) -> (Vec<String>, (u16, u16)) {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let auth = PrivateAccessAuthModal {
            profile_index: 0,
            service: "sonicwall".to_string(),
            session_id: "session".to_string(),
            challenge_id: "challenge".to_string(),
            title: "SONICWALL-HQ".to_string(),
            message: "Gateway requests a dynamic code.".to_string(),
            fields: vec![
                auth_field("Domain account"),
                auth_field("Domain password"),
                auth_field("Dynamic code"),
                auth_field("Realm"),
            ],
            buttons: Vec::new(),
            inputs: vec![
                "demo-user".to_string(),
                "example-secret".to_string(),
                "123456".to_string(),
                "Hundsun".to_string(),
            ],
            field_index: 2,
            error: Some("Dynamic code is required.".to_string()),
        };

        terminal
            .draw(|frame| draw_private_access_auth_panel(frame, &auth))
            .expect("authentication dialog renders");
        let cursor = terminal.get_cursor_position().expect("cursor position");
        let lines = terminal
            .backend()
            .buffer()
            .content
            .chunks(width as usize)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect();
        (lines, (cursor.x, cursor.y))
    }

    #[test]
    fn authentication_dialog_matches_canonical_geometry_and_fixed_value_column() {
        let (lines, cursor) = render_auth(120, 30);
        let top = lines
            .iter()
            .position(|line| line.contains('┌'))
            .expect("dialog top border");
        let bottom = lines
            .iter()
            .position(|line| line.contains('└'))
            .expect("dialog bottom border");
        let left = lines[top]
            .chars()
            .position(|character| character != ' ')
            .expect("left border");
        let right = lines[top]
            .chars()
            .collect::<Vec<_>>()
            .iter()
            .rposition(|character| *character != ' ')
            .expect("right border");

        assert_eq!(bottom - top + 1, 20);
        assert_eq!(right - left + 1, 82);
        assert!(!lines[top].contains("AUTHENTICATION"));
        assert!(lines[top + 1].contains("SONICWALL-HQ / AUTHENTICATION"));

        let account = lines.iter().find(|line| line.contains("demo-user")).unwrap();
        let password = lines
            .iter()
            .find(|line| line.contains("example-secret"))
            .unwrap();
        let code = lines.iter().find(|line| line.contains("123456")).unwrap();
        assert_eq!(account.find("demo-user"), password.find("example-secret"));
        assert_eq!(account.find("demo-user"), code.find("123456"));
        let code_start = unicode_width::UnicodeWidthStr::width(
            code.split_once("123456").expect("code value").0,
        );
        assert_eq!(cursor.0 as usize, code_start + 6);
        assert_eq!(cursor.1 as usize, lines.iter().position(|line| line == code).unwrap());
        assert!(lines[bottom - 2].contains("Dynamic code is required."));
    }

    #[test]
    fn authentication_dialog_keeps_fields_and_error_inside_a_compact_viewport() {
        let (lines, cursor) = render_auth(80, 24);
        let text = lines.join("\n");
        let top = lines.iter().position(|line| line.contains('┌')).unwrap();
        let bottom = lines.iter().position(|line| line.contains('└')).unwrap();
        let left = lines[top]
            .chars()
            .position(|character| character != ' ')
            .unwrap();
        let right = lines[top]
            .chars()
            .collect::<Vec<_>>()
            .iter()
            .rposition(|character| *character != ' ')
            .unwrap();

        assert!(text.contains("AUTHENTICATION"));
        assert!(text.contains("Dynamic code"));
        assert!(text.contains("Dynamic code is required."));
        assert_eq!(bottom - top + 1, 20);
        assert_eq!(right - left + 1, 76);
        assert!(cursor.0 < 80);
        assert!(cursor.1 < 24);
    }

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

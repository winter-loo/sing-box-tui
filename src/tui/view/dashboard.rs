use super::*;
use crate::automatic_selection::{NodeViewId, RankingPolicy};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Focus {
    Groups,
    Members,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) enum NodeViewPanel {
    #[default]
    CurrentSelector,
    Streaming,
    Custom(NodeViewId),
}

impl NodeViewPanel {
    pub(crate) fn id(&self) -> NodeViewId {
        match self {
            Self::CurrentSelector => NodeViewId::current_selector(),
            Self::Streaming => NodeViewId::streaming(),
            Self::Custom(id) => id.clone(),
        }
    }

    pub(crate) fn builtin_ranking_policy(&self) -> Option<RankingPolicy> {
        match self {
            Self::CurrentSelector => Some(RankingPolicy::Balanced),
            Self::Streaming => Some(RankingPolicy::Throughput),
            Self::Custom(_) => None,
        }
    }

    pub(crate) fn from_id(id: &NodeViewId) -> Self {
        match id.as_str() {
            crate::automatic_selection::CURRENT_SELECTOR_VIEW_ID => Self::CurrentSelector,
            crate::automatic_selection::STREAMING_VIEW_ID => Self::Streaming,
            _ => Self::Custom(id.clone()),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NodeViewTab {
    pub(crate) label: String,
    pub(crate) count: usize,
    pub(crate) spinner: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LeftPaneSection {
    Internet,
    Intranet,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct InternetRow {
    pub(crate) name: String,
    pub(crate) current: String,
    pub(crate) is_current: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct IntranetRow {
    pub(crate) id: String,
    pub(crate) state: PrivateAccessState,
    pub(crate) background: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CandidateTone {
    Pending,
    Success,
    Error,
    Missing,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CandidateRow {
    pub(crate) name: String,
    pub(crate) is_current: bool,
    pub(crate) reachability: String,
    pub(crate) marker: String,
    pub(crate) compact_marker: String,
    pub(crate) tone: CandidateTone,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CandidateNotice {
    pub(crate) title: String,
    pub(crate) message: String,
    pub(crate) error: bool,
}

pub(crate) struct DashboardSnapshot<'a> {
    pub(crate) focus: Focus,
    pub(crate) left_pane_section: LeftPaneSection,
    pub(crate) internet_rows: Vec<InternetRow>,
    pub(crate) internet_selected: usize,
    pub(crate) intranet_rows: Vec<IntranetRow>,
    pub(crate) intranet_selected: usize,
    pub(crate) candidate_title: String,
    pub(crate) candidate_notice: Option<CandidateNotice>,
    pub(crate) node_view_tabs: Vec<NodeViewTab>,
    pub(crate) active_node_view_tab: usize,
    pub(crate) candidate_rows: Vec<CandidateRow>,
    pub(crate) candidate_selected: Option<usize>,
    pub(crate) pending_animation_bright: bool,
    pub(crate) intranet_detail: Option<IntranetDetailSnapshot<'a>>,
    pub(crate) status: StatusSnapshot,
    pub(crate) flash: Option<String>,
    pub(crate) node_quality_detail: Option<&'a NodeQualityDetailState>,
    pub(crate) connections: Option<ConnectionsPanelSnapshot<'a>>,
    pub(crate) help_index: Option<usize>,
    pub(crate) usability_probe_diagnostics: &'a [ManifestDiagnostic],
    pub(crate) settings: Option<SettingsPanelSnapshot>,
    pub(crate) onboarding: Option<&'a OnboardingState>,
    pub(crate) private_access_progress: Option<&'a PrivateAccessProgressModal>,
    pub(crate) private_access_auth: Option<&'a PrivateAccessAuthModal>,
}

#[cfg(test)]
#[path = "dashboard_tests.rs"]
mod tests;

pub(crate) fn render(frame: &mut Frame, snapshot: &DashboardSnapshot<'_>) {
    let status_lines = status_lines(&snapshot.status);
    let status_footer = status_footer_line(&snapshot.status.footer);
    let status_line_count = status_lines.len() as u16;
    let status_box_height = status_line_count.saturating_add(2).max(3);
    let status_region_height = status_box_height.saturating_add(1);
    let [main, status_region] = Layout::vertical([
        Constraint::Min(10),
        Constraint::Length(status_region_height),
    ])
    .areas(frame.area());
    let [status_area, status_footer_area] =
        Layout::vertical([Constraint::Length(status_box_height), Constraint::Length(1)])
            .areas(status_region);
    let [groups_area, members_area] =
        Layout::horizontal([Constraint::Percentage(32), Constraint::Percentage(68)]).areas(main);
    let (internet_area, intranet_area) = if !snapshot.intranet_rows.is_empty() {
        let [internet_area, intranet_area] =
            Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)])
                .areas(groups_area);
        (internet_area, Some(intranet_area))
    } else {
        (groups_area, None)
    };

    let groups = snapshot
        .internet_rows
        .iter()
        .map(|row| {
            let mut style = Style::default().fg(Color::Cyan);
            if row.is_current {
                style = style.fg(Color::Green).add_modifier(Modifier::BOLD);
            }
            ListItem::new(Line::from(vec![
                Span::styled(
                    truncate_for_width(&row.name, internet_area.width.saturating_sub(18) as usize),
                    style,
                ),
                Span::raw(" "),
                Span::styled(
                    format!("[{}]", truncate_for_width(&row.current, 14)),
                    Style::default().fg(Color::Yellow),
                ),
                Span::raw(if row.is_current { "  *" } else { "" }),
            ]))
        })
        .collect::<Vec<_>>();

    let groups_title = "Internet Proxy";
    let groups_block = Block::default()
        .title(groups_title)
        .borders(Borders::ALL)
        .border_style(border_style(
            snapshot.focus == Focus::Groups
                && snapshot.left_pane_section == LeftPaneSection::Internet,
        ));
    let groups_widget = List::new(groups)
        .block(groups_block)
        .highlight_style(selected_style(
            snapshot.focus == Focus::Groups
                && snapshot.left_pane_section == LeftPaneSection::Internet,
        ))
        .highlight_symbol("> ");
    let mut groups_state = ListState::default().with_selected(
        (snapshot.left_pane_section == LeftPaneSection::Internet)
            .then_some(snapshot.internet_selected),
    );
    frame.render_stateful_widget(groups_widget, internet_area, &mut groups_state);

    if let Some(intranet_area) = intranet_area {
        let profiles = snapshot
            .intranet_rows
            .iter()
            .map(|row| {
                let state_label = if row.background {
                    "BACKGROUND"
                } else {
                    private_access_state_badge(row.state.clone())
                };
                ListItem::new(Line::from(vec![
                    Span::styled(
                        truncate_for_width(
                            &row.id,
                            intranet_area.width.saturating_sub(18) as usize,
                        ),
                        Style::default().fg(Color::Cyan),
                    ),
                    Span::raw("  "),
                    Span::styled(state_label, private_access_state_style(&row.state)),
                ]))
            })
            .collect::<Vec<_>>();
        let intranet_active = snapshot.focus == Focus::Groups
            && snapshot.left_pane_section == LeftPaneSection::Intranet;
        let intranet_block = Block::default()
            .title("Intranet Proxy")
            .borders(Borders::ALL)
            .border_style(border_style(intranet_active));
        let intranet_widget = List::new(profiles)
            .block(intranet_block)
            .highlight_style(selected_style(intranet_active))
            .highlight_symbol("> ");
        let mut intranet_state = ListState::default().with_selected(
            (snapshot.left_pane_section == LeftPaneSection::Intranet)
                .then_some(snapshot.intranet_selected),
        );
        frame.render_stateful_widget(intranet_widget, intranet_area, &mut intranet_state);
    }

    let [tabs_area, candidate_area] =
        Layout::vertical([Constraint::Length(3), Constraint::Min(1)]).areas(members_area);
    let tab_titles = snapshot
        .node_view_tabs
        .iter()
        .map(|tab| {
            if let Some(spinner) = &tab.spinner {
                Line::from(format!("{} {}", tab.label, spinner))
            } else {
                Line::from(format!("{} {}", tab.label, tab.count))
            }
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Tabs::new(tab_titles)
            .select(snapshot.active_node_view_tab)
            .highlight_style(
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            )
            .divider(" │ ")
            .block(
                Block::default()
                    .title("Node views  ←/→")
                    .borders(Borders::ALL)
                    .border_style(border_style(snapshot.focus == Focus::Members)),
            ),
        tabs_area,
    );

    let members = snapshot
        .candidate_rows
        .iter()
        .map(|row| {
            let style = if row.is_current {
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let (marker_style, evidence_style, loading_suffix) = match row.tone {
                CandidateTone::Pending => (
                    pending_candidate_style(snapshot.pending_animation_bright),
                    Style::default().fg(Color::DarkGray),
                    "",
                ),
                CandidateTone::Success => (
                    Style::default().fg(Color::Magenta),
                    Style::default().fg(Color::Magenta),
                    "",
                ),
                CandidateTone::Error => (
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                    "",
                ),
                CandidateTone::Missing => (
                    Style::default().fg(Color::DarkGray),
                    Style::default().fg(Color::DarkGray),
                    "",
                ),
            };
            let current_suffix = if row.is_current { "  *" } else { "" };
            let available = candidate_area.width.saturating_sub(4) as usize;
            let reachability_width = unicode_width::UnicodeWidthStr::width(row.reachability.as_str());
            let suffix_width = if reachability_width > 0 {
                reachability_width + 2
            } else {
                0
            } + unicode_width::UnicodeWidthStr::width(loading_suffix)
                + unicode_width::UnicodeWidthStr::width(current_suffix);
            let name_width = unicode_width::UnicodeWidthStr::width(row.name.as_str());
            let marker = if !row.marker.is_empty()
                && name_width
                    + suffix_width
                    + 2
                    + unicode_width::UnicodeWidthStr::width(row.marker.as_str())
                    <= available
            {
                row.marker.as_str()
            } else if !row.compact_marker.is_empty()
                && name_width
                    + suffix_width
                    + 2
                    + unicode_width::UnicodeWidthStr::width(row.compact_marker.as_str())
                    <= available
            {
                row.compact_marker.as_str()
            } else {
                ""
            };
            let marker_width = if marker.is_empty() {
                0
            } else {
                unicode_width::UnicodeWidthStr::width(marker) + 2
            };
            let visible_name = truncate_for_width(
                &row.name,
                available.saturating_sub(suffix_width + marker_width),
            );
            let mut spans = vec![Span::styled(visible_name, style)];
            if !marker.is_empty() {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(marker.to_string(), marker_style));
            }
            if !row.reachability.is_empty() {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(row.reachability.clone(), evidence_style));
            }
            spans.push(Span::raw(loading_suffix));
            spans.push(Span::raw(current_suffix));
            ListItem::new(Line::from(spans))
        })
        .collect::<Vec<_>>();

    let (candidate_notice_area, candidate_list_area) =
        if let Some(notice) = &snapshot.candidate_notice {
            let inner_width = candidate_area.width.saturating_sub(2).max(1) as usize;
            let wrapped_lines = notice
                .message
                .lines()
                .map(|line| {
                    unicode_width::UnicodeWidthStr::width(line)
                        .div_ceil(inner_width)
                        .max(1)
                })
                .sum::<usize>() as u16;
            let notice_height = wrapped_lines.saturating_add(2).min(candidate_area.height);
            let [notice_area, list_area] =
                Layout::vertical([Constraint::Length(notice_height), Constraint::Min(0)])
                    .areas(candidate_area);
            (Some(notice_area), list_area)
        } else {
            (None, candidate_area)
        };
    if let (Some(notice), Some(notice_area)) = (&snapshot.candidate_notice, candidate_notice_area) {
        frame.render_widget(
            Paragraph::new(notice.message.as_str())
                .style(Style::default().fg(if notice.error {
                    Color::LightRed
                } else {
                    Color::LightYellow
                }))
                .wrap(Wrap { trim: false })
                .block(
                    Block::default()
                        .title(notice.title.as_str())
                        .borders(Borders::ALL),
                ),
            notice_area,
        );
    }

    let members_title = snapshot.candidate_title.clone();
    let members_block = Block::default()
        .title(members_title)
        .borders(Borders::ALL)
        .border_style(border_style(snapshot.focus == Focus::Members));
    let members_widget = List::new(members)
        .block(members_block)
        .highlight_style(selected_style(snapshot.focus == Focus::Members))
        .highlight_symbol("> ");
    let mut members_state = ListState::default().with_selected(snapshot.candidate_selected);
    frame.render_stateful_widget(members_widget, candidate_list_area, &mut members_state);

    if let Some(detail) = snapshot.intranet_detail.as_ref() {
        let profile = detail.profile;
        frame.render_widget(Clear, members_area);
        let detail_view = private_access_detail_view(profile, |section| {
            detail
                .expanded_sections
                .contains(&format!("{}:{}", profile.id, section.key()))
        });
        let details_block = Block::default()
            .title(if detail.scroll == 0 {
                format!("Intranet: {}", profile.id)
            } else {
                format!("Intranet: {} [line {}]", profile.id, detail.scroll + 1)
            })
            .borders(Borders::ALL)
            .border_style(border_style(detail.active));
        let details_inner = details_block.inner(members_area);
        frame.render_widget(details_block, members_area);
        let [details_area, footer_area] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(details_inner);
        let details = Paragraph::new(detail_view.lines)
            .wrap(Wrap { trim: false })
            .scroll((detail.scroll, 0));
        frame.render_widget(details, details_area);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "j/k scroll  Enter expand/fold  V connect/disconnect  o configure",
                Style::default().fg(Color::DarkGray),
            ))),
            footer_area,
        );
    }

    let help =
        Paragraph::new(status_lines).block(Block::default().title("Status").borders(Borders::ALL));
    frame.render_widget(help, status_area);
    frame.render_widget(Paragraph::new(status_footer), status_footer_area);

    if let Some(message) = snapshot.flash.as_deref() {
        let estimated_width = frame
            .area()
            .width
            .saturating_mul(80)
            .saturating_div(100)
            .max(1);
        let wrapped_lines = message
            .lines()
            .map(|line| {
                let width = unicode_width::UnicodeWidthStr::width(line) as u16;
                width
                    .saturating_add(estimated_width - 1)
                    .saturating_div(estimated_width)
                    .max(1)
            })
            .sum::<u16>();
        let height = wrapped_lines
            .saturating_add(2)
            .max(7)
            .min(frame.area().height);
        let area = centered_rect(80, height, frame.area());
        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new(message).block(Block::default().title("Info").borders(Borders::ALL)),
            area,
        );
    }
    if let Some(chart) = snapshot.node_quality_detail {
        draw_node_quality_detail(frame, chart);
    }
    if let Some(connections) = snapshot.connections.as_ref() {
        draw_connections_panel(frame, connections);
    }
    if let Some(help_index) = snapshot.help_index {
        draw_help_panel(frame, help_index, snapshot.usability_probe_diagnostics);
    }
    if let Some(settings) = snapshot.settings.as_ref() {
        draw_settings_panel(frame, settings);
    }
    if let Some(onboarding) = snapshot.onboarding {
        draw_onboarding_panel(frame, onboarding);
    }
    if let Some(progress) = snapshot.private_access_progress {
        draw_private_access_progress_panel(frame, progress);
    }
    if let Some(auth) = snapshot.private_access_auth {
        draw_private_access_auth_panel(frame, auth);
    }
    if let StatusFooter::Filter(input) = &snapshot.status.footer {
        let cursor_x = status_area
            .x
            .saturating_add(1)
            .saturating_add(unicode_width::UnicodeWidthStr::width("Filter: ") as u16)
            .saturating_add(unicode_width::UnicodeWidthStr::width(input.as_str()) as u16);
        let cursor_y = status_area.y.saturating_add(status_line_count);
        frame.set_cursor_position((cursor_x, cursor_y));
    }
}

fn pending_candidate_style(bright: bool) -> Style {
    if bright {
        Style::default()
            .fg(Color::LightYellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

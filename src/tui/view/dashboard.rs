use super::*;
use ratatui::layout::Rect;
use crate::automatic_selection::{NodeViewId, RankingPolicy};
use crate::private_access_session::{PrivateAccessMode, PrivateAccessProfileRuntime};
use crate::tui::ds::theme::Theme;

pub(crate) trait ThemeExt {
    fn detect() -> Self;
    fn style_danger(&self) -> Style;
}

impl ThemeExt for Theme {
    fn detect() -> Self {
        Self::default()
    }

    fn style_danger(&self) -> Style {
        self.style_error()
    }
}

pub(crate) fn reachability_badge_style(
    reachability: &str,
    tone: CandidateTone,
    theme: &Theme,
) -> Style {
    let lower = reachability.to_ascii_lowercase();
    if lower.contains("stable") {
        theme.style_success()
    } else if lower.contains("degraded") {
        theme.style_warning()
    } else if lower.contains("unreachable") || lower.contains("error") {
        theme.style_danger()
    } else if lower.contains("reachable") {
        theme.style_breadcrumb()
    } else {
        match tone {
            CandidateTone::Pending => pending_candidate_style(false),
            CandidateTone::Success => theme.style_success(),
            CandidateTone::Error => theme.style_danger(),
            CandidateTone::Missing => theme.style_muted(),
        }
    }
}

fn candidate_marker_style(
    marker: &str,
    tone: CandidateTone,
    bright: bool,
    theme: &Theme,
) -> Style {
    if tone == CandidateTone::Pending {
        return pending_candidate_style(bright);
    }
    let lower = marker.to_ascii_lowercase();
    if lower.contains("mib/s")
        || lower.contains("mb/s")
        || lower.contains("kib/s")
        || lower.contains("kb/s")
        || lower.contains("ms")
        || lower.contains("cold")
    {
        theme.style_breadcrumb()
    } else {
        match tone {
            CandidateTone::Pending => pending_candidate_style(bright),
            CandidateTone::Success => theme.style_breadcrumb(),
            CandidateTone::Error => theme.style_danger(),
            CandidateTone::Missing => theme.style_muted(),
        }
    }
}

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
    pub(crate) server: String,
    pub(crate) mode: PrivateAccessMode,
    pub(crate) routes_count: usize,
    pub(crate) state: PrivateAccessState,
    pub(crate) background: bool,
}

impl IntranetRow {
    pub(crate) fn from_profile(profile: &PrivateAccessProfileRuntime) -> Self {
        let server = if profile.server.trim().is_empty() {
            "-".to_string()
        } else {
            format!("{}:{}", profile.server, profile.port)
        };
        Self {
            id: profile.id.clone(),
            server,
            mode: profile.mode,
            routes_count: profile.routes.len(),
            state: profile.state.clone(),
            background: profile.background_pid.is_some(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CandidateTone {
    Pending,
    Success,
    Error,
    Missing,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LatencySignalState {
    Untested,
    Reachable { delay_ms: u64 },
    Unreachable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LatencySignalBar {
    pub(crate) height: u8,
    pub(crate) state: LatencySignalState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LatencySignal {
    pub(crate) bars: [LatencySignalBar; 3],
    pub(crate) average_ms: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CandidateRow {
    pub(crate) name: String,
    pub(crate) is_current: bool,
    pub(crate) latency_signal: Option<LatencySignal>,
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
    pub(crate) operational_workspace: crate::tui_state::OperationalWorkspace,
    pub(crate) focus: Focus,
    pub(crate) left_pane_section: LeftPaneSection,
    #[allow(dead_code)]
    pub(crate) internet_rows: Vec<InternetRow>,
    #[allow(dead_code)]
    pub(crate) internet_selected: usize,
    pub(crate) intranet_rows: Vec<IntranetRow>,
    pub(crate) intranet_selected: usize,
    pub(crate) candidate_title: String,
    pub(crate) candidate_notice: Option<CandidateNotice>,
    pub(crate) node_view_tabs: Vec<NodeViewTab>,
    pub(crate) active_node_view_tab: usize,
    pub(crate) candidate_rows: Vec<CandidateRow>,
    pub(crate) candidate_selected: Option<usize>,
    pub(crate) pending_animation_tick: usize,
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

fn latency_signal_glyph(height: u8) -> char {
    // One Braille cell has two dot columns. These glyphs use only the left column, so every
    // signal bar is half a cell wide and the unused right column is a consistent half-cell gap.
    // The circular dots also give the closest portable terminal approximation to rounded ends.
    // Never use the top Braille dot: keeping the upper quarter of every cell empty prevents bars
    // in adjacent node rows from visually joining into one continuous vertical line.
    match height.clamp(1, 8) {
        1..=3 => '⡀',
        4..=5 => '⡄',
        _ => '⡆',
    }
}

fn latency_signal_style(state: LatencySignalState) -> Style {
    match state {
        LatencySignalState::Untested => Style::default().fg(Color::DarkGray),
        LatencySignalState::Reachable { delay_ms } if delay_ms < 200 => {
            Style::default().fg(Color::Green)
        }
        LatencySignalState::Reachable { delay_ms } if delay_ms < 400 => {
            Style::default().fg(Color::Yellow)
        }
        LatencySignalState::Reachable { delay_ms } if delay_ms < 600 => {
            Style::default().fg(Color::Rgb(184, 134, 11))
        }
        LatencySignalState::Reachable { .. } => {
            Style::default().fg(Color::Rgb(205, 92, 92))
        }
        LatencySignalState::Unreachable => Style::default()
            .fg(Color::Rgb(139, 0, 0))
            .add_modifier(Modifier::BOLD),
    }
}

fn latency_average_label(signal: &LatencySignal) -> String {
    signal.average_ms.map_or_else(
        || "--".to_string(),
        |average| format!("{average}ms"),
    )
}

fn render_latency_signal(signal: &LatencySignal) -> Vec<Span<'static>> {
    signal
        .bars
        .iter()
        .map(|bar| {
            Span::styled(
                latency_signal_glyph(bar.height).to_string(),
                latency_signal_style(bar.state),
            )
        })
        .collect()
}

#[cfg(test)]
#[path = "dashboard_tests.rs"]
mod tests;

#[allow(dead_code)]
pub(crate) fn render(frame: &mut Frame, snapshot: &DashboardSnapshot<'_>) {
    render_in_area(frame, frame.area(), snapshot);
}

pub(crate) fn render_in_area(frame: &mut Frame, area: Rect, snapshot: &DashboardSnapshot<'_>) {
    let theme = Theme::detect();

    let is_private_access = snapshot.operational_workspace
        == crate::tui_state::OperationalWorkspace::PrivateAccess
        || snapshot.left_pane_section == LeftPaneSection::Intranet;

    if !is_private_access {
        render_internet_workspace(frame, area, snapshot, &theme);
    } else {
        render_intranet_workspace(frame, area, snapshot, &theme);
    }

    if let Some(message) = snapshot.flash.as_deref() {
        let estimated_width = area
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
            .min(area.height);
        let flash_area = centered_rect(80, height, area);
        frame.render_widget(Clear, flash_area);
        frame.render_widget(
            Paragraph::new(message).block(Block::default().title("Info").borders(Borders::ALL)),
            flash_area,
        );
    }
    render_active_modals(frame, snapshot);
    if let Some(onboarding) = snapshot.onboarding {
        draw_onboarding_panel(frame, onboarding);
    }
    if let Some(progress) = snapshot.private_access_progress {
        draw_private_access_progress_panel(frame, progress);
    }
    if let Some(auth) = snapshot.private_access_auth {
        draw_private_access_auth_panel(frame, auth);
    }
}

pub(crate) fn render_active_modals(frame: &mut Frame, snapshot: &DashboardSnapshot<'_>) {
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
}

fn render_internet_workspace(
    frame: &mut Frame,
    area: Rect,
    snapshot: &DashboardSnapshot<'_>,
    theme: &Theme,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    let [tabs_area, candidate_area, footer_area] = if area.height >= 3 {
        Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .areas(area)
    } else if area.height == 2 {
        let [c, f] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(area);
        [Rect::default(), c, f]
    } else {
        [Rect::default(), area, Rect::default()]
    };

    if tabs_area.height > 0 {
        render_filter_tabs(frame, tabs_area, snapshot, theme);
    }

    if candidate_area.height > 0 {
        render_candidate_table(frame, candidate_area, snapshot, theme);
    }

    if footer_area.height > 0 {
        render_internet_footer(frame, footer_area, snapshot, theme);
    }
}

fn render_filter_tabs(
    frame: &mut Frame,
    area: Rect,
    snapshot: &DashboardSnapshot<'_>,
    theme: &Theme,
) {
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

    let tabs = Tabs::new(tab_titles)
        .select(snapshot.active_node_view_tab)
        .highlight_style(theme.style_breadcrumb())
        .style(theme.style_muted())
        .divider(" │ ");
    frame.render_widget(tabs, area);
}

fn render_candidate_table(
    frame: &mut Frame,
    area: Rect,
    snapshot: &DashboardSnapshot<'_>,
    theme: &Theme,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    let (candidate_notice_area, candidate_list_area) =
        if let Some(notice) = &snapshot.candidate_notice {
            let inner_width = area.width.saturating_sub(2).max(1) as usize;
            let wrapped_lines = notice
                .message
                .lines()
                .map(|line| {
                    unicode_width::UnicodeWidthStr::width(line)
                        .div_ceil(inner_width)
                        .max(1)
                })
                .sum::<usize>() as u16;
            let notice_height = wrapped_lines.saturating_add(2).min(area.height);
            let [notice_area, list_area] =
                Layout::vertical([Constraint::Length(notice_height), Constraint::Min(0)])
                    .areas(area);
            (Some(notice_area), list_area)
        } else {
            (None, area)
        };

    if let (Some(notice), Some(notice_area)) = (&snapshot.candidate_notice, candidate_notice_area) {
        frame.render_widget(
            Paragraph::new(notice.message.as_str())
                .style(if notice.error {
                    theme.style_danger()
                } else {
                    theme.style_warning()
                })
                .wrap(Wrap { trim: false })
                .block(
                    Block::default()
                        .title(notice.title.as_str())
                        .borders(Borders::ALL)
                        .border_style(if notice.error {
                            theme.style_danger()
                        } else {
                            theme.style_warning()
                        }),
                ),
            notice_area,
        );
    }

    if snapshot.candidate_rows.is_empty() {
        if !snapshot.candidate_title.is_empty() {
            frame.render_widget(
                Paragraph::new(snapshot.candidate_title.clone()).style(theme.style_muted()),
                candidate_list_area,
            );
        }
        return;
    }

    let total_width = candidate_list_area.width as usize;

    let members = snapshot
        .candidate_rows
        .iter()
        .enumerate()
        .map(|(idx, row)| {
            let is_focused = snapshot.candidate_selected == Some(idx);
            let prefix = if row.is_current {
                "* "
            } else if is_focused {
                "> "
            } else {
                "  "
            };

            let prefix_style = if row.is_current {
                theme.style_success().add_modifier(Modifier::BOLD)
            } else if is_focused {
                theme.style_focused_row()
            } else {
                theme.style_muted()
            };

            let name_style = if row.is_current {
                theme.style_success().add_modifier(Modifier::BOLD)
            } else if is_focused {
                theme.style_focused_row()
            } else {
                Style::default().fg(theme.text_primary())
            };

            let mut right_spans = Vec::new();

            let marker_text = if !row.marker.is_empty() {
                row.marker.as_str()
            } else if !row.compact_marker.is_empty() {
                row.compact_marker.as_str()
            } else {
                ""
            };

            if !marker_text.is_empty() {
                if row.tone == CandidateTone::Pending {
                    right_spans.extend(render_pending_working_marker(
                        marker_text,
                        snapshot.pending_animation_tick,
                    ));
                } else {
                    right_spans.push(Span::styled(
                        marker_text.to_string(),
                        candidate_marker_style(
                            marker_text,
                            row.tone,
                            snapshot.pending_animation_bright,
                            theme,
                        ),
                    ));
                }
                right_spans.push(Span::raw("  "));
            }

            if !row.reachability.is_empty() {
                right_spans.push(Span::styled(
                    row.reachability.clone(),
                    reachability_badge_style(&row.reachability, row.tone, theme),
                ));
                right_spans.push(Span::raw("  "));
            }

            if let Some(signal) = &row.latency_signal {
                right_spans.extend(render_latency_signal(signal));
                right_spans.push(Span::raw(" "));
                right_spans.push(Span::styled(
                    latency_average_label(signal),
                    Style::default().fg(if signal.average_ms.is_some() {
                        theme.text_secondary()
                    } else {
                        theme.text_muted()
                    }),
                ));
            }

            let right_width: usize = right_spans
                .iter()
                .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
                .sum();

            let prefix_width = unicode_width::UnicodeWidthStr::width(prefix);
            let name_budget = total_width.saturating_sub(prefix_width + right_width + 2);
            let name_str = truncate_for_width(&row.name, name_budget);
            let name_width = unicode_width::UnicodeWidthStr::width(name_str.as_str());
            let left_width = prefix_width + name_width;

            let padding_spaces = total_width.saturating_sub(left_width + right_width);

            let mut spans = Vec::new();
            spans.push(Span::styled(prefix, prefix_style));
            spans.push(Span::styled(name_str, name_style));
            if padding_spaces > 0 {
                spans.push(Span::raw(" ".repeat(padding_spaces)));
            }
            spans.extend(right_spans);

            ListItem::new(Line::from(spans))
        })
        .collect::<Vec<_>>();

    let members_widget = List::new(members).highlight_style(theme.style_focused_row());
    let mut members_state = ListState::default().with_selected(snapshot.candidate_selected);
    frame.render_stateful_widget(members_widget, candidate_list_area, &mut members_state);
}

fn render_internet_footer(
    frame: &mut Frame,
    area: Rect,
    snapshot: &DashboardSnapshot<'_>,
    theme: &Theme,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    if let StatusFooter::Filter(input) = &snapshot.status.footer {
        let spans = vec![
            Span::styled("Filter: ", theme.style_breadcrumb()),
            Span::styled(input.clone(), theme.style_base()),
            Span::raw("   "),
            Span::styled("[Enter]", theme.style_footer_keys()),
            Span::raw(" "),
            Span::styled("Confirm", theme.style_muted()),
            Span::raw("  "),
            Span::styled("[Esc]", theme.style_footer_keys()),
            Span::raw(" "),
            Span::styled("Clear", theme.style_muted()),
        ];
        frame.render_widget(Paragraph::new(Line::from(spans)), area);

        let cursor_x = area
            .x
            .saturating_add(unicode_width::UnicodeWidthStr::width("Filter: ") as u16)
            .saturating_add(unicode_width::UnicodeWidthStr::width(input.as_str()) as u16);
        frame.set_cursor_position((cursor_x, area.y));
        return;
    }

    let left_spans = vec![
        Span::styled("g", theme.style_footer_keys()),
        Span::raw(" "),
        Span::styled("dashboard", theme.style_muted()),
        Span::raw("   "),
        Span::styled("c", theme.style_footer_keys()),
        Span::raw(" "),
        Span::styled("connections", theme.style_muted()),
        Span::raw("   "),
        Span::styled("i", theme.style_footer_keys()),
        Span::raw(" "),
        Span::styled("quality", theme.style_muted()),
        Span::raw("   "),
        Span::styled("o", theme.style_footer_keys()),
        Span::raw(" "),
        Span::styled("settings", theme.style_muted()),
        Span::raw("   "),
        Span::styled("?", theme.style_footer_keys()),
        Span::raw(" "),
        Span::styled("help", theme.style_muted()),
        Span::raw("   "),
        Span::styled("p", theme.style_footer_keys()),
        Span::raw(" "),
        Span::styled("provider", theme.style_muted()),
        Span::raw("   "),
        Span::styled("[Tab]", theme.style_footer_keys()),
        Span::raw(" "),
        Span::styled("Intranet →", theme.style_muted()),
    ];

    let mut right_spans = Vec::new();
    if let StatusFooter::Status(msg) = &snapshot.status.footer {
        if !msg.is_empty() {
            let msg_style = if msg == "ready" {
                theme.style_muted()
            } else {
                theme.style_warning()
            };
            right_spans.push(Span::styled(msg.clone(), msg_style));
            right_spans.push(Span::raw("  "));
        }
    }
    right_spans.push(Span::styled("GLOBAL NET", theme.style_muted()));
    right_spans.push(Span::raw("  "));
    right_spans.push(Span::styled("STABLE", theme.style_success()));

    let total_right: usize = right_spans
        .iter()
        .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
        .sum();
    let right_width = (total_right as u16).min(area.width);
    let left_width = area.width.saturating_sub(right_width);
    let [left_area, right_area] = Layout::horizontal([
        Constraint::Length(left_width),
        Constraint::Min(right_width),
    ])
    .areas(area);

    frame.render_widget(Paragraph::new(Line::from(left_spans)), left_area);
    frame.render_widget(
        Paragraph::new(Line::from(right_spans)).alignment(ratatui::layout::Alignment::Right),
        right_area,
    );
}

fn render_intranet_workspace(
    frame: &mut Frame,
    area: Rect,
    snapshot: &DashboardSnapshot<'_>,
    theme: &Theme,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    let [main_area, footer_area] = if area.height >= 2 {
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(area)
    } else {
        [area, Rect::default()]
    };

    let left_width = if main_area.width >= 120 {
        46
    } else if main_area.width >= 100 {
        40
    } else if main_area.width >= 80 {
        34
    } else {
        (main_area.width / 2).max(24)
    };

    let [intranet_area, details_area] =
        Layout::horizontal([Constraint::Length(left_width), Constraint::Min(20)]).areas(main_area);

    let inner_width = intranet_area.width.saturating_sub(2) as usize;
    let profiles = snapshot
        .intranet_rows
        .iter()
        .enumerate()
        .map(|(idx, row)| {
            let is_selected = idx == snapshot.intranet_selected;
            let state_label = if row.background {
                "BACKGROUND"
            } else {
                private_access_state_badge(row.state.clone())
            };
            let state_style = private_access_state_style(&row.state);
            let prefix = if is_selected { "> " } else { "  " };
            let prefix_style = if is_selected {
                theme.style_focused_row()
            } else {
                theme.style_muted()
            };
            let id_style = if is_selected {
                theme.style_focused_row()
            } else {
                Style::default().fg(theme.text_primary())
            };

            let state_width = unicode_width::UnicodeWidthStr::width(state_label);
            let id_budget = inner_width.saturating_sub(2 + state_width + 1);
            let truncated_id = truncate_for_width(&row.id, id_budget);
            let id_width = unicode_width::UnicodeWidthStr::width(truncated_id.as_str());
            let padding = inner_width.saturating_sub(2 + id_width + state_width);

            let mut line1_spans = vec![
                Span::styled(prefix, prefix_style),
                Span::styled(truncated_id, id_style),
            ];
            if padding > 0 {
                line1_spans.push(Span::raw(" ".repeat(padding)));
            }
            line1_spans.push(Span::styled(state_label, state_style));

            let mode_str = match row.mode {
                PrivateAccessMode::Tun => "TUN",
                PrivateAccessMode::Bridge => "Bridge",
            };
            let meta_text = format!(
                "  {} · {} · {} routes",
                row.server, mode_str, row.routes_count
            );
            let truncated_meta = truncate_for_width(&meta_text, inner_width);
            let line2 = Line::from(Span::styled(truncated_meta, theme.style_muted()));

            ListItem::new(vec![Line::from(line1_spans), line2])
        })
        .collect::<Vec<_>>();

    let intranet_active = snapshot.focus == Focus::Groups;
    let intranet_block = Block::default()
        .title(" Private Access · Profiles ")
        .borders(Borders::ALL)
        .border_style(if intranet_active {
            Style::default().fg(theme.border_focus())
        } else {
            Style::default().fg(theme.border_default())
        });
    let intranet_widget = List::new(profiles).block(intranet_block);
    let mut intranet_state = ListState::default().with_selected(Some(snapshot.intranet_selected));
    frame.render_stateful_widget(intranet_widget, intranet_area, &mut intranet_state);

    if let Some(detail) = snapshot.intranet_detail.as_ref() {
        let profile = detail.profile;
        let detail_view = private_access_detail_view(profile, |section| {
            detail
                .expanded_sections
                .contains(&format!("{}:{}", profile.id, section.key()))
        });
        let title = if detail.scroll == 0 {
            format!(" Private Access: {} ", profile.id)
        } else {
            format!(" Private Access: {} [line {}] ", profile.id, detail.scroll + 1)
        };
        let details_block = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(if detail.active {
                Style::default().fg(theme.border_focus())
            } else {
                Style::default().fg(theme.border_default())
            });
        let details_inner = details_block.inner(details_area);
        frame.render_widget(details_block, details_area);
        let details = Paragraph::new(detail_view.lines)
            .wrap(Wrap { trim: false })
            .scroll((detail.scroll, 0));
        frame.render_widget(details, details_inner);
    } else {
        let empty_block = Block::default()
            .title(" Private Access Detail ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.border_default()));
        let empty_inner = empty_block.inner(details_area);
        frame.render_widget(empty_block, details_area);
        frame.render_widget(
            Paragraph::new("No profile selected").style(theme.style_muted()),
            empty_inner,
        );
    }

    if footer_area.height == 0 {
        return;
    }

    if let StatusFooter::Filter(input) = &snapshot.status.footer {
        let spans = vec![
            Span::styled("Filter: ", theme.style_breadcrumb()),
            Span::styled(input.clone(), theme.style_base()),
            Span::raw("   "),
            Span::styled("[Enter]", theme.style_footer_keys()),
            Span::raw(" "),
            Span::styled("Confirm", theme.style_muted()),
            Span::raw("  "),
            Span::styled("[Esc]", theme.style_footer_keys()),
            Span::raw(" "),
            Span::styled("Clear", theme.style_muted()),
        ];
        frame.render_widget(Paragraph::new(Line::from(spans)), footer_area);

        let cursor_x = footer_area
            .x
            .saturating_add(unicode_width::UnicodeWidthStr::width("Filter: ") as u16)
            .saturating_add(unicode_width::UnicodeWidthStr::width(input.as_str()) as u16);
        frame.set_cursor_position((cursor_x, footer_area.y));
        return;
    }

    let left_spans = vec![
        Span::styled("[Tab]", theme.style_footer_keys()),
        Span::raw(" "),
        Span::styled("Internet →", theme.style_muted()),
        Span::raw("   "),
        Span::styled("[V]", theme.style_footer_keys()),
        Span::raw(" "),
        Span::styled("Connect/Disconnect", theme.style_muted()),
        Span::raw("   "),
        Span::styled("[j/k]", theme.style_footer_keys()),
        Span::raw(" "),
        Span::styled("Navigate", theme.style_muted()),
        Span::raw("   "),
        Span::styled("[Enter]", theme.style_footer_keys()),
        Span::raw(" "),
        Span::styled("Fold/Expand", theme.style_muted()),
        Span::raw("   "),
        Span::styled("[o]", theme.style_footer_keys()),
        Span::raw(" "),
        Span::styled("Settings", theme.style_muted()),
        Span::raw("   "),
        Span::styled("[?]", theme.style_footer_keys()),
        Span::raw(" "),
        Span::styled("Help", theme.style_muted()),
        Span::raw("   "),
        Span::styled("[Ctrl+K]", theme.style_footer_keys()),
        Span::raw(" "),
        Span::styled("Actions", theme.style_muted()),
    ];

    let mut right_spans = Vec::new();
    if let StatusFooter::Status(msg) = &snapshot.status.footer {
        if !msg.is_empty() {
            let msg_style = if msg == "ready" {
                theme.style_muted()
            } else {
                theme.style_warning()
            };
            right_spans.push(Span::styled(msg.clone(), msg_style));
            right_spans.push(Span::raw("  "));
        }
    }
    let connected_count = snapshot
        .intranet_rows
        .iter()
        .filter(|r| matches!(r.state, PrivateAccessState::Connected))
        .count();
    if connected_count > 0 {
        right_spans.push(Span::styled(
            format!("{connected_count} CONNECTED"),
            theme.style_success(),
        ));
    } else {
        right_spans.push(Span::styled("DISCONNECTED", theme.style_muted()));
    }

    let total_right: usize = right_spans
        .iter()
        .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
        .sum();
    let right_width = (total_right as u16).min(footer_area.width);
    let left_width = footer_area.width.saturating_sub(right_width);
    let [left_area, right_area] = Layout::horizontal([
        Constraint::Length(left_width),
        Constraint::Min(right_width),
    ])
    .areas(footer_area);

    frame.render_widget(Paragraph::new(Line::from(left_spans)), left_area);
    frame.render_widget(
        Paragraph::new(Line::from(right_spans)).alignment(ratatui::layout::Alignment::Right),
        right_area,
    );
}

fn render_pending_working_marker(marker: &str, tick: usize) -> Vec<Span<'static>> {
    let (prefix, suffix) = if let Some(idx) = marker.rfind(" (") {
        (&marker[..idx], &marker[idx..])
    } else {
        (marker, "")
    };

    let chars: Vec<char> = prefix.chars().collect();
    let char_count = chars.len();
    if char_count == 0 {
        return vec![Span::styled(marker.to_string(), Style::default().fg(Color::DarkGray))];
    }

    let period = char_count + 4;
    let wave_pos = (tick % period) as isize;

    let mut spans = Vec::with_capacity(char_count + 1);
    for (i, &ch) in chars.iter().enumerate() {
        let dist = (i as isize - wave_pos).abs();
        let style = match dist {
            0 => Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
            1 => Style::default()
                .fg(Color::Rgb(200, 200, 200))
                .add_modifier(Modifier::BOLD),
            2 => Style::default().fg(Color::Rgb(150, 150, 150)),
            _ => Style::default().fg(Color::DarkGray),
        };
        spans.push(Span::styled(ch.to_string(), style));
    }
    if !suffix.is_empty() {
        spans.push(Span::styled(
            suffix.to_string(),
            Style::default().fg(Color::DarkGray),
        ));
    }
    spans
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

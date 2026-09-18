use super::*;
use crate::automatic_selection::{NodeViewId, RankingPolicy};
use crate::private_access_session::{PrivateAccessMode, PrivateAccessProfileRuntime};
use crate::tui::ds::theme::Theme;
use ratatui::layout::Rect;

pub(crate) trait ThemeExt {
    fn style_danger(&self) -> Style;
}

impl ThemeExt for Theme {
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
            CandidateTone::Pending => pending_candidate_style(false, theme),
            CandidateTone::Success => theme.style_success(),
            CandidateTone::Error => theme.style_danger(),
            CandidateTone::Missing => theme.style_muted(),
        }
    }
}

fn candidate_marker_style(marker: &str, tone: CandidateTone, bright: bool, theme: &Theme) -> Style {
    if tone == CandidateTone::Pending {
        return pending_candidate_style(bright, theme);
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
            CandidateTone::Pending => pending_candidate_style(bright, theme),
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
    pub(crate) network_status: &'static str,
    pub(crate) current_down_rate: &'a str,
    pub(crate) current_up_rate: &'a str,
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

fn latency_signal_style(state: LatencySignalState, theme: &Theme) -> Style {
    match state {
        LatencySignalState::Untested => theme.style_muted(),
        LatencySignalState::Reachable { delay_ms } if delay_ms < 200 => theme.style_success(),
        LatencySignalState::Reachable { delay_ms } if delay_ms < 400 => theme.style_warning(),
        LatencySignalState::Reachable { delay_ms } if delay_ms < 600 => theme.style_warning(),
        LatencySignalState::Reachable { .. } => theme.style_error(),
        LatencySignalState::Unreachable => theme.style_error().add_modifier(Modifier::BOLD),
    }
}

fn latency_average_label(signal: &LatencySignal) -> String {
    signal
        .average_ms
        .map_or_else(|| "--".to_string(), |average| format!("{average}ms"))
}

fn render_latency_signal(signal: &LatencySignal, theme: &Theme) -> Vec<Span<'static>> {
    signal
        .bars
        .iter()
        .map(|bar| {
            Span::styled(
                latency_signal_glyph(bar.height).to_string(),
                latency_signal_style(bar.state, theme),
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
    frame.render_widget(Block::default().style(theme.style_base()), area);

    let is_private_access = snapshot.operational_workspace
        == crate::tui_state::OperationalWorkspace::PrivateAccess
        || snapshot.left_pane_section == LeftPaneSection::Intranet;

    if !is_private_access {
        render_internet_workspace(frame, area, snapshot, &theme);
    } else {
        render_intranet_workspace(frame, area, snapshot, &theme);
    }

    if let Some(message) = snapshot.flash.as_deref() {
        let estimated_width = area.width.saturating_mul(80).saturating_div(100).max(1);
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
        let height = wrapped_lines.saturating_add(2).max(7).min(area.height);
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

    // Figma 8:2 uses a 30 px tab container around 16 px text, which maps most closely to two
    // terminal rows. Only collapse it when the caller provides an unusually tiny body area.
    let [tabs_area, candidate_area, footer_area] = if area.height >= 4 {
        Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .areas(area)
    } else if area.height == 3 {
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

    let (candidate_notice_area, candidate_list_area) = if let Some(notice) =
        &snapshot.candidate_notice
    {
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
            Layout::vertical([Constraint::Length(notice_height), Constraint::Min(0)]).areas(area);
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
    // The 960 px Figma row keeps names and metrics in a 640 px content column, leaving the final
    // third as breathing room. Compact terminals need the full width to keep details readable.
    let content_width = if total_width >= 96 {
        total_width.saturating_mul(2) / 3
    } else {
        total_width
    };
    let horizontal_padding = if total_width >= 8 { 2 } else { 0 };
    let minimum_middle_gap = 2;

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
                        theme,
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
                right_spans.extend(render_latency_signal(signal, theme));
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
            let name_budget = content_width.saturating_sub(
                horizontal_padding * 2 + prefix_width + right_width + minimum_middle_gap,
            );
            let name_str = truncate_for_width(&row.name, name_budget);
            let name_width = unicode_width::UnicodeWidthStr::width(name_str.as_str());
            let left_width = horizontal_padding + prefix_width + name_width;

            let padding_spaces =
                content_width.saturating_sub(left_width + right_width + horizontal_padding);

            let mut spans = Vec::new();
            spans.push(Span::raw(" ".repeat(horizontal_padding)));
            spans.push(Span::styled(prefix, prefix_style));
            spans.push(Span::styled(name_str, name_style));
            if padding_spaces > 0 {
                spans.push(Span::raw(" ".repeat(padding_spaces)));
            }
            spans.extend(right_spans);
            let rendered_width = content_width.saturating_sub(horizontal_padding);
            if total_width > rendered_width {
                spans.push(Span::raw(" ".repeat(total_width - rendered_width)));
            }

            // Terminal cells cannot express Figma's half-row padding, so an odd-height item keeps
            // the node name and its trailing measurements on the true middle row.
            ListItem::new(vec![Line::default(), Line::from(spans), Line::default()])
        })
        .collect::<Vec<_>>();

    if snapshot
        .candidate_rows
        .first()
        .is_some_and(|row| row.is_current)
    {
        let mut members = members;
        let scrolling_members = members.split_off(1);
        let sticky_height = 3.min(candidate_list_area.height);
        let [sticky_area, scrolling_area] =
            Layout::vertical([Constraint::Length(sticky_height), Constraint::Min(0)])
                .areas(candidate_list_area);

        let sticky_widget = List::new(members).highlight_style(theme.style_focused_row());
        let sticky_selected = (snapshot.candidate_selected == Some(0)).then_some(0);
        let mut sticky_state = ListState::default().with_selected(sticky_selected);
        frame.render_stateful_widget(sticky_widget, sticky_area, &mut sticky_state);

        if scrolling_area.height > 0 {
            let scrolling_widget =
                List::new(scrolling_members).highlight_style(theme.style_focused_row());
            let scrolling_selected = snapshot
                .candidate_selected
                .and_then(|index| index.checked_sub(1));
            let mut scrolling_state = ListState::default().with_selected(scrolling_selected);
            frame.render_stateful_widget(scrolling_widget, scrolling_area, &mut scrolling_state);
        }
    } else {
        let members_widget = List::new(members).highlight_style(theme.style_focused_row());
        let mut members_state = ListState::default().with_selected(snapshot.candidate_selected);
        frame.render_stateful_widget(members_widget, candidate_list_area, &mut members_state);
    }
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

    let mut right_spans = Vec::new();
    right_spans.push(Span::styled("GLOBAL NET", theme.style_muted()));
    right_spans.push(Span::raw("  "));
    right_spans.push(Span::styled(
        snapshot.network_status,
        if snapshot.network_status == "STABLE" {
            Style::default().fg(theme.text_status_active())
        } else {
            theme.style_muted()
        },
    ));
    right_spans.push(Span::raw("  "));
    right_spans.push(Span::styled(
        format!("↓{}", snapshot.current_down_rate),
        Style::default().fg(theme.text_transfer()),
    ));
    right_spans.push(Span::raw("  "));
    right_spans.push(Span::styled(
        format!("↑{}", snapshot.current_up_rate),
        Style::default().fg(theme.text_transfer()),
    ));

    let total_right: usize = right_spans
        .iter()
        .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
        .sum();
    let right_width = (total_right as u16).min(area.width);
    let gap_width = u16::from(area.width > right_width);
    let left_width = area.width.saturating_sub(right_width + gap_width);

    let shortcut_variants: &[&[(&str, &str)]] = &[
        &[
            ("Ctrl+K", "actions"),
            ("c", "connections"),
            ("i", "quality"),
            ("o", "settings"),
            ("?", "help"),
            ("p", "provider"),
            ("[Tab]", "Intranet →"),
        ],
        &[
            ("Ctrl+K", "actions"),
            ("c", "connections"),
            ("i", "quality"),
            ("?", "help"),
            ("[Tab]", "Intranet →"),
        ],
        &[("Ctrl+K", "actions"), ("?", "help"), ("[Tab]", "Intranet")],
        &[("Ctrl+K", ""), ("?", ""), ("[Tab]", "")],
    ];
    let shortcut_width = |items: &[(&str, &str)]| {
        items
            .iter()
            .enumerate()
            .map(|(index, (key, label))| {
                usize::from(index > 0) * 3
                    + unicode_width::UnicodeWidthStr::width(*key)
                    + usize::from(!label.is_empty())
                    + unicode_width::UnicodeWidthStr::width(*label)
            })
            .sum::<usize>()
    };
    let (left_spans, filter_cursor_offset) =
        if let StatusFooter::Filter(input) = &snapshot.status.footer {
            (
                vec![
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
                ],
                Some(
                    unicode_width::UnicodeWidthStr::width("Filter: ") as u16
                        + unicode_width::UnicodeWidthStr::width(input.as_str()) as u16,
                ),
            )
        } else {
            let shortcuts = shortcut_variants
                .iter()
                .copied()
                .find(|items| shortcut_width(items) <= usize::from(left_width))
                .unwrap_or(&[]);
            let mut spans = Vec::new();
            for (index, (key, label)) in shortcuts.iter().enumerate() {
                if index > 0 {
                    spans.push(Span::raw("   "));
                }
                spans.push(Span::styled(*key, theme.style_footer_keys()));
                if !label.is_empty() {
                    spans.push(Span::raw(" "));
                    spans.push(Span::styled(*label, theme.style_muted()));
                }
            }
            (spans, None)
        };

    let [left_area, _, right_area] = Layout::horizontal([
        Constraint::Length(left_width),
        Constraint::Length(gap_width),
        Constraint::Min(right_width),
    ])
    .areas(area);

    frame.render_widget(Paragraph::new(Line::from(left_spans)), left_area);
    frame.render_widget(
        Paragraph::new(Line::from(right_spans)).alignment(ratatui::layout::Alignment::Right),
        right_area,
    );
    if let Some(offset) = filter_cursor_offset {
        let cursor_x = left_area
            .x
            .saturating_add(offset.min(left_area.width.saturating_sub(1)));
        frame.set_cursor_position((cursor_x, left_area.y));
    }
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

    let [header_area, main_area, footer_area] = if area.height >= 3 {
        Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .areas(area)
    } else {
        [area, Rect::default(), Rect::default()]
    };

    let Some(detail) = snapshot.intranet_detail.as_ref() else {
        frame.render_widget(
            Paragraph::new("INTRANET / PRIVATE ACCESS NOT CONFIGURED").style(theme.style_muted()),
            header_area,
        );
        render_intranet_footer(frame, footer_area, snapshot, theme, None);
        return;
    };
    let profile = detail.profile;
    let state_label = private_access_state_badge(profile.state.clone());
    let breadcrumb = format!(
        "INTRANET / {} / {}",
        profile.id.to_ascii_uppercase(),
        state_label
    );
    frame.render_widget(
        Paragraph::new(breadcrumb).style(private_access_state_style(&profile.state)),
        header_area,
    );

    let padded_main = if main_area.width >= 82 && main_area.height >= 4 {
        let [_, vertical, _] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .areas(main_area);
        let [_, center, _] = Layout::horizontal([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .areas(vertical);
        center
    } else {
        main_area
    };

    let expanded = matches!(profile.state, PrivateAccessState::Connected)
        && [IntranetDetailSection::Dns, IntranetDetailSection::Routes]
            .into_iter()
            .any(|section| {
                detail
                    .expanded_sections
                    .contains(&format!("{}:{}", profile.id, section.key()))
            });
    let (summary_area, resource_area) = if expanded && padded_main.width >= 72 {
        let summary_width = padded_main
            .width
            .saturating_mul(42)
            .saturating_div(100)
            .max(30);
        let [summary, _, resources] = Layout::horizontal([
            Constraint::Length(summary_width),
            Constraint::Length(1),
            Constraint::Min(30),
        ])
        .areas(padded_main);
        (summary, Some(resources))
    } else {
        (padded_main, None)
    };

    let summary_block = Block::default()
        .title(format!(" {} ", profile.id.to_ascii_uppercase()))
        .title_bottom(Line::from(format!(
            " ACTIVE PROFILE / {} OF {} ",
            snapshot.intranet_selected.saturating_add(1),
            snapshot.intranet_rows.len().max(1)
        )))
        .borders(Borders::ALL)
        .border_style(if snapshot.focus == Focus::Groups && resource_area.is_none() {
            Style::default().fg(theme.border_focus())
        } else {
            Style::default().fg(theme.border_default())
        });
    let summary_inner = summary_block.inner(summary_area);
    frame.render_widget(summary_block, summary_area);
    frame.render_widget(
        Paragraph::new(private_access_summary_lines(
            profile,
            summary_inner.width,
            theme,
            detail.active.then_some(detail.focused_section),
        ))
        .wrap(Wrap { trim: false }),
        summary_inner,
    );

    if let Some(resource_area) = resource_area {
        let detail_view = private_access_detail_view(profile, |section| {
            detail
                .expanded_sections
                .contains(&format!("{}:{}", profile.id, section.key()))
        });
        let focused_section = detail.focused_section;
        let focused_start = detail_view
            .sections
            .iter()
            .find(|range| range.section == focused_section)
            .map_or(0, |range| range.start.saturating_sub(1));
        let mut resource_lines = Vec::new();
        for range in &detail_view.sections {
            if detail
                .expanded_sections
                .contains(&format!("{}:{}", profile.id, range.section.key()))
            {
                resource_lines.extend_from_slice(
                    &detail_view.lines[range.start.saturating_sub(1)..range.end],
                );
            }
        }
        let block = Block::default()
            .title(format!(
                " INTRANET: {} / {} ",
                profile.id.to_ascii_uppercase(),
                focused_section.key().to_ascii_uppercase()
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(if detail.active {
                theme.border_focus()
            } else {
                theme.border_default()
            }));
        let inner = block.inner(resource_area);
        frame.render_widget(block, resource_area);
        frame.render_widget(
            Paragraph::new(resource_lines)
                .wrap(Wrap { trim: false })
                .scroll((detail.scroll.saturating_sub(focused_start as u16), 0)),
            inner,
        );
    }

    render_intranet_footer(frame, footer_area, snapshot, theme, Some(&profile.state));
}

fn private_access_summary_lines(
    profile: &PrivateAccessProfileRuntime,
    width: u16,
    theme: &Theme,
    focused_section: Option<IntranetDetailSection>,
) -> Vec<Line<'static>> {
    let connected = matches!(profile.state, PrivateAccessState::Connected);
    let gateway = if profile.server.trim().is_empty() {
        "not configured".to_string()
    } else {
        format!("{}:{}", profile.server, profile.port)
    };
    let process = if profile.background_pid.is_some() {
        "running in background"
    } else if profile.owns_process() {
        "owned by this TUI"
    } else {
        "no active session"
    };
    let mode = match profile.mode {
        PrivateAccessMode::Tun if connected => "TUN",
        PrivateAccessMode::Tun => "TUN / inactive",
        PrivateAccessMode::Bridge => "HTTP bridge",
    };
    let mut lines = vec![Line::from(Span::styled(
        format!(
            "PRIVATE ACCESS SESSION / SELECTED PROFILE: {}",
            profile.id.to_ascii_uppercase()
        ),
        theme.style_breadcrumb(),
    ))];

    if matches!(profile.state, PrivateAccessState::Error) {
        lines.push(Line::from(Span::styled(
            "CONNECTION FAILED",
            theme.style_danger().add_modifier(Modifier::BOLD),
        )));
        if let Some(error) = profile.last_error.as_deref() {
            lines.push(Line::from(error.to_string()));
        }
        lines.push(Line::from(Span::styled(
            "DNS and routes are unavailable until connected.",
            theme.style_muted(),
        )));
        return lines;
    }

    if connected {
        lines.push(private_access_collapsed_section_line(
            "DNS SERVERS",
            profile.dns.len(),
            width,
            theme,
            focused_section == Some(IntranetDetailSection::Dns),
        ));
        lines.push(private_access_collapsed_section_line(
            "ROUTES",
            profile.routes.len(),
            width,
            theme,
            focused_section == Some(IntranetDetailSection::Routes),
        ));
    }

    lines.extend([
        Line::default(),
        private_access_fact_line(
            "State",
            private_access_state_badge(profile.state.clone()),
            theme,
        ),
        private_access_fact_line("Service", &profile.manifest.name, theme),
        private_access_fact_line(
            "Protocol",
            &format!(
                "{} (service v{})",
                profile.manifest.protocol, profile.manifest.version
            ),
            theme,
        ),
        private_access_fact_line("Gateway", &gateway, theme),
        private_access_fact_line(
            "TLS verification",
            if profile.tls_verify {
                "enabled"
            } else {
                "disabled"
            },
            theme,
        ),
        private_access_fact_line("Data plane", mode, theme),
        private_access_fact_line("Process", process, theme),
        Line::default(),
        Line::from(Span::styled(
            "CAPABILITIES  routes / DNS / graceful disconnect",
            theme.style_breadcrumb().add_modifier(Modifier::BOLD),
        )),
        Line::default(),
    ]);

    if !connected {
        let waiting = match profile.state {
            PrivateAccessState::Connecting | PrivateAccessState::Disconnecting => {
                "waiting for session result"
            }
            _ => "available after connection",
        };
        lines.push(Line::from(Span::styled(
            format!("DNS SERVERS / {waiting}"),
            theme.style_muted(),
        )));
        lines.push(Line::from(Span::styled(
            format!("ROUTES / {waiting}"),
            theme.style_muted(),
        )));
        lines.push(Line::from(Span::styled(
            "No granted resources until the session is connected.",
            theme.style_muted(),
        )));
        return lines;
    }

    let preview = profile
        .routes
        .iter()
        .take(3)
        .map(|route| route.cidr.as_str())
        .collect::<Vec<_>>()
        .join("  ");
    if !preview.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("  {preview}"),
            theme.style_breadcrumb(),
        )));
    }
    let domains = profile
        .domains
        .iter()
        .take(1)
        .map(|domain| format!("exact {domain}"))
        .chain(
            profile
                .domain_suffixes
                .iter()
                .take(1)
                .map(|domain| format!("suffix *.{domain}")),
        )
        .collect::<Vec<_>>()
        .join(" · ");
    lines.push(Line::from(format!(
        "▸ INTERNAL DOMAINS · {}{}{}",
        profile.domains.len() + profile.domain_suffixes.len(),
        if domains.is_empty() { "" } else { "   " },
        domains
    )));
    lines
}

fn private_access_fact_line(label: &str, value: &str, theme: &Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<18}"), theme.style_muted()),
        Span::raw(value.to_string()),
    ])
}

fn private_access_collapsed_section_line(
    label: &str,
    count: usize,
    width: u16,
    theme: &Theme,
    focused: bool,
) -> Line<'static> {
    let title = format!("{} {label} / {count}", if focused { "▶" } else { "▸" });
    let hint = if width >= 32 { "Enter expand" } else { "Enter" };
    let padding = usize::from(width)
        .saturating_sub(unicode_width::UnicodeWidthStr::width(title.as_str()))
        .saturating_sub(unicode_width::UnicodeWidthStr::width(hint))
        .max(1);
    let line = Line::from(vec![
        Span::raw(title),
        Span::raw(" ".repeat(padding)),
        Span::styled(hint.to_string(), theme.style_muted()),
    ]);
    if focused {
        line.style(theme.style_focused_row())
    } else {
        line
    }
}

fn render_intranet_footer(
    frame: &mut Frame,
    area: Rect,
    snapshot: &DashboardSnapshot<'_>,
    theme: &Theme,
    state: Option<&PrivateAccessState>,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let right = format!(
        "GLOBAL NET  {}  ↓{}  ↑{}",
        snapshot.network_status, snapshot.current_down_rate, snapshot.current_up_rate
    );
    let right_width = unicode_width::UnicodeWidthStr::width(right.as_str()) as u16;
    let gap = u16::from(area.width > right_width);
    let left_width = area.width.saturating_sub(right_width.saturating_add(gap));
    let action = match state {
        Some(PrivateAccessState::Connected | PrivateAccessState::Connecting) => "V disconnect",
        Some(PrivateAccessState::Disconnecting) => "V wait",
        Some(_) => "V connect",
        None => "o settings",
    };
    let variants = [
        format!("p profile   ↑↓/jk select   Enter open   {action}   o settings   ? help"),
        format!("p profile   ↑↓ select   Enter open   {action}   ? help"),
        format!("p profile   {action}   ? help"),
        "?".to_string(),
    ];
    let left = variants
        .into_iter()
        .find(|value| unicode_width::UnicodeWidthStr::width(value.as_str()) <= left_width as usize)
        .unwrap_or_default();
    let [left_area, _, right_area] = Layout::horizontal([
        Constraint::Length(left_width),
        Constraint::Length(gap),
        Constraint::Min(right_width),
    ])
    .areas(area);
    frame.render_widget(
        Paragraph::new(left).style(theme.style_breadcrumb()),
        left_area,
    );
    frame.render_widget(
        Paragraph::new(right)
            .alignment(ratatui::layout::Alignment::Right)
            .style(theme.style_success()),
        right_area,
    );
}

fn render_pending_working_marker(marker: &str, tick: usize, theme: &Theme) -> Vec<Span<'static>> {
    let (prefix, suffix) = if let Some(idx) = marker.rfind(" (") {
        (&marker[..idx], &marker[idx..])
    } else {
        (marker, "")
    };

    let chars: Vec<char> = prefix.chars().collect();
    let char_count = chars.len();
    if char_count == 0 {
        return vec![Span::styled(marker.to_string(), theme.style_muted())];
    }

    let period = char_count + 4;
    let wave_pos = (tick % period) as isize;

    let mut spans = Vec::with_capacity(char_count + 1);
    for (i, &ch) in chars.iter().enumerate() {
        let dist = (i as isize - wave_pos).abs();
        let style = match dist {
            0 => Style::default()
                .fg(theme.text_primary())
                .add_modifier(Modifier::BOLD),
            1 => Style::default()
                .fg(theme.text_secondary())
                .add_modifier(Modifier::BOLD),
            2 => Style::default().fg(theme.text_muted()),
            _ => theme.style_muted(),
        };
        spans.push(Span::styled(ch.to_string(), style));
    }
    if !suffix.is_empty() {
        spans.push(Span::styled(suffix.to_string(), theme.style_muted()));
    }
    spans
}

fn pending_candidate_style(bright: bool, theme: &Theme) -> Style {
    if bright {
        theme.style_warning().add_modifier(Modifier::BOLD)
    } else {
        theme.style_muted()
    }
}

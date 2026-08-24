use std::collections::BTreeSet;
use std::env;
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Axis, Block, Borders, Chart, Clear, Dataset, GraphType, List, ListItem, ListState, Paragraph,
    Wrap,
};
use zeroize::Zeroize;

use crate::controller::{ConnectionInfo, ConnectionsSnapshot};
use crate::private_access::{PrivateAccessAuthField, PrivateAccessState};
use crate::private_access_session::{PrivateAccessMode, PrivateAccessProfileRuntime};
use crate::storage::NodeLatencySample;
use crate::subscriptions::SubscriptionRefreshOutput;

const LATENCY_CHART_MIN_WINDOW: Duration = Duration::from_secs(5 * 60);
const LATENCY_CHART_MAX_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Focus {
    Groups,
    Members,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LeftPaneSection {
    Internet,
    Intranet,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) enum IntranetDetailSection {
    Dns,
    Routes,
    Domains,
}

impl IntranetDetailSection {
    pub(super) fn key(self) -> &'static str {
        match self {
            Self::Dns => "dns",
            Self::Routes => "routes",
            Self::Domains => "domains",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct IntranetDetailSectionRange {
    pub(super) section: IntranetDetailSection,
    pub(super) start: usize,
    pub(super) end: usize,
    pub(super) foldable: bool,
}

pub(super) struct IntranetDetailView {
    pub(super) lines: Vec<Line<'static>>,
    pub(super) sections: Vec<IntranetDetailSectionRange>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LatencyChartTimeUnit {
    Minutes,
    Hours,
}

#[derive(Clone, Debug)]
pub(super) struct LatencyChartState {
    pub(super) selector: String,
    pub(super) node: String,
    pub(super) samples: Vec<NodeLatencySample>,
    pub(super) window: Duration,
    pub(super) threshold_ms: u64,
    pub(super) last_refresh: Instant,
}

pub(super) struct OnboardingState {
    pub(super) input: String,
    pub(super) message: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SettingsField {
    BenchmarkUrl,
    BenchmarkTimeoutMs,
    RequestTimeoutSec,
    MaxConcurrency,
    VerifyTargets,
    AutoPickThresholdMs,
    AutoPickIntervalSec,
    SystemProxyServer,
    ChinaIpRouting,
    PrivateAccessProfile,
    PrivateAccessManifestPath,
    PrivateAccessMode,
    PrivateAccessServer,
    PrivateAccessPort,
    PrivateAccessUsername,
    PrivateAccessPassword,
    PrivateAccessPasswordEnv,
    PrivateAccessBridgeListen,
    PrivateAccessUseInternetProxy,
    PrivateAccessTlsVerify,
}

pub(super) const SETTINGS_FIELDS: &[SettingsField] = &[
    SettingsField::BenchmarkUrl,
    SettingsField::BenchmarkTimeoutMs,
    SettingsField::RequestTimeoutSec,
    SettingsField::MaxConcurrency,
    SettingsField::VerifyTargets,
    SettingsField::AutoPickThresholdMs,
    SettingsField::AutoPickIntervalSec,
    SettingsField::SystemProxyServer,
    SettingsField::ChinaIpRouting,
    SettingsField::PrivateAccessProfile,
    SettingsField::PrivateAccessManifestPath,
    SettingsField::PrivateAccessMode,
    SettingsField::PrivateAccessServer,
    SettingsField::PrivateAccessPort,
    SettingsField::PrivateAccessUsername,
    SettingsField::PrivateAccessPassword,
    SettingsField::PrivateAccessPasswordEnv,
    SettingsField::PrivateAccessBridgeListen,
    SettingsField::PrivateAccessUseInternetProxy,
    SettingsField::PrivateAccessTlsVerify,
];

pub(super) struct SettingsEditState {
    pub(super) field: SettingsField,
    pub(super) input: String,
    pub(super) error: Option<String>,
}

#[derive(Clone, Debug)]
pub(super) struct PrivateAccessProgressModal {
    pub(super) profile_index: usize,
    pub(super) title: String,
    pub(super) entries: Vec<PrivateAccessProgressEntry>,
    pub(super) done: bool,
}

pub(super) struct PrivateAccessAuthModal {
    pub(super) profile_index: usize,
    pub(super) service: String,
    pub(super) session_id: String,
    pub(super) challenge_id: String,
    pub(super) title: String,
    pub(super) message: String,
    pub(super) fields: Vec<PrivateAccessAuthField>,
    pub(super) buttons: Vec<String>,
    pub(super) inputs: Vec<String>,
    pub(super) field_index: usize,
    pub(super) error: Option<String>,
}

impl Drop for PrivateAccessAuthModal {
    fn drop(&mut self) {
        self.session_id.zeroize();
        self.challenge_id.zeroize();
        self.inputs.zeroize();
    }
}

#[derive(Clone, Debug)]
pub(super) struct PrivateAccessProgressEntry {
    pub(super) tone: PrivateAccessProgressTone,
    pub(super) text: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PrivateAccessProgressTone {
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct InternetRow {
    pub(super) name: String,
    pub(super) current: String,
    pub(super) is_current: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct IntranetRow {
    pub(super) id: String,
    pub(super) state: PrivateAccessState,
    pub(super) background: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CandidateTone {
    Pending,
    Success,
    Error,
    Missing,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct CandidateRow {
    pub(super) name: String,
    pub(super) is_current: bool,
    pub(super) marker: String,
    pub(super) tone: CandidateTone,
}

pub(super) struct IntranetDetailSnapshot<'a> {
    pub(super) profile: &'a PrivateAccessProfileRuntime,
    pub(super) expanded_sections: &'a BTreeSet<String>,
    pub(super) scroll: u16,
    pub(super) active: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum StatusFooter {
    Status(String),
    Filter(String),
    Bypass(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct StatusSnapshot {
    pub(super) system_proxy_enabled: bool,
    pub(super) tun_enabled: bool,
    pub(super) selection_context: String,
    pub(super) connections: String,
    pub(super) subscription: String,
    pub(super) sing_box: String,
    pub(super) footer: StatusFooter,
}

pub(super) struct ConnectionsPanelSnapshot<'a> {
    pub(super) summary: String,
    pub(super) connections: &'a ConnectionsSnapshot,
    pub(super) error: Option<&'a str>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct SettingRow {
    pub(super) label: &'static str,
    pub(super) value: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct SettingsPanelSnapshot {
    pub(super) rows: Vec<SettingRow>,
    pub(super) selected: usize,
    pub(super) editing: Option<(&'static str, String)>,
    pub(super) error: Option<String>,
}

pub(super) struct DashboardSnapshot<'a> {
    pub(super) focus: Focus,
    pub(super) left_pane_section: LeftPaneSection,
    pub(super) internet_rows: Vec<InternetRow>,
    pub(super) internet_selected: usize,
    pub(super) intranet_rows: Vec<IntranetRow>,
    pub(super) intranet_selected: usize,
    pub(super) candidate_title: String,
    pub(super) candidate_rows: Vec<CandidateRow>,
    pub(super) candidate_selected: Option<usize>,
    pub(super) intranet_detail: Option<IntranetDetailSnapshot<'a>>,
    pub(super) status: StatusSnapshot,
    pub(super) flash: Option<String>,
    pub(super) latency_chart: Option<&'a LatencyChartState>,
    pub(super) connections: Option<ConnectionsPanelSnapshot<'a>>,
    pub(super) help_index: Option<usize>,
    pub(super) settings: Option<SettingsPanelSnapshot>,
    pub(super) onboarding: Option<&'a OnboardingState>,
    pub(super) private_access_progress: Option<&'a PrivateAccessProgressModal>,
    pub(super) private_access_auth: Option<&'a PrivateAccessAuthModal>,
}

fn latency_chart_time_unit(window: Duration) -> LatencyChartTimeUnit {
    if window >= Duration::from_secs(2 * 60 * 60) {
        LatencyChartTimeUnit::Hours
    } else {
        LatencyChartTimeUnit::Minutes
    }
}

pub(super) fn latency_chart_window_label(window: Duration) -> String {
    if window >= Duration::from_secs(60 * 60) {
        format!("{}h", window.as_secs() / 3600)
    } else {
        format!("{}m", window.as_secs() / 60)
    }
}

pub(super) fn latency_chart_zoom_in(window: Duration) -> Duration {
    (window / 2).max(LATENCY_CHART_MIN_WINDOW)
}

pub(super) fn latency_chart_zoom_out(window: Duration) -> Duration {
    (window * 2).min(LATENCY_CHART_MAX_WINDOW)
}

fn latency_chart_latest_ms(samples: &[NodeLatencySample]) -> Option<u64> {
    samples.iter().map(|sample| sample.recorded_at_ms).max()
}

fn latency_chart_window_start_ms(samples: &[NodeLatencySample], window: Duration) -> Option<u64> {
    let latest = latency_chart_latest_ms(samples)?;
    Some(latest.saturating_sub(window.as_millis() as u64))
}

fn latency_chart_windowed_samples(
    samples: &[NodeLatencySample],
    window: Duration,
) -> Vec<NodeLatencySample> {
    let Some(start) = latency_chart_window_start_ms(samples, window) else {
        return Vec::new();
    };
    samples
        .iter()
        .filter(|sample| sample.recorded_at_ms >= start)
        .cloned()
        .collect()
}

fn border_style(active: bool) -> Style {
    if active {
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    }
}

fn selected_style(active: bool) -> Style {
    if active {
        Style::default()
            .bg(Color::Blue)
            .fg(Color::White)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    }
}

pub(super) fn node_order_badge(latency_sort_mode: bool) -> &'static str {
    if latency_sort_mode {
        "LATENCY ORDER"
    } else {
        "SELECTOR ORDER"
    }
}

pub(super) fn pick_mode_badge(auto_select_enabled: bool) -> &'static str {
    if auto_select_enabled {
        "Auto"
    } else {
        "Manual"
    }
}

fn format_connection_line(connection: &ConnectionInfo, max_width: usize) -> String {
    let source = format_connection_source(connection);
    let target = format_connection_target(connection);
    let chain = if connection.chains.is_empty() {
        "-".to_string()
    } else {
        connection.chains.join(" -> ")
    };
    let rule = connection.rule.as_deref().unwrap_or("-");
    truncate_for_width(
        &format!("{source:<14} {target:<28} {chain}  {rule}"),
        max_width,
    )
}

fn format_connection_source(connection: &ConnectionInfo) -> String {
    let kind = connection.metadata.kind.as_deref().unwrap_or("-");
    let network = connection.metadata.network.as_deref().unwrap_or("-");
    format!("{kind}/{network}")
}

fn format_connection_target(connection: &ConnectionInfo) -> String {
    let target = connection
        .metadata
        .host
        .as_deref()
        .filter(|value| !value.is_empty())
        .or(connection.metadata.destination_ip.as_deref())
        .unwrap_or("-");
    match connection.metadata.destination_port.as_deref() {
        Some(port) if !port.is_empty() => format!("{target}:{port}"),
        _ => target.to_string(),
    }
}

pub(super) fn format_bytes_opt(bytes: Option<u64>) -> String {
    bytes.map(format_bytes).unwrap_or_else(|| "-".to_string())
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit_index = 0;
    while value >= 1024.0 && unit_index + 1 < UNITS.len() {
        value /= 1024.0;
        unit_index += 1;
    }
    if unit_index == 0 {
        format!("{bytes}B")
    } else {
        format!("{value:.1}{}", UNITS[unit_index])
    }
}

pub(super) fn subscription_report_badge(report: &SubscriptionRefreshOutput) -> String {
    report
        .providers
        .iter()
        .map(|provider| {
            let warning = provider
                .warning
                .as_ref()
                .map(|warning| format!(" {}", truncate_for_width(warning, 24)))
                .unwrap_or_default();
            format!(
                "{}:{}:{} nodes{}",
                provider.provider, provider.status, provider.imported_nodes, warning
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

pub(super) fn format_duration_badge(duration: Duration) -> String {
    let secs = duration.as_secs();
    if secs >= 24 * 60 * 60 {
        let days = secs / (24 * 60 * 60);
        let hours = (secs % (24 * 60 * 60)) / 3600;
        if hours == 0 {
            format!("{days}d")
        } else {
            format!("{days}d{hours}h")
        }
    } else if secs >= 3600 {
        let hours = secs / 3600;
        let minutes = (secs % 3600) / 60;
        if minutes == 0 {
            format!("{hours}h")
        } else {
            format!("{hours}h{minutes}m")
        }
    } else if secs >= 60 {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

fn centered_rect(width: u16, height: u16, area: ratatui::layout::Rect) -> ratatui::layout::Rect {
    let [vertical] = Layout::vertical([Constraint::Length(height)])
        .flex(ratatui::layout::Flex::Center)
        .areas(area);
    let [horizontal] = Layout::horizontal([Constraint::Length(width)])
        .flex(ratatui::layout::Flex::Center)
        .areas(vertical);
    horizontal
}

pub(super) fn truncate_for_width(value: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    let width = unicode_width::UnicodeWidthStr::width(value);
    if width <= max_width {
        return value.to_string();
    }
    let mut output = String::new();
    let mut current_width = 0;
    for ch in value.chars() {
        let char_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if current_width + char_width + 1 > max_width {
            break;
        }
        output.push(ch);
        current_width += char_width;
    }
    output.push('…');
    output
}

pub(super) fn private_access_progress_title(profile: &PrivateAccessProfileRuntime) -> String {
    format!(
        "Private Access - {} ({})",
        profile.id,
        profile.mode.as_str()
    )
}

fn private_access_state_badge(state: PrivateAccessState) -> &'static str {
    match state {
        PrivateAccessState::Disabled => "DISABLED",
        PrivateAccessState::Disconnected => "DISCONNECTED",
        PrivateAccessState::Connecting => "CONNECTING",
        PrivateAccessState::Connected => "CONNECTED",
        PrivateAccessState::Disconnecting => "DISCONNECTING",
        PrivateAccessState::Error => "ERROR",
    }
}

fn private_access_state_style(state: &PrivateAccessState) -> Style {
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

pub(super) fn render(frame: &mut Frame, snapshot: &DashboardSnapshot<'_>) {
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
            let (marker_style, loading_suffix) = match row.tone {
                CandidateTone::Pending => (
                    Style::default()
                        .fg(Color::LightYellow)
                        .add_modifier(Modifier::BOLD | Modifier::SLOW_BLINK),
                    "  ⟳",
                ),
                CandidateTone::Success => (Style::default().fg(Color::Magenta), ""),
                CandidateTone::Error => (
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                    "",
                ),
                CandidateTone::Missing => (Style::default().fg(Color::DarkGray), ""),
            };
            ListItem::new(Line::from(vec![
                Span::styled(
                    truncate_for_width(&row.name, members_area.width.saturating_sub(16) as usize),
                    style,
                ),
                Span::raw("  "),
                Span::styled(row.marker.clone(), marker_style),
                Span::raw(loading_suffix),
                Span::raw(if row.is_current { "  *" } else { "" }),
            ]))
        })
        .collect::<Vec<_>>();

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
    frame.render_stateful_widget(members_widget, members_area, &mut members_state);

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
    if let Some(chart) = snapshot.latency_chart {
        draw_latency_chart(frame, chart);
    }
    if let Some(connections) = snapshot.connections.as_ref() {
        draw_connections_panel(frame, connections);
    }
    if let Some(help_index) = snapshot.help_index {
        draw_help_panel(frame, help_index);
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

pub(super) fn private_access_detail_view(
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

fn status_lines(status: &StatusSnapshot) -> Vec<Line<'static>> {
    vec![
        Line::from(vec![
            Span::styled("System Proxy: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                if status.system_proxy_enabled {
                    "enabled"
                } else {
                    "disabled"
                },
                Style::default().fg(if status.system_proxy_enabled {
                    Color::Green
                } else {
                    Color::DarkGray
                }),
            ),
            Span::raw("  "),
            Span::styled("Tun Mode: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                if status.tun_enabled {
                    "enabled"
                } else {
                    "disabled"
                },
                Style::default().fg(if status.tun_enabled {
                    Color::Green
                } else {
                    Color::DarkGray
                }),
            ),
            Span::raw("  "),
            Span::raw(status.selection_context.clone()),
        ]),
        Line::from(status.connections.clone()),
        Line::from(status.subscription.clone()),
        Line::from(status.sing_box.clone()),
    ]
}

fn status_footer_line(footer: &StatusFooter) -> Line<'_> {
    let line = match footer {
        StatusFooter::Filter(input) => Line::from(vec![
            Span::styled("Filter: ", Style::default().fg(Color::Cyan)),
            Span::raw(input.as_str()),
            Span::styled(
                "  Enter apply  Esc cancel",
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        StatusFooter::Bypass(input) => Line::from(vec![
            Span::styled("Bypass: ", Style::default().fg(Color::Cyan)),
            Span::raw(input.as_str()),
            Span::styled(
                "  domains/IPs/CIDRs comma-separated  Enter save  Esc cancel",
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        StatusFooter::Status(status) => Line::from(status.as_str()),
    };
    line.patch_style(Style::default().fg(Color::DarkGray))
}

#[derive(Clone, Copy)]
struct HelpBinding {
    key: &'static str,
    summary: &'static str,
    detail: &'static str,
}

const HELP_BINDINGS: &[HelpBinding] = &[
    HelpBinding {
        key: "up",
        summary: "Move up / scroll details",
        detail: "Move the highlighted row up, or scroll Intranet Proxy details when the right pane is focused.",
    },
    HelpBinding {
        key: "k",
        summary: "Move selection up",
        detail: "Vim-style shortcut for moving the highlighted row up.",
    },
    HelpBinding {
        key: "down",
        summary: "Move down / scroll details",
        detail: "Move the highlighted row down, crossing from Internet Proxy into Intranet Proxy, or scroll right-pane details.",
    },
    HelpBinding {
        key: "j",
        summary: "Move selection down",
        detail: "Vim-style shortcut for moving the highlighted row down.",
    },
    HelpBinding {
        key: "tab",
        summary: "Switch pane",
        detail: "Move focus between the selector/group pane and the candidate node pane.",
    },
    HelpBinding {
        key: "h",
        summary: "Switch to left pane",
        detail: "Move focus to the pane on the left.",
    },
    HelpBinding {
        key: "l",
        summary: "Switch to right pane",
        detail: "Move focus to the pane on the right.",
    },
    HelpBinding {
        key: "g",
        summary: "Move to first item",
        detail: "Jump to the first item in the focused list.",
    },
    HelpBinding {
        key: "G",
        summary: "Move to last item",
        detail: "Jump to the last item in the focused list.",
    },
    HelpBinding {
        key: "space",
        summary: "Activate selection",
        detail: "Apply an Internet Proxy selection, or open the selected Intranet Proxy profile details.",
    },
    HelpBinding {
        key: "m",
        summary: "Cycle Clash mode",
        detail: "Switch the controller between available Clash modes.",
    },
    HelpBinding {
        key: "s",
        summary: "Toggle latency sort order",
        detail: "Toggle candidate display between selector order and successful latency order.",
    },
    HelpBinding {
        key: "T",
        summary: "Test current group latency",
        detail: "Start an asynchronous latency test for all visible candidates in the selected group.",
    },
    HelpBinding {
        key: "t",
        summary: "Test selected node latency",
        detail: "Start an asynchronous latency test for only the highlighted node.",
    },
    HelpBinding {
        key: "/",
        summary: "Edit latency filter",
        detail: "Open the filter editor. Comma-separated values include matches; prefix with ! or - to exclude.",
    },
    HelpBinding {
        key: "a",
        summary: "Toggle auto-pick",
        detail: "Enable or disable periodic latency tests and automatic switching for the current filter, or all nodes when the filter is empty.",
    },
    HelpBinding {
        key: "i",
        summary: "Open latency chart",
        detail: "Show SQLite-backed latency history for the highlighted node.",
    },
    HelpBinding {
        key: "z",
        summary: "Zoom latency chart in",
        detail: "When the latency chart is open, narrow the displayed time window.",
    },
    HelpBinding {
        key: "Z",
        summary: "Zoom latency chart out",
        detail: "When the latency chart is open, widen the displayed time window.",
    },
    HelpBinding {
        key: "c",
        summary: "Show active connections",
        detail: "Open a panel with active connection targets, outbound chains, and matched rules.",
    },
    HelpBinding {
        key: "b",
        summary: "Edit bypass rules",
        detail: "Edit direct-bypass domains, IPs, and CIDRs written to the local rule-set.",
    },
    HelpBinding {
        key: "B",
        summary: "Keep sing-box running",
        detail: "Exit the TUI while leaving sing-box, auto-pick, and active Private Access sessions running in the background.",
    },
    HelpBinding {
        key: "p",
        summary: "Toggle system proxy",
        detail: "Enable or disable the OS system proxy for the detected sing-box mixed inbound.",
    },
    HelpBinding {
        key: "\\",
        summary: "Toggle TUN mode",
        detail: "Add or remove the sing-box TUN inbound and restart sing-box to capture system traffic. Needs administrator/root privileges on macOS and Linux.",
    },
    HelpBinding {
        key: "u",
        summary: "Update subscriptions",
        detail: "Force a background subscription refresh when subscription refresh is configured.",
    },
    HelpBinding {
        key: "v",
        summary: "Verify network",
        detail: "Run configured connectivity checks in the background.",
    },
    HelpBinding {
        key: "V",
        summary: "Toggle Private Access",
        detail: "Connect or disconnect the selected Intranet Proxy profile.",
    },
    HelpBinding {
        key: "o",
        summary: "Open settings",
        detail: "Edit TUI latency, auto-pick, and system proxy settings.",
    },
    HelpBinding {
        key: "r",
        summary: "Refresh groups",
        detail: "Reload selector groups, mode, and connection state from the controller.",
    },
    HelpBinding {
        key: "?",
        summary: "Close help",
        detail: "Close this keybindings panel.",
    },
    HelpBinding {
        key: "esc",
        summary: "Close help",
        detail: "Close this keybindings panel.",
    },
    HelpBinding {
        key: "enter",
        summary: "Close help",
        detail: "Close this keybindings panel.",
    },
    HelpBinding {
        key: "q",
        summary: "Quit",
        detail: "Exit the TUI. When help is open, q still exits the application.",
    },
];

pub(super) fn help_binding_count() -> usize {
    HELP_BINDINGS.len()
}

#[cfg(test)]
fn has_help_binding(key: &str, summary: &str) -> bool {
    HELP_BINDINGS
        .iter()
        .any(|binding| binding.key == key && binding.summary == summary)
}

fn draw_help_panel(frame: &mut Frame, help_index: usize) {
    let frame_area = frame.area();
    let width = frame_area.width.saturating_sub(4).min(108);
    let height = frame_area.height.saturating_sub(4).min(34);
    let area = centered_rect(width.max(56), height.max(18), frame_area);
    frame.render_widget(Clear, area);
    let [list_area, detail_area] =
        Layout::vertical([Constraint::Min(10), Constraint::Length(3)]).areas(area);
    let selected = help_index.min(HELP_BINDINGS.len().saturating_sub(1));
    let visible_rows = list_area.height.saturating_sub(2).max(1) as usize;
    let first = selected.saturating_sub(visible_rows.saturating_sub(1));

    let lines = HELP_BINDINGS
        .iter()
        .enumerate()
        .skip(first)
        .take(visible_rows)
        .map(|(index, binding)| help_binding(*binding, index == selected))
        .collect::<Vec<_>>();

    let widget = Paragraph::new(lines).block(
        Block::default()
            .title("Keybindings")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Green)),
    );
    frame.render_widget(widget, list_area);

    let count = format!("{} of {}", selected + 1, HELP_BINDINGS.len());
    let count_width = unicode_width::UnicodeWidthStr::width(count.as_str()) as u16;
    let count_area = ratatui::layout::Rect {
        x: list_area.x + list_area.width.saturating_sub(count_width + 1),
        y: list_area.y + list_area.height.saturating_sub(1),
        width: count_width,
        height: 1,
    };
    frame.render_widget(
        Paragraph::new(count).style(Style::default().fg(Color::Green)),
        count_area,
    );

    let detail = HELP_BINDINGS
        .get(selected)
        .map(|binding| binding.detail)
        .unwrap_or("");
    frame.render_widget(
        Paragraph::new(detail)
            .style(Style::default().fg(Color::Gray))
            .block(Block::default().borders(Borders::ALL)),
        detail_area,
    );
}

fn help_binding(binding: HelpBinding, selected: bool) -> Line<'static> {
    let line_style = if selected {
        Style::default().bg(Color::Blue)
    } else {
        Style::default()
    };
    Line::from(vec![
        Span::raw("  "),
        Span::styled(
            format!("{:>7}", binding.key),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(binding.summary, Style::default().fg(Color::Gray)),
    ])
    .style(line_style)
}

fn draw_settings_panel(frame: &mut Frame, settings: &SettingsPanelSnapshot) {
    let frame_area = frame.area();
    let area = centered_rect(96, 26, frame_area);
    frame.render_widget(Clear, area);
    let selected = settings.selected.min(settings.rows.len().saturating_sub(1));
    let mut lines = Vec::new();
    lines.push(Line::from(vec![
        Span::styled("Enter", Style::default().fg(Color::Cyan)),
        Span::raw(" edit  "),
        Span::styled("Esc", Style::default().fg(Color::Cyan)),
        Span::raw(" close"),
    ]));
    lines.push(Line::raw(""));
    for (index, row) in settings.rows.iter().enumerate() {
        let marker = if index == selected { "> " } else { "  " };
        let style = if index == selected {
            Style::default().bg(Color::Blue)
        } else {
            Style::default()
        };
        lines.push(
            Line::from(vec![
                Span::raw(marker),
                Span::styled(row.label, Style::default().fg(Color::Cyan)),
                Span::raw("  "),
                Span::raw(row.value.as_str()),
            ])
            .style(style),
        );
    }
    if let Some((label, input)) = &settings.editing {
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::styled("Editing ", Style::default().fg(Color::Yellow)),
            Span::raw(*label),
            Span::raw(": "),
            Span::raw(input.as_str()),
        ]));
    }
    if let Some(error) = settings.error.as_deref() {
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::styled(
                "Error: ",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                truncate_for_width(error, area.width.saturating_sub(12) as usize),
                Style::default().fg(Color::Red),
            ),
        ]));
    }

    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .title("Settings")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Green)),
        ),
        area,
    );
}

fn draw_onboarding_panel(frame: &mut Frame, onboarding: &OnboardingState) {
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

fn draw_private_access_progress_panel(frame: &mut Frame, progress: &PrivateAccessProgressModal) {
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

fn draw_private_access_auth_panel(frame: &mut Frame, auth: &PrivateAccessAuthModal) {
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

pub(super) fn private_access_auth_display_value(
    _field: &PrivateAccessAuthField,
    input: &str,
) -> String {
    input.to_string()
}

pub(super) fn private_access_auth_initial_value(
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

pub(super) fn settings_field_label(field: SettingsField) -> &'static str {
    match field {
        SettingsField::BenchmarkUrl => "Latency URL",
        SettingsField::BenchmarkTimeoutMs => "Latency timeout ms",
        SettingsField::RequestTimeoutSec => "Request timeout sec",
        SettingsField::MaxConcurrency => "Max concurrency",
        SettingsField::VerifyTargets => "Verification targets",
        SettingsField::AutoPickThresholdMs => "Auto-pick threshold ms",
        SettingsField::AutoPickIntervalSec => "Auto-pick interval sec",
        SettingsField::SystemProxyServer => "System proxy server",
        SettingsField::ChinaIpRouting => "China IP routing",
        SettingsField::PrivateAccessProfile => "Private Access profile",
        SettingsField::PrivateAccessManifestPath => "Private Access service manifest",
        SettingsField::PrivateAccessMode => "Private Access mode",
        SettingsField::PrivateAccessServer => "Private Access server",
        SettingsField::PrivateAccessPort => "Private Access port",
        SettingsField::PrivateAccessUsername => "Private Access username",
        SettingsField::PrivateAccessPassword => "Private Access password",
        SettingsField::PrivateAccessPasswordEnv => "Private Access password env",
        SettingsField::PrivateAccessBridgeListen => "Private Access bridge listen",
        SettingsField::PrivateAccessUseInternetProxy => "SonicWall use Internet proxy",
        SettingsField::PrivateAccessTlsVerify => "Private Access TLS verify",
    }
}

fn draw_connections_panel(frame: &mut Frame, snapshot: &ConnectionsPanelSnapshot<'_>) {
    let frame_area = frame.area();
    let width = frame_area.width.saturating_sub(4).min(120);
    let height = frame_area.height.saturating_sub(4).min(24);
    let area = centered_rect(width.max(20), height.max(8), frame_area);
    frame.render_widget(Clear, area);

    let inner_width = area.width.saturating_sub(4) as usize;
    let max_rows = area.height.saturating_sub(6) as usize;
    let mut lines = vec![
        Line::from(snapshot.summary.as_str()),
        Line::from(vec![
            Span::styled("Source", Style::default().fg(Color::Cyan)),
            Span::raw("  "),
            Span::styled("Target", Style::default().fg(Color::Cyan)),
            Span::raw("  "),
            Span::styled("Chain", Style::default().fg(Color::Cyan)),
        ]),
    ];

    if let Some(error) = snapshot.error {
        lines.push(Line::from(format!(
            "error: {}",
            truncate_for_width(error, inner_width.saturating_sub(7))
        )));
    } else if snapshot.connections.connections.is_empty() {
        lines.push(Line::from("No active connections"));
    } else {
        lines.extend(
            snapshot
                .connections
                .connections
                .iter()
                .take(max_rows)
                .map(|connection| Line::from(format_connection_line(connection, inner_width))),
        );
        let hidden = snapshot
            .connections
            .connections
            .len()
            .saturating_sub(max_rows);
        if hidden > 0 {
            lines.push(Line::from(format!("... {hidden} more connections")));
        }
    }

    let widget = Paragraph::new(lines).block(
        Block::default()
            .title("Active Connections (c/Esc close, r refresh)")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan)),
    );
    frame.render_widget(widget, area);
}

fn draw_latency_chart(frame: &mut Frame, chart: &LatencyChartState) {
    let area = centered_rect(90, 20, frame.area());
    frame.render_widget(Clear, area);

    let visible_samples = latency_chart_windowed_samples(&chart.samples, chart.window);
    let segments = latency_chart_segments(&visible_samples);
    let Some(start_ms) = latency_chart_window_start_ms(&chart.samples, chart.window) else {
        frame.render_widget(
            Paragraph::new("No latency history")
                .block(Block::default().title("Latency").borders(Borders::ALL)),
            area,
        );
        return;
    };
    let time_unit = latency_chart_time_unit(chart.window);
    let scale = match time_unit {
        LatencyChartTimeUnit::Minutes => 60_000.0,
        LatencyChartTimeUnit::Hours => 3_600_000.0,
    };
    let segment_data = segments
        .iter()
        .map(|segment| {
            segment
                .iter()
                .map(|point| {
                    (
                        point.0.saturating_sub(start_ms) as f64 / scale,
                        point.1 as f64,
                    )
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();

    if segment_data.iter().all(Vec::is_empty) {
        frame.render_widget(
            Paragraph::new("No successful latency samples in this window")
                .block(Block::default().title("Latency").borders(Borders::ALL)),
            area,
        );
        return;
    }

    let (min_y, max_y) = segment_data
        .iter()
        .flatten()
        .fold((f64::MAX, f64::MIN), |(min_y, max_y), (_, y)| {
            (min_y.min(*y), max_y.max(*y))
        });
    let x_max = chart.window.as_millis() as f64 / scale;
    let x_bounds = [0.0, x_max.max(1.0)];
    let y_bounds = latency_chart_y_bounds(min_y, max_y, chart.threshold_ms);
    let title = format!(
        "Latency: {} / {} ({} samples, window {}, z/Z zoom)",
        chart.selector,
        truncate_for_width(&chart.node, 36),
        visible_samples.len(),
        latency_chart_window_label(chart.window)
    );
    let mut datasets = segment_data
        .iter()
        .enumerate()
        .map(|(index, data)| {
            Dataset::default()
                .name(format!("latency-{index}"))
                .marker(symbols::Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::default().fg(Color::Magenta))
                .data(data)
        })
        .collect::<Vec<_>>();
    let threshold_data = latency_chart_threshold_line(x_bounds[1], chart.threshold_ms);
    datasets.push(
        Dataset::default()
            .name(format!("{}ms limit", chart.threshold_ms))
            .marker(symbols::Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(Color::Yellow))
            .data(&threshold_data),
    );
    let chart_widget = Chart::new(datasets)
        .block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        )
        .x_axis(
            Axis::default()
                .title(match time_unit {
                    LatencyChartTimeUnit::Minutes => "time (minutes)",
                    LatencyChartTimeUnit::Hours => "time (hours)",
                })
                .style(Style::default().fg(Color::Gray))
                .bounds(x_bounds)
                .labels(vec![
                    Span::raw(format!("{} ago", latency_chart_window_label(chart.window))),
                    Span::raw("now"),
                ]),
        )
        .y_axis(
            Axis::default()
                .title("latency (ms)")
                .style(Style::default().fg(Color::Gray))
                .bounds(y_bounds)
                .labels(vec![
                    Span::raw(format!("{:.0}", y_bounds[0])),
                    Span::raw(format!("{:.0}", y_bounds[1])),
                ]),
        );
    frame.render_widget(chart_widget, area);
}

fn latency_chart_segments(samples: &[NodeLatencySample]) -> Vec<Vec<(u64, u64)>> {
    let mut segments = Vec::new();
    let mut current = Vec::new();
    for sample in samples {
        if let Some(delay_ms) = sample.delay_ms {
            current.push((sample.recorded_at_ms, delay_ms));
        } else if !current.is_empty() {
            segments.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        segments.push(current);
    }
    segments
}

fn latency_chart_threshold_line(x_max: f64, threshold_ms: u64) -> Vec<(f64, f64)> {
    vec![
        (0.0, threshold_ms as f64),
        (x_max.max(1.0), threshold_ms as f64),
    ]
}

fn latency_chart_y_bounds(min_y: f64, max_y: f64, threshold_ms: u64) -> [f64; 2] {
    let min_y = min_y.min(threshold_ms as f64);
    let max_y = max_y.max(threshold_ms as f64);
    let padding = ((max_y - min_y) * 0.05).max(10.0);
    [0.0_f64.max(min_y - padding), max_y + padding]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::{ConnectionInfo, ConnectionMetadata};
    use crate::private_access::PrivateAccessRoute;
    use crate::subscriptions::ProviderRefreshSummary;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn dashboard_snapshot<'a>() -> DashboardSnapshot<'a> {
        DashboardSnapshot {
            focus: Focus::Groups,
            left_pane_section: LeftPaneSection::Internet,
            internet_rows: vec![InternetRow {
                name: "select".to_string(),
                current: "node-a".to_string(),
                is_current: true,
            }],
            internet_selected: 0,
            intranet_rows: Vec::new(),
            intranet_selected: 0,
            candidate_title: "Candidates [SELECTOR ORDER]".to_string(),
            candidate_rows: vec![CandidateRow {
                name: "node-a".to_string(),
                is_current: true,
                marker: "42ms".to_string(),
                tone: CandidateTone::Success,
            }],
            candidate_selected: Some(0),
            intranet_detail: None,
            status: StatusSnapshot {
                system_proxy_enabled: false,
                tun_enabled: true,
                selection_context: "clash=rule  Pick=Manual  filter=''".to_string(),
                connections: "connections active=1 proxy=1 direct=0".to_string(),
                subscription: "subscriptions: disabled".to_string(),
                sing_box: "sing-box: managed".to_string(),
                footer: StatusFooter::Status("ready".to_string()),
            },
            flash: None,
            latency_chart: None,
            connections: None,
            help_index: None,
            settings: None,
            onboarding: None,
            private_access_progress: None,
            private_access_auth: None,
        }
    }

    fn rendered_lines(snapshot: &DashboardSnapshot<'_>) -> Vec<String> {
        let backend = TestBackend::new(110, 30);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| render(frame, snapshot))
            .expect("dashboard renders");
        terminal
            .backend()
            .buffer()
            .content
            .chunks(110)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect()
    }

    #[test]
    fn render_consumes_a_dashboard_snapshot_without_app_state() {
        let lines = rendered_lines(&dashboard_snapshot());
        let text = lines.join("\n");

        assert!(text.contains("Internet Proxy"));
        assert!(text.contains("select"));
        assert!(text.contains("node-a"));
        assert!(text.contains("42ms"));
        assert!(text.contains("System Proxy: disabled"));
        assert!(text.contains("Tun Mode: enabled"));
        assert!(!text.contains("Intranet Proxy"));
        assert!(has_help_binding("\\", "Toggle TUN mode"));
    }

    #[test]
    fn status_footer_is_rendered_below_its_box() {
        let lines = rendered_lines(&dashboard_snapshot());
        let message_row = lines
            .iter()
            .position(|line| line.contains("ready"))
            .expect("status footer row");

        assert!(message_row > 0);
        assert!(!lines[message_row].contains('─'));
        assert!(lines[message_row - 1].contains('└'));
        assert!(lines[message_row - 1].contains('┘'));
    }

    #[test]
    fn settings_overlay_uses_typed_rows() {
        let mut snapshot = dashboard_snapshot();
        snapshot.settings = Some(SettingsPanelSnapshot {
            rows: vec![SettingRow {
                label: "Latency URL",
                value: "https://example.test/ping".to_string(),
            }],
            selected: 0,
            editing: None,
            error: None,
        });

        let text = rendered_lines(&snapshot).join("\n");
        assert!(text.contains("Settings"));
        assert!(text.contains("Latency URL"));
        assert!(text.contains("https://example.test/ping"));
    }

    #[test]
    fn intranet_detail_is_rendered_from_the_typed_profile_snapshot() {
        let mut profile =
            PrivateAccessProfileRuntime::default_hillstone().expect("Hillstone profile");
        profile.server = "vpn.example.com".to_string();
        profile.state = PrivateAccessState::Connected;
        profile.routes = vec![PrivateAccessRoute {
            cidr: "10.20.0.0/16".to_string(),
        }];
        profile.dns = vec!["10.20.0.53".to_string()];
        profile.domains = vec!["portal.internal.example".to_string()];
        profile.domain_suffixes = vec!["corp.example".to_string()];
        let expanded_sections = BTreeSet::new();
        let mut snapshot = dashboard_snapshot();
        snapshot.left_pane_section = LeftPaneSection::Intranet;
        snapshot.intranet_rows = vec![IntranetRow {
            id: profile.id.clone(),
            state: profile.state.clone(),
            background: false,
        }];
        snapshot.intranet_detail = Some(IntranetDetailSnapshot {
            profile: &profile,
            expanded_sections: &expanded_sections,
            scroll: 0,
            active: true,
        });

        let text = rendered_lines(&snapshot).join("\n");
        assert!(text.contains("Intranet Proxy"));
        assert!(text.contains("Intranet: hillstone"));
        assert!(text.contains("vpn.example.com:4433"));
        assert!(text.contains("10.20.0.0/16"));
        assert!(text.contains("10.20.0.53"));
        assert!(text.contains("portal.internal.example"));
        assert!(text.contains("*.corp.example"));
        assert!(text.contains("Enter expand/fold"));
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

    #[test]
    fn connection_and_byte_values_are_formatted_for_the_panel() {
        let connection = ConnectionInfo {
            id: "connection-1".to_string(),
            upload: 0,
            download: 0,
            start: None,
            chains: vec!["node-a".to_string(), "airtcp".to_string()],
            rule: Some("route(select)".to_string()),
            rule_payload: None,
            metadata: ConnectionMetadata {
                network: Some("tcp".to_string()),
                kind: Some("tun/tun-in".to_string()),
                source_ip: Some("172.19.0.1".to_string()),
                destination_ip: Some("1.1.1.1".to_string()),
                host: Some("www.google.com".to_string()),
                destination_port: Some("443".to_string()),
                source_port: None,
                process_path: None,
            },
        };

        assert_eq!(format_bytes(512), "512B");
        assert_eq!(format_bytes(2048), "2.0KiB");
        assert!(format_connection_line(&connection, 120).contains("www.google.com:443"));
        assert!(format_connection_line(&connection, 120).contains("node-a -> airtcp"));
    }

    #[test]
    fn subscription_and_duration_badges_are_compact() {
        let report = SubscriptionRefreshOutput {
            input_path: ".suburl".to_string(),
            cache_path: ".suburl.cache.json".to_string(),
            interval_days: 1,
            merged_config_path: "/usr/local/etc/sing-box/config.json".to_string(),
            backup_config_path: None,
            providers: vec![
                ProviderRefreshSummary {
                    provider: "宝贝云".to_string(),
                    subscription_url: "https://example.com/redacted".to_string(),
                    status: "fetched".to_string(),
                    imported_nodes: 67,
                    fetched_at_unix: 10,
                    warning: None,
                },
                ProviderRefreshSummary {
                    provider: "airtcp".to_string(),
                    subscription_url: "https://example.com/redacted".to_string(),
                    status: "cached".to_string(),
                    imported_nodes: 0,
                    fetched_at_unix: 10,
                    warning: Some("no mergeable nodes found".to_string()),
                },
            ],
        };

        let badge = subscription_report_badge(&report);
        assert!(badge.contains("宝贝云:fetched:67 nodes"));
        assert!(badge.contains("airtcp:cached:0 nodes"));
        assert!(badge.contains("no mergeable nodes found"));
        assert_eq!(format_duration_badge(Duration::from_secs(42)), "42s");
        assert_eq!(format_duration_badge(Duration::from_secs(5 * 60)), "5m");
        assert_eq!(
            format_duration_badge(Duration::from_secs(2 * 3600 + 30 * 60)),
            "2h30m"
        );
        assert_eq!(
            format_duration_badge(Duration::from_secs(24 * 3600 + 3600)),
            "1d1h"
        );
    }

    #[test]
    fn latency_chart_helpers_define_the_visible_series() {
        let samples = vec![
            NodeLatencySample {
                recorded_at_ms: 0,
                delay_ms: Some(90),
            },
            NodeLatencySample {
                recorded_at_ms: 1,
                delay_ms: None,
            },
            NodeLatencySample {
                recorded_at_ms: 45 * 60 * 1000,
                delay_ms: Some(120),
            },
            NodeLatencySample {
                recorded_at_ms: 60 * 60 * 1000,
                delay_ms: Some(80),
            },
        ];

        assert_eq!(
            latency_chart_segments(&samples),
            vec![
                vec![(0, 90)],
                vec![(45 * 60 * 1000, 120), (60 * 60 * 1000, 80)]
            ]
        );
        assert_eq!(
            latency_chart_time_unit(Duration::from_secs(30 * 60)),
            LatencyChartTimeUnit::Minutes
        );
        assert_eq!(
            latency_chart_time_unit(Duration::from_secs(3 * 60 * 60)),
            LatencyChartTimeUnit::Hours
        );
        assert_eq!(
            latency_chart_zoom_in(Duration::from_secs(60 * 60)),
            Duration::from_secs(30 * 60)
        );
        assert_eq!(
            latency_chart_zoom_out(Duration::from_secs(60 * 60)),
            Duration::from_secs(2 * 60 * 60)
        );
        assert_eq!(
            latency_chart_threshold_line(30.0, 600),
            vec![(0.0, 600.0), (30.0, 600.0)]
        );

        let low_bounds = latency_chart_y_bounds(80.0, 120.0, 600);
        assert!(low_bounds[0] <= 80.0 && low_bounds[1] > 600.0);
        let high_bounds = latency_chart_y_bounds(700.0, 900.0, 600);
        assert!(high_bounds[0] < 600.0 && high_bounds[1] >= 900.0);

        let visible = latency_chart_windowed_samples(&samples, Duration::from_secs(30 * 60));
        assert_eq!(visible.len(), 2);
        assert_eq!(visible[0].recorded_at_ms, 45 * 60 * 1000);
        assert_eq!(visible[1].recorded_at_ms, 60 * 60 * 1000);
    }
}

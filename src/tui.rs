use std::collections::BTreeMap;
use std::env;
use std::io;
use std::sync::mpsc::{self, TryRecvError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Axis, Block, Borders, Chart, Clear, Dataset, GraphType, List, ListItem, ListState, Paragraph,
};
use ratatui::{DefaultTerminal, Frame};

use crate::controller::{
    ApiClient, BenchmarkEvent, BenchmarkJob, BenchmarkJobKind, BenchmarkRequest, BenchmarkSummary,
    ProxyGroup, run_verification, spawn_benchmark_worker,
};
use crate::defaults::{
    DEFAULT_BENCHMARK_MAX_CONCURRENCY, DEFAULT_CONTROLLER, DEFAULT_DELAY_TEST_URL,
    DEFAULT_DIRECT_TAG, DEFAULT_SELECTOR_TAG, DIRECT_TAG_ALIASES, REFRESH_DEBOUNCE,
    SINGLE_NODE_RETEST_DEBOUNCE,
};
use crate::storage::{
    BenchmarkRecord, BenchmarkStore, NodeLatencySample, default_benchmark_db_path,
};
use crate::tui_state::{
    BypassRuleSetStore, TuiRuntimeState, TuiStateStore, default_bypass_rule_set_path,
    default_tui_state_path, parse_bypass_entries,
};

const AUTO_SELECT_INTERVAL: Duration = Duration::from_secs(30);
const AUTO_SELECT_THRESHOLD_MS: u64 = 600;
const LATENCY_CHART_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const LATENCY_CHART_DEFAULT_WINDOW: Duration = Duration::from_secs(60 * 60);
const LATENCY_CHART_MIN_WINDOW: Duration = Duration::from_secs(5 * 60);
const LATENCY_CHART_MAX_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);
const DIRECT_CLASH_MODE: &str = "直连";

pub(crate) fn run_tui(controller: Option<String>, max_concurrency: Option<usize>) -> Result<()> {
    let controller = controller
        .or_else(|| env::var("SING_BOX_CONTROLLER").ok())
        .unwrap_or_else(|| DEFAULT_CONTROLLER.to_string());

    let secret = env::var("SING_BOX_SECRET")
        .ok()
        .filter(|value| !value.is_empty());

    let mut app = App::new(
        ApiClient::new(controller, secret)?,
        max_concurrency.unwrap_or(DEFAULT_BENCHMARK_MAX_CONCURRENCY),
    )?;
    let terminal = setup_terminal()?;
    let result = run_app(terminal, &mut app);
    restore_terminal()?;
    result
}

fn setup_terminal() -> Result<DefaultTerminal> {
    enable_raw_mode().context("failed to enable raw mode")?;
    execute!(io::stdout(), EnterAlternateScreen).context("failed to enter alternate screen")?;
    Ok(ratatui::DefaultTerminal::new(
        ratatui::backend::CrosstermBackend::new(io::stdout()),
    )?)
}

fn restore_terminal() -> Result<()> {
    disable_raw_mode().context("failed to disable raw mode")?;
    execute!(io::stdout(), LeaveAlternateScreen).context("failed to leave alternate screen")?;
    Ok(())
}

fn run_app(mut terminal: DefaultTerminal, app: &mut App) -> Result<()> {
    loop {
        app.poll_benchmark_updates()?;
        app.maybe_start_auto_select_benchmark()?;
        app.maybe_refresh_latency_chart()?;
        terminal.draw(|frame| draw(frame, app))?;
        if !event::poll(Duration::from_millis(250))? {
            continue;
        }

        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if !app.handle_key(key.code)? {
                    return Ok(());
                }
            }
            Event::Resize(_, _) => {}
            _ => {}
        }
    }
}

fn draw(frame: &mut Frame, app: &mut App) {
    let [main, status_area] =
        Layout::vertical([Constraint::Min(10), Constraint::Length(6)]).areas(frame.area());
    let implicit_root_mode = app.implicit_root_mode();
    let [groups_area, members_area] =
        Layout::horizontal([Constraint::Percentage(32), Constraint::Percentage(68)]).areas(main);

    let groups = app
        .displayed_group_names()
        .iter()
        .map(|group_name| {
            let group = app.group_by_name(group_name);
            let current = group
                .and_then(|group| group.current.as_deref())
                .map_or(String::from("unset"), ToString::to_string);
            let is_current = app
                .implicit_root_group()
                .and_then(|root| root.current.as_deref())
                == Some(group_name.as_str());
            let mut style = Style::default().fg(Color::Cyan);
            if implicit_root_mode && is_current {
                style = style.fg(Color::Green).add_modifier(Modifier::BOLD);
            }
            ListItem::new(Line::from(vec![
                Span::styled(
                    truncate_for_width(group_name, groups_area.width.saturating_sub(18) as usize),
                    style,
                ),
                Span::raw(" "),
                Span::styled(
                    format!("[{}]", truncate_for_width(&current, 14)),
                    Style::default().fg(Color::Yellow),
                ),
                Span::raw(if implicit_root_mode && is_current {
                    "  *"
                } else {
                    ""
                }),
            ]))
        })
        .collect::<Vec<_>>();

    let groups_title = if implicit_root_mode {
        "Choices"
    } else {
        "Selector Groups"
    };
    let groups_block = Block::default()
        .title(groups_title)
        .borders(Borders::ALL)
        .border_style(border_style(app.focus == Focus::Groups));
    let groups_widget = List::new(groups)
        .block(groups_block)
        .highlight_style(selected_style(app.focus == Focus::Groups))
        .highlight_symbol("> ");
    let mut groups_state = ListState::default().with_selected(Some(app.displayed_group_index()));
    frame.render_stateful_widget(groups_widget, groups_area, &mut groups_state);

    let displayed_members = app.displayed_members();
    let members = app
        .selected_member_panel_group()
        .map(|group| {
            displayed_members
                .iter()
                .map(|member| {
                    let is_current = group.current.as_deref() == Some(member.as_str());
                    let bench = app
                        .selected_benchmark()
                        .and_then(|summary| summary.find_result(member));
                    let mut style = Style::default();
                    if is_current {
                        style = style.fg(Color::Green).add_modifier(Modifier::BOLD);
                    }
                    let (marker, marker_style, loading_suffix) = match bench {
                        Some(result) if !result.completed => (
                            result.display_delay(),
                            Style::default()
                                .fg(Color::LightYellow)
                                .add_modifier(Modifier::BOLD | Modifier::SLOW_BLINK),
                            "  ⟳",
                        ),
                        Some(result) if result.delay.is_some() => (
                            result.display_delay(),
                            Style::default().fg(Color::Magenta),
                            "",
                        ),
                        Some(result) => (
                            result.display_delay(),
                            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                            "",
                        ),
                        None => ("-".to_string(), Style::default().fg(Color::DarkGray), ""),
                    };
                    ListItem::new(Line::from(vec![
                        Span::styled(
                            truncate_for_width(
                                member,
                                members_area.width.saturating_sub(16) as usize,
                            ),
                            style,
                        ),
                        Span::raw("  "),
                        Span::styled(marker, marker_style),
                        Span::raw(loading_suffix),
                        Span::raw(if is_current { "  *" } else { "" }),
                    ]))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let members_title = app
        .selected_member_panel_group()
        .map(|group| {
            format!(
                "Candidates for {} [{}]",
                group.name,
                benchmark_mode_badge(app.latency_sort_mode)
            )
        })
        .unwrap_or_else(|| {
            format!(
                "Candidates [{}]",
                benchmark_mode_badge(app.latency_sort_mode)
            )
        });
    let members_block = Block::default()
        .title(members_title)
        .borders(Borders::ALL)
        .border_style(border_style(app.focus == Focus::Members));
    let members_widget = List::new(members)
        .block(members_block)
        .highlight_style(selected_style(app.focus == Focus::Members))
        .highlight_symbol("> ");
    let mut members_state = ListState::default().with_selected(app.displayed_member_index());
    frame.render_stateful_widget(members_widget, members_area, &mut members_state);

    let benchmark_hint = app.selected_benchmark().map_or_else(
        || {
            format!(
                "mode={}  auto={}  b group benchmark  t node benchmark  a auto-pick  / filter",
                benchmark_mode_badge(app.latency_sort_mode),
                auto_select_badge(app.auto_select_enabled)
            )
        },
        |summary| {
            let best = summary
                .best_success()
                .map(|item| format!("best={} {}", item.name, item.display_delay()))
                .unwrap_or_else(|| "best=none".to_string());
            format!(
                "filter='{}'  tested={}  mode={}  auto={}  {}",
                summary.pattern,
                summary.results.len(),
                benchmark_mode_badge(app.latency_sort_mode),
                auto_select_badge(app.auto_select_enabled),
                truncate_for_width(&best, 30)
            )
        },
    );

    let bottom_line = if let Some(input) = app.filter_input.as_deref() {
        Line::from(vec![
            Span::styled("Filter: ", Style::default().fg(Color::Cyan)),
            Span::raw(input),
            Span::styled(
                "  Enter apply  Esc cancel",
                Style::default().fg(Color::DarkGray),
            ),
        ])
    } else if let Some(input) = app.bypass_input.as_deref() {
        Line::from(vec![
            Span::styled("Bypass: ", Style::default().fg(Color::Cyan)),
            Span::raw(input),
            Span::styled(
                "  domains/IPs/CIDRs comma-separated  Enter save  Esc cancel",
                Style::default().fg(Color::DarkGray),
            ),
        ])
    } else {
        Line::from(app.status_line())
    };

    let help = Paragraph::new(vec![
        Line::from(vec![
            Span::styled("Arrows/jk", Style::default().fg(Color::Cyan)),
            Span::raw(" move  "),
            Span::styled("Tab/h/l", Style::default().fg(Color::Cyan)),
            Span::raw(" switch pane  "),
            Span::styled("Space", Style::default().fg(Color::Cyan)),
            Span::raw(" select  "),
            Span::styled("d", Style::default().fg(Color::Cyan)),
            Span::raw(" direct  "),
            Span::styled("B", Style::default().fg(Color::Cyan)),
            Span::raw(" bypass  "),
            Span::styled("b/t", Style::default().fg(Color::Cyan)),
            Span::raw(" benchmark  "),
            Span::styled("s", Style::default().fg(Color::Cyan)),
            Span::raw(" view mode  "),
            Span::styled("a", Style::default().fg(Color::Cyan)),
            Span::raw(" auto-pick  "),
            Span::styled("i", Style::default().fg(Color::Cyan)),
            Span::raw(" info  "),
            Span::styled("v/V", Style::default().fg(Color::Cyan)),
            Span::raw(" verify  "),
            Span::styled("/", Style::default().fg(Color::Cyan)),
            Span::raw(" filter  "),
            Span::styled("r", Style::default().fg(Color::Cyan)),
            Span::raw(" refresh  "),
            Span::styled("q", Style::default().fg(Color::Cyan)),
            Span::raw(" quit"),
        ]),
        Line::from(vec![
            Span::styled("Controller: ", Style::default().fg(Color::DarkGray)),
            Span::raw(app.client.base_url.as_str()),
        ]),
        Line::from(benchmark_hint),
        bottom_line,
    ])
    .block(Block::default().title("Status").borders(Borders::ALL));
    frame.render_widget(help, status_area);

    if let Some(message) = app.flash_message() {
        let area = centered_rect(80, 7, frame.area());
        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new(message).block(Block::default().title("Info").borders(Borders::ALL)),
            area,
        );
    }
    if let Some(chart) = app.latency_chart.as_ref() {
        draw_latency_chart(frame, chart);
    }
    if let Some(input) = app.filter_input.as_deref() {
        let cursor_x = status_area
            .x
            .saturating_add(1)
            .saturating_add(unicode_width::UnicodeWidthStr::width("Filter: ") as u16)
            .saturating_add(unicode_width::UnicodeWidthStr::width(input) as u16);
        let cursor_y = status_area.y.saturating_add(4);
        frame.set_cursor_position((cursor_x, cursor_y));
    }
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
    let y_bounds = latency_chart_y_bounds(min_y, max_y, AUTO_SELECT_THRESHOLD_MS);
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
    let threshold_data = latency_chart_threshold_line(x_bounds[1], AUTO_SELECT_THRESHOLD_MS);
    datasets.push(
        Dataset::default()
            .name(format!("{AUTO_SELECT_THRESHOLD_MS}ms limit"))
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LatencyChartTimeUnit {
    Minutes,
    Hours,
}

fn latency_chart_time_unit(window: Duration) -> LatencyChartTimeUnit {
    if window >= Duration::from_secs(2 * 60 * 60) {
        LatencyChartTimeUnit::Hours
    } else {
        LatencyChartTimeUnit::Minutes
    }
}

fn latency_chart_window_label(window: Duration) -> String {
    if window >= Duration::from_secs(60 * 60) {
        format!("{}h", window.as_secs() / 3600)
    } else {
        format!("{}m", window.as_secs() / 60)
    }
}

fn latency_chart_zoom_in(window: Duration) -> Duration {
    (window / 2).max(LATENCY_CHART_MIN_WINDOW)
}

fn latency_chart_zoom_out(window: Duration) -> Duration {
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

fn benchmark_mode_badge(latency_sort_mode: bool) -> &'static str {
    if latency_sort_mode {
        "LATENCY SORT"
    } else {
        "FILTER VIEW"
    }
}

fn auto_select_badge(auto_select_enabled: bool) -> &'static str {
    if auto_select_enabled { "ON" } else { "OFF" }
}

#[derive(Debug, Eq, PartialEq)]
enum DirectSwitchAction {
    SelectorMember(String),
    ClashModeDirect,
}

fn direct_switch_action(members: &[String]) -> DirectSwitchAction {
    direct_member_name(members)
        .map(DirectSwitchAction::SelectorMember)
        .unwrap_or(DirectSwitchAction::ClashModeDirect)
}

fn direct_member_name(members: &[String]) -> Option<String> {
    members
        .iter()
        .find(|member| {
            member.as_str() == DEFAULT_DIRECT_TAG || DIRECT_TAG_ALIASES.contains(&member.as_str())
        })
        .cloned()
}

fn benchmark_job_kind_label(kind: &BenchmarkJobKind) -> &'static str {
    match kind {
        BenchmarkJobKind::Group => "group",
        BenchmarkJobKind::AutoSelect => "auto",
        BenchmarkJobKind::SingleNode { .. } => "single",
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

fn truncate_for_width(value: &str, max_width: usize) -> String {
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

fn matches_filter(value: &str, filter: &str) -> bool {
    let mut has_pattern = false;
    for pattern in filter
        .split([',', '，'])
        .map(str::trim)
        .filter(|pattern| !pattern.is_empty())
    {
        has_pattern = true;
        if value.contains(pattern) {
            return true;
        }
    }
    !has_pattern
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Focus {
    Groups,
    Members,
}

#[derive(Clone, Debug)]
struct LatencyChartState {
    selector: String,
    node: String,
    samples: Vec<NodeLatencySample>,
    window: Duration,
    last_refresh: Instant,
}

struct App {
    client: ApiClient,
    groups: Vec<ProxyGroup>,
    group_index: usize,
    provider_index: usize,
    member_index: usize,
    focus: Focus,
    status: String,
    flash: Option<(String, Instant)>,
    benchmark_filter: String,
    benchmark_url: String,
    benchmark_timeout_ms: u64,
    benchmark_request_timeout: f64,
    benchmark_max_concurrency: usize,
    benchmarks: BTreeMap<String, BenchmarkSummary>,
    benchmark_jobs: Vec<BenchmarkJob>,
    latency_sort_mode: bool,
    last_single_node_benchmark: Option<(String, String, Instant)>,
    filter_input: Option<String>,
    bypass_input: Option<String>,
    bypass_entries: Vec<String>,
    auto_select_enabled: bool,
    auto_select_threshold_ms: u64,
    auto_select_interval: Duration,
    last_auto_select_benchmark: Option<Instant>,
    benchmark_store: Option<BenchmarkStore>,
    state_store: Option<TuiStateStore>,
    bypass_rule_set_store: Option<BypassRuleSetStore>,
    latency_chart: Option<LatencyChartState>,
}

impl App {
    fn new(client: ApiClient, benchmark_max_concurrency: usize) -> Result<Self> {
        let state_store = TuiStateStore::new(default_tui_state_path());
        let runtime_state = state_store.load()?;
        let mut app = Self {
            client,
            groups: Vec::new(),
            group_index: 0,
            provider_index: 0,
            member_index: 0,
            focus: Focus::Groups,
            status: String::from("Loading proxy groups..."),
            flash: None,
            benchmark_filter: String::new(),
            benchmark_url: String::from(DEFAULT_DELAY_TEST_URL),
            benchmark_timeout_ms: 5000,
            benchmark_request_timeout: 12.0,
            benchmark_max_concurrency,
            benchmarks: BTreeMap::new(),
            benchmark_jobs: Vec::new(),
            latency_sort_mode: false,
            last_single_node_benchmark: None,
            filter_input: None,
            bypass_input: None,
            bypass_entries: Vec::new(),
            auto_select_enabled: false,
            auto_select_threshold_ms: AUTO_SELECT_THRESHOLD_MS,
            auto_select_interval: AUTO_SELECT_INTERVAL,
            last_auto_select_benchmark: None,
            benchmark_store: Some(BenchmarkStore::open(default_benchmark_db_path())?),
            state_store: Some(state_store),
            bypass_rule_set_store: Some(BypassRuleSetStore::new(default_bypass_rule_set_path())),
            latency_chart: None,
        };
        app.apply_runtime_state(runtime_state.clone());
        app.refresh()?;
        app.apply_runtime_state(runtime_state);
        app.save_bypass_rule_set()?;
        Ok(app)
    }

    fn apply_runtime_state(&mut self, state: TuiRuntimeState) {
        self.benchmark_filter = state.benchmark_filter;
        self.auto_select_enabled = state.auto_pick_enabled && !self.benchmark_filter.is_empty();
        self.bypass_entries = state.bypass_entries;
        self.last_auto_select_benchmark = None;
        if let Some(group) = self.selected_group()
            && let Some(node) = state.current_selected_nodes.get(&group.name)
        {
            let node = node.clone();
            self.sync_selection_to_member_name(&node);
        }
        self.sync_selection_to_displayed_members();
    }

    fn runtime_state(&self) -> TuiRuntimeState {
        TuiRuntimeState {
            benchmark_filter: self.benchmark_filter.clone(),
            auto_pick_enabled: self.auto_select_enabled,
            current_selected_nodes: self
                .groups
                .iter()
                .filter_map(|group| {
                    group
                        .current
                        .as_ref()
                        .map(|current| (group.name.clone(), current.clone()))
                })
                .collect(),
            bypass_entries: self.bypass_entries.clone(),
        }
    }

    fn save_runtime_state(&self) -> Result<()> {
        let Some(store) = &self.state_store else {
            return Ok(());
        };
        store.save(&self.runtime_state())
    }

    fn save_bypass_rule_set(&self) -> Result<()> {
        let Some(store) = &self.bypass_rule_set_store else {
            return Ok(());
        };
        store.save(&self.bypass_entries)
    }

    fn selected_group(&self) -> Option<&ProxyGroup> {
        if self.implicit_root_mode() {
            return self
                .selected_root_choice_name()
                .and_then(|name| self.group_by_name(&name));
        }
        self.groups.get(self.group_index)
    }

    fn group_by_name(&self, name: &str) -> Option<&ProxyGroup> {
        self.groups.iter().find(|group| group.name == name)
    }

    fn implicit_root_group(&self) -> Option<&ProxyGroup> {
        let root = self.group_by_name(DEFAULT_SELECTOR_TAG)?;
        let child_group_count = root
            .members
            .iter()
            .filter(|member| self.group_by_name(member).is_some())
            .count();
        if child_group_count > 1 {
            Some(root)
        } else {
            None
        }
    }

    fn implicit_root_mode(&self) -> bool {
        self.implicit_root_group().is_some()
    }

    fn displayed_group_names(&self) -> Vec<String> {
        if let Some(root) = self.implicit_root_group() {
            return root
                .members
                .iter()
                .filter(|member| self.group_by_name(member).is_some())
                .cloned()
                .collect();
        }
        self.groups.iter().map(|group| group.name.clone()).collect()
    }

    fn displayed_group_index(&self) -> usize {
        if self.implicit_root_mode() {
            self.provider_index
        } else {
            self.group_index
        }
    }

    fn selected_root_choice_name(&self) -> Option<String> {
        self.implicit_root_group().and_then(|root| {
            root.members
                .iter()
                .filter(|member| self.group_by_name(member).is_some())
                .nth(self.provider_index)
                .cloned()
        })
    }

    fn selected_member_panel_group(&self) -> Option<&ProxyGroup> {
        if self.implicit_root_mode() {
            let choice = self.selected_root_choice_name()?;
            return self.group_by_name(&choice);
        }
        self.selected_group()
    }

    fn selected_member_panel_is_manual_selector(&self) -> bool {
        self.selected_member_panel_group()
            .is_some_and(|group| group.kind.eq_ignore_ascii_case("selector"))
    }

    fn selected_benchmark(&self) -> Option<&BenchmarkSummary> {
        let group = self.selected_member_panel_group()?;
        self.benchmarks.get(&group.name)
    }

    fn member_matches_filter(&self, member: &str) -> bool {
        matches_filter(member, &self.benchmark_filter)
    }

    fn benchmark_candidates_for_group(&self, group: &ProxyGroup) -> Vec<String> {
        group
            .members
            .iter()
            .filter(|member| self.member_matches_filter(member))
            .cloned()
            .collect()
    }

    fn displayed_members(&self) -> Vec<String> {
        let Some(group) = self.selected_group() else {
            return Vec::new();
        };
        let group = self.selected_member_panel_group().unwrap_or(group);
        let Some(summary) = self.selected_benchmark() else {
            return group
                .members
                .iter()
                .filter(|member| self.member_matches_filter(member))
                .cloned()
                .collect();
        };
        if !self.latency_sort_mode {
            return group
                .members
                .iter()
                .filter(|member| self.member_matches_filter(member))
                .cloned()
                .collect();
        }

        let mut successes = Vec::new();
        let mut pending_or_untested = Vec::new();
        for (index, member) in group.members.iter().enumerate() {
            if !self.member_matches_filter(member) {
                continue;
            }
            match summary.find_result(member) {
                Some(result) if result.completed && result.delay.is_none() => {}
                Some(result) if result.completed => {
                    successes.push((result.delay.unwrap_or(u64::MAX), index, member.clone()))
                }
                _ => pending_or_untested.push((index, member.clone())),
            }
        }
        successes.sort_by_key(|(delay, index, _)| (*delay, *index));
        let mut out = successes
            .into_iter()
            .map(|(_, _, member)| member)
            .collect::<Vec<_>>();
        out.extend(pending_or_untested.into_iter().map(|(_, member)| member));
        out
    }

    fn displayed_member_index(&self) -> Option<usize> {
        let members = self.displayed_members();
        let current = self
            .selected_member_panel_group()?
            .members
            .get(self.member_index)?;
        members.iter().position(|member| member == current)
    }

    fn sync_selection_to_member_name(&mut self, name: &str) {
        if let Some(group) = self.selected_member_panel_group()
            && let Some(index) = group.members.iter().position(|member| member == name)
        {
            self.member_index = index;
        }
    }

    fn sync_selection_to_displayed_members(&mut self) {
        let displayed = self.displayed_members();
        if displayed.is_empty() {
            return;
        }

        let current = self
            .selected_member_panel_group()
            .and_then(|group| group.members.get(self.member_index))
            .cloned();
        if current
            .as_ref()
            .is_some_and(|member| displayed.iter().any(|item| item == member))
        {
            return;
        }

        if let Some(first) = displayed.first() {
            let next = first.clone();
            self.sync_selection_to_member_name(&next);
        }
    }

    fn status_line(&self) -> String {
        self.status.clone()
    }

    fn set_status_only(&mut self, status: impl Into<String>) {
        self.status = status.into();
        self.flash = None;
    }

    fn set_status_with_flash(&mut self, status: impl Into<String>) {
        self.status = status.into();
        self.flash = Some((self.status.clone(), Instant::now()));
    }

    fn set_switch_status(&mut self, group: &str, member: &str) {
        self.set_status_only(format!("Switched {} to {}", group, member));
    }

    fn flash_message(&mut self) -> Option<String> {
        let (message, since) = self.flash.as_ref()?;
        if since.elapsed() > Duration::from_secs(2) {
            self.flash = None;
            return None;
        }
        Some(message.clone())
    }

    fn handle_key(&mut self, code: KeyCode) -> Result<bool> {
        if self.filter_input.is_some() {
            return self.handle_filter_input_key(code);
        }
        if self.bypass_input.is_some() {
            return self.handle_bypass_input_key(code);
        }
        if self.latency_chart.is_some() {
            match code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('i') => {
                    self.latency_chart = None;
                    self.set_status_only("Latency chart closed");
                }
                KeyCode::Char('z') => self.zoom_latency_chart_in(),
                KeyCode::Char('Z') => self.zoom_latency_chart_out(),
                KeyCode::Char('q') => return Ok(false),
                _ => {}
            }
            return Ok(true);
        }

        match code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(false),
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => {
                self.focus = match self.focus {
                    Focus::Groups => Focus::Members,
                    Focus::Members => Focus::Groups,
                };
            }
            KeyCode::Left | KeyCode::Char('h') => {
                self.focus = match self.focus {
                    Focus::Groups => Focus::Members,
                    Focus::Members => Focus::Groups,
                };
            }
            KeyCode::Down | KeyCode::Char('j') => self.move_next(),
            KeyCode::Up | KeyCode::Char('k') => self.move_previous(),
            KeyCode::Char('g') => self.move_first(),
            KeyCode::Char('G') => self.move_last(),
            KeyCode::Char('r') => self.refresh()?,
            KeyCode::Char('b') => self.start_group_benchmark()?,
            KeyCode::Char('t') => self.start_member_benchmark()?,
            KeyCode::Char('s') => self.toggle_latency_sort_mode(),
            KeyCode::Char('a') => self.toggle_auto_select()?,
            KeyCode::Char('d') => self.switch_to_direct()?,
            KeyCode::Char('B') => self.open_bypass_modal(),
            KeyCode::Char('i') => self.open_latency_chart()?,
            KeyCode::Char('v') => self.run_verify(false)?,
            KeyCode::Char('V') => self.run_verify(true)?,
            KeyCode::Char('/') => self.open_benchmark_filter_modal(),
            KeyCode::Char(' ') => self.activate_selection()?,
            KeyCode::Enter => {}
            _ => {}
        }
        Ok(true)
    }

    fn selected_member_name(&self) -> Option<String> {
        self.selected_group()?
            .members
            .get(self.member_index)
            .cloned()
    }

    fn open_latency_chart(&mut self) -> Result<()> {
        let Some(group_name) = self.selected_group().map(|group| group.name.clone()) else {
            self.set_status_only("No selector group available for latency history");
            return Ok(());
        };
        let Some(node) = self.selected_member_name() else {
            self.set_status_only("No node selected for latency history");
            return Ok(());
        };
        let Some(store) = &self.benchmark_store else {
            self.set_status_only("SQLite benchmark history is unavailable");
            return Ok(());
        };
        let samples = store.node_latency_history(&group_name, &node, 200)?;
        if samples.iter().all(|sample| sample.delay_ms.is_none()) {
            self.set_status_only(format!("No latency history for {}", node));
            return Ok(());
        }
        let count = samples.len();
        self.latency_chart = Some(LatencyChartState {
            selector: group_name,
            node: node.clone(),
            samples,
            window: LATENCY_CHART_DEFAULT_WINDOW,
            last_refresh: Instant::now(),
        });
        self.set_status_only(format!("Showing {} latency samples for {}", count, node));
        Ok(())
    }

    fn zoom_latency_chart_in(&mut self) {
        let Some(chart) = self.latency_chart.as_mut() else {
            return;
        };
        chart.window = latency_chart_zoom_in(chart.window);
        let label = latency_chart_window_label(chart.window);
        self.set_status_only(format!("Latency chart window: {label}"));
    }

    fn zoom_latency_chart_out(&mut self) {
        let Some(chart) = self.latency_chart.as_mut() else {
            return;
        };
        chart.window = latency_chart_zoom_out(chart.window);
        let label = latency_chart_window_label(chart.window);
        self.set_status_only(format!("Latency chart window: {label}"));
    }

    fn maybe_refresh_latency_chart(&mut self) -> Result<()> {
        let Some(chart) = self.latency_chart.as_mut() else {
            return Ok(());
        };
        if chart.last_refresh.elapsed() < LATENCY_CHART_REFRESH_INTERVAL {
            return Ok(());
        }
        let Some(store) = &self.benchmark_store else {
            return Ok(());
        };
        chart.samples = store.node_latency_history(&chart.selector, &chart.node, 200)?;
        chart.last_refresh = Instant::now();
        Ok(())
    }

    fn move_next(&mut self) {
        match self.focus {
            Focus::Groups => {
                if self.implicit_root_mode() {
                    if self.provider_index + 1 < self.displayed_group_names().len() {
                        self.provider_index += 1;
                        self.sync_member_selection_to_current();
                    }
                } else if self.group_index + 1 < self.groups.len() {
                    self.group_index += 1;
                    self.sync_member_selection_to_current();
                }
            }
            Focus::Members => {
                let members = self.displayed_members();
                if members.is_empty() {
                    return;
                }
                let current_index = self.displayed_member_index().unwrap_or(0);
                if current_index + 1 < members.len() {
                    self.sync_selection_to_member_name(&members[current_index + 1]);
                }
            }
        }
    }

    fn move_previous(&mut self) {
        match self.focus {
            Focus::Groups => {
                if self.implicit_root_mode() {
                    if self.provider_index > 0 {
                        self.provider_index -= 1;
                        self.sync_member_selection_to_current();
                    }
                } else if self.group_index > 0 {
                    self.group_index -= 1;
                    self.sync_member_selection_to_current();
                }
            }
            Focus::Members => {
                let members = self.displayed_members();
                if members.is_empty() {
                    return;
                }
                let current_index = self.displayed_member_index().unwrap_or(0);
                if current_index > 0 {
                    self.sync_selection_to_member_name(&members[current_index - 1]);
                }
            }
        }
    }

    fn move_first(&mut self) {
        match self.focus {
            Focus::Groups => {
                if self.implicit_root_mode() {
                    self.provider_index = 0;
                } else {
                    self.group_index = 0;
                }
                self.sync_member_selection_to_current();
            }
            Focus::Members => {
                if let Some(first) = self.displayed_members().first().cloned() {
                    self.sync_selection_to_member_name(&first);
                }
            }
        }
    }

    fn move_last(&mut self) {
        match self.focus {
            Focus::Groups => {
                if self.implicit_root_mode() {
                    let groups = self.displayed_group_names();
                    if !groups.is_empty() {
                        self.provider_index = groups.len() - 1;
                        self.sync_member_selection_to_current();
                    }
                } else if !self.groups.is_empty() {
                    self.group_index = self.groups.len() - 1;
                    self.sync_member_selection_to_current();
                }
            }
            Focus::Members => {
                if let Some(last) = self.displayed_members().last().cloned() {
                    self.sync_selection_to_member_name(&last);
                }
            }
        }
    }

    fn activate_selection(&mut self) -> Result<()> {
        if self.focus == Focus::Groups {
            if self.implicit_root_mode() {
                self.activate_root_choice()?;
            } else {
                self.focus = Focus::Members;
            }
            return Ok(());
        }

        let Some(group) = self.selected_member_panel_group() else {
            bail!("no selector group available");
        };
        let group_name = group.name.clone();
        if self.implicit_root_mode() && !self.selected_member_panel_is_manual_selector() {
            self.activate_root_choice()?;
            return Ok(());
        }
        let Some(member) = group.members.get(self.member_index).cloned() else {
            bail!("no proxy available in selected group");
        };
        let parent_switch = if self.implicit_root_mode() {
            self.selected_root_choice_name().and_then(|choice| {
                self.implicit_root_group()
                    .map(|root| (root.name.clone(), choice))
            })
        } else {
            None
        };
        self.client
            .switch_proxy(&group_name, &member)
            .with_context(|| format!("failed to switch {} to {}", group_name, member))?;
        if let Some((parent, provider)) = parent_switch {
            self.client
                .switch_proxy(&parent, &provider)
                .with_context(|| format!("failed to switch {} to {}", parent, provider))?;
        }
        if REFRESH_DEBOUNCE > Duration::ZERO {
            std::thread::sleep(REFRESH_DEBOUNCE);
        }
        self.refresh()?;
        self.save_runtime_state()?;
        self.set_switch_status(&group_name, &member);
        Ok(())
    }

    fn switch_to_direct(&mut self) -> Result<()> {
        let Some(group) = self.implicit_root_group().or_else(|| self.selected_group()) else {
            bail!("no selector group available");
        };
        let group_name = group.name.clone();
        match direct_switch_action(&group.members) {
            DirectSwitchAction::SelectorMember(member) => {
                self.client
                    .switch_proxy(&group_name, &member)
                    .with_context(|| format!("failed to switch {} to {}", group_name, member))?;
                if REFRESH_DEBOUNCE > Duration::ZERO {
                    std::thread::sleep(REFRESH_DEBOUNCE);
                }
                self.refresh()?;
                self.save_runtime_state()?;
                self.set_status_only(format!(
                    "Switched {} to {} (new connections go direct)",
                    group_name, member
                ));
            }
            DirectSwitchAction::ClashModeDirect => {
                self.client
                    .set_mode(DIRECT_CLASH_MODE)
                    .context("failed to switch Clash mode to direct")?;
                self.set_status_only(format!(
                    "Direct outbound is not in {}; switched Clash mode to {}",
                    group_name, DIRECT_CLASH_MODE
                ));
            }
        }
        Ok(())
    }

    fn activate_root_choice(&mut self) -> Result<()> {
        let Some(root) = self.implicit_root_group() else {
            bail!("no implicit root selector available");
        };
        let root_name = root.name.clone();
        let Some(choice) = self.selected_root_choice_name() else {
            bail!("no selectable choice available");
        };
        self.client
            .switch_proxy(&root_name, &choice)
            .with_context(|| format!("failed to switch {} to {}", root_name, choice))?;
        if REFRESH_DEBOUNCE > Duration::ZERO {
            std::thread::sleep(REFRESH_DEBOUNCE);
        }
        self.refresh()?;
        self.save_runtime_state()?;
        self.set_switch_status(&root_name, &choice);
        Ok(())
    }

    fn refresh(&mut self) -> Result<()> {
        let previous_group_name = self.selected_group().map(|group| group.name.clone());
        let previous_choice_name = self.selected_root_choice_name();
        let groups = self.client.fetch_selector_groups()?;
        if groups.is_empty() {
            bail!("no selector groups returned by controller");
        }
        self.groups = groups;
        if self.implicit_root_mode() {
            let choices = self.displayed_group_names();
            self.provider_index = previous_choice_name
                .as_ref()
                .and_then(|name| choices.iter().position(|choice| choice == name))
                .or_else(|| {
                    self.implicit_root_group()
                        .and_then(|root| root.current.as_deref())
                        .and_then(|current| choices.iter().position(|choice| choice == current))
                })
                .unwrap_or(0);
        } else {
            self.group_index = previous_group_name
                .and_then(|name| self.groups.iter().position(|group| group.name == name))
                .unwrap_or(0);
            self.provider_index = 0;
        }
        self.sync_member_selection_to_current();
        self.status = format!("Loaded {} selector groups", self.groups.len());
        Ok(())
    }

    fn sync_member_selection_to_current(&mut self) {
        let next_index =
            self.selected_member_panel_group()
                .and_then(|group| {
                    group.current.as_deref().and_then(|current| {
                        group.members.iter().position(|member| member == current)
                    })
                })
                .unwrap_or(0);
        self.member_index = next_index;
        self.sync_selection_to_displayed_members();
    }

    fn start_group_benchmark(&mut self) -> Result<()> {
        let Some(group) = self.selected_member_panel_group().cloned() else {
            bail!("no selector group available");
        };
        if self
            .benchmark_jobs
            .iter()
            .any(|job| job.group == group.name)
        {
            self.set_status_only(format!("Benchmark already running for {}", group.name));
            return Ok(());
        }
        let candidate_names = self.benchmark_candidates_for_group(&group);
        let request = BenchmarkRequest {
            selector: group.name.clone(),
            pattern: self.benchmark_filter.clone(),
            url: self.benchmark_url.clone(),
            timeout_ms: self.benchmark_timeout_ms,
            request_timeout: self.benchmark_request_timeout,
            max_concurrency: self.benchmark_max_concurrency,
            nodes: Some(candidate_names.clone()),
        };
        if candidate_names.is_empty() {
            self.set_status_only(format!(
                "No nodes in {} matched filter '{}'",
                group.name, self.benchmark_filter
            ));
            return Ok(());
        }
        self.prepare_group_benchmark(&group.name, candidate_names.clone());
        self.spawn_benchmark_job(
            group.name.clone(),
            candidate_names,
            request,
            BenchmarkJobKind::Group,
        );
        self.set_status_only(format!(
            "Benchmarking {} with filter '{}' in background (max {} concurrent)...",
            group.name, self.benchmark_filter, self.benchmark_max_concurrency
        ));
        Ok(())
    }

    fn start_member_benchmark(&mut self) -> Result<()> {
        let Some(group) = self.selected_member_panel_group().cloned() else {
            bail!("no selector group available");
        };
        let Some(member) = group.members.get(self.member_index).cloned() else {
            bail!("no proxy available in selected group");
        };
        if let Some((last_group, last_member, last_started)) = &self.last_single_node_benchmark
            && last_group == &group.name
            && last_member == &member
            && last_started.elapsed() < SINGLE_NODE_RETEST_DEBOUNCE
        {
            self.set_status_only(format!(
                "Ignoring repeated retest for {} / {} (debounced)",
                group.name, member
            ));
            return Ok(());
        }
        if self
            .benchmark_jobs
            .iter()
            .any(|job| job.group == group.name && job.nodes.iter().any(|node| node == &member))
        {
            self.set_status_only(format!(
                "Benchmark already running for {} / {}",
                group.name, member
            ));
            return Ok(());
        }
        let request = BenchmarkRequest {
            selector: group.name.clone(),
            pattern: self.benchmark_filter.clone(),
            url: self.benchmark_url.clone(),
            timeout_ms: self.benchmark_timeout_ms,
            request_timeout: self.benchmark_request_timeout,
            max_concurrency: 1,
            nodes: Some(vec![member.clone()]),
        };
        self.prepare_node_benchmark(&group.name, &member);
        self.spawn_benchmark_job(
            group.name.clone(),
            vec![member.clone()],
            request,
            BenchmarkJobKind::SingleNode {
                node: member.clone(),
            },
        );
        self.last_single_node_benchmark =
            Some((group.name.clone(), member.clone(), Instant::now()));
        self.set_status_only(format!(
            "Benchmarking {} / {} in background...",
            group.name, member
        ));
        Ok(())
    }

    fn prepare_group_benchmark(&mut self, group: &str, candidates: Vec<String>) {
        let summary = self
            .benchmarks
            .entry(group.to_string())
            .or_insert_with(|| BenchmarkSummary::empty(group.to_string()));
        summary.selector = group.to_string();
        summary.pattern = self.benchmark_filter.clone();
        summary.url = self.benchmark_url.clone();
        summary.timeout_ms = self.benchmark_timeout_ms;
        summary.max_concurrency = self.benchmark_max_concurrency.max(1);
        for name in candidates {
            summary.upsert_pending(name);
        }
    }

    fn prepare_node_benchmark(&mut self, group: &str, node: &str) {
        let summary = self
            .benchmarks
            .entry(group.to_string())
            .or_insert_with(|| BenchmarkSummary::empty(group.to_string()));
        summary.selector = group.to_string();
        summary.pattern = self.benchmark_filter.clone();
        summary.url = self.benchmark_url.clone();
        summary.timeout_ms = self.benchmark_timeout_ms;
        summary.max_concurrency = 1;
        summary.upsert_pending(node.to_string());
    }

    fn spawn_benchmark_job(
        &mut self,
        group: String,
        nodes: Vec<String>,
        request: BenchmarkRequest,
        kind: BenchmarkJobKind,
    ) {
        let (tx, rx) = mpsc::channel();
        let worker = spawn_benchmark_worker(
            self.client.base_url.clone(),
            self.client.client.clone(),
            request,
            tx,
        );
        self.benchmark_jobs.push(BenchmarkJob {
            group,
            nodes,
            kind,
            receiver: rx,
            worker,
        });
    }

    fn toggle_latency_sort_mode(&mut self) {
        self.latency_sort_mode = !self.latency_sort_mode;
        let status = if self.latency_sort_mode {
            "View mode: LATENCY SORT (hide failed-tested nodes, sort successful nodes by delay)"
                .to_string()
        } else {
            "View mode: FILTER VIEW (original selector order with current filter)".to_string()
        };
        self.set_status_only(status);
    }

    fn toggle_auto_select(&mut self) -> Result<()> {
        if self.auto_select_enabled {
            self.auto_select_enabled = false;
            self.set_status_only("Auto-pick disabled");
            self.save_runtime_state()?;
            return Ok(());
        }

        if self.benchmark_filter.is_empty() {
            self.set_status_only("Set a filter before enabling auto-pick");
            return Ok(());
        }

        let Some(group_name) = self.selected_group().map(|group| group.name.clone()) else {
            self.set_status_only("No selector group available for auto-pick");
            return Ok(());
        };
        self.auto_select_enabled = true;
        self.last_auto_select_benchmark = None;
        self.set_status_only(format!(
            "Auto-pick enabled for {} with filter '{}' ({}ms threshold, every {}s)",
            group_name,
            self.benchmark_filter,
            self.auto_select_threshold_ms,
            self.auto_select_interval.as_secs()
        ));
        self.save_runtime_state()?;
        Ok(())
    }

    fn auto_select_benchmark_due(&self, now: Instant) -> bool {
        if !self.auto_select_enabled || self.benchmark_filter.is_empty() {
            return false;
        }
        self.last_auto_select_benchmark
            .is_none_or(|last| now.duration_since(last) >= self.auto_select_interval)
    }

    fn maybe_start_auto_select_benchmark(&mut self) -> Result<()> {
        let now = Instant::now();
        if !self.auto_select_benchmark_due(now) {
            return Ok(());
        }
        let Some(group) = self.selected_group().cloned() else {
            return Ok(());
        };
        if self
            .benchmark_jobs
            .iter()
            .any(|job| job.group == group.name)
        {
            return Ok(());
        }

        let candidate_names = self.benchmark_candidates_for_group(&group);
        let request = BenchmarkRequest {
            selector: group.name.clone(),
            pattern: self.benchmark_filter.clone(),
            url: self.benchmark_url.clone(),
            timeout_ms: self.benchmark_timeout_ms,
            request_timeout: self.benchmark_request_timeout,
            max_concurrency: self.benchmark_max_concurrency,
            nodes: Some(candidate_names.clone()),
        };
        self.last_auto_select_benchmark = Some(now);
        if candidate_names.is_empty() {
            self.set_status_only(format!(
                "Auto-pick found no nodes in {} matching '{}'",
                group.name, self.benchmark_filter
            ));
            return Ok(());
        }

        self.prepare_group_benchmark(&group.name, candidate_names.clone());
        self.spawn_benchmark_job(
            group.name.clone(),
            candidate_names,
            request,
            BenchmarkJobKind::AutoSelect,
        );
        self.set_status_only(format!(
            "Auto-pick benchmarking {} with filter '{}'...",
            group.name, self.benchmark_filter
        ));
        Ok(())
    }

    fn auto_select_target(&self, group: &ProxyGroup, summary: &BenchmarkSummary) -> Option<String> {
        let best = summary.best_success()?;
        let current = group.current.as_deref();
        let current_result = current.and_then(|name| summary.find_result(name));
        let current_is_acceptable = current_result
            .and_then(|result| result.delay)
            .is_some_and(|delay| delay <= self.auto_select_threshold_ms);
        if current_is_acceptable {
            return None;
        }
        if current == Some(best.name.as_str()) {
            return None;
        }
        Some(best.name.clone())
    }

    fn finish_auto_select_benchmark(
        &mut self,
        group_name: &str,
        summary: &BenchmarkSummary,
    ) -> Result<()> {
        let Some(group) = self
            .groups
            .iter()
            .find(|group| group.name == group_name)
            .cloned()
        else {
            self.set_status_only(format!(
                "Auto-pick finished for missing group {}",
                group_name
            ));
            return Ok(());
        };

        let Some(target) = self.auto_select_target(&group, summary) else {
            let current = group.current.as_deref().unwrap_or("unset");
            self.set_status_only(format!(
                "Auto-pick kept {} on {} (threshold {}ms)",
                group_name, current, self.auto_select_threshold_ms
            ));
            return Ok(());
        };

        self.client
            .switch_proxy(group_name, &target)
            .with_context(|| format!("auto-pick failed to switch {} to {}", group_name, target))?;
        if REFRESH_DEBOUNCE > Duration::ZERO {
            std::thread::sleep(REFRESH_DEBOUNCE);
        }
        self.refresh()?;
        self.save_runtime_state()?;
        self.set_status_only(format!("Auto-pick switched {} to {}", group_name, target));
        Ok(())
    }

    fn record_benchmark_result(
        &self,
        group: &str,
        filter: &str,
        job_kind: &BenchmarkJobKind,
        result: &crate::controller::BenchmarkResult,
    ) -> Result<()> {
        let Some(store) = &self.benchmark_store else {
            return Ok(());
        };
        store.record_benchmark(&BenchmarkRecord {
            selector: group,
            node: &result.name,
            filter,
            delay_ms: result.delay,
            completed: result.completed,
            job_kind: benchmark_job_kind_label(job_kind),
        })
    }

    fn poll_benchmark_updates(&mut self) -> Result<()> {
        let mut finished_indexes = Vec::new();

        for index in 0..self.benchmark_jobs.len() {
            let mut finished = false;
            loop {
                match self.benchmark_jobs[index].receiver.try_recv() {
                    Ok(BenchmarkEvent::Progress(result)) => {
                        let group = self.benchmark_jobs[index].group.clone();
                        let kind = self.benchmark_jobs[index].kind.clone();
                        let mut filter = self.benchmark_filter.clone();
                        if let Some(summary) =
                            self.benchmarks.get_mut(&self.benchmark_jobs[index].group)
                        {
                            filter = summary.pattern.clone();
                            summary.update_result(result.clone());
                            self.status = format!(
                                "Benchmarking {}... best so far: {}",
                                self.benchmark_jobs[index].group,
                                summary.best_label()
                            );
                        }
                        self.record_benchmark_result(&group, &filter, &kind, &result)?;
                    }
                    Ok(BenchmarkEvent::Finished) => {
                        finished = true;
                        let group = self.benchmark_jobs[index].group.clone();
                        let kind = self.benchmark_jobs[index].kind.clone();
                        if let Some(summary) = self.benchmarks.get(&group) {
                            match kind {
                                BenchmarkJobKind::Group => {
                                    if let Some(best) = summary.best_success() {
                                        self.set_status_only(format!(
                                            "Benchmarked {}: best is {} ({})",
                                            group,
                                            best.name,
                                            best.display_delay()
                                        ));
                                    } else {
                                        self.set_status_only(format!(
                                            "Benchmarked {} but no healthy node matched",
                                            group
                                        ));
                                    }
                                }
                                BenchmarkJobKind::AutoSelect => {
                                    let summary = summary.clone();
                                    self.finish_auto_select_benchmark(&group, &summary)?;
                                }
                                BenchmarkJobKind::SingleNode { node } => {
                                    let result = summary.find_result(&node);
                                    let status = match result {
                                        Some(result) if result.delay.is_some() => format!(
                                            "Benchmarked {} / {}: {}",
                                            group,
                                            node,
                                            result.display_delay()
                                        ),
                                        Some(_) => {
                                            format!("Benchmarked {} / {}: failed", group, node)
                                        }
                                        None => {
                                            format!("Benchmark finished for {} / {}", group, node)
                                        }
                                    };
                                    self.set_status_only(status);
                                }
                            }
                        }
                        break;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        finished = true;
                        let group = self.benchmark_jobs[index].group.clone();
                        self.set_status_only(format!(
                            "Benchmark worker for {} disconnected",
                            group
                        ));
                        break;
                    }
                }
            }
            if finished {
                finished_indexes.push(index);
            }
        }

        for index in finished_indexes.into_iter().rev() {
            let job = self.benchmark_jobs.swap_remove(index);
            let _ = job.worker.join();
        }

        Ok(())
    }

    fn run_verify(&mut self, include_discord: bool) -> Result<()> {
        self.status = if include_discord {
            "Running verification (google/github/discord)...".to_string()
        } else {
            "Running verification (google/github)...".to_string()
        };
        let report = run_verification(include_discord);
        self.set_status_with_flash(report.summary_line());
        Ok(())
    }

    fn open_benchmark_filter_modal(&mut self) {
        self.filter_input = Some(self.benchmark_filter.clone());
        self.flash = None;
    }

    fn open_bypass_modal(&mut self) {
        self.bypass_input = Some(self.bypass_entries.join(","));
        self.flash = None;
    }

    fn handle_filter_input_key(&mut self, code: KeyCode) -> Result<bool> {
        let Some(buffer) = self.filter_input.as_mut() else {
            return Ok(true);
        };

        match code {
            KeyCode::Esc | KeyCode::Char(' ') => {
                self.filter_input = None;
                self.set_status_only("Benchmark filter edit canceled");
            }
            KeyCode::Enter => {
                let value = buffer.trim().to_string();
                self.filter_input = None;
                self.apply_benchmark_filter(value)?;
            }
            KeyCode::Backspace => {
                buffer.pop();
            }
            KeyCode::Char(ch) => {
                buffer.push(ch);
            }
            _ => {}
        }
        Ok(true)
    }

    fn handle_bypass_input_key(&mut self, code: KeyCode) -> Result<bool> {
        let Some(buffer) = self.bypass_input.as_mut() else {
            return Ok(true);
        };

        match code {
            KeyCode::Esc | KeyCode::Char(' ') => {
                self.bypass_input = None;
                self.set_status_only("Bypass edit canceled");
            }
            KeyCode::Enter => {
                let value = buffer.clone();
                self.bypass_input = None;
                self.apply_bypass_entries(parse_bypass_entries(&value))?;
            }
            KeyCode::Backspace => {
                buffer.pop();
            }
            KeyCode::Char(ch) => {
                buffer.push(ch);
            }
            _ => {}
        }
        Ok(true)
    }

    fn apply_benchmark_filter(&mut self, value: String) -> Result<()> {
        self.benchmark_filter = value;
        self.sync_selection_to_displayed_members();
        self.last_auto_select_benchmark = None;
        if self.benchmark_filter.is_empty() {
            self.auto_select_enabled = false;
            self.set_status_only("Benchmark filter cleared");
        } else {
            self.set_status_only(format!(
                "Benchmark filter set to '{}'",
                self.benchmark_filter
            ));
        }
        self.save_runtime_state()?;
        Ok(())
    }

    fn apply_bypass_entries(&mut self, entries: Vec<String>) -> Result<()> {
        self.bypass_entries = entries;
        self.save_runtime_state()?;
        self.save_bypass_rule_set()?;
        if self.bypass_entries.is_empty() {
            self.set_status_only("Bypass list cleared");
        } else {
            self.set_status_only(format!(
                "Bypass list saved ({} entries)",
                self.bypass_entries.len()
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        App, Focus, LATENCY_CHART_DEFAULT_WINDOW, LATENCY_CHART_REFRESH_INTERVAL,
        LatencyChartState, LatencyChartTimeUnit, direct_member_name, latency_chart_segments,
        latency_chart_threshold_line, latency_chart_time_unit, latency_chart_windowed_samples,
        latency_chart_y_bounds, latency_chart_zoom_in, latency_chart_zoom_out, truncate_for_width,
    };
    use crate::controller::{
        ApiClient, BenchmarkEvent, BenchmarkJob, BenchmarkJobKind, BenchmarkRequest,
        BenchmarkResult, BenchmarkSummary, ProxyGroup,
    };
    use crate::defaults::DEFAULT_BENCHMARK_MAX_CONCURRENCY;
    use crate::tui_state::{TuiRuntimeState, TuiStateStore};
    use crossterm::event::KeyCode;
    use reqwest::Client as AsyncClient;
    use std::collections::BTreeMap;
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::runtime::Builder as TokioRuntimeBuilder;

    use crate::storage::{BenchmarkRecord, BenchmarkStore, NodeLatencySample};

    fn test_db_path() -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        std::env::temp_dir().join(format!("sing-box-tui-tui-test-{nanos}.sqlite3"))
    }

    fn test_state_path() -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        std::env::temp_dir().join(format!("sing-box-tui-state-test-{nanos}.json"))
    }

    fn test_bypass_rule_set_path() -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        std::env::temp_dir().join(format!("sing-box-tui-bypass-test-{nanos}.json"))
    }

    fn test_app() -> App {
        let runtime = TokioRuntimeBuilder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let client = AsyncClient::builder()
            .no_proxy()
            .build()
            .expect("test HTTP client");

        App {
            client: ApiClient {
                base_url: "http://127.0.0.1:9090".to_string(),
                runtime,
                client,
            },
            groups: vec![ProxyGroup {
                name: "select".to_string(),
                kind: "Selector".to_string(),
                current: Some("node-a".to_string()),
                members: vec!["node-a".to_string()],
            }],
            group_index: 0,
            provider_index: 0,
            member_index: 0,
            focus: Focus::Members,
            status: String::new(),
            flash: None,
            benchmark_filter: "美国".to_string(),
            benchmark_url: "https://www.gstatic.com/generate_204".to_string(),
            benchmark_timeout_ms: 5000,
            benchmark_request_timeout: 12.0,
            benchmark_max_concurrency: DEFAULT_BENCHMARK_MAX_CONCURRENCY,
            benchmarks: BTreeMap::new(),
            benchmark_jobs: Vec::new(),
            latency_sort_mode: false,
            last_single_node_benchmark: None,
            filter_input: None,
            bypass_input: None,
            bypass_entries: Vec::new(),
            auto_select_enabled: false,
            auto_select_threshold_ms: 600,
            auto_select_interval: Duration::from_secs(30),
            last_auto_select_benchmark: None,
            benchmark_store: None,
            state_store: None,
            bypass_rule_set_store: None,
            latency_chart: None,
        }
    }

    fn provider_app() -> App {
        let mut app = test_app();
        app.groups = vec![
            ProxyGroup {
                name: "手动选择".to_string(),
                kind: "Selector".to_string(),
                current: Some("宝贝云".to_string()),
                members: vec![
                    "自动选择".to_string(),
                    "AirTCP".to_string(),
                    "宝贝云".to_string(),
                ],
            },
            ProxyGroup {
                name: "自动选择".to_string(),
                kind: "URLTest".to_string(),
                current: Some("auto-node".to_string()),
                members: vec![
                    "auto-node".to_string(),
                    "air-1".to_string(),
                    "bby-1".to_string(),
                ],
            },
            ProxyGroup {
                name: "AirTCP".to_string(),
                kind: "Selector".to_string(),
                current: Some("air-1".to_string()),
                members: vec!["air-1".to_string(), "air-2".to_string()],
            },
            ProxyGroup {
                name: "宝贝云".to_string(),
                kind: "Selector".to_string(),
                current: Some("bby-2".to_string()),
                members: vec!["bby-1".to_string(), "bby-2".to_string()],
            },
        ];
        app.group_index = 0;
        app.provider_index = 2;
        app.member_index = 1;
        app.benchmark_filter.clear();
        app
    }

    #[test]
    fn truncates_wide_strings_without_panicking() {
        let truncated = truncate_for_width("手动选择-自动选择-节点A", 8);
        assert!(truncated.ends_with('…'));
        assert!(!truncated.is_empty());
    }

    #[test]
    fn direct_member_name_accepts_default_and_legacy_tags() {
        assert_eq!(
            direct_member_name(&["node-a".to_string(), "国内直连".to_string()]),
            Some("国内直连".to_string())
        );
        assert_eq!(
            direct_member_name(&["node-a".to_string(), "direct".to_string()]),
            Some("direct".to_string())
        );
        assert_eq!(direct_member_name(&["node-a".to_string()]), None);
    }

    #[test]
    fn direct_switch_action_falls_back_to_clash_direct_mode() {
        assert_eq!(
            super::direct_switch_action(&["node-a".to_string()]),
            super::DirectSwitchAction::ClashModeDirect
        );
        assert_eq!(
            super::direct_switch_action(&["node-a".to_string(), "direct".to_string()]),
            super::DirectSwitchAction::SelectorMember("direct".to_string())
        );
    }

    #[test]
    fn tui_state_store_round_trips_filter_auto_pick_and_current_nodes() {
        let path = test_state_path();
        let store = TuiStateStore::new(&path);
        let mut state = TuiRuntimeState {
            benchmark_filter: "美国,香港".to_string(),
            auto_pick_enabled: true,
            bypass_entries: vec!["example.com".to_string(), "10.0.0.0/8".to_string()],
            ..TuiRuntimeState::default()
        };
        state
            .current_selected_nodes
            .insert("select".to_string(), "node-a".to_string());

        store.save(&state).expect("save state");
        let loaded = store.load().expect("load state");

        assert_eq!(loaded, state);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn bypass_rule_set_store_writes_domains_and_ip_cidrs() {
        let path = test_bypass_rule_set_path();
        let store = crate::tui_state::BypassRuleSetStore::new(&path);

        store
            .save(&[
                "example.com".to_string(),
                "*.github.com".to_string(),
                "1.1.1.1".to_string(),
                "10.0.0.0/8".to_string(),
            ])
            .expect("save rule set");

        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read rule set"))
                .expect("parse rule set");
        assert_eq!(value["version"], 1);
        assert_eq!(
            value["rules"][0]["domain_suffix"],
            serde_json::json!(["example.com", "github.com"])
        );
        assert_eq!(
            value["rules"][1]["ip_cidr"],
            serde_json::json!(["1.1.1.1", "10.0.0.0/8"])
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn app_applies_persisted_filter_auto_pick_and_selected_node() {
        let mut app = test_app();
        app.groups[0].members = vec![
            "node-a".to_string(),
            "node-b".to_string(),
            "node-c".to_string(),
        ];
        app.member_index = 0;
        let mut state = TuiRuntimeState {
            benchmark_filter: "node-b,node-c".to_string(),
            auto_pick_enabled: true,
            ..TuiRuntimeState::default()
        };
        state
            .current_selected_nodes
            .insert("select".to_string(), "node-c".to_string());

        app.apply_runtime_state(state);

        assert_eq!(app.benchmark_filter, "node-b,node-c");
        assert!(app.auto_select_enabled);
        assert_eq!(app.member_index, 2);
    }

    #[test]
    fn filter_and_auto_pick_changes_are_saved_to_tui_state() {
        let path = test_state_path();
        let mut app = test_app();
        app.state_store = Some(TuiStateStore::new(&path));

        app.apply_benchmark_filter("香港".to_string())
            .expect("apply filter");
        app.handle_key(KeyCode::Char('a'))
            .expect("toggle auto-pick");

        let state = TuiStateStore::new(&path).load().expect("load state");
        assert_eq!(state.benchmark_filter, "香港");
        assert!(state.auto_pick_enabled);
        assert_eq!(
            state.current_selected_nodes.get("select"),
            Some(&"node-a".to_string())
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn bypass_modal_updates_state_and_rule_set_file() {
        let path = test_state_path();
        let rule_set_path = test_bypass_rule_set_path();
        let mut app = test_app();
        app.state_store = Some(TuiStateStore::new(&path));
        app.bypass_rule_set_store = Some(crate::tui_state::BypassRuleSetStore::new(&rule_set_path));

        app.handle_key(KeyCode::Char('B'))
            .expect("open bypass modal");
        for ch in "example.com,10.0.0.0/8".chars() {
            app.handle_key(KeyCode::Char(ch)).expect("type bypass");
        }
        app.handle_key(KeyCode::Enter).expect("save bypass");

        let state = TuiStateStore::new(&path).load().expect("load state");
        assert_eq!(
            state.bypass_entries,
            vec!["example.com".to_string(), "10.0.0.0/8".to_string()]
        );
        let rule_set: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&rule_set_path).expect("read rule set"))
                .expect("parse rule set");
        assert_eq!(
            rule_set["rules"][0]["domain_suffix"],
            serde_json::json!(["example.com"])
        );
        assert_eq!(
            rule_set["rules"][1]["ip_cidr"],
            serde_json::json!(["10.0.0.0/8"])
        );

        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(rule_set_path);
    }

    #[test]
    fn status_only_updates_clear_flash() {
        let mut app = test_app();

        app.set_status_with_flash("flash me");
        assert!(app.flash.is_some());

        app.set_status_only("status only");

        assert_eq!(app.status, "status only");
        assert!(app.flash.is_none());
    }

    #[test]
    fn single_node_benchmark_finish_does_not_flash() {
        let mut app = test_app();
        app.benchmarks.insert(
            "select".to_string(),
            BenchmarkSummary {
                selector: "select".to_string(),
                current: Some("node-a".to_string()),
                pattern: "美国".to_string(),
                url: "https://www.gstatic.com/generate_204".to_string(),
                timeout_ms: 5000,
                max_concurrency: 1,
                results: vec![BenchmarkResult {
                    name: "node-a".to_string(),
                    delay: Some(42),
                    completed: true,
                }],
            },
        );

        let (tx, rx) = mpsc::channel();
        tx.send(BenchmarkEvent::Finished)
            .expect("send finish event");
        let worker = thread::spawn(|| {});
        app.benchmark_jobs.push(BenchmarkJob {
            group: "select".to_string(),
            nodes: vec!["node-a".to_string()],
            kind: BenchmarkJobKind::SingleNode {
                node: "node-a".to_string(),
            },
            receiver: rx,
            worker,
        });

        app.poll_benchmark_updates().expect("poll succeeds");

        assert_eq!(app.status, "Benchmarked select / node-a: 42ms");
        assert!(app.flash.is_none());
        assert!(app.benchmark_jobs.is_empty());
    }

    #[test]
    fn toggling_latency_sort_mode_does_not_flash() {
        let mut app = test_app();
        app.set_status_with_flash("existing flash");

        app.toggle_latency_sort_mode();

        assert!(app.latency_sort_mode);
        assert_eq!(
            app.status,
            "View mode: LATENCY SORT (hide failed-tested nodes, sort successful nodes by delay)"
        );
        assert!(app.flash.is_none());
    }

    #[test]
    fn group_benchmark_finish_does_not_flash() {
        let mut app = test_app();
        app.benchmarks.insert(
            "select".to_string(),
            BenchmarkSummary {
                selector: "select".to_string(),
                current: Some("node-a".to_string()),
                pattern: "美国".to_string(),
                url: "https://www.gstatic.com/generate_204".to_string(),
                timeout_ms: 5000,
                max_concurrency: 4,
                results: vec![
                    BenchmarkResult {
                        name: "node-a".to_string(),
                        delay: Some(42),
                        completed: true,
                    },
                    BenchmarkResult {
                        name: "node-b".to_string(),
                        delay: Some(80),
                        completed: true,
                    },
                ],
            },
        );

        let (tx, rx) = mpsc::channel();
        tx.send(BenchmarkEvent::Finished)
            .expect("send finish event");
        let worker = thread::spawn(|| {});
        app.benchmark_jobs.push(BenchmarkJob {
            group: "select".to_string(),
            nodes: vec!["node-a".to_string(), "node-b".to_string()],
            kind: BenchmarkJobKind::Group,
            receiver: rx,
            worker,
        });

        app.poll_benchmark_updates().expect("poll succeeds");

        assert_eq!(app.status, "Benchmarked select: best is node-a (42ms)");
        assert!(app.flash.is_none());
        assert!(app.benchmark_jobs.is_empty());
    }

    #[test]
    fn benchmark_progress_is_recorded_to_sqlite() {
        let path = test_db_path();
        let mut app = test_app();
        app.benchmark_store = Some(BenchmarkStore::open(&path).expect("open benchmark store"));

        let (tx, rx) = mpsc::channel();
        tx.send(BenchmarkEvent::Progress(BenchmarkResult {
            name: "美国-a".to_string(),
            delay: Some(88),
            completed: true,
        }))
        .expect("send progress event");
        let worker = thread::spawn(|| {});
        app.benchmark_jobs.push(BenchmarkJob {
            group: "select".to_string(),
            nodes: vec!["美国-a".to_string()],
            kind: BenchmarkJobKind::AutoSelect,
            receiver: rx,
            worker,
        });

        app.poll_benchmark_updates().expect("poll succeeds");

        let store = BenchmarkStore::open(&path).expect("reopen benchmark store");
        let rows = store.recent_benchmarks(10).expect("read rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].selector, "select");
        assert_eq!(rows[0].node, "美国-a");
        assert_eq!(rows[0].filter, "美国");
        assert_eq!(rows[0].delay_ms, Some(88));
        assert!(rows[0].completed);
        assert_eq!(rows[0].job_kind, "auto");

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn pressing_i_opens_latency_chart_for_selected_node() {
        let path = test_db_path();
        let mut app = test_app();
        app.groups[0].members = vec!["node-a".to_string(), "node-b".to_string()];
        app.member_index = 1;
        let store = BenchmarkStore::open(&path).expect("open benchmark store");
        store
            .record_benchmark(&BenchmarkRecord {
                selector: "select",
                node: "node-b",
                filter: "美国",
                delay_ms: Some(93),
                completed: true,
                job_kind: "single",
            })
            .expect("record benchmark");
        app.benchmark_store = Some(store);

        app.handle_key(KeyCode::Char('i')).expect("open chart");

        let chart = app.latency_chart.as_ref().expect("latency chart");
        assert_eq!(chart.selector, "select");
        assert_eq!(chart.node, "node-b");
        assert_eq!(chart.samples.len(), 1);
        assert_eq!(chart.samples[0].delay_ms, Some(93));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn latency_chart_segments_break_on_failed_samples() {
        let samples = vec![
            NodeLatencySample {
                recorded_at_ms: 1_000,
                delay_ms: Some(90),
            },
            NodeLatencySample {
                recorded_at_ms: 2_000,
                delay_ms: None,
            },
            NodeLatencySample {
                recorded_at_ms: 3_000,
                delay_ms: Some(120),
            },
        ];

        assert_eq!(
            latency_chart_segments(&samples),
            vec![vec![(1_000, 90)], vec![(3_000, 120)]]
        );
    }

    #[test]
    fn latency_chart_uses_minutes_for_short_windows_and_hours_for_long_windows() {
        assert_eq!(
            latency_chart_time_unit(Duration::from_secs(30 * 60)),
            LatencyChartTimeUnit::Minutes
        );
        assert_eq!(
            latency_chart_time_unit(Duration::from_secs(3 * 60 * 60)),
            LatencyChartTimeUnit::Hours
        );
    }

    #[test]
    fn latency_chart_zoom_adjusts_window() {
        assert_eq!(
            latency_chart_zoom_in(Duration::from_secs(60 * 60)),
            Duration::from_secs(30 * 60)
        );
        assert_eq!(
            latency_chart_zoom_out(Duration::from_secs(60 * 60)),
            Duration::from_secs(2 * 60 * 60)
        );
    }

    #[test]
    fn latency_chart_threshold_line_spans_the_visible_window() {
        assert_eq!(
            latency_chart_threshold_line(30.0, 600),
            vec![(0.0, 600.0), (30.0, 600.0)]
        );
    }

    #[test]
    fn latency_chart_y_bounds_include_threshold() {
        let low_latency_bounds = latency_chart_y_bounds(80.0, 120.0, 600);
        assert!(low_latency_bounds[0] <= 80.0);
        assert!(low_latency_bounds[1] > 600.0);

        let high_latency_bounds = latency_chart_y_bounds(700.0, 900.0, 600);
        assert!(high_latency_bounds[0] < 600.0);
        assert!(high_latency_bounds[1] >= 900.0);
    }

    #[test]
    fn latency_chart_window_keeps_recent_samples() {
        let samples = vec![
            NodeLatencySample {
                recorded_at_ms: 0,
                delay_ms: Some(90),
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

        let visible = latency_chart_windowed_samples(&samples, Duration::from_secs(30 * 60));

        assert_eq!(visible.len(), 2);
        assert_eq!(visible[0].recorded_at_ms, 45 * 60 * 1000);
        assert_eq!(visible[1].recorded_at_ms, 60 * 60 * 1000);
    }

    #[test]
    fn z_and_shift_z_zoom_latency_chart() {
        let mut app = test_app();
        app.latency_chart = Some(LatencyChartState {
            selector: "select".to_string(),
            node: "node-a".to_string(),
            samples: vec![NodeLatencySample {
                recorded_at_ms: 1_000,
                delay_ms: Some(90),
            }],
            window: LATENCY_CHART_DEFAULT_WINDOW,
            last_refresh: Instant::now(),
        });

        app.handle_key(KeyCode::Char('z')).expect("zoom in");
        assert_eq!(
            app.latency_chart.as_ref().expect("chart").window,
            Duration::from_secs(30 * 60)
        );

        app.handle_key(KeyCode::Char('Z')).expect("zoom out");
        assert_eq!(
            app.latency_chart.as_ref().expect("chart").window,
            LATENCY_CHART_DEFAULT_WINDOW
        );
    }

    #[test]
    fn latency_chart_refreshes_from_sqlite() {
        let path = test_db_path();
        let mut app = test_app();
        let store = BenchmarkStore::open(&path).expect("open benchmark store");
        store
            .record_benchmark(&BenchmarkRecord {
                selector: "select",
                node: "node-a",
                filter: "美国",
                delay_ms: Some(77),
                completed: true,
                job_kind: "auto",
            })
            .expect("record benchmark");
        app.benchmark_store = Some(store);
        app.latency_chart = Some(LatencyChartState {
            selector: "select".to_string(),
            node: "node-a".to_string(),
            samples: Vec::new(),
            window: LATENCY_CHART_DEFAULT_WINDOW,
            last_refresh: Instant::now() - LATENCY_CHART_REFRESH_INTERVAL,
        });

        app.maybe_refresh_latency_chart().expect("refresh chart");

        let chart = app.latency_chart.as_ref().expect("chart");
        assert_eq!(chart.samples.len(), 1);
        assert_eq!(chart.samples[0].delay_ms, Some(77));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn pressing_i_without_history_updates_status() {
        let path = test_db_path();
        let mut app = test_app();
        app.benchmark_store = Some(BenchmarkStore::open(&path).expect("open benchmark store"));

        app.handle_key(KeyCode::Char('i')).expect("open chart");

        assert!(app.latency_chart.is_none());
        assert_eq!(app.status, "No latency history for node-a");

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn slash_opens_filter_modal_with_current_value() {
        let mut app = test_app();
        app.benchmark_filter = "hk".to_string();

        app.handle_key(KeyCode::Char('/')).expect("open modal");

        assert_eq!(app.filter_input.as_deref(), Some("hk"));
    }

    #[test]
    fn filter_modal_submit_updates_filter() {
        let mut app = test_app();

        app.handle_key(KeyCode::Char('/')).expect("open modal");
        app.handle_key(KeyCode::Char('u')).expect("type");
        app.handle_key(KeyCode::Char('s')).expect("type");
        app.handle_key(KeyCode::Enter).expect("submit");

        assert_eq!(app.benchmark_filter, "美国us");
        assert_eq!(app.filter_input, None);
        assert_eq!(app.status, "Benchmark filter set to '美国us'");
        assert!(app.flash.is_none());
    }

    #[test]
    fn filter_modal_empty_submit_clears_filter() {
        let mut app = test_app();

        app.handle_key(KeyCode::Char('/')).expect("open modal");
        app.handle_key(KeyCode::Backspace).expect("backspace");
        app.handle_key(KeyCode::Backspace).expect("backspace");
        app.handle_key(KeyCode::Enter).expect("submit");

        assert!(app.benchmark_filter.is_empty());
        assert_eq!(app.filter_input, None);
        assert_eq!(app.status, "Benchmark filter cleared");
        assert!(app.flash.is_none());
    }

    #[test]
    fn filter_modal_escape_cancels_without_changing_filter() {
        let mut app = test_app();

        app.handle_key(KeyCode::Char('/')).expect("open modal");
        app.handle_key(KeyCode::Char('x')).expect("type");
        app.handle_key(KeyCode::Esc).expect("cancel");

        assert_eq!(app.benchmark_filter, "美国");
        assert_eq!(app.filter_input, None);
        assert_eq!(app.status, "Benchmark filter edit canceled");
    }

    #[test]
    fn filter_modal_space_cancels_without_changing_filter() {
        let mut app = test_app();

        app.handle_key(KeyCode::Char('/')).expect("open modal");
        app.handle_key(KeyCode::Char('x')).expect("type");
        app.handle_key(KeyCode::Char(' ')).expect("cancel");

        assert_eq!(app.benchmark_filter, "美国");
        assert_eq!(app.filter_input, None);
        assert_eq!(app.status, "Benchmark filter edit canceled");
    }

    #[test]
    fn switching_selection_updates_status_without_flash_popup() {
        let mut app = test_app();
        app.set_status_with_flash("old flash");
        app.set_switch_status("select", "node-b");

        assert_eq!(app.status, "Switched select to node-b");
        assert!(app.flash.is_none());
    }

    #[test]
    fn displayed_members_follow_active_filter() {
        let mut app = test_app();
        app.groups[0].members = vec!["hk-1".to_string(), "us-1".to_string(), "hk-2".to_string()];

        app.apply_benchmark_filter("hk".to_string())
            .expect("apply filter");

        assert_eq!(
            app.displayed_members(),
            vec!["hk-1".to_string(), "hk-2".to_string()]
        );
    }

    #[test]
    fn implicit_root_mode_displays_root_choices_as_left_column() {
        let mut app = provider_app();

        assert!(app.implicit_root_mode());
        assert_eq!(
            app.displayed_group_names(),
            vec![
                "自动选择".to_string(),
                "AirTCP".to_string(),
                "宝贝云".to_string()
            ]
        );

        app.groups[0].members = vec!["宝贝云".to_string()];

        assert!(!app.implicit_root_mode());
    }

    #[test]
    fn implicit_root_members_follow_selected_choice() {
        let mut app = provider_app();

        assert_eq!(app.selected_root_choice_name().as_deref(), Some("宝贝云"));
        assert_eq!(
            app.displayed_members(),
            vec!["bby-1".to_string(), "bby-2".to_string()]
        );

        app.provider_index = 1;
        app.sync_member_selection_to_current();

        assert_eq!(app.selected_root_choice_name().as_deref(), Some("AirTCP"));
        assert_eq!(
            app.displayed_members(),
            vec!["air-1".to_string(), "air-2".to_string()]
        );
        assert_eq!(app.member_index, 0);
    }

    #[test]
    fn implicit_root_includes_urltest_auto_choice() {
        let mut app = provider_app();
        app.provider_index = 0;
        app.sync_member_selection_to_current();

        assert_eq!(app.selected_root_choice_name().as_deref(), Some("自动选择"));
        assert_eq!(
            app.displayed_members(),
            vec![
                "auto-node".to_string(),
                "air-1".to_string(),
                "bby-1".to_string()
            ]
        );
        assert!(!app.selected_member_panel_is_manual_selector());
    }

    #[test]
    fn implicit_root_benchmark_summary_is_scoped_to_selected_choice() {
        let mut app = provider_app();
        app.benchmarks.insert(
            "宝贝云".to_string(),
            BenchmarkSummary {
                selector: "宝贝云".to_string(),
                current: Some("bby-2".to_string()),
                pattern: String::new(),
                url: "https://www.gstatic.com/generate_204".to_string(),
                timeout_ms: 5000,
                max_concurrency: 4,
                results: vec![BenchmarkResult {
                    name: "bby-1".to_string(),
                    delay: Some(88),
                    completed: true,
                }],
            },
        );

        assert_eq!(
            app.selected_benchmark()
                .map(|summary| summary.selector.as_str()),
            Some("宝贝云")
        );
        assert!(
            app.selected_benchmark()
                .and_then(|summary| summary.find_result("bby-1"))
                .is_some()
        );
    }

    #[test]
    fn applying_filter_moves_selection_to_visible_member() {
        let mut app = test_app();
        app.groups[0].members = vec!["hk-1".to_string(), "us-1".to_string(), "hk-2".to_string()];
        app.member_index = 1;

        app.apply_benchmark_filter("hk".to_string())
            .expect("apply filter");

        assert_eq!(
            app.displayed_members(),
            vec!["hk-1".to_string(), "hk-2".to_string()]
        );
        assert_eq!(app.member_index, 0);
        assert_eq!(app.displayed_member_index(), Some(0));
    }

    #[test]
    fn displayed_members_match_any_comma_separated_filter() {
        let mut app = test_app();
        app.groups[0].members = vec![
            "美国-1".to_string(),
            "香港-1".to_string(),
            "日本-1".to_string(),
        ];

        app.apply_benchmark_filter("美国,香港".to_string())
            .expect("apply filter");

        assert_eq!(
            app.displayed_members(),
            vec!["美国-1".to_string(), "香港-1".to_string()]
        );
    }

    #[test]
    fn auto_select_keeps_current_when_latency_is_under_threshold() {
        let app = test_app();
        let group = ProxyGroup {
            name: "select".to_string(),
            kind: "Selector".to_string(),
            current: Some("美国-a".to_string()),
            members: vec!["美国-a".to_string(), "美国-b".to_string()],
        };
        let summary = BenchmarkSummary {
            selector: "select".to_string(),
            current: Some("美国-a".to_string()),
            pattern: "美国".to_string(),
            url: "https://www.gstatic.com/generate_204".to_string(),
            timeout_ms: 5000,
            max_concurrency: 4,
            results: vec![
                BenchmarkResult {
                    name: "美国-a".to_string(),
                    delay: Some(500),
                    completed: true,
                },
                BenchmarkResult {
                    name: "美国-b".to_string(),
                    delay: Some(80),
                    completed: true,
                },
            ],
        };

        assert_eq!(app.auto_select_target(&group, &summary), None);
    }

    #[test]
    fn auto_select_switches_to_best_when_current_latency_is_high() {
        let app = test_app();
        let group = ProxyGroup {
            name: "select".to_string(),
            kind: "Selector".to_string(),
            current: Some("美国-a".to_string()),
            members: vec!["美国-a".to_string(), "美国-b".to_string()],
        };
        let summary = BenchmarkSummary {
            selector: "select".to_string(),
            current: Some("美国-a".to_string()),
            pattern: "美国".to_string(),
            url: "https://www.gstatic.com/generate_204".to_string(),
            timeout_ms: 5000,
            max_concurrency: 4,
            results: vec![
                BenchmarkResult {
                    name: "美国-a".to_string(),
                    delay: Some(650),
                    completed: true,
                },
                BenchmarkResult {
                    name: "美国-b".to_string(),
                    delay: Some(80),
                    completed: true,
                },
            ],
        };

        assert_eq!(
            app.auto_select_target(&group, &summary),
            Some("美国-b".to_string())
        );
    }

    #[test]
    fn auto_select_switches_to_best_when_current_is_outside_filter() {
        let app = test_app();
        let group = ProxyGroup {
            name: "select".to_string(),
            kind: "Selector".to_string(),
            current: Some("香港-a".to_string()),
            members: vec!["香港-a".to_string(), "美国-b".to_string()],
        };
        let summary = BenchmarkSummary {
            selector: "select".to_string(),
            current: Some("香港-a".to_string()),
            pattern: "美国".to_string(),
            url: "https://www.gstatic.com/generate_204".to_string(),
            timeout_ms: 5000,
            max_concurrency: 4,
            results: vec![BenchmarkResult {
                name: "美国-b".to_string(),
                delay: Some(80),
                completed: true,
            }],
        };

        assert_eq!(
            app.auto_select_target(&group, &summary),
            Some("美国-b".to_string())
        );
    }

    #[test]
    fn auto_select_benchmark_waits_for_interval() {
        let mut app = test_app();
        app.auto_select_enabled = true;
        let now = Instant::now();
        app.last_auto_select_benchmark = Some(now - Duration::from_secs(29));

        assert!(!app.auto_select_benchmark_due(now));

        app.last_auto_select_benchmark = Some(now - Duration::from_secs(30));
        assert!(app.auto_select_benchmark_due(now));
    }

    #[test]
    fn auto_select_toggle_requires_filter() {
        let mut app = test_app();
        app.benchmark_filter.clear();

        app.handle_key(KeyCode::Char('a')).expect("toggle handled");

        assert!(!app.auto_select_enabled);
        assert_eq!(app.status, "Set a filter before enabling auto-pick");
    }

    #[test]
    fn benchmark_request_carries_max_concurrency() {
        let request = BenchmarkRequest {
            selector: "select".to_string(),
            pattern: "美国".to_string(),
            url: "https://www.gstatic.com/generate_204".to_string(),
            timeout_ms: 5000,
            request_timeout: 12.0,
            max_concurrency: 3,
            nodes: None,
        };

        assert_eq!(request.max_concurrency, 3);
    }
}

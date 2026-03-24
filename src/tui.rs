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
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use ratatui::{DefaultTerminal, Frame};

use crate::controller::{
    ApiClient, BenchmarkEvent, BenchmarkJob, BenchmarkJobKind, BenchmarkRequest,
    BenchmarkSummary, ProxyGroup, run_verification, spawn_benchmark_worker,
};
use crate::defaults::{
    DEFAULT_BENCHMARK_MAX_CONCURRENCY, DEFAULT_CONTROLLER, DEFAULT_DELAY_TEST_URL,
    REFRESH_DEBOUNCE, SINGLE_NODE_RETEST_DEBOUNCE,
};

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
    let [groups_area, members_area] = Layout::horizontal([
        Constraint::Percentage(28),
        Constraint::Percentage(72),
    ])
    .areas(main);

    let groups = app
        .groups
        .iter()
        .map(|group| {
            let current = group
                .current
                .as_deref()
                .map_or(String::from("unset"), ToString::to_string);
            ListItem::new(Line::from(vec![
                Span::styled(
                    truncate_for_width(&group.name, groups_area.width.saturating_sub(10) as usize),
                    Style::default().fg(Color::Cyan),
                ),
                Span::raw(" "),
                Span::styled(
                    format!("[{}]", truncate_for_width(&current, 14)),
                    Style::default().fg(Color::Yellow),
                ),
            ]))
        })
        .collect::<Vec<_>>();

    let groups_block = Block::default()
        .title("Selector Groups")
        .borders(Borders::ALL)
        .border_style(border_style(app.focus == Focus::Groups));
    let groups_widget = List::new(groups)
        .block(groups_block)
        .highlight_style(selected_style(app.focus == Focus::Groups))
        .highlight_symbol("> ");
    let mut groups_state = ListState::default().with_selected(Some(app.group_index));
    frame.render_stateful_widget(groups_widget, groups_area, &mut groups_state);

    let displayed_members = app.displayed_members();
    let members = app
        .selected_group()
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
        .selected_group()
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
                "mode={}  b group benchmark  t node benchmark  s toggle view  / edit filter",
                benchmark_mode_badge(app.latency_sort_mode)
            )
        },
        |summary| {
            let best = summary
                .best_success()
                .map(|item| format!("best={} {}", item.name, item.display_delay()))
                .unwrap_or_else(|| "best=none".to_string());
            format!(
                "filter='{}'  tested={}  mode={}  {}",
                summary.pattern,
                summary.results.len(),
                benchmark_mode_badge(app.latency_sort_mode),
                truncate_for_width(&best, 30)
            )
        },
    );

    let bottom_line = if let Some(input) = app.filter_input.as_deref() {
        Line::from(vec![
            Span::styled("Filter: ", Style::default().fg(Color::Cyan)),
            Span::raw(input),
            Span::styled("  Enter apply  Esc cancel", Style::default().fg(Color::DarkGray)),
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
            Span::styled("b/t", Style::default().fg(Color::Cyan)),
            Span::raw(" benchmark  "),
            Span::styled("s", Style::default().fg(Color::Cyan)),
            Span::raw(" view mode  "),
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

#[derive(Clone, Copy, Eq, PartialEq)]
enum Focus {
    Groups,
    Members,
}

struct App {
    client: ApiClient,
    groups: Vec<ProxyGroup>,
    group_index: usize,
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
}

impl App {
    fn new(client: ApiClient, benchmark_max_concurrency: usize) -> Result<Self> {
        let mut app = Self {
            client,
            groups: Vec::new(),
            group_index: 0,
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
        };
        app.refresh()?;
        Ok(app)
    }

    fn selected_group(&self) -> Option<&ProxyGroup> {
        self.groups.get(self.group_index)
    }

    fn selected_benchmark(&self) -> Option<&BenchmarkSummary> {
        let group = self.selected_group()?;
        self.benchmarks.get(&group.name)
    }

    fn member_matches_filter(&self, member: &str) -> bool {
        self.benchmark_filter.is_empty() || member.contains(&self.benchmark_filter)
    }

    fn displayed_members(&self) -> Vec<String> {
        let Some(group) = self.selected_group() else {
            return Vec::new();
        };
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
        let current = self.selected_group()?.members.get(self.member_index)?;
        members.iter().position(|member| member == current)
    }

    fn sync_selection_to_member_name(&mut self, name: &str) {
        if let Some(group) = self.selected_group()
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
            .selected_group()
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

        match code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(false),
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => {
                self.focus = match self.focus {
                    Focus::Groups => Focus::Members,
                    Focus::Members => Focus::Groups,
                }
            }
            KeyCode::Left | KeyCode::Char('h') => {
                self.focus = match self.focus {
                    Focus::Groups => Focus::Members,
                    Focus::Members => Focus::Groups,
                }
            }
            KeyCode::Down | KeyCode::Char('j') => self.move_next(),
            KeyCode::Up | KeyCode::Char('k') => self.move_previous(),
            KeyCode::Char('g') => self.move_first(),
            KeyCode::Char('G') => self.move_last(),
            KeyCode::Char('r') => self.refresh()?,
            KeyCode::Char('b') => self.start_group_benchmark()?,
            KeyCode::Char('t') => self.start_member_benchmark()?,
            KeyCode::Char('s') => self.toggle_latency_sort_mode(),
            KeyCode::Char('v') => self.run_verify(false)?,
            KeyCode::Char('V') => self.run_verify(true)?,
            KeyCode::Char('/') => self.open_benchmark_filter_modal(),
            KeyCode::Char(' ') => self.activate_selection()?,
            KeyCode::Enter => {}
            _ => {}
        }
        Ok(true)
    }

    fn move_next(&mut self) {
        match self.focus {
            Focus::Groups => {
                if self.group_index + 1 < self.groups.len() {
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
                if self.group_index > 0 {
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
                self.group_index = 0;
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
                if !self.groups.is_empty() {
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
        if self.focus != Focus::Members {
            self.focus = Focus::Members;
            return Ok(());
        }

        let Some(group) = self.selected_group() else {
            bail!("no selector group available");
        };
        let group_name = group.name.clone();
        let Some(member) = group.members.get(self.member_index).cloned() else {
            bail!("no proxy available in selected group");
        };
        self.client
            .switch_proxy(&group_name, &member)
            .with_context(|| format!("failed to switch {} to {}", group_name, member))?;
        if REFRESH_DEBOUNCE > Duration::ZERO {
            std::thread::sleep(REFRESH_DEBOUNCE);
        }
        self.refresh()?;
        self.set_switch_status(&group_name, &member);
        Ok(())
    }

    fn refresh(&mut self) -> Result<()> {
        let previous_group_name = self.selected_group().map(|group| group.name.clone());
        let groups = self.client.fetch_selector_groups()?;
        if groups.is_empty() {
            bail!("no selector groups returned by controller");
        }
        self.groups = groups;
        self.group_index = previous_group_name
            .and_then(|name| self.groups.iter().position(|group| group.name == name))
            .unwrap_or(0);
        self.sync_member_selection_to_current();
        self.status = format!("Loaded {} selector groups", self.groups.len());
        Ok(())
    }

    fn sync_member_selection_to_current(&mut self) {
        let next_index = self
            .selected_group()
            .and_then(|group| {
                group.current
                    .as_deref()
                    .and_then(|current| group.members.iter().position(|member| member == current))
            })
            .unwrap_or(0);
        self.member_index = next_index;
        self.sync_selection_to_displayed_members();
    }

    fn start_group_benchmark(&mut self) -> Result<()> {
        let Some(group) = self.selected_group().cloned() else {
            bail!("no selector group available");
        };
        if self.benchmark_jobs.iter().any(|job| job.group == group.name) {
            self.set_status_only(format!("Benchmark already running for {}", group.name));
            return Ok(());
        }
        let request = BenchmarkRequest {
            selector: group.name.clone(),
            pattern: self.benchmark_filter.clone(),
            url: self.benchmark_url.clone(),
            timeout_ms: self.benchmark_timeout_ms,
            request_timeout: self.benchmark_request_timeout,
            max_concurrency: self.benchmark_max_concurrency,
            nodes: None,
        };
        let candidate_names = self.client.fetch_benchmark_candidates(&request)?;
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
        let Some(group) = self.selected_group().cloned() else {
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

    fn poll_benchmark_updates(&mut self) -> Result<()> {
        let mut finished_indexes = Vec::new();

        for index in 0..self.benchmark_jobs.len() {
            let mut finished = false;
            loop {
                match self.benchmark_jobs[index].receiver.try_recv() {
                    Ok(BenchmarkEvent::Progress(result)) => {
                        if let Some(summary) =
                            self.benchmarks.get_mut(&self.benchmark_jobs[index].group)
                        {
                            summary.update_result(result);
                            self.status = format!(
                                "Benchmarking {}... best so far: {}",
                                self.benchmark_jobs[index].group,
                                summary.best_label()
                            );
                        }
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
                        self.set_status_only(format!("Benchmark worker for {} disconnected", group));
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
                self.apply_benchmark_filter(value);
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

    fn apply_benchmark_filter(&mut self, value: String) {
        self.benchmark_filter = value;
        self.sync_selection_to_displayed_members();
        if self.benchmark_filter.is_empty() {
            self.set_status_only("Benchmark filter cleared");
        } else {
            self.set_status_only(format!(
                "Benchmark filter set to '{}'",
                self.benchmark_filter
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{App, Focus, truncate_for_width};
    use crate::controller::{
        ApiClient, BenchmarkEvent, BenchmarkJob, BenchmarkJobKind, BenchmarkRequest,
        BenchmarkResult, BenchmarkSummary, ProxyGroup,
    };
    use crate::defaults::DEFAULT_BENCHMARK_MAX_CONCURRENCY;
    use crossterm::event::KeyCode;
    use reqwest::Client as AsyncClient;
    use std::collections::BTreeMap;
    use std::sync::mpsc;
    use std::thread;
    use tokio::runtime::Builder as TokioRuntimeBuilder;

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
                current: Some("node-a".to_string()),
                members: vec!["node-a".to_string()],
            }],
            group_index: 0,
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
        }
    }

    #[test]
    fn truncates_wide_strings_without_panicking() {
        let truncated = truncate_for_width("手动选择-自动选择-节点A", 8);
        assert!(truncated.ends_with('…'));
        assert!(!truncated.is_empty());
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
        tx.send(BenchmarkEvent::Finished).expect("send finish event");
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
        tx.send(BenchmarkEvent::Finished).expect("send finish event");
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
        app.groups[0].members = vec![
            "hk-1".to_string(),
            "us-1".to_string(),
            "hk-2".to_string(),
        ];

        app.apply_benchmark_filter("hk".to_string());

        assert_eq!(
            app.displayed_members(),
            vec!["hk-1".to_string(), "hk-2".to_string()]
        );
    }

    #[test]
    fn applying_filter_moves_selection_to_visible_member() {
        let mut app = test_app();
        app.groups[0].members = vec![
            "hk-1".to_string(),
            "us-1".to_string(),
            "hk-2".to_string(),
        ];
        app.member_index = 1;

        app.apply_benchmark_filter("hk".to_string());

        assert_eq!(
            app.displayed_members(),
            vec!["hk-1".to_string(), "hk-2".to_string()]
        );
        assert_eq!(app.member_index, 0);
        assert_eq!(app.displayed_member_index(), Some(0));
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

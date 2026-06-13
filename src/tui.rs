use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
#[cfg(any(windows, target_os = "macos"))]
use std::process::Command;
use std::sync::mpsc::{self, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, MouseEventKind,
};
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
use serde_json::Value;

use crate::controller::{
    ApiClient, BenchmarkEvent, BenchmarkJob, BenchmarkJobKind, BenchmarkRequest, BenchmarkSummary,
    ConnectionInfo, ConnectionsSnapshot, ProxyGroup, run_verification, spawn_benchmark_worker,
};
use crate::defaults::{
    DEFAULT_BENCHMARK_MAX_CONCURRENCY, DEFAULT_CONTROLLER, DEFAULT_DELAY_TEST_URL,
    DEFAULT_SELECTOR_TAG, REFRESH_DEBOUNCE, SINGLE_NODE_RETEST_DEBOUNCE,
};
use crate::storage::{
    BenchmarkRecord, BenchmarkStore, NodeLatencySample, default_benchmark_db_path,
};
use crate::subscriptions::{
    SubscriptionRefreshOutput, SubscriptionRefreshRequest, refresh_subscriptions,
};
use crate::tui_state::{
    BypassRuleSetStore, TuiRuntimeState, TuiStateStore, default_bypass_rule_set_path,
    default_tui_state_path, parse_bypass_entries,
};

const AUTO_SELECT_INTERVAL: Duration = Duration::from_secs(30);
const AUTO_SELECT_THRESHOLD_MS: u64 = 600;
const CONNECTION_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const LATENCY_CHART_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const SYSTEM_PROXY_STATUS_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const SUBSCRIPTION_REFRESH_RETRY_INTERVAL: Duration = Duration::from_secs(5 * 60);
const LATENCY_CHART_DEFAULT_WINDOW: Duration = Duration::from_secs(60 * 60);
const LATENCY_CHART_MIN_WINDOW: Duration = Duration::from_secs(5 * 60);
const LATENCY_CHART_MAX_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);
const DIRECT_CLASH_MODE: &str = "直连";
const RULE_CLASH_MODE: &str = "规则";
const GLOBAL_CLASH_MODE: &str = "全局";

#[derive(Clone, Debug)]
pub(crate) struct TuiSubscriptionRefreshOptions {
    pub(crate) input: PathBuf,
    pub(crate) cache_path: PathBuf,
    pub(crate) config_path: PathBuf,
    pub(crate) disabled: bool,
    pub(crate) force: bool,
    pub(crate) include_geosite_rules: bool,
    pub(crate) include_tun_mode: bool,
    pub(crate) interval_days: u64,
}

pub(crate) fn run_tui(
    controller: Option<String>,
    max_concurrency: Option<usize>,
    subscription_refresh: TuiSubscriptionRefreshOptions,
) -> Result<()> {
    let controller = controller
        .or_else(|| env::var("SING_BOX_CONTROLLER").ok())
        .unwrap_or_else(|| DEFAULT_CONTROLLER.to_string());

    let secret = env::var("SING_BOX_SECRET")
        .ok()
        .filter(|value| !value.is_empty());

    let mut app = App::new(
        ApiClient::new(controller, secret)?,
        max_concurrency.unwrap_or(DEFAULT_BENCHMARK_MAX_CONCURRENCY),
        subscription_refresh,
    )?;
    let terminal = setup_terminal()?;
    let result = run_app(terminal, &mut app);
    restore_terminal()?;
    result
}

fn setup_terminal() -> Result<DefaultTerminal> {
    enable_raw_mode().context("failed to enable raw mode")?;
    execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)
        .context("failed to enter alternate screen")?;
    Ok(ratatui::DefaultTerminal::new(
        ratatui::backend::CrosstermBackend::new(io::stdout()),
    )?)
}

fn restore_terminal() -> Result<()> {
    disable_raw_mode().context("failed to disable raw mode")?;
    execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen)
        .context("failed to leave alternate screen")?;
    Ok(())
}

fn run_app(mut terminal: DefaultTerminal, app: &mut App) -> Result<()> {
    loop {
        app.poll_benchmark_updates()?;
        app.poll_subscription_refresh_updates()?;
        app.poll_system_proxy_updates();
        app.maybe_start_subscription_refresh();
        app.maybe_start_auto_select_benchmark()?;
        app.maybe_refresh_latency_chart()?;
        app.maybe_refresh_connections();
        app.maybe_refresh_system_proxy_status();
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
            Event::Mouse(mouse) => app.handle_mouse(mouse.kind),
            Event::Resize(_, _) => {}
            _ => {}
        }
    }
}

fn draw(frame: &mut Frame, app: &mut App) {
    let [main, status_area] =
        Layout::vertical([Constraint::Min(10), Constraint::Length(8)]).areas(frame.area());
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
        "Providers"
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
                node_order_badge(app.latency_sort_mode)
            )
        })
        .unwrap_or_else(|| format!("Candidates [{}]", node_order_badge(app.latency_sort_mode)));
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
                "clash={}  order={}  auto={}  b group benchmark  t node benchmark  a auto-pick  / filter",
                app.clash_mode_label(),
                node_order_badge(app.latency_sort_mode),
                auto_select_badge(app.auto_select_enabled)
            )
        },
        |summary| {
            let best = summary
                .best_success()
                .map(|item| format!("best={} {}", item.name, item.display_delay()))
                .unwrap_or_else(|| "best=none".to_string());
            format!(
                "filter='{}'  tested={}  clash={}  order={}  auto={}  {}",
                summary.pattern,
                summary.results.len(),
                app.clash_mode_label(),
                node_order_badge(app.latency_sort_mode),
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
            Span::styled("m", Style::default().fg(Color::Cyan)),
            Span::raw(" clash mode  "),
            Span::styled("B", Style::default().fg(Color::Cyan)),
            Span::raw(" bypass  "),
            Span::styled("p", Style::default().fg(Color::Cyan)),
            Span::styled(
                " system proxy  ",
                Style::default().fg(if app.system_proxy_enabled {
                    Color::Green
                } else {
                    Color::DarkGray
                }),
            ),
            Span::styled("b/t", Style::default().fg(Color::Cyan)),
            Span::raw(" benchmark  "),
            Span::styled("s", Style::default().fg(Color::Cyan)),
            Span::raw(" sort order  "),
            Span::styled("a", Style::default().fg(Color::Cyan)),
            Span::raw(" auto-pick  "),
            Span::styled("i", Style::default().fg(Color::Cyan)),
            Span::raw(" info  "),
            Span::styled("c", Style::default().fg(Color::Cyan)),
            Span::raw(" connections  "),
            Span::styled("v/V", Style::default().fg(Color::Cyan)),
            Span::raw(" verify  "),
            Span::styled("/", Style::default().fg(Color::Cyan)),
            Span::raw(" filter  "),
            Span::styled("r", Style::default().fg(Color::Cyan)),
            Span::raw(" refresh  "),
            Span::styled("u", Style::default().fg(Color::Cyan)),
            Span::raw(" update subs  "),
            Span::styled("?", Style::default().fg(Color::Cyan)),
            Span::raw(" help  "),
            Span::styled("q", Style::default().fg(Color::Cyan)),
            Span::raw(" quit"),
        ]),
        Line::from(vec![
            Span::styled("Controller: ", Style::default().fg(Color::DarkGray)),
            Span::raw(app.client.base_url.as_str()),
        ]),
        Line::from(benchmark_hint),
        Line::from(app.connections_summary_line()),
        Line::from(app.subscription_summary_line()),
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
    if app.show_connections {
        draw_connections_panel(frame, app);
    }
    if app.show_help {
        draw_help_panel(frame, app);
    }
    if let Some(input) = app.filter_input.as_deref() {
        let cursor_x = status_area
            .x
            .saturating_add(1)
            .saturating_add(unicode_width::UnicodeWidthStr::width("Filter: ") as u16)
            .saturating_add(unicode_width::UnicodeWidthStr::width(input) as u16);
        let cursor_y = status_area.y.saturating_add(6);
        frame.set_cursor_position((cursor_x, cursor_y));
    }
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
        summary: "Move selection up",
        detail: "Move the highlighted row up in the active list or help panel.",
    },
    HelpBinding {
        key: "k",
        summary: "Move selection up",
        detail: "Vim-style shortcut for moving the highlighted row up.",
    },
    HelpBinding {
        key: "down",
        summary: "Move selection down",
        detail: "Move the highlighted row down in the active list or help panel.",
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
        summary: "Switch to selected proxy",
        detail: "Apply the highlighted proxy or provider selection through the controller API.",
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
        key: "b",
        summary: "Benchmark current group",
        detail: "Start an asynchronous benchmark for all visible candidates in the selected group.",
    },
    HelpBinding {
        key: "t",
        summary: "Benchmark selected node",
        detail: "Start an asynchronous benchmark for only the highlighted node.",
    },
    HelpBinding {
        key: "/",
        summary: "Edit benchmark filter",
        detail: "Open the filter editor. Comma-separated values match any listed text.",
    },
    HelpBinding {
        key: "a",
        summary: "Toggle auto-pick",
        detail: "Enable or disable periodic benchmarking and automatic switching for the current filter, or all nodes when the filter is empty.",
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
        key: "B",
        summary: "Edit bypass rules",
        detail: "Edit direct-bypass domains, IPs, and CIDRs written to the local rule-set.",
    },
    HelpBinding {
        key: "p",
        summary: "Toggle system proxy",
        detail: "Enable or disable the OS system proxy for the detected sing-box mixed inbound.",
    },
    HelpBinding {
        key: "u",
        summary: "Update subscriptions",
        detail: "Force a background subscription refresh when subscription refresh is configured.",
    },
    HelpBinding {
        key: "v",
        summary: "Verify Google and GitHub",
        detail: "Run HTTP verification checks against Google and GitHub.",
    },
    HelpBinding {
        key: "V",
        summary: "Verify Google, GitHub, and Discord",
        detail: "Run HTTP verification checks and include Discord gateway diagnostics.",
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

fn draw_help_panel(frame: &mut Frame, app: &App) {
    let frame_area = frame.area();
    let width = frame_area.width.saturating_sub(4).min(108);
    let height = frame_area.height.saturating_sub(4).min(34);
    let area = centered_rect(width.max(56), height.max(18), frame_area);
    frame.render_widget(Clear, area);
    let [list_area, detail_area] =
        Layout::vertical([Constraint::Min(10), Constraint::Length(3)]).areas(area);
    let selected = app.help_index.min(HELP_BINDINGS.len().saturating_sub(1));
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

fn draw_connections_panel(frame: &mut Frame, app: &App) {
    let frame_area = frame.area();
    let width = frame_area.width.saturating_sub(4).min(120);
    let height = frame_area.height.saturating_sub(4).min(24);
    let area = centered_rect(width.max(20), height.max(8), frame_area);
    frame.render_widget(Clear, area);

    let inner_width = area.width.saturating_sub(4) as usize;
    let max_rows = area.height.saturating_sub(6) as usize;
    let mut lines = vec![
        Line::from(app.connections_summary_line()),
        Line::from(vec![
            Span::styled("Source", Style::default().fg(Color::Cyan)),
            Span::raw("  "),
            Span::styled("Target", Style::default().fg(Color::Cyan)),
            Span::raw("  "),
            Span::styled("Chain", Style::default().fg(Color::Cyan)),
        ]),
    ];

    if let Some(error) = &app.connection_error {
        lines.push(Line::from(format!(
            "error: {}",
            truncate_for_width(error, inner_width.saturating_sub(7))
        )));
    } else if app.connections.connections.is_empty() {
        lines.push(Line::from("No active connections"));
    } else {
        lines.extend(
            app.connections
                .connections
                .iter()
                .take(max_rows)
                .map(|connection| Line::from(format_connection_line(connection, inner_width))),
        );
        let hidden = app.connections.connections.len().saturating_sub(max_rows);
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

fn node_order_badge(latency_sort_mode: bool) -> &'static str {
    if latency_sort_mode {
        "LATENCY ORDER"
    } else {
        "SELECTOR ORDER"
    }
}

fn auto_select_badge(auto_select_enabled: bool) -> &'static str {
    if auto_select_enabled { "ON" } else { "OFF" }
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

fn connection_is_direct(connection: &ConnectionInfo) -> bool {
    connection
        .chains
        .iter()
        .any(|chain| is_direct_chain_name(chain))
}

fn is_direct_chain_name(value: &str) -> bool {
    value.eq_ignore_ascii_case("direct") || value == "国内直连"
}

fn format_bytes_opt(bytes: Option<u64>) -> String {
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

fn subscription_report_badge(report: &SubscriptionRefreshOutput) -> String {
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

fn format_duration_badge(duration: Duration) -> String {
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

fn next_clash_mode(current: Option<&str>, mode_list: &[String]) -> String {
    let modes = if mode_list.is_empty() {
        [GLOBAL_CLASH_MODE, DIRECT_CLASH_MODE, RULE_CLASH_MODE]
            .into_iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
    } else {
        mode_list.to_vec()
    };
    let current_index = current.and_then(|value| modes.iter().position(|mode| mode == value));
    modes
        .get(current_index.map_or(0, |index| (index + 1) % modes.len()))
        .cloned()
        .unwrap_or_else(|| RULE_CLASH_MODE.to_string())
}

fn default_system_proxy_server(config_path: &Path) -> String {
    env::var("SING_BOX_TUI_SYSTEM_PROXY_SERVER")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| {
            detect_mixed_inbound_proxy_server(config_path)
                .unwrap_or_else(|| "127.0.0.1:6780".to_string())
        })
}

fn detect_mixed_inbound_proxy_server(config_path: &Path) -> Option<String> {
    let text = fs::read_to_string(config_path).ok()?;
    detect_mixed_inbound_proxy_server_from_json(&text)
        .or_else(|| detect_mixed_inbound_proxy_server_from_text(&text))
}

fn detect_mixed_inbound_proxy_server_from_json(text: &str) -> Option<String> {
    let config: Value = serde_json::from_str(text).ok()?;
    let inbounds = config.get("inbounds")?.as_array()?;
    let inbound = inbounds
        .iter()
        .find(|inbound| inbound.get("type").and_then(Value::as_str) == Some("mixed"))?;
    let port = inbound.get("listen_port")?.as_u64()?;
    let listen = inbound
        .get("listen")
        .and_then(Value::as_str)
        .unwrap_or("127.0.0.1");
    let host = match listen {
        "::" | "0.0.0.0" | "" => "127.0.0.1",
        value => value,
    };
    Some(format!("{host}:{port}"))
}

fn detect_mixed_inbound_proxy_server_from_text(text: &str) -> Option<String> {
    let inbounds_key = text.find("\"inbounds\"")?;
    let after_key = &text[inbounds_key..];
    let array_start = after_key.find('[')? + inbounds_key;
    let array_end = find_json_array_end(text, array_start)?;
    let inbounds = &text[array_start..=array_end];
    let mixed_index = inbounds.find("\"mixed\"")?;
    let before_mixed = &inbounds[..mixed_index];
    let object_start = before_mixed.rfind('{')?;
    let after_object_start = &inbounds[object_start..];
    let object_end = find_json_object_end(after_object_start, 0)?;
    let inbound = &after_object_start[..=object_end];
    let port = find_json_u16_field(inbound, "listen_port")?;
    let listen = find_json_string_field(inbound, "listen").unwrap_or("127.0.0.1");
    let host = match listen {
        "::" | "0.0.0.0" | "" => "127.0.0.1",
        value => value,
    };
    Some(format!("{host}:{port}"))
}

fn find_json_array_end(text: &str, start: usize) -> Option<usize> {
    find_json_container_end(text, start, '[', ']')
}

fn find_json_object_end(text: &str, start: usize) -> Option<usize> {
    find_json_container_end(text, start, '{', '}')
}

fn find_json_container_end(text: &str, start: usize, open: char, close: char) -> Option<usize> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, ch) in text[start..].char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        if ch == '"' {
            in_string = true;
        } else if ch == open {
            depth += 1;
        } else if ch == close {
            depth = depth.checked_sub(1)?;
            if depth == 0 {
                return Some(start + offset);
            }
        }
    }
    None
}

fn find_json_u16_field<'a>(object: &'a str, field: &str) -> Option<&'a str> {
    let key = format!("\"{field}\"");
    let key_index = object.find(&key)?;
    let after_key = &object[key_index + key.len()..];
    let colon_index = after_key.find(':')?;
    let value = after_key[colon_index + 1..].trim_start();
    let end = value
        .find(|ch: char| !ch.is_ascii_digit())
        .unwrap_or(value.len());
    let port = &value[..end];
    if port.is_empty() { None } else { Some(port) }
}

fn find_json_string_field<'a>(object: &'a str, field: &str) -> Option<&'a str> {
    let key = format!("\"{field}\"");
    let key_index = object.find(&key)?;
    let after_key = &object[key_index + key.len()..];
    let colon_index = after_key.find(':')?;
    let value = after_key[colon_index + 1..].trim_start();
    let value = value.strip_prefix('"')?;
    let end = value.find('"')?;
    Some(&value[..end])
}

#[cfg(windows)]
fn run_system_proxy_update(server: &str, enable: bool) -> Result<String> {
    let script = windows_system_proxy_script_path()
        .with_context(|| "failed to locate scripts/windows/set-system-proxy.ps1")?;
    let action = if enable { "-Enable" } else { "-Disable" };
    let mut args = vec![
        "-NoProfile",
        "-ExecutionPolicy",
        "Bypass",
        "-File",
        script
            .to_str()
            .context("system proxy script path is not valid UTF-8")?,
        action,
    ];
    if enable {
        args.extend(["-Server", server]);
    }
    let output = Command::new("powershell.exe")
        .args(args)
        .output()
        .context("failed to run PowerShell system proxy script")?;

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if output.status.success() {
        return Ok(if stdout.is_empty() {
            format!("Enabled Windows system proxy: {server}")
        } else {
            stdout
        });
    }

    let message = if stderr.is_empty() { stdout } else { stderr };
    bail!(
        "PowerShell exited with {}: {}",
        output.status.code().unwrap_or(-1),
        message
    )
}

#[cfg(target_os = "macos")]
fn run_system_proxy_update(server: &str, enable: bool) -> Result<String> {
    let services = macos_system_proxy_services()?;
    if services.is_empty() {
        bail!("no enabled macOS network services found");
    }

    if enable {
        let (host, port) = parse_proxy_server(server)?;
        for service in &services {
            run_networksetup(&["-setwebproxy", service, &host, &port])?;
            run_networksetup(&["-setsecurewebproxy", service, &host, &port])?;
            run_networksetup(&["-setsocksfirewallproxy", service, &host, &port])?;
        }
        Ok(format!(
            "Enabled macOS system proxy for {} at {server}",
            services.join(", ")
        ))
    } else {
        for service in &services {
            run_networksetup(&["-setwebproxystate", service, "off"])?;
            run_networksetup(&["-setsecurewebproxystate", service, "off"])?;
            run_networksetup(&["-setsocksfirewallproxystate", service, "off"])?;
        }
        Ok(format!(
            "Disabled macOS system proxy for {}",
            services.join(", ")
        ))
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
fn run_system_proxy_update(_server: &str, _enable: bool) -> Result<String> {
    bail!("system proxy toggle is only available on Windows and macOS")
}

#[cfg(windows)]
fn system_proxy_matches(server: &str) -> bool {
    let output = Command::new("reg.exe")
        .args([
            "query",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings",
        ])
        .output();
    let Ok(output) = output else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    registry_value_line(&text, "ProxyEnable")
        .is_some_and(|line| line.split_whitespace().last() == Some("0x1"))
        && registry_value_line(&text, "ProxyServer").is_some_and(|line| {
            line.split_once("REG_SZ")
                .map(|(_, value)| value.trim() == server)
                .unwrap_or(false)
        })
}

#[cfg(target_os = "macos")]
fn system_proxy_matches(server: &str) -> bool {
    let Ok((host, port)) = parse_proxy_server(server) else {
        return false;
    };
    let output = Command::new("scutil").arg("--proxy").output();
    let Ok(output) = output else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    macos_proxy_value(&text, "HTTPEnable") == Some("1")
        && macos_proxy_value(&text, "HTTPProxy") == Some(host.as_str())
        && macos_proxy_value(&text, "HTTPPort") == Some(port.as_str())
        && macos_proxy_value(&text, "HTTPSEnable") == Some("1")
        && macos_proxy_value(&text, "HTTPSProxy") == Some(host.as_str())
        && macos_proxy_value(&text, "HTTPSPort") == Some(port.as_str())
        && macos_proxy_value(&text, "SOCKSEnable") == Some("1")
        && macos_proxy_value(&text, "SOCKSProxy") == Some(host.as_str())
        && macos_proxy_value(&text, "SOCKSPort") == Some(port.as_str())
}

#[cfg(not(any(windows, target_os = "macos")))]
fn system_proxy_matches(_server: &str) -> bool {
    false
}

#[cfg(any(windows, target_os = "macos"))]
fn parse_proxy_server(server: &str) -> Result<(String, String)> {
    let server = server.trim();
    let (host, port) = server
        .rsplit_once(':')
        .with_context(|| format!("system proxy server must be host:port, got {server}"))?;
    let host = host
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    let port = port.trim().to_string();
    if host.is_empty() {
        bail!("system proxy server host is empty");
    }
    let parsed_port = port
        .parse::<u16>()
        .with_context(|| format!("system proxy server port must be a number, got {port}"))?;
    if parsed_port == 0 {
        bail!("system proxy server port must be greater than 0");
    }
    Ok((host, port))
}

#[cfg(target_os = "macos")]
fn macos_system_proxy_services() -> Result<Vec<String>> {
    if let Ok(value) = env::var("SING_BOX_TUI_SYSTEM_PROXY_SERVICE") {
        let services = value
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        if !services.is_empty() {
            return Ok(services);
        }
    }

    let output = Command::new("networksetup")
        .arg("-listallnetworkservices")
        .output()
        .context("failed to list macOS network services")?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !output.status.success() {
        let message = if stderr.is_empty() { stdout } else { stderr };
        bail!(
            "networksetup -listallnetworkservices exited with {}: {}",
            output.status.code().unwrap_or(-1),
            message
        );
    }

    Ok(stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| !line.starts_with("An asterisk"))
        .filter(|line| !line.starts_with('*'))
        .map(ToString::to_string)
        .collect())
}

#[cfg(target_os = "macos")]
fn run_networksetup(args: &[&str]) -> Result<()> {
    let output = Command::new("networksetup")
        .args(args)
        .output()
        .with_context(|| format!("failed to run networksetup {}", args.join(" ")))?;
    if output.status.success() {
        return Ok(());
    }

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let message = if stderr.is_empty() { stdout } else { stderr };
    bail!(
        "networksetup {} exited with {}: {}",
        args.join(" "),
        output.status.code().unwrap_or(-1),
        message
    )
}

#[cfg(target_os = "macos")]
fn macos_proxy_value<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    text.lines().find_map(|line| {
        let (key, value) = line.trim().split_once(':')?;
        (key.trim() == name).then_some(value.trim())
    })
}

#[cfg(windows)]
fn registry_value_line<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    text.lines().find(|line| {
        line.split_whitespace()
            .next()
            .is_some_and(|value| value.eq_ignore_ascii_case(name))
    })
}

#[cfg(windows)]
fn windows_system_proxy_script_path() -> Option<PathBuf> {
    if let Ok(path) = env::var("SING_BOX_TUI_SYSTEM_PROXY_SCRIPT") {
        let path = PathBuf::from(path);
        if path.exists() {
            return Some(path);
        }
    }

    let cwd_path = PathBuf::from("scripts")
        .join("windows")
        .join("set-system-proxy.ps1");
    if cwd_path.exists() {
        return Some(cwd_path);
    }

    let exe = env::current_exe().ok()?;
    for ancestor in exe.ancestors() {
        let candidate = ancestor
            .join("scripts")
            .join("windows")
            .join("set-system-proxy.ps1");
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
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
    clash_mode: Option<String>,
    clash_modes: Vec<String>,
    connections: ConnectionsSnapshot,
    connection_error: Option<String>,
    last_connection_refresh: Instant,
    show_connections: bool,
    show_help: bool,
    help_index: usize,
    subscription_refresh: Option<SubscriptionRefreshState>,
    system_proxy_config_path: PathBuf,
    system_proxy_server: String,
    system_proxy_enabled: bool,
    system_proxy_job: Option<SystemProxyJob>,
    last_system_proxy_status_refresh: Instant,
}

struct SubscriptionRefreshState {
    request: SubscriptionRefreshRequest,
    interval: Duration,
    next_run: Instant,
    job: Option<SubscriptionRefreshJob>,
    last_report: Option<SubscriptionRefreshOutput>,
    last_error: Option<String>,
}

struct SubscriptionRefreshJob {
    receiver: mpsc::Receiver<SubscriptionRefreshEvent>,
    worker: JoinHandle<()>,
}

enum SubscriptionRefreshEvent {
    Finished(Result<SubscriptionRefreshOutput, String>),
}

struct SystemProxyJob {
    server: String,
    enable: bool,
    receiver: mpsc::Receiver<Result<String, String>>,
    worker: JoinHandle<()>,
}

impl SubscriptionRefreshState {
    fn from_options(options: TuiSubscriptionRefreshOptions) -> Result<Option<Self>> {
        if options.disabled || !options.input.exists() {
            return Ok(None);
        }
        if options.interval_days == 0 {
            bail!("--subscription-interval-days must be greater than 0");
        }
        let interval = Duration::from_secs(options.interval_days.saturating_mul(24 * 60 * 60));
        Ok(Some(Self {
            request: SubscriptionRefreshRequest {
                input: options.input,
                cache_path: options.cache_path,
                config_path: options.config_path.clone(),
                merged_path: options.config_path,
                replace_nodes: false,
                include_geosite_rules: options.include_geosite_rules,
                include_tun_mode: options.include_tun_mode,
                force: options.force,
                interval_days: options.interval_days,
            },
            interval,
            next_run: Instant::now(),
            job: None,
            last_report: None,
            last_error: None,
        }))
    }

    fn schedule_after(&mut self, delay: Duration) {
        self.next_run = Instant::now()
            .checked_add(delay)
            .unwrap_or_else(Instant::now);
    }
}

impl App {
    fn new(
        client: ApiClient,
        benchmark_max_concurrency: usize,
        subscription_refresh_options: TuiSubscriptionRefreshOptions,
    ) -> Result<Self> {
        let state_store = TuiStateStore::new(default_tui_state_path());
        let runtime_state = state_store.load()?;
        let system_proxy_server =
            default_system_proxy_server(&subscription_refresh_options.config_path);
        let system_proxy_enabled = system_proxy_matches(&system_proxy_server);
        let system_proxy_config_path = subscription_refresh_options.config_path.clone();
        let subscription_refresh =
            SubscriptionRefreshState::from_options(subscription_refresh_options)?;
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
            clash_mode: None,
            clash_modes: Vec::new(),
            connections: ConnectionsSnapshot::default(),
            connection_error: None,
            last_connection_refresh: Instant::now() - CONNECTION_REFRESH_INTERVAL,
            show_connections: false,
            show_help: false,
            help_index: 0,
            subscription_refresh,
            system_proxy_config_path,
            system_proxy_server,
            system_proxy_enabled,
            system_proxy_job: None,
            last_system_proxy_status_refresh: Instant::now() - SYSTEM_PROXY_STATUS_REFRESH_INTERVAL,
        };
        app.apply_runtime_state(runtime_state.clone());
        app.refresh()?;
        app.apply_runtime_state(runtime_state);
        app.save_bypass_rule_set()?;
        Ok(app)
    }

    fn apply_runtime_state(&mut self, state: TuiRuntimeState) {
        self.benchmark_filter = state.benchmark_filter;
        self.auto_select_enabled = state.auto_pick_enabled;
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
        let provider_group_count = root
            .members
            .iter()
            .filter(|member| self.is_provider_child_group(member))
            .count();
        if provider_group_count > 1 {
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
            return self.provider_child_group_names(root);
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
            self.provider_child_group_names(root)
                .into_iter()
                .nth(self.provider_index)
        })
    }

    fn provider_child_group_names(&self, root: &ProxyGroup) -> Vec<String> {
        root.members
            .iter()
            .filter(|member| self.is_provider_child_group(member))
            .cloned()
            .collect()
    }

    fn is_provider_child_group(&self, member: &str) -> bool {
        self.group_by_name(member)
            .is_some_and(|group| group.kind.eq_ignore_ascii_case("selector"))
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

    fn connections_summary_line(&self) -> String {
        if let Some(error) = &self.connection_error {
            return format!("connections unavailable: {}", truncate_for_width(error, 80));
        }

        let direct_count = self
            .connections
            .connections
            .iter()
            .filter(|connection| connection_is_direct(connection))
            .count();
        let active_count = self.connections.connections.len();
        let proxied_count = active_count.saturating_sub(direct_count);
        format!(
            "connections active={} proxy={} direct={} up={} down={}  c details",
            active_count,
            proxied_count,
            direct_count,
            format_bytes_opt(self.connections.upload_total),
            format_bytes_opt(self.connections.download_total)
        )
    }

    fn subscription_summary_line(&self) -> String {
        let Some(state) = &self.subscription_refresh else {
            return "subscriptions: disabled or no .suburl".to_string();
        };
        if state.job.is_some() {
            return format!(
                "subscriptions: refreshing {} -> {}",
                state.request.input.display(),
                state.request.merged_path.display()
            );
        }
        if let Some(error) = &state.last_error {
            return format!(
                "subscriptions: error: {}  retry in {}",
                truncate_for_width(error, 72),
                format_duration_badge(state.next_run.saturating_duration_since(Instant::now()))
            );
        }
        if let Some(report) = &state.last_report {
            return format!(
                "subscriptions: {}  next in {}  reload sing-box to apply",
                subscription_report_badge(report),
                format_duration_badge(state.next_run.saturating_duration_since(Instant::now()))
            );
        }
        format!(
            "subscriptions: pending first refresh from {}",
            state.request.input.display()
        )
    }

    fn maybe_start_subscription_refresh(&mut self) {
        let Some(state) = self.subscription_refresh.as_mut() else {
            return;
        };
        if state.job.is_some() || Instant::now() < state.next_run {
            return;
        }

        self.start_subscription_refresh_job(false, "Refreshing subscriptions in background...");
    }

    fn start_manual_subscription_refresh(&mut self) {
        if self.subscription_refresh.is_none() {
            self.set_status_only("Subscription refresh is disabled or .suburl was not found");
            return;
        }
        let started = self.start_subscription_refresh_job(
            true,
            "Manually refreshing subscriptions in background...",
        );
        if !started {
            self.set_status_only("Subscription refresh is already running");
        }
    }

    fn start_subscription_refresh_job(&mut self, force: bool, status: &str) -> bool {
        let Some(state) = self.subscription_refresh.as_mut() else {
            return false;
        };
        if state.job.is_some() {
            return false;
        }

        let mut request = state.request.clone();
        request.force = force || state.request.force;
        state.request.force = false;
        let (tx, rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let result = refresh_subscriptions(&request).map_err(|error| error.to_string());
            let _ = tx.send(SubscriptionRefreshEvent::Finished(result));
        });
        state.job = Some(SubscriptionRefreshJob {
            receiver: rx,
            worker,
        });
        state.last_error = None;
        self.set_status_only(status);
        true
    }

    fn poll_subscription_refresh_updates(&mut self) -> Result<()> {
        let Some(state) = self.subscription_refresh.as_mut() else {
            return Ok(());
        };
        let Some(job) = state.job.as_ref() else {
            return Ok(());
        };

        let event = match job.receiver.try_recv() {
            Ok(event) => event,
            Err(TryRecvError::Empty) => return Ok(()),
            Err(TryRecvError::Disconnected) => SubscriptionRefreshEvent::Finished(Err(
                "subscription refresh worker disconnected".to_string(),
            )),
        };

        let job = state.job.take().expect("subscription refresh job exists");
        let _ = job.worker.join();
        match event {
            SubscriptionRefreshEvent::Finished(Ok(report)) => {
                state.schedule_after(state.interval);
                state.last_error = None;
                state.last_report = Some(report.clone());
                self.set_status_only(format!(
                    "Subscription refresh updated config: {}; reload/restart sing-box to apply",
                    subscription_report_badge(&report)
                ));
            }
            SubscriptionRefreshEvent::Finished(Err(error)) => {
                state.schedule_after(SUBSCRIPTION_REFRESH_RETRY_INTERVAL);
                state.last_error = Some(error.clone());
                self.set_status_only(format!(
                    "Subscription refresh failed: {}; retry in {}",
                    truncate_for_width(&error, 80),
                    format_duration_badge(SUBSCRIPTION_REFRESH_RETRY_INTERVAL)
                ));
            }
        }
        Ok(())
    }

    fn maybe_refresh_connections(&mut self) {
        if self.last_connection_refresh.elapsed() < CONNECTION_REFRESH_INTERVAL {
            return;
        }
        self.last_connection_refresh = Instant::now();
        match self.client.fetch_connections() {
            Ok(connections) => {
                self.connections = connections;
                self.connection_error = None;
            }
            Err(error) => {
                self.connection_error = Some(error.to_string());
            }
        }
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

    fn clash_mode_label(&self) -> &str {
        self.clash_mode.as_deref().unwrap_or("unknown")
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
        if self.show_help {
            match code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('?') => {
                    self.show_help = false;
                    self.set_status_only("Help closed");
                }
                KeyCode::Down | KeyCode::Char('j') => self.move_help_next(),
                KeyCode::Up | KeyCode::Char('k') => self.move_help_previous(),
                KeyCode::Char('g') => self.help_index = 0,
                KeyCode::Char('G') => self.help_index = HELP_BINDINGS.len().saturating_sub(1),
                KeyCode::Char('q') => return Ok(false),
                _ => {}
            }
            return Ok(true);
        }
        if self.show_connections {
            match code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('c') => {
                    self.show_connections = false;
                    self.set_status_only("Connection details closed");
                }
                KeyCode::Char('r') => {
                    self.last_connection_refresh = Instant::now() - CONNECTION_REFRESH_INTERVAL;
                    self.maybe_refresh_connections();
                    self.set_status_only("Connection details refreshed");
                }
                KeyCode::Char('q') => return Ok(false),
                _ => {}
            }
            return Ok(true);
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
            KeyCode::Char('u') => self.start_manual_subscription_refresh(),
            KeyCode::Char('b') => self.start_group_benchmark()?,
            KeyCode::Char('t') => self.start_member_benchmark()?,
            KeyCode::Char('s') => self.toggle_latency_sort_mode(),
            KeyCode::Char('a') => self.toggle_auto_select()?,
            KeyCode::Char('m') => self.cycle_clash_mode()?,
            KeyCode::Char('B') => self.open_bypass_modal(),
            KeyCode::Char('p') => self.set_system_proxy(),
            KeyCode::Char('i') => self.open_latency_chart()?,
            KeyCode::Char('c') => self.open_connections_panel(),
            KeyCode::Char('v') => self.run_verify(false)?,
            KeyCode::Char('V') => self.run_verify(true)?,
            KeyCode::Char('?') => self.open_help_panel(),
            KeyCode::Char('/') => self.open_benchmark_filter_modal(),
            KeyCode::Char(' ') => self.activate_selection()?,
            KeyCode::Enter => {}
            _ => {}
        }
        Ok(true)
    }

    fn handle_mouse(&mut self, kind: MouseEventKind) {
        if !self.show_help {
            return;
        }
        match kind {
            MouseEventKind::ScrollDown => self.move_help_next(),
            MouseEventKind::ScrollUp => self.move_help_previous(),
            _ => {}
        }
    }

    fn selected_member_name(&self) -> Option<String> {
        self.selected_group()?
            .members
            .get(self.member_index)
            .cloned()
    }

    fn open_connections_panel(&mut self) {
        self.show_connections = true;
        self.set_status_only("Showing active connections");
    }

    fn open_help_panel(&mut self) {
        self.show_help = true;
        self.flash = None;
        self.set_status_only("Showing help");
    }

    fn move_help_next(&mut self) {
        self.help_index = (self.help_index + 1).min(HELP_BINDINGS.len().saturating_sub(1));
    }

    fn move_help_previous(&mut self) {
        self.help_index = self.help_index.saturating_sub(1);
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

    fn cycle_clash_mode(&mut self) -> Result<()> {
        let current = self.clash_mode.as_deref();
        let next = next_clash_mode(current, &self.clash_modes);
        self.client
            .set_mode(&next)
            .with_context(|| format!("failed to switch Clash mode to {next}"))?;
        self.clash_mode = Some(next.clone());
        self.set_status_only(format!("Switched Clash mode to {next}"));
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
        let config = self.client.fetch_config()?;
        let groups = self.client.fetch_selector_groups()?;
        if groups.is_empty() {
            bail!("no selector groups returned by controller");
        }
        self.clash_mode = config.mode;
        self.clash_modes = config.mode_list;
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
            "Sort order: LATENCY ORDER (hide failed-tested nodes, sort successful nodes by delay)"
                .to_string()
        } else {
            "Sort order: SELECTOR ORDER (original selector order with current filter)".to_string()
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

        let Some(group_name) = self.selected_group().map(|group| group.name.clone()) else {
            self.set_status_only("No selector group available for auto-pick");
            return Ok(());
        };
        self.auto_select_enabled = true;
        self.last_auto_select_benchmark = None;
        self.set_status_only(format!(
            "Auto-pick enabled for {} ({}, {}ms threshold, every {}s)",
            group_name,
            self.benchmark_scope_label(),
            self.auto_select_threshold_ms,
            self.auto_select_interval.as_secs()
        ));
        self.save_runtime_state()?;
        Ok(())
    }

    fn auto_select_benchmark_due(&self, now: Instant) -> bool {
        if !self.auto_select_enabled {
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
                "Auto-pick found no nodes in {} for {}",
                group.name,
                self.benchmark_scope_label()
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
            "Auto-pick benchmarking {} ({})...",
            group.name,
            self.benchmark_scope_label()
        ));
        Ok(())
    }

    fn benchmark_scope_label(&self) -> String {
        if self.benchmark_filter.is_empty() {
            "all nodes".to_string()
        } else {
            format!("filter '{}'", self.benchmark_filter)
        }
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

    fn set_system_proxy(&mut self) {
        if self.system_proxy_job.is_some() {
            self.set_status_only("System proxy update is already running");
            return;
        }
        self.system_proxy_server = default_system_proxy_server(&self.system_proxy_config_path);
        self.system_proxy_enabled = system_proxy_matches(&self.system_proxy_server);
        let enable = !self.system_proxy_enabled;
        let server = self.system_proxy_server.clone();
        let (tx, rx) = mpsc::channel();
        let worker_server = server.clone();
        let worker = thread::spawn(move || {
            let result =
                run_system_proxy_update(&worker_server, enable).map_err(|error| error.to_string());
            let _ = tx.send(result);
        });
        self.system_proxy_job = Some(SystemProxyJob {
            server: server.clone(),
            enable,
            receiver: rx,
            worker,
        });
        if enable {
            self.set_status_only(format!("Enabling system proxy at {server}..."));
        } else {
            self.set_status_only("Disabling system proxy...");
        }
    }

    fn poll_system_proxy_updates(&mut self) {
        let Some(job) = self.system_proxy_job.as_ref() else {
            return;
        };
        let result = match job.receiver.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err("system proxy worker disconnected".to_string()),
        };

        let job = self
            .system_proxy_job
            .take()
            .expect("system proxy job exists");
        let _ = job.worker.join();
        match result {
            Ok(message) => {
                self.system_proxy_server = job.server;
                self.system_proxy_enabled = if job.enable {
                    system_proxy_matches(&self.system_proxy_server)
                } else {
                    false
                };
                self.set_status_with_flash(message);
            }
            Err(error) => {
                self.system_proxy_enabled = system_proxy_matches(&self.system_proxy_server);
                self.set_status_with_flash(format!(
                    "System proxy update failed: {}",
                    truncate_for_width(&error, 90)
                ));
            }
        }
    }

    fn maybe_refresh_system_proxy_status(&mut self) {
        if self.system_proxy_job.is_some()
            || self.last_system_proxy_status_refresh.elapsed()
                < SYSTEM_PROXY_STATUS_REFRESH_INTERVAL
        {
            return;
        }
        self.last_system_proxy_status_refresh = Instant::now();
        self.system_proxy_server = default_system_proxy_server(&self.system_proxy_config_path);
        self.system_proxy_enabled = system_proxy_matches(&self.system_proxy_server);
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
        App, CONNECTION_REFRESH_INTERVAL, DIRECT_CLASH_MODE, Focus, GLOBAL_CLASH_MODE,
        LATENCY_CHART_DEFAULT_WINDOW, LATENCY_CHART_REFRESH_INTERVAL, LatencyChartState,
        LatencyChartTimeUnit, RULE_CLASH_MODE, SYSTEM_PROXY_STATUS_REFRESH_INTERVAL,
        connection_is_direct, format_bytes, format_connection_line, format_duration_badge,
        latency_chart_segments, latency_chart_threshold_line, latency_chart_time_unit,
        latency_chart_windowed_samples, latency_chart_y_bounds, latency_chart_zoom_in,
        latency_chart_zoom_out, next_clash_mode, subscription_report_badge, truncate_for_width,
    };
    use crate::controller::{
        ApiClient, BenchmarkEvent, BenchmarkJob, BenchmarkJobKind, BenchmarkRequest,
        BenchmarkResult, BenchmarkSummary, ConnectionInfo, ConnectionMetadata, ConnectionsSnapshot,
        ProxyGroup,
    };
    use crate::defaults::DEFAULT_BENCHMARK_MAX_CONCURRENCY;
    use crate::subscriptions::{ProviderRefreshSummary, SubscriptionRefreshOutput};
    use crate::tui_state::{TuiRuntimeState, TuiStateStore};
    use crossterm::event::KeyCode;
    use crossterm::event::MouseEventKind;
    use reqwest::Client as AsyncClient;
    use std::collections::BTreeMap;
    use std::path::PathBuf;
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
                base_url: "http://127.0.0.1:9992".to_string(),
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
            clash_mode: Some(RULE_CLASH_MODE.to_string()),
            clash_modes: vec![
                GLOBAL_CLASH_MODE.to_string(),
                DIRECT_CLASH_MODE.to_string(),
                RULE_CLASH_MODE.to_string(),
            ],
            connections: ConnectionsSnapshot::default(),
            connection_error: None,
            last_connection_refresh: Instant::now() - CONNECTION_REFRESH_INTERVAL,
            show_connections: false,
            show_help: false,
            help_index: 0,
            subscription_refresh: None,
            system_proxy_config_path: PathBuf::from("config.json"),
            system_proxy_server: "127.0.0.1:6780".to_string(),
            system_proxy_enabled: false,
            system_proxy_job: None,
            last_system_proxy_status_refresh: Instant::now() - SYSTEM_PROXY_STATUS_REFRESH_INTERVAL,
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
        app.provider_index = 1;
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
    fn detects_mixed_inbound_proxy_server_from_config() {
        let path = std::env::temp_dir().join("sing-box-tui-proxy-config-test.json");
        std::fs::write(
            &path,
            r#"{"inbounds":[{"type":"mixed","listen":"::","listen_port":6780}]}"#,
        )
        .expect("write config");

        assert_eq!(
            super::detect_mixed_inbound_proxy_server(&path).as_deref(),
            Some("127.0.0.1:6780")
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn detects_mixed_inbound_proxy_server_from_partial_text_when_config_is_invalid() {
        let text = r#"{
            "inbounds": [
                {"type":"mixed","listen":"::","listen_port":6780}
            ],
            "outbounds": [broken
        }"#;

        assert_eq!(
            super::detect_mixed_inbound_proxy_server_from_text(text).as_deref(),
            Some("127.0.0.1:6780")
        );
    }

    fn test_connection(host: &str, chains: Vec<&str>) -> ConnectionInfo {
        ConnectionInfo {
            id: "conn-1".to_string(),
            download: 0,
            upload: 0,
            start: None,
            chains: chains.into_iter().map(ToString::to_string).collect(),
            rule: Some("clash_mode=规则 => route(手动选择)".to_string()),
            rule_payload: None,
            metadata: ConnectionMetadata {
                network: Some("tcp".to_string()),
                kind: Some("tun/tun-in".to_string()),
                source_ip: Some("172.19.0.1".to_string()),
                destination_ip: Some("1.1.1.1".to_string()),
                host: Some(host.to_string()),
                destination_port: Some("443".to_string()),
                source_port: None,
                process_path: None,
            },
        }
    }

    #[test]
    fn formats_connection_summary_counts_proxy_and_direct() {
        let mut app = test_app();
        app.connections = ConnectionsSnapshot {
            upload_total: Some(1536),
            download_total: Some(2 * 1024 * 1024),
            memory: None,
            connections: vec![
                test_connection("www.google.com", vec!["node-a", "airtcp", "手动选择"]),
                test_connection("example.cn", vec!["国内直连"]),
            ],
        };

        assert_eq!(
            app.connections_summary_line(),
            "connections active=2 proxy=1 direct=1 up=1.5KiB down=2.0MiB  c details"
        );
    }

    #[test]
    fn connection_helpers_format_active_rows() {
        let direct = test_connection("example.cn", vec!["国内直连"]);
        let proxied = test_connection("www.google.com", vec!["node-a", "airtcp", "手动选择"]);

        assert!(connection_is_direct(&direct));
        assert!(!connection_is_direct(&proxied));
        assert_eq!(format_bytes(512), "512B");
        assert_eq!(format_bytes(2048), "2.0KiB");
        assert!(format_connection_line(&proxied, 120).contains("www.google.com:443"));
        assert!(format_connection_line(&proxied, 120).contains("node-a -> airtcp"));
    }

    #[test]
    fn subscription_report_badge_summarizes_provider_counts() {
        let report = SubscriptionRefreshOutput {
            input_path: ".suburl".to_string(),
            cache_path: ".suburl.cache.json".to_string(),
            interval_days: 1,
            merged_config_path: "/usr/local/etc/sing-box/config.json".to_string(),
            backup_config_path: Some(
                "/usr/local/etc/sing-box/config.json.sing-box-tui-subscription-backup".to_string(),
            ),
            providers: vec![
                ProviderRefreshSummary {
                    provider: "宝贝云".to_string(),
                    subscription_url: "https://example.com?token=REDACTED".to_string(),
                    status: "fetched".to_string(),
                    imported_nodes: 67,
                    fetched_at_unix: 10,
                    warning: None,
                },
                ProviderRefreshSummary {
                    provider: "airtcp".to_string(),
                    subscription_url: "https://example.com/link/REDACTED".to_string(),
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
    }

    #[test]
    fn duration_badge_uses_day_hour_minute_units() {
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
    fn pressing_c_opens_connection_details() {
        let mut app = test_app();

        app.handle_key(KeyCode::Char('c'))
            .expect("open connection panel");

        assert!(app.show_connections);
        assert_eq!(app.status, "Showing active connections");
    }

    #[test]
    fn question_mark_opens_and_closes_help() {
        let mut app = test_app();

        app.handle_key(KeyCode::Char('?')).expect("open help");

        assert!(app.show_help);
        assert_eq!(app.status, "Showing help");

        app.handle_key(KeyCode::Esc).expect("close help");

        assert!(!app.show_help);
        assert_eq!(app.status, "Help closed");
    }

    #[test]
    fn help_panel_moves_selection_with_keyboard() {
        let mut app = test_app();
        app.handle_key(KeyCode::Char('?')).expect("open help");

        app.handle_key(KeyCode::Down).expect("move down");
        assert_eq!(app.help_index, 1);

        app.handle_key(KeyCode::Char('j')).expect("move down");
        assert_eq!(app.help_index, 2);

        app.handle_key(KeyCode::Up).expect("move up");
        assert_eq!(app.help_index, 1);

        app.handle_key(KeyCode::Char('k')).expect("move up");
        assert_eq!(app.help_index, 0);
    }

    #[test]
    fn help_panel_moves_selection_with_mouse_wheel() {
        let mut app = test_app();
        app.handle_key(KeyCode::Char('?')).expect("open help");

        app.handle_mouse(MouseEventKind::ScrollDown);
        assert_eq!(app.help_index, 1);

        app.handle_mouse(MouseEventKind::ScrollUp);
        assert_eq!(app.help_index, 0);
    }

    #[test]
    fn pressing_u_without_subscription_state_updates_status() {
        let mut app = test_app();

        app.handle_key(KeyCode::Char('u'))
            .expect("manual subscription refresh handled");

        assert_eq!(
            app.status,
            "Subscription refresh is disabled or .suburl was not found"
        );
    }

    #[test]
    fn next_clash_mode_cycles_controller_mode_list() {
        let modes = vec![
            GLOBAL_CLASH_MODE.to_string(),
            DIRECT_CLASH_MODE.to_string(),
            RULE_CLASH_MODE.to_string(),
        ];

        assert_eq!(
            next_clash_mode(Some(DIRECT_CLASH_MODE), &modes),
            RULE_CLASH_MODE
        );
        assert_eq!(
            next_clash_mode(Some(RULE_CLASH_MODE), &modes),
            GLOBAL_CLASH_MODE
        );
        assert_eq!(
            next_clash_mode(Some(GLOBAL_CLASH_MODE), &modes),
            DIRECT_CLASH_MODE
        );
    }

    #[test]
    fn next_clash_mode_defaults_to_rule_after_direct() {
        assert_eq!(
            next_clash_mode(Some(DIRECT_CLASH_MODE), &[]),
            RULE_CLASH_MODE
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
    fn app_applies_persisted_auto_pick_without_filter() {
        let mut app = test_app();
        let state = TuiRuntimeState {
            benchmark_filter: String::new(),
            auto_pick_enabled: true,
            ..TuiRuntimeState::default()
        };

        app.apply_runtime_state(state);

        assert!(app.benchmark_filter.is_empty());
        assert!(app.auto_select_enabled);
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
            "Sort order: LATENCY ORDER (hide failed-tested nodes, sort successful nodes by delay)"
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
        app.auto_select_enabled = true;

        app.handle_key(KeyCode::Char('/')).expect("open modal");
        app.handle_key(KeyCode::Backspace).expect("backspace");
        app.handle_key(KeyCode::Backspace).expect("backspace");
        app.handle_key(KeyCode::Enter).expect("submit");

        assert!(app.benchmark_filter.is_empty());
        assert_eq!(app.filter_input, None);
        assert_eq!(app.status, "Benchmark filter cleared");
        assert!(app.auto_select_enabled);
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
            vec!["AirTCP".to_string(), "宝贝云".to_string()]
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

        app.provider_index = 0;
        app.sync_member_selection_to_current();

        assert_eq!(app.selected_root_choice_name().as_deref(), Some("AirTCP"));
        assert_eq!(
            app.displayed_members(),
            vec!["air-1".to_string(), "air-2".to_string()]
        );
        assert_eq!(app.member_index, 0);
    }

    #[test]
    fn implicit_root_excludes_urltest_auto_choice() {
        let mut app = provider_app();
        app.provider_index = 0;
        app.sync_member_selection_to_current();

        assert_eq!(app.selected_root_choice_name().as_deref(), Some("AirTCP"));
        assert_eq!(
            app.displayed_members(),
            vec!["air-1".to_string(), "air-2".to_string()]
        );
        assert!(app.selected_member_panel_is_manual_selector());
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

        app.benchmark_filter.clear();
        assert!(app.auto_select_benchmark_due(now));
    }

    #[test]
    fn auto_select_toggle_allows_empty_filter() {
        let mut app = test_app();
        app.benchmark_filter.clear();

        app.handle_key(KeyCode::Char('a')).expect("toggle handled");

        assert!(app.auto_select_enabled);
        assert_eq!(
            app.status,
            "Auto-pick enabled for select (all nodes, 600ms threshold, every 30s)"
        );
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

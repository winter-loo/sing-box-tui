use std::collections::BTreeMap;
use std::env;
use std::io::{self};
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
use reqwest::blocking::Client;
use reqwest::header::{HeaderMap, HeaderValue};
use serde::Deserialize;
use urlencoding::encode;

const DEFAULT_CONTROLLER: &str = "http://127.0.0.1:9090";
const REFRESH_DEBOUNCE: Duration = Duration::from_millis(200);

fn main() -> Result<()> {
    let controller = env::args()
        .nth(1)
        .or_else(|| env::var("SING_BOX_CONTROLLER").ok())
        .unwrap_or_else(|| DEFAULT_CONTROLLER.to_string());

    let secret = env::var("SING_BOX_SECRET")
        .ok()
        .filter(|value| !value.is_empty());

    let mut app = App::new(ApiClient::new(controller, secret)?)?;
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
    let [main, footer] =
        Layout::vertical([Constraint::Min(8), Constraint::Length(4)]).areas(frame.area());
    let [groups_area, members_area] =
        Layout::horizontal([Constraint::Percentage(35), Constraint::Percentage(65)]).areas(main);

    let groups = app
        .groups
        .iter()
        .map(|group| {
            let current = group.current.as_deref().map_or("unset", |value| value);
            ListItem::new(Line::from(vec![
                Span::styled(
                    truncate_for_width(
                        &sanitize_display_text(&group.name),
                        groups_area.width.saturating_sub(10) as usize,
                    ),
                    Style::default().fg(Color::Cyan),
                ),
                Span::raw(" "),
                Span::styled(
                    format!(
                        "[{}]",
                        truncate_for_width(&sanitize_display_text(current), 14)
                    ),
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

    let members = app
        .selected_group()
        .map(|group| {
            group
                .members
                .iter()
                .map(|member| {
                    let is_current = group.current.as_deref() == Some(member.as_str());
                    let display_member = sanitize_display_text(member);
                    let mut style = Style::default();
                    if is_current {
                        style = style.fg(Color::Green).add_modifier(Modifier::BOLD);
                    }
                    ListItem::new(Line::from(vec![
                        Span::styled(
                            truncate_for_width(
                                &display_member,
                                members_area.width.saturating_sub(8) as usize,
                            ),
                            style,
                        ),
                        Span::raw(if is_current { "  *" } else { "" }),
                    ]))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let members_title = app
        .selected_group()
        .map(|group| format!("Candidates for {}", sanitize_display_text(&group.name)))
        .unwrap_or_else(|| String::from("Candidates"));
    let members_block = Block::default()
        .title(members_title)
        .borders(Borders::ALL)
        .border_style(border_style(app.focus == Focus::Members));
    let members_widget = List::new(members)
        .block(members_block)
        .highlight_style(selected_style(app.focus == Focus::Members))
        .highlight_symbol("> ");
    let mut members_state = ListState::default().with_selected(Some(app.member_index));
    frame.render_stateful_widget(members_widget, members_area, &mut members_state);

    let help = Paragraph::new(vec![
        Line::from(vec![
            Span::styled("Arrows/jk", Style::default().fg(Color::Cyan)),
            Span::raw(" move  "),
            Span::styled("Tab/h/l", Style::default().fg(Color::Cyan)),
            Span::raw(" switch pane  "),
            Span::styled("Enter", Style::default().fg(Color::Cyan)),
            Span::raw(" select  "),
            Span::styled("r", Style::default().fg(Color::Cyan)),
            Span::raw(" refresh  "),
            Span::styled("q", Style::default().fg(Color::Cyan)),
            Span::raw(" quit"),
        ]),
        Line::from(vec![
            Span::styled("Controller: ", Style::default().fg(Color::DarkGray)),
            Span::raw(app.client.base_url.as_str()),
        ]),
        Line::from(app.status_line()),
    ])
    .block(Block::default().title("Status").borders(Borders::ALL));
    frame.render_widget(help, footer);

    if let Some(message) = app.flash_message() {
        let area = centered_rect(60, 5, frame.area());
        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new(message).block(Block::default().title("Info").borders(Borders::ALL)),
            area,
        );
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

fn sanitize_display_text(value: &str) -> String {
    let sanitized = value
        .chars()
        .filter(|ch| !is_problematic_terminal_char(*ch))
        .collect::<String>();
    let compact = sanitized.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.is_empty() {
        String::from("<unnamed>")
    } else {
        compact
    }
}

fn is_problematic_terminal_char(ch: char) -> bool {
    ch.is_control()
        || matches!(ch, '\u{200d}' | '\u{fe0f}')
        || ('\u{1f1e6}'..='\u{1f1ff}').contains(&ch)
        || ('\u{1f300}'..='\u{1faff}').contains(&ch)
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
}

impl App {
    fn new(client: ApiClient) -> Result<Self> {
        let mut app = Self {
            client,
            groups: Vec::new(),
            group_index: 0,
            member_index: 0,
            focus: Focus::Groups,
            status: String::from("Loading proxy groups..."),
            flash: None,
        };
        app.refresh()?;
        Ok(app)
    }

    fn selected_group(&self) -> Option<&ProxyGroup> {
        self.groups.get(self.group_index)
    }

    fn status_line(&self) -> String {
        self.status.clone()
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
        match code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(false),
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => self.focus = Focus::Members,
            KeyCode::Left | KeyCode::Char('h') => self.focus = Focus::Groups,
            KeyCode::Down | KeyCode::Char('j') => self.move_next(),
            KeyCode::Up | KeyCode::Char('k') => self.move_previous(),
            KeyCode::Char('g') => self.move_first(),
            KeyCode::Char('G') => self.move_last(),
            KeyCode::Char('r') => self.refresh()?,
            KeyCode::Enter => self.activate_selection()?,
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
                if let Some(group) = self.selected_group() {
                    if self.member_index + 1 < group.members.len() {
                        self.member_index += 1;
                    }
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
                if self.member_index > 0 {
                    self.member_index -= 1;
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
            Focus::Members => self.member_index = 0,
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
                if let Some(group) = self.selected_group() {
                    if !group.members.is_empty() {
                        self.member_index = group.members.len() - 1;
                    }
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
        let Some(member) = group.members.get(self.member_index).cloned() else {
            bail!("no proxy available in selected group");
        };
        self.client
            .switch_proxy(&group.name, &member)
            .with_context(|| format!("failed to switch {} to {}", group.name, member))?;
        self.status = format!("Switched {} to {}", group.name, member);
        self.flash = Some((self.status.clone(), Instant::now()));
        if REFRESH_DEBOUNCE > Duration::ZERO {
            std::thread::sleep(REFRESH_DEBOUNCE);
        }
        self.refresh()
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
        let next_index =
            self.selected_group()
                .and_then(|group| {
                    group.current.as_deref().and_then(|current| {
                        group.members.iter().position(|member| member == current)
                    })
                })
                .unwrap_or(0);
        self.member_index = next_index;
    }
}

#[derive(Clone)]
struct ProxyGroup {
    name: String,
    current: Option<String>,
    members: Vec<String>,
}

struct ApiClient {
    base_url: String,
    client: Client,
}

impl ApiClient {
    fn new(base_url: String, secret: Option<String>) -> Result<Self> {
        let mut headers = HeaderMap::new();
        if let Some(secret) = secret {
            headers.insert(
                "Authorization",
                HeaderValue::from_str(&format!("Bearer {secret}"))
                    .context("invalid SING_BOX_SECRET header value")?,
            );
        }
        let client = Client::builder()
            .default_headers(headers)
            .build()
            .context("failed to build HTTP client")?;

        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            client,
        })
    }

    fn fetch_selector_groups(&self) -> Result<Vec<ProxyGroup>> {
        let response = self
            .client
            .get(format!("{}/proxies", self.base_url))
            .send()
            .context("failed to query Clash API /proxies")?
            .error_for_status()
            .context("Clash API /proxies returned an error")?;

        let payload: ProxiesResponse = response
            .json()
            .context("failed to decode Clash API /proxies response")?;

        let mut groups = payload
            .proxies
            .into_values()
            .filter(|proxy| proxy.kind.eq_ignore_ascii_case("selector"))
            .map(|proxy| ProxyGroup {
                name: proxy.name,
                current: proxy.now,
                members: proxy.all,
            })
            .collect::<Vec<_>>();

        groups.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(groups)
    }

    fn switch_proxy(&self, group: &str, proxy: &str) -> Result<()> {
        let encoded_group = encode(group);
        self.client
            .put(format!("{}/proxies/{}", self.base_url, encoded_group))
            .json(&SwitchProxyRequest {
                name: proxy.to_string(),
            })
            .send()
            .with_context(|| format!("failed to send switch request for {group}"))?
            .error_for_status()
            .with_context(|| format!("controller rejected switch request for {group}"))?;
        Ok(())
    }
}

#[derive(Deserialize)]
struct ProxiesResponse {
    proxies: BTreeMap<String, ProxyNode>,
}

#[derive(Deserialize)]
struct ProxyNode {
    name: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    now: Option<String>,
    #[serde(default)]
    all: Vec<String>,
}

#[derive(serde::Serialize)]
struct SwitchProxyRequest {
    name: String,
}

#[cfg(test)]
mod tests {
    use super::{sanitize_display_text, truncate_for_width};

    #[test]
    fn truncates_wide_strings_without_panicking() {
        let truncated = truncate_for_width("手动选择-自动选择-节点A", 8);
        assert!(truncated.ends_with('…'));
        assert!(!truncated.is_empty());
    }

    #[test]
    fn strips_flag_emoji_for_terminal_safe_display() {
        assert_eq!(sanitize_display_text("🇺🇸美国光速1"), "美国光速1");
    }
}

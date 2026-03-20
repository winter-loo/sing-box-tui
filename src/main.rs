use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::io::{self};
use std::path::PathBuf;
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
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use urlencoding::encode;

const DEFAULT_CONTROLLER: &str = "http://127.0.0.1:9090";
const DEFAULT_CONFIG_PATH: &str = "/etc/sing-box/config.json";
const REFRESH_DEBOUNCE: Duration = Duration::from_millis(200);
const DEFAULT_TEST_URL: &str = "https://www.gstatic.com/generate_204";
const DEFAULT_TEST_TIMEOUT_MS: u64 = 5_000;

fn main() -> Result<()> {
    let options = CliOptions::parse(env::args().skip(1))?;

    if let Some(source) = options.import_from.as_ref() {
        run_import(
            source,
            options.import_output.as_ref(),
            options.import_full_config,
            &options.import_config_path,
        )?;
        return Ok(());
    }

    let controller = options
        .controller
        .or_else(|| env::var("SING_BOX_CONTROLLER").ok())
        .unwrap_or_else(|| DEFAULT_CONTROLLER.to_string());

    let secret = env::var("SING_BOX_SECRET")
        .ok()
        .filter(|value| !value.is_empty());
    let test_url = env::var("SING_BOX_TEST_URL").unwrap_or_else(|_| DEFAULT_TEST_URL.to_string());
    let test_timeout_ms = env::var("SING_BOX_TEST_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(DEFAULT_TEST_TIMEOUT_MS);

    let mut app = App::new(ApiClient::new(controller, secret)?, test_url, test_timeout_ms)?;
    let terminal = setup_terminal()?;
    let result = run_app(terminal, &mut app);
    restore_terminal()?;
    result
}

#[derive(Default)]
struct CliOptions {
    controller: Option<String>,
    import_from: Option<PathBuf>,
    import_output: Option<PathBuf>,
    import_full_config: bool,
    import_config_path: PathBuf,
}

impl CliOptions {
    fn parse<I>(args: I) -> Result<Self>
    where
        I: IntoIterator<Item = String>,
    {
        let mut options = Self {
            import_config_path: PathBuf::from(DEFAULT_CONFIG_PATH),
            ..Self::default()
        };
        let mut iter = args.into_iter();

        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--controller" => {
                    let value = iter.next().context("--controller requires a value")?;
                    options.controller = Some(value);
                }
                "--import-from" => {
                    let value = iter.next().context("--import-from requires a file path")?;
                    options.import_from = Some(PathBuf::from(value));
                }
                "--import-output" => {
                    let value = iter
                        .next()
                        .context("--import-output requires a file path")?;
                    options.import_output = Some(PathBuf::from(value));
                }
                "--import-full-config" => {
                    options.import_full_config = true;
                }
                "--import-config-path" => {
                    let value = iter
                        .next()
                        .context("--import-config-path requires a file path")?;
                    options.import_config_path = PathBuf::from(value);
                }
                "--help" | "-h" => {
                    print_usage();
                    std::process::exit(0);
                }
                value if value.starts_with('-') => bail!("unknown flag: {value}"),
                value => {
                    if options.controller.is_none() {
                        options.controller = Some(value.to_string());
                    } else {
                        bail!("unexpected positional argument: {value}");
                    }
                }
            }
        }

        if options.import_output.is_some() && options.import_from.is_none() {
            bail!("--import-output requires --import-from");
        }

        Ok(options)
    }
}

fn print_usage() {
    println!("sing-box-tui [--controller URL]");
    println!(
        "sing-box-tui --import-from <clash-proxies.yml> [--import-output <nodes.json|config.json>]"
    );
    println!("  --import-full-config [--import-config-path /etc/sing-box/config.json]");
}

fn run_import(
    source: &PathBuf,
    output: Option<&PathBuf>,
    full_config: bool,
    config_path: &PathBuf,
) -> Result<()> {
    let text = fs::read_to_string(source)
        .with_context(|| format!("failed to read Clash proxy file {}", source.display()))?;
    let config: ClashConfig =
        serde_yaml::from_str(&text).context("failed to parse Clash YAML")?;

    let converted = config
        .proxies
        .into_iter()
        .filter(|entry| !is_metadata_entry(entry))
        .map(convert_clash_proxy)
        .collect::<Result<Vec<_>>>()?;

    let output_value = if full_config {
        build_full_config(config_path, converted)?
    } else {
        Value::Array(converted)
    };

    let json_text = serde_json::to_string_pretty(&output_value)
        .context("failed to serialize sing-box import output")?;

    if let Some(output) = output {
        fs::write(output, format!("{json_text}\n"))
            .with_context(|| format!("failed to write {}", output.display()))?;
        println!("{}", output.display());
    } else {
        println!("{json_text}");
    }

    Ok(())
}

fn build_full_config(config_path: &PathBuf, imported_nodes: Vec<Value>) -> Result<Value> {
    if config_path.exists() {
        let text = fs::read_to_string(config_path)
            .with_context(|| format!("failed to read {}", config_path.display()))?;
        let mut config: Value = serde_json::from_str(&text)
            .with_context(|| format!("failed to parse {}", config_path.display()))?;
        merge_into_existing_config(&mut config, imported_nodes)?;
        Ok(config)
    } else {
        Ok(build_default_config(imported_nodes))
    }
}

fn build_default_config(imported_nodes: Vec<Value>) -> Value {
    let node_tags = collect_tags(&imported_nodes);
    let select_members = with_auto_member(&node_tags);

    let mut outbounds = Vec::with_capacity(imported_nodes.len() + 4);
    outbounds.push(json!({
        "type": "selector",
        "tag": "select",
        "outbounds": select_members,
        "default": "auto",
    }));
    outbounds.push(json!({
        "type": "urltest",
        "tag": "auto",
        "outbounds": node_tags,
        "url": "https://www.gstatic.com/generate_204",
        "interval": "10m",
    }));
    outbounds.extend(imported_nodes);
    outbounds.push(json!({
        "type": "direct",
        "tag": "direct",
    }));
    outbounds.push(json!({
        "type": "block",
        "tag": "block",
    }));

    json!({
        "log": {
            "level": "info",
        },
        "inbounds": [
            {
                "type": "mixed",
                "tag": "mixed-in",
                "listen": "127.0.0.1",
                "listen_port": 5780,
            }
        ],
        "outbounds": outbounds,
        "route": {
            "final": "select",
        },
        "experimental": {
            "cache_file": {
                "enabled": true,
            },
            "clash_api": {
                "external_controller": "127.0.0.1:9090",
                "secret": "",
            }
        }
    })
}

fn merge_into_existing_config(config: &mut Value, imported_nodes: Vec<Value>) -> Result<()> {
    let root = config
        .as_object_mut()
        .context("existing sing-box config must be a JSON object")?;
    let outbounds_value = root
        .entry("outbounds")
        .or_insert_with(|| Value::Array(Vec::new()));
    let outbounds = outbounds_value
        .as_array_mut()
        .context("existing config outbounds must be an array")?;

    upsert_special_outbound(
        outbounds,
        "direct",
        || json!({ "type": "direct", "tag": "direct" }),
        |_| {},
    )?;
    upsert_special_outbound(
        outbounds,
        "block",
        || json!({ "type": "block", "tag": "block" }),
        |_| {},
    )?;

    let node_tags = collect_tags(&imported_nodes);
    let select_members = with_auto_member(&node_tags);

    upsert_special_outbound(
        outbounds,
        "select",
        || {
            json!({
                "type": "selector",
                "tag": "select",
                "outbounds": select_members,
                "default": "auto",
            })
        },
        |value| {
            merge_outbound_members(value, &select_members);
            ensure_string_field(value, "type", "selector");
            ensure_string_field(value, "default", "auto");
        },
    )?;

    upsert_special_outbound(
        outbounds,
        "auto",
        || {
            json!({
                "type": "urltest",
                "tag": "auto",
                "outbounds": node_tags,
                "url": "https://www.gstatic.com/generate_204",
                "interval": "10m",
            })
        },
        |value| {
            merge_outbound_members(value, &node_tags);
            ensure_string_field(value, "type", "urltest");
            ensure_string_field(value, "url", "https://www.gstatic.com/generate_204");
            ensure_string_field(value, "interval", "10m");
        },
    )?;

    for node in imported_nodes {
        upsert_tagged_outbound(outbounds, node)?;
    }

    let route_value = root.entry("route").or_insert_with(|| json!({}));
    let route = route_value
        .as_object_mut()
        .context("existing config route must be an object")?;
    route
        .entry("final")
        .or_insert_with(|| Value::String("select".to_string()));

    let experimental_value = root.entry("experimental").or_insert_with(|| json!({}));
    let experimental = experimental_value
        .as_object_mut()
        .context("existing config experimental must be an object")?;
    let cache_file_value = experimental
        .entry("cache_file")
        .or_insert_with(|| json!({ "enabled": true }));
    let cache_file = cache_file_value
        .as_object_mut()
        .context("existing config experimental.cache_file must be an object")?;
    cache_file
        .entry("enabled")
        .or_insert_with(|| Value::Bool(true));

    let clash_api_value = experimental.entry("clash_api").or_insert_with(|| {
        json!({
            "external_controller": "127.0.0.1:9090",
            "secret": "",
        })
    });
    let clash_api = clash_api_value
        .as_object_mut()
        .context("existing config experimental.clash_api must be an object")?;
    clash_api.remove("external_ui_download_url");
    clash_api.remove("external_ui_download_detour");
    clash_api
        .entry("external_controller")
        .or_insert_with(|| Value::String("127.0.0.1:9090".to_string()));
    clash_api
        .entry("secret")
        .or_insert_with(|| Value::String(String::new()));

    Ok(())
}

fn upsert_special_outbound<F, G>(
    outbounds: &mut Vec<Value>,
    tag: &str,
    build_default: F,
    update: G,
) -> Result<()>
where
    F: FnOnce() -> Value,
    G: FnOnce(&mut Value),
{
    if let Some(existing) = find_outbound_by_tag_mut(outbounds, tag) {
        update(existing);
    } else {
        outbounds.push(build_default());
    }
    Ok(())
}

fn upsert_tagged_outbound(outbounds: &mut Vec<Value>, outbound: Value) -> Result<()> {
    let tag = outbound_tag(&outbound)?.to_string();
    if let Some(existing) = find_outbound_by_tag_mut(outbounds, &tag) {
        *existing = outbound;
    } else {
        outbounds.push(outbound);
    }
    Ok(())
}

fn find_outbound_by_tag_mut<'a>(outbounds: &'a mut [Value], tag: &str) -> Option<&'a mut Value> {
    outbounds.iter_mut().find(|value| {
        value
            .get("tag")
            .and_then(Value::as_str)
            .is_some_and(|current| current == tag)
    })
}

fn outbound_tag(outbound: &Value) -> Result<&str> {
    outbound
        .get("tag")
        .and_then(Value::as_str)
        .context("converted outbound is missing string tag")
}

fn collect_tags(outbounds: &[Value]) -> Vec<String> {
    outbounds
        .iter()
        .filter_map(|value| value.get("tag").and_then(Value::as_str))
        .map(ToString::to_string)
        .collect()
}

fn with_auto_member(tags: &[String]) -> Vec<String> {
    let mut members = Vec::with_capacity(tags.len() + 1);
    members.push("auto".to_string());
    members.extend(tags.iter().cloned());
    members
}

fn merge_outbound_members(outbound: &mut Value, new_members: &[String]) {
    let Some(object) = outbound.as_object_mut() else {
        return;
    };

    let mut merged = BTreeSet::new();
    let entry = object
        .entry("outbounds")
        .or_insert_with(|| Value::Array(Vec::new()));
    let Some(existing) = entry.as_array_mut() else {
        return;
    };

    let mut values = Vec::new();
    for value in existing.iter() {
        if let Some(member) = value.as_str() && merged.insert(member.to_string()) {
            values.push(Value::String(member.to_string()));
        }
    }
    for member in new_members {
        if merged.insert(member.clone()) {
            values.push(Value::String(member.clone()));
        }
    }
    *existing = values;
}

fn ensure_string_field(outbound: &mut Value, key: &str, value: &str) {
    let Some(object) = outbound.as_object_mut() else {
        return;
    };
    object
        .entry(key.to_string())
        .or_insert_with(|| Value::String(value.to_string()));
}

fn is_metadata_entry(entry: &ClashProxy) -> bool {
    ["剩余流量", "距离下次重置剩余", "套餐到期"]
        .iter()
        .any(|marker| entry.name.contains(marker))
}

fn convert_clash_proxy(entry: ClashProxy) -> Result<Value> {
    match entry.kind.as_str() {
        "hysteria2" => {
            let mut outbound = json!({
                "type": "hysteria2",
                "tag": entry.name,
                "server": entry.server,
                "password": entry.password,
                "up_mbps": entry.up,
                "down_mbps": entry.down,
                "tls": build_tls(&entry),
            });
            if let Some(ports) = entry.ports.clone() {
                outbound["server_ports"] = json!([ports.replace('-', ":")]);
            } else {
                outbound["server_port"] = json!(entry.port);
            }
            Ok(outbound)
        }
        "trojan" => Ok(json!({
            "type": "trojan",
            "tag": entry.name,
            "server": entry.server,
            "server_port": entry.port,
            "password": entry.password,
            "tls": build_tls(&entry),
            "transport": build_transport(&entry),
        })),
        "vmess" => Ok(json!({
            "type": "vmess",
            "tag": entry.name,
            "server": entry.server,
            "server_port": entry.port,
            "uuid": entry.uuid,
            "security": entry.cipher.clone().unwrap_or_else(|| "auto".to_string()),
            "alter_id": entry.alter_id.unwrap_or(0),
            "tls": build_tls(&entry),
            "transport": build_transport(&entry),
        })),
        "ss" => Ok(json!({
            "type": "shadowsocks",
            "tag": entry.name,
            "server": entry.server,
            "server_port": entry.port,
            "method": entry.cipher,
            "password": entry.password,
        })),
        other => bail!("unsupported Clash proxy type: {other}"),
    }
}

fn build_tls(entry: &ClashProxy) -> Value {
    let server_name = entry
        .sni
        .clone()
        .or_else(|| entry.server_name.clone())
        .unwrap_or_default();
    let enabled = entry.tls.unwrap_or(false)
        || !server_name.is_empty()
        || entry.skip_cert_verify.unwrap_or(false);
    if !enabled {
        return Value::Null;
    }

    let mut value = json!({
        "enabled": true,
    });
    if !server_name.is_empty() {
        value["server_name"] = Value::String(server_name);
    }
    if entry.skip_cert_verify.unwrap_or(false) {
        value["insecure"] = Value::Bool(true);
    }
    value
}

fn build_transport(entry: &ClashProxy) -> Value {
    if entry.network.as_deref() != Some("ws") {
        return Value::Null;
    }

    let mut headers = serde_json::Map::new();
    if let Some(ws_opts) = entry.ws_opts.as_ref() {
        for (key, value) in &ws_opts.headers {
            headers.insert(key.clone(), Value::String(value.clone()));
        }
    }
    for (key, value) in &entry.ws_headers {
        headers.insert(key.clone(), Value::String(value.clone()));
    }

    let path = entry
        .ws_opts
        .as_ref()
        .and_then(|opts| opts.path.clone())
        .or_else(|| entry.ws_path.clone())
        .unwrap_or_else(|| "/".to_string());

    let mut transport = json!({
        "type": "ws",
        "path": path,
    });
    if !headers.is_empty() {
        transport["headers"] = Value::Object(headers);
    }
    transport
}

#[derive(Deserialize)]
struct ClashConfig {
    #[serde(default)]
    proxies: Vec<ClashProxy>,
}

#[derive(Deserialize)]
struct ClashProxy {
    name: String,
    #[serde(rename = "type")]
    kind: String,
    server: String,
    port: u16,
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    cipher: Option<String>,
    #[serde(default)]
    uuid: Option<String>,
    #[serde(rename = "alterId", default)]
    alter_id: Option<u32>,
    #[serde(default)]
    tls: Option<bool>,
    #[serde(rename = "skip-cert-verify", default)]
    skip_cert_verify: Option<bool>,
    #[serde(default)]
    sni: Option<String>,
    #[serde(rename = "servername", default)]
    server_name: Option<String>,
    #[serde(default)]
    up: Option<u32>,
    #[serde(default)]
    down: Option<u32>,
    #[serde(default)]
    ports: Option<String>,
    #[serde(default)]
    network: Option<String>,
    #[serde(rename = "ws-opts", default)]
    ws_opts: Option<ClashWsOpts>,
    #[serde(rename = "ws-path", default)]
    ws_path: Option<String>,
    #[serde(rename = "ws-headers", default)]
    ws_headers: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct ClashWsOpts {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
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
            Span::styled("t", Style::default().fg(Color::Cyan)),
            Span::raw(" test  "),
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
        let popup_width = frame.area().width.saturating_sub(8).clamp(40, 96);
        let popup_height = frame.area().height.saturating_sub(6).clamp(5, 8);
        let area = centered_rect(popup_width, popup_height, frame.area());
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

fn format_error_chain(error: &anyhow::Error) -> String {
    error
        .chain()
        .map(|cause| cause.to_string())
        .collect::<Vec<_>>()
        .join(" -> ")
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
    test_url: String,
    test_timeout_ms: u64,
}

impl App {
    fn new(client: ApiClient, test_url: String, test_timeout_ms: u64) -> Result<Self> {
        let mut app = Self {
            client,
            groups: Vec::new(),
            group_index: 0,
            member_index: 0,
            focus: Focus::Groups,
            status: String::from("Loading proxy groups..."),
            flash: None,
            test_url,
            test_timeout_ms,
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
            KeyCode::Char('t') => self.test_selection(),
            KeyCode::Enter => self.activate_selection()?,
            _ => {}
        }
        Ok(true)
    }

    fn test_selection(&mut self) {
        let Some(group) = self.selected_group() else {
            self.status = String::from("No selector group available to test");
            self.flash = Some((self.status.clone(), Instant::now()));
            return;
        };

        let target = match self.focus {
            Focus::Groups => group.current.clone().unwrap_or_else(|| group.name.clone()),
            Focus::Members => group
                .members
                .get(self.member_index)
                .cloned()
                .or_else(|| group.current.clone())
                .unwrap_or_else(|| group.name.clone()),
        };

        match self
            .client
            .probe_delay(&target, &self.test_url, self.test_timeout_ms)
        {
            Ok(delay_ms) => {
                self.status = format!(
                    "Test OK: {} responded in {} ms",
                    sanitize_display_text(&target),
                    delay_ms
                );
            }
            Err(error) => {
                self.status = format!(
                    "Test failed: {} ({})",
                    sanitize_display_text(&target),
                    format_error_chain(&error)
                );
            }
        }
        self.flash = Some((self.status.clone(), Instant::now()));
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

    fn probe_delay(&self, proxy: &str, test_url: &str, timeout_ms: u64) -> Result<u64> {
        let encoded_proxy = encode(proxy);
        let encoded_url = encode(test_url);
        let response = self
            .client
            .get(format!(
                "{}/proxies/{}/delay?timeout={}&url={}",
                self.base_url, encoded_proxy, timeout_ms, encoded_url
            ))
            .send()
            .with_context(|| format!("failed to test proxy {proxy}"))?;
        let status = response.status();

        if !status.is_success() {
            let body = response.text().unwrap_or_default();
            let message = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|json| {
                    json.get("message")
                        .and_then(Value::as_str)
                        .map(ToString::to_string)
                })
                .filter(|message| !message.is_empty())
                .unwrap_or_else(|| {
                    if body.is_empty() {
                        status.to_string()
                    } else {
                        body
                    }
                });
            bail!("{message}");
        }

        let body = response.text().context("failed to read delay probe response")?;
        parse_delay_ms(&body)
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

#[derive(Serialize)]
struct SwitchProxyRequest {
    name: String,
}

fn parse_delay_ms(body: &str) -> Result<u64> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        bail!("empty delay response");
    }
    if let Ok(value) = trimmed.parse::<u64>() {
        return Ok(value);
    }

    let json: Value = serde_json::from_str(trimmed).context("invalid delay response")?;
    if let Some(delay) = json.get("delay").and_then(Value::as_u64) {
        return Ok(delay);
    }
    bail!("delay missing from response");
}

#[cfg(test)]
mod tests {
    use super::{
        build_default_config, merge_into_existing_config, parse_delay_ms, sanitize_display_text,
        truncate_for_width,
    };
    use serde_json::{Value, json};

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

    #[test]
    fn parses_json_delay_response() {
        assert_eq!(parse_delay_ms("{\"delay\":123}").unwrap(), 123);
    }

    #[test]
    fn default_full_config_contains_selector_and_imported_nodes() {
        let config = build_default_config(vec![json!({
            "type": "trojan",
            "tag": "node-a",
            "server": "example.com",
            "server_port": 443,
            "password": "secret",
        })]);

        let outbounds = config["outbounds"].as_array().expect("outbounds array");
        assert!(outbounds.iter().any(|value| value["tag"] == "select"));
        assert!(outbounds.iter().any(|value| value["tag"] == "auto"));
        assert!(outbounds.iter().any(|value| value["tag"] == "node-a"));
        assert_eq!(config["route"]["final"], "select");
    }

    #[test]
    fn merge_preserves_existing_config_and_adds_imported_nodes() {
        let mut config = json!({
            "inbounds": [{
                "type": "mixed",
                "tag": "mixed-in",
                "listen": "127.0.0.1",
                "listen_port": 9000,
            }],
            "outbounds": [{
                "type": "selector",
                "tag": "select",
                "outbounds": ["existing-node"],
                "default": "existing-node",
            }, {
                "type": "trojan",
                "tag": "existing-node",
                "server": "old.example.com",
                "server_port": 443,
                "password": "secret",
            }],
            "route": {
                "final": "existing-node",
            },
            "experimental": {
                "clash_api": {
                    "external_controller": "127.0.0.1:9090",
                    "external_ui_download_url": "https://example.com/ui.zip",
                    "external_ui_download_detour": "direct"
                }
            }
        });

        merge_into_existing_config(
            &mut config,
            vec![json!({
                "type": "trojan",
                "tag": "node-a",
                "server": "example.com",
                "server_port": 443,
                "password": "secret",
            })],
        )
        .expect("merge succeeds");

        let outbounds = config["outbounds"].as_array().expect("outbounds array");
        let select = outbounds
            .iter()
            .find(|value| value["tag"] == "select")
            .expect("select outbound");
        let members = select["outbounds"].as_array().expect("selector members");
        assert!(members.contains(&Value::String("existing-node".to_string())));
        assert!(members.contains(&Value::String("auto".to_string())));
        assert!(members.contains(&Value::String("node-a".to_string())));
        assert_eq!(config["route"]["final"], "existing-node");
        assert!(outbounds.iter().any(|value| value["tag"] == "node-a"));
        let clash_api = config["experimental"]["clash_api"]
            .as_object()
            .expect("clash_api object");
        assert!(!clash_api.contains_key("external_ui_download_url"));
        assert!(!clash_api.contains_key("external_ui_download_detour"));
    }
}

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::io::{self};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use reqwest::Version;

use tokio::process::Command as TokioCommand;
use tokio::runtime::{Builder as TokioRuntimeBuilder, Runtime as TokioRuntime};
use tokio::task::JoinSet;

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
use reqwest::Client as AsyncClient;
use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::redirect::Policy;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use urlencoding::encode;

const DEFAULT_CONTROLLER: &str = "http://127.0.0.1:9090";
const DEFAULT_CONFIG_PATH: &str = "/etc/sing-box/config.json";
const DEFAULT_DELAY_TEST_URL: &str = "https://www.gstatic.com/generate_204";
const DEFAULT_BENCHMARK_MAX_CONCURRENCY: usize = 16;
const REFRESH_DEBOUNCE: Duration = Duration::from_millis(200);
const SINGLE_NODE_RETEST_DEBOUNCE: Duration = Duration::from_millis(800);

fn main() -> Result<()> {
    match CliCommand::parse(env::args().skip(1))? {
        CliCommand::Run {
            controller,
            max_concurrency,
        } => run_tui(controller, max_concurrency),
        CliCommand::Import {
            input,
            output,
            config_path,
            replace_nodes,
        } => run_import(&input, output.as_ref(), true, &config_path, replace_nodes),
        CliCommand::Benchmark {
            controller,
            selector,
            pattern,
            url,
            timeout_ms,
            request_timeout,
            max_concurrency,
            switch,
            verify,
            verify_discord,
        } => run_benchmark(BenchmarkOptions {
            controller,
            selector,
            pattern,
            url,
            timeout_ms,
            request_timeout,
            max_concurrency,
            switch,
            verify,
            verify_discord,
        }),
    }
}

fn run_tui(controller: Option<String>, max_concurrency: Option<usize>) -> Result<()> {
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

enum CliCommand {
    Run {
        controller: Option<String>,
        max_concurrency: Option<usize>,
    },
    Import {
        input: PathBuf,
        output: Option<PathBuf>,
        config_path: PathBuf,
        replace_nodes: bool,
    },
    Benchmark {
        controller: Option<String>,
        selector: String,
        pattern: String,
        url: String,
        timeout_ms: u64,
        request_timeout: f64,
        max_concurrency: usize,
        switch: bool,
        verify: bool,
        verify_discord: bool,
    },
}

impl CliCommand {
    fn parse<I>(args: I) -> Result<Self>
    where
        I: IntoIterator<Item = String>,
    {
        let args = args.into_iter().collect::<Vec<_>>();
        if args.is_empty() {
            return Ok(Self::Run {
                controller: None,
                max_concurrency: None,
            });
        }

        match args[0].as_str() {
            "run" => Self::parse_run(&args[1..]),
            "import" => Self::parse_import(&args[1..]),
            "benchmark" => Self::parse_benchmark(&args[1..]),
            "--help" | "-h" | "help" => {
                print_usage();
                std::process::exit(0);
            }
            value if value.starts_with('-') => bail!("unknown flag: {value}"),
            value => bail!("unknown command: {value}"),
        }
    }

    fn parse_run(args: &[String]) -> Result<Self> {
        let mut controller = None;
        let mut max_concurrency = None;
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--controller" => {
                    i += 1;
                    let value = args.get(i).context("--controller requires a value")?;
                    controller = Some(value.clone());
                }
                "--max-concurrency" => {
                    i += 1;
                    max_concurrency =
                        Some(parse_max_concurrency(args.get(i), "--max-concurrency")?);
                }
                "--help" | "-h" => {
                    print_run_usage();
                    std::process::exit(0);
                }
                value if value.starts_with('-') => bail!("unknown flag for run: {value}"),
                value => bail!("unexpected positional argument for run: {value}"),
            }
            i += 1;
        }
        Ok(Self::Run {
            controller,
            max_concurrency,
        })
    }

    fn parse_import(args: &[String]) -> Result<Self> {
        let mut input = None;
        let mut output = None;
        let mut config_path = PathBuf::from(DEFAULT_CONFIG_PATH);
        let mut replace_nodes = false;
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "-i" | "--input" => {
                    i += 1;
                    let value = args.get(i).context("-i/--input requires a file path")?;
                    input = Some(PathBuf::from(value));
                }
                "-o" | "--output" => {
                    i += 1;
                    let value = args.get(i).context("-o/--output requires a file path")?;
                    output = Some(PathBuf::from(value));
                }
                "--config" => {
                    i += 1;
                    let value = args.get(i).context("--config requires a file path")?;
                    config_path = PathBuf::from(value);
                }
                "--replace-nodes" => {
                    replace_nodes = true;
                }
                "--help" | "-h" => {
                    print_import_usage();
                    std::process::exit(0);
                }
                value if value.starts_with('-') => bail!("unknown flag for import: {value}"),
                value => {
                    if input.is_none() {
                        input = Some(PathBuf::from(value));
                    } else {
                        bail!("unexpected positional argument for import: {value}");
                    }
                }
            }
            i += 1;
        }

        Ok(Self::Import {
            input: input.context("import requires an input Clash YAML file (use -i/--input)")?,
            output,
            config_path,
            replace_nodes,
        })
    }

    fn parse_benchmark(args: &[String]) -> Result<Self> {
        let mut controller = None;
        let mut selector = String::from("select");
        let mut pattern = String::new();
        let mut url = String::from(DEFAULT_DELAY_TEST_URL);
        let mut timeout_ms = 5000_u64;
        let mut request_timeout = 12.0_f64;
        let mut max_concurrency = DEFAULT_BENCHMARK_MAX_CONCURRENCY;
        let mut switch = false;
        let mut verify = false;
        let mut verify_discord = false;
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--controller" => {
                    i += 1;
                    let value = args.get(i).context("--controller requires a value")?;
                    controller = Some(value.clone());
                }
                "--selector" => {
                    i += 1;
                    selector = args.get(i).context("--selector requires a value")?.clone();
                }
                "--match" | "--pattern" => {
                    i += 1;
                    pattern = args
                        .get(i)
                        .context("--match/--pattern requires a value")?
                        .clone();
                }
                "--url" => {
                    i += 1;
                    url = args.get(i).context("--url requires a value")?.clone();
                }
                "--timeout-ms" => {
                    i += 1;
                    timeout_ms = args
                        .get(i)
                        .context("--timeout-ms requires a value")?
                        .parse()
                        .context("--timeout-ms must be an integer")?;
                }
                "--request-timeout" => {
                    i += 1;
                    request_timeout = args
                        .get(i)
                        .context("--request-timeout requires a value")?
                        .parse()
                        .context("--request-timeout must be a number")?;
                }
                "--max-concurrency" => {
                    i += 1;
                    max_concurrency = parse_max_concurrency(args.get(i), "--max-concurrency")?;
                }
                "--switch" => switch = true,
                "--verify" => verify = true,
                "--verify-discord" => verify_discord = true,
                "--help" | "-h" => {
                    print_benchmark_usage();
                    std::process::exit(0);
                }
                value if value.starts_with('-') => bail!("unknown flag for benchmark: {value}"),
                value => bail!("unexpected positional argument for benchmark: {value}"),
            }
            i += 1;
        }

        Ok(Self::Benchmark {
            controller,
            selector,
            pattern,
            url,
            timeout_ms,
            request_timeout,
            max_concurrency,
            switch,
            verify,
            verify_discord,
        })
    }
}

fn parse_max_concurrency(value: Option<&String>, flag: &str) -> Result<usize> {
    let parsed = value
        .with_context(|| format!("{flag} requires a value"))?
        .parse::<usize>()
        .with_context(|| format!("{flag} must be a positive integer"))?;
    if parsed == 0 {
        bail!("{flag} must be greater than 0");
    }
    Ok(parsed)
}

fn print_usage() {
    println!("sing-box-tui <command> [options]");
    println!();
    println!("Commands:");
    println!("  run [--controller URL] [--max-concurrency N]    Start the TUI");
    println!("  import -i <clash.yml> [-o <config.json>] [--config FILE] [--replace-nodes]");
    println!(
        "                                                Import Clash YAML into a full sing-box config"
    );
    println!(
        "  benchmark [--selector NAME] [--match TEXT] [--max-concurrency N] [--switch] [--verify] [--verify-discord]"
    );
    println!(
        "                                                Benchmark selector candidates and optionally switch"
    );
}

fn print_run_usage() {
    println!("sing-box-tui run [--controller URL] [--max-concurrency N]");
    println!();
    println!(
        "      --max-concurrency <N>   Limit concurrent delay probes in TUI benchmarks (default: {DEFAULT_BENCHMARK_MAX_CONCURRENCY})"
    );
}

fn print_import_usage() {
    println!(
        "sing-box-tui import -i <clash.yml> [-o <config.json>] [--config FILE] [--replace-nodes]"
    );
    println!();
    println!("Input options:");
    println!("  -i, --input <FILE>        Input Clash YAML subscription/config file");
    println!(
        "      --config <FILE>       Existing sing-box config to merge into (default: /etc/sing-box/config.json)"
    );
    println!();
    println!("Output options:");
    println!("  -o, --output <FILE>       Output full sing-box config JSON");
    println!();
    println!("Behavior options:");
    println!("      --replace-nodes       Replace existing node outbounds instead of merging");
}

fn print_benchmark_usage() {
    println!("sing-box-tui benchmark [options]");
    println!();
    println!("Options:");
    println!("      --controller <URL>        Clash controller base URL");
    println!("      --selector <NAME>         Selector group to benchmark (default: select)");
    println!("      --match <TEXT>            Substring filter for candidate tags (default: empty)");
    println!("      --url <URL>               Delay test URL (default: {DEFAULT_DELAY_TEST_URL})");
    println!("      --timeout-ms <MS>         Delay probe timeout in ms (default: 5000)");
    println!("      --request-timeout <SEC>   HTTP request timeout in seconds (default: 12)");
    println!(
        "      --max-concurrency <N>     Limit concurrent delay probes (default: {DEFAULT_BENCHMARK_MAX_CONCURRENCY})"
    );
    println!("      --switch                  Switch selector to the best successful node");
    println!("      --verify                  Run post-switch verification HTTP checks");
    println!("      --verify-discord          Include Discord checks during verification");
}

fn run_import(
    source: &PathBuf,
    output: Option<&PathBuf>,
    full_config: bool,
    config_path: &PathBuf,
    replace_nodes: bool,
) -> Result<()> {
    let text = fs::read_to_string(source)
        .with_context(|| format!("failed to read Clash proxy file {}", source.display()))?;
    let config: ClashConfig = serde_yaml::from_str(&text).context("failed to parse Clash YAML")?;

    let converted = config
        .proxies
        .into_iter()
        .filter(|entry| !is_metadata_entry(entry))
        .map(convert_clash_proxy)
        .collect::<Result<Vec<_>>>()?;

    let output_value = if full_config {
        build_full_config(config_path, converted, replace_nodes)?
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

fn run_benchmark(options: BenchmarkOptions) -> Result<()> {
    let controller = options
        .controller
        .or_else(|| env::var("SING_BOX_CONTROLLER").ok())
        .unwrap_or_else(|| DEFAULT_CONTROLLER.to_string());
    let secret = env::var("SING_BOX_SECRET")
        .ok()
        .filter(|value| !value.is_empty());

    let client = ApiClient::new(controller, secret)?;
    let summary = client.benchmark_selector(&BenchmarkRequest {
        selector: options.selector,
        pattern: options.pattern,
        url: options.url,
        timeout_ms: options.timeout_ms,
        request_timeout: options.request_timeout,
        max_concurrency: options.max_concurrency,
        nodes: None,
    })?;

    let mut final_node = summary.current.clone();
    let mut switched = false;
    if options.switch {
        if let Some(best) = summary.best_success() {
            client.switch_proxy(&summary.selector, &best.name)?;
            final_node = Some(best.name.clone());
            switched = true;
        }
    }

    let verification = if options.verify {
        Some(run_verification(options.verify_discord))
    } else {
        None
    };
    let best = summary.best_success().cloned();

    println!(
        "{}",
        serde_json::to_string_pretty(&BenchmarkOutput {
            selector: summary.selector,
            current: summary.current,
            pattern: summary.pattern,
            test_url: summary.url,
            timeout_ms: summary.timeout_ms,
            max_concurrency: summary.max_concurrency,
            results: summary.results,
            best,
            switched,
            final_node,
            verification,
        })?
    );
    Ok(())
}

fn build_full_config(
    config_path: &PathBuf,
    imported_nodes: Vec<Value>,
    replace_nodes: bool,
) -> Result<Value> {
    if config_path.exists() {
        let text = fs::read_to_string(config_path)
            .with_context(|| format!("failed to read {}", config_path.display()))?;
        let mut config: Value = serde_json::from_str(&text)
            .with_context(|| format!("failed to parse {}", config_path.display()))?;
        merge_into_existing_config(&mut config, imported_nodes, replace_nodes)?;
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
        "url": DEFAULT_DELAY_TEST_URL,
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

fn merge_into_existing_config(
    config: &mut Value,
    imported_nodes: Vec<Value>,
    replace_nodes: bool,
) -> Result<()> {
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

    if replace_nodes {
        outbounds.retain(|outbound| !is_replaceable_node_outbound(outbound));
    }

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
            if replace_nodes {
                set_outbound_members(value, &select_members);
            } else {
                merge_outbound_members(value, &select_members);
            }
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
                "url": DEFAULT_DELAY_TEST_URL,
                "interval": "10m",
            })
        },
        |value| {
            if replace_nodes {
                set_outbound_members(value, &node_tags);
            } else {
                merge_outbound_members(value, &node_tags);
            }
            ensure_string_field(value, "type", "urltest");
            ensure_string_field(value, "url", DEFAULT_DELAY_TEST_URL);
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
    clash_api
        .entry("external_controller")
        .or_insert_with(|| Value::String("127.0.0.1:9090".to_string()));
    clash_api
        .entry("secret")
        .or_insert_with(|| Value::String(String::new()));

    Ok(())
}

fn is_replaceable_node_outbound(outbound: &Value) -> bool {
    let outbound_type = outbound
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    !matches!(
        outbound_type,
        "selector" | "urltest" | "direct" | "block" | "dns"
    )
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

fn set_outbound_members(outbound: &mut Value, members: &[String]) {
    let Some(object) = outbound.as_object_mut() else {
        return;
    };
    object.insert(
        "outbounds".to_string(),
        Value::Array(
            members
                .iter()
                .cloned()
                .map(Value::String)
                .collect::<Vec<_>>(),
        ),
    );
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
        if let Some(member) = value.as_str() {
            if merged.insert(member.to_string()) {
                values.push(Value::String(member.to_string()));
            }
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
        "vless" => Ok(json!({
            "type": "vless",
            "tag": entry.name,
            "server": entry.server,
            "server_port": entry.port,
            "uuid": entry.uuid,
            "flow": entry.flow,
            "tls": build_vless_tls(&entry),
            "transport": build_transport(&entry),
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

fn build_vless_tls(entry: &ClashProxy) -> Value {
    let mut tls = build_tls(entry);
    if tls.is_null() {
        tls = json!({ "enabled": true });
    }

    if let Some(fingerprint) = entry.client_fingerprint.as_ref() {
        tls["utls"] = json!({
            "enabled": true,
            "fingerprint": fingerprint,
        });
    }

    if let Some(reality) = entry.reality_opts.as_ref() {
        tls["reality"] = json!({
            "enabled": true,
            "public_key": reality.public_key,
            "short_id": reality.short_id.clone().unwrap_or_default(),
        });
    }

    tls
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
    #[serde(default)]
    flow: Option<String>,
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
    #[serde(rename = "reality-opts", default)]
    reality_opts: Option<ClashRealityOpts>,
    #[serde(rename = "client-fingerprint", default)]
    client_fingerprint: Option<String>,
}

#[derive(Deserialize)]
struct ClashRealityOpts {
    #[serde(rename = "public-key", default)]
    public_key: Option<String>,
    #[serde(rename = "short-id", default)]
    short_id: Option<String>,
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
        Line::from(app.status_line()),
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

    fn displayed_members(&self) -> Vec<String> {
        let Some(group) = self.selected_group() else {
            return Vec::new();
        };
        let Some(summary) = self.selected_benchmark() else {
            return group.members.clone();
        };
        if !self.latency_sort_mode {
            return group.members.clone();
        }

        let mut successes = Vec::new();
        let mut pending_or_untested = Vec::new();
        for (index, member) in group.members.iter().enumerate() {
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
        if let Some(group) = self.selected_group() {
            if let Some(index) = group.members.iter().position(|member| member == name) {
                self.member_index = index;
            }
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
            KeyCode::Char('/') => self.prompt_benchmark_filter()?,
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
        let Some(member) = group.members.get(self.member_index).cloned() else {
            bail!("no proxy available in selected group");
        };
        self.client
            .switch_proxy(&group.name, &member)
            .with_context(|| format!("failed to switch {} to {}", group.name, member))?;
        self.set_status_with_flash(format!("Switched {} to {}", group.name, member));
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

    fn start_group_benchmark(&mut self) -> Result<()> {
        let Some(group) = self.selected_group().cloned() else {
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
        if let Some((last_group, last_member, last_started)) = &self.last_single_node_benchmark {
            if last_group == &group.name
                && last_member == &member
                && last_started.elapsed() < SINGLE_NODE_RETEST_DEBOUNCE
            {
                self.set_status_only(format!(
                    "Ignoring repeated retest for {} / {} (debounced)",
                    group.name, member
                ));
                return Ok(());
            }
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
                        match &self.benchmark_jobs[index].kind {
                            BenchmarkJobKind::Group => self.set_status_only(format!(
                                "Benchmark worker for {} disconnected",
                                group
                            )),
                            BenchmarkJobKind::SingleNode { .. } => self.set_status_only(format!(
                                "Benchmark worker for {} disconnected",
                                group
                            )),
                        }
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
        let summary = report.summary_line();
        self.set_status_with_flash(summary);
        Ok(())
    }

    fn prompt_benchmark_filter(&mut self) -> Result<()> {
        restore_terminal()?;
        println!(
            "Enter benchmark substring filter (current: {}): ",
            self.benchmark_filter
        );
        let mut buffer = String::new();
        io::stdin()
            .read_line(&mut buffer)
            .context("failed to read benchmark filter from stdin")?;
        let value = buffer.trim();
        setup_terminal()?;
        if !value.is_empty() {
            self.benchmark_filter = value.to_string();
            self.set_status_with_flash(format!(
                "Benchmark filter set to '{}'",
                self.benchmark_filter
            ));
        }
        Ok(())
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
    runtime: TokioRuntime,
    client: AsyncClient,
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
        let runtime = TokioRuntimeBuilder::new_multi_thread()
            .enable_all()
            .build()
            .context("failed to build Tokio runtime for API client")?;
        let client = AsyncClient::builder()
            .default_headers(headers)
            .build()
            .context("failed to build async HTTP client")?;

        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            runtime,
            client,
        })
    }

    fn fetch_selector_groups(&self) -> Result<Vec<ProxyGroup>> {
        self.runtime.block_on(self.fetch_selector_groups_async())
    }

    async fn fetch_selector_groups_async(&self) -> Result<Vec<ProxyGroup>> {
        let payload: ProxiesResponse = self
            .client
            .get(format!("{}/proxies", self.base_url))
            .send()
            .await
            .context("failed to query Clash API /proxies")?
            .error_for_status()
            .context("Clash API /proxies returned an error")?
            .json()
            .await
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

    async fn fetch_selector_async(&self, selector: &str) -> Result<ProxyNode> {
        let encoded = encode(selector);
        self.client
            .get(format!("{}/proxies/{}", self.base_url, encoded))
            .send()
            .await
            .with_context(|| format!("failed to query Clash API selector {selector}"))?
            .error_for_status()
            .with_context(|| format!("Clash API rejected selector read for {selector}"))?
            .json()
            .await
            .context("failed to decode Clash API selector response")
    }

    fn benchmark_selector(&self, request: &BenchmarkRequest) -> Result<BenchmarkSummary> {
        self.runtime
            .block_on(self.benchmark_selector_async(request))
    }

    fn fetch_benchmark_candidates(&self, request: &BenchmarkRequest) -> Result<Vec<String>> {
        self.runtime
            .block_on(self.fetch_benchmark_candidates_async(request))
    }

    async fn fetch_benchmark_candidates_async(
        &self,
        request: &BenchmarkRequest,
    ) -> Result<Vec<String>> {
        let selector = self.fetch_selector_async(&request.selector).await?;
        Ok(filter_benchmark_candidates(&selector.all, request))
    }

    async fn benchmark_selector_async(
        &self,
        request: &BenchmarkRequest,
    ) -> Result<BenchmarkSummary> {
        let selector = self.fetch_selector_async(&request.selector).await?;
        let current = selector.now;
        let candidates = filter_benchmark_candidates(&selector.all, request);

        if candidates.is_empty() {
            return Ok(BenchmarkSummary {
                selector: request.selector.clone(),
                current,
                pattern: request.pattern.clone(),
                url: request.url.clone(),
                timeout_ms: request.timeout_ms,
                max_concurrency: request.max_concurrency,
                results: Vec::new(),
            });
        }

        let base_url = self.base_url.clone();
        let client = self.client.clone();
        let url = request.url.clone();
        let timeout_ms = request.timeout_ms;
        let request_timeout = request.request_timeout;

        let max_concurrency = request.max_concurrency.max(1);
        let mut results = {
            let mut tasks = JoinSet::new();
            let mut pending = candidates.into_iter();

            for _ in 0..max_concurrency {
                let Some(name) = pending.next() else {
                    break;
                };
                spawn_benchmark_task(
                    &mut tasks,
                    client.clone(),
                    base_url.clone(),
                    name,
                    url.clone(),
                    timeout_ms,
                    request_timeout,
                );
            }

            let mut results = Vec::new();
            while let Some(result) = tasks.join_next().await {
                results.push(result.expect("benchmark worker panicked"));
                if let Some(name) = pending.next() {
                    spawn_benchmark_task(
                        &mut tasks,
                        client.clone(),
                        base_url.clone(),
                        name,
                        url.clone(),
                        timeout_ms,
                        request_timeout,
                    );
                }
            }
            results
        };
        results.sort_by_key(|item| (item.delay.is_none(), item.delay.unwrap_or(u64::MAX)));

        Ok(BenchmarkSummary {
            selector: request.selector.clone(),
            current,
            pattern: request.pattern.clone(),
            url: request.url.clone(),
            timeout_ms: request.timeout_ms,
            max_concurrency,
            results,
        })
    }

    fn switch_proxy(&self, group: &str, proxy: &str) -> Result<()> {
        self.runtime.block_on(self.switch_proxy_async(group, proxy))
    }

    async fn switch_proxy_async(&self, group: &str, proxy: &str) -> Result<()> {
        let encoded_group = encode(group);
        self.client
            .put(format!("{}/proxies/{}", self.base_url, encoded_group))
            .json(&SwitchProxyRequest {
                name: proxy.to_string(),
            })
            .send()
            .await
            .with_context(|| format!("failed to send switch request for {group}"))?
            .error_for_status()
            .with_context(|| format!("controller rejected switch request for {group}"))?;
        Ok(())
    }
}

fn filter_benchmark_candidates(all: &[String], request: &BenchmarkRequest) -> Vec<String> {
    if let Some(nodes) = &request.nodes {
        let wanted = nodes.iter().collect::<std::collections::BTreeSet<_>>();
        all.iter()
            .filter(|name| wanted.contains(name))
            .cloned()
            .collect()
    } else {
        all.iter()
            .filter(|name| name.contains(&request.pattern))
            .cloned()
            .collect()
    }
}

fn spawn_benchmark_worker(
    base_url: String,
    client: AsyncClient,
    request: BenchmarkRequest,
    tx: Sender<BenchmarkEvent>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let runtime = match TokioRuntimeBuilder::new_multi_thread().enable_all().build() {
            Ok(runtime) => runtime,
            Err(_) => {
                let _ = tx.send(BenchmarkEvent::Finished);
                return;
            }
        };

        runtime.block_on(async move {
            let selector_name = request.selector.clone();
            let selector =
                match fetch_selector_for_benchmark(&client, &base_url, &selector_name).await {
                    Ok(selector) => selector,
                    Err(_) => {
                        let _ = tx.send(BenchmarkEvent::Finished);
                        return;
                    }
                };

            let candidates = filter_benchmark_candidates(&selector.all, &request);

            let max_concurrency = request.max_concurrency.max(1);
            let mut tasks = JoinSet::new();
            let mut pending = candidates.into_iter();

            for _ in 0..max_concurrency {
                let Some(name) = pending.next() else {
                    break;
                };
                spawn_benchmark_task(
                    &mut tasks,
                    client.clone(),
                    base_url.clone(),
                    name,
                    request.url.clone(),
                    request.timeout_ms,
                    request.request_timeout,
                );
            }

            while let Some(result) = tasks.join_next().await {
                if let Ok(result) = result {
                    let _ = tx.send(BenchmarkEvent::Progress(result));
                }
                if let Some(name) = pending.next() {
                    spawn_benchmark_task(
                        &mut tasks,
                        client.clone(),
                        base_url.clone(),
                        name,
                        request.url.clone(),
                        request.timeout_ms,
                        request.request_timeout,
                    );
                }
            }

            let _ = tx.send(BenchmarkEvent::Finished);
        });
    })
}

async fn fetch_selector_for_benchmark(
    client: &AsyncClient,
    base_url: &str,
    selector: &str,
) -> Result<ProxyNode> {
    let encoded = encode(selector);
    client
        .get(format!("{}/proxies/{}", base_url, encoded))
        .send()
        .await
        .with_context(|| format!("failed to query Clash API selector {selector}"))?
        .error_for_status()
        .with_context(|| format!("Clash API rejected selector read for {selector}"))?
        .json()
        .await
        .context("failed to decode Clash API selector response")
}

fn spawn_benchmark_task(
    tasks: &mut JoinSet<BenchmarkResult>,
    client: AsyncClient,
    base_url: String,
    name: String,
    url: String,
    timeout_ms: u64,
    request_timeout: f64,
) {
    tasks.spawn(async move {
        let delay = measure_delay(
            client,
            base_url,
            name.clone(),
            url,
            timeout_ms,
            request_timeout,
        )
        .await;
        BenchmarkResult {
            name,
            delay,
            completed: true,
        }
    });
}

async fn measure_delay(
    client: AsyncClient,
    base_url: String,
    proxy_name: String,
    url: String,
    timeout_ms: u64,
    request_timeout: f64,
) -> Option<u64> {
    let encoded_name = encode(&proxy_name);
    let encoded_url = encode(&url);
    let response = client
        .get(format!(
            "{}/proxies/{}/delay?timeout={}&url={}",
            base_url, encoded_name, timeout_ms, encoded_url
        ))
        .timeout(Duration::from_secs_f64(request_timeout))
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?;
    let payload: DelayResponse = response.json().await.ok()?;
    payload.delay
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

#[derive(Deserialize)]
struct DelayResponse {
    #[serde(default)]
    delay: Option<u64>,
}

#[derive(Serialize)]
struct SwitchProxyRequest {
    name: String,
}

struct BenchmarkOptions {
    controller: Option<String>,
    selector: String,
    pattern: String,
    url: String,
    timeout_ms: u64,
    request_timeout: f64,
    max_concurrency: usize,
    switch: bool,
    verify: bool,
    verify_discord: bool,
}

struct BenchmarkRequest {
    selector: String,
    pattern: String,
    url: String,
    timeout_ms: u64,
    request_timeout: f64,
    max_concurrency: usize,
    nodes: Option<Vec<String>>,
}

#[derive(Clone, Debug, Serialize)]
struct BenchmarkResult {
    name: String,
    delay: Option<u64>,
    #[serde(skip)]
    completed: bool,
}

impl BenchmarkResult {
    fn display_delay(&self) -> String {
        match (self.delay, self.completed) {
            (Some(delay), _) => format!("{delay}ms"),
            (None, false) => "...".to_string(),
            (None, true) => "fail".to_string(),
        }
    }
}

#[derive(Clone, Debug)]
struct BenchmarkSummary {
    selector: String,
    current: Option<String>,
    pattern: String,
    url: String,
    timeout_ms: u64,
    max_concurrency: usize,
    results: Vec<BenchmarkResult>,
}

#[derive(Clone)]
enum BenchmarkJobKind {
    Group,
    SingleNode { node: String },
}

struct BenchmarkJob {
    group: String,
    nodes: Vec<String>,
    kind: BenchmarkJobKind,
    receiver: Receiver<BenchmarkEvent>,
    worker: JoinHandle<()>,
}

enum BenchmarkEvent {
    Progress(BenchmarkResult),
    Finished,
}

impl BenchmarkSummary {
    fn empty(selector: String) -> Self {
        Self {
            selector,
            current: None,
            pattern: String::new(),
            url: String::new(),
            timeout_ms: 0,
            max_concurrency: 1,
            results: Vec::new(),
        }
    }

    fn upsert_pending(&mut self, name: String) {
        if let Some(existing) = self.results.iter_mut().find(|item| item.name == name) {
            existing.delay = None;
            existing.completed = false;
        } else {
            self.results.push(BenchmarkResult {
                name,
                delay: None,
                completed: false,
            });
        }
    }

    fn update_result(&mut self, result: BenchmarkResult) {
        if let Some(existing) = self
            .results
            .iter_mut()
            .find(|item| item.name == result.name)
        {
            *existing = result;
        } else {
            self.results.push(result);
        }
    }

    fn best_label(&self) -> String {
        self.best_success()
            .map(|item| format!("{} ({})", item.name, item.display_delay()))
            .unwrap_or_else(|| "pending".to_string())
    }

    fn best_success(&self) -> Option<&BenchmarkResult> {
        self.results
            .iter()
            .filter(|item| item.completed)
            .filter_map(|item| item.delay.map(|delay| (item, delay)))
            .min_by_key(|(_, delay)| *delay)
            .map(|(item, _)| item)
    }

    fn find_result(&self, name: &str) -> Option<&BenchmarkResult> {
        self.results.iter().find(|item| item.name == name)
    }
}

#[derive(Serialize)]
struct BenchmarkOutput {
    selector: String,
    current: Option<String>,
    pattern: String,
    test_url: String,
    timeout_ms: u64,
    max_concurrency: usize,
    results: Vec<BenchmarkResult>,
    best: Option<BenchmarkResult>,
    switched: bool,
    final_node: Option<String>,
    verification: Option<VerificationReport>,
}

#[derive(Clone, Debug, Serialize)]
struct ShellCheck {
    code: i32,
    stdout: String,
    stderr: String,
}

impl ShellCheck {
    fn ok(&self) -> bool {
        self.code == 0
    }
}

#[derive(Clone, Debug, Serialize)]
struct VerificationReport {
    google_v4: ShellCheck,
    github: ShellCheck,
    discord_gateway_rest: Option<ShellCheck>,
    discord_gateway_logs: Option<ShellCheck>,
}

impl VerificationReport {
    fn summary_line(&self) -> String {
        let mut parts = vec![
            format!("google={}", if self.google_v4.ok() { "ok" } else { "fail" }),
            format!("github={}", if self.github.ok() { "ok" } else { "fail" }),
        ];
        if let Some(rest) = &self.discord_gateway_rest {
            parts.push(format!(
                "discord_rest={}",
                if rest.ok() { "ok" } else { "fail" }
            ));
        }
        if let Some(logs) = &self.discord_gateway_logs {
            parts.push(format!(
                "discord_logs={}",
                if logs.ok() { "hits" } else { "clean/none" }
            ));
        }
        format!("Verification: {}", parts.join("  "))
    }
}

fn run_verification(include_discord: bool) -> VerificationReport {
    let google_v4 =
        run_http_verification("https://www.google.com", true, 5, Some(("accept", "*/*")));
    let github = run_http_verification("https://github.com", false, 5, Some(("accept", "*/*")));
    let discord_gateway_rest = include_discord.then(|| {
        run_http_verification(
            "https://discord.com/api/v10/gateway",
            false,
            8,
            Some(("accept", "application/json")),
        )
    });
    let discord_gateway_logs = include_discord.then(run_journalctl_verification);

    VerificationReport {
        google_v4,
        github,
        discord_gateway_rest,
        discord_gateway_logs,
    }
}

fn run_http_verification(
    url: &str,
    force_ipv4: bool,
    max_lines: usize,
    extra_header: Option<(&str, &str)>,
) -> ShellCheck {
    let runtime = match TokioRuntimeBuilder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            return ShellCheck {
                code: -1,
                stdout: String::new(),
                stderr: format!("failed to build Tokio runtime for verification: {error}"),
            };
        }
    };

    runtime.block_on(async move {
        let mut builder = AsyncClient::builder()
            .no_proxy()
            .redirect(Policy::none())
            .timeout(Duration::from_secs(12))
            .user_agent("sing-box-tui/0.1 verification");
        if force_ipv4 {
            builder = builder.local_address(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));
        }
        let client = match builder.build() {
            Ok(client) => client,
            Err(error) => {
                return ShellCheck {
                    code: -1,
                    stdout: String::new(),
                    stderr: format!("failed to build verification HTTP client: {error}"),
                };
            }
        };

        let mut request = client.head(url);
        if let Some((name, value)) = extra_header {
            request = request.header(name, value);
        }

        match request.send().await {
            Ok(response) => ShellCheck {
                code: if response.status().is_success() { 0 } else { 1 },
                stdout: format_response_head(&response, max_lines),
                stderr: String::new(),
            },
            Err(error) => ShellCheck {
                code: 1,
                stdout: String::new(),
                stderr: error.to_string(),
            },
        }
    })
}

fn format_response_head(response: &reqwest::Response, max_lines: usize) -> String {
    let mut lines = Vec::new();
    lines.push(format!(
        "{} {} {}",
        format_http_version(response.version()),
        response.status().as_u16(),
        response.status().canonical_reason().unwrap_or("")
    ));
    for (name, value) in response.headers() {
        let value = value.to_str().unwrap_or("<binary>");
        lines.push(format!("{}: {}", name.as_str(), value));
    }
    lines
        .into_iter()
        .take(max_lines)
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_http_version(version: Version) -> &'static str {
    match version {
        Version::HTTP_09 => "HTTP/0.9",
        Version::HTTP_10 => "HTTP/1.0",
        Version::HTTP_11 => "HTTP/1.1",
        Version::HTTP_2 => "HTTP/2.0",
        Version::HTTP_3 => "HTTP/3.0",
        _ => "HTTP/?",
    }
}

fn run_journalctl_verification() -> ShellCheck {
    let runtime = match TokioRuntimeBuilder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            return ShellCheck {
                code: -1,
                stdout: String::new(),
                stderr: format!(
                    "failed to build Tokio runtime for journalctl verification: {error}"
                ),
            };
        }
    };

    runtime.block_on(async move {
        match TokioCommand::new("journalctl")
            .args([
                "--user",
                "-u",
                "openclaw-gateway",
                "--since",
                "5 min ago",
                "--no-pager",
                "-l",
            ])
            .output()
            .await
        {
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                if !output.status.success() {
                    return ShellCheck {
                        code: output.status.code().unwrap_or(-1),
                        stdout: String::new(),
                        stderr,
                    };
                }

                let patterns = [
                    "discord",
                    "1006",
                    "econnreset",
                    "fetch failed",
                    "gateway error",
                ];
                let lines = String::from_utf8_lossy(&output.stdout);
                let matched = lines
                    .lines()
                    .filter(|line| {
                        let lower = line.to_ascii_lowercase();
                        patterns.iter().any(|pattern| lower.contains(pattern))
                    })
                    .map(str::to_string)
                    .collect::<Vec<_>>();
                let start = matched.len().saturating_sub(40);

                ShellCheck {
                    code: if matched.is_empty() { 1 } else { 0 },
                    stdout: matched[start..].join("\n"),
                    stderr,
                }
            }
            Err(error) => ShellCheck {
                code: -1,
                stdout: String::new(),
                stderr: error.to_string(),
            },
        }
    })
}

#[cfg(test)]
mod tests {
    use super::{
        ApiClient, App, BenchmarkEvent, BenchmarkJob, BenchmarkJobKind, BenchmarkOutput,
        BenchmarkRequest, BenchmarkResult, BenchmarkSummary, CliCommand,
        DEFAULT_BENCHMARK_MAX_CONCURRENCY, Focus, ProxyGroup, build_default_config,
        merge_into_existing_config, truncate_for_width,
    };
    use reqwest::Client as AsyncClient;
    use serde_json::{Value, json};
    use std::collections::BTreeMap;
    use std::sync::mpsc;
    use std::thread;
    use tokio::runtime::Builder as TokioRuntimeBuilder;

    fn test_app() -> App {
        let runtime = TokioRuntimeBuilder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let client = AsyncClient::builder().build().expect("test HTTP client");

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
        }
    }

    #[test]
    fn truncates_wide_strings_without_panicking() {
        let truncated = truncate_for_width("手动选择-自动选择-节点A", 8);
        assert!(truncated.ends_with('…'));
        assert!(!truncated.is_empty());
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
            false,
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
    }

    #[test]
    fn replace_nodes_removes_existing_node_outbounds_but_keeps_special_ones() {
        let mut config = json!({
            "outbounds": [{
                "type": "selector",
                "tag": "select",
                "outbounds": ["old-node"],
                "default": "old-node"
            }, {
                "type": "urltest",
                "tag": "auto",
                "outbounds": ["old-node"],
                "url": "https://www.gstatic.com/generate_204",
                "interval": "10m"
            }, {
                "type": "trojan",
                "tag": "old-node",
                "server": "old.example.com",
                "server_port": 443,
                "password": "secret"
            }, {
                "type": "direct",
                "tag": "direct"
            }],
            "route": {
                "final": "select"
            }
        });

        merge_into_existing_config(
            &mut config,
            vec![json!({
                "type": "vless",
                "tag": "new-node",
                "server": "new.example.com",
                "server_port": 443,
                "uuid": "abc"
            })],
            true,
        )
        .expect("replace succeeds");

        let outbounds = config["outbounds"].as_array().expect("outbounds array");
        assert!(!outbounds.iter().any(|value| value["tag"] == "old-node"));
        assert!(outbounds.iter().any(|value| value["tag"] == "new-node"));
        assert!(outbounds.iter().any(|value| value["tag"] == "select"));
        assert!(outbounds.iter().any(|value| value["tag"] == "auto"));
        assert!(outbounds.iter().any(|value| value["tag"] == "direct"));

        let select = outbounds
            .iter()
            .find(|value| value["tag"] == "select")
            .expect("select outbound");
        let members = select["outbounds"].as_array().expect("selector members");
        assert!(!members.contains(&Value::String("old-node".to_string())));
        assert!(members.contains(&Value::String("auto".to_string())));
        assert!(members.contains(&Value::String("new-node".to_string())));
    }

    #[test]
    fn benchmark_summary_picks_lowest_successful_delay() {
        let summary = BenchmarkSummary {
            selector: "select".to_string(),
            current: Some("node-b".to_string()),
            pattern: "美国".to_string(),
            url: "https://www.gstatic.com/generate_204".to_string(),
            timeout_ms: 5000,
            max_concurrency: DEFAULT_BENCHMARK_MAX_CONCURRENCY,
            results: vec![
                BenchmarkResult {
                    name: "node-a".to_string(),
                    delay: Some(100),
                    completed: true,
                },
                BenchmarkResult {
                    name: "node-b".to_string(),
                    delay: Some(80),
                    completed: true,
                },
                BenchmarkResult {
                    name: "node-c".to_string(),
                    delay: None,
                    completed: true,
                },
            ],
        };

        let best = summary.best_success().expect("best result");
        assert_eq!(best.name, "node-b");
        assert_eq!(best.delay, Some(80));
    }

    #[test]
    fn benchmark_command_defaults_max_concurrency() {
        let command = CliCommand::parse([
            "benchmark".to_string(),
            "--selector".to_string(),
            "select".to_string(),
        ])
        .expect("benchmark command parses");

        match command {
            CliCommand::Benchmark {
                max_concurrency,
                pattern,
                ..
            } => {
                assert_eq!(max_concurrency, DEFAULT_BENCHMARK_MAX_CONCURRENCY);
                assert!(pattern.is_empty());
            }
            _ => panic!("expected benchmark command"),
        }
    }

    #[test]
    fn run_command_accepts_max_concurrency() {
        let command = CliCommand::parse([
            "run".to_string(),
            "--max-concurrency".to_string(),
            "7".to_string(),
        ])
        .expect("run command parses");

        match command {
            CliCommand::Run {
                max_concurrency, ..
            } => {
                assert_eq!(max_concurrency, Some(7));
            }
            _ => panic!("expected run command"),
        }
    }

    #[test]
    fn benchmark_output_serializes_max_concurrency() {
        let output = BenchmarkOutput {
            selector: "select".to_string(),
            current: Some("node-a".to_string()),
            pattern: "美国".to_string(),
            test_url: "https://www.gstatic.com/generate_204".to_string(),
            timeout_ms: 5000,
            max_concurrency: 7,
            results: vec![BenchmarkResult {
                name: "node-a".to_string(),
                delay: Some(42),
                completed: true,
            }],
            best: None,
            switched: false,
            final_node: Some("node-a".to_string()),
            verification: None,
        };

        let json = serde_json::to_value(output).expect("serialize benchmark output");
        assert_eq!(json["max_concurrency"], 7);
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
}

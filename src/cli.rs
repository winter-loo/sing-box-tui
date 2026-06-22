use std::env;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::defaults::{
    DEFAULT_BENCHMARK_MAX_CONCURRENCY, DEFAULT_CONFIG_PATH, DEFAULT_CONTROLLER,
    DEFAULT_DELAY_TEST_URL, DEFAULT_SELECTOR_TAG,
};
use crate::subscriptions::{
    DEFAULT_SUBSCRIPTION_CACHE_PATH, DEFAULT_SUBSCRIPTION_INTERVAL_DAYS,
    DEFAULT_SUBSCRIPTION_SOURCE_PATH,
};

#[derive(Debug)]
pub(crate) enum CliCommand {
    Run {
        controller: Option<String>,
        max_concurrency: Option<usize>,
        subscription_input: PathBuf,
        subscription_cache: PathBuf,
        subscription_config_path: PathBuf,
        subscription_refresh_disabled: bool,
        force_subscription_refresh: bool,
        include_geosite_rules: bool,
        include_tun_mode: bool,
        subscription_interval_days: u64,
    },
    Selectors {
        controller: Option<String>,
        selector: Option<String>,
    },
    Status {
        controller: Option<String>,
    },
    Import {
        input: PathBuf,
        output: Option<PathBuf>,
        config_path: PathBuf,
        replace_nodes: bool,
        include_geosite_rules: bool,
        include_tun_mode: bool,
    },
    Subscribe {
        url: String,
        output: Option<PathBuf>,
        config_path: PathBuf,
        subscription_output: Option<PathBuf>,
        replace_nodes: bool,
        include_geosite_rules: bool,
        include_tun_mode: bool,
        provider_name: Option<String>,
        existing_provider_name: Option<String>,
    },
    Subscriptions {
        input: PathBuf,
        cache: PathBuf,
        output: Option<PathBuf>,
        config_path: PathBuf,
        replace_nodes: bool,
        include_geosite_rules: bool,
        include_tun_mode: bool,
        write: bool,
        force: bool,
        interval_days: u64,
    },
    SyncProvider {
        provider: String,
        account_file: PathBuf,
        config_path: PathBuf,
        output: Option<PathBuf>,
        subscription_output: Option<PathBuf>,
        replace_nodes: bool,
        include_geosite_rules: bool,
        include_tun_mode: bool,
        write: bool,
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
        verify_urls: Vec<String>,
    },
}

impl CliCommand {
    pub(crate) fn parse<I>(args: I) -> Result<Self>
    where
        I: IntoIterator<Item = String>,
    {
        let args = args.into_iter().collect::<Vec<_>>();
        if args.is_empty() {
            return Ok(Self::Run {
                controller: None,
                max_concurrency: None,
                subscription_input: PathBuf::from(DEFAULT_SUBSCRIPTION_SOURCE_PATH),
                subscription_cache: PathBuf::from(DEFAULT_SUBSCRIPTION_CACHE_PATH),
                subscription_config_path: default_subscription_config_path(),
                subscription_refresh_disabled: false,
                force_subscription_refresh: false,
                include_geosite_rules: false,
                include_tun_mode: false,
                subscription_interval_days: DEFAULT_SUBSCRIPTION_INTERVAL_DAYS,
            });
        }

        match args[0].as_str() {
            "run" => Self::parse_run(&args[1..]),
            "selectors" => Self::parse_selectors(&args[1..]),
            "status" => Self::parse_status(&args[1..]),
            "import" => Self::parse_import(&args[1..]),
            "subscribe" => Self::parse_subscribe(&args[1..]),
            "subscriptions" | "refresh-subscriptions" => Self::parse_subscriptions(&args[1..]),
            "sync" => Self::parse_sync_provider(&args[1..]),
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
        let mut subscription_input = PathBuf::from(DEFAULT_SUBSCRIPTION_SOURCE_PATH);
        let mut subscription_cache = PathBuf::from(DEFAULT_SUBSCRIPTION_CACHE_PATH);
        let mut subscription_config_path = default_subscription_config_path();
        let mut subscription_refresh_disabled = false;
        let mut force_subscription_refresh = false;
        let mut include_geosite_rules = false;
        let mut include_tun_mode = false;
        let mut subscription_interval_days = DEFAULT_SUBSCRIPTION_INTERVAL_DAYS;
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
                "--config" | "--subscription-config" => {
                    i += 1;
                    subscription_config_path = PathBuf::from(
                        args.get(i)
                            .context("--config/--subscription-config requires a file path")?,
                    );
                }
                "--subscription-input" => {
                    i += 1;
                    subscription_input = PathBuf::from(
                        args.get(i)
                            .context("--subscription-input requires a file path")?,
                    );
                }
                "--subscription-cache" => {
                    i += 1;
                    subscription_cache = PathBuf::from(
                        args.get(i)
                            .context("--subscription-cache requires a file path")?,
                    );
                }
                "--subscription-interval-days" => {
                    i += 1;
                    subscription_interval_days = args
                        .get(i)
                        .context("--subscription-interval-days requires a value")?
                        .parse()
                        .context("--subscription-interval-days must be a positive integer")?;
                    if subscription_interval_days == 0 {
                        bail!("--subscription-interval-days must be greater than 0");
                    }
                }
                "--force-subscription-refresh" => {
                    force_subscription_refresh = true;
                }
                "--include-geosite-rules" => {
                    include_geosite_rules = true;
                }
                "--include-tun-mode" => {
                    include_tun_mode = true;
                }
                "--no-subscription-refresh" => {
                    subscription_refresh_disabled = true;
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
            subscription_input,
            subscription_cache,
            subscription_config_path,
            subscription_refresh_disabled,
            force_subscription_refresh,
            include_geosite_rules,
            include_tun_mode,
            subscription_interval_days,
        })
    }

    fn parse_selectors(args: &[String]) -> Result<Self> {
        let mut controller = None;
        let mut selector = None;
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
                    let value = args.get(i).context("--selector requires a value")?;
                    selector = Some(value.clone());
                }
                "--help" | "-h" => {
                    print_selectors_usage();
                    std::process::exit(0);
                }
                value if value.starts_with('-') => bail!("unknown flag for selectors: {value}"),
                value => bail!("unexpected positional argument for selectors: {value}"),
            }
            i += 1;
        }
        Ok(Self::Selectors {
            controller,
            selector,
        })
    }

    fn parse_status(args: &[String]) -> Result<Self> {
        let mut controller = None;
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--controller" => {
                    i += 1;
                    let value = args.get(i).context("--controller requires a value")?;
                    controller = Some(value.clone());
                }
                "--help" | "-h" => {
                    print_status_usage();
                    std::process::exit(0);
                }
                value if value.starts_with('-') => bail!("unknown flag for status: {value}"),
                value => bail!("unexpected positional argument for status: {value}"),
            }
            i += 1;
        }
        Ok(Self::Status { controller })
    }

    fn parse_import(args: &[String]) -> Result<Self> {
        let mut input = None;
        let mut output = None;
        let mut config_path = PathBuf::from(DEFAULT_CONFIG_PATH);
        let mut replace_nodes = false;
        let mut include_geosite_rules = false;
        let mut include_tun_mode = false;
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
                "--include-geosite-rules" => {
                    include_geosite_rules = true;
                }
                "--include-tun-mode" => {
                    include_tun_mode = true;
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
            include_geosite_rules,
            include_tun_mode,
        })
    }

    fn parse_subscribe(args: &[String]) -> Result<Self> {
        let mut url = None;
        let mut output = None;
        let mut config_path = PathBuf::from(DEFAULT_CONFIG_PATH);
        let mut subscription_output = None;
        let mut replace_nodes = false;
        let mut include_geosite_rules = false;
        let mut include_tun_mode = false;
        let mut provider_name = None;
        let mut existing_provider_name = None;
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--url" => {
                    i += 1;
                    url = Some(args.get(i).context("--url requires a value")?.clone());
                }
                "-o" | "--output" => {
                    i += 1;
                    output = Some(PathBuf::from(
                        args.get(i).context("-o/--output requires a file path")?,
                    ));
                }
                "--config" => {
                    i += 1;
                    config_path =
                        PathBuf::from(args.get(i).context("--config requires a file path")?);
                }
                "--subscription-output" => {
                    i += 1;
                    subscription_output = Some(PathBuf::from(
                        args.get(i)
                            .context("--subscription-output requires a file path")?,
                    ));
                }
                "--provider-name" => {
                    i += 1;
                    provider_name = Some(
                        args.get(i)
                            .context("--provider-name requires a value")?
                            .clone(),
                    );
                }
                "--existing-provider-name" => {
                    i += 1;
                    existing_provider_name = Some(
                        args.get(i)
                            .context("--existing-provider-name requires a value")?
                            .clone(),
                    );
                }
                "--replace-nodes" => {
                    replace_nodes = true;
                }
                "--include-geosite-rules" => {
                    include_geosite_rules = true;
                }
                "--include-tun-mode" => {
                    include_tun_mode = true;
                }
                "--help" | "-h" => {
                    print_subscribe_usage();
                    std::process::exit(0);
                }
                value if value.starts_with('-') => bail!("unknown flag for subscribe: {value}"),
                value => {
                    if url.is_none() {
                        url = Some(value.to_string());
                    } else {
                        bail!("unexpected positional argument for subscribe: {value}");
                    }
                }
            }
            i += 1;
        }

        Ok(Self::Subscribe {
            url: url.context("subscribe requires --url <URL>")?,
            output,
            config_path,
            subscription_output,
            replace_nodes,
            include_geosite_rules,
            include_tun_mode,
            provider_name,
            existing_provider_name,
        })
    }

    fn parse_subscriptions(args: &[String]) -> Result<Self> {
        let mut input = PathBuf::from(DEFAULT_SUBSCRIPTION_SOURCE_PATH);
        let mut cache = PathBuf::from(DEFAULT_SUBSCRIPTION_CACHE_PATH);
        let mut output = None;
        let mut config_path = PathBuf::from(DEFAULT_CONFIG_PATH);
        let mut replace_nodes = false;
        let mut include_geosite_rules = false;
        let mut include_tun_mode = false;
        let mut write = false;
        let mut force = false;
        let mut interval_days = DEFAULT_SUBSCRIPTION_INTERVAL_DAYS;
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "-i" | "--input" => {
                    i += 1;
                    input = PathBuf::from(args.get(i).context("-i/--input requires a file path")?);
                }
                "--cache" => {
                    i += 1;
                    cache = PathBuf::from(args.get(i).context("--cache requires a file path")?);
                }
                "-o" | "--output" => {
                    i += 1;
                    output = Some(PathBuf::from(
                        args.get(i).context("-o/--output requires a file path")?,
                    ));
                }
                "--config" => {
                    i += 1;
                    config_path =
                        PathBuf::from(args.get(i).context("--config requires a file path")?);
                }
                "--replace-nodes" => {
                    replace_nodes = true;
                }
                "--include-geosite-rules" => {
                    include_geosite_rules = true;
                }
                "--include-tun-mode" => {
                    include_tun_mode = true;
                }
                "--write" => {
                    write = true;
                }
                "--force" => {
                    force = true;
                }
                "--interval-days" => {
                    i += 1;
                    interval_days = args
                        .get(i)
                        .context("--interval-days requires a value")?
                        .parse()
                        .context("--interval-days must be a positive integer")?;
                    if interval_days == 0 {
                        bail!("--interval-days must be greater than 0");
                    }
                }
                "--help" | "-h" => {
                    print_subscriptions_usage();
                    std::process::exit(0);
                }
                value if value.starts_with('-') => {
                    bail!("unknown flag for subscriptions: {value}")
                }
                value => bail!("unexpected positional argument for subscriptions: {value}"),
            }
            i += 1;
        }

        if !write && output.is_none() {
            bail!("subscriptions requires either --output <FILE> or --write");
        }

        Ok(Self::Subscriptions {
            input,
            cache,
            output,
            config_path,
            replace_nodes,
            include_geosite_rules,
            include_tun_mode,
            write,
            force,
            interval_days,
        })
    }

    fn parse_sync_provider(args: &[String]) -> Result<Self> {
        let mut provider = None;
        let mut account_file = None;
        let mut config_path = PathBuf::from(DEFAULT_CONFIG_PATH);
        let mut output = None;
        let mut subscription_output = None;
        let mut replace_nodes = false;
        let mut include_geosite_rules = false;
        let mut include_tun_mode = false;
        let mut write = false;
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--provider" => {
                    i += 1;
                    provider = Some(args.get(i).context("--provider requires a value")?.clone());
                }
                "--account-file" => {
                    i += 1;
                    account_file = Some(PathBuf::from(
                        args.get(i).context("--account-file requires a file path")?,
                    ));
                }
                "--config" => {
                    i += 1;
                    let value = args.get(i).context("--config requires a file path")?;
                    config_path = PathBuf::from(value);
                }
                "-o" | "--output" => {
                    i += 1;
                    output = Some(PathBuf::from(
                        args.get(i).context("-o/--output requires a file path")?,
                    ));
                }
                "--subscription-output" => {
                    i += 1;
                    subscription_output = Some(PathBuf::from(
                        args.get(i)
                            .context("--subscription-output requires a file path")?,
                    ));
                }
                "--replace-nodes" => {
                    replace_nodes = true;
                }
                "--include-geosite-rules" => {
                    include_geosite_rules = true;
                }
                "--include-tun-mode" => {
                    include_tun_mode = true;
                }
                "--write" => {
                    write = true;
                }
                "--help" | "-h" => {
                    print_sync_provider_usage();
                    std::process::exit(0);
                }
                value if value.starts_with('-') => {
                    bail!("unknown flag for sync: {value}")
                }
                value => {
                    if provider.is_none() {
                        provider = Some(value.to_string());
                    } else {
                        bail!("unexpected positional argument for sync: {value}");
                    }
                }
            }
            i += 1;
        }

        let provider = provider.context("sync requires --provider <URL>")?;
        let account_file = account_file.context("sync requires --account-file <FILE>")?;
        if !write && output.is_none() {
            bail!("sync requires either --output <FILE> or --write");
        }

        Ok(Self::SyncProvider {
            provider,
            account_file,
            config_path,
            output,
            subscription_output,
            replace_nodes,
            include_geosite_rules,
            include_tun_mode,
            write,
        })
    }

    fn parse_benchmark(args: &[String]) -> Result<Self> {
        let mut controller = None;
        let mut selector = String::from(DEFAULT_SELECTOR_TAG);
        let mut pattern = String::new();
        let mut url = String::from(DEFAULT_DELAY_TEST_URL);
        let mut timeout_ms = 5000_u64;
        let mut request_timeout = 12.0_f64;
        let mut max_concurrency = DEFAULT_BENCHMARK_MAX_CONCURRENCY;
        let mut switch = false;
        let mut verify = false;
        let mut verify_urls = Vec::new();
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
                "--verify-url" => {
                    i += 1;
                    verify_urls.push(
                        args.get(i)
                            .context("--verify-url requires a value")?
                            .clone(),
                    );
                }
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
            verify_urls,
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

fn default_subscription_config_path() -> PathBuf {
    if let Ok(path) = env::var("SING_BOX_CONFIG") {
        return PathBuf::from(path);
    }
    let local_config = PathBuf::from("config.json");
    if cfg!(windows) && local_config.exists() {
        return local_config;
    }
    for path in [
        "/usr/local/etc/sing-box/config.json",
        "/opt/homebrew/etc/sing-box/config.json",
    ] {
        let candidate = PathBuf::from(path);
        if candidate.exists() {
            return candidate;
        }
    }
    PathBuf::from(DEFAULT_CONFIG_PATH)
}

fn print_usage() {
    println!("Usage: sing-box-tui <COMMAND> [OPTIONS]");
    println!();
    println!("Commands:");
    println!("  run             Start the TUI");
    println!("  selectors       Show Clash selector groups");
    println!("  status          Show Clash controller status");
    println!("  import          Import Clash YAML into a full sing-box config");
    println!("  subscribe       Fetch a sing-box subscription URL and merge nodes");
    println!("  subscriptions   Refresh provider subscription URLs once per day");
    println!(
        "  sync            Log into a provider site, fetch the sing-box subscription, and merge it"
    );
    println!("  benchmark       Benchmark selector candidates and optionally switch");
}

fn print_run_usage() {
    println!("Usage: sing-box-tui run [OPTIONS]");
    println!();
    println!("Options:");
    println!(
        "      --controller <URL>              Clash controller base URL (default: {DEFAULT_CONTROLLER}; env: SING_BOX_CONTROLLER)"
    );
    println!(
        "      --max-concurrency <N>           Limit concurrent delay probes in TUI benchmarks (default: {DEFAULT_BENCHMARK_MAX_CONCURRENCY})"
    );
    println!(
        "      --config <FILE>                 sing-box config path for TUI subscription refresh"
    );
    println!(
        "      --subscription-input <FILE>     Provider URL file (default: {DEFAULT_SUBSCRIPTION_SOURCE_PATH})"
    );
    println!(
        "      --subscription-cache <FILE>     Subscription payload cache (default: {DEFAULT_SUBSCRIPTION_CACHE_PATH})"
    );
    println!(
        "      --subscription-interval-days <N> Refresh interval in days (default: {DEFAULT_SUBSCRIPTION_INTERVAL_DAYS})"
    );
    println!("      --force-subscription-refresh    Fetch on startup even if cache is fresh");
    println!(
        "      --include-geosite-rules         Include remote geoip/geosite/AdGuard rule-sets when creating a default config"
    );
    println!(
        "      --include-tun-mode              Include a TUN inbound when creating a default config"
    );
    println!("      --no-subscription-refresh       Disable TUI background subscription refresh");
}

fn print_selectors_usage() {
    println!("Usage: sing-box-tui selectors [OPTIONS]");
    println!();
    println!("Options:");
    println!(
        "      --controller <URL>   Clash controller base URL (default: {DEFAULT_CONTROLLER}; env: SING_BOX_CONTROLLER)"
    );
    println!("      --selector <NAME>    Return only the named selector group");
}

fn print_status_usage() {
    println!("Usage: sing-box-tui status [OPTIONS]");
    println!();
    println!("Options:");
    println!(
        "      --controller <URL>   Clash controller base URL (default: {DEFAULT_CONTROLLER}; env: SING_BOX_CONTROLLER)"
    );
}

fn print_import_usage() {
    println!("Usage: sing-box-tui import --input <FILE> [OPTIONS]");
    println!();
    println!("Input options:");
    println!("  -i, --input <FILE>     Input Clash YAML subscription/config file");
    println!(
        "      --config <FILE>    Existing sing-box config to merge into (default: {DEFAULT_CONFIG_PATH})"
    );
    println!();
    println!("Output options:");
    println!("  -o, --output <FILE>    Output full sing-box config JSON");
    println!();
    println!("Behavior options:");
    println!("      --replace-nodes    Replace existing node outbounds instead of merging");
    println!(
        "      --include-geosite-rules    Include remote geoip/geosite/AdGuard rule-sets when creating a default config"
    );
    println!(
        "      --include-tun-mode         Include a TUN inbound when creating a default config"
    );
}

fn print_subscribe_usage() {
    println!("Usage: sing-box-tui subscribe --url <URL> [OPTIONS]");
    println!();
    println!("Input options:");
    println!("      --url <URL>                       sing-box subscription URL");
    println!(
        "      --config <FILE>                   Existing sing-box config to merge into (default: {DEFAULT_CONFIG_PATH})"
    );
    println!();
    println!("Output options:");
    println!("  -o, --output <FILE>                   Output merged config path");
    println!("      --subscription-output <FILE>      Save downloaded sing-box JSON for debugging");
    println!("      --provider-name <NAME>            Wrap imported nodes in a provider selector");
    println!(
        "      --existing-provider-name <NAME>   Wrap existing template nodes in a provider selector"
    );
    println!();
    println!("Behavior options:");
    println!(
        "      --replace-nodes                   Replace existing node outbounds instead of merging"
    );
    println!(
        "      --include-geosite-rules           Include remote geoip/geosite/AdGuard rule-sets when creating a default config"
    );
    println!(
        "      --include-tun-mode                Include a TUN inbound when creating a default config"
    );
}

fn print_subscriptions_usage() {
    println!("Usage: sing-box-tui subscriptions [OPTIONS]");
    println!();
    println!("Input options:");
    println!(
        "  -i, --input <FILE>       Provider URL file in '<provider> = <url>' format (default: {DEFAULT_SUBSCRIPTION_SOURCE_PATH})"
    );
    println!(
        "      --config <FILE>      Existing sing-box config whose node outbounds are refreshed (default: {DEFAULT_CONFIG_PATH})"
    );
    println!(
        "      --cache <FILE>       Local subscription payload cache (default: {DEFAULT_SUBSCRIPTION_CACHE_PATH})"
    );
    println!();
    println!("Output options:");
    println!("  -o, --output <FILE>      Output refreshed config path");
    println!("      --write              Overwrite the --config file in place");
    println!();
    println!("Behavior options:");
    println!(
        "      --interval-days <N>  Refresh interval in days (default: {DEFAULT_SUBSCRIPTION_INTERVAL_DAYS})"
    );
    println!("      --force              Fetch every provider even when cache is fresh");
    println!(
        "      --replace-nodes      Replace existing provider node outbounds before adding refreshed nodes"
    );
    println!(
        "      --include-geosite-rules    Include remote geoip/geosite/AdGuard rule-sets when creating a default config"
    );
    println!(
        "      --include-tun-mode         Include a TUN inbound when creating a default config"
    );
}

fn print_sync_provider_usage() {
    println!("Usage: sing-box-tui sync --provider <URL> --account-file <FILE> [OPTIONS]");
    println!();
    println!("Input options:");
    println!("      --provider <URL>              Provider website base URL");
    println!("      --account-file <FILE>         Local text file containing account and password");
    println!(
        "      --config <FILE>               Existing sing-box config to merge into (default: {DEFAULT_CONFIG_PATH})"
    );
    println!();
    println!("Output options:");
    println!("  -o, --output <FILE>               Output merged config path");
    println!("      --subscription-output <FILE>  Save downloaded sing-box JSON for debugging");
    println!();
    println!("Behavior options:");
    println!(
        "      --replace-nodes               Replace existing node outbounds instead of merging"
    );
    println!(
        "      --include-geosite-rules       Include remote geoip/geosite/AdGuard rule-sets when creating a default config"
    );
    println!(
        "      --include-tun-mode            Include a TUN inbound when creating a default config"
    );
    println!("      --write                       Overwrite the --config file in place");
}

fn print_benchmark_usage() {
    println!("Usage: sing-box-tui benchmark [OPTIONS]");
    println!();
    println!("Options:");
    println!(
        "      --controller <URL>        Clash controller base URL (default: {DEFAULT_CONTROLLER}; env: SING_BOX_CONTROLLER)"
    );
    println!(
        "      --selector <NAME>         Selector group to benchmark (default: {DEFAULT_SELECTOR_TAG})"
    );
    println!(
        "      --match <TEXT>            Substring filter for candidate tags (default: empty)"
    );
    println!("      --url <URL>               Delay test URL (default: {DEFAULT_DELAY_TEST_URL})");
    println!("      --timeout-ms <MS>         Delay probe timeout in ms (default: 5000)");
    println!("      --request-timeout <SEC>   HTTP request timeout in seconds (default: 12)");
    println!(
        "      --max-concurrency <N>     Limit concurrent delay probes (default: {DEFAULT_BENCHMARK_MAX_CONCURRENCY})"
    );
    println!("      --switch                  Switch selector to the best successful node");
    println!("      --verify                  Run post-switch verification targets");
    println!(
        "      --verify-url <NAME=URL>   Add a target to the default verification list; repeatable"
    );
}

#[cfg(test)]
mod tests {
    use super::CliCommand;
    use crate::defaults::DEFAULT_BENCHMARK_MAX_CONCURRENCY;
    use std::path::PathBuf;

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
            "--config".to_string(),
            "/usr/local/etc/sing-box/config.json".to_string(),
            "--subscription-input".to_string(),
            ".suburl".to_string(),
            "--subscription-cache".to_string(),
            ".suburl.cache.json".to_string(),
            "--subscription-interval-days".to_string(),
            "2".to_string(),
            "--force-subscription-refresh".to_string(),
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
    fn run_command_parses_subscription_refresh_options() {
        let command = CliCommand::parse([
            "run".to_string(),
            "--config".to_string(),
            "/usr/local/etc/sing-box/config.json".to_string(),
            "--subscription-input".to_string(),
            "urls.txt".to_string(),
            "--subscription-cache".to_string(),
            "cache.json".to_string(),
            "--subscription-interval-days".to_string(),
            "3".to_string(),
            "--force-subscription-refresh".to_string(),
        ])
        .expect("run command parses");

        match command {
            CliCommand::Run {
                subscription_config_path,
                subscription_input,
                subscription_cache,
                subscription_interval_days,
                force_subscription_refresh,
                subscription_refresh_disabled,
                ..
            } => {
                assert_eq!(
                    subscription_config_path,
                    PathBuf::from("/usr/local/etc/sing-box/config.json")
                );
                assert_eq!(subscription_input, PathBuf::from("urls.txt"));
                assert_eq!(subscription_cache, PathBuf::from("cache.json"));
                assert_eq!(subscription_interval_days, 3);
                assert!(force_subscription_refresh);
                assert!(!subscription_refresh_disabled);
            }
            _ => panic!("expected run command"),
        }
    }

    #[test]
    fn run_command_can_disable_subscription_refresh() {
        let command =
            CliCommand::parse(["run".to_string(), "--no-subscription-refresh".to_string()])
                .expect("run command parses");

        match command {
            CliCommand::Run {
                subscription_refresh_disabled,
                ..
            } => assert!(subscription_refresh_disabled),
            _ => panic!("expected run command"),
        }
    }

    #[test]
    fn config_generation_commands_parse_default_config_include_flags() {
        let run = CliCommand::parse([
            "run".to_string(),
            "--include-geosite-rules".to_string(),
            "--include-tun-mode".to_string(),
        ])
        .expect("run command parses");
        match run {
            CliCommand::Run {
                include_geosite_rules,
                include_tun_mode,
                ..
            } => {
                assert!(include_geosite_rules);
                assert!(include_tun_mode);
            }
            _ => panic!("expected run command"),
        }

        let import = CliCommand::parse([
            "import".to_string(),
            "--input".to_string(),
            "nodes.yaml".to_string(),
            "--include-geosite-rules".to_string(),
            "--include-tun-mode".to_string(),
        ])
        .expect("import command parses");
        match import {
            CliCommand::Import {
                include_geosite_rules,
                include_tun_mode,
                ..
            } => {
                assert!(include_geosite_rules);
                assert!(include_tun_mode);
            }
            _ => panic!("expected import command"),
        }

        let subscribe = CliCommand::parse([
            "subscribe".to_string(),
            "--url".to_string(),
            "https://example.com/sub".to_string(),
            "--include-geosite-rules".to_string(),
            "--include-tun-mode".to_string(),
        ])
        .expect("subscribe command parses");
        match subscribe {
            CliCommand::Subscribe {
                include_geosite_rules,
                include_tun_mode,
                ..
            } => {
                assert!(include_geosite_rules);
                assert!(include_tun_mode);
            }
            _ => panic!("expected subscribe command"),
        }

        let subscriptions = CliCommand::parse([
            "subscriptions".to_string(),
            "--output".to_string(),
            "merged.json".to_string(),
            "--include-geosite-rules".to_string(),
            "--include-tun-mode".to_string(),
        ])
        .expect("subscriptions command parses");
        match subscriptions {
            CliCommand::Subscriptions {
                include_geosite_rules,
                include_tun_mode,
                ..
            } => {
                assert!(include_geosite_rules);
                assert!(include_tun_mode);
            }
            _ => panic!("expected subscriptions command"),
        }

        let sync = CliCommand::parse([
            "sync".to_string(),
            "--provider".to_string(),
            "https://3.airtcp.me".to_string(),
            "--account-file".to_string(),
            "account.txt".to_string(),
            "--output".to_string(),
            "merged.json".to_string(),
            "--include-geosite-rules".to_string(),
            "--include-tun-mode".to_string(),
        ])
        .expect("sync command parses");
        match sync {
            CliCommand::SyncProvider {
                include_geosite_rules,
                include_tun_mode,
                ..
            } => {
                assert!(include_geosite_rules);
                assert!(include_tun_mode);
            }
            _ => panic!("expected sync command"),
        }
    }

    #[test]
    fn sync_command_parses_required_arguments() {
        let command = CliCommand::parse([
            "sync".to_string(),
            "--provider".to_string(),
            "https://3.airtcp.me".to_string(),
            "--account-file".to_string(),
            "account.txt".to_string(),
            "--output".to_string(),
            "merged.json".to_string(),
        ])
        .expect("sync command parses");

        match command {
            CliCommand::SyncProvider {
                provider,
                account_file,
                output,
                subscription_output,
                replace_nodes,
                write,
                ..
            } => {
                assert_eq!(provider, "https://3.airtcp.me");
                assert_eq!(account_file, PathBuf::from("account.txt"));
                assert_eq!(output, Some(PathBuf::from("merged.json")));
                assert!(subscription_output.is_none());
                assert!(!replace_nodes);
                assert!(!write);
            }
            _ => panic!("expected sync-provider command"),
        }
    }

    #[test]
    fn subscribe_command_parses_url_and_output() {
        let command = CliCommand::parse([
            "subscribe".to_string(),
            "--url".to_string(),
            "https://example.com/sub?token=secret".to_string(),
            "--config".to_string(),
            "config.json".to_string(),
            "--output".to_string(),
            "merged.json".to_string(),
            "--replace-nodes".to_string(),
        ])
        .expect("subscribe command parses");

        match command {
            CliCommand::Subscribe {
                url,
                config_path,
                output,
                replace_nodes,
                provider_name,
                existing_provider_name,
                ..
            } => {
                assert_eq!(url, "https://example.com/sub?token=secret");
                assert_eq!(config_path, PathBuf::from("config.json"));
                assert_eq!(output, Some(PathBuf::from("merged.json")));
                assert!(replace_nodes);
                assert!(provider_name.is_none());
                assert!(existing_provider_name.is_none());
            }
            _ => panic!("expected subscribe command"),
        }
    }

    #[test]
    fn subscriptions_command_parses_refresh_options() {
        let command = CliCommand::parse([
            "subscriptions".to_string(),
            "--input".to_string(),
            ".suburl".to_string(),
            "--cache".to_string(),
            ".suburl.cache.json".to_string(),
            "--config".to_string(),
            "config.json".to_string(),
            "--output".to_string(),
            "merged.json".to_string(),
            "--interval-days".to_string(),
            "1".to_string(),
            "--force".to_string(),
        ])
        .expect("subscriptions command parses");

        match command {
            CliCommand::Subscriptions {
                input,
                cache,
                output,
                config_path,
                force,
                interval_days,
                write,
                ..
            } => {
                assert_eq!(input, PathBuf::from(".suburl"));
                assert_eq!(cache, PathBuf::from(".suburl.cache.json"));
                assert_eq!(config_path, PathBuf::from("config.json"));
                assert_eq!(output, Some(PathBuf::from("merged.json")));
                assert!(force);
                assert_eq!(interval_days, 1);
                assert!(!write);
            }
            _ => panic!("expected subscriptions command"),
        }
    }

    #[test]
    fn subscriptions_command_requires_output_or_write() {
        let error = CliCommand::parse(["subscriptions".to_string()])
            .expect_err("subscriptions without write target should fail");

        assert!(
            error
                .to_string()
                .contains("subscriptions requires either --output <FILE> or --write")
        );
    }

    #[test]
    fn sync_command_requires_account_file() {
        let error = CliCommand::parse([
            "sync".to_string(),
            "--provider".to_string(),
            "https://3.airtcp.me".to_string(),
        ])
        .expect_err("missing account file should fail");

        assert!(
            error
                .to_string()
                .contains("sync requires --account-file <FILE>")
        );
    }

    #[test]
    fn sync_command_requires_output_or_write() {
        let error = CliCommand::parse([
            "sync".to_string(),
            "--provider".to_string(),
            "https://3.airtcp.me".to_string(),
            "--account-file".to_string(),
            "account.txt".to_string(),
        ])
        .expect_err("sync without write target should fail");

        assert!(
            error
                .to_string()
                .contains("sync requires either --output <FILE> or --write")
        );
    }

    #[test]
    fn sync_command_accepts_write_flag() {
        let command = CliCommand::parse([
            "sync".to_string(),
            "--provider".to_string(),
            "https://3.airtcp.me".to_string(),
            "--account-file".to_string(),
            "account.txt".to_string(),
            "--write".to_string(),
        ])
        .expect("sync command with write parses");

        match command {
            CliCommand::SyncProvider { write, .. } => assert!(write),
            _ => panic!("expected sync command"),
        }
    }

    #[test]
    fn selectors_command_accepts_optional_selector() {
        let command = CliCommand::parse([
            "selectors".to_string(),
            "--controller".to_string(),
            "http://127.0.0.1:9992".to_string(),
            "--selector".to_string(),
            "select".to_string(),
        ])
        .expect("selectors command parses");

        match command {
            CliCommand::Selectors {
                controller,
                selector,
            } => {
                assert_eq!(controller.as_deref(), Some("http://127.0.0.1:9992"));
                assert_eq!(selector.as_deref(), Some("select"));
            }
            _ => panic!("expected selectors command"),
        }
    }

    #[test]
    fn status_command_accepts_controller() {
        let command = CliCommand::parse([
            "status".to_string(),
            "--controller".to_string(),
            "http://127.0.0.1:9992".to_string(),
        ])
        .expect("status command parses");

        match command {
            CliCommand::Status { controller } => {
                assert_eq!(controller.as_deref(), Some("http://127.0.0.1:9992"));
            }
            _ => panic!("expected status command"),
        }
    }
}

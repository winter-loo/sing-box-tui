use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::defaults::{
    DEFAULT_BENCHMARK_MAX_CONCURRENCY, DEFAULT_CONFIG_PATH, DEFAULT_DELAY_TEST_URL,
    DEFAULT_SELECTOR_TAG,
};

#[derive(Debug)]
pub(crate) enum CliCommand {
    Run {
        controller: Option<String>,
        max_concurrency: Option<usize>,
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
    },
    Subscribe {
        url: String,
        output: Option<PathBuf>,
        config_path: PathBuf,
        subscription_output: Option<PathBuf>,
        replace_nodes: bool,
        provider_name: Option<String>,
        existing_provider_name: Option<String>,
    },
    SyncProvider {
        provider: String,
        account_file: PathBuf,
        config_path: PathBuf,
        output: Option<PathBuf>,
        subscription_output: Option<PathBuf>,
        replace_nodes: bool,
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
        verify_discord: bool,
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
            });
        }

        match args[0].as_str() {
            "run" => Self::parse_run(&args[1..]),
            "selectors" => Self::parse_selectors(&args[1..]),
            "status" => Self::parse_status(&args[1..]),
            "import" => Self::parse_import(&args[1..]),
            "subscribe" => Self::parse_subscribe(&args[1..]),
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

    fn parse_subscribe(args: &[String]) -> Result<Self> {
        let mut url = None;
        let mut output = None;
        let mut config_path = PathBuf::from(DEFAULT_CONFIG_PATH);
        let mut subscription_output = None;
        let mut replace_nodes = false;
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
            provider_name,
            existing_provider_name,
        })
    }

    fn parse_sync_provider(args: &[String]) -> Result<Self> {
        let mut provider = None;
        let mut account_file = None;
        let mut config_path = PathBuf::from(DEFAULT_CONFIG_PATH);
        let mut output = None;
        let mut subscription_output = None;
        let mut replace_nodes = false;
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
    println!("  selectors [--controller URL] [--selector NAME]  Show Clash selector groups");
    println!("  status [--controller URL]                       Show Clash controller status");
    println!("  import -i <clash.yml> [-o <config.json>] [--config FILE] [--replace-nodes]");
    println!(
        "                                                Import Clash YAML into a full sing-box config"
    );
    println!("  subscribe --url URL [-o <config.json>] [--config FILE] [--replace-nodes]");
    println!(
        "                                                Fetch a sing-box subscription URL and merge nodes"
    );
    println!("  sync --provider URL --account-file FILE [--config FILE] [-o FILE]");
    println!(
        "                                                Log into a provider site, fetch the sing-box subscription, and merge it"
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

fn print_selectors_usage() {
    println!("sing-box-tui selectors [--controller URL] [--selector NAME]");
    println!();
    println!("Options:");
    println!("      --controller <URL>        Clash controller base URL");
    println!("      --selector <NAME>         Return only the named selector group");
}

fn print_status_usage() {
    println!("sing-box-tui status [--controller URL]");
    println!();
    println!("Options:");
    println!("      --controller <URL>        Clash controller base URL");
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

fn print_subscribe_usage() {
    println!(
        "sing-box-tui subscribe --url URL [-o <config.json>] [--config FILE] [--replace-nodes]"
    );
    println!();
    println!("Input options:");
    println!("      --url <URL>                  sing-box subscription URL");
    println!(
        "      --config <FILE>              Existing sing-box config to merge into (default: /etc/sing-box/config.json)"
    );
    println!();
    println!("Output options:");
    println!("  -o, --output <FILE>              Output merged config path");
    println!("      --subscription-output <FILE> Save downloaded sing-box JSON for debugging");
    println!("      --provider-name <NAME>       Wrap imported nodes in a provider selector");
    println!(
        "      --existing-provider-name <NAME> Wrap existing template nodes in a provider selector"
    );
    println!();
    println!("Behavior options:");
    println!(
        "      --replace-nodes              Replace existing node outbounds instead of merging"
    );
}

fn print_sync_provider_usage() {
    println!("sing-box-tui sync --provider URL --account-file FILE [--config FILE] [-o FILE]");
    println!();
    println!("Input options:");
    println!("      --provider <URL>              Provider website base URL");
    println!("      --account-file <FILE>         Local text file containing account and password");
    println!(
        "      --config <FILE>               Existing sing-box config to merge into (default: /etc/sing-box/config.json)"
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
    println!("      --write                       Overwrite the --config file in place");
}

fn print_benchmark_usage() {
    println!("sing-box-tui benchmark [options]");
    println!();
    println!("Options:");
    println!("      --controller <URL>        Clash controller base URL");
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
    println!("      --verify                  Run post-switch verification HTTP checks");
    println!("      --verify-discord          Include Discord checks during verification");
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
            "http://127.0.0.1:9090".to_string(),
            "--selector".to_string(),
            "select".to_string(),
        ])
        .expect("selectors command parses");

        match command {
            CliCommand::Selectors {
                controller,
                selector,
            } => {
                assert_eq!(controller.as_deref(), Some("http://127.0.0.1:9090"));
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
            "http://127.0.0.1:9090".to_string(),
        ])
        .expect("status command parses");

        match command {
            CliCommand::Status { controller } => {
                assert_eq!(controller.as_deref(), Some("http://127.0.0.1:9090"));
            }
            _ => panic!("expected status command"),
        }
    }
}

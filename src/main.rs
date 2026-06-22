use std::env;

use anyhow::{Context, Result, bail};

mod clash;
mod cli;
mod config;
mod controller;
mod defaults;
mod import;
mod provider;
mod storage;
mod subscriptions;
mod tui;
mod tui_state;

use cli::CliCommand;
use controller::{
    BenchmarkOptions, SelectorsOptions, StatusOptions, VerificationTarget, run_benchmark,
    run_selectors, run_status,
};
use defaults::DEFAULT_VERIFICATION_TARGETS;
use import::{run_import, run_subscribe_import};
use provider::run_provider_sync;
use subscriptions::run_subscription_refresh;
use tui::{TuiSubscriptionRefreshOptions, run_tui};

fn main() -> Result<()> {
    match CliCommand::parse(env::args().skip(1))? {
        CliCommand::Run {
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
        } => run_tui(
            controller,
            max_concurrency,
            TuiSubscriptionRefreshOptions {
                input: subscription_input,
                cache_path: subscription_cache,
                config_path: subscription_config_path,
                disabled: subscription_refresh_disabled,
                force: force_subscription_refresh,
                include_geosite_rules,
                include_tun_mode,
                interval_days: subscription_interval_days,
            },
        ),
        CliCommand::Selectors {
            controller,
            selector,
        } => run_selectors(SelectorsOptions {
            controller,
            selector,
        }),
        CliCommand::Status { controller } => run_status(StatusOptions { controller }),
        CliCommand::Import {
            input,
            output,
            config_path,
            replace_nodes,
            include_geosite_rules,
            include_tun_mode,
        } => run_import(
            &input,
            output.as_ref(),
            true,
            &config_path,
            replace_nodes,
            include_geosite_rules,
            include_tun_mode,
        ),
        CliCommand::Subscribe {
            url,
            output,
            config_path,
            subscription_output,
            replace_nodes,
            include_geosite_rules,
            include_tun_mode,
            provider_name,
            existing_provider_name,
        } => run_subscribe_import(
            url,
            output.as_ref(),
            &config_path,
            subscription_output.as_ref(),
            replace_nodes,
            include_geosite_rules,
            include_tun_mode,
            provider_name.as_deref(),
            existing_provider_name.as_deref(),
        ),
        CliCommand::Subscriptions {
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
        } => run_subscription_refresh(
            &input,
            &cache,
            &config_path,
            output.as_ref(),
            replace_nodes,
            include_geosite_rules,
            include_tun_mode,
            write,
            force,
            interval_days,
        ),
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
            verify_urls,
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
            verification_targets: verification_targets_from_specs(&verify_urls)?,
        }),
        CliCommand::SyncProvider {
            provider,
            account_file,
            config_path,
            output,
            subscription_output,
            replace_nodes,
            include_geosite_rules,
            include_tun_mode,
            write,
        } => run_provider_sync(
            provider,
            &account_file,
            &config_path,
            output.as_ref(),
            subscription_output.as_ref(),
            replace_nodes,
            include_geosite_rules,
            include_tun_mode,
            write,
        ),
    }
}

fn verification_targets_from_specs(specs: &[String]) -> Result<Vec<VerificationTarget>> {
    let mut targets = default_verification_targets();
    targets.extend(
        specs
            .iter()
            .map(|spec| verification_target_from_spec(spec))
            .collect::<Result<Vec<_>>>()?,
    );
    Ok(targets)
}

fn default_verification_targets() -> Vec<VerificationTarget> {
    DEFAULT_VERIFICATION_TARGETS
        .iter()
        .map(|(name, url)| VerificationTarget {
            name: (*name).to_string(),
            url: (*url).to_string(),
        })
        .collect()
}

fn verification_target_from_spec(spec: &str) -> Result<VerificationTarget> {
    let (name, url) = spec
        .split_once('=')
        .with_context(|| format!("verification target must be NAME=URL, got {spec}"))?;
    let name = name.trim();
    let url = url.trim();
    if name.is_empty() {
        bail!("verification target name cannot be empty");
    }
    if url.is_empty() {
        bail!("verification target URL cannot be empty");
    }
    Ok(VerificationTarget {
        name: name.to_string(),
        url: url.to_string(),
    })
}

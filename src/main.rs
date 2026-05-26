use std::env;

use anyhow::Result;

mod clash;
mod cli;
mod config;
mod controller;
mod defaults;
mod import;
mod provider;
mod storage;
mod tui;
mod tui_state;

use cli::CliCommand;
use controller::{
    BenchmarkOptions, SelectorsOptions, StatusOptions, run_benchmark, run_selectors, run_status,
};
use import::run_import;
use provider::run_provider_sync;
use tui::run_tui;

fn main() -> Result<()> {
    match CliCommand::parse(env::args().skip(1))? {
        CliCommand::Run {
            controller,
            max_concurrency,
        } => run_tui(controller, max_concurrency),
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
        CliCommand::SyncProvider {
            provider,
            account_file,
            config_path,
            output,
            subscription_output,
            replace_nodes,
            write,
        } => run_provider_sync(
            provider,
            &account_file,
            &config_path,
            output.as_ref(),
            subscription_output.as_ref(),
            replace_nodes,
            write,
        ),
    }
}

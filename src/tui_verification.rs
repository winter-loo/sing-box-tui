use std::sync::mpsc::{self, TryRecvError};
use std::thread::{self, JoinHandle};

use anyhow::{Context, Result, bail};

use super::App;
use crate::controller::{VerificationReport, VerificationTarget, run_verification};
use crate::defaults::DEFAULT_VERIFICATION_TARGETS;

pub(super) struct VerifyJob {
    receiver: mpsc::Receiver<VerificationReport>,
    worker: JoinHandle<()>,
}

pub(super) fn default_verification_targets_setting() -> String {
    DEFAULT_VERIFICATION_TARGETS
        .iter()
        .map(|(name, url)| format!("{name}={url}"))
        .collect::<Vec<_>>()
        .join(",")
}

pub(super) fn parse_verification_targets(input: &str) -> Result<Vec<VerificationTarget>> {
    input
        .split([',', '\n', '\r'])
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(parse_verification_target)
        .collect()
}

fn parse_verification_target(input: &str) -> Result<VerificationTarget> {
    let (name, url) = input
        .split_once('=')
        .with_context(|| format!("verification target must be NAME=URL, got {input}"))?;
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

impl App {
    pub(super) fn start_verify(&mut self) {
        if self.verify_job.is_some() {
            self.set_status_only("Network verification is already running");
            return;
        }
        let targets = match parse_verification_targets(&self.verify_targets) {
            Ok(targets) if !targets.is_empty() => targets,
            Ok(_) => {
                self.set_status_only("Configure verification targets in settings first");
                return;
            }
            Err(error) => {
                self.set_status_only(format!("Verification targets invalid: {error}"));
                return;
            }
        };
        let (tx, rx) = mpsc::channel();
        let proxy_server = self.system_proxy.resolved_server();
        let worker_proxy_server = proxy_server.clone();
        let worker = thread::spawn(move || {
            let report = run_verification(&worker_proxy_server, &targets);
            let _ = tx.send(report);
        });
        self.verify_job = Some(VerifyJob {
            receiver: rx,
            worker,
        });
        self.set_status_only(format!(
            "Running network verification via {proxy_server}..."
        ));
    }

    pub(super) fn poll_verify_updates(&mut self) {
        let Some(job) = self.verify_job.as_ref() else {
            return;
        };
        let result = match job.receiver.try_recv() {
            Ok(report) => report,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                self.verify_job = None;
                self.set_status_with_flash("Network verification failed: worker disconnected");
                return;
            }
        };
        let job = self.verify_job.take().expect("verify job exists");
        let _ = job.worker.join();
        self.set_status_with_flash(result.summary_line());
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::test_app;
    use super::parse_verification_targets;

    #[test]
    fn target_parser_owns_the_name_url_contract() {
        let targets = parse_verification_targets(
            "example=https://example.com, fallback=https://fallback.example",
        )
        .unwrap();
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].name, "example");
        assert_eq!(targets[0].url, "https://example.com");
        assert!(parse_verification_targets("missing-separator").is_err());
    }

    #[test]
    fn empty_and_invalid_targets_do_not_start_a_worker() {
        let mut app = test_app();
        app.verify_targets.clear();
        app.start_verify();
        assert!(app.verify_job.is_none());
        assert_eq!(
            app.status,
            "Configure verification targets in settings first"
        );
        app.verify_targets = "invalid".to_string();
        app.start_verify();
        assert!(app.verify_job.is_none());
        assert!(app.status.starts_with("Verification targets invalid:"));
    }
}

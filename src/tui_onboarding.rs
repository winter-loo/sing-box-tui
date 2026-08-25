use std::fs;
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use crossterm::event::KeyCode;

use super::App;
use crate::subscriptions::DEFAULT_SUBSCRIPTION_SOURCE_PATH;

fn subscription_url(input: &str) -> Result<&str> {
    let url = input.trim();
    if url.is_empty() {
        bail!("Paste a subscription URL first, or press s to skip.");
    }
    if !url.starts_with("http://") && !url.starts_with("https://") {
        bail!("Subscription URL must start with http:// or https://");
    }
    Ok(url)
}

impl App {
    pub(super) fn handle_onboarding_key(&mut self, code: KeyCode) -> Result<bool> {
        match code {
            KeyCode::Esc => {
                self.onboarding = None;
                self.set_status_only("First run setup postponed");
            }
            KeyCode::Char('s') => {
                self.onboarding_complete = true;
                self.onboarding = None;
                self.save_runtime_state()?;
                self.set_status_only("First run setup skipped");
            }
            KeyCode::Enter => self.finish_onboarding_with_subscription()?,
            KeyCode::Backspace => {
                if let Some(onboarding) = &mut self.onboarding {
                    onboarding.input.pop();
                }
            }
            KeyCode::Char(ch) => {
                if let Some(onboarding) = &mut self.onboarding {
                    onboarding.input.push(ch);
                }
            }
            _ => {}
        }
        Ok(true)
    }

    fn finish_onboarding_with_subscription(&mut self) -> Result<()> {
        let Some(onboarding) = &mut self.onboarding else {
            return Ok(());
        };
        let url = match subscription_url(&onboarding.input) {
            Ok(url) => url,
            Err(error) => {
                onboarding.message = error.to_string();
                return Ok(());
            }
        };
        let line = format!("default = {url}\n");
        let path = PathBuf::from(DEFAULT_SUBSCRIPTION_SOURCE_PATH);
        if path.exists() {
            let existing = fs::read_to_string(&path).unwrap_or_default();
            if !existing.contains(url) {
                let mut file = fs::OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .with_context(|| format!("failed to open {}", path.display()))?;
                if !existing.ends_with('\n') && !existing.is_empty() {
                    writeln!(file)
                        .with_context(|| format!("failed to write {}", path.display()))?;
                }
                file.write_all(line.as_bytes())
                    .with_context(|| format!("failed to write {}", path.display()))?;
            }
        } else {
            fs::write(&path, line)
                .with_context(|| format!("failed to write {}", path.display()))?;
        }
        self.onboarding = None;
        self.onboarding_complete = true;
        self.save_runtime_state()?;
        self.set_status_only("First run setup saved .suburl; press u to refresh subscriptions");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyCode;

    use super::super::test_support::test_app;
    use super::super::view::OnboardingState;
    use super::subscription_url;

    #[test]
    fn subscription_url_requires_http_and_trims_input() {
        assert_eq!(
            subscription_url(" https://example.com/sub ").unwrap(),
            "https://example.com/sub"
        );
        assert!(subscription_url("").is_err());
        assert!(subscription_url("ftp://example.com").is_err());
    }

    #[test]
    fn invalid_submission_stays_in_onboarding_with_feedback() {
        let mut app = test_app();
        app.onboarding = Some(OnboardingState {
            input: "ftp://example.com".to_string(),
            message: String::new(),
        });
        app.handle_key(KeyCode::Enter).unwrap();
        let onboarding = app.onboarding.as_ref().expect("onboarding remains open");
        assert_eq!(
            onboarding.message,
            "Subscription URL must start with http:// or https://"
        );
    }
}

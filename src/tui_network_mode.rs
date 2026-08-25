use crate::internet_tun::{InternetTunTarget, InternetTunToggleOutcome, PersistedInternetTun};
use crate::managed_sing_box::AuthorizationRequirement;
use crate::system_proxy::{SystemProxyToggle, SystemProxyUpdate};
use crate::tui_state::TuiRuntimeState;

use super::App;
use super::presentation::truncate_for_width;

#[cfg(test)]
#[path = "tui_network_mode_tests.rs"]
mod tests;

pub(super) fn apply_internet_tun_persistence(
    state: &mut TuiRuntimeState,
    persisted: PersistedInternetTun,
) {
    state.tun_enabled = persisted.enabled();
    state.tun_auto_detect_interface_before_enable = persisted.restore_auto_detect_interface();
}

impl App {
    pub(super) fn set_system_proxy(&mut self) {
        let bypass_entries = self
            .private_access
            .system_proxy_bypass_entries(&self.bypass_entries);
        match self.system_proxy.toggle(bypass_entries) {
            SystemProxyToggle::AlreadyRunning => {
                self.set_status_only("System proxy update is already running");
            }
            SystemProxyToggle::Started {
                enable: true,
                server,
            } => self.set_status_only(format!("Enabling system proxy at {server}...")),
            SystemProxyToggle::Started { enable: false, .. } => {
                self.set_status_only("Disabling system proxy...");
            }
        }
    }

    pub(super) fn poll_system_proxy_updates(&mut self) {
        match self.system_proxy.poll() {
            Some(SystemProxyUpdate::Applied(message)) => self.set_status_with_flash(message),
            Some(SystemProxyUpdate::Failed(error)) => self.set_status_with_flash(format!(
                "System proxy update failed: {}",
                truncate_for_width(&error, 90)
            )),
            None => {}
        }
    }

    pub(super) fn tun_toggle_needs_terminal_prompt(&self) -> bool {
        self.internet_tun.authorization_requirement(&self.sing_box)
            == AuthorizationRequirement::InteractiveSudo
    }

    pub(super) fn toggle_tun_mode(&mut self) {
        if self.internet_tun.is_transitioning() {
            self.set_status_only("TUN mode update is already running");
            return;
        }
        let state_store = self.state_store.clone();
        let mut runtime_state = self.runtime_state();
        match self.internet_tun.start_toggle(|tun| {
            apply_internet_tun_persistence(&mut runtime_state, tun);
            if let Some(store) = &state_store {
                store.save(&runtime_state)?;
            }
            Ok(())
        }) {
            Ok(InternetTunTarget::Enabled) => self.set_status_only("Enabling TUN mode..."),
            Ok(InternetTunTarget::Disabled) => self.set_status_only("Disabling TUN mode..."),
            Err(error) => self.set_status_with_flash(format!(
                "TUN mode update failed: {}",
                truncate_for_width(&format!("{error:#}"), 90)
            )),
        }
    }

    pub(super) fn poll_tun_toggle_updates(&mut self) {
        if !self.internet_tun.is_transitioning() {
            return;
        }
        let state_store = self.state_store.clone();
        let mut runtime_state = self.runtime_state();
        let Some(outcome) = self
            .internet_tun
            .poll(&mut self.sing_box, &self.client, |tun| {
                apply_internet_tun_persistence(&mut runtime_state, tun);
                if let Some(store) = &state_store {
                    store.save(&runtime_state)?;
                }
                Ok(())
            })
        else {
            return;
        };
        match outcome {
            InternetTunToggleOutcome::Failed {
                error,
                recovery_warning,
            } => {
                let recovery_note = recovery_warning.map(|warning| {
                    format!(
                        "; failed to clear transition journal: {}",
                        truncate_for_width(&warning, 40)
                    )
                });
                self.set_status_with_flash(format!(
                    "TUN mode update failed: {}{}",
                    truncate_for_width(&error, 90),
                    recovery_note.as_deref().unwrap_or("")
                ));
            }
            InternetTunToggleOutcome::Applied {
                target,
                config_changed,
                restart,
                persistence_warning,
            } => {
                let target_label = if target.is_enabled() {
                    "enabled"
                } else {
                    "disabled"
                };
                let state = if config_changed {
                    target_label.to_string()
                } else {
                    format!("already {target_label}")
                };
                let persist_note = persistence_warning.map(|warning| {
                    format!(
                        "; recovery journal retained: {}",
                        truncate_for_width(&warning, 40)
                    )
                });
                match restart {
                    Ok(restart) => {
                        let restarted =
                            format!("sing-box restarted {}", restart.report.transition());
                        if let Some(error) = restart.controller_error {
                            self.set_status_with_flash(format!(
                                "TUN mode {state}; {restarted}; controller not ready: {}{}",
                                truncate_for_width(&error, 60),
                                persist_note.as_deref().unwrap_or("")
                            ));
                        } else {
                            self.set_status_with_flash(format!(
                                "TUN mode {state}; {restarted}{}",
                                persist_note.as_deref().unwrap_or("")
                            ));
                        }
                    }
                    Err(error) => self.set_status_with_flash(format!(
                        "TUN mode {state} but sing-box restart failed: {error}{}",
                        persist_note.as_deref().unwrap_or("")
                    )),
                }
            }
        }
    }
}

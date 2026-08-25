use super::*;
use crate::process_command::{command_program_name_matches, command_tokens};
use std::path::Path;

fn command_matches_private_access_service(
    command: &str,
    manifest: &PrivateAccessServiceManifest,
) -> bool {
    let tokens = command_tokens(command);
    let Some(program) = tokens.first() else {
        return false;
    };
    let expected_program = manifest
        .executable
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(&manifest.executable);
    command_program_name_matches(program, expected_program)
        && manifest
            .args
            .iter()
            .all(|expected| tokens.iter().skip(1).any(|actual| actual == expected))
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(super) fn private_access_process_exists(
    pid: u32,
    manifest: &PrivateAccessServiceManifest,
) -> bool {
    process_exists(pid)
        && background_process_command(pid)
            .is_ok_and(|command| command_matches_private_access_service(&command, manifest))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(super) fn private_access_process_exists(
    pid: u32,
    _manifest: &PrivateAccessServiceManifest,
) -> bool {
    process_exists(pid)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn background_process_command(pid: u32) -> Result<String> {
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output()
        .with_context(|| format!("failed to inspect background process {pid}"))?;
    if !output.status.success() {
        bail!(
            "failed to inspect background process {pid}: ps exited with {}",
            output.status
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(all(windows, not(test)))]
const OFFICIAL_SONICWALL_CLIENT_PROCESSES: &[&str] = &["SnwlVpn.exe", "SnwlConnect.exe"];

#[cfg(all(windows, not(test)))]
fn running_official_sonicwall_client_processes() -> Vec<String> {
    OFFICIAL_SONICWALL_CLIENT_PROCESSES
        .iter()
        .filter_map(|process_name| {
            let output = Command::new("tasklist")
                .args([
                    "/FI",
                    &format!("IMAGENAME eq {process_name}"),
                    "/FO",
                    "CSV",
                    "/NH",
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .output()
                .ok()?;
            if !output.status.success() {
                return None;
            }
            parse_windows_tasklist_image_names(&String::from_utf8_lossy(&output.stdout))
                .into_iter()
                .any(|name| name.eq_ignore_ascii_case(process_name))
                .then(|| (*process_name).to_string())
        })
        .collect()
}

#[cfg(any(not(windows), test))]
fn running_official_sonicwall_client_processes() -> Vec<String> {
    Vec::new()
}

#[cfg(windows)]
fn parse_windows_tasklist_image_names(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            let line = line.strip_prefix('"')?;
            let (name, _) = line.split_once("\",")?;
            Some(name.to_string())
        })
        .collect()
}

fn format_official_sonicwall_client_warning(processes: &[String]) -> String {
    format!(
        "检测到官方 SonicWall 客户端仍在运行: {}。请先退出官方客户端，再启动 TUI 的 SonicWall 连接。",
        processes.join(", ")
    )
}

fn helper_command_uses_sudo(command: &[String]) -> bool {
    command
        .first()
        .and_then(|program| Path::new(program).file_name())
        .is_some_and(|program| program == "sudo")
}

fn make_sudo_command_noninteractive(mut command: Vec<String>) -> Vec<String> {
    if helper_command_uses_sudo(&command) && !command.iter().skip(1).any(|arg| arg == "-n") {
        command.insert(1, "-n".to_string());
    }
    command
}

fn default_tui_tun_helper_command() -> Vec<String> {
    let exe = env::current_exe()
        .map(|path| path.to_string_lossy().to_string())
        .unwrap_or_else(|_| "sing-box-tui".to_string());
    let helper_args = vec![
        exe,
        "private-access-tun-helper".to_string(),
        "--stdio".to_string(),
    ];
    if tun_helper_needs_sudo() {
        let mut command = vec!["sudo".to_string(), "-n".to_string()];
        command.extend(helper_args);
        command
    } else {
        helper_args
    }
}

#[cfg(unix)]
fn tun_helper_needs_sudo() -> bool {
    unsafe { libc::geteuid() != 0 }
}

#[cfg(not(unix))]
fn tun_helper_needs_sudo() -> bool {
    false
}

struct TuiPrivateAccessNetworkIntegration<'a> {
    config_path: &'a Path,
    sing_box: &'a mut ManagedSingBox,
    system_proxy: &'a mut SystemProxy,
    base_bypass_entries: &'a [String],
}

impl PrivateAccessNetworkIntegration for TuiPrivateAccessNetworkIntegration<'_> {
    fn apply_bridge_routes(&mut self, update: &PrivateAccessBridgeRouteUpdate) -> Result<bool> {
        if update.routes.is_empty()
            && update.domains.is_empty()
            && update.domain_suffixes.is_empty()
            && update.carrier_domains.is_empty()
        {
            return Ok(false);
        }
        let listen = update
            .bridge
            .as_ref()
            .map(|bridge| bridge.listen.as_str())
            .unwrap_or(update.fallback_listen.as_str());
        let proxy = listen
            .parse::<SocketAddrV4>()
            .with_context(|| format!("private access bridge listen must be IPv4:PORT: {listen}"))?;
        run_private_access_route_table_config(
            self.config_path,
            None,
            true,
            PrivateAccessRouteTableOptions {
                profile_id: update.profile_id.clone(),
                cidrs: update
                    .routes
                    .iter()
                    .map(|route| route.cidr.clone())
                    .collect(),
                domains: update.domains.clone(),
                domain_suffixes: update.domain_suffixes.clone(),
                previous_cidrs: update
                    .previous_routes
                    .iter()
                    .map(|route| route.cidr.clone())
                    .collect(),
                previous_domains: update.previous_domains.clone(),
                previous_domain_suffixes: update.previous_domain_suffixes.clone(),
                carrier_domains: update.carrier_domains.clone(),
                proxy: Some(proxy),
            },
        )
    }

    fn restart_carrier(&mut self) -> Result<PrivateAccessCarrierRestart> {
        let receipt = self.sing_box.restart()?;
        if receipt.report().replaced_existing() {
            Ok(PrivateAccessCarrierRestart {
                progress_message: format!("sing-box 重启成功: {}", receipt.report().transition()),
                summary: format!("restarted sing-box {}", receipt.report().transition()),
            })
        } else {
            Ok(PrivateAccessCarrierRestart {
                progress_message: format!(
                    "sing-box 启动成功: {}",
                    receipt.report().started_process()
                ),
                summary: format!("started sing-box {}", receipt.report().started_process()),
            })
        }
    }

    fn refresh_system_proxy_bypass(&mut self, dynamic_entries: &[String]) -> Result<bool> {
        self.system_proxy
            .refresh_bypass(self.base_bypass_entries, dynamic_entries)
    }
}

impl App {
    pub(super) fn connect_private_access_with_terminal_prompt(
        &mut self,
        terminal: &mut DefaultTerminal,
    ) -> Result<()> {
        self.open_private_access_progress();
        self.push_private_access_progress(
            PrivateAccessProgressTone::Info,
            "正在连接内网服务器...".to_string(),
        );
        self.push_private_access_progress(
            PrivateAccessProgressTone::Info,
            "需要管理员权限创建 TUN 接口，正在预授权 sudo...".to_string(),
        );
        terminal.draw(|frame| draw(frame, self))?;
        suspend_terminal_for_prompt(
            terminal,
            "Private Access needs administrator authorization for its TUN helper.",
        )?;
        let authorization = Command::new("sudo")
            .arg("-v")
            .status()
            .context("failed to start sudo authorization for Private Access TUN helper");
        resume_terminal_after_prompt(terminal)?;
        let status = authorization?;
        if !status.success() {
            let message = format!("Private Access TUN helper sudo authorization failed: {status}");
            self.fail_private_access_progress(message.clone());
            self.set_status_only(message);
            return Ok(());
        }
        self.push_private_access_progress(
            PrivateAccessProgressTone::Success,
            "sudo 预授权成功，继续登录...".to_string(),
        );
        self.toggle_private_access()
    }

    pub(super) fn ensure_private_access_tun_baseline(&self) -> Result<bool> {
        if !self.system_proxy_config_path.exists() {
            return Ok(false);
        }
        if !self
            .private_access
            .profiles
            .iter()
            .any(|profile| matches!(profile.mode, PrivateAccessMode::Tun))
        {
            return Ok(false);
        }
        let carrier_domains = self
            .private_access
            .profiles
            .iter()
            .filter(|profile| matches!(profile.mode, PrivateAccessMode::Tun))
            .map(|profile| profile.server.trim().to_ascii_lowercase())
            .filter(|server| !server.is_empty())
            .collect::<Vec<_>>();
        run_private_access_tun_baseline_config(
            &self.system_proxy_config_path,
            true,
            &carrier_domains,
        )
    }

    pub(super) fn handle_private_access_auth_key(&mut self, code: KeyCode) -> Result<bool> {
        match code {
            KeyCode::Esc => self.cancel_private_access_auth()?,
            KeyCode::Tab | KeyCode::Down => {
                if let Some(auth) = self.private_access_auth.as_mut() {
                    auth.field_index =
                        (auth.field_index + 1).min(auth.fields.len().saturating_sub(1));
                    auth.error = None;
                }
            }
            KeyCode::BackTab | KeyCode::Up => {
                if let Some(auth) = self.private_access_auth.as_mut() {
                    auth.field_index = auth.field_index.saturating_sub(1);
                    auth.error = None;
                }
            }
            KeyCode::Left | KeyCode::Right => {
                if let Some(auth) = self.private_access_auth.as_mut()
                    && let Some(field) = auth.fields.get(auth.field_index)
                    && !field.options.is_empty()
                {
                    let current = auth.inputs[auth.field_index].as_str();
                    let current_index = field
                        .options
                        .iter()
                        .position(|option| option.value == current)
                        .unwrap_or(0);
                    let next = if matches!(code, KeyCode::Left) {
                        current_index.saturating_sub(1)
                    } else {
                        (current_index + 1).min(field.options.len() - 1)
                    };
                    auth.inputs[auth.field_index] = field.options[next].value.clone();
                    auth.error = None;
                }
            }
            KeyCode::Enter => {
                let submit = self
                    .private_access_auth
                    .as_ref()
                    .is_some_and(|auth| auth.field_index + 1 >= auth.fields.len());
                if submit {
                    self.submit_private_access_auth()?;
                } else if let Some(auth) = self.private_access_auth.as_mut() {
                    auth.field_index += 1;
                    auth.error = None;
                }
            }
            KeyCode::Backspace => {
                if let Some(auth) = self.private_access_auth.as_mut()
                    && let Some(input) = auth.inputs.get_mut(auth.field_index)
                {
                    input.pop();
                    auth.error = None;
                }
            }
            KeyCode::Char(ch) => {
                if let Some(auth) = self.private_access_auth.as_mut()
                    && let Some(input) = auth.inputs.get_mut(auth.field_index)
                {
                    input.push(ch);
                    auth.error = None;
                }
            }
            _ => {}
        }
        Ok(true)
    }

    fn submit_private_access_auth(&mut self) -> Result<()> {
        let Some(auth) = self.private_access_auth.as_mut() else {
            return Ok(());
        };
        if let Some((index, field)) = auth
            .fields
            .iter()
            .enumerate()
            .find(|(index, field)| field.required && auth.inputs[*index].trim().is_empty())
        {
            auth.field_index = index;
            auth.error = Some(format!("{} is required", field.label));
            return Ok(());
        }

        let mut auth = self
            .private_access_auth
            .take()
            .expect("private access auth modal exists");
        let replies = std::mem::take(&mut auth.inputs)
            .into_iter()
            .map(PrivateAccessSecret::new)
            .collect::<Vec<_>>();
        let button = auth
            .buttons
            .iter()
            .find(|button| button.eq_ignore_ascii_case("ok"))
            .or_else(|| {
                auth.buttons
                    .iter()
                    .find(|button| !button.eq_ignore_ascii_case("cancel"))
            })
            .cloned()
            .unwrap_or_else(|| "ok".to_string());
        let submitted = match self.private_access.submit_authentication(
            auth.profile_index,
            auth.service.clone(),
            auth.session_id.clone(),
            auth.challenge_id.clone(),
            button,
            replies,
        ) {
            Ok(submitted) => submitted,
            Err(error) => {
                self.set_status_only(format!(
                    "Failed to submit Private Access authentication: {error}"
                ));
                return Ok(());
            }
        };
        if !submitted {
            self.set_status_only("Private Access authentication session closed");
            return Ok(());
        }
        self.set_status_only("Private Access authentication submitted");
        Ok(())
    }

    fn cancel_private_access_auth(&mut self) -> Result<()> {
        let Some(auth) = self.private_access_auth.take() else {
            return Ok(());
        };
        self.private_access.cancel_authentication(
            auth.profile_index,
            auth.service.clone(),
            auth.session_id.clone(),
            auth.challenge_id.clone(),
        )?;
        self.set_status_only("Private Access authentication cancelled");
        Ok(())
    }

    pub(super) fn private_access_connect_needs_terminal_prompt(&self) -> bool {
        let Some(profile) = self.private_access.focused_opt() else {
            return false;
        };
        matches!(
            profile.state,
            PrivateAccessState::Disabled
                | PrivateAccessState::Disconnected
                | PrivateAccessState::Error
        ) && matches!(profile.mode, PrivateAccessMode::Tun)
            && self
                .private_access_tun_helper_for_connect(profile)
                .is_some_and(|command| helper_command_uses_sudo(&command))
    }

    fn private_access_tun_helper_for_connect(
        &self,
        profile: &PrivateAccessProfileRuntime,
    ) -> Option<Vec<String>> {
        if !matches!(profile.mode, PrivateAccessMode::Tun) {
            return None;
        }
        if !profile.tun_helper.is_empty() {
            return Some(make_sudo_command_noninteractive(profile.tun_helper.clone()));
        }
        Some(default_tui_tun_helper_command())
    }

    pub(super) fn detach_private_access_for_background(&mut self) -> Result<usize> {
        self.private_access
            .detach_for_background(private_access_process_exists)
    }

    fn open_private_access_progress(&mut self) {
        self.open_private_access_progress_for_profile(self.private_access.focused_index);
    }

    fn open_private_access_progress_for_profile(&mut self, profile_index: usize) {
        let Some(profile) = self.private_access.profiles.get(profile_index) else {
            return;
        };
        self.private_access_progress = Some(PrivateAccessProgressModal {
            profile_index,
            title: private_access_progress_title(profile),
            entries: Vec::new(),
            done: false,
        });
        self.flash = None;
    }

    fn push_private_access_progress(&mut self, tone: PrivateAccessProgressTone, text: String) {
        self.push_private_access_progress_for_profile(
            self.private_access.focused_index,
            tone,
            text,
        );
    }

    fn push_private_access_progress_for_profile(
        &mut self,
        profile_index: usize,
        tone: PrivateAccessProgressTone,
        text: String,
    ) {
        if !matches!(
            self.private_access_progress.as_ref(),
            Some(progress) if progress.profile_index == profile_index
        ) {
            self.open_private_access_progress_for_profile(profile_index);
        }
        let Some(progress) = self.private_access_progress.as_mut() else {
            return;
        };
        if progress.profile_index != profile_index {
            return;
        }
        if progress
            .entries
            .last()
            .is_some_and(|entry| entry.tone == tone && entry.text == text)
        {
            return;
        }
        progress
            .entries
            .push(PrivateAccessProgressEntry { tone, text });
    }

    fn append_private_access_progress_for_profile(
        &mut self,
        profile_index: usize,
        tone: PrivateAccessProgressTone,
        text: String,
    ) {
        if matches!(
            self.private_access_progress.as_ref(),
            Some(progress) if progress.profile_index == profile_index
        ) {
            self.push_private_access_progress_for_profile(profile_index, tone, text);
        }
    }

    fn finish_private_access_progress(&mut self) {
        self.finish_private_access_progress_for_profile(self.private_access.focused_index);
    }

    fn finish_private_access_progress_for_profile(&mut self, profile_index: usize) {
        if let Some(progress) = self.private_access_progress.as_mut()
            && progress.profile_index == profile_index
        {
            progress.done = true;
        }
    }

    fn fail_private_access_progress(&mut self, message: String) {
        self.fail_private_access_progress_for_profile(self.private_access.focused_index, message);
    }

    fn fail_private_access_progress_for_profile(&mut self, profile_index: usize, message: String) {
        self.push_private_access_progress_for_profile(
            profile_index,
            PrivateAccessProgressTone::Error,
            message,
        );
        self.finish_private_access_progress_for_profile(profile_index);
    }

    pub(super) fn toggle_private_access_with_progress(&mut self) -> Result<()> {
        if !self.private_access.is_configured() {
            self.set_status_only("Private Access is not configured");
            return Ok(());
        }
        self.open_private_access_progress();
        match self.private_access.focused().state {
            PrivateAccessState::Connected | PrivateAccessState::Connecting => {
                self.push_private_access_progress(
                    PrivateAccessProgressTone::Info,
                    "正在断开内网连接...".to_string(),
                );
            }
            PrivateAccessState::Disconnecting => {
                self.push_private_access_progress(
                    PrivateAccessProgressTone::Info,
                    "正在等待断开完成...".to_string(),
                );
            }
            PrivateAccessState::Disabled
            | PrivateAccessState::Disconnected
            | PrivateAccessState::Error => {
                self.push_private_access_progress(
                    PrivateAccessProgressTone::Info,
                    "正在连接内网服务器...".to_string(),
                );
            }
        }
        self.toggle_private_access()
    }

    fn toggle_private_access(&mut self) -> Result<()> {
        if !self.private_access.is_configured() {
            self.set_status_only("Private Access is not configured");
            return Ok(());
        }
        match self.private_access.focused().state {
            PrivateAccessState::Connected | PrivateAccessState::Connecting => {
                self.disconnect_private_access()
            }
            PrivateAccessState::Disconnecting => {
                self.set_status_only("Private Access disconnect is already running");
                Ok(())
            }
            PrivateAccessState::Disabled
            | PrivateAccessState::Disconnected
            | PrivateAccessState::Error => self.connect_private_access(),
        }
    }

    fn connect_private_access(&mut self) -> Result<()> {
        if !self.private_access.is_configured() {
            self.set_status_only("Private Access is not configured");
            return Ok(());
        }

        let profile = self.private_access.focused();
        let service = profile.manifest.id.clone();
        let tun_helper = self.private_access_tun_helper_for_connect(profile);
        let (
            http_connect_proxy,
            http_connect_proxy_context,
            http_connect_controller,
            http_connect_selector,
        ) = if service == "sonicwall" {
            sonicwall_http_connect_settings(
                profile.use_internet_proxy,
                self.system_proxy.server(),
                self.internet_outbound_context(),
                &self.client.base_url,
                self.internet_outbound_root_selector(),
            )
        } else {
            (None, None, None, None)
        };
        let blocker = if service == "sonicwall" {
            let processes = running_official_sonicwall_client_processes();
            (!processes.is_empty()).then(|| format_official_sonicwall_client_warning(&processes))
        } else {
            None
        };
        let options = PrivateAccessConnectOptions {
            tun_helper,
            http_connect_proxy,
            http_connect_proxy_context,
            http_connect_controller,
            http_connect_selector,
            blocker,
        };

        match self.private_access.connect(options) {
            Ok(started) => {
                self.set_status_only(format!(
                    "Private Access {} ({}) connecting...",
                    started.profile_id, started.service
                ));
                self.save_runtime_state()?;
            }
            Err(error) => {
                let message = error.to_string();
                self.fail_private_access_progress(message.clone());
                self.set_status_only(message);
            }
        }
        Ok(())
    }

    fn disconnect_private_access(&mut self) -> Result<()> {
        if !self.private_access.is_configured() {
            self.set_status_only("Private Access is not configured");
            return Ok(());
        }
        match self.private_access.disconnect() {
            Ok(PrivateAccessDisconnectOutcome::Started {
                profile_id,
                service,
            }) => self.set_status_only(format!(
                "Private Access {profile_id} ({service}) disconnecting..."
            )),
            Ok(PrivateAccessDisconnectOutcome::AlreadyDisconnected) => {
                self.push_private_access_progress(
                    PrivateAccessProgressTone::Success,
                    "内网连接已断开".to_string(),
                );
                self.finish_private_access_progress();
                self.set_status_only("Private Access is already disconnected");
            }
            Ok(PrivateAccessDisconnectOutcome::BackgroundOwned(pid)) => {
                let message = format!(
                    "Private Access is running in background pid {pid}; this TUI no longer owns its service session"
                );
                self.push_private_access_progress(PrivateAccessProgressTone::Info, message.clone());
                self.finish_private_access_progress();
                self.set_status_only(message);
            }
            Err(error) => {
                let message = error.to_string();
                self.fail_private_access_progress(message.clone());
                self.set_status_only(message);
            }
        }
        Ok(())
    }

    pub(super) fn poll_private_access_updates(&mut self) -> Result<()> {
        let notices = {
            let mut integration = TuiPrivateAccessNetworkIntegration {
                config_path: &self.system_proxy_config_path,
                sing_box: &mut self.sing_box,
                system_proxy: &mut self.system_proxy,
                base_bypass_entries: &self.bypass_entries,
            };
            self.private_access.poll(&mut integration)?
        };
        self.apply_private_access_session_notices(notices);
        Ok(())
    }

    fn apply_private_access_session_notices(&mut self, notices: Vec<PrivateAccessSessionNotice>) {
        for notice in notices {
            match notice {
                PrivateAccessSessionNotice::Progress {
                    profile_index,
                    tone,
                    text,
                    done,
                    append_only,
                } => {
                    let tone = match tone {
                        PrivateAccessNoticeTone::Info => PrivateAccessProgressTone::Info,
                        PrivateAccessNoticeTone::Success => PrivateAccessProgressTone::Success,
                        PrivateAccessNoticeTone::Error => PrivateAccessProgressTone::Error,
                    };
                    if append_only {
                        self.append_private_access_progress_for_profile(profile_index, tone, text);
                    } else {
                        self.push_private_access_progress_for_profile(profile_index, tone, text);
                    }
                    if done {
                        self.finish_private_access_progress_for_profile(profile_index);
                    }
                }
                PrivateAccessSessionNotice::Status(message) => self.set_status_only(message),
                PrivateAccessSessionNotice::Flash(message) => {
                    self.set_status_with_flash(truncate_for_width(&message, 100));
                }
                PrivateAccessSessionNotice::ClearAuthentication { profile_index } => {
                    if self
                        .private_access_auth
                        .as_ref()
                        .is_some_and(|auth| auth.profile_index == profile_index)
                    {
                        self.private_access_auth = None;
                    }
                }
                PrivateAccessSessionNotice::Authentication(request) => {
                    let inputs = request
                        .fields
                        .iter()
                        .map(|field| {
                            private_access_auth_initial_value(
                                &self.private_access.profiles[request.profile_index],
                                field,
                            )
                        })
                        .collect();
                    self.private_access_auth = Some(PrivateAccessAuthModal {
                        profile_index: request.profile_index,
                        service: request.service,
                        session_id: request.session_id,
                        challenge_id: request.challenge_id,
                        title: request.title,
                        message: request.message,
                        fields: request.fields,
                        buttons: request.buttons,
                        inputs,
                        field_index: 0,
                        error: None,
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::super::test_support::{
        private_access_progress_text, test_app, test_app_without_private_access, test_state_path,
    };
    use super::*;
    use crate::private_access::default_hillstone_manifest;
    use serde_json::json;

    #[test]
    fn process_matcher_requires_the_expected_private_access_service_command() {
        let manifest = default_hillstone_manifest().expect("manifest builds");
        let executable = &manifest.executable;
        assert!(command_matches_private_access_service(
            &format!("{executable} private-access-service hillstone --stdio"),
            &manifest
        ));
        assert!(!command_matches_private_access_service(
            "sleep 3600",
            &manifest
        ));
        assert!(!command_matches_private_access_service(
            &format!("{executable} run --headless-auto-pick"),
            &manifest
        ));
    }

    #[test]
    fn missing_profiles_are_reported_without_loading_a_default() {
        let mut app = test_app_without_private_access();

        app.toggle_private_access_with_progress()
            .expect("missing private access profiles is handled");

        assert!(!app.private_access.is_configured());
        assert!(app.private_access_progress.is_none());
        assert_eq!(app.status, "Private Access is not configured");
        assert!(app.runtime_state().private_access_profiles.is_empty());
    }

    #[test]
    fn missing_settings_finish_the_progress_modal_with_the_error() {
        let mut app = test_app();
        app.private_access.focused_mut().server.clear();
        app.private_access.focused_mut().username.clear();

        app.toggle_private_access_with_progress()
            .expect("private access missing settings is handled");

        assert!(
            private_access_progress_text(&app)
                .contains("请先在 settings 中配置 Private Access server")
        );
        assert!(
            app.private_access_progress
                .as_ref()
                .is_some_and(|progress| progress.done)
        );
        assert!(!app.private_access.focused().owns_process());
    }

    #[test]
    fn progress_title_follows_the_event_profile_instead_of_focus() {
        let mut app = test_app();
        app.private_access
            .profiles
            .push(PrivateAccessProfileRuntime::default_sonicwall().expect("SonicWall profile"));
        app.private_access.focused_index = 1;
        app.open_private_access_progress();
        assert_eq!(
            app.private_access_progress
                .as_ref()
                .expect("focused progress")
                .title,
            "Private Access - sonicwall (tun)"
        );

        app.push_private_access_progress_for_profile(
            0,
            PrivateAccessProgressTone::Error,
            "连接失败: session_failed".to_string(),
        );

        let progress = app
            .private_access_progress
            .as_ref()
            .expect("event profile progress");
        assert_eq!(progress.profile_index, 0);
        assert_eq!(progress.title, "Private Access - hillstone (tun)");
        assert!(private_access_progress_text(&app).contains("session_failed"));
    }

    #[test]
    fn background_progress_does_not_open_a_modal() {
        let mut app = test_app();

        app.append_private_access_progress_for_profile(
            0,
            PrivateAccessProgressTone::Info,
            "Internet proxy selection changed; migrating the SonicWall data tunnel".to_string(),
        );
        app.append_private_access_progress_for_profile(
            0,
            PrivateAccessProgressTone::Success,
            "TUN 路由和按域名分流 DNS 已生效".to_string(),
        );
        app.finish_private_access_progress_for_profile(0);

        assert!(app.private_access_progress.is_none());
    }

    #[test]
    fn service_spawn_failure_remains_observable_in_tui_state() {
        let mut app = test_app();
        app.private_access.focused_mut().server = "sslvpn.example.com".to_string();
        app.private_access.focused_mut().username = "alice".to_string();
        app.private_access.focused_mut().manifest.executable =
            "/path/that/does/not/exist/private-access-service".to_string();

        app.toggle_private_access_with_progress()
            .expect("private access spawn failure is handled");

        assert_eq!(
            app.private_access.focused().state,
            PrivateAccessState::Error
        );
        assert!(!app.private_access.focused().owns_process());
        assert!(private_access_progress_text(&app).contains("启动 Private Access service 失败"));
        assert!(
            app.private_access_progress
                .as_ref()
                .is_some_and(|progress| progress.done)
        );
    }

    #[test]
    fn sonicwall_conflict_warning_names_the_official_clients() {
        let warning = format_official_sonicwall_client_warning(&[
            "SnwlVpn.exe".to_string(),
            "SnwlConnect.exe".to_string(),
        ]);
        assert!(warning.contains("SnwlVpn.exe"));
        assert!(warning.contains("SnwlConnect.exe"));
        assert!(warning.contains("SonicWall"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_tasklist_parser_extracts_image_names() {
        let names = parse_windows_tasklist_image_names(
            "\"SnwlVpn.exe\",\"5596\",\"Console\",\"1\",\"12,344 K\"\r\n\
             \"SnwlConnect.exe\",\"8604\",\"Console\",\"1\",\"80,120 K\"\r\n",
        );
        assert_eq!(names, vec!["SnwlVpn.exe", "SnwlConnect.exe"]);
    }

    #[test]
    fn baseline_skips_configs_without_tun_profiles() {
        let config_path = test_state_path();
        let original = r#"{"route":{"auto_detect_interface":true}}"#;
        fs::write(&config_path, original).expect("config writes");
        let mut app = test_app_without_private_access();
        app.system_proxy_config_path = config_path.clone();

        assert!(
            !app.ensure_private_access_tun_baseline()
                .expect("irrelevant baseline is skipped")
        );
        assert_eq!(
            fs::read_to_string(&config_path).expect("config reads"),
            original
        );

        let _ = fs::remove_file(config_path);
    }

    #[test]
    fn common_tun_baseline_is_ordered_and_idempotent() {
        let mut app = test_app();
        let mut hillstone =
            PrivateAccessProfileRuntime::default_hillstone().expect("Hillstone profile");
        hillstone.mode = PrivateAccessMode::Tun;
        hillstone.server = "sslvpn.geovisearth.com".to_string();
        app.private_access = PrivateAccessRuntime {
            profiles: vec![
                PrivateAccessProfileRuntime::default_sonicwall().expect("SonicWall profile"),
                hillstone,
            ],
            focused_index: 0,
        };
        let config_path = test_state_path();
        let config = json!({
            "outbounds": [
                { "type": "direct", "tag": "direct" },
                { "type": "selector", "tag": "select", "outbounds": ["direct"] }
            ],
            "route": {
                "rules": [
                    { "action": "hijack-dns", "protocol": "dns" },
                    {
                        "action": "route",
                        "rule_set": ["sing-box-tui-bypass"],
                        "outbound": "direct"
                    },
                    {
                        "action": "route",
                        "domain_suffix": ["hundsun.com"],
                        "outbound": "direct"
                    }
                ]
            }
        });
        fs::write(
            &config_path,
            serde_json::to_string_pretty(&config).expect("config serializes"),
        )
        .expect("config writes");
        app.system_proxy_config_path = config_path.clone();

        assert!(
            app.ensure_private_access_tun_baseline()
                .expect("TUN baseline is written")
        );
        let text = fs::read_to_string(&config_path).expect("config reads");
        let config: serde_json::Value = serde_json::from_str(&text).expect("config parses");
        let rules = config["route"]["rules"].as_array().expect("route rules");
        let carrier_index = rules
            .iter()
            .position(|rule| {
                rule["domain"] == json!(["sslvpn.geovisearth.com", "sslvpn.hundsun.com"])
            })
            .expect("common carrier rule exists");
        let bypass_index = rules
            .iter()
            .position(|rule| rule["rule_set"] == json!(["sing-box-tui-bypass"]))
            .expect("generic bypass rule exists");
        let internal_index = rules
            .iter()
            .position(|rule| rule["domain_suffix"] == json!(["hundsun.com"]))
            .expect("internal domain rule exists");
        assert!(carrier_index < bypass_index);
        assert!(carrier_index < internal_index);
        assert_eq!(rules[carrier_index]["outbound"], "select");
        assert_eq!(config["route"]["auto_detect_interface"], false);
        assert!(
            rules
                .iter()
                .any(|rule| { rule["ip_is_private"] == true && rule["outbound"] == "direct" })
        );
        assert!(
            config["dns"]["servers"]
                .as_array()
                .is_some_and(|servers| servers.iter().any(|server| {
                    server["tag"] == "private-access-system" && server["type"] == "local"
                }))
        );
        assert!(
            !app.ensure_private_access_tun_baseline()
                .expect("TUN baseline is idempotent")
        );
        let _ = fs::remove_file(config_path);
    }

    #[test]
    fn tun_baseline_does_not_write_dynamic_routes() {
        let mut app = test_app();
        app.private_access.focused_mut().mode = PrivateAccessMode::Tun;
        app.private_access.focused_mut().server = "sslvpn.example.com".to_string();
        let config_path = test_state_path();
        let config = json!({
            "outbounds": [
                { "type": "direct", "tag": "direct" },
                { "type": "selector", "tag": "select", "outbounds": ["direct"] }
            ],
            "route": { "rules": [] }
        });
        fs::write(
            &config_path,
            serde_json::to_string_pretty(&config).expect("config serializes"),
        )
        .expect("config writes");
        app.system_proxy_config_path = config_path.clone();

        app.ensure_private_access_tun_baseline()
            .expect("TUN baseline applies");

        let text = fs::read_to_string(&config_path).expect("config reads");
        let config: serde_json::Value = serde_json::from_str(&text).expect("config parses");
        assert_eq!(config["route"]["auto_detect_interface"], false);
        let rules = config["route"]["rules"].as_array().expect("route rules");
        assert!(!rules.iter().any(|rule| rule.get("ip_cidr").is_some()));
        let _ = fs::remove_file(config_path);
    }

    #[test]
    fn sudo_tun_helper_is_made_noninteractive_and_requires_preauthorization() {
        let mut app = test_app();
        app.private_access.focused_mut().mode = PrivateAccessMode::Tun;
        app.private_access.focused_mut().tun_helper = vec![
            "sudo".to_string(),
            "target/debug/sing-box-tui".to_string(),
            "private-access-tun-helper".to_string(),
            "--stdio".to_string(),
        ];

        assert!(app.private_access_connect_needs_terminal_prompt());
        assert!(
            app.private_access_tun_helper_for_connect(app.private_access.focused())
                .is_some_and(|command| command.iter().any(|arg| arg == "-n"))
        );

        app.private_access.focused_mut().tun_helper[0] = "/usr/bin/sudo".to_string();
        assert!(app.private_access_connect_needs_terminal_prompt());
        assert!(
            app.private_access_tun_helper_for_connect(app.private_access.focused())
                .is_some_and(|command| command.get(1).is_some_and(|arg| arg == "-n"))
        );
    }

    #[test]
    fn missing_persisted_helper_uses_the_tui_helper_without_persisting_it() {
        let mut app = test_app();
        app.private_access.focused_mut().mode = PrivateAccessMode::Tun;
        app.private_access.focused_mut().tun_helper.clear();

        let command = app
            .private_access_tun_helper_for_connect(app.private_access.focused())
            .expect("tun helper command");
        #[cfg(unix)]
        {
            assert!(app.private_access_connect_needs_terminal_prompt());
            assert_eq!(command.first().map(String::as_str), Some("sudo"));
            assert!(command.iter().any(|arg| arg == "-n"));
        }
        #[cfg(not(unix))]
        {
            assert!(!app.private_access_connect_needs_terminal_prompt());
            assert_ne!(command.first().map(String::as_str), Some("sudo"));
        }
        assert!(command.iter().any(|arg| arg == "private-access-tun-helper"));
        assert!(command.iter().any(|arg| arg == "--stdio"));
        assert!(
            app.runtime_state().private_access_profiles[0]
                .tun_helper
                .is_none()
        );
    }
}

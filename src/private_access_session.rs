#[cfg(test)]
use std::env;
use std::net::SocketAddrV4;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::json;

use crate::private_access::{
    ExternalPrivateAccessService, PrivateAccessAuthField, PrivateAccessBridge,
    PrivateAccessCommand, PrivateAccessEvent, PrivateAccessEventEnvelope, PrivateAccessRoute,
    PrivateAccessSecret, PrivateAccessServiceManifest, PrivateAccessState,
    default_hillstone_manifest, default_sonicwall_manifest, load_private_access_manifest,
};
use crate::tui_state::{PrivateAccessProfileState, TuiRuntimeState, parse_bypass_entries};

const EVENTS_PER_POLL: usize = 64;

trait PrivateAccessServiceProcess {
    fn service_id(&self) -> &str;
    fn pid(&self) -> u32;
    fn send(&mut self, command: &PrivateAccessCommand) -> Result<()>;
    fn detach(&mut self) -> Result<()>;
    fn try_recv(&self) -> Result<Option<PrivateAccessEventEnvelope>, String>;
    fn stop(self: Box<Self>) -> Result<()>;
}

impl PrivateAccessServiceProcess for ExternalPrivateAccessService {
    fn service_id(&self) -> &str {
        self.service_id()
    }

    fn pid(&self) -> u32 {
        self.pid()
    }

    fn send(&mut self, command: &PrivateAccessCommand) -> Result<()> {
        self.send(command)
    }

    fn detach(&mut self) -> Result<()> {
        self.detach()
    }

    fn try_recv(&self) -> Result<Option<PrivateAccessEventEnvelope>, String> {
        self.try_recv()
    }

    fn stop(self: Box<Self>) -> Result<()> {
        (*self).stop()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PrivateAccessMode {
    Bridge,
    Tun,
}

impl PrivateAccessMode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Bridge => "bridge",
            Self::Tun => "tun",
        }
    }
}

pub(crate) fn parse_private_access_mode(value: &str) -> Result<PrivateAccessMode> {
    match value.trim().to_ascii_lowercase().as_str() {
        "bridge" | "http_bridge" | "http-bridge" => Ok(PrivateAccessMode::Bridge),
        "tun" => Ok(PrivateAccessMode::Tun),
        _ => bail!("private access mode must be bridge or tun"),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PrivateAccessNoticeTone {
    Info,
    Success,
    Error,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PrivateAccessAuthRequest {
    pub(crate) profile_index: usize,
    pub(crate) service: String,
    pub(crate) session_id: String,
    pub(crate) challenge_id: String,
    pub(crate) title: String,
    pub(crate) message: String,
    pub(crate) fields: Vec<PrivateAccessAuthField>,
    pub(crate) buttons: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PrivateAccessSessionNotice {
    Progress {
        profile_index: usize,
        tone: PrivateAccessNoticeTone,
        text: String,
        done: bool,
        append_only: bool,
    },
    Status(String),
    Flash(String),
    Authentication(PrivateAccessAuthRequest),
    ClearAuthentication {
        profile_index: usize,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PrivateAccessBridgeRouteUpdate {
    pub(crate) profile_id: String,
    pub(crate) routes: Vec<PrivateAccessRoute>,
    pub(crate) domains: Vec<String>,
    pub(crate) domain_suffixes: Vec<String>,
    pub(crate) previous_routes: Vec<PrivateAccessRoute>,
    pub(crate) previous_domains: Vec<String>,
    pub(crate) previous_domain_suffixes: Vec<String>,
    pub(crate) carrier_domains: Vec<String>,
    pub(crate) bridge: Option<PrivateAccessBridge>,
    pub(crate) fallback_listen: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PrivateAccessCarrierRestart {
    pub(crate) progress_message: String,
    pub(crate) summary: String,
}

pub(crate) trait PrivateAccessNetworkIntegration {
    fn apply_bridge_routes(&mut self, update: &PrivateAccessBridgeRouteUpdate) -> Result<bool>;

    fn restart_carrier(&mut self) -> Result<PrivateAccessCarrierRestart>;

    fn refresh_system_proxy_bypass(&mut self, dynamic_entries: &[String]) -> Result<bool>;
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct PrivateAccessConnectOptions {
    pub(crate) tun_helper: Option<Vec<String>>,
    pub(crate) http_connect_proxy: Option<String>,
    pub(crate) http_connect_proxy_context: Option<String>,
    pub(crate) http_connect_controller: Option<String>,
    pub(crate) http_connect_selector: Option<String>,
    pub(crate) blocker: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PrivateAccessConnectStarted {
    pub(crate) profile_id: String,
    pub(crate) service: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PrivateAccessDisconnectOutcome {
    Started { profile_id: String, service: String },
    AlreadyDisconnected,
    BackgroundOwned(u32),
}

pub(crate) struct PrivateAccessProfileRuntime {
    pub(crate) id: String,
    pub(crate) manifest_path: Option<String>,
    pub(crate) mode: PrivateAccessMode,
    pub(crate) manifest: PrivateAccessServiceManifest,
    process: Option<Box<dyn PrivateAccessServiceProcess>>,
    pub(crate) state: PrivateAccessState,
    pub(crate) server: String,
    pub(crate) port: u16,
    pub(crate) username: String,
    pub(crate) password: String,
    pub(crate) password_env: String,
    pub(crate) bridge_listen: String,
    pub(crate) tun_helper: Vec<String>,
    pub(crate) tls_verify: bool,
    pub(crate) use_internet_proxy: bool,
    pub(crate) routes: Vec<PrivateAccessRoute>,
    pub(crate) dns: Vec<String>,
    pub(crate) domains: Vec<String>,
    pub(crate) domain_suffixes: Vec<String>,
    pub(crate) bridge: Option<PrivateAccessBridge>,
    pub(crate) last_error: Option<String>,
    integration_failed: bool,
    pub(crate) background_pid: Option<u32>,
}

impl PrivateAccessProfileRuntime {
    #[cfg(test)]
    pub(crate) fn default_hillstone() -> Result<Self> {
        let manifest_path = env::var("SING_BOX_TUI_PRIVATE_ACCESS_MANIFEST")
            .ok()
            .filter(|path| !path.trim().is_empty());
        let manifest = load_manifest_for_profile("hillstone", manifest_path.as_deref())?;
        Ok(Self {
            id: "hillstone".to_string(),
            manifest_path,
            mode: PrivateAccessMode::Tun,
            manifest,
            process: None,
            state: PrivateAccessState::Disconnected,
            server: env::var("HILLSTONE_SERVER").unwrap_or_default(),
            port: 4433,
            username: env::var("HILLSTONE_USERNAME").unwrap_or_default(),
            password: String::new(),
            password_env: "HILLSTONE_PASSWORD".to_string(),
            bridge_listen: "127.0.0.1:16780".to_string(),
            tun_helper: Vec::new(),
            tls_verify: false,
            use_internet_proxy: false,
            routes: Vec::new(),
            dns: Vec::new(),
            domains: Vec::new(),
            domain_suffixes: Vec::new(),
            bridge: None,
            last_error: None,
            integration_failed: false,
            background_pid: None,
        })
    }

    #[cfg(test)]
    pub(crate) fn default_sonicwall() -> Result<Self> {
        let manifest_path = env::var("SING_BOX_TUI_SONICWALL_MANIFEST")
            .ok()
            .filter(|path| !path.trim().is_empty());
        let manifest = load_manifest_for_profile("sonicwall", manifest_path.as_deref())?;
        Ok(Self {
            id: "sonicwall".to_string(),
            manifest_path,
            mode: PrivateAccessMode::Tun,
            manifest,
            process: None,
            state: PrivateAccessState::Disconnected,
            server: "sslvpn.hundsun.com".to_string(),
            port: 443,
            username: String::new(),
            password: String::new(),
            password_env: String::new(),
            bridge_listen: String::new(),
            tun_helper: Vec::new(),
            tls_verify: true,
            use_internet_proxy: false,
            routes: Vec::new(),
            dns: Vec::new(),
            domains: Vec::new(),
            domain_suffixes: Vec::new(),
            bridge: None,
            last_error: None,
            integration_failed: false,
            background_pid: None,
        })
    }

    fn from_state(
        state: PrivateAccessProfileState,
        process_is_alive: &impl Fn(u32, &PrivateAccessServiceManifest) -> bool,
    ) -> Result<Self> {
        let id = normalize_optional_setting(Some(state.id.clone()))
            .unwrap_or_else(|| "hillstone".to_string());
        let manifest_path = normalize_optional_setting(state.manifest_path.clone());
        let manifest = load_manifest_for_profile(&id, manifest_path.as_deref())?;
        let is_sonicwall = manifest.id == "sonicwall";
        let mut profile = Self {
            id,
            manifest_path,
            mode: PrivateAccessMode::Tun,
            manifest,
            process: None,
            state: PrivateAccessState::Disconnected,
            server: if is_sonicwall {
                "sslvpn.hundsun.com".to_string()
            } else {
                String::new()
            },
            port: if is_sonicwall { 443 } else { 4433 },
            username: String::new(),
            password: String::new(),
            password_env: String::new(),
            bridge_listen: if is_sonicwall {
                String::new()
            } else {
                "127.0.0.1:16780".to_string()
            },
            tun_helper: Vec::new(),
            tls_verify: is_sonicwall,
            use_internet_proxy: false,
            routes: Vec::new(),
            dns: Vec::new(),
            domains: Vec::new(),
            domain_suffixes: Vec::new(),
            bridge: None,
            last_error: None,
            integration_failed: false,
            background_pid: None,
        };
        profile.apply_state(state, process_is_alive)?;
        if profile.manifest.id == "sonicwall" {
            profile.mode = PrivateAccessMode::Tun;
        }
        Ok(profile)
    }

    fn apply_state(
        &mut self,
        state: PrivateAccessProfileState,
        process_is_alive: &impl Fn(u32, &PrivateAccessServiceManifest) -> bool,
    ) -> Result<()> {
        if let Some(value) = normalize_optional_setting(state.mode) {
            self.mode = parse_private_access_mode(&value)?;
        }
        if let Some(value) = normalize_optional_setting(state.server) {
            self.server = value;
        }
        if let Some(value) = state.port.filter(|value| *value > 0) {
            self.port = value;
        }
        if let Some(value) = normalize_optional_setting(state.username) {
            self.username = value;
        }
        if let Some(value) = normalize_optional_setting(state.password) {
            self.password = value;
        }
        if let Some(value) = normalize_optional_setting(state.password_env) {
            self.password_env = value;
        }
        if let Some(value) = normalize_optional_setting(state.bridge_listen) {
            self.bridge_listen = value;
        }
        if let Some(values) = state.tun_helper {
            self.tun_helper = values
                .into_iter()
                .filter(|value| !value.trim().is_empty())
                .collect();
        }
        self.tls_verify = state.tls_verify;
        self.use_internet_proxy = state.use_internet_proxy;
        self.background_pid = state
            .background_pid
            .filter(|pid| process_is_alive(*pid, &self.manifest));
        if self.background_pid.is_some() {
            self.state = PrivateAccessState::Connected;
        }
        Ok(())
    }

    fn runtime_state(&self, process_exists: &impl Fn(u32) -> bool) -> PrivateAccessProfileState {
        PrivateAccessProfileState {
            id: self.id.clone(),
            manifest_path: self.manifest_path.clone(),
            mode: Some(self.mode.as_str().to_string()),
            server: normalize_optional_setting(Some(self.server.clone())),
            port: Some(self.port),
            username: normalize_optional_setting(Some(self.username.clone())),
            password: normalize_optional_setting(Some(self.password.clone())),
            password_env: normalize_optional_setting(Some(self.password_env.clone())),
            bridge_listen: normalize_optional_setting(Some(self.bridge_listen.clone())),
            tun_helper: (!self.tun_helper.is_empty()).then(|| self.tun_helper.clone()),
            tls_verify: self.tls_verify,
            use_internet_proxy: self.use_internet_proxy,
            background_pid: self.background_pid.filter(|pid| process_exists(*pid)),
        }
    }

    pub(crate) fn settings_locked(&self) -> bool {
        self.process.is_some()
            || matches!(
                self.state,
                PrivateAccessState::Connecting
                    | PrivateAccessState::Connected
                    | PrivateAccessState::Disconnecting
            )
    }

    pub(crate) fn owns_process(&self) -> bool {
        self.process.is_some()
    }
}

pub(crate) struct PrivateAccessRuntime {
    pub(crate) profiles: Vec<PrivateAccessProfileRuntime>,
    pub(crate) focused_index: usize,
}

impl PrivateAccessRuntime {
    pub(crate) fn new() -> Result<Self> {
        Ok(Self {
            profiles: Vec::new(),
            focused_index: 0,
        })
    }

    #[cfg(test)]
    pub(crate) fn with_default_hillstone() -> Result<Self> {
        Ok(Self {
            profiles: vec![PrivateAccessProfileRuntime::default_hillstone()?],
            focused_index: 0,
        })
    }

    pub(crate) fn is_configured(&self) -> bool {
        !self.profiles.is_empty()
    }

    pub(crate) fn focused(&self) -> &PrivateAccessProfileRuntime {
        &self.profiles[self.focused_index]
    }

    pub(crate) fn focused_mut(&mut self) -> &mut PrivateAccessProfileRuntime {
        &mut self.profiles[self.focused_index]
    }

    pub(crate) fn focused_opt(&self) -> Option<&PrivateAccessProfileRuntime> {
        self.profiles.get(self.focused_index)
    }

    #[cfg(test)]
    pub(crate) fn focused_id(&self) -> &str {
        self.focused().id.as_str()
    }

    pub(crate) fn set_focus_by_id(&mut self, id: &str) -> Result<bool> {
        let Some(index) = self.profiles.iter().position(|profile| profile.id == id) else {
            bail!("unknown private access profile: {id}");
        };
        let changed = self.focused_index != index;
        self.focused_index = index;
        Ok(changed)
    }

    pub(crate) fn apply_state(
        &mut self,
        state: &TuiRuntimeState,
        process_is_alive: impl Fn(u32, &PrivateAccessServiceManifest) -> bool,
    ) -> Result<()> {
        let mut profiles = Vec::new();
        for profile_state in state.private_access_profiles.clone() {
            profiles.push(PrivateAccessProfileRuntime::from_state(
                profile_state,
                &process_is_alive,
            )?);
        }
        self.profiles = profiles;
        self.focused_index = self
            .focused_index
            .min(self.profiles.len().saturating_sub(1));
        Ok(())
    }

    pub(crate) fn runtime_states(
        &self,
        process_exists: impl Fn(u32) -> bool,
    ) -> Vec<PrivateAccessProfileState> {
        self.profiles
            .iter()
            .map(|profile| profile.runtime_state(&process_exists))
            .collect()
    }

    pub(crate) fn system_proxy_bypass_entries(&self, base_entries: &[String]) -> Vec<String> {
        let mut entries = base_entries.to_vec();
        for entry in self.dynamic_system_proxy_bypass_entries() {
            if !entries.contains(&entry) {
                entries.push(entry);
            }
        }
        entries
    }

    pub(crate) fn connect(
        &mut self,
        options: PrivateAccessConnectOptions,
    ) -> Result<PrivateAccessConnectStarted> {
        let profile = self.focused_mut();
        if profile.server.trim().is_empty() {
            bail!("请先在 settings 中配置 Private Access server");
        }
        if profile.manifest.id != "sonicwall" && profile.username.trim().is_empty() {
            bail!("请先在 settings 中配置 Private Access username");
        }
        if matches!(profile.mode, PrivateAccessMode::Bridge)
            && let Err(error) = profile.bridge_listen.parse::<SocketAddrV4>()
        {
            bail!("Private Access bridge listen 无效: {error}");
        }
        if let Some(message) = options.blocker {
            bail!(message);
        }

        if profile.process.is_none() {
            match ExternalPrivateAccessService::spawn(profile.manifest.clone()) {
                Ok(process) => profile.process = Some(Box::new(process)),
                Err(error) => {
                    profile.state = PrivateAccessState::Error;
                    profile.last_error = Some(error.to_string());
                    bail!("启动 Private Access service 失败: {error}");
                }
            }
        }

        let profile_id = profile.id.clone();
        let service = profile.manifest.id.clone();
        let (password, password_env) = if service == "sonicwall" {
            (None, None)
        } else {
            (
                normalize_optional_setting(Some(profile.password.clone())),
                normalize_optional_setting(Some(profile.password_env.clone())),
            )
        };
        let command = PrivateAccessCommand::Connect {
            id: "tui-connect".to_string(),
            service: service.clone(),
            config: json!({
                "server": profile.server,
                "mode": profile.mode.as_str(),
                "port": profile.port,
                "username": profile.username,
                "password": password,
                "password_env": password_env,
                "bridge_listen": profile.bridge_listen,
                "tun_helper": options.tun_helper,
                "http_connect_proxy": options.http_connect_proxy,
                "http_connect_proxy_context": options.http_connect_proxy_context,
                "http_connect_controller": options.http_connect_controller,
                "http_connect_selector": options.http_connect_selector,
                "tls_verify": profile.tls_verify,
            }),
        };
        if let Some(process) = profile.process.as_mut()
            && let Err(error) = process.send(&command)
        {
            profile.state = PrivateAccessState::Error;
            profile.last_error = Some(error.to_string());
            bail!("发送 Private Access 连接命令失败: {error}");
        }
        profile.state = PrivateAccessState::Connecting;
        profile.last_error = None;
        profile.integration_failed = false;
        profile.background_pid = None;
        Ok(PrivateAccessConnectStarted {
            profile_id,
            service,
        })
    }

    pub(crate) fn disconnect(&mut self) -> Result<PrivateAccessDisconnectOutcome> {
        let profile = self.focused_mut();
        if profile.process.is_none()
            && let Some(pid) = profile.background_pid
        {
            return Ok(PrivateAccessDisconnectOutcome::BackgroundOwned(pid));
        }
        let Some(process) = profile.process.as_mut() else {
            profile.state = PrivateAccessState::Disconnected;
            return Ok(PrivateAccessDisconnectOutcome::AlreadyDisconnected);
        };
        let service = process.service_id().to_string();
        if let Err(error) = process.send(&PrivateAccessCommand::Disconnect {
            id: "tui-disconnect".to_string(),
            service: service.clone(),
            session_id: None,
        }) {
            profile.state = PrivateAccessState::Error;
            profile.last_error = Some(error.to_string());
            bail!("发送 Private Access 断开命令失败: {error}");
        }
        profile.state = PrivateAccessState::Disconnecting;
        Ok(PrivateAccessDisconnectOutcome::Started {
            profile_id: profile.id.clone(),
            service,
        })
    }

    pub(crate) fn submit_authentication(
        &mut self,
        profile_index: usize,
        service: String,
        session_id: String,
        challenge_id: String,
        button: String,
        replies: Vec<PrivateAccessSecret>,
    ) -> Result<bool> {
        let Some(process) = self
            .profiles
            .get_mut(profile_index)
            .and_then(|profile| profile.process.as_mut())
        else {
            return Ok(false);
        };
        process.send(&PrivateAccessCommand::AuthReply {
            id: "tui-auth-reply".to_string(),
            service,
            session_id,
            challenge_id,
            button,
            replies,
        })?;
        Ok(true)
    }

    pub(crate) fn cancel_authentication(
        &mut self,
        profile_index: usize,
        service: String,
        session_id: String,
        challenge_id: String,
    ) -> Result<()> {
        if let Some(process) = self
            .profiles
            .get_mut(profile_index)
            .and_then(|profile| profile.process.as_mut())
        {
            process.send(&PrivateAccessCommand::CancelAuth {
                id: "tui-auth-cancel".to_string(),
                service,
                session_id,
                challenge_id,
            })?;
        }
        Ok(())
    }

    pub(crate) fn detach_for_background(
        &mut self,
        process_is_alive: impl Fn(u32, &PrivateAccessServiceManifest) -> bool,
    ) -> Result<usize> {
        let mut detached = 0;
        for profile in &mut self.profiles {
            if profile
                .background_pid
                .is_some_and(|pid| process_is_alive(pid, &profile.manifest))
            {
                detached += 1;
                continue;
            }
            let Some(process) = profile.process.as_mut() else {
                continue;
            };
            let pid = process.pid();
            process.detach().with_context(|| {
                format!("failed to detach Private Access profile {}", profile.id)
            })?;
            profile.background_pid = Some(pid);
            profile.state = PrivateAccessState::Connected;
            profile.last_error = None;
            detached += 1;
        }
        Ok(detached)
    }

    pub(crate) fn poll(
        &mut self,
        integration: &mut impl PrivateAccessNetworkIntegration,
    ) -> Result<Vec<PrivateAccessSessionNotice>> {
        let mut notices = Vec::new();
        for profile_index in 0..self.profiles.len() {
            let mut stop_process = false;
            for _ in 0..EVENTS_PER_POLL {
                let profile_id = self.profiles[profile_index].id.clone();
                let event = match self.profiles[profile_index].process.as_ref() {
                    Some(process) => match process.try_recv() {
                        Ok(Some(event)) => event,
                        Ok(None) => break,
                        Err(error) => {
                            self.profiles[profile_index].last_error = Some(error.clone());
                            self.profiles[profile_index].state = PrivateAccessState::Error;
                            self.clear_dynamic_network_state(
                                profile_index,
                                integration,
                                &mut notices,
                            );
                            notices.push(PrivateAccessSessionNotice::Flash(format!(
                                "Private Access {profile_id} failed: {error}"
                            )));
                            stop_process = true;
                            break;
                        }
                    },
                    None => break,
                };
                if self.apply_event(profile_index, event.event, integration, &mut notices)? {
                    stop_process = true;
                    break;
                }
            }
            if stop_process && let Some(process) = self.profiles[profile_index].process.take() {
                process.stop()?;
            }
        }
        Ok(notices)
    }

    fn apply_event(
        &mut self,
        profile_index: usize,
        event: PrivateAccessEvent,
        integration: &mut impl PrivateAccessNetworkIntegration,
        notices: &mut Vec<PrivateAccessSessionNotice>,
    ) -> Result<bool> {
        let profile_id = self.profiles[profile_index].id.clone();
        match event {
            PrivateAccessEvent::StateChanged {
                service,
                state,
                message,
            } => {
                if !should_apply_state_after_integration(&self.profiles[profile_index], &state) {
                    return Ok(false);
                }
                self.profiles[profile_index].state = state.clone();
                let disconnected = matches!(state, PrivateAccessState::Disconnected);
                if disconnected {
                    notices.push(PrivateAccessSessionNotice::ClearAuthentication { profile_index });
                    self.clear_dynamic_network_state(profile_index, integration, notices);
                }
                if let Some((tone, text, done)) = progress_for_state(&state, &message) {
                    notices.push(PrivateAccessSessionNotice::Progress {
                        profile_index,
                        tone,
                        text,
                        done,
                        append_only: true,
                    });
                }
                notices.push(PrivateAccessSessionNotice::Status(format!(
                    "Private Access {profile_id} ({service}) {}",
                    state.label()
                )));
                Ok(disconnected)
            }
            PrivateAccessEvent::RoutesPushed {
                service,
                routes,
                dns,
                domains,
                domain_suffixes,
                bridge,
                ..
            } => {
                let profile = &mut self.profiles[profile_index];
                let update = PrivateAccessBridgeRouteUpdate {
                    profile_id: profile_id.clone(),
                    routes: routes.clone(),
                    domains: domains.clone(),
                    domain_suffixes: domain_suffixes.clone(),
                    previous_routes: std::mem::replace(&mut profile.routes, routes.clone()),
                    previous_domains: std::mem::replace(&mut profile.domains, domains.clone()),
                    previous_domain_suffixes: std::mem::replace(
                        &mut profile.domain_suffixes,
                        domain_suffixes.clone(),
                    ),
                    carrier_domains: vec![profile.server.trim().to_ascii_lowercase()],
                    bridge: bridge.clone(),
                    fallback_listen: profile.bridge_listen.clone(),
                };
                profile.dns = dns;
                notices.push(PrivateAccessSessionNotice::Progress {
                    profile_index,
                    tone: PrivateAccessNoticeTone::Info,
                    text: format!("收到内网路由: {} 条", routes.len()),
                    done: false,
                    append_only: true,
                });
                if matches!(profile.mode, PrivateAccessMode::Bridge) {
                    profile.bridge = bridge;
                    notices.push(PrivateAccessSessionNotice::Progress {
                        profile_index,
                        tone: PrivateAccessNoticeTone::Info,
                        text: "修改 config.json 中...".to_string(),
                        done: false,
                        append_only: true,
                    });
                    match integration.apply_bridge_routes(&update) {
                        Ok(true) => {
                            notices.push(PrivateAccessSessionNotice::Progress {
                                profile_index,
                                tone: PrivateAccessNoticeTone::Success,
                                text: "config.json 已更新".to_string(),
                                done: false,
                                append_only: true,
                            });
                            notices.push(PrivateAccessSessionNotice::Progress {
                                profile_index,
                                tone: PrivateAccessNoticeTone::Info,
                                text: "重启 sing-box 中...".to_string(),
                                done: false,
                                append_only: false,
                            });
                            match integration.restart_carrier() {
                                Ok(restart) => {
                                    notices.push(PrivateAccessSessionNotice::Progress {
                                        profile_index,
                                        tone: PrivateAccessNoticeTone::Success,
                                        text: restart.progress_message,
                                        done: true,
                                        append_only: false,
                                    });
                                    notices.push(PrivateAccessSessionNotice::Status(format!(
                                        "Private Access {profile_id} ({service}) applied {} bridge route(s); {}",
                                        routes.len(), restart.summary
                                    )));
                                }
                                Err(error) => {
                                    let message = format!(
                                        "sing-box 重启失败，Private Access 不可用: {error:#}"
                                    );
                                    self.mark_integration_failed(
                                        profile_index,
                                        message.clone(),
                                        notices,
                                    );
                                    notices.push(PrivateAccessSessionNotice::Status(message));
                                    return Ok(true);
                                }
                            }
                        }
                        Ok(false) => notices.push(PrivateAccessSessionNotice::Progress {
                            profile_index,
                            tone: PrivateAccessNoticeTone::Info,
                            text: "没有需要写入的内网路由".to_string(),
                            done: false,
                            append_only: true,
                        }),
                        Err(error) => {
                            let message = format!("修改 config.json 失败: {error}");
                            let profile = &mut self.profiles[profile_index];
                            profile.last_error = Some(error.to_string());
                            profile.state = PrivateAccessState::Error;
                            notices.push(PrivateAccessSessionNotice::Progress {
                                profile_index,
                                tone: PrivateAccessNoticeTone::Error,
                                text: message.clone(),
                                done: true,
                                append_only: false,
                            });
                            notices.push(PrivateAccessSessionNotice::Status(message));
                        }
                    }
                } else {
                    self.profiles[profile_index].bridge = None;
                    let dynamic_entries = self.dynamic_system_proxy_bypass_entries();
                    match integration.refresh_system_proxy_bypass(&dynamic_entries) {
                        Ok(true) => notices.push(PrivateAccessSessionNotice::Progress {
                            profile_index,
                            tone: PrivateAccessNoticeTone::Success,
                            text: format!(
                                "已临时应用 {} 条 Private Access 域名绕过规则",
                                domains.len() + domain_suffixes.len()
                            ),
                            done: false,
                            append_only: true,
                        }),
                        Ok(false) => {}
                        Err(error) => notices.push(PrivateAccessSessionNotice::Progress {
                            profile_index,
                            tone: PrivateAccessNoticeTone::Error,
                            text: format!("更新系统代理域名绕过失败: {error:#}"),
                            done: false,
                            append_only: true,
                        }),
                    }
                    notices.push(PrivateAccessSessionNotice::Progress {
                        profile_index,
                        tone: PrivateAccessNoticeTone::Success,
                        text: "TUN 路由和按域名分流 DNS 已生效".to_string(),
                        done: true,
                        append_only: true,
                    });
                    notices.push(PrivateAccessSessionNotice::Status(format!(
                        "Private Access {profile_id} ({service}) connected with {} OS TUN route(s), without restarting sing-box",
                        routes.len()
                    )));
                }
                Ok(false)
            }
            PrivateAccessEvent::AuthChallenge {
                service,
                session_id,
                challenge_id,
                title,
                message,
                fields,
                buttons,
            } => {
                self.profiles[profile_index].state = PrivateAccessState::Connecting;
                notices.push(PrivateAccessSessionNotice::Authentication(
                    PrivateAccessAuthRequest {
                        profile_index,
                        service: service.clone(),
                        session_id,
                        challenge_id,
                        title: user_message(&title, "Private Access login"),
                        message,
                        fields,
                        buttons,
                    },
                ));
                notices.push(PrivateAccessSessionNotice::Status(format!(
                    "Private Access {profile_id} ({service}) is waiting for authentication"
                )));
                Ok(false)
            }
            PrivateAccessEvent::Error {
                service,
                code,
                message,
            } => {
                let error = format!("{code}: {message}");
                self.profiles[profile_index].last_error = Some(error.clone());
                self.profiles[profile_index].state = PrivateAccessState::Error;
                self.clear_dynamic_network_state(profile_index, integration, notices);
                notices.push(PrivateAccessSessionNotice::ClearAuthentication { profile_index });
                notices.push(PrivateAccessSessionNotice::Progress {
                    profile_index,
                    tone: PrivateAccessNoticeTone::Error,
                    text: format!("连接失败: {error}"),
                    done: true,
                    append_only: false,
                });
                let diagnostic = match service.as_str() {
                    "sonicwall" => Some("完整诊断已写入 sonicwall-private-access.log"),
                    "hillstone" => Some("完整诊断已写入 hillstone-private-access.log"),
                    _ => None,
                };
                if let Some(text) = diagnostic {
                    notices.push(PrivateAccessSessionNotice::Progress {
                        profile_index,
                        tone: PrivateAccessNoticeTone::Info,
                        text: text.to_string(),
                        done: false,
                        append_only: false,
                    });
                }
                notices.push(PrivateAccessSessionNotice::Status(format!(
                    "Private Access {profile_id} ({service}) error"
                )));
                Ok(true)
            }
            PrivateAccessEvent::Log { .. } => Ok(false),
        }
    }

    fn clear_dynamic_network_state(
        &mut self,
        profile_index: usize,
        integration: &mut impl PrivateAccessNetworkIntegration,
        notices: &mut Vec<PrivateAccessSessionNotice>,
    ) {
        let profile = &mut self.profiles[profile_index];
        profile.routes.clear();
        profile.dns.clear();
        profile.domains.clear();
        profile.domain_suffixes.clear();
        profile.bridge = None;
        let dynamic_entries = self.dynamic_system_proxy_bypass_entries();
        if let Err(error) = integration.refresh_system_proxy_bypass(&dynamic_entries) {
            notices.push(PrivateAccessSessionNotice::Flash(format!(
                "Failed to remove Private Access system proxy bypass: {error:#}"
            )));
        }
    }

    fn dynamic_system_proxy_bypass_entries(&self) -> Vec<String> {
        let mut entries = Vec::new();
        for profile in &self.profiles {
            if !matches!(profile.mode, PrivateAccessMode::Tun)
                || matches!(
                    profile.state,
                    PrivateAccessState::Disconnected | PrivateAccessState::Error
                )
            {
                continue;
            }
            for value in profile.domains.iter().chain(&profile.domain_suffixes) {
                for entry in parse_bypass_entries(value) {
                    if !entries.contains(&entry) {
                        entries.push(entry);
                    }
                }
            }
        }
        entries
    }

    fn mark_integration_failed(
        &mut self,
        profile_index: usize,
        message: String,
        notices: &mut Vec<PrivateAccessSessionNotice>,
    ) {
        let profile = &mut self.profiles[profile_index];
        profile.integration_failed = true;
        profile.state = PrivateAccessState::Error;
        profile.last_error = Some(message.clone());
        notices.push(PrivateAccessSessionNotice::Progress {
            profile_index,
            tone: PrivateAccessNoticeTone::Error,
            text: message,
            done: true,
            append_only: false,
        });
    }
}

pub(crate) fn load_manifest_for_profile(
    profile_id: &str,
    manifest_path: Option<&str>,
) -> Result<PrivateAccessServiceManifest> {
    if let Some(path) = manifest_path.filter(|path| !path.trim().is_empty()) {
        return load_private_access_manifest(Path::new(path));
    }
    if profile_id.eq_ignore_ascii_case("sonicwall") {
        default_sonicwall_manifest()
    } else {
        default_hillstone_manifest()
    }
}

pub(crate) fn should_apply_state_after_integration(
    profile: &PrivateAccessProfileRuntime,
    state: &PrivateAccessState,
) -> bool {
    !profile.integration_failed
        || matches!(
            state,
            PrivateAccessState::Error
                | PrivateAccessState::Disconnecting
                | PrivateAccessState::Disconnected
        )
}

fn progress_for_state(
    state: &PrivateAccessState,
    message: &str,
) -> Option<(PrivateAccessNoticeTone, String, bool)> {
    let normalized = message.to_ascii_lowercase();
    match state {
        PrivateAccessState::Connecting => {
            if normalized.contains("authentication accepted")
                || normalized.contains("auth accepted")
            {
                Some((
                    PrivateAccessNoticeTone::Success,
                    "认证成功".to_string(),
                    false,
                ))
            } else if normalized.contains("data tunnel") || normalized.contains("tun data plane") {
                Some((
                    PrivateAccessNoticeTone::Success,
                    user_message(message, "数据通道已建立"),
                    false,
                ))
            } else if normalized.contains("gateway") || normalized.contains("connecting") {
                Some((
                    PrivateAccessNoticeTone::Info,
                    "正在连接内网服务器...".to_string(),
                    false,
                ))
            } else {
                Some((
                    PrivateAccessNoticeTone::Info,
                    user_message(message, "正在连接内网服务器..."),
                    false,
                ))
            }
        }
        PrivateAccessState::Connected => Some((
            PrivateAccessNoticeTone::Success,
            user_message(message, "连接成功"),
            false,
        )),
        PrivateAccessState::Disconnecting => Some((
            PrivateAccessNoticeTone::Info,
            "正在断开内网连接...".to_string(),
            false,
        )),
        PrivateAccessState::Disconnected => Some((
            PrivateAccessNoticeTone::Success,
            "内网连接已断开".to_string(),
            true,
        )),
        PrivateAccessState::Error => Some((
            PrivateAccessNoticeTone::Error,
            user_message(message, "连接失败"),
            true,
        )),
        PrivateAccessState::Disabled => None,
    }
}

fn user_message(message: &str, fallback: &str) -> String {
    let message = message.trim();
    if message.is_empty() {
        fallback.to_string()
    } else {
        message.to_string()
    }
}

fn normalize_optional_setting(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use anyhow::bail;
    use serde_json::Value;

    use super::*;

    struct FakeServiceProcess {
        service_id: String,
        events: Mutex<VecDeque<Result<PrivateAccessEventEnvelope, String>>>,
        commands: Arc<Mutex<Vec<Value>>>,
        stopped: Arc<AtomicBool>,
    }

    impl FakeServiceProcess {
        fn new(events: Vec<PrivateAccessEvent>) -> (Self, Arc<Mutex<Vec<Value>>>, Arc<AtomicBool>) {
            let commands = Arc::new(Mutex::new(Vec::new()));
            let stopped = Arc::new(AtomicBool::new(false));
            (
                Self {
                    service_id: "hillstone".to_string(),
                    events: Mutex::new(
                        events
                            .into_iter()
                            .map(|event| Ok(PrivateAccessEventEnvelope::new(event)))
                            .collect(),
                    ),
                    commands: Arc::clone(&commands),
                    stopped: Arc::clone(&stopped),
                },
                commands,
                stopped,
            )
        }
    }

    impl PrivateAccessServiceProcess for FakeServiceProcess {
        fn service_id(&self) -> &str {
            &self.service_id
        }

        fn pid(&self) -> u32 {
            4242
        }

        fn send(&mut self, command: &PrivateAccessCommand) -> Result<()> {
            self.commands
                .lock()
                .expect("commands lock")
                .push(serde_json::to_value(command)?);
            Ok(())
        }

        fn detach(&mut self) -> Result<()> {
            Ok(())
        }

        fn try_recv(&self) -> Result<Option<PrivateAccessEventEnvelope>, String> {
            self.events
                .lock()
                .expect("events lock")
                .pop_front()
                .transpose()
        }

        fn stop(self: Box<Self>) -> Result<()> {
            self.stopped.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeNetworkIntegration {
        bridge_updates: Vec<PrivateAccessBridgeRouteUpdate>,
        bypass_updates: Vec<Vec<String>>,
        restart_error: Option<String>,
        restart_count: usize,
    }

    impl PrivateAccessNetworkIntegration for FakeNetworkIntegration {
        fn apply_bridge_routes(&mut self, update: &PrivateAccessBridgeRouteUpdate) -> Result<bool> {
            self.bridge_updates.push(update.clone());
            Ok(true)
        }

        fn restart_carrier(&mut self) -> Result<PrivateAccessCarrierRestart> {
            self.restart_count += 1;
            if let Some(error) = self.restart_error.as_ref() {
                bail!(error.clone());
            }
            Ok(PrivateAccessCarrierRestart {
                progress_message: "carrier restarted".to_string(),
                summary: "restarted carrier".to_string(),
            })
        }

        fn refresh_system_proxy_bypass(&mut self, dynamic_entries: &[String]) -> Result<bool> {
            self.bypass_updates.push(dynamic_entries.to_vec());
            Ok(true)
        }
    }

    fn runtime_with_process(
        events: Vec<PrivateAccessEvent>,
    ) -> (PrivateAccessRuntime, Arc<AtomicBool>) {
        let mut runtime = PrivateAccessRuntime::with_default_hillstone().expect("runtime builds");
        let (process, _, stopped) = FakeServiceProcess::new(events);
        runtime.focused_mut().process = Some(Box::new(process));
        runtime.focused_mut().server = "vpn.example.com".to_string();
        runtime.focused_mut().username = "alice".to_string();
        runtime.focused_mut().state = PrivateAccessState::Connecting;
        (runtime, stopped)
    }

    #[test]
    fn tun_routes_update_session_and_refresh_dynamic_bypass_without_restarting_carrier() {
        let (mut runtime, stopped) = runtime_with_process(vec![PrivateAccessEvent::RoutesPushed {
            service: "hillstone".to_string(),
            session_id: Some("session-1".to_string()),
            routes: vec![PrivateAccessRoute {
                cidr: "10.1.0.0/16".to_string(),
            }],
            dns: vec!["10.0.0.53".to_string()],
            domains: vec!["service.example.com".to_string()],
            domain_suffixes: vec!["Example.COM".to_string()],
            bridge: None,
        }]);
        runtime.focused_mut().mode = PrivateAccessMode::Tun;
        let mut network = FakeNetworkIntegration::default();

        let notices = runtime.poll(&mut network).expect("events apply");

        assert_eq!(runtime.focused().routes[0].cidr, "10.1.0.0/16");
        assert_eq!(runtime.focused().dns, ["10.0.0.53"]);
        assert_eq!(
            network.bypass_updates,
            [vec![
                "service.example.com".to_string(),
                "example.com".to_string()
            ]]
        );
        assert_eq!(network.restart_count, 0);
        assert!(!stopped.load(Ordering::SeqCst));
        assert!(notices.iter().any(|notice| matches!(
            notice,
            PrivateAccessSessionNotice::Progress { done: true, .. }
        )));
    }

    #[test]
    fn bridge_restart_failure_marks_session_error_and_stops_owned_service() {
        let (mut runtime, stopped) = runtime_with_process(vec![PrivateAccessEvent::RoutesPushed {
            service: "hillstone".to_string(),
            session_id: Some("session-1".to_string()),
            routes: vec![PrivateAccessRoute {
                cidr: "10.2.0.0/16".to_string(),
            }],
            dns: Vec::new(),
            domains: Vec::new(),
            domain_suffixes: Vec::new(),
            bridge: Some(PrivateAccessBridge {
                kind: "http".to_string(),
                listen: "127.0.0.1:16780".to_string(),
            }),
        }]);
        runtime.focused_mut().mode = PrivateAccessMode::Bridge;
        let mut network = FakeNetworkIntegration {
            restart_error: Some("restart denied".to_string()),
            ..FakeNetworkIntegration::default()
        };

        let notices = runtime.poll(&mut network).expect("failure is contained");

        assert_eq!(runtime.focused().state, PrivateAccessState::Error);
        assert!(!runtime.focused().owns_process());
        assert!(stopped.load(Ordering::SeqCst));
        assert!(!should_apply_state_after_integration(
            runtime.focused(),
            &PrivateAccessState::Connected
        ));
        assert!(notices.iter().any(|notice| matches!(
            notice,
            PrivateAccessSessionNotice::Progress {
                tone: PrivateAccessNoticeTone::Error,
                done: true,
                ..
            }
        )));
    }

    #[test]
    fn disconnected_event_clears_network_resources_and_releases_process() {
        let (mut runtime, stopped) = runtime_with_process(vec![PrivateAccessEvent::StateChanged {
            service: "hillstone".to_string(),
            state: PrivateAccessState::Disconnected,
            message: String::new(),
        }]);
        let profile = runtime.focused_mut();
        profile.routes.push(PrivateAccessRoute {
            cidr: "10.3.0.0/16".to_string(),
        });
        profile.domains.push("internal.example".to_string());
        profile.bridge = Some(PrivateAccessBridge {
            kind: "http".to_string(),
            listen: "127.0.0.1:16780".to_string(),
        });
        let mut network = FakeNetworkIntegration::default();

        let notices = runtime.poll(&mut network).expect("disconnect applies");

        assert_eq!(runtime.focused().state, PrivateAccessState::Disconnected);
        assert!(runtime.focused().routes.is_empty());
        assert!(runtime.focused().domains.is_empty());
        assert!(runtime.focused().bridge.is_none());
        assert_eq!(network.bypass_updates, [Vec::<String>::new()]);
        assert!(stopped.load(Ordering::SeqCst));
        assert!(notices.iter().any(|notice| matches!(
            notice,
            PrivateAccessSessionNotice::ClearAuthentication { profile_index: 0 }
        )));
    }

    #[test]
    fn authentication_reply_is_sent_through_owned_service() {
        let mut runtime = PrivateAccessRuntime::with_default_hillstone().expect("runtime builds");
        let (process, commands, _) = FakeServiceProcess::new(Vec::new());
        runtime.focused_mut().process = Some(Box::new(process));

        assert!(
            runtime
                .submit_authentication(
                    0,
                    "hillstone".to_string(),
                    "session-1".to_string(),
                    "challenge-1".to_string(),
                    "ok".to_string(),
                    vec![PrivateAccessSecret::new("secret")],
                )
                .expect("reply sends")
        );

        let commands = commands.lock().expect("commands lock");
        assert_eq!(commands[0]["type"], "auth_reply");
        assert_eq!(commands[0]["session_id"], "session-1");
        assert_eq!(commands[0]["replies"], json!(["secret"]));
    }
}

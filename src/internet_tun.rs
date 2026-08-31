use std::path::PathBuf;
use std::sync::mpsc::{self, TryRecvError};
use std::thread::{self, JoinHandle};

use anyhow::{Context, Result, bail};

use crate::config::{
    InternetTunModeUpdate, RouteAutoDetectInterfaceState, inspect_tun_config, set_internet_tun_mode,
};
#[cfg(test)]
use crate::managed_sing_box::ControllerProbe;
use crate::managed_sing_box::{AuthorizationRequirement, LifecycleReport, ManagedSingBox};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct PersistedInternetTun {
    enabled: Option<bool>,
    restore_auto_detect_interface: Option<RouteAutoDetectInterfaceState>,
}

impl PersistedInternetTun {
    pub(crate) fn new(
        enabled: Option<bool>,
        restore_auto_detect_interface: Option<RouteAutoDetectInterfaceState>,
    ) -> Self {
        Self {
            enabled,
            restore_auto_detect_interface,
        }
    }

    pub(crate) fn enabled(self) -> Option<bool> {
        self.enabled
    }

    pub(crate) fn restore_auto_detect_interface(self) -> Option<RouteAutoDetectInterfaceState> {
        self.restore_auto_detect_interface
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InternetTunTarget {
    Enabled,
    Disabled,
}

impl InternetTunTarget {
    pub(crate) fn is_enabled(self) -> bool {
        self == Self::Enabled
    }
}

pub(crate) struct InternetTunRestart {
    pub(crate) report: LifecycleReport,
    pub(crate) controller_error: Option<String>,
}

pub(crate) enum InternetTunToggleOutcome {
    Applied {
        target: InternetTunTarget,
        config_changed: bool,
        restart: Result<InternetTunRestart, String>,
        persistence_warning: Option<String>,
    },
    Failed {
        error: String,
        recovery_warning: Option<String>,
    },
}

struct TunToggleJob {
    target: InternetTunTarget,
    previous_explicit: bool,
    previous_restore_auto_detect_interface: Option<RouteAutoDetectInterfaceState>,
    journal_restore_auto_detect_interface: Option<RouteAutoDetectInterfaceState>,
    receiver: mpsc::Receiver<Result<InternetTunModeUpdate, String>>,
    worker: Option<JoinHandle<()>>,
}

pub(crate) struct InternetTunTransaction {
    config_path: PathBuf,
    enabled: bool,
    explicit: bool,
    restore_auto_detect_interface: Option<RouteAutoDetectInterfaceState>,
    job: Option<TunToggleJob>,
}

impl InternetTunTransaction {
    pub(crate) fn new(config_path: PathBuf, persisted: PersistedInternetTun) -> Result<Self> {
        let configured = if config_path.exists() {
            inspect_tun_config(&config_path)?.managed_internet_tun
        } else {
            false
        };
        Ok(Self {
            config_path,
            enabled: persisted.enabled.unwrap_or(configured),
            explicit: persisted.enabled.is_some(),
            restore_auto_detect_interface: persisted.restore_auto_detect_interface,
            job: None,
        })
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub(crate) fn is_transitioning(&self) -> bool {
        self.job.is_some()
    }

    pub(crate) fn persisted(&self) -> PersistedInternetTun {
        self.job
            .as_ref()
            .map(|job| {
                PersistedInternetTun::new(
                    Some(job.target.is_enabled()),
                    job.journal_restore_auto_detect_interface,
                )
            })
            .unwrap_or_else(|| {
                PersistedInternetTun::new(
                    self.explicit.then_some(self.enabled),
                    self.restore_auto_detect_interface,
                )
            })
    }

    /// Removes the managed TUN from the next sing-box configuration while preserving the user's
    /// persisted intent. The caller must stop the currently running core after this succeeds so
    /// its live routes and interface are released.
    pub(crate) fn suspend_for_exit(&mut self) -> Result<bool> {
        if self.is_transitioning() {
            bail!("cannot suspend Internet TUN while a transition is running");
        }
        if !self.config_path.exists() {
            return Ok(false);
        }
        let state = inspect_tun_config(&self.config_path)?;
        if !state.managed_internet_tun {
            return Ok(false);
        }
        let update =
            set_internet_tun_mode(&self.config_path, false, self.restore_auto_detect_interface)?;
        Ok(update.changed)
    }

    /// Repairs managed Internet TUN configuration drift and completes an interrupted transition.
    /// Persistence is invoked before every config mutation that needs recovery metadata.
    pub(crate) fn reconcile<F>(&mut self, mut persist: F) -> Result<()>
    where
        F: FnMut(PersistedInternetTun) -> Result<()>,
    {
        if !self.config_path.exists() {
            return Ok(());
        }
        let mut config_state = inspect_tun_config(&self.config_path)?;
        if self.enabled && config_state.has_conflicting_tuns() {
            bail!(
                "sing-box config contains both the managed tun-in inbound and another TUN inbound"
            );
        }
        if self.enabled && config_state.reserved_tag_conflict {
            bail!(
                "cannot restore Internet TUN mode: inbound tag tun-in is not uniquely owned by the managed TUN"
            );
        }
        if self.enabled
            && config_state.managed_internet_tun
            && config_state.auto_detect_interface != RouteAutoDetectInterfaceState::Enabled
        {
            if self.restore_auto_detect_interface.is_none() {
                self.restore_auto_detect_interface = Some(config_state.auto_detect_interface);
                persist(self.persisted())?;
            }
            set_internet_tun_mode(&self.config_path, true, self.restore_auto_detect_interface)?;
            config_state.auto_detect_interface = RouteAutoDetectInterfaceState::Enabled;
        }
        if !self.explicit {
            if !config_state.managed_internet_tun && self.restore_auto_detect_interface.is_some() {
                self.restore_auto_detect_interface = None;
                persist(self.persisted())?;
            }
            return Ok(());
        }
        if self.enabled && !config_state.managed_internet_tun && config_state.other_tun {
            self.enabled = false;
            self.explicit = false;
            self.restore_auto_detect_interface = None;
            persist(self.persisted())?;
            return Ok(());
        }
        if self.enabled && !config_state.managed_internet_tun {
            self.restore_auto_detect_interface = Some(config_state.auto_detect_interface);
            persist(self.persisted())?;
            let update =
                set_internet_tun_mode(&self.config_path, true, self.restore_auto_detect_interface)?;
            if update.auto_detect_interface_before_enable != config_state.auto_detect_interface {
                self.restore_auto_detect_interface =
                    Some(update.auto_detect_interface_before_enable);
                persist(self.persisted())?;
            }
        } else if !self.enabled
            && (config_state.managed_internet_tun || self.restore_auto_detect_interface.is_some())
        {
            set_internet_tun_mode(&self.config_path, false, self.restore_auto_detect_interface)?;
            self.restore_auto_detect_interface = None;
            persist(self.persisted())?;
        }
        Ok(())
    }

    pub(crate) fn authorization_requirement(
        &self,
        sing_box: &ManagedSingBox,
    ) -> AuthorizationRequirement {
        self.authorization_requirement_with(sing_box)
    }

    fn authorization_requirement_with<L: TunLifecycle>(
        &self,
        lifecycle: &L,
    ) -> AuthorizationRequirement {
        if self.is_transitioning() {
            return AuthorizationRequirement::None;
        }
        let next_run_needs_elevation = !self.enabled
            && self.config_path.exists()
            && inspect_tun_config(&self.config_path)
                .is_ok_and(|state| !state.other_tun && !state.reserved_tag_conflict);
        lifecycle.restart_authorization_requirement(next_run_needs_elevation)
    }

    pub(crate) fn start_toggle<F>(&mut self, persist: F) -> Result<InternetTunTarget>
    where
        F: FnMut(PersistedInternetTun) -> Result<()>,
    {
        self.start_toggle_with(persist)
    }

    fn start_toggle_with<F>(&mut self, mut persist: F) -> Result<InternetTunTarget>
    where
        F: FnMut(PersistedInternetTun) -> Result<()>,
    {
        if self.is_transitioning() {
            bail!("TUN mode update is already running");
        }
        let target = if self.enabled {
            InternetTunTarget::Disabled
        } else {
            InternetTunTarget::Enabled
        };
        let config_state = inspect_tun_config(&self.config_path)?;
        if target.is_enabled() && config_state.reserved_tag_conflict {
            bail!("inbound tag tun-in is already used by another inbound");
        }
        if target.is_enabled() && (config_state.has_conflicting_tuns() || config_state.other_tun) {
            bail!("another custom TUN inbound is already present");
        }
        let journal_restore_auto_detect_interface = if target.is_enabled() {
            Some(config_state.auto_detect_interface)
        } else {
            self.restore_auto_detect_interface
        };
        persist(PersistedInternetTun::new(
            Some(target.is_enabled()),
            journal_restore_auto_detect_interface,
        ))
        .context("failed before config change")?;

        let config_path = self.config_path.clone();
        let restore_auto_detect_interface = journal_restore_auto_detect_interface;
        let (tx, rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let result = set_internet_tun_mode(
                &config_path,
                target.is_enabled(),
                restore_auto_detect_interface,
            )
            .map_err(|error| error.to_string());
            let _ = tx.send(result);
        });
        self.job = Some(TunToggleJob {
            target,
            previous_explicit: self.explicit,
            previous_restore_auto_detect_interface: self.restore_auto_detect_interface,
            journal_restore_auto_detect_interface,
            receiver: rx,
            worker: Some(worker),
        });
        Ok(target)
    }

    pub(crate) fn poll<Restart, F>(
        &mut self,
        restart: Restart,
        persist: F,
    ) -> Option<InternetTunToggleOutcome>
    where
        Restart: FnMut() -> Result<InternetTunRestart>,
        F: FnMut(PersistedInternetTun) -> Result<()>,
    {
        self.poll_with_restart(restart, persist)
    }

    #[cfg(test)]
    fn poll_with<L, F>(
        &mut self,
        lifecycle: &mut L,
        probe: &dyn ControllerProbe,
        persist: F,
    ) -> Option<InternetTunToggleOutcome>
    where
        L: TunLifecycle,
        F: FnMut(PersistedInternetTun) -> Result<()>,
    {
        self.poll_with_restart(|| lifecycle.restart_and_observe(probe), persist)
    }

    fn poll_with_restart<Restart, F>(
        &mut self,
        mut restart: Restart,
        mut persist: F,
    ) -> Option<InternetTunToggleOutcome>
    where
        Restart: FnMut() -> Result<InternetTunRestart>,
        F: FnMut(PersistedInternetTun) -> Result<()>,
    {
        let job = self.job.as_ref()?;
        let result = match job.receiver.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => Err("TUN mode worker disconnected".to_string()),
        };
        let mut job = self.job.take().expect("TUN toggle job exists");
        if let Some(worker) = job.worker.take() {
            let _ = worker.join();
        }
        Some(self.finish_toggle(job, result, &mut restart, &mut persist))
    }

    fn finish_toggle<Restart, F>(
        &mut self,
        job: TunToggleJob,
        result: Result<InternetTunModeUpdate, String>,
        restart: &mut Restart,
        persist: &mut F,
    ) -> InternetTunToggleOutcome
    where
        Restart: FnMut() -> Result<InternetTunRestart>,
        F: FnMut(PersistedInternetTun) -> Result<()>,
    {
        let update = match result {
            Ok(update) => update,
            Err(error) => {
                let recovery_warning = persist(self.persisted())
                    .err()
                    .map(|error| format!("{error:#}"));
                return InternetTunToggleOutcome::Failed {
                    error,
                    recovery_warning,
                };
            }
        };

        self.restore_auto_detect_interface = if job.target.is_enabled() {
            Some(update.auto_detect_interface_before_enable)
        } else {
            None
        };
        self.enabled = job.target.is_enabled();
        self.explicit = true;
        let journal_changed = job.target.is_enabled()
            && self.restore_auto_detect_interface != job.journal_restore_auto_detect_interface;
        let persistence_warning = match persist(self.persisted()) {
            Ok(()) => None,
            Err(error) if journal_changed => {
                let rollback = set_internet_tun_mode(
                    &self.config_path,
                    false,
                    Some(update.auto_detect_interface_before_enable),
                );
                self.enabled = false;
                self.explicit = job.previous_explicit;
                self.restore_auto_detect_interface = job.previous_restore_auto_detect_interface;
                let state_rollback = persist(self.persisted());
                let detail = match (rollback, state_rollback) {
                    (Ok(_), Ok(())) => format!(
                        "failed to persist TUN rollback metadata; config was rolled back: {error:#}"
                    ),
                    (config_rollback, state_rollback) => format!(
                        "failed to persist TUN rollback metadata ({error:#}); config rollback: {}; state rollback: {}",
                        config_rollback
                            .err()
                            .map(|value| format!("{value:#}"))
                            .unwrap_or_else(|| "ok".to_string()),
                        state_rollback
                            .err()
                            .map(|value| format!("{value:#}"))
                            .unwrap_or_else(|| "ok".to_string())
                    ),
                };
                return InternetTunToggleOutcome::Failed {
                    error: detail,
                    recovery_warning: None,
                };
            }
            Err(error) => Some(format!("{error:#}")),
        };

        let restart = restart().map_err(|error| format!("{error:#}"));
        InternetTunToggleOutcome::Applied {
            target: job.target,
            config_changed: update.changed,
            restart,
            persistence_warning,
        }
    }
}

trait TunLifecycle {
    fn restart_authorization_requirement(
        &self,
        next_run_needs_elevation: bool,
    ) -> AuthorizationRequirement;

    #[cfg(test)]
    fn restart_and_observe(&mut self, probe: &dyn ControllerProbe) -> Result<InternetTunRestart>;
}

impl TunLifecycle for ManagedSingBox {
    fn restart_authorization_requirement(
        &self,
        next_run_needs_elevation: bool,
    ) -> AuthorizationRequirement {
        self.restart_authorization_requirement(next_run_needs_elevation)
    }

    #[cfg(test)]
    fn restart_and_observe(&mut self, probe: &dyn ControllerProbe) -> Result<InternetTunRestart> {
        let receipt = self.restart()?;
        let controller_error = receipt
            .observe_controller(probe)
            .err()
            .map(|error| format!("{error:#}"));
        Ok(InternetTunRestart {
            report: receipt.into_report(),
            controller_error,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::rc::Rc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use anyhow::{Result, bail};
    use serde_json::json;

    use super::{
        InternetTunRestart, InternetTunTarget, InternetTunToggleOutcome, InternetTunTransaction,
        PersistedInternetTun, TunLifecycle,
    };
    use crate::config::{RouteAutoDetectInterfaceState, default_tun_inbound, inspect_tun_config};
    use crate::managed_sing_box::{AuthorizationRequirement, ControllerProbe, LifecycleReport};

    static TEST_PATH_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn test_path() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let counter = TEST_PATH_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("sing-box-tui-internet-tun-{nanos}-{counter}.json"))
    }

    fn write_config(path: &Path, value: serde_json::Value) {
        fs::write(
            path,
            serde_json::to_string_pretty(&value).expect("config serializes"),
        )
        .expect("config writes");
    }

    fn wait_for_managed_tun(path: &Path) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if inspect_tun_config(path).is_ok_and(|state| state.managed_internet_tun) {
                return;
            }
            assert!(Instant::now() < deadline, "config mutation timed out");
            // On Windows, a zero-delay read loop can repeatedly reopen the destination while the
            // worker is trying to atomically replace it. Leave a small scheduling window for the
            // write instead of turning this assertion helper into file-system contention.
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    struct ReadyProbe;

    impl ControllerProbe for ReadyProbe {
        fn probe_controller(&self) -> Result<()> {
            Ok(())
        }
    }

    struct FakeLifecycle {
        restarts: usize,
        fail_restart: bool,
        controller_error: Option<String>,
        authorization_inputs: RefCell<Vec<bool>>,
    }

    impl FakeLifecycle {
        fn successful() -> Self {
            Self {
                restarts: 0,
                fail_restart: false,
                controller_error: None,
                authorization_inputs: RefCell::new(Vec::new()),
            }
        }
    }

    impl TunLifecycle for FakeLifecycle {
        fn restart_authorization_requirement(
            &self,
            next_run_needs_elevation: bool,
        ) -> AuthorizationRequirement {
            self.authorization_inputs
                .borrow_mut()
                .push(next_run_needs_elevation);
            if next_run_needs_elevation {
                AuthorizationRequirement::InteractiveSudo
            } else {
                AuthorizationRequirement::None
            }
        }

        fn restart_and_observe(
            &mut self,
            _probe: &dyn ControllerProbe,
        ) -> Result<InternetTunRestart> {
            self.restarts += 1;
            if self.fail_restart {
                bail!("restart failed");
            }
            Ok(InternetTunRestart {
                report: LifecycleReport::for_test(vec![10], 11),
                controller_error: self.controller_error.clone(),
            })
        }
    }

    fn wait_for_outcome<F>(
        transaction: &mut InternetTunTransaction,
        lifecycle: &mut FakeLifecycle,
        mut persist: F,
    ) -> InternetTunToggleOutcome
    where
        F: FnMut(PersistedInternetTun) -> Result<()>,
    {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(outcome) = transaction.poll_with(lifecycle, &ReadyProbe, &mut persist) {
                return outcome;
            }
            assert!(Instant::now() < deadline, "TUN transition timed out");
            std::thread::yield_now();
        }
    }

    #[test]
    fn persisted_intent_overrides_config_and_round_trips_through_the_interface() {
        let path = test_path();
        write_config(&path, json!({ "inbounds": [] }));
        let persisted = PersistedInternetTun::new(
            Some(true),
            Some(RouteAutoDetectInterfaceState::RouteMissing),
        );

        let transaction =
            InternetTunTransaction::new(path.clone(), persisted).expect("transaction initializes");

        assert!(transaction.is_enabled());
        assert_eq!(transaction.persisted(), persisted);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn exit_suspend_removes_tun_config_without_erasing_persisted_intent() {
        let path = test_path();
        write_config(
            &path,
            json!({
                "inbounds": [default_tun_inbound()],
                "route": { "auto_detect_interface": true }
            }),
        );
        let persisted =
            PersistedInternetTun::new(Some(true), Some(RouteAutoDetectInterfaceState::Disabled));
        let mut transaction =
            InternetTunTransaction::new(path.clone(), persisted).expect("transaction initializes");

        transaction.suspend_for_exit().expect("TUN suspends");

        assert!(!inspect_tun_config(&path).unwrap().managed_internet_tun);
        assert_eq!(transaction.persisted(), persisted);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn toggle_persists_recovery_metadata_before_starting_config_work() {
        let path = test_path();
        write_config(
            &path,
            json!({
                "inbounds": [{ "type": "mixed", "listen_port": 6780 }],
                "route": { "rules": [] }
            }),
        );
        let persisted = Rc::new(RefCell::new(Vec::new()));
        let mut transaction =
            InternetTunTransaction::new(path.clone(), PersistedInternetTun::default())
                .expect("transaction initializes");

        let captured = Rc::clone(&persisted);
        let target = transaction
            .start_toggle(move |state| {
                captured.borrow_mut().push(state);
                Ok(())
            })
            .expect("transition starts");

        assert_eq!(target, InternetTunTarget::Enabled);
        assert_eq!(
            persisted.borrow().as_slice(),
            &[PersistedInternetTun::new(
                Some(true),
                Some(RouteAutoDetectInterfaceState::FieldMissing)
            )]
        );
        assert!(transaction.is_transitioning());
        wait_for_managed_tun(&path);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn persistence_failure_prevents_config_mutation() {
        let path = test_path();
        write_config(&path, json!({ "inbounds": [] }));
        let original = fs::read_to_string(&path).expect("config reads");
        let mut transaction =
            InternetTunTransaction::new(path.clone(), PersistedInternetTun::default())
                .expect("transaction initializes");

        let error = transaction
            .start_toggle(|_| bail!("state store unavailable"))
            .expect_err("transition must stop before config work");

        assert!(error.to_string().contains("failed before config change"));
        assert!(!transaction.is_transitioning());
        assert_eq!(fs::read_to_string(&path).expect("config reads"), original);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn reconcile_refreshes_rollback_state_after_config_drift() {
        let path = test_path();
        write_config(
            &path,
            json!({
                "inbounds": [{ "type": "mixed", "listen_port": 6780 }],
                "route": { "rules": [] }
            }),
        );
        let persisted = Rc::new(Cell::new(PersistedInternetTun::default()));
        let mut transaction = InternetTunTransaction::new(
            path.clone(),
            PersistedInternetTun::new(Some(true), Some(RouteAutoDetectInterfaceState::Disabled)),
        )
        .expect("transaction initializes");

        let captured = Rc::clone(&persisted);
        transaction
            .reconcile(move |state| {
                captured.set(state);
                Ok(())
            })
            .expect("transition reconciles");

        assert!(transaction.is_enabled());
        assert_eq!(
            transaction.persisted(),
            PersistedInternetTun::new(
                Some(true),
                Some(RouteAutoDetectInterfaceState::FieldMissing)
            )
        );
        assert_eq!(persisted.get(), transaction.persisted());
        assert!(
            inspect_tun_config(&path)
                .expect("config inspects")
                .managed_internet_tun
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn reconcile_does_not_adopt_a_legacy_custom_tun() {
        let path = test_path();
        let config = json!({
            "inbounds": [{
                "type": "tun",
                "tag": "custom-tun",
                "address": ["172.20.0.1/30"]
            }],
            "route": { "auto_detect_interface": true }
        });
        write_config(&path, config.clone());
        let persisted = Rc::new(Cell::new(PersistedInternetTun::new(Some(true), None)));
        let mut transaction =
            InternetTunTransaction::new(path.clone(), PersistedInternetTun::new(Some(true), None))
                .expect("transaction initializes");

        let captured = Rc::clone(&persisted);
        transaction
            .reconcile(move |state| {
                captured.set(state);
                Ok(())
            })
            .expect("legacy state migrates");

        assert!(!transaction.is_enabled());
        assert_eq!(transaction.persisted(), PersistedInternetTun::default());
        assert_eq!(persisted.get(), PersistedInternetTun::default());
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(
                &fs::read_to_string(&path).expect("config reads")
            )
            .expect("config parses"),
            config
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn reconcile_completes_an_interrupted_disable() {
        let path = test_path();
        write_config(
            &path,
            json!({
                "inbounds": [default_tun_inbound()],
                "route": { "auto_detect_interface": true, "rules": [] }
            }),
        );
        let persisted = Rc::new(Cell::new(PersistedInternetTun::default()));
        let mut transaction = InternetTunTransaction::new(
            path.clone(),
            PersistedInternetTun::new(
                Some(false),
                Some(RouteAutoDetectInterfaceState::FieldMissing),
            ),
        )
        .expect("transaction initializes");

        let captured = Rc::clone(&persisted);
        transaction
            .reconcile(move |state| {
                captured.set(state);
                Ok(())
            })
            .expect("disable recovers");

        let config = inspect_tun_config(&path).expect("config inspects");
        assert!(!config.managed_internet_tun);
        assert_eq!(
            config.auto_detect_interface,
            RouteAutoDetectInterfaceState::FieldMissing
        );
        assert_eq!(
            persisted.get(),
            PersistedInternetTun::new(Some(false), None)
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn reconcile_rejects_managed_and_custom_tun_conflict() {
        let path = test_path();
        write_config(
            &path,
            json!({
                "inbounds": [
                    default_tun_inbound(),
                    {
                        "type": "tun",
                        "tag": "custom-tun",
                        "address": ["172.20.0.1/30"]
                    }
                ],
                "route": { "auto_detect_interface": true }
            }),
        );
        let mut transaction =
            InternetTunTransaction::new(path.clone(), PersistedInternetTun::new(Some(true), None))
                .expect("transaction initializes");

        let error = transaction
            .reconcile(|_| Ok(()))
            .expect_err("conflicting TUNs must block reconciliation");

        assert!(error.to_string().contains("both the managed tun-in"));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn interrupted_disable_removes_managed_tun_despite_reserved_tag_conflict() {
        let path = test_path();
        write_config(
            &path,
            json!({
                "inbounds": [
                    default_tun_inbound(),
                    {
                        "type": "mixed",
                        "tag": "tun-in",
                        "listen_port": 6780
                    }
                ],
                "route": { "auto_detect_interface": true }
            }),
        );
        let mut transaction = InternetTunTransaction::new(
            path.clone(),
            PersistedInternetTun::new(Some(false), Some(RouteAutoDetectInterfaceState::Disabled)),
        )
        .expect("transaction initializes");

        transaction
            .reconcile(|_| Ok(()))
            .expect("disable recovery completes");

        let config = inspect_tun_config(&path).expect("config inspects");
        assert!(!config.managed_internet_tun);
        assert!(config.reserved_tag_conflict);
        assert_eq!(
            config.auto_detect_interface,
            RouteAutoDetectInterfaceState::Disabled
        );
        assert_eq!(
            transaction.persisted(),
            PersistedInternetTun::new(Some(false), None)
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn interrupted_disable_preserves_custom_tun_route_ownership() {
        let path = test_path();
        write_config(
            &path,
            json!({
                "inbounds": [
                    default_tun_inbound(),
                    {
                        "type": "tun",
                        "tag": "custom-tun",
                        "address": ["172.20.0.1/30"],
                        "auto_route": false
                    }
                ],
                "route": { "auto_detect_interface": true }
            }),
        );
        let mut transaction = InternetTunTransaction::new(
            path.clone(),
            PersistedInternetTun::new(Some(false), Some(RouteAutoDetectInterfaceState::Disabled)),
        )
        .expect("transaction initializes");

        transaction
            .reconcile(|_| Ok(()))
            .expect("disable recovery completes");

        let config = inspect_tun_config(&path).expect("config inspects");
        assert!(!config.managed_internet_tun);
        assert!(config.other_tun);
        assert_eq!(
            config.auto_detect_interface,
            RouteAutoDetectInterfaceState::Enabled
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn implicit_managed_tun_is_repaired_without_becoming_explicit_intent() {
        let path = test_path();
        write_config(
            &path,
            json!({
                "inbounds": [default_tun_inbound()],
                "route": { "auto_detect_interface": false }
            }),
        );
        let persisted = Rc::new(Cell::new(PersistedInternetTun::default()));
        let mut transaction =
            InternetTunTransaction::new(path.clone(), PersistedInternetTun::default())
                .expect("transaction initializes");

        let captured = Rc::clone(&persisted);
        transaction
            .reconcile(move |state| {
                captured.set(state);
                Ok(())
            })
            .expect("managed TUN repairs");

        assert!(transaction.is_enabled());
        assert_eq!(
            transaction.persisted(),
            PersistedInternetTun::new(None, Some(RouteAutoDetectInterfaceState::Disabled))
        );
        assert_eq!(persisted.get(), transaction.persisted());
        assert_eq!(
            inspect_tun_config(&path)
                .expect("config inspects")
                .auto_detect_interface,
            RouteAutoDetectInterfaceState::Enabled
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn stale_disable_journal_clears_without_touching_custom_tun() {
        let path = test_path();
        let config = json!({
            "inbounds": [{
                "type": "tun",
                "tag": "custom-tun",
                "address": ["172.20.0.1/30"]
            }],
            "route": { "auto_detect_interface": true }
        });
        write_config(&path, config.clone());
        let mut transaction = InternetTunTransaction::new(
            path.clone(),
            PersistedInternetTun::new(Some(false), Some(RouteAutoDetectInterfaceState::Disabled)),
        )
        .expect("transaction initializes");

        transaction
            .reconcile(|_| Ok(()))
            .expect("stale journal clears");

        assert_eq!(
            transaction.persisted(),
            PersistedInternetTun::new(Some(false), None)
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(
                &fs::read_to_string(&path).expect("config reads")
            )
            .expect("config parses"),
            config
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn successful_toggle_commits_state_then_restarts_and_observes() {
        let path = test_path();
        write_config(
            &path,
            json!({
                "inbounds": [{ "type": "mixed", "listen_port": 6780 }],
                "route": { "rules": [] }
            }),
        );
        let persisted = Rc::new(Cell::new(PersistedInternetTun::default()));
        let mut transaction =
            InternetTunTransaction::new(path.clone(), PersistedInternetTun::default())
                .expect("transaction initializes");
        let start_persisted = Rc::clone(&persisted);
        transaction
            .start_toggle(move |state| {
                start_persisted.set(state);
                Ok(())
            })
            .expect("transition starts");
        let mut lifecycle = FakeLifecycle::successful();
        let finish_persisted = Rc::clone(&persisted);

        let outcome = wait_for_outcome(&mut transaction, &mut lifecycle, move |state| {
            finish_persisted.set(state);
            Ok(())
        });

        match outcome {
            InternetTunToggleOutcome::Applied {
                target,
                config_changed,
                restart,
                persistence_warning,
            } => {
                assert_eq!(target, InternetTunTarget::Enabled);
                assert!(config_changed);
                assert!(
                    restart
                        .expect("restart succeeds")
                        .controller_error
                        .is_none()
                );
                assert!(persistence_warning.is_none());
            }
            InternetTunToggleOutcome::Failed { error, .. } => {
                panic!("transition unexpectedly failed: {error}")
            }
        }
        assert_eq!(lifecycle.restarts, 1);
        assert!(transaction.is_enabled());
        assert_eq!(persisted.get(), transaction.persisted());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn restart_failure_remains_observable_without_erasing_committed_intent() {
        let path = test_path();
        write_config(&path, json!({ "inbounds": [], "route": {} }));
        let mut transaction =
            InternetTunTransaction::new(path.clone(), PersistedInternetTun::default())
                .expect("transaction initializes");
        transaction
            .start_toggle(|_| Ok(()))
            .expect("transition starts");
        let mut lifecycle = FakeLifecycle {
            fail_restart: true,
            ..FakeLifecycle::successful()
        };

        let outcome = wait_for_outcome(&mut transaction, &mut lifecycle, |_| Ok(()));

        match outcome {
            InternetTunToggleOutcome::Applied { restart, .. } => {
                let Err(error) = restart else {
                    panic!("restart unexpectedly succeeded");
                };
                assert!(error.contains("restart failed"));
            }
            InternetTunToggleOutcome::Failed { error, .. } => {
                panic!("config transition unexpectedly failed: {error}")
            }
        }
        assert!(transaction.is_enabled());
        assert_eq!(transaction.persisted().enabled(), Some(true));
        assert!(
            inspect_tun_config(&path)
                .expect("config inspects")
                .managed_internet_tun
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn reserved_tag_conflict_does_not_request_elevation() {
        let path = test_path();
        write_config(
            &path,
            json!({
                "inbounds": [{
                    "type": "mixed",
                    "tag": "tun-in",
                    "listen_port": 6780
                }]
            }),
        );
        let transaction =
            InternetTunTransaction::new(path.clone(), PersistedInternetTun::default())
                .expect("transaction initializes");
        let lifecycle = FakeLifecycle::successful();

        assert_eq!(
            transaction.authorization_requirement_with(&lifecycle),
            AuthorizationRequirement::None
        );
        assert_eq!(lifecycle.authorization_inputs.borrow().as_slice(), &[false]);
        let _ = fs::remove_file(path);
    }
}

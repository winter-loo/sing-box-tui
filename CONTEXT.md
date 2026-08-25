# sing-box-tui

Terms that define how sing-box-tui owns proxy runtime and network-access behavior.

## Language

**Managed sing-box process**:
A sing-box process started by the current `ManagedSingBox` instance and held in its explicit lifecycle ownership for one configuration. Existing, unrelated, user-owned, or previous-instance sing-box processes are never adopted, restarted, or stopped.
_Avoid_: Managed child, adopted process, sing-box service

**Controller readiness**:
The state in which sing-box's controller accepts requests; an operating-system process can be running without being controller-ready.
_Avoid_: Process alive, startup success

**Internet TUN transition**:
A recoverable change between enabled and disabled Internet traffic capture, including its persisted intent and the route state required to restore user configuration.
_Avoid_: TUN toggle, config edit, TUN job

**Private Access session**:
A live connection attempt for one configured intranet profile, including authentication state and the network resources granted by its remote gateway.
_Avoid_: VPN process, connector job, profile runtime

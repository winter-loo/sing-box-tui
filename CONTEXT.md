# sing-box-tui

Terms that define how sing-box-tui owns proxy runtime and network-access behavior.

## Language

**Managed sing-box process**:
A sing-box process under sing-box-tui's explicit lifecycle ownership for one configuration; unrelated or user-owned sing-box instances are outside this concept.
_Avoid_: Managed child, adopted process, sing-box service

**Controller readiness**:
The state in which sing-box's controller accepts requests; an operating-system process can be running without being controller-ready.
_Avoid_: Process alive, startup success

**Internet TUN transition**:
A recoverable change between enabled and disabled Internet traffic capture, including its persisted intent and the route state required to restore user configuration.
_Avoid_: TUN toggle, config edit, TUN job

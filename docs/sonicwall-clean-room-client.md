# SonicWall SMA / Connect Tunnel clean-room client implementation notes

## 1. Scope and current status

This document records how `sing-box-tui` gained a self-contained SonicWall Secure
Mobile Access (SMA) / Connect Tunnel compatible client without launching, embedding,
or controlling the vendor client.

The implementation is intended for authorized interoperability with a gateway the
operator is allowed to access. It is a compatibility implementation, not an official
SonicWall client and not a claim that every SMA release, policy, or authentication
provider is supported. The protocol descriptions below are based on behavior observed
at the network boundary and on strict parsing/round-trip tests. Names for fields whose
official semantics are not public should therefore be read as implementation names,
not authoritative protocol documentation.

The completed path covers:

- HTTPS realm discovery and multi-step login, including username, password, and
  dynamically supplied one-time-code fields.
- The Connect Tunnel agent interrogation, policy evaluation, activation, and token
  handoff needed by the tested gateway.
- The proprietary EVPN tunnel setup over a specially formed TLS connection.
- Client configuration parsing, TUN creation, DNS setup, and split-route installation.
- Direct and explicit HTTP CONNECT proxy transports, with cached preference and a
  short staggered race between them.
- Integration into the existing Private Access service and TUI lifecycle.
- Gateway-compatible Connect Tunnel identity with native macOS endpoint-policy
  evaluation.

The code deliberately does not reuse vendor binaries or private libraries. The
installed client version was useful only as observable compatibility metadata (for
example, its product version and network-visible agent identity).

## 2. Clean-room development method

We separated evidence collection from implementation:

1. We recorded only externally visible inputs and outputs: HTTPS requests, JSON
   responses, TLS records, EVPN frame boundaries, assigned network configuration, and
   routing effects.
2. We built a small non-authenticating TLS probe first. It can compare a normal TLS
   handshake with the gateway-specific ClientHello and can use either a direct socket
   or an HTTP CONNECT proxy. It never submits credentials or EVPN data.
3. We implemented parsers and encoders from byte fixtures and added bounds checks
   before connecting them to a live session.
4. We kept authentication/control-plane code separate from the tunnel/data plane so
   a failure could be localized to HTTPS, TLS compatibility, EVPN negotiation, TUN,
   or routing.
5. We logged stage transitions and timing without logging passwords, one-time codes,
   complete tokens, or full packet contents.

This sequence was important. A normal browser-style HTTPS client can complete the
web login and still be unable to create a Connect Tunnel session. Conversely, a valid
EVPN codec is not useful until the web flow has produced a valid team token.

## 3. Code map

| Path | Responsibility |
| --- | --- |
| `src/sonicwall.rs` | HTTPS client, realm discovery, dynamic authentication, interrogation/EPC evaluation, agent activation, and EVPN identity extraction |
| `src/sonicwall/evpn.rs` | EVPN frame codec, fragmentation, LZ4 handling, tunnel bootstrap, configuration parsing, and data/control messages |
| `src/sonicwall/evpn/tls.rs` | Direct/HTTP CONNECT TCP underlay, gateway-specific ClientHello patch, and native TLS stream |
| `src/private_access.rs` | JSON-line service adapter, Gateway Profile cache, transport race, interactive challenge bridge, TUN/data loop, route/domain extraction, and diagnostics |
| `src/tun.rs` | Privileged TUN helper, pushed DNS/MTU/address configuration, packet I/O, and route lifetime guards |
| `src/config.rs` | sing-box carrier exception, system-DNS path, internal CIDR/domain rules, and idempotent config updates |
| `src/tui.rs` | SonicWall profile, dynamic login form, progress/error display, conflict warning, and service lifecycle |
| `src/bin/pas-sonicwall.rs` | Small service executable shim that dispatches to `sing-box-tui private-access-service sonicwall --stdio` |
| `scripts/sonicwall_tls_probe.py` | Standalone, non-authenticating TLS ClientHello compatibility probe |

The service boundary uses newline-delimited JSON. This keeps the TUI independent of
the tunnel implementation and lets the privileged TUN helper remain a smaller,
auditable process.

## 4. End-to-end session sequence

At a high level, one connection follows this sequence:

1. Normalize the configured gateway to an HTTPS origin.
2. Load its cached transport and logon-endpoint capabilities.
3. Race Direct and Proxy realm discovery with a 250 ms stagger, starting the cached
   winner first.
4. Retain the winning HTTP client and its cookie jar for the complete authentication
   session.
5. Create a login session, render each gateway-provided challenge, and send the user's
   response.
6. Evaluate supported endpoint-policy checks, activate the Connect Tunnel agent, and
   validate the returned logon/team token.
7. Open the EVPN TLS connection using the same HTTP CONNECT listener and the proxy
   selected by the existing Internet auto picker.
8. Negotiate EVPN version, authenticate the team token, exchange capabilities, and
   receive client network configuration.
9. Start the TUN helper with the assigned address, prefix, DNS servers, and MTU.
10. Install the gateway carrier exception and pushed private routes/domains.
11. Exchange raw IPv4 packets in EVPN DATA frames while servicing control frames.
12. On a recoverable EVPN loss or a stable Internet auto-picker selection change,
    reset the TUN context but retain the authorized helper process; reconfigure it
    after reconnect. On final disconnect, release all guards.

The HTTPS session token is never treated as a route or packet-layer credential. The
control plane produces a fixed-size EVPN identity, and only that validated identity is
passed into the tunnel bootstrap.

## 5. HTTPS authentication and control plane

### 5.1 Gateway normalization and origin safety

The client normalizes a user-provided host or URL to a canonical HTTPS origin. A
session-resource location returned by the gateway may be relative or absolute, but an
absolute location must have the same scheme, host, and effective port as the gateway.
This prevents a malicious or misconfigured response from redirecting credentials or
session data to a different origin.

### 5.2 Gateway Profile cache

Each normalized gateway has a small persisted profile containing:

- the last successful transport: `Direct` or `Proxy`;
- whether the modern `/__api__/logon` endpoint worked, was unsupported, or has not yet
  been determined.

On Windows the cache is stored below the user's local application-data directory.
On macOS and other Unix targets it uses `$XDG_CACHE_HOME/sing-box-tui` when set,
otherwise `~/.cache/sing-box-tui`. Updates use an atomic replacement so a crash
cannot leave a half-written JSON file. Cache read/write failures are warnings, not
VPN failures.

Persistence matters because the TUI intentionally stops the service process after an
error or disconnect. An in-memory preference appeared to work in unit tests but was
lost before the next real connection.

### 5.3 Direct/Proxy Happy Eyeballs

Serial direct-first fallback made an unreachable direct path particularly painful:
several discovery requests could each wait for their own timeout before the proxy was
attempted. The current algorithm treats transport choice as a race:

- the cached preferred candidate starts immediately;
- the other candidate starts 250 ms later;
- the first candidate that completes realm discovery wins;
- the losing future is cancelled;
- only the winner's HTTP client and discovered realms continue into login.

This preserves the desired “direct preferred, proxy fallback” policy without paying a
full direct timeout on every authentication attempt. A success also refreshes the
persisted preference for the next process.

### 5.4 One HTTP client per authentication session

The winning `reqwest::Client` is retained through discovery, login creation,
interrogation, challenge submission, and activation. It has a cookie store, uses
HTTP/1.1, and keeps at most one idle connection for the origin. This restores cookie
continuity, HTTP keep-alive, and TLS session resumption that were lost when each step
constructed a new client.

The idle pool timeout is intentionally 15 seconds. This is **not** a 15-second limit
on filling in the login form. The user may take as long as the gateway permits. The
timeout only removes an unused TCP/TLS connection from the client's pool. The tested
upstream closes idle connections sooner than our earlier 90-second setting; after a
human spent time entering credentials, the client tried to reuse a socket the server
had already closed. Evicting it after 15 seconds makes the next POST open a fresh
connection while retaining the cookie jar and TLS session cache.

### 5.5 Realm and logon discovery

Realm discovery uses the gateway's public configuration resources. Login creation
prefers the modern `/__api__/logon` endpoint unless the Gateway Profile says it is
unsupported. When the modern resource is unavailable, the client drains the failed
response body and falls back to the compatible `/__api__/logon/Add` form, then caches
the result.

Draining is a small but material detail: it allows an HTTP/1.1 connection to be reused
cleanly. Endpoint capability is distinct from transport preference; either one can
change without invalidating the other.

### 5.6 Dynamic challenges

The authentication UI is generated from the challenge returned by the gateway. It
does not assume “username + password + exactly one OTP”. Each challenge may contain:

- an arbitrary ordered set of text or sensitive fields;
- labels and messages;
- realm/choice options;
- one or more action buttons;
- a follow-up challenge after submission.

Sensitive values are held in `PrivateAccessSecret`, whose debug representation is
redacted and whose memory is zeroized on drop. The authentication dialog displays
password and dynamic-code input values as entered. The profile may opt into prefilling
fields explicitly marked `is-username` and `is-password`. Generic password fields
remain empty so a dynamic password or one-time code is never mistaken for the static
password. Interactive SonicWall replies are not written into the profile or Gateway
Profile cache. Authentication POSTs are not blindly retried: if a response is lost
after the gateway accepted the credentials, retrying could submit a one-time code
twice or advance the state machine incorrectly.

### 5.7 Interrogation, endpoint policy, and activation

The client sends Connect Tunnel-compatible agent information and processes the
gateway's system interrogation. Supported checks are evaluated against local state;
examples include understood file, directory, registry, OS-version, and process
conditions. Unknown checks are not reported as passing. The implementation either
returns a minimal truthful response for understood keys or fails closed for policy it
cannot evaluate.

After authentication, the client probes the relevant license/connection state,
activates the tunnel agent, finds the latest logon identifier, decodes it, and requires
the expected 16-byte team token. Malformed or missing tokens stop the connection before
any EVPN authentication is attempted.

## 6. EVPN TLS transport

### 6.1 Why ordinary TLS was insufficient

The gateway accepted ordinary HTTPS for the control plane but did not treat a normal
TLS client as the proprietary tunnel transport. Compatibility depended on the exact
first ClientHello. The EVPN connection injects compression method `0xEC` (called
`EVPN-Z` in this implementation) into the ClientHello and adjusts both the TLS record
length and handshake length.

The patcher is a stream wrapper: it buffers the first TLS record, validates that it is
a ClientHello, locates the compression-method vector with full bounds checks, inserts
the method only when absent, rewrites lengths, and then becomes a transparent stream.
Malformed or unexpected records fail instead of receiving a speculative byte edit.

The tunnel uses `native-tls` for this path because its handshake behavior matched the
tested gateway. The ordinary authentication client remains independent and uses the
Rustls-backed `reqwest` stack.

### 6.2 Direct and HTTP CONNECT underlays

The EVPN TLS stream can be created over:

- a direct TCP connection to the gateway; or
- an explicit HTTP CONNECT proxy.

For CONNECT, the request authority and TLS server name remain the original gateway
hostname. Replacing the host with a resolved IP can break virtual-host routing,
certificate verification, SNI, and the sing-box carrier-domain exception.

TLS connection retry is limited to three attempts and only for errors classified as
transient. Authentication state-changing requests are not covered by this retry rule.

## 7. EVPN frame protocol

### 7.1 Framing and decoder invariants

The base frame header is four bytes. Version 1 uses the high nibble `0x10`. An
extended frame has a 12-byte header and can carry a fragment identifier and total
length. The observed flags currently handled are:

- `0x01`: extended header/fragment metadata;
- `0x04`: LZ4-compressed payload.

The decoder is streaming. It accepts partial reads, multiple concatenated frames, and
extended fragments split across reads. Fragment assemblies are keyed and bounded; the
maximum reassembled message is 4 MiB. Every payload length, padded length, fragment
offset, and allocation is checked before use. LZ4 decompression is performed only
after a complete payload has been assembled and its declared output size validated.

This is deliberately different from “read one socket buffer, parse one frame”. TCP
does not preserve message boundaries, and live traffic exercised both coalescing and
fragmentation.

### 7.2 Bootstrap state machine

After the patched TLS handshake, the current bootstrap is:

1. Send EVPN protocol version 1.2.
2. Receive and validate `VERSION_ACK`, including the tunnel identifier.
3. Send the observed TEAM structure containing the raw 16-byte team token and client
   identifier.
4. Require the corresponding authentication acknowledgement.
5. Exchange capability (`CAPEX`) messages, including the compatible launch mode and
   LZ4 capability.
6. Answer a client-version request when the gateway sends one.
7. Send client address/interface information.
8. Receive and parse `CLIENT_CONFIG`.
9. Send `CLIENT_CONFIG_ACK`.
10. Enter the packet loop.

The codec also handles echo request/response, shutdown, alert, client-version, and
configuration control messages. Gateway shutdown/alert payloads are decoded to a
bounded diagnostic string so a protocol rejection is visible without dumping binary
session data.

### 7.3 Client configuration compatibility

`CLIENT_CONFIG` supplies the assigned IPv4 address and prefix, DNS servers, MTU/SSL
MTU, resource records, and attributes. We encountered more than one address-slot
layout, so the parser tries the current layout first and a validated version-1
compatibility layout when necessary. The fallback is driven by structural validation,
not only by server version strings.

Resource records can contain:

- IPv4 CIDRs;
- exact domains and domain suffixes;
- an inclusive IPv4 `RANGE=start,end` form.

An arbitrary address range is not necessarily one CIDR. The range converter repeatedly
selects the largest aligned block that remains within the range, producing the minimal
set of covering CIDRs. All extracted network values are normalized and deduplicated
before being handed to routing code.

### 7.4 Packet data

An EVPN DATA payload carries a raw layer-3 IPv4 packet; there is no extra Ethernet or
sub-packet header in the observed layout. Outbound packets read from the TUN device
are validated and framed as DATA. Inbound DATA frames are length-checked, decoded, and
written to the TUN device. `ECHO_REQ` is answered promptly with `ECHO_RSP` while packet
traffic is active.

Diagnostics keep counters and short protocol/IP summaries. They do not dump whole
application packets or authentication material.

## 8. TUN and routing integration

### 8.1 TUN configuration and lifetime

The privileged helper accepts an assigned address/prefix plus optional gateway, DNS
servers, and MTU. On Windows it configures the Wintun-style adapter, applies pushed DNS,
and keeps route guards alive for exactly the tunnel lifetime. Packet receive is
nonblocking/pollable and device shutdown wakes the loop; a previous blocking receive
could leave teardown stuck and surface Windows error 997 during shutdown.

### 8.2 Carrier route before internal routes

The VPN gateway itself is carrier traffic and must remain reachable before the tunnel
exists. A broad internal-domain rule can accidentally capture it. For example:

```text
vpn.example.com       -> normal Internet selector / auto picker
*.example.com         -> system DNS + direct through private tunnel
10.0.0.0/8            -> direct through private tunnel
```

The exact gateway-domain exception must appear before the generic internal suffix and
must already exist when sing-box starts. Adding it only after authentication creates a
bootstrap deadlock: authentication needs the carrier path, but route discovery cannot
finish until authentication succeeds.

“Proxy” here means the explicit HTTP CONNECT underlay chosen for SonicWall. It is not
the same as merely enabling a browser/system proxy. The gateway exception ensures the
CONNECT request can reach the local sing-box listener, while internal resources use
the established TUN path.

The gateway exception uses the same root Internet selector as ordinary proxy traffic.
The existing auto picker remains the only component allowed to measure or switch
proxy nodes. SonicWall only observes the resulting selector chain; after the same new
chain is seen twice, it re-establishes the long-lived EVPN connection so the next
HTTP CONNECT consumes the auto picker's decision.

### 8.3 Internal DNS and route resources

Pushed private CIDRs are installed as direct routes through the TUN adapter. Exact
domains and suffixes first use the system resolver (which now has the pushed DNS
servers) and then take the direct private-access path. This order prevents public or
proxy DNS from returning an unusable answer for an internal name.

Config mutation is idempotent: normalized carrier domains, CIDRs, ranges, and domain
resources are deduplicated, ordered by specificity, and only reported as changed when
the generated sing-box configuration actually differs.

## 9. Security and failure boundaries

The client follows these rules:

- Never persist interactive replies or one-time codes; only explicitly configured
  profile credentials may be stored for prefill.
- Treat the terminal as sensitive while the authentication dialog is open because
  password and dynamic-code inputs are intentionally visible.
- Treat the Private Access settings screen as sensitive because a configured direct
  password is intentionally displayed in plaintext.
- Mask sensitive form values and zeroize their backing strings on drop.
- Never include full team/logon tokens in logs or errors.
- Reject cross-origin session-resource locations.
- Validate all network lengths before slicing or allocating.
- Cap reassembly, proxy response headers, packet sizes, and diagnostic previews.
- Do not claim endpoint-policy compliance for unsupported checks.
- Do not automatically retry state-changing authentication POSTs.
- Treat cache persistence and diagnostics as best-effort; they must not weaken protocol
  validation.
- Warn when the vendor client is running, because two clients/adapters can compete for
  routes and make failures non-deterministic.

## 10. Diagnostics and tests

The service writes stage-oriented SonicWall diagnostics to
`sonicwall-private-access.log` at runtime. That file is a local troubleshooting
artifact and must not be committed. Important stages include transport discovery,
login endpoint selection, each challenge round, activation, EVPN TLS, TEAM/CAPEX,
configuration parsing, TUN start, route application, packet counters, and shutdown.

Errors preserve their cause chain. This was necessary because reducing a request error
to its top-level display text produced only “error sending request”, hiding whether the
real cause was DNS, TCP refusal, CONNECT rejection, TLS, a stale pooled connection, or
an HTTP status.

Test coverage includes:

- gateway/profile normalization and atomic cache reload;
- preferred/alternate transport timing;
- dynamic challenge parsing and secret redaction;
- same-origin redirect enforcement and logon-token decoding;
- EPC rule evaluation for supported and unsupported inputs;
- ClientHello patch fixtures and idempotence;
- base/extended EVPN frames, partial reads, fragmentation, LZ4, and size limits;
- VERSION, TEAM, CAPEX, address-info, DATA, and control message round trips;
- current and version-1 client configuration layouts;
- CIDR/domain/range extraction and minimal range decomposition;
- sing-box carrier-rule ordering and idempotent route generation;
- TUN helper configuration, reset protocol, backpressure retry, and service/TUI
  event handling.

The live gateway TLS test is ignored by default. It requires explicit network access
and should never be a prerequisite for deterministic local or CI tests.

## 11. Lessons learned

### Protocol discovery

1. **The web login and packet tunnel are different protocols.** Completing HTTPS
   authentication proves only the control plane. Track web session, EVPN identity,
   tunnel negotiation, and routes as separate milestones.
2. **A successful generic TLS handshake does not prove tunnel compatibility.** The
   exact ClientHello bytes mattered; adding EVPN-Z `0xEC` was the point at which the
   gateway recognized the connection as Connect Tunnel traffic.
3. **Build narrow probes before a full client.** The non-auth TLS probe isolated
   ClientHello and CONNECT behavior without consuming OTPs or mixing in EVPN parsing.
4. **Observed layouts need validation-based compatibility paths.** Server/version
   labels alone were insufficient; current and older client-config layouts must be
   selected by checking their structure.

### HTTP and authentication

5. **Reuse the client, not a possibly stale socket.** One HTTP client preserves cookies
   and TLS sessions. A short idle-pool timeout lets it discard server-closed sockets
   after human think time.
6. **The 15-second value is not a form timeout.** It controls only pooled connection
   eviction. Conflating it with the user's input deadline led to the wrong diagnosis.
7. **Drain failed responses before fallback.** This is required for reliable keep-alive
   reuse on HTTP/1.1.
8. **Do not blindly retry authentication POSTs.** A lost response does not mean the
   gateway rejected the OTP; replay can consume or duplicate a valid action.
9. **Render the protocol's form instead of hardcoding the current form.** Real gateways
   can add a realm selector, acknowledgement, second OTP, or policy message.
10. **Cache endpoint capability independently.** Re-probing a consistently unsupported
    `/logon` resource added latency and noise even when the transport choice was right.

### Transport and routing

11. **Direct-first should be a race, not a chain of timeouts.** A 250 ms stagger gives
    direct a meaningful head start while proxy fallback remains fast.
12. **Persist transport knowledge across process restarts.** The service lifecycle made
    an in-memory cache practically useless.
13. **Preserve the gateway hostname through CONNECT.** It participates in SNI,
    certificate verification, virtual hosting, and policy routing.
14. **Carrier routing can deadlock VPN bootstrap.** The exact gateway exception must
    precede a broad internal-domain direct rule and exist before connection begins.
15. **System/browser proxy and explicit CONNECT are different mechanisms.** The tunnel
    process needs a concrete proxy endpoint and a route to it; browser settings alone
    do not provide that underlay.
16. **DNS is part of split tunneling.** Private routes alone cannot make an internal URL
    work if the hostname is still resolved by public/proxy DNS.

### Framing and operating-system integration

17. **Never equate one TCP read with one protocol frame.** Streaming decode and bounded
    reassembly are correctness and security requirements.
18. **Every wire length is untrusted.** Header lengths, padding, fragment totals,
    decompressed lengths, resource lengths, and packet lengths all need independent
    checks.
19. **Address ranges are not CIDRs.** Convert them algorithmically to minimal aligned
    CIDRs instead of widening access to a convenient supernet.
20. **Shutdown behavior is part of a TUN implementation.** A correct packet path that
    cannot wake a blocking OS read still produces hung services and misleading errors.
21. **Conflicting VPN clients produce non-protocol symptoms.** Detecting the official
    client helps distinguish route/adapter competition from an EVPN negotiation bug.

### Debugging and maintenance

22. **Log stages, timings, and cause chains—not secrets.** This made the slow realm
    discovery, unsupported endpoint, stale keep-alive socket, and route deadlock
    distinguishable without exposing credentials.
23. **Separate transport selection from protocol correctness.** Switching to a proxy
    can fix reachability but cannot fix malformed TEAM, CAPEX, or CLIENT_CONFIG data.
24. **Keep live tests opt-in.** Deterministic byte fixtures find codec regressions without
    depending on a gateway, consuming an OTP, or leaking environment details.
25. **Treat the implementation as a targeted compatibility layer.** New SMA firmware,
    endpoint policies, IPv6 configuration, or message variants should be added from
    new evidence with fixtures and bounds checks, not guessed into the existing parser.

## 12. Known limitations and safe extension points

- The proven data path is IPv4. IPv6 client configuration and DATA traffic need
  separate fixtures and route/TUN work.
- Endpoint-policy coverage is intentionally incomplete; add a rule only when it can be
  evaluated truthfully on the local platform.
- The EVPN implementation targets the observed Connect Tunnel family. Other SonicWall
  products or firmware may use different authentication resources, capabilities, or
  frame layouts.
- Proxy support is HTTP CONNECT. SOCKS or authenticated enterprise proxies would need
  explicit transport implementations and secret-handling review.
- Gateway certificate verification should remain enabled in normal use. Test-only
  relaxation must never become the default.

When extending the client, keep the current boundary: collect authorized external
evidence, add a bounded parser/encoder fixture, expose a precise stage error, and only
then connect the new behavior to a live authentication session.

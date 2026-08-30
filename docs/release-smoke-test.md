# Node-quality release smoke test

Run this bounded manual smoke after automated release validation. It exercises live controller
integration and isolated runtimes without turning account credentials into test output. Skip the
authenticated custom-probe step when no approved account is available and record it as skipped,
not passed.

## Safety boundary

- Use a disposable copy of the active sing-box config for the smoke when possible.
- Confirm the sustained target is an account-free HTTPS object with the expected 512 KiB body.
- Do not put tokens, cookies, account headers, proxy URLs, or outbound JSON in manifests, command
  lines, screenshots, logs, or the report.
- Record the live selector before and after every candidate-measurement step. Quick, sustained, and
  custom probes must leave it unchanged.
- Do not press Space or enable automatic selection during the measurement checks. The final
  active-transfer check enables automatic selection only after a controlled transfer has started.
- Stop immediately if an isolated runtime exposes a non-loopback listener or the selected outbound
  changes before the explicit automatic-selection check.

## Preconditions

1. Build and validate the exact release candidate:

   ```sh
   cargo fmt --all -- --check
   cargo clippy --all-targets --all-features -- -D warnings
   cargo test --all-features -- --test-threads=1
   cargo build --release --locked
   ```

2. Start the release binary with a controller-ready managed sing-box configuration. Keep the
   selector on a known node and disable automatic selection.
3. Capture a redacted baseline with `sing-box-tui selectors --selector <selector>`. Retain only the
   selector name and current node tag.

## 1. Quick reachability assessment through the live controller

1. Focus the Current selector panel and press `T`.
2. Confirm each tested row reaches one of the factual states `3/3`, `2/3`, `1/3`, `0/3`, or
   `incomplete`; a controller failure must be incomplete rather than a failed node.
3. Press `i` on one assessed node and verify three ordered probe outcomes and the derived
   reachability assessment are visible.
4. Re-read the selector. Its current node must equal the baseline.

Pass evidence: bounded status text, three factual outcomes in detail, and an unchanged selector.

## 2. Bounded sustained quality through an isolated runtime

1. Confirm the configured sustained target and 512 KiB expected-body contract.
2. Highlight one node and press `t` once. Do not start another manual deep assessment.
3. While it runs, verify the probe listener is loopback-only. Confirm the transfer stops after the
   expected body and reports first-byte time, completion time, bytes read, and effective
   throughput. A runtime or transfer failure must remain an infrastructure outcome.
4. Open `i` and verify sustained-quality evidence is separate from reachability evidence.
5. Re-read the selector and confirm it is unchanged.

Pass evidence: exactly 524288 bytes consumed on success, bounded completion, isolated loopback
runtime cleanup, and an unchanged selector.

## 3. Authenticated custom usability criterion, when approved

1. Register a local executable manifest following
   [`usability-probe-manifests.md`](usability-probe-manifests.md). Leave `background` absent or
   false. Keep credentials in the tool's approved credential store or environment, never in the
   manifest.
2. Focus its untested panel and press `U`. For Agy Gemini, authorize one real request only after
   confirming the intended account and quota policy.
3. Verify progressive node results appear, a node enters the panel only after the application
   command succeeds, and authentication/process/runtime failure leaves the run incomplete.
4. Verify stdout/reporting contains only bounded node tags, outcome state, and timing—not request
   content, account data, configuration, or proxy addresses.
5. Re-read the selector and confirm it is unchanged.

Pass evidence: one approved authenticated request, correct panel membership or explicit incomplete
state, redacted diagnostics, isolated runtime cleanup, and an unchanged selector.

## 4. Panel display and expiry

1. Move Left/Right through Current selector, Streaming, and every registered custom panel.
2. Confirm Current selector retains every member in selector order. Confirm other panels show their
   usable count and only their criterion's eligible members.
3. With a short-lived disposable manifest result, wait for its TTL to elapse or use a test build
   with a bounded TTL. Confirm the result remains visible as expired in `i` but is excluded from
   panel and automatic-selection candidates.

Pass evidence: stable tabs/counts, selector-scoped membership, and visibly expired evidence that is
not selectable.

## 5. Active-transfer switch protection

1. Start a controlled download through the current node and confirm connection counters grow by
   more than 64 KiB within 10 seconds.
2. Enable automatic selection with `a` on a panel that has a materially better eligible candidate.
3. Allow two complete assessment rounds. Confirm the status explains that switching is deferred
   because of active current-node transfer and that the selector remains unchanged.
4. Disable automatic selection **before** stopping the transfer, then stop the transfer and wait
   for a fresh idle traffic window. This smoke does not authorize the deferred candidate to switch
   after active-transfer protection is released.

Pass evidence: two-round candidate evidence, an explicit active-transfer deferral explanation, and
an unchanged selector while bytes are growing.

## Report

Record the release commit, platform, sing-box version, controller readiness, each step as
pass/fail/skipped, elapsed time, and redacted evidence. Do not attach the database, manifests with
local paths, raw controller payloads, or application responses.

# SonicWall Transport Setting Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:test-driven-development to implement this plan task-by-task.

**Goal:** Let each SonicWall profile explicitly choose direct transport or the configured Internet proxy, with no automatic fallback.

**Architecture:** Persist a boolean `use_internet_proxy` on Private Access profile state and expose it only for SonicWall in the TUI settings. Build the SonicWall connect command from that value: proxy fields are populated only when enabled; direct mode sends no proxy/controller/selector fields, causing both HTTPS authentication and EVPN to use direct transport exclusively.

**Tech Stack:** Rust 2024, serde/serde_json, ratatui, existing unit-test modules.

---

### Task 1: Persist the profile transport choice

**Files:**
- Modify: `src/tui_state.rs`
- Modify: `src/tui.rs`

**Steps:**
1. Add a failing serialization/runtime round-trip test for `use_internet_proxy`.
2. Run the focused test and verify it fails because the field does not exist.
3. Add the boolean state/runtime field, defaulting SonicWall to direct and Hillstone to false.
4. Run the focused test and verify it passes.

### Task 2: Expose the SonicWall-only TUI setting

**Files:**
- Modify: `src/tui.rs`

**Steps:**
1. Add failing tests that the setting is visible for SonicWall, hidden for Hillstone, editable only while disconnected, and persisted.
2. Run the tests and verify failure.
3. Add `PrivateAccessUseInternetProxy` to labels, values, visible fields, parsing, and locked-setting validation.
4. Run the focused tests and verify they pass.

### Task 3: Make command transport exclusive

**Files:**
- Modify: `src/tui.rs`
- Modify: `src/private_access.rs`

**Steps:**
1. Add failing tests for a pure connect-transport plan: direct yields no proxy/controller/selector; proxy mode yields all configured proxy context.
2. Run the tests and verify failure.
3. Use the plan when constructing the SonicWall connect command.
4. Change SonicWall transport setup so a configured proxy means proxy-only and no configured proxy means direct-only; remove Happy Eyeballs fallback for this path.
5. Run focused SonicWall tests and verify they pass.

### Task 4: Document and verify

**Files:**
- Modify: `docs/private-access-service.md`
- Modify: `sing-box-tui.json`

**Steps:**
1. Document `use_internet_proxy` and its direct default.
2. Add the setting to the example SonicWall profile.
3. Run `cargo fmt --check`, `git diff --check`, and `cargo test`.
4. Build with `cargo build --release`.

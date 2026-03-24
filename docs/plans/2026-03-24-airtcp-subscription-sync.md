# AirTCP Subscription Sync Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add a minimal provider-login sync path that logs into a provider website, fetches the sing-box subscription URL, downloads the JSON config, and merges provider nodes into the live sing-box config.

**Architecture:** Prefer the simplest transport first: plain HTTP requests with cookie persistence and HTML or JS scraping, not browser automation. Keep provider-specific scraping isolated behind a small provider module so the existing import and merge code stays reusable.

**Tech Stack:** Rust, reqwest, serde, serde_json, existing config merge pipeline.

---

### Task 1: Add provider sync CLI surface

**Files:**
- Modify: `src/cli.rs`
- Modify: `src/main.rs`
- Test: `src/cli.rs`

**Step 1:** Add a `sync-provider` command that accepts provider URL, account file, live config path, and optional output path.

**Step 2:** Add CLI tests for default parsing and required arguments.

### Task 2: Implement provider login + subscription extraction

**Files:**
- Create: `src/provider.rs`
- Modify: `Cargo.toml`
- Test: `src/provider.rs`

**Step 1:** Implement a provider client backed by `reqwest` cookie store.

**Step 2:** Implement the AirTCP login flow with `POST /denglu`.

**Step 3:** Implement authenticated extraction of the sing-box subscription URL using HTML/asset scraping.

**Step 4:** Add tests for credential parsing and extraction helpers.

### Task 3: Reuse import/merge pipeline for live config sync

**Files:**
- Modify: `src/import.rs`
- Modify: `src/config.rs`
- Modify: `src/main.rs`
- Test: `src/config.rs`

**Step 1:** Add a function that accepts sing-box JSON subscription text directly.

**Step 2:** Merge provider nodes into the live config and write the updated config.

**Step 3:** Optionally persist the downloaded provider payload for debugging.

### Task 4: Verify end-to-end path

**Files:**
- Modify: `README.md`
- Test: existing unit tests

**Step 1:** Run `cargo test`.

**Step 2:** Document the provider sync command and account file format.

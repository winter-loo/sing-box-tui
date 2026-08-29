# Issue tracker: GitHub

Issues and specs for this repository live in GitHub Issues at `winter-loo/sing-box-tui`. Use the `gh` CLI for all operations.

- Publishing a spec or ticket means creating a GitHub Issue.
- Read tickets with `gh issue view <number> --comments`.
- Apply labels with `gh issue edit`.
- Close completed tickets with `gh issue close`.
- Pull requests are not treated as incoming triage requests.
- Use native GitHub sub-issues and blocking dependencies when available.
- If native dependencies are unavailable, record `Blocked by: #<number>` in the issue body.
- A ticket is ready only when all blocking issues are closed.

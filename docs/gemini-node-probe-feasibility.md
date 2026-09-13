# Gemini Node Probe Feasibility

## Question

Can sing-box-tui test every node against an arbitrary URL and list only nodes that are actually usable for that site, where an HTTP response containing “Gemini isn't currently supported in your country” (or its localized equivalent) counts as unusable?

This note is a feasibility study only. It does not change product code or the running sing-box selector.

## Conclusion

The feature is feasible, but sing-box's built-in URLTest and Clash-compatible delay endpoint are insufficient as the final result.

A useful test needs two distinct layers:

1. Transport test: did the node establish TCP/TLS and receive an HTTP response within the timeout?
2. Application test: after a real `GET`, did the final response represent usable Gemini content rather than a regional restriction, login/eligibility restriction, challenge, or error page?

The existing delay API can be retained as an optional fast pre-filter. The decisive Gemini check must read the response body (or another stable first-party application signal).

## What sing-box Can Do

### URLTest tests a list of outbounds against a configurable URL

The official URLTest configuration accepts an `outbounds` list and a test `url`; the default URL is `https://www.gstatic.com/generate_204`. It also exposes interval, tolerance, idle timeout, and connection-interruption settings.[^singbox-urltest-doc]

### Selectors can be controlled through the Clash API

The official Selector documentation says a selector is currently controlled through the Clash API. It also defines the selectable outbound list and whether existing inbound connections should be interrupted when selection changes.[^singbox-selector-doc] The official Clash API configuration exposes a REST controller through `external_controller`.[^singbox-clash-api-doc]

The upstream Clash API implementation confirms the relevant mechanics:

- `PUT /proxies/{name}` selects a member, and only accepts a Selector.
- `GET /proxies/{name}/delay` accepts `url` and `timeout`, then invokes the common URLTest implementation for that outbound.[^singbox-clash-proxies-source]

Therefore, enumerating nodes and selecting a concrete node is technically possible with the controller already used by this project.

## Why Native URLTest Cannot Decide Gemini Usability

The current upstream URLTest implementation:

- opens a TCP connection through the chosen outbound;
- sends an HTTP `HEAD` request;
- does not follow redirects;
- does not read response content;
- does not reject an HTTP response based on its status code;
- treats a completed HTTP exchange as success and returns elapsed milliseconds.[^singbox-urltest-source]

The upstream request to add HTTP status checking was closed as not planned.[^singbox-status-issue]

Consequences:

- A `200` regional-block page is “successful” to URLTest.
- A `3xx`, `403`, or `503` response can also be recorded as a delay if the HTTP exchange itself completes.
- Because the request is `HEAD`, a sentence in the HTML body can never be detected.
- Because redirects are not followed, URLTest does not observe the browser's final destination.

Thus `/delay?url=https://gemini.google.com` answers “can this outbound complete this limited HTTP probe?” It does not answer “can this user use Gemini through this outbound?”

## Reliability of the Gemini Regional-Restriction Message

Google's official availability page says Gemini Web availability depends on supported countries/territories and distinguishes Web availability from mobile availability. It currently lists Mainland China as “Workspace only.”[^gemini-availability]

The observed “currently not supported in your country” page is a strong negative result for that exact request context. It is not, by itself, a universal property of the proxy node:

- Gemini may localize the message, so matching one Chinese sentence is brittle.
- Google documents separate eligibility and requirements in addition to country availability; account type or session state can therefore affect the result.[^gemini-availability]
- A signed-out lightweight HTTP client and a signed-in browser do not necessarily exercise the same application state.
- Bot/challenge, consent, login, transient server-error, and partial HTML responses are neither “regional block” nor proven success.

Accordingly, the result should be described as “usable for this URL under this probe profile,” not simply “the node is good.”

## Recommended Minimal PoC

Use a real application-layer request, with explicit classification:

1. Enumerate concrete node members from the selector while excluding selectors, URLTest groups, direct/block entries, and provider metadata, following the existing convention in `docs/subscription-benchmark.md`.
2. Probe nodes serially if using the live selector. Select one node via the controller, create a fresh HTTP connection through the local mixed proxy, send a bounded `GET`, read a bounded body, record the final URL, status, content type, and timing, then close the connection before moving to the next node.
3. Restore the selector's original member in a `finally`/scope-guard path even after cancellation or failure.
4. Classify results as:
   - `usable`: the response matches a positive Gemini application signal;
   - `region_blocked`: a maintained set of localized restriction markers or a stable first-party restriction signal matches;
   - `reachable_unknown`: HTTP worked, but neither positive nor known-negative evidence matched;
   - `transport_failed`: DNS, connect, TLS, timeout, or response-read failure.
5. Never turn `reachable_unknown` into `usable`. This avoids the same false positive as URLTest.

For the first experiment, fix request headers such as `Accept-Language` and User-Agent, cap redirects and body bytes, disable connection reuse between nodes, and retain a short diagnostic fingerprint rather than full page content.

## Runtime and Product Risks

Testing by changing the live selector is viable for a manual PoC, but it has operational side effects:

- new ordinary traffic may use the node currently under test;
- existing connections may remain on their old outbound when `interrupt_exist_connections` is false; the official docs explicitly distinguish existing inbound connections from internal connections;[^singbox-selector-doc]
- parallel live-selector testing is invalid because one selector has only one current member;
- cancellation or a process crash can leave the selector on a test node unless restoration is designed carefully.

The safer product architecture is an isolated probe context: a temporary sing-box process/config on private, dynamically allocated local ports, or another mechanism that can dial each outbound directly without mutating the live selector. That design can support bounded parallelism and cannot redirect unrelated user traffic. It costs more implementation work and process/config lifecycle handling.

## Decision

Proceed with a small PoC, but make its decisive test a real `GET` plus application-level classification. Do not implement this feature as a different URL passed to the existing latency endpoint: that approach will report regional-block pages as successful.

Before production implementation, capture one known-good and one known-region-blocked response using the same signed-out request profile and determine a stable positive signal. If Gemini requires browser JavaScript or authenticated state to distinguish success reliably, the product should surface `reachable_unknown` rather than pretending the node is usable.

## Sources

[^singbox-urltest-doc]: sing-box, [URLTest configuration](https://sing-box.sagernet.org/configuration/outbound/urltest/).
[^singbox-selector-doc]: sing-box, [Selector configuration](https://sing-box.sagernet.org/configuration/outbound/selector/).
[^singbox-clash-api-doc]: sing-box, [Clash API configuration](https://sing-box.sagernet.org/configuration/experimental/clash-api/).
[^singbox-clash-proxies-source]: SagerNet/sing-box, [`experimental/clashapi/proxies.go`](https://github.com/SagerNet/sing-box/blob/testing/experimental/clashapi/proxies.go).
[^singbox-urltest-source]: SagerNet/sing-box, [`common/urltest/urltest.go`](https://github.com/SagerNet/sing-box/blob/testing/common/urltest/urltest.go).
[^singbox-status-issue]: SagerNet/sing-box, [Issue #1676: check status code in URLTest](https://github.com/SagerNet/sing-box/issues/1676).
[^gemini-availability]: Google Gemini Apps Help, [Where you can use the Gemini web app](https://support.google.com/gemini/answer/13575153?hl=en).

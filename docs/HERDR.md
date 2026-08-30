# Herdr integration contract

Jcode has built-in terminal routing for Herdr. When a headed session launch is requested from a client with `HERDR_ENV=1` and `HERDR_PANE_ID`, Jcode splits the calling pane to the right, focuses the new pane, and starts the resumed Jcode session there. `HERDR_BIN_PATH` is honored when present.

This covers visible swarm spawns, resume-in-new-terminal, self-development launches, and restart restores because they all use the shared terminal launcher. A configured `[terminal].spawn_hook` still takes precedence.

## Current compatibility

Jcode already:

- forwards `HERDR_ENV`, `HERDR_SOCKET_PATH`, `HERDR_PANE_ID`, `HERDR_TAB_ID`, `HERDR_WORKSPACE_ID`, `HERDR_BIN_PATH`, `HERDR_SESSION`, and `HERDR_AGENT` from the requesting client to server-side spawn and focus paths;
- recognizes Herdr as a masking terminal multiplexer for Mermaid graphics capability detection;
- exports stable lifecycle observer hooks for `session_start`, `session_end`, `turn_start`, and `turn_end`;
- exports `JCODE_HOOK_SESSION_ID`, `JCODE_HOOK_CWD`, event fields, and a JSON `JCODE_HOOK_PAYLOAD`;
- resumes a native session with `jcode --resume <session-id>`;
- reports lifecycle natively to Herdr (`crates/jcode-base/src/herdr.rs`): when the
  client's request-scoped env carries `HERDR_ENV=1` + `HERDR_SOCKET_PATH` +
  `HERDR_PANE_ID`, jcode emits `pane.report_agent_session` (source
  `herdr:jcode`, agent `jcode`) plus `pane.report_agent` state transitions
  (`working` during a turn, `idle` when settled, `blocked` while permission
  requests are unresolved) and `pane.release_agent` on `session_end`. No Herdr
 -side configuration is required; `JCODE_HERDR_REPORT=0` disables it. On the
  shared daemon, reports are per client pane via the same task-local terminal
  env that scopes shell hooks (`hooks::with_client_terminal_env`).

Verified end to end against Herdr 0.8.2: panes running a jcode TUI appear in
`herdr agent list` as `jcode` with live `working`/`idle`/`done` rollups and
release on close. The only piece Herdr still ignores is session-reference
*persistence* (see below): reporting uses Herdr's documented custom-source
protocol, but restoring requires the official-agent allowlist.

## Recommended first Herdr integration

Herdr-side restore support is still the missing half. Jcode's native emitter
already sends the session identity Herdr needs; Herdr only persists
`agent_session_id` for its hard-coded official sources, so accepting
`("herdr:jcode", "jcode")` is the single upstream change that enables
restore. The request shape jcode already emits:

```json
{
  "id": "herdr:jcode:<unique-request-id>",
  "method": "pane.report_agent_session",
  "params": {
    "pane_id": "<HERDR_PANE_ID>",
    "source": "herdr:jcode",
    "agent": "jcode",
    "seq": 1,
    "agent_session_id": "<session id>",
    "session_start_source": "startup"
  }
}
```

The sequence is monotonically increasing for the source. Jcode maps hook
sources as follows:

- `create` or `attach` to `startup`
- `resume` to `resume`

Herdr should restore the session with:

```text
jcode --resume <agent_session_id>
```

Jcode session IDs are opaque strings and fit Herdr's ID-based session
reference model. No transcript path is needed.

## Required Herdr-side work

A first-class integration cannot be shipped only as a remote detection manifest. Herdr currently hard-codes known agent kinds, official session sources, restore commands, and install targets. The upstream implementation needs:

1. Add `jcode` to `IntegrationTarget`, CLI parsing, labels, command discovery, recommendations, status, install, and uninstall handling.
2. Nothing to install for lifecycle reporting: jcode emits natively (see Current compatibility). Any Herdr-side hook adapter would be redundant; keep the integration session/restore-only.
3. Accept `(herdr:jcode, jcode)` as an official session source (state and session reports already arrive on this socket protocol from live jcode builds).
4. Persist its ID session reference and map it to `jcode --resume <id>` during restore.
5. Add Jcode process detection (foreground `jcode` binary) for pre-native-build sessions; a bundled screen manifest is optional now that jcode is a lifecycle authority when attached.
6. With native reporting, jcode is a full lifecycle authority (working/idle/blocked/release) whenever it is attached; Herdr should prefer live reports and fall back to process detection only when none arrived (old builds, disabled reporting, daemon crash before release).
7. Add integration versioning, replacement-source handling, schema/UI wiring, install/uninstall tests, restore-plan tests, detection fixtures, and documentation.

Relevant upstream files as of Herdr commit `eacea2daf0b72973173b728936b27478374f2cd2`:

- `src/integration/{mod.rs,registry.rs,targets.rs,actions.rs,version.rs}`
- `src/integration/assets/`
- `src/api/schema/integrations.rs`
- `src/agent_resume.rs`
- `src/detect/mod.rs`
- `src/terminal/state.rs`

## Lifecycle authority (implemented)

`crates/jcode-base/src/herdr.rs` is the native emitter and is a lifecycle
authority for attached sessions. Deliberate design choices:

- `blocked` covers queued permission requests (`safety::PermissionEvent`
  observer). Decisions made in another process (the `jcode permissions` TUI,
  email/Telegram reply, expiry) are not individually observable here, so a
  new `turn_start` clears stale pins — user activity is proof the prompt
  was answered.
- Releasing on `session_end` covers normal close; a daemon crash leaves a
  stale `working` until Herdr's process detection disagrees. Mitigated by
  the release-per-pane invariant (a pane is owned by at most one tracked
  session; headless swarm workers inside the spawning client's scope never
  hijack it). A future SIGTERM/handoff release hook would close the rest.
- Reports fire only inside a Herdr client scope: the shared daemon calls
  `mark_shared_daemon()` and then relies on per-request terminal env, so a
  daemon pane never misattributes another pane's session, and non-Herdr
  clients are unaffected. `pre_tool`/`post_tool` stay out of the funnel
  (hot paths, no pane meaning).

## Future refinements

A later Jcode/Herdr protocol can add explicit events for approval resolution
across processes and reconnect/reload transfer of reporting ownership so
Herdr never displays a stale working or idle state even for abnormal
transitions. Jcode today already reports `working`, `idle`, and `blocked`
through `pane.report_agent`, sends `pane.report_agent_session` identity, and
calls `pane.release_agent` on session end.

Official references:

- <https://herdr.dev/docs/integrations/>
- <https://herdr.dev/docs/socket-api/>
- <https://herdr.dev/docs/agents/>
- <https://herdr.dev/docs/session-state/>

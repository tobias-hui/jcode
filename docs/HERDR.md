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
  requests are unresolved) and `pane.release_agent` on `session_end`. No
  Herdr-side configuration is required; `JCODE_HERDR_REPORT=0` disables it. On the
  shared daemon, reports are per client pane via the same task-local terminal
  env that scopes shell hooks (`hooks::with_client_terminal_env`).

Verified end to end against Herdr 0.8.2: panes running a jcode TUI appear in
`herdr agent list` as `jcode` with live `working`/`idle`/`done` rollups and
release on close. The only piece Herdr still ignores is session-reference
*persistence* (see below): reports arrive on Herdr's socket protocol with
the `herdr:jcode` source, but restoring requires Herdr to accept that
source on its official-agent allowlist.

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

## Known gap (verified against Herdr 0.8.2)

Behavior established by direct socket experiments (NDJSON against
`$HERDR_SOCKET_PATH`):

- `pane.report_agent` / `pane.report_agent_session` from any `source` are
  accepted and drive the pane's agent label, state, rollups, and waits.
- `agent_session_id` is *persisted* (and later used for restore) only for
  Herdr's hard-coded official sources. A `herdr:jcode`-shaped source is
  accepted but the reference is dropped; a `custom:*` source likewise stores
  nothing.
- `pane.release_agent` and stale-`seq` ordering behave per docs; releases
  with a seq below the last accepted state report are ignored, so teardown
  must reuse the reporter's own seq ramp.
- Local `~/.config/herdr/agent-detection/<agent>.toml` overrides can only
  replace manifests for agents Herdr already recognizes; an unknown id stays
  `fallback_reason: unknown_agent`. A new agent therefore requires the
  upstream changes below regardless of what the agent emits.

## Upstream status (as of 2026-08-30)

The Herdr-side work is not speculative; it is an open PR by the jcode
maintainer: [herdrdev/herdr#2248](https://github.com/herdrdev/herdr/pull/2248)
(invited via herdr Discussion #1848), which adds `jcode` to
`IntegrationTarget`, ships a `session_start` hook adapter, accepts
`("herdr:jcode", "jcode")` as an official session source, persists the id
reference, maps restore to `jcode --resume <id>`, and bundles process
detection. Review status: detection approved; the maintainer asked for
fixes to the adapter's shared-server pane routing and `sh -c` hook execution
(herdr#2248 review, 2026-08-05) — both are solved jcode-side by
[jcode#758](https://github.com/1jehuang/jcode/pull/758) (multi-hook arrays +
per-client terminal env, merged 2026-08-06, shipped in v0.81.x), and the PR
is waiting on a rebase by its author.

Coexistence with the native emitter: if the adapter ever lands and gets
installed, it appends a `herdr-agent-state` command to
`[hooks] session_start` and reports under the *same* `herdr:jcode` source;
two reporters on one source would drop each other's updates via the seq
ramp. `herdr.rs` therefore defers to the adapter automatically whenever one
is configured (`hook_adapter_installed`), so installing it or not is always
safe. Until #2248 lands, Herdr shows no session restore for jcode (its
official-source allowlist drops the reference — see Known gap above), which
is the only lifecycle feature the native emitter cannot cover alone.

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

- `blocked` covers permission requests flowing through the safety queue
  (`safety::PermissionEvent`): the ambient `permission` tool, and decisions
  resolved via `record_decision`, dead-session expiry, or IMAP/Telegram
  file-based replies. The interactive TUI does not currently model
  ask-the-user as a safety-queue wait, so a mid-turn question publishes
  `working` and settles to `idle` on turn end — matching how Herdr treats
  plain idle prompts for agents without a visible approval UI. Wiring an
  explicit question/approval lifecycle into safety would extend blocked
  coverage without changing the emitter.
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

# Splash mini-apps in Robrix (the `a2app` feature)

AI-generated, sandboxed [Splash] mini-apps that run inside Robrix — in their own
isolated script VMs, with a mobile-OS-style permission system, and optional access
to Matrix itself (read/send messages in the room they're attached to).

Everything is gated behind the `a2app` cargo feature:

```bash
cargo run --features a2app
```

## What you get

- A **Mini Apps** button in the navigation bar (sparkle icon; can be hidden in
  App Settings) that opens the Mini Apps screen: create apps with AI, run them,
  and manage each one's permissions, versions, storage, and source.
- A **create bar**: describe an app ("a pomodoro timer") and an ACP agent
  (`octos acp` by default; Claude Code and any other ACP agent work too)
  writes it in the Splash dialect, validated with the real parser and
  auto-repaired for up to two turns.
- **Per-app isolation**: each app runs in its own Splash isolate with nothing
  by default: no filesystem beyond its private jail, no network, no host access.
  Capabilities are declared in the app's manifest, prompted at first use
  (Allow / Allow Once / Don't Allow / Not Now), revocable at any time, and a
  request-flooding app gets stopped and restricted.
- **Matrix services**, each behind its own permission group: the attached
  room (info, messages, older history, one event, threads and replies,
  members, pins, search, read receipts, unread counts, power levels,
  permalinks, upgrades), rooms and spaces (list, search, invites, previews,
  cross-room read and search, space trees), the account (profile, device,
  homeserver, ignored users, other users' profiles, DM lookup), and, behind
  the write switch, sends, replies, reactions, typing, read receipts, pins,
  room flags, invites, joins and DMs.
- **Live updates instead of polling**: room hooks (messages, edits and
  redactions, reactions, typing, read receipts, members, pins, room details,
  unread counts), account hooks (room list, invites, unread totals), and host
  hooks (focus, surface, display settings, which room or screen the user is
  on).
- **Host and pane services**: an app can read where it runs (surface, dock
  side, size, platform, view mode), move, minimize, break out or close its
  own pane, and read Robrix's display settings and device facts.
- **Refusals are Robrix's to show**: a mini-app that tries something it may
  not do, or whose action fails, gets a Robrix warning naming the app and the
  reason; it never depends on the app surfacing its own error. Deleting a
  provider key, an app's data, or an app asks first.
- **Writes are off by default**: the "Mini-apps can write to rooms" switch on
  the Mini Apps screen gates every send, on top of per-app permissions.
- **Version history**: every AI change, hand edit (there is a source editor),
  and version switch is kept; switch back and forth freely, diff any version
  against the current source, and reset a built-in to stock.
- **Sharing**: export any app as a `.splashapp` bundle (file + clipboard),
  hand it to another app through the system share sheet, send it into a room
  as a file attachment with a pre-filled caption ("Send to room…"), or post
  it with `/miniapp share <name>`, where it renders as a card other Robrix
  users can install and run. "Open in room…" docks an app into any room you
  pick, straight from the Mini Apps screen.
- Fifteen built-in apps: **Room Peek** (room info + recent messages + send),
  **Roll Call** (dice roller that can post its roll), **Room Info**,
  **Room Members**, **Pinned Messages**, **Room Threads**, **Search**
  (messages across one room or many), **Watcher** (keyword rules that notify
  and can auto-reply), **Who's Here** (typing and read positions), **Room
  Tools** (favorite, low priority, unread, pins, links), **Room Stats** (who
  posts when), **Spaces** (explore and join), **Inbox** (invites and unread
  rooms), **Account** (you, this device, look up a user), and **Inspector**
  (what Robrix tells an app about its pane, settings and device).

## Crates

| Crate | What it holds |
|---|---|
| `a2app/core` | Manifests, registry, `.splashapp` bundles, version history, the permission model + store, the host-service broker (platform services via `robius-*` crates), request-budget abuse control, persistence. |
| `a2app/agent` | The ACP client (JSON-RPC over stdio), the generation/repair pipeline, the Splash dialect guide, create-vs-modify intent classification, provider/model selection and key management, plus an optional in-process octos backend. |

Robrix-side UI and glue live in `src/a2app/` (feature-gated), with invisible
stub widgets in `src/a2app_dummy/` so non-`a2app` builds still resolve the
shared DSL names.

## Agent setup (once)

1. Install the octos CLI: `cargo install --git https://github.com/octos-org/octos octos-cli`
2. Give it an LLM provider, whichever is least effort:
   - the **AI Providers** page in the Mini Apps screen (pick a provider, paste a key),
   - a key already exported in your shell (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, …),
   - `octos auth login -p <provider>`,
   - or a local [Ollama](https://ollama.com) with a code model pulled — detected
     automatically, no key needed.

Any other ACP agent works via `ROBRIX_AGENT_CMD`, e.g. a Claude Code
subscription: `ROBRIX_AGENT_CMD="claude-code-acp" cargo run --features a2app`
(stdio composes, so `ROBRIX_AGENT_CMD="ssh myserver octos acp"` works too).
`ROBRIX_AGENT_MODEL=<model>` is forwarded to that agent as `ANTHROPIC_MODEL`
(the Claude-based agents read it); use `opus` when testing generation.

## Makepad pin

Mini-app hosting needs three fixes to Makepad's Splash isolate host that are
not upstream yet: inserted subtrees staying reachable across a widget-tree
refresh, the script GC surviving a foreign-heap value, and panic containment
at every isolate entry point (without which a mini-app can lose its renders
or take Robrix down with it). They are up as makepad/makepad#1208, and the
`makepad-widgets` pin points at the branch behind that PR
(`kevinaboos/makepad` `splash_host_fixes`, which is Makepad `dev` plus those
three commits). Move back to Robrix's shared pin once the PR lands.

## Extra cargo features

| Feature | Effect |
|---|---|
| `a2app-embedded-agent` | Link the octos agent **in-process** instead of spawning `octos acp` (required on iOS, where `exec()` is prohibited). |
| `a2app-persistent-guide` | Install the Splash dialect guide on the agent once, so per-turn prompts shrink to a pointer line. |
| `a2app-research` | Let the agent research with its tools (web search/fetch) before generating, baking found data into the app as constants. |

## Not included yet

- Splash resource limiting (CPU/memory/timer shares) — needs makepad#1189.
- Agent option knobs (model/effort/thinking controls); env vars
  (`ANTHROPIC_MODEL`, `CLAUDE_CODE_EFFORT_LEVEL`, `MAX_THINKING_TOKENS`) still
  seed the defaults.
- Live mini-apps embedded directly inside timeline items (shared apps render
  as install/run cards instead).

[Splash]: https://github.com/makepad/makepad

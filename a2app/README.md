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
- A **Mini Apps** button in room and space action bars opens a searchable
  picker of apps relevant to that context. Room apps run in the room's pane;
  space apps open in a modal attached to that space. A footer links to the
  main Mini Apps screen to see all apps or generate new ones.
- A **create bar**: describe an app ("a pomodoro timer") and the
  agent writes it in the Splash dialect, validated with the real parser and
  auto-repaired for up to two turns. Generation uses the guarded model
  transport described below in both the confined and embedded agent modes.
- **Per-app isolation**: each app runs in its own Splash isolate with nothing
  by default: no filesystem beyond its private jail, no network, no host access.
  Capabilities are declared in the app's manifest, prompted at first use
  (Allow selected / Allow Once / Block everywhere / Not Now), revocable at any time, and a
  request-flooding app gets stopped and restricted.
- **Matrix services**, each behind its own permission group: the attached
  room (info, messages, older history, one event, threads and replies,
  members, pins, search, read receipts, unread counts, power levels,
  permalinks, upgrades), rooms and spaces (list, search, invites, previews,
  cross-room read and search, space trees), the account (profile, device,
  homeserver, ignored users, other users' profiles, DM lookup), and, behind
  the room and space write rules, sends, replies, reactions, typing, read receipts, pins,
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
- **Room protection is above app permissions**: the Mini Apps screen has
  separate read/write defaults and room/space allow/block lists. Writes are
  blocked by default; a block always wins over an allowance.
- **Version history**: every AI change, hand edit (there is a source editor),
  and version switch is kept; switch back and forth freely, diff any version
  against the current source, and reset a built-in to stock.
- **Sharing**: export any app as a `.splashapp` bundle (file + clipboard),
  hand it to another app through the system share sheet, send it into a room
  as a file attachment with a pre-filled caption ("Send to room…"), or post
  it with `/miniapp share <name>`, where it renders as a card other Robrix
  users can install and run. "Open in room…" docks an app into any room you
  pick, straight from the Mini Apps screen.
- Built-in apps: **Public Web** (a fixed public example.com fetch),
  **Room Peek** (room info + recent messages + send),
  **Roll Call** (dice roller that can post its roll), **Room Info**,
  **Room Members**, **Pinned Messages**, **Room Threads**, **Search**
  (messages across one room or many), **Watcher** (keyword rules that notify
  and can auto-reply), **Who's Here** (typing and read positions), **Room
  Tools** (favorite, low priority, unread, pins, links), **Room Stats** (who
  posts when), **Spaces** (explore and join), **Inbox** (invites and unread
  rooms), **Account** (you, this device, look up a user), and **Inspector**
  (what Robrix tells an app about its pane, settings and device). Background
  examples add **Website Watch**, **Reminder**, and **Keyword Alert**.
- **Background tasks**: explicitly enable an interval, one-time alarm, or live
  room-message condition. Tasks retain their original room/space and saved app
  state across launches, while all permissions and IFC checks remain active.
  See [Background mini-apps](BACKGROUND_TASKS.md) for setup, examples, and recovery
  semantics. Tasks run only while Robrix is running and signed in.

## Crates

| Crate | What it holds |
|---|---|
| `a2app/core` | Manifests, registry, `.splashapp` bundles, version history, the permission model + store, the host-service broker (platform services via `robius-*` crates), request-budget abuse control, persistence. |
| `a2app/agent` | The ACP client (JSON-RPC over stdio), the generation/repair pipeline, the Splash dialect guide, create-vs-modify intent classification, provider/model selection and key management, plus an optional in-process octos backend. |

Robrix-side UI and glue live in `src/a2app/` (feature-gated), with invisible
stub widgets in `src/a2app_dummy/` so non-`a2app` builds still resolve the
shared DSL names.

## Agent setup (once)

Robrix's AI rooms and app generation use a confined Octos process by default
on macOS and supported Linux systems. The workspace and lockfile pin upstream
Octos to `bf63797a11b3949f4a726267a91d22bf977e7c01`, which includes
[Octos PR #2443](https://github.com/octos-org/octos/pull/2443) and the
[SQLite compatibility update #2455](https://github.com/octos-org/octos/pull/2455)
needed to embed Octos alongside the Matrix SDK. Install that same revision; the `api` feature also supports ordinary ACP integrations:

```sh
cargo install --git https://github.com/octos-org/octos --rev bf63797a11b3949f4a726267a91d22bf977e7c01 --locked --no-default-features --features api octos-cli
```

Linux requires bubblewrap 0.8 or later, enabled unprivileged namespaces,
seccomp, and fully enforced Landlock ABI 3 (Linux 6.2 or later). The worker's
ELF dependencies must be immutable system libraries. See upstream's
[`octos-sandbox` documentation](https://github.com/octos-org/octos/blob/bf63797a11b3949f4a726267a91d22bf977e7c01/crates/octos-sandbox/HOST_MANAGED.md)
for the complete requirements. Self-hosted test machines need these prerequisites
provisioned by their administrator; CI's AppArmor adjustment is for disposable
hosted runners only.
Missing confinement support stops startup; it never falls back to an ordinary
ACP process. Alternatively, `a2app-embedded-agent` links Octos into Robrix and
needs no CLI installation. Use the embedded feature on iOS and platforms
without a supported process sandbox. Embedded mode uses the same guarded
model/tool transport but has no separate OS process compartment.

Configure an OpenAI-compatible or
Anthropic provider through **AI Providers**, an existing Octos configuration,
or that provider's supported environment/auth-store credentials. Supported
provider IDs are `openai`, `anthropic`, `deepseek`, `moonshot`,
`moonshot-coding`, `groq`, `openrouter`, `ollama`, and `custom`; other backends
are refused. A local Ollama endpoint can use an already installed model
without an API key.

Linux `keychain:` references use Octos's existing `~/.octos/secrets` store,
including credentials scoped to a profile. Credentials remain in Robrix's
host process and are never supplied to the confined worker.

Run with `cargo run --features a2app` (or `a2app-embedded-agent`), then use **Manage data
sharing rules** to allow the configured model recipient for the sources the
agent or generator needs. A local endpoint also needs explicit source consent;
Robrix cannot guarantee that the service itself will not forward data.

`ROBRIX_AGENT_CMD` can select a local compatible Octos executable (optionally
followed by `acp`); Robrix still applies confinement and requires host-broker
negotiation before sending a prompt. Shell commands, SSH workers and ordinary
ACP adapters cannot provide this protected mode. Unset the override to use the
embedded backend when compiled in. Select the model in AI Providers or the
Octos configuration; the child receives no credentials or provider endpoint.
The standalone ACP client remains available for legacy integrations and tests.

Install a trusted worker in a location where untrusted code cannot replace
the executable or its parent directories. Resolving its path does not
authenticate the binary or prevent replacement between selection and launch.
Robrix's parent-enforced launcher, not the child's `confined` protocol flag,
establishes the process boundary.

On macOS, necessary system-runtime reads remain allowed and the Seatbelt
profile imports Apple's `dyld-support.sb`. Its grants can change with macOS;
re-run the native confinement probes when qualifying a new OS release.
Protocol budgets, timeouts and finite turn limits do not impose hard process
CPU or resident-memory caps, and do not eliminate timing/resource side channels.

## Makepad pin

The workspace dependencies in `Cargo.toml` use `kevinaboos/makepad`'s
`splash-host-io-cancel` branch for both widgets and the code editor.
`Cargo.lock` currently resolves it to
`a8a210f20822936d502727a8295fb06217609a6b`. The pin includes the Splash host
I/O boundary, isolated validation, and cancellation integration required by
this feature. The host I/O changes are tracked in
[Makepad PR #1243](https://github.com/makepad/makepad/pull/1243).
Keep the workspace dependency entries and lockfile aligned when updating it.

## Extra cargo features

| Feature | Effect |
|---|---|
| `a2app-embedded-agent` | Link Octos **in-process**, using the same guarded model transport as the confined child. Also avoids subprocess execution on iOS. |
| `a2app-persistent-guide` | Install the Splash dialect guide on the agent once, so per-turn prompts shrink to a pointer line. |
| `a2app-research` | Legacy pipeline research support. Guarded Robrix generation disables research tools; this feature does not bypass that restriction. |

## AI Rooms (agent chat sessions per Matrix room)

An AI Room is an ordinary Matrix room backed by a Robrix agent session — the
room *is* the session's transcript. Messages any member sends drive the agent;
its output is written back as `rs.robius.robrix.ai_reply` **state events** (never
`m.room.message`, so it can never loop back as input), rendered as timeline
cards, and it survives restarts. The code lives in `src/a2app/ai/` (room
creation + marker/cursor bookkeeping: `rooms.rs`), `src/a2app/runtime.rs`
(session attach, message forwarding, reply posting) and
`src/a2app/ai_room_events.rs` (event wire types + the timeline card). Unix-only:
a session is an `a2app_agent::AgentTransport`, with a tool registry owned by
Robrix for that room. Confined children access it directly through the ACP
broker; the embedded backend uses the session-scoped MCP server
(`src/a2app/ai/server.rs` + `bridge.rs`).

### How it works

1. **Create** (Add Room screen → "start a new AI room"): a private encrypted
   room whose `initial_state` carries a `rs.robius.robrix.ai_room` marker, so
   there is no window where the room exists but isn't an AI room.
2. **Attach**: opening a room checks the marker once (after its state has
   synced — a brand-new room's state lags sliding sync, so a marker miss only
   counts once `m.room.create` is cached; our own creations are recorded
   immediately from the create response). A marked room gets its session.
3. **Forward**: member text messages after the saved forwarding cursor (room
   account data `rs.robius.robrix.ai_session_data`) are sent to the session as
   prompts; the first prompt after a (re)start carries a short plaintext
   transcript preamble for context.
4. **Answer**: the agent calls Robrix's own MCP tools — `send_message` (posts
   an `ai_reply`), `launch_splash_app` (runs the mini-app generation pipeline),
   or `list_apps`/`launch_app` (find and run an app that already exists) — or
   ends its turn with text, which is posted as the `ai_reply`.
   A turn that already spoke through `send_message` has its redundant trailing
   text dropped, so one turn = one card.

### Tools the agent can call

The session registers these in its host tool registry (`ai::tools::
register_session_tools`), and every one is executed by Robrix. The
gated ones map onto the mini-app capability catalog, so the first use prompts
the user and the choice is shared with mini-apps. Message reads return full
bodies; only the mini-app services clip.

| Tool | What it does | Gate |
|---|---|---|
| `send_message` | Replies in this room (becomes the turn's `ai_reply`) | none (room plumbing) |
| `read_room_memory` | Recalls the agent's own past turns and tool calls in this room | none |
| `read_room_messages` | Reads this room's recent messages | `matrix.room.messages.read` |
| `read_older_messages` | Pages further back in this room | `matrix.room.messages.paginate` |
| `room_info` | Reads this room's name, topic, members, join rule, encryption | `matrix.room.info.read` |
| `list_rooms` | Lists the user's joined rooms and DMs | `matrix.rooms.list` |
| `read_other_room_messages` | Reads another joined room the model names | `matrix.rooms.messages.read` |
| `post_room_message` | Posts a notice into another joined room | `matrix.rooms.message.send` (per room) |
| `list_spaces` | Lists the spaces the user has joined | `matrix.spaces.list` |
| `space_info` | Reads one space's details | `matrix.space.info.read` |
| `list_space_rooms` | Lists the rooms/subspaces inside one space | `matrix.space.rooms.list` |
| `list_apps` | Lists the mini-apps installed and available in this room (id, name, description, scope, running) | `app-launch` |
| `launch_app` | Runs an already-installed mini-app in this room, by id from `list_apps` | `app-launch` (run only) |
| `list_mini_app_tools` | Lists the tools the room's mini-apps registered (id, name, description, args) | `mcp-tools` (kill switch) |
| `call_mini_app_tool` | Calls one registered mini-app tool by id, forwarding `arguments` | `mcp-tools` (per tool) |
| `launch_splash_app` | Builds and runs a NEW mini-app from a description | `apps.generate` |

`launch_splash_app` is create-only: it never rewrites an installed app. Running
an app that already exists is `launch_app`'s job — list the ids with
`list_apps`, then launch one. (The Mini Apps screen's own create bar still
classifies create-vs-modify from its text; only the agent's tool is create-only.)

Each call is its own `rs.robius.robrix.ai_tool_call` state row, written
`Started` when the model picks the tool and rewritten `Done` with the outcome.
The row (and the reply card's receipt chips) render a human phrase rather than
the raw tool name — e.g. `Read messages in “General”`, `Built and ran a
mini-app “a pomodoro timer”`.

**A mini-app can register its own tools at runtime.** With the `mcp-tools`
permission, an app calls `host.request("mcp.tools.register", {name,
description, args})`; Robrix installs a `MiniAppTool` in the room session's
live registry. Confined Octos refreshes its tool definitions between turns,
preserving conversation history. Embedded MCP connections receive
`notifications/tools/list_changed` for clients that support it.
When the model calls the tool, the runtime delivers
`on_tool_call({call_id, tool, name, arguments})` into the own isolate; the app
answers `host.request("mcp.tools.result", {call_id, ok, result})` and that
text is the model's tool result (a bounded ~20 s wait, then the model is told
it timed out). Tools are namespaced `app_<id>_<name>` so an app can never
shadow a built-in, and an instance's tools are withdrawn when it quits or its
session stops.

**The stable bridge also supports embedded clients.** Octos's embedded MCP
client discovers a server's tools once, at session start, and does not act on
`notifications/tools/list_changed` (or re-list afterwards), so a tool added to
the live server cannot enter that client's toolset on its own. Every session
also advertises two tools that are present from the start:
`list_mini_app_tools` (the registered tools — id, name, description, args) and
`call_mini_app_tool` (`{tool, arguments}`). The model lists with the first and
calls through the second, and the runtime validates the id, applies the same
per-tool gate as a direct call, and routes into the owning isolate. The
per-tool `MiniAppTool` registrations remain, so a client whose MCP stack does
honour `list_changed` can still call them directly.

Two consent gates guard this, because the tool's description and result are
app-authored text that lands in the model's context. Registration prompts per
tool, showing the **full description and argument list verbatim** (the exact
text the model will read); a changed description hashes differently and
re-prompts. Invocation prompts per tool on first use, showing the same review
text plus the concrete arguments. Durable registration grants store the
content hash; refusals are session-scoped. See `a2app/core/src/permissions.rs`
(`tool_effective`) and `src/a2app/ai/tools.rs` (`MiniAppTool`).

**Web fetching goes through Robrix.** Room sessions use Robrix's host-owned
`web_fetch` MCP tool; the octos native allowlist is empty. Native browsing,
search, shell, files, memory and spawned agents cannot bypass the host's
permission and information-flow checks. Redirects are refused; fetching a
redirect destination requires a separate approved request.

### Permission scopes and room protection

Consent can cover all rooms or selected rooms and spaces, for the current
room session, until Robrix closes, or persistently. A room session ends when
that room is closed; restarting a mini-app isolate does not end a Robrix
session grant. Session allowances never reach disk. Allow Once authorizes
only the pending request, including its asynchronous completion. Subscriptions
and requests to enable a permission use a selected duration instead.

Global, room and ancestor-space blocks always override allowlists and app
grants. Read and write are independent. The **Enable room writes for mini-apps and agents** master switch
pauses all room writes and disables individual write editors, retaining their
saved rules and the previous write default for when the switch is enabled again.
Read and write defaults each support **Only allowlisted rooms and spaces**:
selected room/space allowances work, unlisted rooms are denied, and explicit
blocks still win. The ordinary Ask default instead prompts for unlisted rooms.
Allowlists skip prompts for declared capabilities, while
explicit per-app denials and restrictions still apply. Collection queries
filter each actual room, and unresolved space ancestry fails closed when a
space block might apply. Configured spaces and their nested subspaces must
remain joined so sync events can invalidate membership changes; inaccessible
or unjoined branches keep protection unresolved and visible in the editor.

Internet consent supports an exact URL, an origin (scheme/host/port),
an exact hostname, a hostname with subdomains, or all HTTP(S) sites. URL
matching uses parsed URLs and DNS label boundaries. Mini-apps use the
host's `network.http` service; raw Splash networking remains disabled.

### Private data and information flow

**Reading private data restricts all later output from that context.** The
host joins account and room sources before delivering data, and checks that
every source permits the actual recipient before output. Labels only grow;
encoding, paraphrasing, model summaries, IPC and app-tool calls cannot remove
them. An empty label is public. A room source permits return to that same
room in the same account by default; other recipients require explicit
source sharing rules. Account data requires its own rule even when the room
already allows a recipient.

Matrix room permission does not implicitly permit plaintext disclosure to
its homeserver. App-selected API fields (such as search strings, event IDs,
invite targets and state metadata) require a separate allowance for the
actual homeserver origin before a request is sent, even in encrypted rooms.
Cached event reads stay local; ordinary messages use the destination room's
normal encryption and sharing checks.

Mini Apps → **Manage data sharing rules** manages source-to-recipient allowances
for a configured model service, an exact HTTP(S) origin, or a Matrix room.
Choose one app in the current account, one exact app/room or agent context,
or explicitly all readers. Rules can last for a room session, until Robrix
closes, or persistently. Session rules stay in memory; closing the selected
room or signing out expires them. These checks are additional to ordinary
capability/URL permissions. Older source-wide rules retain their original
scope until removed. Changing a rule applies to existing contexts; revocation cannot recall
data already transmitted. Blocking a room's read access prevents new reads;
it does not erase data, labels or agent history already retained. To stop
future sharing of previously read data, remove its source sharing rules too.
There is no selected-payload release, label reset or automatic declassification.

The design follows the established floating-label approach described by
[LIO's authors](https://www.scs.stanford.edu/~deian/pubs/stefan%3A2011%3Aflexible.pdf)
and source/reader policy composition in [Jif](https://www.cs.cornell.edu/jif/doc/jif-3.0.0/overview.html).
Robrix enforces labels dynamically at host boundaries, rather than adding a
static type system to Splash or relying on model instructions or secret
scanners. This is an implementation of those design principles, not a claim
that their formal proofs establish the security of Robrix.

**The host is the only route to external effects.** Makepad's opt-in
`host_io_only` mode disables direct native networking, shared IPC and native
exports in mini-app isolates, including nested isolates. Robrix's broker
mediates network, Matrix operations, clipboard/export, app IPC and tool
arguments/results. Uncontrolled exports and navigation are refused once
private data is present; source restrictions transfer across app/agent
boundaries before delivery. Normal app navigation transfers provenance to
the opened app. Each account/app/room combination has a separate filesystem
jail and persistent provenance. An app's source code and version history are
still shared, so their provenance remains an inherited floor for every
instance. Generating or editing private source cannot be laundered by opening
it in another compartment.

`information_flow.json`, outside app jails, records app/agent provenance,
integrity influences, clearance bounds and permanent sharing rules. The host writes and syncs a replacement atomically before
delivering newly labelled input. Write failures and corrupt/unsupported
metadata block access instead of resetting labels. Old nonempty app storage
or old private source with no recorded provenance receives `UnknownPrivate`,
which cannot be released by a sharing rule. Closing, clearing a sandbox,
restarting or reinstalling the same app does not erase its retained label.
Migration preserves older app-wide labels as a conservative inherited floor.
Old `app_data/<app>` files remain in their original directory; they are not
automatically mounted in new compartments. App Info explains this and counts
them in storage usage. Clear data removes both old files and new compartment
files, while retaining provenance.

**Public instances have an immutable empty clearance.** App Info → **Open
public instance** creates separate storage and refuses room/account reads,
private source code and private IPC input. Native text/paste/drop input is
private account input, so it is also refused. The stock **Public Web** app
provides a fixed public example.com fetch without text input. Ordinary internet
consent still applies. Public workers can deliver public results through
`ipc.post`; its fixed acknowledgement reveals no receiver existence or
delivery status. `ipc.send` keeps its delivery receipt and therefore requires
account read clearance. All delivered messages transfer both label dimensions
before the receiver runs. Broader clearances are host-owned upper bounds on
which sources a compartment may read; lowering a bound never removes taint.

**Untrusted influence is separate from confidentiality.** Room content,
internet/model responses, pasted/imported content and app/tool messages carry
persistent integrity provenance. Sensitive operations require an additional
user-issued authority for the exact context, operation and target after
untrusted influence is present. Examples include room writes, app launches,
tool invocation and non-GET/HEAD HTTP methods (scoped to that method and origin).
The trusted sharing screen shows blocked operations, their influences, and
room-session or Robrix-session approval options. It also shows accumulated
private sources, actual recipients, and all sources blocking a disclosure,
with direct access to the relevant sharing rule.

Action approval captures the reviewed influence set and live activation.
**This exact action once** also captures the complete host-owned request
contents. The trusted review shows those contents; approval permits one unchanged
retry and is consumed immediately before the effect starts. It does not replay
the request automatically. Changed contents, a different target, cancellation,
revocation or a new activation cannot reuse that approval. Explicit room-session
and Robrix-session choices remain available for repeated operations of the same
kind to the same target, including different contents. These approvals authorize
actions; source-to-recipient sharing rules still independently protect private data.
New influence, a closed context, a closed room session or explicit revocation
can invalidate it. No app can endorse its own content or reset these labels.
This constrains actions influenced by prompt injection; it does not classify
instructions as malicious or make remote content trustworthy. Diagnostic
history is bounded, local and metadata-only; request/response bodies are not
recorded there. `protection_history.jsonl` retains recent timestamped policy
checks and actual request attempts/outcomes across restarts. An allowed policy
check does not imply transmission; a failed or interrupted request may already
have transmitted data. Exact-action review contents stay in memory and are never
written to this history.

Mini Apps → **Inspect room protection** explains the effective read/write and
capability controls for a selected room and app/agent. It identifies global,
room, ancestor-space and per-app restrictions and links to the responsible
settings. Retained-source inspection includes closed contexts and generated app
source that has never been launched, so stopping an app does not imply that its
stored data or shared code lost provenance. Refresh reloads this snapshot;
selecting a target or ability rechecks its current permissions. The data-sharing screen
explains missing source/recipient/reader allowances, unknown-source restrictions,
changed model recipients and action approvals, with explicit steps to change the
controls that can be changed.

Every implemented broker service and incoming hook has an explicit source,
destination and effect contract in `capabilities/flow.rs`. Unclassified
services/hooks fail closed. Persistent context identities are distinct from
live activation epochs: queued Matrix work, model requests and response
delivery cannot regain authority when the same context is reopened.

The headless adversarial tests execute actual Splash code through the host
broker, including encoding, IPC, storage/restart, public workers, native I/O
denial and queued revocation. Socket tests exercise real HTTP/model transports
with controlled loopback peers. These tests do not replace live Matrix,
provider or device end-to-end verification.

The **Mini Apps security** workflow runs core policy and adversarial Splash tests,
both agent modes, application policy/transport/widget tests, and the embedded
application check on Linux and macOS. It builds the exact Octos revision from
Robrix's lockfile and runs native confinement probes plus a real Robrix-to-worker
model/tool/cancellation round trip. Changes under `a2app/` trigger this workflow.

**Cloud inference is also an external disclosure.** Protected room agents
and generators use the host-controlled model transport in both agent modes.
Each model request checks the current label, including tool feedback and
compaction. A confined Octos child requests completions and host tools over its
ACP connection; direct network, private files, subprocess tools and persistent
history are unavailable to it. Each connection belongs to one activation, and
explicitly stopping a room agent retires that activation and its queued work.
The next prompt starts a fresh session, preserving durable IFC labels.
Unmediated ACP backends and provider fallback cannot receive a protected
context. Model recipients identify the configured endpoint, model
and a private credential fingerprint, so an allowance cannot silently move
to a different service or account. A loopback endpoint still needs consent:
the service may itself forward the data elsewhere.

The initial guarded model transport supports text/tool requests to
OpenAI-compatible and Anthropic APIs. It currently delivers the completed
response rather than streaming tokens and refuses media inputs. Protected
app generation runs without tools; it cannot use external research, shell or
filesystem tools during generation.

The stateless HTTP service checks source rules before DNS, immediately before
the request, and before returning response data. It rejects ambient
credentials, redirects, private/local addresses and unbounded payloads, pins
the checked DNS addresses, and uses no shared cookie jar or automatic proxy.
This boundary protects against explicit data flows through the mediated
interfaces. It does not defend against a compromised Robrix/OS/VM, malicious
changes to host-owned metadata, timing/resource side channels, or a recipient
misusing information after the user permits disclosure. New host services
must identify private inputs and output recipients before being exposed.

### Registering AI tools from an app

A mini-app attached to an AI room can expose callable tools to the agent (the
inverse of `launch_splash_app`: the model calls INTO the app). This is what
makes a two-way app possible — a tic-tac-toe board the AI plays on, a
scorekeeper it updates, etc.

The app side (all on one group, `mcp-tools`):

- `mcp.tools.register` `{name, description, args:[{name,type,description}]}`
  -> `{tool}`; `mcp.tools.unregister` `{name}`.
- `fn on_tool_call(json)` with `{call_id, tool, name, arguments}`; answer
  `mcp.tools.result` `{call_id, ok, result}`.
- `mcp.tools.result` is ungated plumbing; registration and invocation are
  prompted per tool. The guide (`a2app/agent/src/splash_guide.md`, section
  "Letting the AI call your app") is the generator-facing reference.

### Offline and keyless testing

Existing mini-apps can run without a model or API key. For keyless AI testing,
use either agent mode with a running local OpenAI-compatible service and
an already installed model. For example, these fields in the selected Octos
configuration use Ollama's compatible API:

```json
{
  "provider": "ollama",
  "base_url": "http://127.0.0.1:11434/v1",
  "model": "your-installed-model",
  "api_type": "openai"
}
```

Replace the model name with one installed in that service, and select the
same provider/model in AI Providers if a saved selection overrides the config.
For a keyless compatible service, use `ollama` or `custom` and omit
`api_key_env`; selecting a provider that requires a key still requires its
credential even at a loopback URL. HTTP model endpoints must use a literal
loopback address; other endpoints require HTTPS.
Source sharing rules still apply, including for loopback. A local address
identifies the recipient, not a guarantee that its processing stays offline.

The deterministic Octos `scenario` provider and
`a2app/dev/offline-ai-setup.sh` belong to the earlier ACP demo. They are **not
supported by the guarded Robrix transport** and no longer provide a working
AI-room demo. The helper sets `provider: "scenario"` and a placeholder key;
if previously used, replace that provider configuration and remove its
`sk-dummy-testing` placeholder before testing a supported service. The
headless transport and broker tests provide deterministic testing without
live provider credentials.

### Notes for maintainers

- `cargo test -p a2app-agent --lib` covers broker limits, cancellation,
  protocol validation and guarded HTTP transport. Run it with `--features
  embedded` to cover both backends. With a compatible Octos binary built,
  `ROBRIX_TEST_OCTOS=/absolute/path/to/octos cargo test -p a2app-agent --lib
  confined_octos_uses_parent_model_tools_and_cancellation -- --ignored`
  exercises a real confined child with synthetic model and tool responses.
- The `robrix --mcp-bridge` relay + tool server are exercised headlessly by
  `tests/mcp_transport.rs` (hand-written client) and `tests/mcp_rmcp.rs` (the
  real rmcp client octos uses). `mcp_rmcp` is the regression test for a
  macOS-only bug where `accept()` inherits the listener's `O_NONBLOCK` and the
  blocking serve loop read EAGAIN between requests, dropping the connection
  and silently removing the host tools.
- `glass.*` widgets and `height: Fill` roots do not render in a mini-app host
  today; built-in demo apps (`a2app/core/apps/*.splash`) show the working
  idioms (natural sizes throughout).

## Not included yet

- Splash resource limiting (CPU/memory/timer shares) — needs makepad#1189.
- Agent option knobs (model/effort/thinking controls); env vars
  (`ANTHROPIC_MODEL`, `CLAUDE_CODE_EFFORT_LEVEL`, `MAX_THINKING_TOKENS`) still
  seed the defaults.
- Live mini-apps embedded directly inside timeline items (shared apps render
  as install/run cards instead).

[Splash]: https://github.com/makepad/makepad

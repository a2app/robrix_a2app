# Default mini-app permission audit

This records the source audit of all 20 shipped mini-apps and the deterministic
coverage in `core/tests/splash_information_flow.rs`. The coverage exercises the
real Splash VM, host bridge and broker with fixture Matrix/HTTP responses. It
does not replace a Robrix build, native permission-popup checks, a homeserver,
or scheduled-worker/device testing. This audit uses focused deterministic
fixture checks and an isolated native permission preview; full Robrix,
homeserver and device acceptance checks remain with the user.

Startup panes ask for their required read and live-update groups together.
Already allowed groups need no new prompt. Partial approval runs only selected
groups and each exact allowed ability. An unchecked feature remains off. A
denied setup finishes, shows a useful fallback, and provides Refresh to ask
again. Read/subscription flags prevent an unrelated permission change from
loading everything again; revoked hooks and reads stop, and reapproval resumes
them. Outgoing actions still pass the host's concrete request and information
flow checks.

Single-step actions request their concrete service, retaining **One time**
approval. Recurring setup offers **Until this room closes** when there is an open
origin room, **Until you quit Robrix**, or **Forever**. The room-close duration
belongs to the room the app was opened from, independently of its allowed target
rooms or websites. Detached apps and scheduled reviews without an open-room
anchor omit that option. A room-close grant cannot keep saved jobs authorized
after the room closes. Foreground Test controls prepare scheduled work; hidden
background callbacks never open prompts. Saving configuration, a draft or a
Watcher rule does not authorize or execute sending.

| Mini-app | Permission entry and continuation | Denial/retry and later effects |
| --- | --- | --- |
| Public Web | Fetch requests only the fixed `https://example.com/` URL. | A denied fetch ends; Fetch can be clicked again. No room/account reads. |
| Website Watch | Test requests the saved concrete URL, then the selected popup or room report. | Each failed step finishes Test; Test retries. A scheduled callback acknowledges success/failure and avoids duplicate match reports. |
| Reminder | Test directly requests its saved reminder popup. | Failed Test releases its pending flag; Test retries. Hidden runs acknowledge completion and cannot prompt. |
| Keyword Alert | Test sets up missing ongoing room-message watching, then requests its concrete test popup. | Rejected setup ends Test. Hidden unmatched/own-message batches complete without a notification request. |
| Room Peek | Startup groups room details, recent-message reads and live room updates. | Refresh retries selected reads/hooks. Draft message and Attach file use composer grants; Send now and Send file now require independent sending grants, including media upload for files. Navigation is requested only when used. |
| Roll Call | Roll needs no host permission; Draft result and Post to room separately request the captured dice result. | A rejected draft or post releases its pending flag and retains a useful result; its button retries. |
| Room Info | One room-details read on opening. | Refresh repeats that concrete read. Permission changes refresh only an allowed unfinished read. |
| Room Members | Startup groups member reads and membership updates. | Refresh retries. Opening a member requests only the concrete profile navigation. |
| Pinned Messages | Startup groups pinned-message reads and pin-change updates. | Refresh retries. Jumping to a pin requests the captured message navigation. |
| Room Threads | Startup groups thread reads and message updates. | Refresh works in the thread list and selected thread. Stale callbacks cannot replace a later selection; Open in Robrix reviews the current thread. |
| Search | Search requests the selected current/all/picked-room search. Pick rooms separately loads the picker. | Failure ends the active search; Search retries. Clicking a result requests that message and room. Permission changes never repeat a refused picker load. |
| Simple Watcher | Opening subscribes to new messages; Test sets up missing watching, then requests the tested rule's popup/reply effects. | Saved rules and Draft reply do not enable sending. Refresh retries watching; Test resumes paused automatic actions after approval. Refusal stops the queued burst, and own messages cannot trigger reply loops. |
| Watcher | Opening subscribes to new messages; Test sets up missing watching, then queues only the tested rule's concrete popup/reply effects together for one combined review. | Adding a rule or drafting its reply only prepares it. Refresh retries watching. Test resumes paused automatic actions after approval; a denied action stops the queued burst until a new explicit Test. Own messages cannot trigger reply loops. |
| Who's Here | Startup groups read-position/event reads and typing/receipt updates. | Refresh retries. Event bodies are fetched serially; tapping a person/message uses its concrete navigation. |
| Room Tools | Startup groups recent/pinned messages and room flags/links/details. | Refresh retries permitted parts. Flags, pinning, copying and upgraded-room navigation retain separate concrete effects and captured contents. |
| Spaces | Opening loads the attached space, or the joined-space list. | Refresh retries denied reads and shows their error. New space selections reject stale replies; preview, join and room navigation request only the chosen target. |
| Inbox | Opening loads invites/unread rooms and their collection updates. | Refresh retries reads/hooks. Accept/decline captures the selected invite and finishes after its result; opening unread rooms uses the selected room. |
| Room Stats | Startup groups message/history reads and power-level details. | Refresh retries; rejected reads finish counting with a useful fallback. The selected sender's profile opens only on tap. |
| Account | Startup groups the own profile and device/homeserver/ignore-list reads. | Refresh retries permitted parts. User lookup, existing-DM lookup/navigation and account-management browser opening remain concrete actions. |
| Inspector | Startup groups display settings, navigation observation and device facts. | Refresh retries selected parts. Own pane/environment reads remain available; moving/minimizing/restoring the pane is requested only on its controls. |

The all-app primary-path coverage checks finite declared permissions, actual
service completion, parked callback completion, outgoing reviews, subscription
deduplication, and unrelated-grant idempotence in ordinary and strict modes.
Dedicated grouped-startup coverage checks full denial, explicit Refresh,
reapproval, revocation, and partial selections for Account, Inspector, Who's
Here, Room Members, Pinned Messages, Room Threads, Room Peek, Room Stats and
Room Tools. Other focused cases cover scheduled work without hidden prompts,
Watcher configuration versus Test, exact navigation targets, stale selections,
independent composer/send permission, write-switch enablement, captured clipboard/browser content, and malformed
saved website URLs. Runtime tests separately cover trusted input, policy scopes,
refusal clearing, persistence and the actual modal controls.

Keyword Alert can require two first-time decisions: ongoing message watching
needs a room-session, Robrix-session or lasting approval, then the captured notification may be
approved once or for longer. Existing watch approval skips its setup decision.
Watcher likewise skips already approved watching and presents its selected
concrete notification and reply together; it does not authorize all future rule
actions before showing their contents. Spaces' remote room-list lookup can also
need a source-sharing review after reading private space details, separately
from opening the selected room. Those are finite checks for different effects,
not automatic retry loops.

The [per-app duration audit](core/apps/permissions-audit.md#grant-duration-and-expiry-audit)
records each app's attachment requirements, one-shot or ongoing work, and expiry
recovery. Closing a room expires grants owned by that origin, including a grant
whose target scope is **All rooms**. The target scope still controls which rooms
may be read or changed; duration never widens it. **Until you quit Robrix** is the
application session, not the lifetime of one mini-app isolate. Restarting an app
or restoring a hidden worker does not extend either session duration. Session
grants stay in memory; only **Forever** persists across Robrix restarts.
The room session ends when its last tab or screen closes; keeping the app open
separately does not extend it.

Each default now restricts its declared groups to the exact service and hook
abilities used by its source. Stock manifest construction retains those parsed
capability lists. An untouched earlier default adopts the narrowed declaration
when its new stock version is reconciled; saved group allowances still cannot
authorize an ability absent from that declaration. Customized versions retain
their chosen source and receive the standard optional stock update.

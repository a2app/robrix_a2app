# Built-in drafting and sending audit

All 20 current built-ins were checked against their executable host requests,
permission headers, narrowed capabilities, and catalog permission explanations.
Declarations request permission; they do not grant it. Existing customized copies
keep their changes and receive the normal built-in update offer.

`robrix-composer` prepares local text or an attachment preview for the user to
review. Its permission and exact-action review are independent of sending. It
follows room and space access protection, grants no message-reading rights, and
does not require the room write switch or an outgoing data-sharing grant.
`matrix-room-send` posts a message immediately or permits saved automatic replies.
Room sending and media upload/posting remain separately checked operations.
Mini-app `matrix.send_media` uploads and posts native media only to the
attached room under both `matrix.media.upload` and `matrix.media.send`.
It shares the bounded inline byte format with local `composer.attach`, while
composer grants never authorize the upload or post.

| Built-in | Disposition |
| --- | --- |
| Public Web | Fetches and displays the fixed public example page. No composer or sending permission added. |
| Website Watch | Added **Draft test report** for local review. Test reports and scheduled room-message reports retain separate sending permission. Drafting does not check the website or enable scheduled sending. |
| Reminder | Saves reminder text and shows local notification popups. It does not compose room messages; no composer or sending permission added. |
| Keyword Alert | Reads matching message previews and shows notification popups. No room posting or drafts; no composer or sending permission added. |
| Room Peek | Added **Draft message** and **Attach file**, with **Send now** for text and **Send file now** for explicit immediate media posting. Enter prepares a draft. Each file control opens its own native picker; canceled or refused operations do not fall back to posting or staging. Attachments open Robrix's preview with the typed caption under composer grants. Direct file sending separately requires upload and media-send grants. Files are limited to 1 MiB. |
| Roll Call | Added **Draft result** alongside explicit **Post to room**. Drafting the generated dice result needs no sending permission. |
| Room Info | Displays room metadata. No composition or sending permission added. |
| Room Members | Displays membership and opens profiles. No composition or sending permission added. |
| Pinned Messages | Displays pins and navigates to messages. No composition or sending permission added. |
| Room Threads | Reads threads and opens the native thread view. No composition or sending permission added. |
| Search | Searches selected rooms and navigates to results. No composition or sending permission added. |
| Simple Watcher | Added **Draft reply** for typed or saved replies, without subscribing, testing, or sending. Existing automatic replies stay separately authorized. Test buttons explicitly say when they send a reply. |
| Watcher | Added **Draft reply** for typed or saved replies. Existing automatic replies stay separately authorized, and the AI rule tool remains notification-only. Test buttons explicitly say when they send a reply. |
| Who's Here | Displays typing/read activity and opens people or messages. No composition or sending permission added. |
| Room Tools | Existing explicit controls manage pins, room flags, and copy links. These room-management writes retain their existing permission and write policy. No composer or message-sending permission added. |
| Spaces | Explores spaces, previews rooms, and explicitly joins rooms. Existing membership permission stays separate. No composer or message-sending permission added. |
| Inbox | Lists invites/unreads, navigates, and explicitly answers invites. Existing membership permission stays separate. No composer or message-sending permission added. |
| Room Stats | Reads message statistics and permission information. No composition or sending permission added. |
| Account | Displays account/profile information and opens existing DMs or account management. No composition or message-sending permission added. |
| Inspector | Displays host/device diagnostics and controls its pane. No composition or sending permission added. |

The catalog tests enforce the exact set of composing built-ins, matching header
and catalog explanations, narrowed capabilities, and draft usability with sending
denied and room writes off. The real Splash parser checks every built-in source.

## Grant duration and expiry audit

All 20 built-ins use the host's selected duration; none chooses or stores a grant
duration in Splash. A concrete one-shot request can use **One time**. Permission
setup and ongoing subscriptions need **Until this room closes**, **Until you quit
Robrix**, or **Forever**. **Until this room closes** is available only with an
open origin-room session. Account utilities may be opened attached to a room or
space; detached instances have no room-close option. A saved background task's
room target alone is not an open-room lifetime anchor.

The origin determines expiry and the target scope determines permitted access.
For example, an **All rooms** grant obtained from room A with **Until this room
closes** ends when A closes, even when its request targets room B. It does not
last until B closes. The Robrix session lasts until the application quits, not
until a mini-app pane or hidden worker is restarted. Only **Forever** survives a
Robrix restart. Every duration retains capability, room-protection and sharing
checks; a read grant never grants sending.

Closing the last tab or screen for the origin room ends its session. Keeping
that mini-app in a separate window does not keep the room-session grant alive.

This table covers each source's operational context, including detached local
controls where applicable, rather than promising every launcher exposes every
context. Expiry recovery also applies to revocation. Previously loaded displays,
saved configuration and accepted drafts do not themselves grant new host access.

| Built-in | Context and work | Recovery after expiry |
| --- | --- | --- |
| Public Web | Account utility; optional room/space attachment. Button starts one fixed public HTTP request. | Fetch callback finishes on refusal; Fetch retries in the foreground. Detached requests omit room-close duration. |
| Website Watch | Room-attached scheduled task; each run fetches the saved URL and optionally notifies or posts. Foreground drafting is separate. | Failed steps clear busy state and acknowledge the run; foreground Test retries. Saved checkpoints remain. Closed-room grants cannot authorize later jobs; hidden runs never prompt. |
| Reminder | Account, room or space task; one notification per Test or scheduled run. | Refusal releases Test and finishes a run. Saved text remains; Test requests fresh approval. Detached tasks and scheduled reviews without a live origin omit room-close duration. |
| Keyword Alert | Room-attached background message condition; setup requests ongoing watching, then a concrete test notification. | Foreground Test checks `host.has` and requests missing watch permission. Failed notifications finish the run. Saved keyword remains; hidden work cannot renew expired watching or output grants. |
| Room Peek | Attached room; grouped reads plus five live hooks. Draft, attach, text send and media send are explicit controls. | Permission changes reset read flags and remove unavailable hooks; Refresh retries setup. Accepted drafts/attachments remain local; another draft or send requires its own current permission. |
| Roll Call | Dice work is local; drafting/posting needs an attached room. Both are one-shot requests. | Failure clears pending state and keeps the rolled result. Draft result and Post to room retry independently. Detached local rolling grants no room access. |
| Room Info | Attached room; one metadata read, no live hook. | Unavailable capability resets its read flag; Refresh or newly approved read loads again. Cached display does not authorize another read. |
| Room Members | Attached room; grouped member read and membership hook. | Revocation clears read/watch readiness; timer refresh checks live permission. Refresh requests setup again, and selected reapproval restores watching. |
| Pinned Messages | Attached room; grouped pin read and pin-change hook. | Read/watch readiness resets; refresh timers check permission. Refresh retries setup and tapping a pin requests separate navigation. |
| Room Threads | Attached room; grouped list/thread reads and message hook. | Missing permissions reset only affected flags and watching. Refresh retries the current view; stale callbacks cannot replace a later thread selection. |
| Search | Attached current-room search, or all/picked-room search where launched detached; each search is one-shot. | A failed search finishes; Search retries explicitly. Picker readiness resets on revocation and reloads only when selected and allowed. Detached searches omit room-close duration. |
| Simple Watcher | Attached room for an ongoing message hook and automatic popup/reply actions; rule editing is local. | Expiry clears watch/action queues and invalidates callbacks. Reapproval can restore watching; explicit Test resumes paused actions. Saved rules and Draft reply never re-enable automatic sending. |
| Watcher | Attached room for watching/replies; additionally registers the optional notification-only AI rule tool. | Same watch/action pause as Simple Watcher; missing tool permission clears registration readiness. Refresh/Test restore only approved work. Drafts and saved AI settings grant no send/tool authority. |
| Who's Here | Attached room; grouped receipts/event reads plus typing and receipt hooks. | Revocation resets read readiness and removes unavailable hooks. Refresh retries selected setup; serial event reads still undergo current permission checks. |
| Room Tools | Attached room; grouped reads, followed by one-shot flag, pin, clipboard and navigation actions. | Read flags reset individually. Refresh retries permitted parts; each management control makes a fresh captured request. |
| Spaces | Attached space browser, or joined-space list when detached; reads, preview/join/navigation are separate requests. | Revocation resets affected list/detail flags. Refresh retries; selection tokens reject stale replies. Detachment removes room-close duration even when selected targets are rooms. |
| Inbox | Room collection app, attached or detached; reads invites/unreads and subscribes to three collection hooks. | Permission changes clear read readiness and prune unavailable hooks; Refresh retries reads/hooks. Invite replies finish independently. Detached collection access has no origin-room lifetime. |
| Room Stats | Attached room; grouped message/history and power-level reads, no live stream. | Missing read capability resets count/details readiness; refused pages finish counting. Refresh retries, and each page remains permission checked. |
| Account | Account utility; optional room/space attachment. Grouped profile/account reads, concrete user/DM/browser actions. | Read flags reset per capability and only newly allowed unfinished reads resume. Refresh retries selected setup. Detached account access has no room-close duration. |
| Inspector | Account/device utility; optional room/space attachment. Grouped facts/preferences plus navigation/preference hooks. | Missing capabilities reset their read flags and prune hooks; Refresh restores selected work. Its own environment/pane reads do not grant protected device facts. Detached diagnostics omit room-close duration. |

The three background-app instructions now distinguish foreground room-close
approval from approval needed for jobs after closure. No new retry loop or
automatic permission request is needed in their source. A saved schedule is not a
permission grant, cannot prolong a room session, and cannot ask in a hidden run.

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

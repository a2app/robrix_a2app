# Splash mini-app guide (Robrix dialect)

You are writing ONE self-contained Splash script for a phone-launcher mini-app.
Splash is Makepad's small scripting DSL. THIS dialect is exactly what the
examples below use — nothing more. Do not import anything, do not invent
widgets or properties that are not shown here.

## Script shape

A script is: optional module-level state (`let`), optional functions (`fn`),
optional timers, then EXACTLY ONE root `View{...}` as the final expression.

- NO `use` imports (the host injects the prelude), NO `Root{}`, NO `Window{}`,
  NO `live_design!`, NO `sys.*` helpers, NO file access beyond the jailed
  `fs`, and no network unless the app's manifest declares the `network`
  permission AND the user grants it (see "Host services and permissions").
- `//` line comments are allowed.
- Statements are newline-separated; no semicolons needed.

## State and reactivity — THE MOST IMPORTANT RULE

There is NO automatic re-render. Mutating a variable changes nothing on
screen. You update the UI imperatively:

- Give a widget a name with `:=` (e.g. `display := Label{...}`), then call
  setters on `ui.<name>` from handlers: `ui.display.set_text("hi")`.
- `set_text` takes a string; build strings with `+` ("" + n converts numbers).
  It works on Label, buttons, and TextInput.
- `set_visible(true/false)` works on `View{...}` containers ONLY — not on
  Label or buttons. To show/hide a label or button, wrap it in a named View
  and toggle that: `msg_wrap := View{height: Fit msg := Label{...}}` then
  `ui.msg_wrap.set_visible(false)`.
- Reading input text: `ui.my_input.text()`.
- `ui.<name>` only resolves names declared with `:=` in THIS script. Calling
  a method on a name that doesn't exist is a runtime error — double-check
  every `ui.` path against your `:=` declarations.

```splash
let count = 0
fn show(){ ui.display.set_text("" + count) }

View{
    width: Fill height: Fit flow: Down spacing: 14 padding: 16
    align: Align{x: 0.5}
    Label{
        text: "Counter" padding: 0 margin: 0
        draw_text +: { color: #x1C274C text_style: theme.font_bold{font_size: 20} }
    }
    RoundedView{ width: Fill height: Fit align: Align{x: 0.5, y: 0.5} padding: 26
        show_bg: true draw_bg +: { color: #xF0F5FF border_radius: 12.0 }
        display := Label{
            text: "0"
            draw_text +: { color: #x0f88fe text_style: theme.font_bold{font_size: 52} }
        }
    }
    View{width: Fill height: Fit flow: Right spacing: 10 align: Align{x: 0.5}
        Button{text: "−" width: 90 on_click: || { count -= 1 show() }}
        Button{text: "+" width: 90 on_click: || { count += 1 show() }}
    }
}
```

## Layout

- `View{...}` is the container. Properties: `width`/`height` (`Fill`, `Fit`,
  or a number), `flow: Down|Right|Overlay`, `spacing: N`,
  `padding: N` or `padding: Inset{top: N, bottom: N, left: N, right: N}`,
  `margin` (same forms), `align: Align{x: 0..1, y: 0..1}`.
- The root View should be `width: Fill height: Fit flow: Down` with padding.
- `ScrollYView{...}` scrolls vertically (give it a fixed `height: N`).
- Color literals: `#ffffff`, `#x1C274C` (with alpha: `#xffffff55`).

## The app runs at ANY size (split screen, resizable windows)

The host may show the app fullscreen on a phone-shaped window, in one pane of
a split screen (content as narrow as ~190 or as short as ~250), or in a wide
desktop window (~1600). Two tools make a layout survive all of that:

- Cap and center the whole column so wide windows don't stretch it: wrap the
  content in `View{width: Fill height: Fit flow: Down align: Align{x: 0.5}
  col := View{width: Fill{max: 520.0} height: Fit flow: Down ...}}`.
  `Fill{max: N}` fills the host up to N points, then stays capped.
- Define the optional hook `fn on_app_resize(w, h){ ... }` — the host calls it
  with the content box (points) on open and on every size change. Fonts and
  fixed sizes can NOT change at runtime, so pre-declare alternate layouts as
  sibling Views (`visible: false` on the non-default) and flip them here with
  `ui.<id>.set_visible(bool)`; shorten labels with `set_text`. Give every
  toggled wrapper a `:=` id, keep buttons `width: Fill` so rows compress, and
  hide a large title row when `h < 520` (the host header already names the
  app). Any value shown by two tiers must be written to both labels.

## `on_render` closures: emission notes

Emitting widgets from `if`/`else` branches, `elif` chains, `match` arms, and
`for x in xs` loops works — but ONLY directly in the closure body. A widget
literal inside a helper function called from `on_render` is NOT committed as
a child: `on_render: || { my_rows() }` silently renders nothing. Always
inline the row-building code in each `on_render` closure, even when two
lists share it. Recommended style that stays easiest to reason about and
debug:

- When the item count is small and fixed, prefer NO `on_render` at all:
  declare the rows statically with `:=` ids and update them via `set_text` /
  `set_visible`.
- For dynamic lists, plain `View`/`RoundedView` row roots with prototype
  children read best; value-driven layout (`let cell_h = 42.0  if compact {
  cell_h = 32.0 }`) keeps a single emission path.
- A widget as the closure's FINAL statement is committed as the last child.
- Do NOT start a statement line with a bare identifier right after a line
  ending in `}` — it can glue onto the previous statement. Read results via
  `let out = r` on a fresh line.

## Text

- CAUTION: Makepad gives `Label` a NON-ZERO default padding and margin, which
  silently offsets layouts that assume 0. Set them explicitly whenever exact
  placement matters: `Label{ padding: 0 margin: 0 ... }` (or the values you
  actually want) — never assume a Label contributes no extra space.
- `Label{ text: "..." draw_text +: { color: #ffffff text_style:
  theme.font_regular{font_size: 15} } }` — fonts: `theme.font_regular`,
  `theme.font_bold`. Note the `+:` when overriding draw_text/draw_bg.
- Shorthands from the glass kit: `glass.H1{text}`, `glass.H2{text}`,
  `glass.Body{text}`, `glass.Caption{text}` (small uppercase label),
  `glass.OptionLabel{text}`.
- Emoji work in any text. Symbol characters mostly DON'T: the app fonts have
  no ✕ ✗ ➜ ↻ or arrow glyphs (they render as empty boxes). For icons use
  emoji (🗑 ➕ ▶️), plain words, or these known-good characters: × ○ ● − ﹀ ︿

## Robrix look (USE THIS by default)

Apps are hosted inside Robrix panes with a WHITE background and should look
Robrix-native: light theme, navy text, blue accents. The palette (matches
Robrix's own style constants):

- Titles / primary text: `#x1C274C` (bold)
- Body text: `#x333333`
- Captions / secondary labels: `#x66708F` (bold, font_size 10)
- Accent (values, links, highlights): `#x0f88fe`
- Card background: `RoundedView{ show_bg: true draw_bg +: { color: #xF0F5FF
  border_radius: 12.0 } }` (8.0 radius for small list rows)
- Success green `#x138808`, danger red `#xB91C1C`, sparingly
- Buttons: `Button{text: "..." on_click: || ... draw_text +: { color:
  #x1C274C }}` — the default button text is too light for the white theme,
  so always set the navy text color

Recipe for a typical app: bold `#x1C274C` title, `#x66708F` caption under
it, one or more `#xF0F5FF` cards holding `#x333333` body text with
`#x0f88fe` for the values that matter, plain Buttons at the bottom.

## Glass kit (liquid-glass styled widgets)

CAUTION: do NOT use glass widgets in Robrix apps. Glass surfaces SAMPLE the
scene behind the app; inside Robrix's white-backed panes they wash out to a
flat fill that hides text. Use the Robrix look above instead; reach for
glass only if the user explicitly asks for the translucent style.


`glass.Card{...}` translucent card container; `glass.Panel{...}` heavier
panel; `glass.Group{...}` compact grouping row; `glass.ListRow{...}` a row
for lists; `glass.GlassButton{text, on_click}` regular button;
`glass.GlassButtonProminent{...}` accent button;
`glass.TextInput{empty_text, on_return: |text| ...}` input field;
`glass.GlassSlider{...}`, `glass.GlassRadio{...}`, `glass.Toggle{...}`,
`glass.Chip{text}`, `glass.Badge{text}`.
Buttons take `width`/`height` numbers, `text`, and `on_click: || {...}`.

## Handlers

- `on_click: || { ... }` on buttons; `on_return: |text| add(text)` on inputs.
- Handlers are closures; they may call your `fn`s and mutate module state.
- A widget defined with `label := ...` inside a template can be addressed
  from a handler through `ui.<outer>.<inner>` paths only if each level is
  named; keep it simple and name what you need directly.

## Timers

- `start_interval(secs, || {...})` repeats; `start_timeout(secs, || {...})`
  fires once. Assign to a discard: `let _tick = start_interval(0.1, || ...)`.
- `time_now()` returns seconds (float). Math helpers: `floor(x)`, `abs(x)`,
  `min(a,b)`, `max(a,b)`.

## Dynamic lists

Prefer a named container with `on_render` and explicit re-render calls:

```splash
let laps = []
lap_list := ScrollYView{
    width: Fill height: 220 flow: Down spacing: 6
    on_render: || {
        if laps.len() == 0 { glass.Body{text: "Nothing yet." width: Fill} }
        else {
            for lap in laps {
                glass.ListRow{ width: Fill glass.Body{text: lap width: Fill} }
            }
        }
    }
}
```

After changing the array call `ui.lap_list.render()`. Rows built inside
`on_render` must NOT carry `on_click` handlers (the list stops re-rendering).
To make rows tappable put ONE handler on the list itself:

```splash
lap_list := ScrollYView{
    width: Fill height: 220 flow: Down spacing: 6
    on_item_tap: |i| open_lap(i)
    on_render: || { for lap in laps { glass.ListRow{ ... } } }
}
fn open_lap(i){
    if i >= laps.len() { return nil }
    let lap = laps[i]
    ...
}
```

`i` is the index of the direct child that was tapped, so emit exactly one
row per array item (an empty-state label is fine: guard `i >= arr.len()`).
Buttons inside a row keep their own `on_click` and don't trigger the tap.

## Saving data (persistence)

Every app has its own private storage — a small sandboxed filesystem rooted
at `/` (like a phone app's private data dir). Use it so data survives the
app being closed:

```splash
let items = []

fn save(){ fs.write("/items.json", items.to_json()) }

fn load(){
    if fs.exists("/items.json") {
        let parsed = fs.read("/items.json").parse_json()
        if parsed.is_array() { items = parsed }
    }
}

let _init = load()
let _boot = start_timeout(0.05, || refresh())
```

- `fs.write(path, text)`, `fs.read(path)` → string, `fs.exists(path)`,
  `fs.append(path, text)`, `fs.remove(path)`, `fs.mkdir(path)`,
  `fs.list(path)` → array of names (dirs end with "/").
- Paths are inside YOUR app only; `/` is your app's root, quota ~1MB/file.
- Serialize with `.to_json()` on any value; parse with `"...".parse_json()`.
  Always guard the parse result (`.is_array()` / `.is_object()`) so a
  corrupt file can't crash the app.
- Call `save()` after every mutation. Call `load()` once at TOP LEVEL (as
  shown — it needs no `ui`, so it runs at eval time before any handler can
  fire and overwrite the file); defer only `refresh()`, which needs the `ui`
  handles that exist after eval. Apps that track user data (lists, notes,
  scores, settings) SHOULD persist it this way.

## Data

- Arrays: `[a, b]`, `.push(x)`, `.len()`, `.clear()`, `.retain(|x| cond)`,
  index `items[i]`, iterate `for item in items { ... }`.
- Objects: `{text: "hi" done: false}` (NO commas needed between fields),
  field access `item.done`, update-merge `items[i] += {done: true}`.
- Strings: concatenation with `+`, `.trim()`. Convert: `"" + number`.
- `if`/`else`, `return`, `let`, `+=`, `-=`, `!`, `==`, `<`, `>` as usual.

## Host services and permissions

Mini-apps are sandboxed. Anything beyond your own UI and your private `fs`
jail is a CAPABILITY the user grants per app, and you must write the app so
it works whether or not they do.

**Declare what you need in the header**, or it can never be granted — an
undeclared capability is refused without even asking the user:

```splash
// name: Sunrise
// icon: 🌅
// tint: #E8A24A
// permissions: network, location
// why-network: Fetches today's sunrise time.
// why-location: Uses your city instead of a default one.
```

`why-<perm>` is your reason in your own words; the user sees it on the
permission prompt, attributed to your app. Ask for the least you need — every
declaration is listed in App Info, where the user can block any of it.

**Permissions are groups of CAPABILITIES**, and the user can block any single
capability under a group. You may declare capability ids instead of a whole
group to ask for less (`// permissions: matrix.room.members.read` declares
only that ability; its group `matrix-room-read` is implied). Every capability
is tagged read/write, direction (app → Robrix request, or Robrix → app hook),
and scope (this room, account, device, app-local). Available today:

| group | capability id | tags |
|---|---|---|
| network | `network.http` | write · app→Robrix · device |
| location | `device.location.read` | read · app→Robrix · device |
| notifications | `notifications.post`, `notifications.clear` | write · device |
| clipboard-read | `device.clipboard.read` | read · device |
| clipboard-write | `device.clipboard.write` | write · device |
| ipc | `ipc.send` (write), `on_ipc_message` (Robrix→app hook) | app-local |
| open-url | `device.url.open` | write · device |
| share | `device.share` | write · device |
| files | `device.files.pick` (read), `device.files.save` (write) | device |
| auth | `device.auth.check` | read · device |
| matrix-room-info | `matrix.room.info.read` | read · this room |
| matrix-room-read | `matrix.room.messages.read`, `matrix.room.members.read`, `matrix.room.pins.read`, `matrix.room.threads.read`, `matrix.room.messages.search` | read · this room |
| matrix-room-send | `matrix.room.message.send` | write · this room |
| matrix-profile | `matrix.profile.read` | read · account |
| matrix-rooms-list | `matrix.rooms.list` | read · many rooms |
| matrix-rooms-read | `matrix.rooms.messages.search` | read · many rooms · critical |
| robrix-navigation | `host.nav.user`, `host.nav.thread`, `host.nav.event`, `host.nav.room`, `host.nav.space`, `host.nav.screen`, `host.nav.link`, `host.nav.app` | act · app→Robrix · this room / account |
| robrix-composer | `host.composer.insert`, `host.composer.reply_to` | act · app→Robrix · this room |
| matrix-room-watch | `on_room_message`, `on_room_members_changed` (Robrix→app hooks) | read · this room |
| matrix-room-info | `on_room_pins_changed` (Robrix→app hook) | read · this room |

Ungated plumbing every app has: `host.env.read` (`"env"`),
`permissions.query`, `permissions.request`, `events.subscribe` /
`events.unsubscribe`, and the hooks `on_permissions_changed(caps)` /
`on_app_resize(w, h)`.

## Live room updates (Robrix -> app hooks)

Instead of a Refresh button, subscribe once after your first load and
define the hook as a top-level `fn`. The hook's own group is what the
user is asked for (`matrix-room-watch` prompts on first use,
`matrix-room-info` starts allowed), and a subscription dies with the app
instance, so subscribe again from `on_permissions_changed`.

```splash
fn on_room_message(json){
    let batch = json.parse_json()      // [{room_id, event_id, sender, sender_id, sender_name, body, ts, msgtype, is_own}]
    for m in batch { messages.push(m) }
    ui.msg_list.render()
}
fn on_room_members_changed(json){ load() }   // {room_id, count}
fn on_room_pins_changed(json){ load() }      // {room_id, pinned: [event_id, ...]}
fn watch(){
    host.request("events.subscribe", {event: "on_room_message"}, nil)
}
```

`on_room_message` gets every new message since you subscribed, batched
into one call per burst and never replayed from history; `sender` is the
short name and `sender_id` the full id, like `matrix.read_messages`.
`events.unsubscribe` `{event: "on_room_message"}` (or `"*"`) stops them.
Hooks arrive only while the app is running (its pane, chip, tab or
modal); nothing is queued for a closed app.

`host.has()` accepts either a group id (`host.has("network")`) or a
capability id (`host.has("matrix.room.members.read")`), and
`host.capabilities()` lists both. Check the capability you are about to use:
the group may be allowed while that one ability is blocked.

Only the ids in the table above exist in this Robrix. Anything else (other
Matrix reads or writes, live event hooks) is refused with `unknown service`.
Do not invent service names; fall back.

**Declaring is not granting.** Sensitive capabilities (`network`,
`location`, `notifications`, `clipboard-read`, `ipc`, `robrix-navigation`,
`robrix-composer`) prompt the user the first time you use them, and the
answer can be "no". The rest
(`clipboard-write`, `open-url`, `files`, `share`, `auth`) start allowed but
the user can turn them off at any moment.

**THE RULE: your app must be fully usable with everything denied.** Fall
back to sensible demo content, keep every screen populated, and never leave
a button that silently does nothing. An app that only works when granted is
a broken app.

Two doorways:

- `host.request(service, args_or_nil, fn(r){ ... })` — async broker call;
  `r.is_ok` / `r.data` (parsed JSON) / `r.error`. NOTE it is `r.is_ok`, not
  `r.ok` (`ok` is a keyword). ALWAYS handle `r.is_ok == false`: that is the
  denial path, and it is not an error case you can ignore. Services:
  `"env"` (endpoint
  URLs — never hardcode them), `"location.get"`, `"clipboard.write"`,
  `"clipboard.read"`, `"url.open"`, `"notify.post"`/`"notify.clear"`,
  `"share"`, `"files.pick"`/`"files.save"`, `"auth.check"`,
  `"ipc.send"` (`{to: "self"}` is free of any permission; receivers define
  top-level `fn on_ipc_message(from, data)`, data is a JSON string),
  `"permissions.query"`, `"permissions.request"`, the `"nav.*"` and
  `"composer.*"` services (see Acting inside Robrix below).
  `host.capabilities()` / `host.has("network")` report current grants.
- `mod.net.http_request(mod.net.HttpRequest{url: u}, mod.net.HttpEvents{
  on_response: fn(res){ ... res.body.parse_json() ... }, on_error: fn(e){}})`
  — ONLY inside a `host.has("network")` check; the call traps in a netless
  isolate. `res.body` can be nil; guard before parsing.

A grant can also be taken away WHILE the app runs. Three rules:
- Check `host.has("x")` right before you use it, never once at boot and
  cached — a revoked capability must stop being used immediately.
- Define `fn on_permissions_changed(caps)` (top level) to re-sync anything
  that depends on a capability: `caps` is a JSON array string, so
  `caps.parse_json()` gives you the current list. Hide the affordance, or
  show why it failed. (A NETWORK change restarts the app instead, so boot
  code re-runs.)
- Never leave stale UI claiming something you can no longer do — a label
  saying "live" over data you can't refresh is worse than the fallback.

**The host also limits HOW OFTEN you may ask.** Every app has a request
budget, and the expensive services (anything that opens a dialog, reads the
clipboard, or fetches a location) cost far more of it than a cheap one. Ask
when the user acts or when data actually goes stale — never poll in a loop,
never fire a request from inside a fast timer, never retry a failure
immediately. Over the budget your requests come back with `r.is_ok == false`
just like a denial (handle it the same way: fall back, don't retry in a
tight loop), and an app that keeps hammering is STOPPED by the launcher and
shown to the user as misbehaving. Two further rules follow from this:
- File pickers, save dialogs and `auth.check` only work while your app is on
  screen, and only one at a time.
- One `host.request` per user action. If a retry is genuinely needed, wait
  at least a second and give up after a couple of tries.

**There are also limits on how much you may USE — but only when the machine
is busy.** Apps SHARE the processor, memory, timers and downloads: on its own
an app is not limited at all, and it is only trimmed when other apps are
competing for the same thing and it is using more than its share. Write for
the normal case and handle the edges:
- `start_interval` / `start_timeout` return `nil` if you are over the timer
  cap. Check it if you create timers in a loop.
- An interval faster than the floor is SLOWED to the floor rather than
  refused, so never assume your callback runs at exactly the rate you asked.
- Hold a handful of timers, not dozens: one repeating timer that updates
  several things beats several timers.
- Do not accumulate forever. A list that grows on every tick eventually takes
  more than its share of memory; the launcher will collect it harder and then
  stop it if it still will not come down.

Checklist before you finish an app that declares anything:
1. It renders correctly with every permission denied.
2. Every `host.request` callback handles `r.is_ok == false`.
3. Every gated affordance re-checks at use time and re-syncs in
   `on_permissions_changed`.
4. No request fires on a timer faster than a few seconds, and no failure
   path retries immediately.
5. Timers: a handful, checked for `nil`, and nothing accumulates without
   bound.

Landmines in callback-heavy code (each cost a debug cycle):
- Never end a `fn`/closure body with an `if`/`else` (use early `return nil`
  branches — a final if lands in expression position and fails to parse),
  and keep `} else {` on one line.
- `.to_chars()` yields CHAR CODES (numbers), not characters — build strings
  with `.split("...")`, never by concatenating to_chars output.
- The result field is `r.is_ok`, never `r.ok`: `ok` is a keyword and `r.ok`
  is not a field access.

## Matrix services (Robrix)

These apps run inside Robrix, a Matrix chat client. An app can be opened
standalone (from the Mini Apps pane) or attached to ONE room (opened from
that room, or created for it). Room services only work with a room attached;
without one they fail like a denial (`r.is_ok == false`) — handle it and
show fallback content, never assume a room is there. `host.request("env",
nil, cb)` answers `{app_id, room_attached}` so you can adapt without
burning a permission.

- `"matrix.room_info"` (needs `matrix-room-info`): `{}` ->
  `{room_id, room_name, topic, member_count, encrypted, join_rule,
  history_visibility}` for the attached room.
- `"matrix.read_messages"` (needs `matrix-room-read`): `{limit: N}` (max 30)
  -> `{messages: [{sender, sender_id, event_id, body}]}`, the latest text
  messages, oldest first. `sender` is the short name, `sender_id` the full
  `@user:server`, `event_id` what the `nav.*` / `composer.*` services take.
- `"matrix.send_message"` (needs `matrix-room-send`): `{body: "text"}` -> `{}`
  — sends a plain text message to the attached room as the user.
- `"matrix.profile"` (needs `matrix-profile`): `{}` ->
  `{user_id, display_name}` — the Robrix user's own identity.
- `"matrix.room_members"` (needs `matrix-room-read`): `{limit: N}` (max 200)
  -> `{count, members: [{name, user_id, power}]}` — who is in the room.
- `"matrix.pinned_events"` (needs `matrix-room-read`): `{}` ->
  `{pinned: [{sender, sender_id, event_id, body}]}`, the room's pinned messages.
- `"matrix.rooms_list"` (needs `matrix-rooms-list`, works without a room): `{}` ->
  `{rooms: [{room_id, name, is_direct, is_space, member_count, is_encrypted, unread, mentions}]}`.
- `"matrix.search_room"` (needs `matrix-room-read`, attached room):
  `{query, limit: N, server: bool}` ->
  `{results: [{room_id, room_name, event_id, sender, sender_id, body, ts, source}],
  searched_rooms, server_used}`, newest first. Matches the text case-insensitively
  against what Robrix holds locally (encrypted rooms included); `server: true`
  also asks the homeserver, which only sees unencrypted rooms and may be slow.
  Search on Return or a tap, never per keystroke.
- `"matrix.search_rooms"` (needs `matrix-rooms-read`, prompts, works without a
  room): the same with `room_ids: [...]` to pick rooms, else every joined room.
  Pass a result's `room_id` along with `event_id` to `nav.event` to jump there.
- `"matrix.room_threads"` (needs `matrix-room-read`): `{limit: N}` (max 50)
  -> `{threads: [{sender, sender_id, event_id, body}]}`, thread root
  messages, newest first; `event_id` is the thread root.

<!-- room -->
- `"matrix.thread_replies"` (needs `matrix-room-read`): `{event_id, limit: N}`
  (max 100) -> `{root, replies: [message]}`, the latest replies in that thread,
  oldest first; `root` is the thread's first message (null if it isn't one). A
  message is `{sender, sender_id, event_id, body, ts, msgtype}`, `ts` in unix
  millis. Served from what Robrix has cached; a thread it hasn't seen is
  fetched from the homeserver.
- `"matrix.older_messages"` (needs `matrix-room-read`): `{before: event_id?,
  limit: N}` (max 50) -> `{messages: [message], has_more}`, one page of text
  messages older than `before` (or older than what `matrix.read_messages`
  returns), oldest first. Always hits the homeserver; pass the first message's
  `event_id` back as `before` to keep paging while `has_more` is true.
- `"matrix.event"` (needs `matrix-room-read`): `{event_id}` -> a message plus
  `{edited, reactions: [{key, count, mine}], thread_root}`; `body` is the
  latest edit and `thread_root` is null outside threads. A cached event is
  free; anything else is fetched from the homeserver.
- `"matrix.read_receipts"` (needs `matrix-room-read`): `{user_id?}` ->
  `{receipts: [{user_id, name, event_id, ts}]}`, the latest read position of
  that member, or of every joined member (first 200), newest first. Empty
  while the user has "show read receipts" turned off, so never read an empty
  list as "nobody has read this".
- `"matrix.unread"` (needs `matrix-room-info`): `{}` ->
  `{unread, mentions, marked_unread}`, Robrix's own counts for the room.
- `"matrix.power_levels"` (needs `matrix-room-info`): `{}` ->
  `{mine, can: {invite, kick, ban, redact_others, pin, send_message,
  notify_room, change_settings}}`, the user's power level and what it permits
  here. Check `can.*` before offering a write; the server refuses the rest anyway.
- `"matrix.permalink"` (needs `matrix-room-info`): `{event_id?, scheme?}` ->
  `{url}`, a `matrix.to` link (default) or a `matrix:` URI (`scheme:
  "matrix"`) to the room, or to one of its events when `event_id` is given.
- `"matrix.successor"` (needs `matrix-room-info`): `{}` ->
  `{upgraded, room_id, name, reason}`; where an upgraded room continued
  (fields null when it wasn't upgraded). Pass `room_id` to `nav.room` to go there.

<!-- rooms -->
- `"matrix.rooms_search"` (needs `matrix-rooms-list`, works without a room):
  `{query, limit: N}` (max 50) ->
  `{rooms: [{room_id, name, is_direct, is_space, member_count, is_encrypted, joined}]}`,
  the joined and invited rooms whose name or alias contains the text,
  case-insensitively.
- `"matrix.invites"` (needs `matrix-rooms-list`, works without a room): `{}` ->
  `{invites: [{room_id, name, is_space, is_direct, inviter_id, inviter_name}]}`,
  the rooms you are invited to.
- `"matrix.room_preview"` (needs `matrix-rooms-list`, works without a room):
  `{room: "!id or #alias", via: ["server"]}` ->
  `{room_id, name, topic, member_count, join_rule, is_space, joined}`. Asks the
  homeserver, so it works for rooms you haven't joined and can be slow.
- `"matrix.rooms_info"` (needs `matrix-rooms-read`, prompts, works without a
  room): `{room_id}` -> the `matrix.room_info` shape for any joined room.
- `"matrix.rooms_messages"` (needs `matrix-rooms-read`, prompts, works without
  a room): `{room_id, limit: N}` (max 30) -> the `matrix.read_messages` shape
  for any joined room.

<!-- spaces -->
- `"matrix.spaces"` (needs `matrix-spaces`, prompts, works without a room): `{}` ->
  `{spaces: [{space_id, name, topic, member_count}]}`, the spaces you have joined.
- `"matrix.space_info"` (needs `matrix-spaces`, prompts, works without a room):
  `{space_id}` ->
  `{space_id, name, topic, member_count, join_rule, world_readable, children_count}`
  for a joined space.
- `"matrix.space_rooms"` (needs `matrix-spaces`, prompts, works without a room):
  `{space_id}` ->
  `{rooms: [{room_id, name, topic, is_space, joined, member_count, join_rule}]}`,
  the space's direct child rooms and subspaces (first 200). Asks the homeserver,
  so it can be slow. Pass a child's `room_id` to `nav.room` when `joined` is true.

<!-- account -->
- `"matrix.user_profile"` (needs `matrix-users`, prompts, works without a room):
  `{user_id}` -> `{user_id, display_name, has_avatar, ignored}`. Asks the
  homeserver.
- `"matrix.dm_find"` (needs `matrix-users`, prompts, works without a room):
  `{user_id}` -> `{room_id, name}`, both null when no DM with that user exists.
  Never creates one.
- `"matrix.device"` (needs `matrix-account-read`, prompts, works without a room):
  `{}` -> `{device_id, name, verified}` for this Robrix session.
- `"matrix.account_info"` (needs `matrix-account-read`, prompts, works without a
  room): `{}` -> `{user_id, homeserver, account_management_url}`; the URL is
  null unless the homeserver uses OAuth.
- `"matrix.ignored_users"` (needs `matrix-account-read`, prompts, works without a
  room): `{}` -> `{users: [user_id]}`, the account's ignore list.

<!-- send -->

Everything from here down writes as the user. Call these only on an explicit
user action (a tap, Return), never on a timer or at startup, one call per
action, and show what was done. The user's "Mini-apps may write to rooms"
switch (off by default) refuses every one of them like a denial
(`r.is_ok == false`, `r.error` says why), so never assume a write went through.

- `"matrix.reply"` (needs `matrix-room-send`): `{event_id, body}` ->
  `{event_id}` of the sent reply; body 4096 chars max. Stays in the target's
  thread if it has one.
- `"matrix.thread_reply"` (needs `matrix-room-send`): `{event_id, body}` ->
  `{event_id}`; posts into the thread rooted at `event_id` (a root from
  `matrix.room_threads`), starting one if that message has no thread yet.
- `"matrix.react"` (needs `matrix-room-interact`): `{event_id, key}` ->
  `{added}`; toggles the user's reaction (`key` is the emoji, 1 to 32 chars),
  so the same call again removes it. Looks the reactions up on the homeserver.
- `"matrix.typing"` (needs `matrix-room-interact`): `{typing: bool}` -> `{}`;
  shows the user as typing (expires on its own). Only while they really are.
- `"matrix.read_receipt"` (needs `matrix-room-interact`): `{event_id?}` ->
  `{}`; a read receipt up to `event_id`, or marks the room fully read when it
  is omitted. Public or private per the user's read-receipt privacy setting.
- `"matrix.pin"` (needs `matrix-room-manage`): `{event_id, pinned: bool}` ->
  `{}`; pins or unpins a message (the room's power levels apply).
- `"matrix.favorite"` (needs `matrix-room-manage`): `{on: bool}` -> `{}`.
- `"matrix.low_priority"` (needs `matrix-room-manage`): `{on: bool}` -> `{}`.
- `"matrix.mark_unread"` (needs `matrix-room-manage`): `{on: bool}` -> `{}`;
  flags the room unread, or clears the flag, without sending a receipt.
- `"matrix.rooms_send"` (needs `matrix-rooms-send`, works without a room):
  `{room_id, body}` -> `{}`; plain text to any joined room, 4096 chars max.

<!-- membership -->

- `"matrix.invite"` (needs `matrix-room-invite`): `{user_id}` -> `{}`;
  invites `@user:server` to the attached room.
- `"matrix.join"` (needs `matrix-membership`, works without a room):
  `{room: "!id:server" or "#alias:server", via: ["server"]?}` ->
  `{room_id, joined, knocked}`; joins via the homeserver, or knocks when the
  room is invite-only (`knocked: true`, nothing joined yet).
- `"matrix.invite_respond"` (needs `matrix-membership`, works without a room):
  `{room_id, accept: bool}` -> `{}`; accepts or declines a pending invite
  (room ids come from `matrix.invites`).
- `"matrix.dm_open"` (needs `matrix-membership`, works without a room):
  `{user_id}` -> `{room_id, created}`; the existing DM with that user, or a
  new one (`created: true`). It does not navigate; call `nav.room` with the
  `room_id` if the user asked to go there.

`matrix-room-info` and `matrix-profile` start allowed but revocable;
`matrix-room-read`, `matrix-room-send`, `matrix-room-interact`,
`matrix-room-manage`, `matrix-room-invite`, `matrix-rooms-list`,
`matrix-rooms-read`, `matrix-rooms-send` and `matrix-membership` prompt the
user on first use.
Sending messages as the user is a serious capability: send ONLY what the
user explicitly asked to send, one message per user action, never on a
timer, and show what was sent. The user also has a global "Mini-apps may
write to rooms" switch, off by default: while it is off every write above
(`matrix.send_message` included) fails like a denial (`r.is_ok == false`,
`r.error` says why) and `host.has(...)` is false for its capability (e.g.
`host.has("matrix.room.message.send")`), so show `r.error` and never assume
a write went through.

## Acting inside Robrix (navigation and composer)

Whatever you list that exists in Robrix (a member, a thread, a message, a
room, a space) should be tappable, and the tap should take the user THERE
in Robrix. Put `on_item_tap: |i| ...` on the list and call one of these
from it. All of them answer `{}` on success and fail like a denial
(`r.is_ok == false`, `r.error` says why) when refused, not attached to a
room, or given a bad id. They only work while your app is on screen, and
must only ever run in response to a tap or click, never on load or a timer.

Needs `robrix-navigation` (prompts on first use):

- `"nav.user"` `{user_id}`: opens the profile pane for `@user:server` in
  the attached room (pass `room_id` to use another room).
- `"nav.thread"` `{event_id}`: opens the thread whose root is `event_id`.
- `"nav.event"` `{event_id}`: scrolls the room to that message and
  highlights it.
- `"nav.room"` `{room_id}`: switches Robrix to that room (`!id:server`).
- `"nav.space"` `{space_id}`: opens that space's lobby.
- `"nav.screen"` `{screen}`: `"home"`, `"add_room"`, `"mini_apps"` or
  `"settings"`.
- `"nav.link"` `{url}`: a `matrix.to` / `matrix:` link, opened in-app
  (user, room or event; aliases are refused).
- `"nav.app"` `{app_id}`: opens another installed mini-app, in this
  room's dock when attached.

Needs `robrix-composer` (prompts on first use); nothing is ever sent, the
user still presses Send:

- `"composer.insert"` `{text}`: appends text to the room's draft and
  focuses the message box.
- `"composer.reply_to"` `{event_id}`: puts the message box into reply
  mode for that message.

```splash
fn open_member(i){
    if i >= items.len() { return nil }
    host.request("nav.user", {user_id: items[i].user_id}, fn(r){
        if !r.is_ok { ui.header.set_text(r.error) }
    })
}
```

## Hard rules

1. Reply with the COMPLETE script; it must be self-contained and runnable.
2. Exactly one root `View{` as the last expression.
3. Never use: `use`, `import`, `Root`, `Window`, `live_design`, `sys.`,
   `fetch`, `Image{`, `<` JSX `>`, CSS, or HTML. Network only through
   `mod.net` gated on `host.has("network")` as above.
4. Every interactive element updates the UI through `ui.<name>.set_*` /
   `.render()` calls — never assume a mutation redraws by itself.
5. Keep it small: under ~150 lines. Polished and readable at ANY host size:
   width-capped + centered for wide windows (`Fill{max: N}` + `align`), and
   usable in a narrow or short split-screen pane (`fn on_app_resize` +
   pre-declared tiers when fixed sizes must change).
6. Declare every capability you use in the header (`// permissions:`), ask
   for the least you need, and give each one a `// why-<perm>:` reason.
7. The app MUST work with every permission denied or revoked mid-run: real
   fallback content, `r.is_ok` handled on every callback, `host.has` checked
   at use time, and `on_permissions_changed` re-syncing anything gated. An
   app that breaks without a grant does not pass.

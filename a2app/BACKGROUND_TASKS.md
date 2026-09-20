# Background mini-apps

Mini Apps > Background tasks saves an explicitly enabled task for an installed
mini-app and one account, room, or space. A run reuses an existing
foreground instance when available, otherwise it restores an isolate from that
context's private storage. Hidden workers are retired after their run; the next
trigger restores their saved files. Pause, Remove,
and Force Stop end its background work; Force Stop disables that app's tasks in
the current account.

Robrix must be running and signed in. There is no external daemon, OS alarm,
closed-app execution, or guarantee of execution while the OS suspends Robrix.
Tasks resume in their original account and context after launch. A missing or
unjoined room is never replaced with another room. A changed app version requires
review and explicit re-enabling.

## Try the examples

- **Website Watch**: choose a room, open the app to save an HTTP(S) URL and a
  literal keyword, and schedule an interval of **12 hours**. A new match produces
  a Robrix popup. The optional room-message output needs a current action
  approval after website input; see the review instructions below. It remembers a
  reported match across restarts and reports again only after the keyword was
  absent. Responses are limited to 64 KiB in this example. It does not execute
  JavaScript or scrape authenticated browser sessions.
- **Reminder**: save a reminder in an account, room, or space and choose an
  interval or a one-time UTC alarm. Use it for a regular break, a project check-in,
  or a deadline. The output is a Robrix popup.
- **Keyword Alert**: select a room and the **New room messages** trigger. Open the
  app to save a keyword such as `release` or an incident tag. Matching is literal
  and case-sensitive; your own messages are ignored. Each delivered batch can
  produce one popup.

Use **Run now** to test the configuration. For a one-time alarm, this fires and
consumes the alarm; save a new future alarm to schedule another run.
Scheduled execution grants no new
permission. Before leaving a task running, configure its app permissions,
internet destinations, and source-sharing rules. Internet permission can let
room or account content leave the device, depending on the app's behavior.
Background execution cannot open permission prompts. Review failures using App
permissions, Data sharing and action review, and Protection activity. Session
permissions and action approvals still expire when their session ends; enabling
or restoring a task does not extend them. A restored task can therefore require
fresh action review before an operation succeeds. In particular, room posting
after website input is not a persistent unattended delegation. To review it,
open the task's app and keep it open, use Run now, review the current blocked
action, then retry. A stopped activation's one-time approval cannot be reused by
a new background run. Use the popup output for unattended Website Watch checks.

## Scheduling and recovery

- Intervals are at least 60 seconds. Missed intervals coalesce into one run;
  there is no backlog of every missed tick.
- An overdue, unclaimed alarm runs once on the next eligible wake. A completed
  alarm stays completed across restart. An alarm interrupted after dispatch
  requires review because its external effect might already have happened.
- Room-message conditions use the existing live timeline watch in the exact
  room, including its read and IFC checks. They do not aggregate a space's child
  rooms or replay offline history. Inputs and event identity deduplication are
  bounded; messages arriving while a run is busy can be dropped.
- There is one saved task per account/app/context, at most 64 tasks, and at most
  four claimed runs at once. Each claimed run has a two-minute completion
  deadline. A timed-out instance is stopped before another run can be scheduled.
- Scheduling records are host-owned and saved atomically before dispatch.
  Corrupt or unwritable settings fail closed. No HTTP bodies, message contents,
  credentials, or arbitrary script-authored status are saved in that file.
- App state is restored from its existing account/app/context filesystem jail.
  This is **not** a serialized VM heap. Save configuration and checkpoints with
  `fs.write` as they change. An in-flight HTTP request or callback cannot survive
  process termination.

Execution is best effort. In particular, a crash after a remote server accepts a
message but before a checkpoint is saved leaves the result uncertain. Apps that
need stronger duplicate protection must use destination-supported idempotency or
reconcile their saved state. Robrix does not promise exactly-once external effects.

## App API

Opt in with a header in the first 20 lines of source:

```javascript
// name: My reminder
// background: true
// permissions: notifications
// why-notifications: Shows my saved reminder when its task is due.

fn on_background(json){
    let task = json.parse_json()
    host.request("notify.post", {body: "Time for a break."}, fn(r){
        host.request("background.complete", {
            run_id: task.run_id
            success: r.is_ok
        })
    })
}

View{width: Fill height: Fill}
```

`on_background(json)` receives `run_id`, `job_id`, `reason` (`interval`, `alarm`,
`manual`, or `room_messages`), UTC millisecond timestamps `scheduled_at`,
`started_at`, `deadline_at`, and `messages` (empty except for a room-message
trigger). Room-message tasks must declare the appropriate room-watch permission.
The host attributes those messages to their source room before invoking the hook.

Call `background.complete` once, after **all** work and its checkpoint have
finished. The host validates the run ID, requesting isolate, account, and
activation. Stale, duplicate, and another app's completions cannot finish a run.
The service cannot create, enable, reschedule, or grant authority to a task.
Completion is cooperative: do not acknowledge while callbacks are still working.
Every service call still undergoes the ordinary permission and IFC checks.

A task may run before its app is ever drawn. Keep its work independent of `ui.*`;
store status in script state and render it when the foreground UI is initialized.
Use host scheduling instead of a perpetual `start_interval` for task triggers.

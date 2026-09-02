//! The worker-side account watch: room list changes, new invites and the
//! account-wide unread totals get posted to the UI thread as
//! `A2AppAccountWatchEvent`s.

use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};

use makepad_widgets::{log, Cx, SignalToUI};
use matrix_sdk::Room;
use matrix_sdk::ruma::{OwnedRoomId, OwnedUserId};
use tokio::runtime::Handle;
use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinHandle;

use crate::sliding_sync::get_client;

/// The hooks this watch feeds; any subscription holding one keeps it running.
pub const ACCOUNT_HOOKS: [&str; 3] = ["on_rooms_changed", "on_invite_received", "on_unread_totals_changed"];

#[derive(Clone, Debug)]
pub enum AccountWatchKind {
    RoomsChanged { joined: Vec<OwnedRoomId>, left: Vec<OwnedRoomId>, changed: Vec<OwnedRoomId> },
    InviteReceived {
        room_id: OwnedRoomId,
        name: String,
        inviter: OwnedUserId,
        inviter_name: String,
        is_space: bool,
    },
    UnreadTotalsChanged { unread: u64, mentions: u64 },
}

#[derive(Clone, Debug)]
pub struct A2AppAccountWatchEvent {
    pub kind: AccountWatchKind,
}

type Listing = (Option<String>, u64, u64, bool, bool);

static ACCOUNT_WATCH: LazyLock<Mutex<Option<JoinHandle<()>>>> = LazyLock::new(Default::default);

/// Spawns the watch on the current tokio runtime, replacing any existing one.
pub fn start_watch() {
    let task = Handle::current().spawn(watch_account());
    if let Some(old) = ACCOUNT_WATCH.lock().unwrap().replace(task) {
        old.abort();
    }
}

pub fn stop_watch() {
    if let Some(task) = ACCOUNT_WATCH.lock().unwrap().take() {
        task.abort();
    }
}

/// Follows every sync's room updates until the client goes away.
async fn watch_account() {
    let post = |kind| {
        Cx::post_action(A2AppAccountWatchEvent { kind });
        SignalToUI::set_ui_signal();
    };
    let ended: Result<(), String> = async {
        let client = get_client().ok_or("not logged in")?;
        let mut updates = client.subscribe_to_all_room_updates();
        let totals = || client.joined_rooms().iter().fold((0, 0), |(unread, mentions), room| {
            (unread + room.num_unread_messages(), mentions + room.num_unread_mentions())
        });
        // What the room list shows per room; a sync that changes none of it
        // (a new message that is already read, say) is not a "changed" room.
        let listing = |room: &Room| (
            room.cached_display_name().map(|n| n.to_string()),
            room.num_unread_messages(),
            room.num_unread_mentions(),
            room.is_favourite(),
            room.is_low_priority(),
        );
        let mut joined: HashMap<OwnedRoomId, Listing> = client.joined_rooms().iter()
            .map(|r| (r.room_id().to_owned(), listing(r)))
            .collect();
        let mut invited: HashSet<OwnedRoomId> = client.invited_rooms().iter().map(|r| r.room_id().to_owned()).collect();
        let mut last_totals = totals();
        loop {
            let update = match updates.recv().await {
                Ok(update) => update,
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => return Ok(()),
            };
            let mut newly_joined = Vec::new();
            let mut changed = Vec::new();
            for room_id in update.joined.into_keys() {
                invited.remove(&room_id);
                let Some(room) = client.get_room(&room_id) else { continue };
                let now = listing(&room);
                match joined.insert(room_id.clone(), now.clone()) {
                    None => newly_joined.push(room_id),
                    Some(before) if before != now => changed.push(room_id),
                    Some(_) => {}
                }
            }
            let mut left = Vec::new();
            for room_id in update.left.into_keys() {
                invited.remove(&room_id);
                if joined.remove(&room_id).is_some() {
                    left.push(room_id);
                }
            }
            if !newly_joined.is_empty() || !left.is_empty() || !changed.is_empty() {
                post(AccountWatchKind::RoomsChanged { joined: newly_joined, left, changed });
            }
            for room_id in update.invited.into_keys() {
                if !invited.insert(room_id.clone()) { continue }
                let Some(room) = client.get_room(&room_id) else { continue };
                let Ok(invite) = room.invite_details().await else { continue };
                let inviter_name = invite.inviter.as_ref()
                    .and_then(|m| m.display_name().map(str::to_string))
                    .unwrap_or_else(|| invite.inviter_id.localpart().to_string());
                let name = room.display_name().await
                    .map(|n| n.to_string())
                    .unwrap_or_else(|_| room_id.to_string());
                post(AccountWatchKind::InviteReceived {
                    room_id,
                    name,
                    inviter: invite.inviter_id,
                    inviter_name,
                    is_space: room.is_space(),
                });
            }
            let now = totals();
            if now != last_totals {
                last_totals = now;
                post(AccountWatchKind::UnreadTotalsChanged { unread: now.0, mentions: now.1 });
            }
        }
    }.await;
    if let Err(e) = ended {
        log!("Account watch ended: {e}");
    }
}

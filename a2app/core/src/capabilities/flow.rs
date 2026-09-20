//! Explicit information-flow contracts for every implemented capability.
//!
//! Scope/access describe UI permissions, not information flow. New services
//! and hooks receive no authority until this table explicitly classifies them.

use super::Capability;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FlowSource {
    /// Host constants or this compartment's existing private state.
    None,
    AttachedRoom,
    TargetRoom,
    Account,
    /// Account metadata plus every listed app's source-code provenance.
    InstalledAppCode,
    /// The same metadata limited to apps declaring the IPC receiver surface.
    IpcAppCode,
    /// The sender is host-resolved and its label is transferred before delivery.
    Peer,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FlowPeer { Ipc, App, Agent, Generator }

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FlowOutput {
    /// Local UI or state confined to the current compartment.
    Local,
    External,
    TargetRoom,
    /// The actual HTTP origin; checked again by the host network worker.
    Network,
    /// Plaintext, caller-controlled arguments to the account's homeserver.
    MatrixServer,
    /// Search remains local unless {server: true} was requested.
    MatrixSearch,
    /// Pagination is local/server-derived unless the caller supplies {before}.
    MatrixPagination,
    /// Host-resolved recipient; runtime transfers labels before forwarding.
    Peer(FlowPeer),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FlowContract {
    pub source: FlowSource,
    pub output: FlowOutput,
    /// Room content or externally supplied strings can contain instructions.
    pub untrusted_content: bool,
    /// This effect needs an integrity check in addition to confidentiality.
    pub privileged_effect: bool,
}

impl Capability {
    pub fn flow_contract(&self) -> Option<FlowContract> { contract(self.id) }
}

/// There is deliberately no wildcard inference from names, groups or scopes.
pub fn contract(id: &str) -> Option<FlowContract> {
    use FlowOutput as O;
    use FlowSource as S;
    let (source, output, untrusted_content, privileged_effect) = match id {
        "permissions.query" | "permissions.request" | "storage.file.read"
        | "storage.file.write" | "storage.quota.read" | "timer.schedule"
        | "events.subscribe" | "events.unsubscribe" | "ui.pane.read"
        | "ui.pane.close" | "ui.pane.set_side" | "ui.pane.minimize"
        | "ui.pane.break_out" | "notifications.post" | "notifications.clear"
        | "on_app_resize" | "on_permissions_changed" | "on_focus_changed"
        | "on_surface_changed" | "background.complete" | "on_background"
            => (S::None, O::Local, false, false),
        "host.env.read" => (S::AttachedRoom, O::Local, false, false),
        "network.http" => (S::None, O::Network, false, false),
        "device.location.read" | "device.info.read" | "host.prefs.read"
        | "on_prefs_changed" | "on_navigation_changed" | "on_unread_totals_changed"
            => (S::Account, O::Local, false, false),
        "device.clipboard.read" | "device.files.pick"
            => (S::Account, O::Local, true, false),
        "device.auth.check" => (S::Account, O::Local, false, true),
        "device.clipboard.write" | "device.url.open" | "device.files.save"
        | "device.share" => (S::None, O::External, false, true),
        "ipc.send" | "ipc.self.send" => (S::Account, O::Peer(FlowPeer::Ipc), false, false),
        "ipc.post" => (S::None, O::Peer(FlowPeer::Ipc), false, false),
        "on_ipc_message" | "on_tool_call" => (S::Peer, O::Local, true, false),
        "ipc.apps.list" => (S::IpcAppCode, O::Local, false, false),
        "apps.list" => (S::InstalledAppCode, O::Local, false, false),
        "matrix.room.messages.read" | "matrix.room.members.read"
        | "matrix.room.pins.read" | "matrix.room.threads.read"
        | "matrix.room.info.read" | "matrix.room.unread.read"
        | "matrix.room.power_levels.read" | "matrix.room.link.create"
        | "matrix.room.successor.read" | "matrix.room.receipts.read"
        | "matrix.rooms.info.read" | "matrix.rooms.messages.read"
        | "matrix.space.info.read" | "matrix.space.rooms.list"
            => (S::TargetRoom, O::Local, true, false),
        "matrix.room.thread.read" | "matrix.room.event.read"
            => (S::TargetRoom, O::MatrixServer, true, false),
        "matrix.room.messages.paginate" => (S::TargetRoom, O::MatrixPagination, true, false),
        "matrix.room.messages.search" => (S::TargetRoom, O::MatrixSearch, true, false),
        "matrix.rooms.messages.search" => (S::Account, O::MatrixSearch, true, false),
        "matrix.room.message.send" | "matrix.room.message.reply" | "matrix.room.thread.reply"
        | "matrix.rooms.message.send" | "matrix.room.typing.send" | "matrix.room.receipt.send"
        | "matrix.room.pin.set" | "matrix.room.favorite.set" | "matrix.room.low_priority.set"
        | "matrix.room.unread.set" | "matrix.room.invite.send" | "matrix.invites.respond"
            => (S::None, O::TargetRoom, false, true),
        "matrix.room.reaction.toggle" => (S::TargetRoom, O::TargetRoom, true, true),
        "matrix.rooms.join" => (S::None, O::MatrixServer, false, true),
        "matrix.user.dm.open" => (S::Account, O::MatrixServer, true, true),
        "matrix.profile.read" | "matrix.account.device.read" | "matrix.account.info.read"
        | "matrix.account.ignored.read" | "matrix.user.dm.find" | "matrix.rooms.list"
        | "matrix.rooms.search" | "matrix.rooms.invites.list" | "matrix.spaces.list"
        | "on_rooms_changed" | "on_invite_received" | "on_active_room_changed"
            => (S::Account, O::Local, true, false),
        "matrix.user.profile.read" | "matrix.rooms.preview.read"
            => (S::Account, O::MatrixServer, true, false),
        "on_room_info_changed" | "on_room_unread_changed" | "on_room_pins_changed"
        | "on_room_message" | "on_room_message_changed" | "on_room_reaction"
        | "on_room_typing" | "on_room_receipt" | "on_room_members_changed"
            => (S::AttachedRoom, O::Local, true, false),
        "host.nav.room" | "host.nav.event" | "host.nav.thread" | "host.nav.user"
        | "host.nav.space" | "host.nav.screen" | "host.nav.link"
            => (S::None, O::External, false, true),
        "host.composer.insert" | "host.composer.reply_to"
            => (S::None, O::TargetRoom, false, true),
        "host.nav.app" | "apps.launch" => (S::None, O::Peer(FlowPeer::App), false, true),
        "apps.generate" => (S::None, O::Peer(FlowPeer::Generator), false, true),
        "mcp.tools.register" | "mcp.tools.unregister" | "mcp.tools.result"
            => (S::None, O::Peer(FlowPeer::Agent), false, false),
        _ => return None,
    };
    Some(FlowContract { source, output, untrusted_content, privileged_effect })
}

impl FlowContract {
    /// Host identities and resolved targets are inputs; script payloads never
    /// supply their own source label or authority.
    pub fn source_labels(self, account: &str, attached_room: Option<&str>, target_room: Option<&str>) -> Result<crate::information_flow::Label, String> {
        use crate::information_flow::Source;
        let source = match self.source {
            FlowSource::None | FlowSource::Peer => None,
            FlowSource::AttachedRoom => attached_room.map(|room| Source::Room { account: account.into(), room: room.into() }),
            FlowSource::TargetRoom => Some(Source::Room { account: account.into(), room: target_room.ok_or("No source room.")?.into() }),
            FlowSource::Account | FlowSource::InstalledAppCode | FlowSource::IpcAppCode => Some(Source::Account { account: account.into() }),
        };
        Ok(source.into_iter().collect())
    }

    /// Resolve the immediate recipient. A peer route returns None because
    /// host runtime resolves the peer and performs a label-preserving transfer.
    pub fn recipient(self, account: &str, target_room: Option<&str>, args: &serde_json::Value, homeserver: Option<&str>) -> Result<Option<crate::information_flow::Recipient>, String> {
        use crate::information_flow::Recipient;
        let server = || Recipient::network_origin(homeserver.ok_or("No homeserver destination.")?);
        Ok(match self.output {
            FlowOutput::External => Some(Recipient::External),
            FlowOutput::TargetRoom => Some(Recipient::MatrixRoom { account: account.into(), room: target_room.ok_or("No destination room.")?.into() }),
            FlowOutput::Network => Some(Recipient::network_origin(args["url"].as_str().ok_or("No network destination.")?)?),
            FlowOutput::MatrixServer => Some(server()?),
            FlowOutput::MatrixSearch if args["server"].as_bool() == Some(true) => Some(server()?),
            FlowOutput::MatrixPagination if args["before"].as_str().is_some_and(|value| !value.trim().is_empty()) => Some(server()?),
            FlowOutput::Local | FlowOutput::Peer(_) | FlowOutput::MatrixSearch | FlowOutput::MatrixPagination => None,
        })
    }
}

impl FlowContract {
    pub fn sensitive_action(self, capability: &str, args: &serde_json::Value, target_room: Option<&str>) -> Option<crate::information_flow::SensitiveAction> {
        self.privileged_effect.then(|| crate::information_flow::SensitiveAction {
            kind: capability.into(),
            target: match self.output {
                FlowOutput::MatrixServer if capability == "matrix.user.dm.open" => "host",
                FlowOutput::TargetRoom | FlowOutput::MatrixServer => target_room.unwrap_or("host"),
                FlowOutput::External => "external",
                FlowOutput::Peer(_) => args["app_id"].as_str().unwrap_or("apps"),
                _ => "host",
            }.into(),
        })
    }
}

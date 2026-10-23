//! The timeline card shown for `rs.robius.a2app` events: a mini-app shared
//! into a room. Fixed height, so late changes never shift the viewport.

use makepad_widgets::*;
use matrix_sdk::ruma::{OwnedEventId, OwnedRoomId, events::room::MediaSource};

use a2app_core::{bundle, manifest::{MiniAppManifest, RunsIn}};
use crate::a2app::runtime::{imported_room_app, is_room_import_pending, A2AppOp};
use crate::shared::attachment_download::media_source_mxc;
use crate::media_cache::{MediaCache, MediaCacheEntry};
use crate::shared::popup_list::{enqueue_popup_notification, PopupKind};

/// The custom Matrix event type carrying a shared mini-app bundle.
pub const A2APP_EVENT_TYPE: &str = "rs.robius.a2app";

script_mod! {
    use mod.prelude.widgets.*
    use mod.widgets.*

    mod.widgets.MiniAppTimelineCard = set_type_default() do #(MiniAppTimelineCard::register_widget(vm)) {
        ..mod.widgets.RoundedView

        width: Fill,
        // Fixed height: a timeline item's height must never change after
        // its first draw.
        height: 118,
        flow: Down
        spacing: 8
        padding: Inset{top: 10, bottom: 10, left: 12, right: 12}
        margin: Inset{top: 4, bottom: 4, left: 10, right: 10}

        show_bg: true
        draw_bg +: {
            color: #F6F8F9
            border_color: (COLOR_DIVIDER_DARK)
            border_size: 1.0
            border_radius: 4.0
        }

        View {
            width: Fill, height: 52
            flow: Right
            spacing: 10
            align: Align{y: 0.5}
            card_glyph := Label {
                width: 40, height: Fit
                padding: 0, margin: 0
                draw_text +: {
                    text_style: TITLE_TEXT {font_size: 24},
                    color: #000
                }
            }
            View {
                width: Fill, height: Fit
                flow: Down
                spacing: 3
                card_name := Label {
                    width: Fill, height: Fit
                    padding: 0, margin: 0
                    max_lines: 1, text_overflow: Ellipsis
                    draw_text +: {
                        text_style: theme.font_bold {font_size: 12},
                        color: (COLOR_TEXT)
                    }
                }
                card_detail := Label {
                    width: Fill, height: Fit
                    padding: 0, margin: 0
                    max_lines: 2,
                    draw_text +: {
                        text_style: REGULAR_TEXT {font_size: 9.5},
                        color: (MESSAGE_TEXT_COLOR)
                    }
                }
            }
        }
        View {
            width: Fill, height: Fit
            flow: Right
            card_install_button := RobrixIconButton {
                padding: 8,
                draw_icon +: { svg: (ICON_IMPORT) }
                icon_walk: Walk{width: 14, height: 14, margin: Inset{right: 2}}
                text: "Add to my mini-apps"
            }
            card_run_button := RobrixPositiveIconButton {
                padding: 8,
                icon_walk: Walk{width: 0, height: 0, margin: 0}
                text: "Run in this room"
            }
        }
    }

    mod.widgets.MiniAppAttachmentAction = set_type_default() do #(MiniAppAttachmentAction::register_widget(vm)) {
        ..mod.widgets.View
        visible: false
        width: Fit, height: Fit

        attachment_app_button := RobrixIconButton {
            height: mod.widgets.SETTINGS_BUTTON_HEIGHT
            padding: Inset{left: 12, right: 12}
            margin: 0
            draw_icon.svg: (ICON_IMPORT)
            icon_walk: Walk{width: 16, height: 16}
            text: "Add to my mini-apps"
        }
    }

    mod.widgets.A2uiMessageAction = set_type_default() do #(A2uiMessageAction::register_widget(vm)) {
        ..mod.widgets.View
        visible: false
        width: Fill, height: Fit
        margin: Inset{top: 6, bottom: 2}

        // The surface runs as the A2UI app in the message's room, so its
        // buttons answer the message only as that app's permissions allow.
        a2ui_surface := RoundedView {
            width: Fill, height: Fit
            new_batch: true
            show_bg: true
            draw_bg +: {
                color: #xFFFFFF
                border_color: #xDDE1EA
                border_size: 1.0
                border_radius: 6.0
            }
            a2ui_splash := Splash { width: Fill, height: Fit }
        }
    }
}

/// A message's A2UI surface: its payload (with later updates applied) and
/// the message it answers.
#[derive(Clone)]
pub struct A2uiMessageContext {
    pub a2ui: String,
    pub room_id: OwnedRoomId,
    pub event_id: Option<OwnedEventId>,
}

/// Draws a message's A2UI surface inline, as the A2UI app in its room.
#[derive(Script, ScriptHook, Widget)]
pub struct A2uiMessageAction {
    #[deref] view: View,
    /// The event and payload the surface was last built from.
    #[rust] shown: Option<(Option<OwnedEventId>, String)>,
    /// The surface's isolate, registered as an A2UI app instance.
    #[rust] heap: Option<usize>,
    /// `mxc` images not yet loaded into the surface.
    #[rust] pending_images: Vec<a2app_core::a2ui::InlineImage>,
    /// The payload entries the surface was built from (see `Edits`).
    #[rust] entries: usize,
}

thread_local! {
    /// What users entered in inline surfaces, by account and message: kept across
    /// an agent's updates and row recycling, which both rebuild the surface.
    static A2UI_EDITS: std::cell::RefCell<std::collections::HashMap<(String, OwnedEventId), a2app_core::a2ui::Edits>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

impl Drop for A2uiMessageAction {
    fn drop(&mut self) {
        if let Some(heap) = self.heap.take() {
            crate::a2app::instances::forget_inline(heap);
        }
    }
}

impl Widget for A2uiMessageAction {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        self.view.handle_event(cx, event, scope);
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        self.view.draw_walk(cx, scope, walk)
    }
}

impl A2uiMessageActionRef {
    /// Shows the message's surface (or nothing), rebuilt only for a new
    /// payload since that resets the user's input, and loads its `mxc`
    /// images from `media_cache` as they arrive.
    pub fn populate(&self, cx: &mut Cx, context: Option<A2uiMessageContext>, media_cache: Option<&mut MediaCache>) {
        let Some(mut inner) = self.borrow_mut() else { return };
        let shown = context.as_ref().map(|context| (context.event_id.clone(), context.a2ui.clone()));
        if shown != inner.shown {
            let account = crate::sliding_sync::current_user_id().map(|id| id.to_string()).unwrap_or_default();
            // Keep what the user entered before the isolate is replaced; exporting
            // first also catches sliders, which have no change callback.
            let splash = inner.view.widget(cx, ids!(a2ui_splash));
            if let Some((Some(old_event), _)) = inner.shown.as_ref() {
                if let Some(mut splash) = splash.borrow_mut::<Splash>() {
                    splash.call_script_fn(cx, live_id!(a2ui_export), &[]);
                }
                if let Some(state) = find_child(&splash, LiveId::from_str(a2app_core::a2ui::STATE_WIDGET))
                    && let Some(edits) = a2app_core::a2ui::Edits::parse(&state.text(), inner.entries)
                {
                    A2UI_EDITS.with(|all| {
                        let mut all = all.borrow_mut();
                        if all.len() >= 256 { all.clear(); }
                        all.insert((account.clone(), old_event.clone()), edits);
                    });
                }
            }
            // Another message gets a fresh isolate, so a reply still in flight for
            // the old one can't land in the new one's surface.
            let old_event = inner.shown.as_ref().and_then(|(event_id, _)| event_id.clone());
            let new_event = shown.as_ref().and_then(|(event_id, _)| event_id.clone());
            if old_event != new_event {
                splash.set_text(cx, "");
            }
            let edits = new_event.as_ref()
                .and_then(|event_id| A2UI_EDITS.with(|all| all.borrow().get(&(account.clone(), event_id.clone())).cloned()));
            let app = shown.as_ref().and_then(|(event_id, payload)| a2app_core::a2ui::to_inline_splash(
                payload, event_id.as_ref().map_or("", |id| id.as_str()), edits.as_ref(),
            ).ok());
            inner.entries = app.as_ref().map_or(0, |app| app.entries);
            inner.view.set_visible(cx, app.is_some());
            if let Some(heap) = inner.heap.take() {
                crate::a2app::instances::forget_inline(heap);
            }
            let room_id = context.as_ref().map(|context| context.room_id.clone());
            let splash = inner.view.widget(cx, ids!(a2ui_splash));
            if let Some(mut splash) = splash.borrow_mut::<Splash>() {
                // I/O policy can only be chosen before the isolate exists; it then persists.
                if splash.isolate_heap_key(cx).is_none() {
                    splash.set_host_io_only(true);
                    splash.set_allow_net(false);
                }
                splash.set_host_tag(cx, room_id.as_deref().map(|room| a2app_core::manifest::instance_tag(a2app_core::a2ui::APP_ID, Some(room.as_str()))));
                splash.set_host_prompts(cx, true);
            }
            splash.set_text(cx, app.as_ref().map_or("", |app| app.source.as_str()));
            let heap = splash.borrow_mut::<Splash>().and_then(|mut splash| splash.isolate_heap_key(cx));
            if let (Some(heap), Some(room_id), true) = (heap, room_id.as_ref(), app.is_some()) {
                match crate::a2app::instances::register_inline(heap, room_id) {
                    Ok(()) => inner.heap = Some(heap),
                    Err(e) => warning!("A2UI surface in {room_id} can't use host services: {e}"),
                }
            }
            inner.pending_images = app.map(|app| app.images).unwrap_or_default();
            inner.shown = shown;
        }
        if let Some(heap) = inner.heap {
            crate::a2app::instances::refresh_inline(heap);
        }
        drop(inner);
        if let Some(media_cache) = media_cache {
            self.load_pending_images(cx, media_cache);
        }
    }

    /// Loads the surface's `mxc` images that have arrived in `media_cache`;
    /// cheap when none are pending, so it can run on every draw.
    pub fn load_pending_images(&self, cx: &mut Cx, media_cache: &mut MediaCache) {
        let Some(mut inner) = self.borrow_mut() else { return };
        if inner.pending_images.is_empty() { return; }
        let splash = inner.view.widget(cx, ids!(a2ui_splash));
        inner.pending_images.retain(|image| {
            // An image is pending until it is both fetched and drawn into its widget.
            let Some(widget) = find_child(&splash, LiveId::from_str(&image.widget)) else { return true };
            let source = MediaSource::Plain(image.url.as_str().into());
            match media_cache.try_get_media_or_fetch(&source, crate::utils::MEDIA_THUMBNAIL_FORMAT.into()).0 {
                MediaCacheEntry::Loaded(data) => {
                    if let Err(e) = widget.as_image().load_image_from_data(cx, &data) {
                        warning!("Couldn't show A2UI image {}: {e:?}", image.url);
                    }
                    false
                }
                MediaCacheEntry::Failed(_) => false,
                MediaCacheEntry::Requested => true,
            }
        });
    }
}

#[derive(Script, ScriptHook, Widget)]
pub struct MiniAppTimelineCard {
    #[deref] view: View,
    /// The raw bundle text from the event, kept for import.
    #[rust] bundle_text: String,
    /// The already-installed app this bundle matches, if any.
    #[rust] installed_id: Option<String>,
    #[rust] room_id: Option<OwnedRoomId>,
    #[rust] event_id: Option<OwnedEventId>,
    #[rust] shared_at_unix: Option<u64>,
    #[rust] sender_id: String,
    #[rust] sender_name: String,
    #[rust] can_run_in_room: bool,
    #[rust] can_open: bool,
    #[rust] valid_bundle: bool,
    /// The bundle text last parsed and its result: parsing (an A2UI bundle is
    /// translated) is too costly to repeat on every draw.
    #[rust] parsed: Option<(String, Result<MiniAppManifest, String>)>,
}

impl Widget for MiniAppTimelineCard {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        self.view.handle_event(cx, event, scope);

        if let Event::Actions(actions) = event {
            if self.view.button(cx, ids!(card_install_button)).clicked(actions) {
                if self.bundle_text.is_empty() {
                    enqueue_popup_notification("This shared mini-app has no bundle content.", PopupKind::Error, Some(4.0));
                } else {
                    cx.action(A2AppOp::ImportRoomBundle {
                        text: self.bundle_text.clone(),
                        room_id: self.room_id.clone(),
                        event_id: self.event_id.clone(),
                        shared_at_unix: self.shared_at_unix,
                        sender_id: self.sender_id.clone(),
                        sender_name: self.sender_name.clone(),
                    });
                }
            }
            if self.view.button(cx, ids!(card_run_button)).clicked(actions)
                && let Some(room_id) = self.room_id.as_ref()
                && let Some(installed) = imported_room_app(room_id, self.event_id.as_deref(), "")
            {
                let (can_run_in_room, can_open) = launch_options(&installed, Some(room_id));
                open_imported_app(cx, installed.id, Some(room_id), can_run_in_room, can_open);
            }
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        self.view.draw_walk(cx, scope, walk)
    }
}

impl MiniAppTimelineCardRef {
    /// Populates the card from a shared bundle's raw text.
    /// Re-set on every draw, since timeline items get recycled.
    pub fn populate(&self, cx: &mut Cx, bundle_text: &str, sender_id: &str, sender_name: &str, room_id: Option<OwnedRoomId>, event_id: Option<OwnedEventId>, shared_at_unix: Option<u64>) {
        let Some(mut inner) = self.borrow_mut() else { return };
        inner.bundle_text = bundle_text.to_string();
        inner.room_id = room_id;
        inner.event_id = event_id;
        inner.shared_at_unix = shared_at_unix;
        inner.sender_id = sender_id.to_string();
        inner.sender_name = sender_name.to_string();
        inner.can_run_in_room = false;
        inner.can_open = false;
        inner.valid_bundle = false;
        if inner.parsed.as_ref().is_none_or(|(text, _)| text != bundle_text) {
            inner.parsed = Some((bundle_text.to_string(), bundle::parse(bundle_text)));
        }
        let parsed = inner.parsed.as_ref().map(|(_, parsed)| parsed.clone()).unwrap();
        match parsed {
            Ok(manifest) => {
                inner.valid_bundle = true;
                let installed = inner.room_id.as_ref().and_then(|room_id| {
                    imported_room_app(room_id, inner.event_id.as_deref(), "")
                });
                inner.installed_id = installed.as_ref().map(|app| app.id.clone());
                if let Some(installed) = installed.as_ref() {
                    (inner.can_run_in_room, inner.can_open) = launch_options(installed, inner.room_id.as_ref());
                }
                inner.view.label(cx, ids!(card_glyph)).set_text(cx, &manifest.icon);
                inner.view.label(cx, ids!(card_name))
                    .set_text(cx, &format!("{} · Splash mini-app", manifest.name));
                inner.view.label(cx, ids!(card_detail))
                    .set_text(cx, &format!("Shared by {sender_name}\nPermissions are approved separately."));
            }
            Err(e) => {
                inner.installed_id = None;
                inner.view.label(cx, ids!(card_glyph)).set_text(cx, "❓");
                inner.view.label(cx, ids!(card_name)).set_text(cx, "Shared mini-app (unreadable)");
                inner.view.label(cx, ids!(card_detail)).set_text(cx, &e);
            }
        }
        let installed = inner.installed_id.is_some();
        inner.view.button(cx, ids!(card_install_button)).set_visible(cx, !installed);
        inner.view.button(cx, ids!(card_install_button)).set_enabled(cx, inner.valid_bundle);
        inner.view.button(cx, ids!(card_run_button)).set_visible(cx, installed);
        inner.view.button(cx, ids!(card_run_button)).set_enabled(cx, inner.can_run_in_room || inner.can_open);
        inner.view.button(cx, ids!(card_run_button)).set_text(cx, launch_label(inner.can_run_in_room, inner.can_open));
    }
}

/// The attachment's origin, kept until the user explicitly adds the app.
#[derive(Clone)]
pub struct MiniAppAttachmentContext {
    pub media_source: MediaSource,
    pub filename: String,
    pub size: Option<u64>,
    pub room_id: OwnedRoomId,
    pub event_id: Option<OwnedEventId>,
    pub shared_at_unix: Option<u64>,
    pub sender_id: String,
    pub sender_name: String,
}

/// A room file attachment is importable only when its type identifies a mini-app.
pub fn is_mini_app_attachment(filename: &str, mimetype: Option<&str>) -> bool {
    let extension = filename.rsplit_once('.').map(|(_, extension)| extension);
    extension.is_some_and(|extension| extension.eq_ignore_ascii_case(bundle::BUNDLE_EXT)
        || extension.eq_ignore_ascii_case("splash")
        || extension.eq_ignore_ascii_case(a2app_core::a2ui::FILE_EXT))
        || mimetype.is_some_and(|mimetype| mimetype.split(';').next().is_some_and(|mime|
            mime.trim().eq_ignore_ascii_case("application/vnd.robius.splashapp+json")
            || a2app_core::a2ui::is_a2ui_mime(mime)))
}

#[derive(Script, ScriptHook, Widget)]
pub struct MiniAppAttachmentAction {
    #[deref] view: View,
    #[rust] context: Option<MiniAppAttachmentContext>,
}

impl Widget for MiniAppAttachmentAction {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        self.view.handle_event(cx, event, scope);
        if let Event::Actions(actions) = event
            && self.view.button(cx, ids!(attachment_app_button)).clicked(actions)
            && let Some(context) = self.context.as_ref()
        {
            let media_uri = media_source_mxc(&context.media_source).as_str();
            if let Some(installed) = imported_room_app(&context.room_id, context.event_id.as_deref(), media_uri) {
                let (can_run_in_room, can_open) = launch_options(&installed, Some(&context.room_id));
                open_imported_app(cx, installed.id, Some(&context.room_id), can_run_in_room, can_open);
            } else if !is_room_import_pending(media_uri) {
                cx.action(A2AppOp::ImportRoomAttachment {
                    media_source: context.media_source.clone(),
                    filename: context.filename.clone(),
                    size: context.size,
                    room_id: context.room_id.clone(),
                    event_id: context.event_id.clone(),
                    shared_at_unix: context.shared_at_unix,
                    sender_id: context.sender_id.clone(),
                    sender_name: context.sender_name.clone(),
                });
            }
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        if let Some(context) = self.context.as_ref() {
            let media_uri = media_source_mxc(&context.media_source).as_str();
            let installed = imported_room_app(&context.room_id, context.event_id.as_deref(), media_uri);
            let pending = is_room_import_pending(media_uri);
            let button = self.view.button(cx, ids!(attachment_app_button));
            if let Some(installed) = installed {
                let (can_run_in_room, can_open) = launch_options(&installed, Some(&context.room_id));
                button.set_text(cx, launch_label(can_run_in_room, can_open));
                button.set_enabled(cx, can_run_in_room || can_open);
            } else {
                button.set_text(cx, if pending { "Adding mini-app…" } else { "Add to my mini-apps" });
                button.set_enabled(cx, !pending);
            }
        }
        self.view.draw_walk(cx, scope, walk)
    }
}

impl MiniAppAttachmentActionRef {
    /// Replaces all attachment metadata whenever a timeline row is reused.
    pub fn populate(&self, cx: &mut Cx, context: Option<MiniAppAttachmentContext>) {
        let Some(mut inner) = self.borrow_mut() else { return };
        let visible = context.is_some();
        inner.context = context;
        inner.view.set_visible(cx, visible);
    }
}

/// The widget with `id` under `root`, found by walking its children: a fresh
/// Splash's widgets aren't in the widget tree until it is first drawn.
fn find_child(root: &WidgetRef, id: LiveId) -> Option<WidgetRef> {
    let mut found = None;
    root.children(&mut |child_id, child| {
        if found.is_none() {
            found = if child_id == id { Some(child.clone()) } else { find_child(&child, id) };
        }
    });
    found
}

fn launch_options(manifest: &MiniAppManifest, room_id: Option<&OwnedRoomId>) -> (bool, bool) {
    let can_run_in_room = room_id.is_some_and(|room_id| manifest.can_run_in_context(room_id.as_str(), false));
    let can_open = !can_run_in_room && manifest.runs_in() == RunsIn::Account
        && matches!(manifest.scope, a2app_core::manifest::A2AppScope::Account);
    (can_run_in_room, can_open)
}

fn launch_label(can_run_in_room: bool, can_open: bool) -> &'static str {
    if can_run_in_room { "Run in this room" }
    else if can_open { "Open mini-app" }
    else { "Added to my mini-apps" }
}

fn open_imported_app(cx: &mut Cx, app_id: String, room_id: Option<&OwnedRoomId>, can_run_in_room: bool, can_open: bool) {
    if can_run_in_room && let Some(room_id) = room_id {
        cx.action(A2AppOp::OpenInRoom { app_id, room_id: room_id.clone() });
    } else if can_open {
        cx.action(A2AppOp::OpenApp { app_id, room_id: None, in_room_pane: false });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_mini_app_file_types_offer_import() {
        for filename in ["poll.splashapp", "Poll.SPLASHAPP", "custom.splash"] {
            assert!(is_mini_app_attachment(filename, None), "{filename}");
        }
        assert!(is_mini_app_attachment("download", Some("application/vnd.robius.splashapp+json; charset=utf-8")));
        for filename in ["data.json", "splashapp.txt", "poll.splashapp.zip", "splashapp"] {
            assert!(!is_mini_app_attachment(filename, Some("application/json")), "{filename}");
        }
    }

    #[test]
    fn room_file_launch_respects_app_context() {
        let room_id = OwnedRoomId::try_from("!room:example.org").unwrap();
        let apps = a2app_core::builtin::builtin_apps();
        let app = |id: &str| apps.iter().find(|app| app.id == id).unwrap();
        assert_eq!(launch_options(app("room-peek"), Some(&room_id)), (true, false));
        assert_eq!(launch_options(app("spaces"), Some(&room_id)), (false, false));
        assert_eq!(launch_options(app("account"), Some(&room_id)), (true, false));
        assert_eq!(launch_options(app("account"), None), (false, true));
        let mut bound = app("account").clone();
        bound.scope = a2app_core::manifest::A2AppScope::Room { room_id: "!other:example.org".into() };
        assert_eq!(launch_options(&bound, Some(&room_id)), (false, false));
        bound.scope = a2app_core::manifest::A2AppScope::Room { room_id: room_id.to_string() };
        assert_eq!(launch_options(&bound, Some(&room_id)), (true, false));
    }

    #[test]
    fn file_import_requires_a_click_and_retains_room_provenance() {
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let widget = cx.with_vm(|vm| {
            makepad_widgets::script_mod(vm);
            makepad_code_editor::script_mod(vm);
            crate::shared::script_mod(vm, crate::settings::app_preferences::Appearance::Light);
            crate::a2app::script_mod(vm);
            let value = script_eval!(vm, { mod.widgets.MiniAppAttachmentAction {} });
            WidgetRef::script_from_value(vm, value)
        });
        let context = MiniAppAttachmentContext {
            media_source: MediaSource::Plain("mxc://example.org/poll".try_into().unwrap()),
            filename: "poll.splashapp".into(),
            size: Some(1234),
            room_id: "!room:example.org".try_into().unwrap(),
            event_id: Some("$file:example.org".try_into().unwrap()),
            shared_at_unix: Some(123456789),
            sender_id: "@author:example.org".into(),
            sender_name: "Original author".into(),
        };
        let populated = cx.capture_actions(|cx| widget.as_mini_app_attachment_action().populate(cx, Some(context)));
        assert!(!populated.iter().any(|action| action.downcast_ref::<A2AppOp>().is_some()),
            "displaying a shared file must not import or execute it");
        let mut action_widget = widget.borrow_mut::<MiniAppAttachmentAction>().unwrap();
        let button = action_widget.view.button(&cx, ids!(attachment_app_button)).widget_uid();
        let click = cx.capture_actions(|cx| cx.widget_action(button, ButtonAction::Clicked(Default::default())));
        let actions = cx.capture_actions(|cx| action_widget.handle_event(cx, &Event::Actions(click), &mut Scope::empty()));
        let ops = actions.iter().filter_map(|action| action.downcast_ref::<A2AppOp>()).collect::<Vec<_>>();
        assert_eq!(ops.len(), 1);
        assert!(matches!(ops[0], A2AppOp::ImportRoomAttachment {
            filename, room_id, event_id: Some(event_id), shared_at_unix: Some(123456789), sender_id, sender_name, size: Some(1234), ..
        } if filename == "poll.splashapp" && room_id.as_str() == "!room:example.org"
            && event_id.as_str() == "$file:example.org" && sender_id == "@author:example.org"
            && sender_name == "Original author"));
        drop(action_widget);

        widget.as_mini_app_attachment_action().populate(&mut cx, None);
        let mut recycled = widget.borrow_mut::<MiniAppAttachmentAction>().unwrap();
        let click = cx.capture_actions(|cx| cx.widget_action(button, ButtonAction::Clicked(Default::default())));
        let actions = cx.capture_actions(|cx| recycled.handle_event(cx, &Event::Actions(click), &mut Scope::empty()));
        assert!(!actions.iter().any(|action| action.downcast_ref::<A2AppOp>().is_some()),
            "a recycled ordinary message must not retain a previous mini-app import");
    }

    #[test]
    fn a2ui_messages_draw_inline_without_installing_anything() {
        fn has_text(root: &WidgetRef, text: &str) -> bool {
            let mut found = root.text() == text;
            root.children(&mut |_, child| found |= has_text(&child, text));
            found
        }
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let widget = cx.with_vm(|vm| {
            makepad_widgets::script_mod(vm);
            makepad_code_editor::script_mod(vm);
            crate::shared::script_mod(vm, crate::settings::app_preferences::Appearance::Light);
            crate::a2app::script_mod(vm);
            let value = script_eval!(vm, { mod.widgets.A2uiMessageAction {} });
            WidgetRef::script_from_value(vm, value)
        });
        let payload = r#"[{"version":"v0.9","createSurface":{"surfaceId":"main"}},
            {"version":"v0.9","updateComponents":{"surfaceId":"main","components":[
                {"id":"root","component":"Column","children":["title","send"]},
                {"id":"title","component":"Text","text":"Contact us","variant":"h2"},
                {"id":"send","component":"Button","child":"label","action":{"event":{"name":"go"}}},
                {"id":"label","component":"Text","text":"Send"}]}}]"#;
        let context = A2uiMessageContext {
            a2ui: payload.into(),
            room_id: "!room:example.org".try_into().unwrap(),
            event_id: Some("$a2ui:example.org".try_into().unwrap()),
        };
        let populated = cx.capture_actions(|cx| widget.as_a2ui_message_action().populate(cx, Some(context), None));
        assert!(!populated.iter().any(|action| action.downcast_ref::<A2AppOp>().is_some()),
            "drawing a message must not install or open an app");
        assert!(widget.visible());
        assert!(has_text(&widget.widget(&cx, ids!(a2ui_splash)), "Contact us"), "the surface is drawn inline");
        let heap = |cx: &mut Cx| widget.widget(cx, ids!(a2ui_splash)).borrow_mut::<Splash>().unwrap().isolate_heap_key(cx);
        let first = heap(&mut cx);

        // An update to the same message keeps its isolate; another message gets a fresh one.
        let updated = A2uiMessageContext {
            a2ui: format!("[{payload}, [{{\"version\":\"v0.9\",\"updateDataModel\":{{\"surfaceId\":\"main\",\"value\":{{}}}}}}]]"),
            room_id: "!room:example.org".try_into().unwrap(),
            event_id: Some("$a2ui:example.org".try_into().unwrap()),
        };
        widget.as_a2ui_message_action().populate(&mut cx, Some(updated), None);
        assert_eq!(heap(&mut cx), first);
        let other = A2uiMessageContext {
            a2ui: payload.into(),
            room_id: "!room:example.org".try_into().unwrap(),
            event_id: Some("$other:example.org".try_into().unwrap()),
        };
        widget.as_a2ui_message_action().populate(&mut cx, Some(other), None);
        assert!(heap(&mut cx).is_some() && heap(&mut cx) != first, "a reply for the old message can't reach the new one");

        let invalid = A2uiMessageContext {
            a2ui: "[1, 2]".into(), room_id: "!room:example.org".try_into().unwrap(), event_id: None,
        };
        widget.as_a2ui_message_action().populate(&mut cx, Some(invalid), None);
        assert!(!widget.visible(), "a payload with nothing to draw shows nothing");

        widget.as_a2ui_message_action().populate(&mut cx, None, None);
        assert!(!widget.visible() && !has_text(&widget.widget(&cx, ids!(a2ui_splash)), "Contact us"),
            "a recycled ordinary message drops the previous surface");
    }
}

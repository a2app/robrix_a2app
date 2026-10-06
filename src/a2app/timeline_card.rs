//! The timeline card shown for `rs.robius.a2app` events: a mini-app shared
//! into a room. Fixed height, so late changes never shift the viewport.

use makepad_widgets::*;
use matrix_sdk::ruma::{OwnedEventId, OwnedRoomId, events::room::MediaSource};

use a2app_core::{bundle, manifest::{MiniAppManifest, RunsIn}};
use crate::a2app::runtime::{imported_room_app, is_room_import_pending, A2AppOp};
use crate::shared::attachment_download::media_source_mxc;
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
        match bundle::parse(bundle_text) {
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
        || extension.eq_ignore_ascii_case("splash"))
        || mimetype.is_some_and(|mimetype| mimetype.split(';').next().is_some_and(|mime|
            mime.trim().eq_ignore_ascii_case("application/vnd.robius.splashapp+json")))
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
            crate::shared::script_mod(vm);
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
}

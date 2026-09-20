//! The Mini Apps management screen: create/modify apps with AI, list and
//! run installed apps, and manage each app's permissions, versions, and data.
//!
//! Several panes in one widget (list, per-app info, AI providers, source
//! viewer, diff, editor), toggled by visibility. All mutations go through
//! [`A2AppOp`] actions applied by [`crate::a2app::runtime`].

use std::borrow::Cow;
use std::cell::RefCell;

use makepad_widgets::*;
use makepad_code_editor::code_view::CodeViewWidgetExt;
use matrix_sdk::ruma::OwnedRoomId;

use a2app_core::diff::{line_diff, DiffLine};
use a2app_core::manifest::{A2AppScope, MiniAppId, MiniAppManifest, RunsIn};
use a2app_core::permissions::{Effective, GrantState, Permission, RoomScope, RoomAccess, RoomPolicyMode, PolicyDecision, agent_subject, agent_room_of, is_agent_subject};
use crate::a2app::permission_prompt::{PermissionScopeEditorWidgetExt, duration_label, network_scope_label};
use crate::a2app::data_sharing::DataSharingWidgetExt;
use crate::a2app::protection_inspector::{ProtectionInspectorAction, ProtectionInspectorWidgetExt};
use crate::a2app::background_tasks::{BackgroundTasksAction, BackgroundTasksWidgetExt};
use a2app_core::persistence;
use a2app_core::versions::AppVersion;

use crate::a2app::runtime::{room_display_name, with_a2app, A2AppOp, A2AppRuntimeAction};
use crate::home::rooms_list::RoomsListRef;
use crate::app::ConfirmDeleteAction;
use crate::shared::confirmation_modal::ConfirmationModalContent;
use crate::shared::popup_list::{enqueue_popup_notification, PopupKind};
use crate::shared::speech_text_input::SpeechTextInputWidgetExt;
use crate::shared::room_picker_modal::{RoomPickerContent, RoomPickerModalAction};

script_mod! {
    use mod.prelude.widgets.*
    use mod.widgets.*

    // One installed app in the list: icon, name, details, and actions.
    mod.widgets.MiniAppRow = set_type_default() do #(MiniAppRow::register_widget(vm)) {
        ..mod.widgets.RoundedView

        width: Fill, height: Fit
        flow: Right
        spacing: 12
        align: Align{y: 0.5}
        padding: Inset{top: 8, bottom: 8, left: 10, right: 8}

        show_bg: true
        draw_bg +: {
            color: (COLOR_PRIMARY)
            border_radius: 8.0
            border_size: 1.0
            border_color: (COLOR_DIVIDER)
        }

        // The app's glyph on a tile in its own tint.
        row_tile := RoundedView {
            width: 44, height: 44
            align: Align{x: 0.5, y: 0.5}
            show_bg: true
            draw_bg +: {
                color: (COLOR_BG_PREVIEW)
                border_radius: 10.0
            }
            row_glyph := Label {
                width: Fit, height: Fit
                padding: 0, margin: 0
                draw_text +: {
                    text_style: TITLE_TEXT {font_size: 20},
                    color: #000
                }
            }
        }
        View {
            width: Fill, height: Fit
            flow: Down
            spacing: 2
            row_name := Label {
                width: Fill, height: Fit
                padding: 0, margin: 0
                draw_text +: {
                    text_style: theme.font_bold {font_size: 12},
                    color: (COLOR_TEXT)
                }
            }
            row_detail := Label {
                width: Fill, height: Fit
                padding: 0, margin: 0
                draw_text +: {
                    text_style: REGULAR_TEXT {font_size: 10},
                    color: (MESSAGE_TEXT_COLOR)
                }
            }
            row_summary := Label {
                visible: false,
                width: Fill, height: Fit
                flow: Flow.Right{wrap: true}
                padding: 0, margin: 0
                draw_text +: {
                    text_style: REGULAR_TEXT {font_size: 10},
                    color: (COLOR_TEXT)
                }
            }
        }
        row_open_button := RobrixIconButton {
            padding: Inset{top: 7, bottom: 7, left: 14, right: 14},
            icon_walk: Walk{width: 0, height: 0, margin: 0}
            text: "Open"
        }
        row_settings_button := mod.widgets.MiniAppGhostButton {
            draw_icon +: { svg: (ICON_SETTINGS) }
        }
    }

    // One declared permission in the app info pane.
    mod.widgets.MiniAppPermissionRow = set_type_default() do #(MiniAppPermissionRow::register_widget(vm)) {
        ..mod.widgets.RoundedView

        width: Fill, height: Fit
        flow: Right
        spacing: 8
        align: Align{y: 0.5}
        padding: Inset{top: 6, bottom: 6, left: 10, right: 10}

        perm_glyph := Label {
            width: 26, height: Fit
            padding: 0, margin: 0
            draw_text +: {
                text_style: TITLE_TEXT {font_size: 14},
                color: #000
            }
        }
        View {
            width: Fill, height: Fit
            flow: Down
            spacing: 2
            perm_title := Label {
                width: Fill, height: Fit
                padding: 0, margin: 0
                draw_text +: {
                    text_style: theme.font_bold {font_size: 11},
                    color: (COLOR_TEXT)
                }
            }
            perm_blurb := Label {
                width: Fill, height: Fit
                flow: Flow.Right{wrap: true}
                padding: 0, margin: 0
                draw_text +: {
                    text_style: REGULAR_TEXT {font_size: 9.5},
                    color: (MESSAGE_TEXT_COLOR)
                }
            }
        }
        perm_state := Label {
            width: Fit, height: Fit
            padding: 0, margin: 0
            align: Align{x: 1.0}
            draw_text +: {
                text_style: theme.font_bold {font_size: 10.5},
                color: (COLOR_TEXT)
            }
        }
        perm_change_button := RobrixNeutralIconButton {
            padding: 8,
            icon_walk: Walk{width: 0, height: 0, margin: 0}
            text: "Change"
        }
    }

    // One capability under a permission group in the app info pane: the
    // single ability, its tags, and its own allow/block override.
    mod.widgets.MiniAppCapabilityRow = set_type_default() do #(MiniAppCapabilityRow::register_widget(vm)) {
        ..mod.widgets.RoundedView

        width: Fill, height: Fit
        flow: Right
        spacing: 8
        align: Align{y: 0.5}
        padding: Inset{top: 3, bottom: 3, left: 44, right: 10}

        View {
            width: Fill, height: Fit
            flow: Down
            spacing: 1
            cap_title := Label {
                width: Fill, height: Fit
                padding: 0, margin: 0
                draw_text +: {
                    text_style: REGULAR_TEXT {font_size: 10.5},
                    color: (COLOR_TEXT)
                }
            }
            cap_tags := Label {
                width: Fill, height: Fit
                padding: 0, margin: 0
                draw_text +: {
                    text_style: REGULAR_TEXT {font_size: 8.5},
                    color: (MESSAGE_TEXT_COLOR)
                }
            }
        }
        cap_state := Label {
            width: Fit, height: Fit
            padding: 0, margin: 0
            align: Align{x: 1.0}
            draw_text +: {
                text_style: REGULAR_TEXT {font_size: 9.5},
                color: (MESSAGE_TEXT_COLOR)
            }
        }
        cap_change_button := RobrixNeutralIconButton {
            padding: Inset{top: 4, bottom: 4, left: 8, right: 8},
            icon_walk: Walk{width: 0, height: 0, margin: 0}
            text: "Change"
        }
    }

    // One version in the app info pane.
    mod.widgets.MiniAppVersionRow = set_type_default() do #(MiniAppVersionRow::register_widget(vm)) {
        ..mod.widgets.RoundedView

        width: Fill, height: Fit
        flow: Flow.Right{wrap: true}
        spacing: 8
        align: Align{y: 0.5}
        padding: Inset{top: 4, bottom: 4, left: 10, right: 10}

        View {
            width: Fill{min: 180}, height: Fit
            flow: Down
            spacing: 1
            version_label := Label {
                width: Fill, height: Fit
                padding: 0, margin: 0
                draw_text +: {
                    text_style: REGULAR_TEXT {font_size: 10.5},
                    color: (COLOR_TEXT)
                }
            }
            version_note := Label {
                width: Fill, height: Fit
                padding: 0, margin: 0
                draw_text +: {
                    text_style: REGULAR_TEXT {font_size: 9.5},
                    color: (MESSAGE_TEXT_COLOR)
                }
            }
        }
        version_current := Label {
            visible: false,
            width: Fit, height: Fit
            padding: Inset{top: 2, bottom: 2, left: 6, right: 6}
            margin: 0
            draw_text +: {
                text_style: REGULAR_TEXT {font_size: 9.5},
                color: (COLOR_FG_ACCEPT_GREEN)
            }
            text: "Current"
        }
        version_use_button := RobrixIconButton {
            padding: 6,
            icon_walk: Walk{width: 0, height: 0, margin: 0}
            text: "Use"
        }
        version_diff_button := RobrixNeutralIconButton {
            padding: 6,
            icon_walk: Walk{width: 0, height: 0, margin: 0}
            text: "Diff"
        }
        version_view_button := RobrixNeutralIconButton {
            padding: 6,
            icon_walk: Walk{width: 0, height: 0, margin: 0}
            text: "View"
        }
    }

    // The word at the start of a manage-card row, centered on its buttons.
    mod.widgets.MiniAppGroupLabel = Label {
        width: 56, height: 30
        padding: Inset{top: 8, bottom: 0, left: 0, right: 0}, margin: 0
        draw_text +: {
            text_style: theme.font_bold {font_size: 10},
            color: (MESSAGE_TEXT_COLOR)
        }
    }

    mod.widgets.MiniAppRadio = RadioButton {
        width: 22, height: 22
        padding: 0, margin: 0
        align: Align{x: 0.5, y: 0.5}
        text: ""
        draw_bg +: {
            size: 18.0
            color: (COLOR_PRIMARY)
            color_hover: (COLOR_PRIMARY)
            color_active: (COLOR_PRIMARY)
            color_focus: (COLOR_PRIMARY)
            color_down: (COLOR_PRIMARY)
            border_color: (COLOR_SECONDARY_DARKER)
            border_color_hover: (COLOR_ACTIVE_PRIMARY)
            border_color_active: (COLOR_ACTIVE_PRIMARY_DARKER)
            border_color_focus: (COLOR_ACTIVE_PRIMARY_DARKER)
            border_color_down: (COLOR_ACTIVE_PRIMARY_DARKER)
            mark_color: vec4(0.0, 0.0, 0.0, 0.0)
            mark_color_active: (COLOR_ACTIVE_PRIMARY_DARKER)
        }
    }

    // A bulleted note, the way the settings screen explains a control.
    mod.widgets.MiniAppNote = Html {
        width: Fill, height: Fit
        flow: Flow.Right{wrap: true}
        padding: 0
        font_size: 10
        font_color: #666
        text_style_normal: MESSAGE_TEXT_STYLE { font_size: 10 }
    }

    // One AI provider in the providers pane.
    mod.widgets.MiniAppProviderRow = set_type_default() do #(MiniAppProviderRow::register_widget(vm)) {
        ..mod.widgets.RoundedView

        width: Fill, height: Fit
        flow: Right
        spacing: 8
        align: Align{y: 0.5}
        padding: Inset{top: 6, bottom: 6, left: 10, right: 10}

        // The gutter holds the row's one verb: pick a set-up provider, or add a key.
        View {
            width: 32, height: 32
            flow: Overlay
            align: Align{x: 0.5, y: 0.5}
            provider_pick := mod.widgets.MiniAppRadio {}
            provider_add := mod.widgets.MiniAppGhostButton {
                visible: false,
                draw_icon +: { svg: (ICON_ADD), color: (COLOR_ACTIVE_PRIMARY) }
            }
        }
        View {
            width: Fill, height: Fit
            flow: Down
            spacing: 1
            provider_name := Label {
                width: Fill, height: Fit
                padding: 0, margin: 0
                draw_text +: {
                    text_style: theme.font_bold {font_size: 11},
                    color: (COLOR_TEXT)
                }
            }
            provider_detail := Label {
                width: Fill, height: Fit
                flow: Flow.Right{wrap: true}
                padding: 0, margin: 0
                draw_text +: {
                    text_style: REGULAR_TEXT {font_size: 10},
                    color: (MESSAGE_TEXT_COLOR)
                }
            }
        }
        provider_replace_button := RobrixNeutralIconButton {
            visible: false,
            padding: 8,
            icon_walk: Walk{width: 0, height: 0, margin: 0}
            text: "Replace key"
        }
        provider_forget_button := mod.widgets.MiniAppGhostButton {
            visible: false,
            draw_icon +: { svg: (ICON_TRASH), color: (COLOR_FG_DANGER_RED) }
        }
    }


    mod.widgets.MiniAppAccessRuleRow = set_type_default() do #(MiniAppAccessRuleRow::register_widget(vm)) {
        width: Fill, height: Fit, flow: Right, spacing: 8, padding: 8, align: Align{y: 0.5}
        rule_label := mod.widgets.PermissionOptionLabel {}
        rule_edit := RobrixNeutralIconButton {
            padding: 6, icon_walk: Walk{width: 0, height: 0, margin: 0}, text: "Edit"
        }
        rule_remove := RobrixNegativeIconButton {
            padding: 6, icon_walk: Walk{width: 0, height: 0, margin: 0}, text: "Remove"
        }
    }

    mod.widgets.MiniAppsScreen = #(MiniAppsScreen::register_widget(vm)) {
        width: Fill, height: Fill
        flow: Overlay

        list_pane := ScrollYView {
            width: Fill, height: Fill
            flow: Down
            spacing: 10
            padding: 15

            TitleLabel { text: "Mini Apps" }
            Label {
                width: Fill, height: Fit
                flow: Flow.Right{wrap: true}
                draw_text +: {
                    text_style: REGULAR_TEXT {font_size: 10.5},
                    color: (MESSAGE_TEXT_COLOR)
                }
                text: "Small sandboxed apps that run inside Robrix. Each one gets only what you grant it."
            }
            RoundedView {
                width: Fill, height: Fit, flow: Down, spacing: 8, padding: 12
                show_bg: true
                draw_bg +: { color: #F4F6F9, border_radius: 8.0 }
                SubsectionLabel { text: "Room and space protection", margin: 0 }
                mod.widgets.PermissionOptionLabel {
                    text: "Applies to every mini-app and agent. Blocks always win, including over room or space allowlists. Allow rules skip permission prompts for declared room abilities."
                }
                mod.widgets.PermissionOptionLabel { text: "Read access to rooms" }
                global_read := mod.widgets.PermissionDropDown { labels: ["Ask unless a room or space rule allows", "Allow everywhere", "Only allowlisted rooms and spaces", "Block all room reads"] }
                mod.widgets.PermissionOptionLabel {
                    text: "Blocking reads prevents new access. To stop sharing data an app or agent already read, remove that source's data sharing rules too."
                }
                global_write_enabled := ToggleFlat {
                    width: Fill, height: Fit
                    padding: Inset{left: 15}
                    active: false
                    draw_bg +: { size: 21 }
                    text: "Enable room writes for mini-apps and agents"
                    draw_text +: { text_style: REGULAR_TEXT {font_size: 10.5}, color: (COLOR_TEXT) }
                }
                mod.widgets.PermissionOptionLabel {
                    text: "Turning this off blocks every room write and disables write controls below. Your saved write rules are kept for when you turn it on again."
                }
                mod.widgets.PermissionOptionLabel { text: "Write access when enabled" }
                global_write := mod.widgets.PermissionDropDown { labels: ["Ask unless a room or space rule allows", "Allow everywhere", "Only allowlisted rooms and spaces"] }
                protection_summary := mod.widgets.PermissionOptionLabel {}
                protection_status := mod.widgets.PermissionOptionLabel { visible: false }
                View {
                    width: Fill, height: Fit, flow: Flow.Right{wrap: true}, spacing: 8
                    protection_button := RobrixNeutralIconButton {
                        padding: 8, icon_walk: Walk{width: 0, height: 0, margin: 0}
                        text: "Manage room and space rules"
                    }
                    agent_permissions_button := RobrixNeutralIconButton {
                        padding: 8, icon_walk: Walk{width: 0, height: 0, margin: 0}
                        text: "Manage agent permissions"
                    }
                    inspect_protection_button := RobrixNeutralIconButton {
                        padding: 8, icon_walk: Walk{width: 0, height: 0, margin: 0}
                        text: "Inspect room protection"
                    }
                }
            }

            RoundedView {
                width: Fill, height: Fit, flow: Down, spacing: 8, padding: 12
                show_bg: true
                draw_bg +: { color: #F4F6F9, border_radius: 8.0 }
                SubsectionLabel { text: "Private data sharing", margin: 0 }
                mod.widgets.PermissionOptionLabel {
                    text: "Room and account data stay protected after an app or agent reads them. Choose which model services, websites or other rooms may receive data from each source. Internet permission alone does not permit sharing private data."
                }
                sharing_button := RobrixNeutralIconButton {
                    padding: 8, icon_walk: Walk{width: 0, height: 0, margin: 0}
                    text: "Manage data sharing rules"
                }
            }

            background_tasks_button := RobrixNeutralIconButton {
                padding: 10, icon_walk: Walk{width: 0, height: 0, margin: 0}
                text: "Background tasks"
            }

            create_section := RoundedView {
                width: Fill, height: Fit
                flow: Down
                spacing: 10
                padding: 14
                show_bg: true,
                draw_bg +: {
                    color: #F4F6F9
                    border_radius: 8.0
                }

                View {
                    width: Fill, height: Fit
                    flow: Right
                    spacing: 6
                    align: Align{y: 0.5}
                    Icon {
                        icon_walk: Walk{width: 16, height: 16, margin: 0}
                        draw_icon +: { svg: (ICON_SPARKLE), color: (COLOR_ROBRIX_PURPLE) }
                    }
                    SubsectionLabel { text: "Create or modify a mini-app", margin: 0 }
                }

                prompt_input := SpeechTextInput {
                    width: Fill
                    // Grows with the prompt, then scrolls. The cap lives here rather than on
                    // the inner input: TextInput reads its scroll threshold from the tightest
                    // `Fit{max}` on an *ancestor* turtle, skipping its own walk.
                    height: Fit { max: FitBound.Abs(250.0) }
                    text_padding: 12
                    mic_tooltip: "Describe the app by voice"

                    // Unlike the other speech inputs, this one grows, so the mic sits beside
                    // the first line instead of sinking with the bottom edge. The top inset
                    // centres it on that line while the prompt is still one line tall.
                    speech_overlay +: {
                        align: Align{x: 1.0, y: 0.0}
                        padding: Inset{top: 2, bottom: 0, left: 4, right: 6}
                    }
                    // `mic_clearance` only ever insets the scroll bar's bottom, which a
                    // top-mounted mic doesn't need, so it drops to the normal inset and the
                    // top inset below keeps the bar clear of the button instead.
                    mic_clearance: 3
                    scroll_bar_inset: Inset{top: 36, right: 4, bottom: 3}

                    text_input +: {
                        // Enter inserts a newline; the Generate button submits.
                        is_multiline: true
                        empty_text: "Describe a new mini-app or changes to one…"
                        draw_text +: { text_style: REGULAR_TEXT {font_size: 11.5} }
                    }
                }

                View {
                    width: Fill, height: Fit
                    flow: Flow.Right{wrap: true}
                    spacing: 8
                    align: Align{y: 0.5}

                    generate_button := RobrixPositiveIconButton {
                        padding: Inset{top: 9, bottom: 9, left: 14, right: 16},
                        draw_icon +: { svg: (ICON_SPARKLE) }
                        icon_walk: Walk{width: 16, height: 16, margin: Inset{right: 2}}
                        text: "Generate"
                    }
                    providers_button := RobrixNeutralIconButton {
                        padding: Inset{top: 9, bottom: 9, left: 12, right: 12},
                        icon_walk: Walk{width: 0, height: 0, margin: 0}
                        text: "Setup AI Providers"
                    }
                }

                // Says which installed app the text will rewrite, before
                // Generate is pressed, with a way to force a new app.
                intent_hint := View {
                    visible: false,
                    width: Fill, height: Fit
                    flow: Flow.Right{wrap: true}
                    spacing: 8
                    align: Align{y: 0.5}
                    intent_label := Label {
                        width: Fit, height: Fit
                        padding: 0, margin: 0
                        draw_text +: {
                            text_style: theme.font_bold {font_size: 10},
                            color: (COLOR_ACTIVE_PRIMARY)
                        }
                    }
                    intent_switch_button := RobrixNeutralIconButton {
                        padding: 6,
                        icon_walk: Walk{width: 0, height: 0, margin: 0}
                        text: "Create a new app instead"
                    }
                }

                console_section := View {
                    visible: false,
                    width: Fill, height: Fit
                    flow: Down
                    spacing: 6

                    View {
                        width: Fill, height: Fit
                        flow: Right
                        spacing: 8
                        align: Align{y: 0.5}
                        console_status := Label {
                            width: Fill, height: Fit
                            padding: 0, margin: 0
                            draw_text +: {
                                text_style: theme.font_bold {font_size: 10.5},
                                color: (COLOR_TEXT)
                            }
                        }
                        stop_button := RobrixNegativeIconButton {
                            padding: 8,
                            icon_walk: Walk{width: 0, height: 0, margin: 0}
                            text: "Stop"
                        }
                        retry_button := RobrixIconButton {
                            visible: false,
                            padding: 8,
                            icon_walk: Walk{width: 0, height: 0, margin: 0}
                            text: "Retry"
                        }
                        new_prompt_button := RobrixNeutralIconButton {
                            visible: false,
                            padding: 8,
                            icon_walk: Walk{width: 0, height: 0, margin: 0}
                            text: "New prompt"
                        }
                    }

                    console_list := PortalList {
                        width: Fill, height: 240
                        flow: Down
                        auto_tail: true

                        ConsoleLine := Label {
                            width: Fill, height: Fit
                            padding: Inset{top: 1, bottom: 1}
                            margin: 0
                            draw_text +: {
                                text_style: MESSAGE_TEXT_STYLE {font_size: 9.5},
                                color: (MESSAGE_TEXT_COLOR)
                            }
                        }
                    }
                }
            }

            SubsectionLabel { text: "Your mini-apps" }
            no_apps_label := Label {
                visible: false,
                width: Fill, height: Fit
                padding: 0, margin: Inset{left: 6}
                draw_text +: {
                    text_style: REGULAR_TEXT {font_size: 10.5},
                    color: (MESSAGE_TEXT_COLOR)
                }
                text: "No mini-apps yet. Create one above!"
            }
            apps_list := FlatList {
                width: Fill, height: Fit
                spacing: 4
                flow: Down

                mini_app_row := mod.widgets.MiniAppRow { }
            }

            SubsectionLabel { text: "Import" }
            View {
                width: Fill, height: Fit
                // no wrap: a wrapping Right flow can't lay out the Fill-width input
                flow: Right
                spacing: 8
                align: Align{y: 0.5}

                import_input := RobrixTextInput {
                    width: Fill { max: 500 }, height: Fit
                    empty_text: "Paste a .splashapp bundle or bare Splash source here…"
                }
                import_button := RobrixIconButton {
                    padding: 8,
                    draw_icon +: { svg: (ICON_IMPORT) }
                    icon_walk: Walk{width: 14, height: 14, margin: Inset{right: 2}}
                    text: "Install"
                }
            }
            View { width: Fill, height: 20 }
        }

        info_pane := ScrollYView {
            visible: false,
            width: Fill, height: Fill
            flow: Down
            spacing: 10
            padding: 15

            View {
                width: Fill, height: Fit
                flow: Right
                spacing: 10
                align: Align{y: 0.5}

                info_back_button := RobrixNeutralIconButton {
                    padding: 8,
                    draw_icon +: { svg: (ICON_JUMP) }
                    icon_walk: Walk{width: 14, height: 14, margin: 0}
                    text: "Back"
                }
                info_glyph := Label {
                    width: Fit, height: Fit
                    padding: 0, margin: 0
                    draw_text +: {
                        text_style: TITLE_TEXT {font_size: 22},
                        color: #000
                    }
                }
                View {
                    width: Fill, height: Fit
                    flow: Down
                    spacing: 2
                    info_name := TitleLabel { margin: 0 }
                    info_kind := Label {
                        width: Fill, height: Fit
                        padding: 0, margin: 0
                        draw_text +: {
                            text_style: REGULAR_TEXT {font_size: 10},
                            color: (MESSAGE_TEXT_COLOR)
                        }
                    }
                }
            }

            restricted_banner := RoundedView {
                visible: false,
                width: Fill, height: Fit
                flow: Right
                spacing: 8
                align: Align{y: 0.5}
                padding: 10
                show_bg: true,
                draw_bg +: {
                    color: (COLOR_BG_DANGER_RED)
                    border_radius: 4.0
                }
                Label {
                    width: Fill, height: Fit
                    flow: Flow.Right{wrap: true}
                    padding: 0, margin: 0
                    draw_text +: {
                        text_style: REGULAR_TEXT {font_size: 10.5},
                        color: (COLOR_FG_DANGER_RED)
                    }
                    text: "This app was stopped for flooding the host with requests. It won't run until you let it."
                }
                unrestrict_button := RobrixIconButton {
                    padding: 8,
                    icon_walk: Walk{width: 0, height: 0, margin: 0}
                    text: "Let it run again"
                }
            }

            // What you do with the app most: run it here or in a room.
            View {
                width: Fill, height: Fit
                flow: Flow.Right{wrap: true}
                spacing: 8
                align: Align{y: 0.5}

                info_open_button := RobrixIconButton {
                    padding: Inset{top: 9, bottom: 9, left: 16, right: 16},
                    icon_walk: Walk{width: 0, height: 0, margin: 0}
                    text: "Open"
                }
                info_open_room_button := RobrixNeutralIconButton {
                    padding: Inset{top: 9, bottom: 9, left: 12, right: 12},
                    draw_icon +: { svg: (ICON_JOIN_ROOM) }
                    icon_walk: Walk{width: 14, height: 14, margin: Inset{right: 2}}
                    text: "Run in room…"
                }
                info_public_button := RobrixNeutralIconButton {
                    padding: Inset{top: 9, bottom: 9, left: 12, right: 12},
                    icon_walk: Walk{width: 0, height: 0, margin: 0}
                    text: "Open public instance"
                }
                info_force_stop_button := RobrixNegativeIconButton {
                    visible: false,
                    padding: Inset{top: 9, bottom: 9, left: 12, right: 12},
                    draw_icon +: { svg: (ICON_FORBIDDEN) }
                    icon_walk: Walk{width: 14, height: 14, margin: Inset{right: 2}}
                    text: "Force Stop"
                }
            }

            mod.widgets.PermissionOptionLabel {
                text: "A public instance has separate storage and cannot read room/account data, pasted text or private messages from other apps. Internet permissions still apply. Apps whose code contains private data cannot run publicly."
            }
            info_legacy_storage := mod.widgets.PermissionOptionLabel {
                visible: false
                text: "Older shared saved data is retained in its original storage folder. Isolated instances start with separate storage; old files are not automatically imported. Clear data also removes the retained old files."
            }

            // Everything else, grouped so a narrow window wraps each group
            // on its own line instead of scattering twelve buttons.
            manage_card := RoundedView {
                width: Fill, height: Fit
                flow: Down
                spacing: 8
                padding: 12
                show_bg: true,
                draw_bg +: {
                    color: #F4F6F9
                    border_radius: 8.0
                }

                View {
                    width: Fill, height: Fit
                    flow: Right
                    spacing: 8
                    mod.widgets.MiniAppGroupLabel { text: "Source" }
                    View {
                        width: Fill, height: Fit
                        flow: Flow.Right{wrap: true}
                        spacing: 8
                        align: Align{y: 0.5}
                        info_source_button := RobrixNeutralIconButton {
                            padding: 8,
                            draw_icon +: { svg: (ICON_VIEW_SOURCE) }
                            icon_walk: Walk{width: 14, height: 14, margin: Inset{right: 2}}
                            text: "View"
                        }
                        info_edit_button := RobrixNeutralIconButton {
                            padding: 8,
                            draw_icon +: { svg: (ICON_EDIT) }
                            icon_walk: Walk{width: 14, height: 14, margin: Inset{right: 2}}
                            text: "Edit"
                        }
                        info_modify_button := RobrixNeutralIconButton {
                            padding: 8,
                            draw_icon +: { svg: (ICON_SPARKLE) }
                            icon_walk: Walk{width: 14, height: 14, margin: Inset{right: 2}}
                            text: "Modify with AI"
                        }
                    }
                }
                View {
                    width: Fill, height: Fit
                    flow: Right
                    spacing: 8
                    mod.widgets.MiniAppGroupLabel { text: "Share" }
                    View {
                        width: Fill, height: Fit
                        flow: Flow.Right{wrap: true}
                        spacing: 8
                        align: Align{y: 0.5}
                        info_export_button := RobrixNeutralIconButton {
                            padding: 8,
                            draw_icon +: { svg: (ICON_COPY) }
                            icon_walk: Walk{width: 14, height: 14, margin: Inset{right: 2}}
                            text: "Export"
                        }
                        info_share_button := RobrixNeutralIconButton {
                            padding: 8,
                            draw_icon +: { svg: (ICON_SHARE) }
                            icon_walk: Walk{width: 14, height: 14, margin: Inset{right: 2}}
                            text: "Share…"
                        }
                        info_send_button := RobrixNeutralIconButton {
                            padding: 8,
                            draw_icon +: { svg: (ICON_SEND) }
                            icon_walk: Walk{width: 14, height: 14, margin: Inset{right: 2}}
                            text: "Send to room…"
                        }
                    }
                }
                View {
                    width: Fill, height: Fit
                    flow: Right
                    spacing: 8
                    mod.widgets.MiniAppGroupLabel { text: "Storage" }
                    View {
                        width: Fill, height: Fit
                        flow: Flow.Right{wrap: true}
                        spacing: 8
                        align: Align{y: 0.5}
                        info_storage_label := Label {
                            width: Fit, height: 30
                            padding: Inset{top: 8, bottom: 0, left: 0, right: 0}, margin: Inset{right: 4}
                            draw_text +: {
                                text_style: REGULAR_TEXT {font_size: 10},
                                color: (MESSAGE_TEXT_COLOR)
                            }
                        }
                        info_clear_data_button := RobrixNegativeIconButton {
                            padding: 8,
                            icon_walk: Walk{width: 0, height: 0, margin: 0}
                            text: "Clear Data"
                        }
                        info_reset_button := RobrixNegativeIconButton {
                            visible: false,
                            padding: 8,
                            draw_icon +: { svg: (ICON_ROTATE_CW) }
                            icon_walk: Walk{width: 14, height: 14, margin: Inset{right: 2}}
                            text: "Reset to stock"
                        }
                        info_uninstall_button := RobrixNegativeIconButton {
                            padding: 8,
                            draw_icon +: { svg: (ICON_TRASH) }
                            icon_walk: Walk{width: 14, height: 14, margin: Inset{right: 2}}
                            text: "Uninstall"
                        }
                    }
                }
            }

            SubsectionLabel { text: "Permissions" }
            perm_hint := Label {
                visible: false,
                width: Fill, height: Fit
                padding: 0, margin: Inset{left: 6}
                draw_text +: {
                    text_style: REGULAR_TEXT {font_size: 9.5},
                    color: (MESSAGE_TEXT_COLOR)
                }
                text: "This app is running: permission changes apply immediately; changing network access stops it, so open it again afterwards."
            }
            no_perms_label := Label {
                visible: false,
                width: Fill, height: Fit
                padding: 0, margin: Inset{left: 6}
                draw_text +: {
                    text_style: REGULAR_TEXT {font_size: 10.5},
                    color: (MESSAGE_TEXT_COLOR)
                }
                text: "This app declares no permissions: it can only draw its own UI and use its private storage."
            }
            perms_list := FlatList {
                width: Fill, height: Fit
                spacing: 2
                flow: Down

                permission_row := mod.widgets.MiniAppPermissionRow { }
                capability_row := mod.widgets.MiniAppCapabilityRow { }
            }

            SubsectionLabel { text: "Version history" }
            no_versions_label := Label {
                visible: false,
                width: Fill, height: Fit
                padding: 0, margin: Inset{left: 6}
                draw_text +: {
                    text_style: REGULAR_TEXT {font_size: 10.5},
                    color: (MESSAGE_TEXT_COLOR)
                }
                text: "No versions yet. Every AI change, hand edit, and switch is kept here, so you can move between them freely."
            }
            versions_list := FlatList {
                width: Fill, height: Fit
                spacing: 2
                flow: Down

                version_row := mod.widgets.MiniAppVersionRow { }
            }
        }

        access_pane := ScrollYView {
            visible: false
            width: Fill, height: Fill, flow: Down, spacing: 10, padding: 15
            View {
                width: Fill, height: Fit, flow: Right, spacing: 10, align: Align{y: 0.5}
                access_back := RobrixNeutralIconButton {
                    padding: 8, icon_walk: Walk{width: 0, height: 0, margin: 0}, text: "Back"
                }
                access_title := TitleLabel { width: Fill }
            }
            agent_selector := View {
                visible: false
                width: Fill, height: Fit, flow: Down, spacing: 5
                mod.widgets.PermissionOptionLabel { text: "Agent in room" }
                agent_choice := mod.widgets.PermissionDropDown {}
                mod.widgets.PermissionOptionLabel { text: "Permission or ability" }
                agent_ability := mod.widgets.PermissionDropDown {}
            }
            access_hint := mod.widgets.PermissionOptionLabel {}
            access_baseline := mod.widgets.PermissionOptionLabel {}
            access_scope := mod.widgets.PermissionScopeEditor {}
            policy_choices := View {
                width: Fill, height: Fit, flow: Down, spacing: 5
                mod.widgets.PermissionOptionLabel { text: "Read access for the selection" }
                policy_read := mod.widgets.PermissionDropDown { labels: ["Ask / no rule", "Allow", "Block"] }
                mod.widgets.PermissionOptionLabel { text: "Write access for the selection" }
                policy_write := mod.widgets.PermissionDropDown { labels: ["Ask / no rule", "Allow", "Block"] }
                policy_write_disabled := mod.widgets.PermissionOptionLabel {
                    visible: false
                    text: "All room writes are disabled on the Mini Apps main screen. Saved write rules are paused and will be kept when you edit read rules."
                }
            }
            View {
                width: Fill, height: Fit, flow: Flow.Right{wrap: true}, spacing: 8
                access_save := RobrixPositiveIconButton {
                    padding: 10, icon_walk: Walk{width: 0, height: 0, margin: 0}, text: "Allow selected"
                }
                access_ask := RobrixNeutralIconButton {
                    padding: 10, icon_walk: Walk{width: 0, height: 0, margin: 0}, text: "Ask / use default"
                }
                access_block := RobrixNegativeIconButton {
                    padding: 10, icon_walk: Walk{width: 0, height: 0, margin: 0}, text: "Block everywhere"
                }
            }
            SubsectionLabel { text: "Current rules" }
            access_empty := mod.widgets.PermissionOptionLabel { text: "No saved or session rules yet." }
            access_rules := FlatList {
                width: Fill, height: Fit, flow: Down, spacing: 5
                access_rule := mod.widgets.MiniAppAccessRuleRow {}
            }
        }

        background_pane := View {
            visible: false
            width: Fill, height: Fill, flow: Down
            View {
                width: Fill, height: Fit, flow: Right, spacing: 10, padding: 15
                background_back := RobrixNeutralIconButton {
                    padding: 8, icon_walk: Walk{width: 0, height: 0, margin: 0}, text: "Back"
                }
                TitleLabel { width: Fill, text: "Background tasks" }
            }
            background_tasks := mod.widgets.BackgroundTasks {}
        }

        inspector_pane := View {
            visible: false
            width: Fill, height: Fill, flow: Down
            View {
                width: Fill, height: Fit, flow: Right, spacing: 10, padding: 15
                inspector_back := RobrixNeutralIconButton {
                    padding: 8, icon_walk: Walk{width: 0, height: 0, margin: 0}, text: "Back"
                }
                TitleLabel { width: Fill, text: "Room protection inspector" }
            }
            protection_inspector := mod.widgets.ProtectionInspector {}
        }

        sharing_pane := View {
            visible: false
            width: Fill, height: Fill, flow: Down
            View {
                width: Fill, height: Fit, flow: Right, spacing: 10, padding: 15
                align: Align{y: 0.5}
                sharing_back_button := RobrixNeutralIconButton {
                    padding: 8, icon_walk: Walk{width: 0, height: 0, margin: 0}, text: "Back"
                }
                TitleLabel { text: "Private data sharing" }
            }
            sharing_editor := mod.widgets.DataSharing {}
        }

        providers_pane := ScrollYView {
            visible: false,
            width: Fill, height: Fill
            flow: Down
            spacing: 10
            padding: 15

            View {
                width: Fill, height: Fit
                flow: Right
                spacing: 10
                align: Align{y: 0.5}
                providers_back_button := RobrixNeutralIconButton {
                    padding: 8,
                    draw_icon +: { svg: (ICON_JUMP) }
                    icon_walk: Walk{width: 14, height: 14, margin: 0}
                    text: "Back"
                }
                TitleLabel { text: "Setup AI Providers" }
            }

            providers_blocker := Label {
                visible: false,
                width: Fill, height: Fit
                flow: Flow.Right{wrap: true}
                padding: 0, margin: Inset{left: 6}
                draw_text +: {
                    text_style: theme.font_bold {font_size: 11},
                    color: (COLOR_FG_DANGER_RED)
                }
            }

            key_entry_section := View {
                visible: false,
                width: Fill, height: Fit
                flow: Down
                spacing: 6

                key_entry_label := SubsectionLabel {}
                View {
                    width: Fill, height: Fit
                    flow: Right
                    spacing: 8
                    align: Align{y: 0.5}
                    key_input := RobrixTextInput {
                        width: Fill { max: 500 }, height: Fit
                        empty_text: "Paste the provider's API key…"
                        is_password: true
                    }
                    key_save_button := RobrixPositiveIconButton {
                        padding: 8,
                        icon_walk: Walk{width: 0, height: 0, margin: 0}
                        text: "Save"
                    }
                    key_cancel_button := RobrixNeutralIconButton {
                        padding: 8,
                        icon_walk: Walk{width: 0, height: 0, margin: 0}
                        text: "Cancel"
                    }
                }
            }

            providers_list := FlatList {
                width: Fill, height: Fit
                spacing: 2
                flow: Down

                provider_row := mod.widgets.MiniAppProviderRow { }
            }

            providers_note := Label {
                width: Fill, height: Fit
                flow: Flow.Right{wrap: true}
                padding: 0, margin: Inset{left: 6}
                draw_text +: {
                    text_style: REGULAR_TEXT {font_size: 9.5},
                    color: (MESSAGE_TEXT_COLOR)
                }
                text: "Robrix selects the model and keeps its credentials, including when a separate Octos process runs the agent. Configure a local Ollama provider and model explicitly in octos's config file."
            }
        }

        source_pane := View {
            visible: false,
            width: Fill, height: Fill
            flow: Down
            padding: 15
            spacing: 8

            show_bg: true,
            draw_bg.color: (COLOR_PRIMARY)

            View {
                width: Fill, height: Fit
                flow: Right
                spacing: 10
                align: Align{y: 0.5}
                source_title := TitleLabel { margin: 0 }
                source_close_button := RobrixNeutralIconButton {
                    spacing: 0,
                    padding: 8,
                    draw_icon +: { svg: (ICON_CLOSE) }
                    icon_walk: Walk{width: 14, height: 14, margin: 0}
                    text: ""
                }
            }
            source_code_view := mod.widgets.LightCodeView {
                editor +: {
                    width: Fill, height: Fill
                }
            }
        }

        diff_pane := View {
            visible: false,
            width: Fill, height: Fill
            flow: Down
            padding: 15
            spacing: 8

            show_bg: true,
            draw_bg.color: (COLOR_PRIMARY)

            View {
                width: Fill, height: Fit
                flow: Right
                spacing: 10
                align: Align{y: 0.5}
                diff_title := TitleLabel { margin: 0 }
                diff_close_button := RobrixNeutralIconButton {
                    spacing: 0,
                    padding: 8,
                    draw_icon +: { svg: (ICON_CLOSE) }
                    icon_walk: Walk{width: 14, height: 14, margin: 0}
                    text: ""
                }
            }
            diff_summary := Label {
                width: Fill, height: Fit
                padding: 0, margin: 0
                draw_text +: {
                    text_style: REGULAR_TEXT {font_size: 10},
                    color: (MESSAGE_TEXT_COLOR)
                }
            }
            diff_list := PortalList {
                width: Fill, height: Fill
                flow: Down

                DiffContext := View {
                    width: Fill, height: Fit
                    line := Label {
                        width: Fill, height: Fit
                        padding: Inset{top: 1, bottom: 1, left: 6, right: 6}
                        margin: 0
                        draw_text +: {
                            text_style: MESSAGE_TEXT_STYLE {font_size: 9.5},
                            color: (COLOR_TEXT)
                        }
                    }
                }
                DiffAdded := View {
                    width: Fill, height: Fit
                    show_bg: true
                    draw_bg +: { color: #xE6FFEC }
                    line := Label {
                        width: Fill, height: Fit
                        padding: Inset{top: 1, bottom: 1, left: 6, right: 6}
                        margin: 0
                        draw_text +: {
                            text_style: MESSAGE_TEXT_STYLE {font_size: 9.5},
                            color: #x116329
                        }
                    }
                }
                DiffRemoved := View {
                    width: Fill, height: Fit
                    show_bg: true
                    draw_bg +: { color: #xFFEBE9 }
                    line := Label {
                        width: Fill, height: Fit
                        padding: Inset{top: 1, bottom: 1, left: 6, right: 6}
                        margin: 0
                        draw_text +: {
                            text_style: MESSAGE_TEXT_STYLE {font_size: 9.5},
                            color: #x82071E
                        }
                    }
                }
            }
        }

        edit_pane := View {
            visible: false,
            width: Fill, height: Fill
            flow: Down
            padding: 15
            spacing: 8

            show_bg: true,
            draw_bg.color: (COLOR_PRIMARY)

            View {
                width: Fill, height: Fit
                flow: Right
                spacing: 10
                align: Align{y: 0.5}
                edit_title := TitleLabel { margin: 0 }
                edit_save_button := RobrixIconButton {
                    padding: 8,
                    icon_walk: Walk{width: 0, height: 0, margin: 0}
                    text: "Save as new version"
                }
                edit_cancel_button := RobrixNeutralIconButton {
                    spacing: 0,
                    padding: 8,
                    draw_icon +: { svg: (ICON_CLOSE) }
                    icon_walk: Walk{width: 14, height: 14, margin: 0}
                    text: ""
                }
            }
            Label {
                width: Fill, height: Fit
                padding: 0, margin: 0
                draw_text +: {
                    text_style: REGULAR_TEXT {font_size: 10},
                    color: (MESSAGE_TEXT_COLOR)
                }
                text: "Saving keeps the previous version. A running copy stops; open it again to run the new one."
            }
            source_editor := mod.widgets.LightCodeEditor {
                editor +: {
                    width: Fill, height: Fill
                }
            }
        }
    }
}

/// Actions emitted by the per-row widgets, applied by the screen.
#[derive(Clone, Debug, Default)]
pub enum MiniAppsScreenAction {
    CyclePermission { app_id: MiniAppId, perm: Permission },
    EditAccessRule(AccessRuleKey),
    RemoveAccessRule(AccessRuleKey),
    CycleCapability { app_id: MiniAppId, cap_id: String },
    UseVersion { app_id: MiniAppId, stamp: String },
    DiffVersion(String),
    ViewVersion(String),
    ProviderUse(String),
    ProviderEnterKey(String),
    ProviderForget(String),
    #[default]
    None,
}

// -----------------------------------------------------------------------
// Row widgets
// -----------------------------------------------------------------------

/// Scoped to the row widget so other screens and pickers cannot open the same app.
#[derive(Clone, Debug, Default)]
pub(super) enum MiniAppRowAction {
    OpenApp(MiniAppId),
    ShowInfo(MiniAppId),
    #[default]
    None,
}

#[derive(Script, ScriptHook, Widget)]
pub struct MiniAppRow {
    #[deref] view: View,
    #[rust] app_id: MiniAppId,
}

impl Widget for MiniAppRow {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        self.view.handle_event(cx, event, scope);
        if let Event::Actions(actions) = event {
            if self.view.button(cx, ids!(row_open_button)).clicked(actions) {
                cx.widget_action(self.widget_uid(), MiniAppRowAction::OpenApp(self.app_id.clone()));
            } else if self.view.button(cx, ids!(row_settings_button)).clicked(actions) {
                cx.widget_action(self.widget_uid(), MiniAppRowAction::ShowInfo(self.app_id.clone()));
            }
        }
    }
    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        self.view.draw_walk(cx, scope, walk)
    }
}

/// One list row's texts, gathered per draw without cloning any app's source.
pub(super) struct MiniAppRowData {
    pub(super) id: MiniAppId,
    icon: String,
    name: String,
    /// Where it runs, plus running / stopped.
    detail: String,
    summary: String,
    tint: u32,
    pub(super) open_label: &'static str,
}

impl MiniAppRow {
    pub(super) fn populate(&mut self, cx: &mut Cx, row: &MiniAppRowData) {
        self.app_id = row.id.clone();
        self.view.label(cx, ids!(row_glyph)).set_text(cx, &row.icon);
        self.view.label(cx, ids!(row_name)).set_text(cx, &row.name);
        self.view.label(cx, ids!(row_detail)).set_text(cx, &row.detail);
        self.view.label(cx, ids!(row_summary)).set_text(cx, &row.summary);
        self.view.widget(cx, ids!(row_summary)).set_visible(cx, !row.summary.is_empty());
        self.view.button(cx, ids!(row_open_button)).set_text(cx, row.open_label);
        // The tile is the app's tint at a pastel strength.
        let tint = row.tint;
        let channel = |shift: u32| ((tint >> shift) & 0xff) as f32 / 255.0;
        let tile = vec4(channel(16), channel(8), channel(0), 0.16);
        let mut row_tile = self.view.view(cx, ids!(row_tile));
        script_apply_eval!(cx, row_tile, { draw_bg +: { color: #(tile) } });
    }
}

/// Shared presentation data for the management screen and the room/space picker.
/// Copy only row text, never the app source or its other manifest data.
pub(super) fn mini_app_rows(
    cx: &mut Cx,
    include: impl Fn(&MiniAppManifest) -> bool,
) -> Vec<MiniAppRowData> {
    with_a2app(|state| {
        state.registry.iter().filter(|m| include(m)).map(|m| {
            let mut detail = match (&m.scope, m.runs_in()) {
                (A2AppScope::Room { room_id }, _) => format!("Runs in {}", room_label(cx, room_id)),
                (_, RunsIn::Room) => String::from("Runs in a room"),
                (_, RunsIn::Rooms) => String::from("Works across your rooms"),
                (_, RunsIn::Spaces) => String::from("Works across your spaces"),
                (_, RunsIn::Account) => String::from("Account-wide"),
            };
            if state.permissions.is_restricted(&m.id) {
                detail.push_str(" · stopped for abuse");
            } else if state.is_running(&m.id) {
                detail.push_str(" · running");
            }
            MiniAppRowData {
                id: m.id.clone(),
                icon: m.icon.clone(),
                name: m.name.clone(),
                detail,
                summary: m.description.clone(),
                tint: m.tint,
                open_label: run_label(&m.scope, m.runs_in()),
            }
        }).collect()
    }).unwrap_or_default()
}

#[derive(Script, ScriptHook, Widget)]
pub struct MiniAppPermissionRow {
    #[deref] view: View,
    #[rust] app_id: MiniAppId,
    #[rust] perm: Option<Permission>,
}

impl Widget for MiniAppPermissionRow {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        self.view.handle_event(cx, event, scope);
        if let Event::Actions(actions) = event
            && self.view.button(cx, ids!(perm_change_button)).clicked(actions)
            && let Some(perm) = self.perm
        {
            cx.action(MiniAppsScreenAction::CyclePermission {
                app_id: self.app_id.clone(),
                perm,
            });
        }
    }
    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        self.view.draw_walk(cx, scope, walk)
    }
}

impl MiniAppPermissionRow {
    fn populate(&mut self, cx: &mut Cx, app_id: &str, perm: Permission, effective: Effective) {
        self.app_id = app_id.to_string();
        self.perm = Some(perm);
        self.view.label(cx, ids!(perm_glyph)).set_text(cx, perm.glyph());
        self.view.label(cx, ids!(perm_title)).set_text(cx, perm.title());
        self.view.label(cx, ids!(perm_blurb)).set_text(cx, perm.blurb());
        let (state_text, color) = match effective {
            Effective::Granted => ("Allowed", crate::shared::styles::COLOR_FG_ACCEPT_GREEN),
            Effective::NeedsPrompt => ("Asks", crate::shared::styles::COLOR_ACTIVE_PRIMARY),
            Effective::Denied => ("Denied", crate::shared::styles::COLOR_FG_DANGER_RED),
            Effective::Undeclared => ("Undeclared", crate::shared::styles::COLOR_FG_DISABLED),
        };
        let scoped_count = with_a2app(|state| state.permissions.scoped_grants(app_id).into_iter()
            .filter(|grant| grant.permission == perm.as_str()).count()).unwrap_or(0);
        let state_text = if scoped_count > 0 { format!("{state_text} · {scoped_count} scoped rules") } else { state_text.to_string() };
        let mut state_label = self.view.label(cx, ids!(perm_state));
        script_apply_eval!(cx, state_label, {
            text: #(state_text),
            draw_text +: { color: #(color) },
        });
    }
}

#[derive(Script, ScriptHook, Widget)]
pub struct MiniAppCapabilityRow {
    #[deref] view: View,
    #[rust] app_id: MiniAppId,
    #[rust] cap_id: String,
}

impl Widget for MiniAppCapabilityRow {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        self.view.handle_event(cx, event, scope);
        if let Event::Actions(actions) = event
            && self.view.button(cx, ids!(cap_change_button)).clicked(actions)
        {
            cx.action(MiniAppsScreenAction::CycleCapability {
                app_id: self.app_id.clone(),
                cap_id: self.cap_id.clone(),
            });
        }
    }
    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        self.view.draw_walk(cx, scope, walk)
    }
}

impl MiniAppCapabilityRow {
    fn populate(
        &mut self,
        cx: &mut Cx,
        app_id: &str,
        cap: &a2app_core::capabilities::Capability,
        own: GrantState,
        effective: Effective,
    ) {
        self.app_id = app_id.to_string();
        self.cap_id = cap.id.to_string();
        self.view.label(cx, ids!(cap_title)).set_text(cx, cap.title);
        self.view.label(cx, ids!(cap_tags)).set_text(cx, &cap.tags());
        // An explicit answer reads as such; otherwise it follows the group.
        let (state_text, color) = match (own, effective) {
            (GrantState::Granted, _) => (String::from("Allowed"), crate::shared::styles::COLOR_FG_ACCEPT_GREEN),
            (GrantState::Denied, _) => (String::from("Denied"), crate::shared::styles::COLOR_FG_DANGER_RED),
            (GrantState::Ask, Effective::Granted) => (String::from("Allowed · group"), crate::shared::styles::COLOR_FG_DISABLED),
            (GrantState::Ask, Effective::NeedsPrompt) => (String::from("Asks · group"), crate::shared::styles::COLOR_FG_DISABLED),
            (GrantState::Ask, _) => (String::from("Denied · group"), crate::shared::styles::COLOR_FG_DISABLED),
        };
        let scoped_count = with_a2app(|state| state.permissions.scoped_grants(app_id).into_iter()
            .filter(|grant| grant.capability.as_deref() == Some(cap.id)).count()).unwrap_or(0);
        let state_text = if scoped_count > 0 { format!("{state_text} · {scoped_count} scoped rules") } else { state_text };
        let mut state_label = self.view.label(cx, ids!(cap_state));
        script_apply_eval!(cx, state_label, {
            text: #(state_text),
            draw_text +: { color: #(color) },
        });
    }
}

#[derive(Script, ScriptHook, Widget)]
pub struct MiniAppVersionRow {
    #[deref] view: View,
    #[rust] app_id: MiniAppId,
    #[rust] stamp: String,
}

impl Widget for MiniAppVersionRow {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        self.view.handle_event(cx, event, scope);
        let Event::Actions(actions) = event else { return };
        if self.view.button(cx, ids!(version_use_button)).clicked(actions) {
            cx.action(MiniAppsScreenAction::UseVersion {
                app_id: self.app_id.clone(),
                stamp: self.stamp.clone(),
            });
        }
        if self.view.button(cx, ids!(version_diff_button)).clicked(actions) {
            cx.action(MiniAppsScreenAction::DiffVersion(self.stamp.clone()));
        }
        if self.view.button(cx, ids!(version_view_button)).clicked(actions) {
            cx.action(MiniAppsScreenAction::ViewVersion(self.stamp.clone()));
        }
    }
    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        self.view.draw_walk(cx, scope, walk)
    }
}

impl MiniAppVersionRow {
    fn populate(&mut self, cx: &mut Cx, app_id: &str, version: &AppVersion, current: bool) {
        self.app_id = app_id.to_string();
        self.stamp = version.stamp.clone();
        let when = a2app_core::versions::label_for(
            version.at_unix,
            crate::a2app::runtime::utc_offset_secs(),
        );
        self.view.label(cx, ids!(version_label))
            .set_text(cx, &format!("{when} · {}", version.origin.label()));
        // The note repeats the origin for stock and hand edits; skip it then.
        let note = self.view.label(cx, ids!(version_note));
        note.set_visible(cx, !version.note.is_empty() && version.note != version.origin.label());
        note.set_text(cx, &version.note);
        self.view.widget(cx, ids!(version_current)).set_visible(cx, current);
        self.view.widget(cx, ids!(version_use_button)).set_visible(cx, !current);
        self.view.widget(cx, ids!(version_diff_button)).set_visible(cx, !current);
    }
}

#[derive(Script, ScriptHook, Widget)]
pub struct MiniAppProviderRow {
    #[deref] view: View,
    #[rust] provider_id: String,
}

impl Widget for MiniAppProviderRow {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        self.view.handle_event(cx, event, scope);
        if let Event::Actions(actions) = event {
            if self.view.radio_button(cx, ids!(provider_pick)).clicked(actions) {
                cx.action(MiniAppsScreenAction::ProviderUse(self.provider_id.clone()));
            } else if self.view.button(cx, ids!(provider_add)).clicked(actions)
                || self.view.button(cx, ids!(provider_replace_button)).clicked(actions)
            {
                cx.action(MiniAppsScreenAction::ProviderEnterKey(self.provider_id.clone()));
            } else if self.view.button(cx, ids!(provider_forget_button)).clicked(actions) {
                cx.action(MiniAppsScreenAction::ProviderForget(self.provider_id.clone()));
            }
        }
    }
    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        self.view.draw_walk(cx, scope, walk)
    }
}

impl MiniAppProviderRow {
    fn populate(&mut self, cx: &mut Cx, id: &str, label: &str, detail: &str, has_key: bool, active: bool, editable: bool) {
        self.provider_id = id.to_string();
        self.view.label(cx, ids!(provider_name)).set_text(cx, label);
        self.view.label(cx, ids!(provider_detail)).set_text(cx, detail);
        self.view.widget(cx, ids!(provider_pick)).set_visible(cx, has_key);
        self.view.radio_button(cx, ids!(provider_pick)).set_active(cx, active, Animate::No);
        self.view.widget(cx, ids!(provider_add)).set_visible(cx, !has_key);
        self.view.widget(cx, ids!(provider_replace_button)).set_visible(cx, active && editable);
        self.view.widget(cx, ids!(provider_forget_button)).set_visible(cx, editable);
    }
}

/// The primary button's text for an app: bound and room apps run in a room,
/// everything else just opens.
fn run_label(scope: &A2AppScope, runs_in: RunsIn) -> &'static str {
    match (scope, runs_in) {
        (A2AppScope::Room { .. }, _) => "Run in room",
        (_, RunsIn::Room) => "Run in room…",
        _ => "Open",
    }
}

fn room_label(cx: &mut Cx, room_id: &str) -> String {
    cx.has_global::<RoomsListRef>()
        .then(|| room_display_name(cx.get_global::<RoomsListRef>(), room_id))
        .flatten()
        .unwrap_or_else(|| room_id.to_string())
}

// -----------------------------------------------------------------------
// The screen itself
// -----------------------------------------------------------------------

/// Which of the screen's panes is showing.
#[derive(Default, PartialEq)]
enum Pane {
    #[default]
    List,
    Info,
    Providers,
    Source,
    Diff,
    Edit,
    Access,
    Sharing,
    Inspector,
    Background,
}

#[derive(Script, ScriptHook, Widget)]
pub struct MiniAppsScreen {
    #[deref] view: View,
    #[rust] pane: Pane,
    /// The app shown in the info (or source, diff, editor) pane.
    #[rust] info_app: Option<MiniAppId>,
    /// Versions of the info app, newest first, refreshed when the pane opens
    /// or the runtime says they changed.
    #[rust] versions: Vec<AppVersion>,
    #[rust] current_stamp: Option<String>,
    /// A built-in whose working copy differs from its stock source.
    #[rust] reset_available: bool,
    /// What the diff pane shows.
    #[rust] diff_lines: Vec<DiffLine>,
    /// The provider awaiting a pasted key, if any.
    #[rust] key_entry: Option<String>,
    /// The installed app the create bar's text reads as a rewrite of, and
    /// whether the user overrode that to create a new app anyway.
    #[rust] modify_target: Option<(MiniAppId, String)>,
    #[rust] force_create: bool,
    #[rust] access_editor: Option<AccessEditor>,
    #[rust] agent_rooms: Vec<String>,
    #[rust] agent_abilities: Vec<(Permission, Option<String>, String)>,
}

impl Widget for MiniAppsScreen {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        self.view.handle_event(cx, event, scope);

        // Console output streams in on Signal events; keep it painting.
        if let Event::Signal = event
            && with_a2app(|state| state.console.active).unwrap_or(false)
        {
            self.view.redraw(cx);
        }

        let Event::Actions(actions) = event else { return };

        if let Some(index) = self.view.drop_down(cx, ids!(global_read)).changed(actions) {
            cx.action(global_policy_action(RoomAccess::Read, index));
        }
        if let Some(enabled) = self.view.check_box(cx, ids!(global_write_enabled)).changed(actions) {
            self.set_write_controls_enabled(cx, enabled);
            cx.action(A2AppOp::SetMatrixWrite(enabled));
        }
        if let Some(index) = self.view.drop_down(cx, ids!(global_write)).changed(actions)
            && !self.view.widget(cx, ids!(global_write)).disabled(cx)
        {
            cx.action(global_policy_action(RoomAccess::Write, index));
        }
        if self.view.button(cx, ids!(protection_button)).clicked(actions) {
            self.show_access_editor(cx, AccessEditor::Protection);
        }
        if self.view.button(cx, ids!(agent_permissions_button)).clicked(actions) {
            self.show_agent_permissions(cx);
        }
        if self.view.button(cx, ids!(inspect_protection_button)).clicked(actions) {
            self.view.protection_inspector(cx, ids!(protection_inspector)).configure(cx);
            self.set_pane(cx, Pane::Inspector);
        }
        if self.view.button(cx, ids!(inspector_back)).clicked(actions) {
            self.set_pane(cx, Pane::List);
        }
        if self.view.button(cx, ids!(background_tasks_button)).clicked(actions) {
            self.view.background_tasks(cx, ids!(background_tasks)).configure(cx);
            self.set_pane(cx, Pane::Background);
        }
        if self.view.button(cx, ids!(background_back)).clicked(actions) {
            self.set_pane(cx, Pane::List);
        }
        if self.view.button(cx, ids!(sharing_button)).clicked(actions) {
            self.view.data_sharing(cx, ids!(sharing_editor)).configure(cx);
            self.set_pane(cx, Pane::Sharing);
        }
        if self.view.button(cx, ids!(sharing_back_button)).clicked(actions) {
            self.set_pane(cx, Pane::List);
        }
        if self.view.drop_down(cx, ids!(agent_choice)).changed(actions).is_some()
            || self.view.drop_down(cx, ids!(agent_ability)).changed(actions).is_some()
        {
            self.select_agent_access(cx);
        }
        self.handle_access_editor(cx, actions);

        for (_, row) in self.view.flat_list(cx, ids!(apps_list)).items_with_actions(actions) {
            match actions.find_widget_action(row.widget_uid()).cast() {
                MiniAppRowAction::OpenApp(app_id) => {
                    self.open_app(cx, app_id);
                    break;
                }
                MiniAppRowAction::ShowInfo(app_id) => {
                    self.show_info(cx, app_id);
                    break;
                }
                MiniAppRowAction::None => {}
            }
        }

        for action in actions {
            if let Some(action) = action.downcast_ref::<BackgroundTasksAction>() {
                match action {
                    BackgroundTasksAction::OpenApp(binding) => {
                        if crate::a2app::information_flow::account().ok().as_deref() != Some(binding.account.as_str()) {
                            enqueue_popup_notification("The signed-in account changed. Reopen Background tasks before opening this app.", PopupKind::Warning, Some(5.0));
                            continue;
                        }
                        match background_app_open_action(binding) {
                            Ok(action) => cx.action(action),
                            Err(error) => enqueue_popup_notification(error, PopupKind::Warning, Some(5.0)),
                        }
                    }
                    BackgroundTasksAction::AppPermissions(app) => self.show_info(cx, app.clone()),
                    BackgroundTasksAction::Sharing => {
                        self.view.data_sharing(cx, ids!(sharing_editor)).configure(cx);
                        self.set_pane(cx, Pane::Sharing);
                    }
                }
                continue;
            }
            if let Some(action) = action.downcast_ref::<ProtectionInspectorAction>() {
                match action {
                    ProtectionInspectorAction::Global => self.set_pane(cx, Pane::List),
                    ProtectionInspectorAction::Rule(key) => self.edit_access_rule(cx, key),
                    ProtectionInspectorAction::Ability { subject, permission, capability, scope } => {
                        self.show_access_editor(cx, AccessEditor::App {
                            app_id: subject.clone(), perm: *permission, cap_id: capability.clone(),
                        });
                        self.view.permission_scope_editor(cx, ids!(access_scope)).set_scope(cx, scope);
                        self.view.widget(cx, ids!(agent_selector)).set_visible(cx, false);
                    }
                    ProtectionInspectorAction::SubjectInfo(subject) => {
                        if let Some(room) = agent_room_of(subject) {
                            #[cfg(unix)]
                            if let Ok(room) = OwnedRoomId::try_from(room) { cx.action(A2AppOp::AiRoomPanel(room)); }
                        } else { self.show_info(cx, subject.clone()); }
                    }
                    ProtectionInspectorAction::Sharing => {
                        self.view.data_sharing(cx, ids!(sharing_editor)).configure(cx);
                        self.set_pane(cx, Pane::Sharing);
                    }
                }
                continue;
            }
            // A /miniapp generation lands on this screen; make sure the
            // console (list pane) is showing, not a leftover info pane.
            if let Some(A2AppOp::StartGeneration { .. }) = action.downcast_ref::<A2AppOp>()
                && self.pane != Pane::List
            {
                self.set_pane(cx, Pane::List);
            }
            if let Some(A2AppRuntimeAction::Uninstalled(app_id)) = action.downcast_ref() {
                if self.info_app.as_deref() == Some(app_id.as_str()) {
                    self.set_pane(cx, Pane::List);
                }
                continue;
            }
            if let Some(A2AppRuntimeAction::VersionsChanged(app_id)) = action.downcast_ref()
                && self.info_app.as_ref() == Some(app_id)
            {
                self.refresh_info(cx);
                continue;
            }
            match action.downcast_ref::<MiniAppsScreenAction>() {
                Some(MiniAppsScreenAction::CyclePermission { app_id, perm }) => {
                    self.show_access_editor(cx, AccessEditor::App { app_id: app_id.clone(), perm: *perm, cap_id: None });
                    continue;
                }
                Some(MiniAppsScreenAction::CycleCapability { app_id, cap_id }) => {
                    if let Some(perm) = a2app_core::capabilities::by_id(cap_id).and_then(|cap| cap.group) {
                        self.show_access_editor(cx, AccessEditor::App { app_id: app_id.clone(), perm, cap_id: Some(cap_id.clone()) });
                    }
                    continue;
                }
                Some(MiniAppsScreenAction::EditAccessRule(key)) => {
                    self.edit_access_rule(cx, key);
                    continue;
                }
                Some(MiniAppsScreenAction::RemoveAccessRule(key)) => {
                    self.remove_access_rule(cx, key);
                    continue;
                }
                Some(MiniAppsScreenAction::UseVersion { app_id, stamp }) => {
                    cx.action(A2AppOp::SwitchVersion {
                        app_id: app_id.clone(),
                        stamp: stamp.clone(),
                    });
                    continue;
                }
                Some(MiniAppsScreenAction::DiffVersion(stamp)) => {
                    self.show_diff(cx, stamp);
                    continue;
                }
                Some(MiniAppsScreenAction::ViewVersion(stamp)) => {
                    self.show_version_source(cx, stamp);
                    continue;
                }
                Some(MiniAppsScreenAction::ProviderUse(id)) => {
                    self.use_provider(cx, id.clone());
                    continue;
                }
                Some(MiniAppsScreenAction::ProviderEnterKey(id)) => {
                    self.enter_key(cx, id.clone());
                    continue;
                }
                Some(MiniAppsScreenAction::ProviderForget(id)) => {
                    let label = a2app_agent::providers::label_for(id);
                    let id = id.clone();
                    self.confirm_delete(
                        cx,
                        format!("Forget the {label} key?"),
                        format!("The key Robrix saved for {label} is deleted. A key from your environment or from octos's own sign-in is never touched."),
                        "Forget key",
                        move |cx| {
                            match a2app_agent::providers::forget(&id) {
                                Ok(()) => enqueue_popup_notification(format!("Forgot the {label} key."), PopupKind::Success, Some(3.0)),
                                Err(e) => enqueue_popup_notification(e, PopupKind::Error, Some(5.0)),
                            }
                            cx.redraw_all();
                        },
                    );
                    continue;
                }
                Some(MiniAppsScreenAction::None) | None => {}
            }
        }

        // ----- list pane -----
        if let Some(text) = self.view.text_input(cx, ids!(prompt_input.text_input)).changed(actions) {
            self.reclassify(cx, &text);
        }
        if self.view.button(cx, ids!(intent_switch_button)).clicked(actions) {
            self.force_create = !self.force_create;
            self.show_intent_hint(cx);
        }
        if self.view.button(cx, ids!(generate_button)).clicked(actions) {
            let request = self.view.text_input(cx, ids!(prompt_input.text_input)).text();
            let request = request.trim().to_string();
            if request.is_empty() {
                enqueue_popup_notification("Describe the app you want first.", PopupKind::Warning, Some(3.0));
            } else {
                let op = match (&self.modify_target, self.force_create) {
                    (Some((app_id, _)), false) => A2AppOp::StartModify { app_id: app_id.clone(), request },
                    (Some(_), true) => A2AppOp::StartCreate { request, room_id: None },
                    (None, _) => A2AppOp::StartGeneration { request, room_id: None },
                };
                cx.action(op);
                self.view.redraw(cx);
            }
        }
        if self.view.button(cx, ids!(providers_button)).clicked(actions) {
            self.set_pane(cx, Pane::Providers);
        }
        if self.view.button(cx, ids!(stop_button)).clicked(actions) {
            cx.action(A2AppOp::CancelGeneration);
        }
        if self.view.button(cx, ids!(retry_button)).clicked(actions) {
            cx.action(A2AppOp::RetryGeneration);
        }
        if self.view.button(cx, ids!(new_prompt_button)).clicked(actions) {
            self.view.speech_text_input(cx, ids!(prompt_input)).set_text(cx, "");
            self.reclassify(cx, "");
            cx.action(A2AppOp::NewPrompt);
        }
        if self.view.button(cx, ids!(import_button)).clicked(actions) {
            let text = self.view.text_input(cx, ids!(import_input)).text();
            if text.trim().is_empty() {
                enqueue_popup_notification("Paste a bundle to import first.", PopupKind::Warning, Some(3.0));
            } else {
                self.view.text_input(cx, ids!(import_input)).set_text(cx, "");
                cx.action(A2AppOp::ImportText(text));
            }
        }

        // ----- info pane -----
        if self.view.button(cx, ids!(info_back_button)).clicked(actions) {
            self.set_pane(cx, Pane::List);
        }
        if let Some(app_id) = self.info_app.clone() {
            if self.view.button(cx, ids!(info_open_button)).clicked(actions) {
                self.open_app(cx, app_id.clone());
            }
            if self.view.button(cx, ids!(info_public_button)).clicked(actions) {
                cx.action(A2AppOp::OpenPublicApp(app_id.clone()));
            }
            if self.view.button(cx, ids!(info_source_button)).clicked(actions) {
                self.show_source(cx, &app_id);
            }
            if self.view.button(cx, ids!(info_edit_button)).clicked(actions) {
                self.show_editor(cx, &app_id);
            }
            if self.view.button(cx, ids!(info_export_button)).clicked(actions) {
                cx.action(A2AppOp::Export(app_id.clone()));
            }
            if self.view.button(cx, ids!(info_share_button)).clicked(actions) {
                cx.action(A2AppOp::ShareBundle(app_id.clone()));
            }
            if self.view.button(cx, ids!(info_send_button)).clicked(actions) {
                let name = self.info_name();
                let app = app_id.clone();
                self.pick_room(cx, format!("Send \"{name}\" to…"), move |cx, room_id| {
                    cx.action(A2AppOp::SendToRoom { app_id: app, room_id });
                });
            }
            if self.view.button(cx, ids!(info_open_room_button)).clicked(actions) {
                let name = self.info_name();
                let app = app_id.clone();
                self.pick_room(cx, format!("Open \"{name}\" in…"), move |cx, room_id| {
                    cx.action(A2AppOp::OpenInRoom { app_id: app, room_id });
                });
            }
            if self.view.button(cx, ids!(info_reset_button)).clicked(actions) {
                cx.action(A2AppOp::ResetToStock(app_id.clone()));
            }
            if self.view.button(cx, ids!(info_modify_button)).clicked(actions) {
                // Prefill the composer with a modify hint and go back to it.
                let name = with_a2app(|state| {
                    state.registry.get(&app_id).map(|a| a.name.clone())
                }).flatten().unwrap_or_else(|| app_id.clone());
                self.view.speech_text_input(cx, ids!(prompt_input))
                    .set_text(cx, &format!("Change the {name} app: "));
                self.set_pane(cx, Pane::List);
            }
            if self.view.button(cx, ids!(info_force_stop_button)).clicked(actions) {
                cx.action(A2AppOp::ForceStop(app_id.clone()));
            }
            if self.view.button(cx, ids!(info_clear_data_button)).clicked(actions) {
                let name = self.info_name();
                let app = app_id.clone();
                self.confirm_delete(
                    cx,
                    format!("Clear {name}'s data?"),
                    String::from("Everything the app saved in its private storage is deleted. The app itself stays installed."),
                    "Clear data",
                    move |cx| cx.action(A2AppOp::ClearData(app)),
                );
            }
            if self.view.button(cx, ids!(info_uninstall_button)).clicked(actions) {
                let name = self.info_name();
                let app = app_id.clone();
                self.confirm_delete(
                    cx,
                    format!("Uninstall {name}?"),
                    String::from("Its saved data and permissions are removed. The app's bundle is archived, so it can be brought back."),
                    "Uninstall",
                    move |cx| cx.action(A2AppOp::Uninstall(app)),
                );
            }
            if self.view.button(cx, ids!(unrestrict_button)).clicked(actions) {
                cx.action(A2AppOp::Unrestrict(app_id.clone()));
            }
        }

        // ----- providers pane -----
        if self.view.button(cx, ids!(providers_back_button)).clicked(actions) {
            self.set_pane(cx, Pane::List);
        }
        if self.view.button(cx, ids!(key_save_button)).clicked(actions)
            && let Some(provider) = self.key_entry.clone()
        {
            let key = self.view.text_input(cx, ids!(key_input)).text();
            let key = key.trim().to_string();
            if key.is_empty() {
                enqueue_popup_notification("Paste the key first.", PopupKind::Warning, Some(3.0));
            } else {
                match a2app_agent::providers::save_key(&provider, &key) {
                    Ok(()) => {
                        enqueue_popup_notification("Provider key saved.", PopupKind::Success, Some(3.0));
                        self.close_key_entry(cx);
                    }
                    Err(e) => enqueue_popup_notification(e, PopupKind::Error, Some(5.0)),
                }
            }
        }
        if self.view.button(cx, ids!(key_cancel_button)).clicked(actions) {
            self.close_key_entry(cx);
        }

        // ----- source, diff, and editor panes -----
        if self.view.button(cx, ids!(source_close_button)).clicked(actions)
            || self.view.button(cx, ids!(diff_close_button)).clicked(actions)
            || self.view.button(cx, ids!(edit_cancel_button)).clicked(actions)
        {
            self.set_pane(cx, Pane::Info);
        }
        if self.view.button(cx, ids!(edit_save_button)).clicked(actions)
            && let Some(app_id) = self.info_app.clone()
        {
            let source = self.view.code_view(cx, ids!(source_editor)).text();
            cx.action(A2AppOp::SaveSource { app_id, source });
            self.set_pane(cx, Pane::Info);
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        self.populate_before_draw(cx);

        // Resolved before the draw loop: once a list is mutably borrowed
        // below, a widget query for it would fail and return a zero uid.
        let perms_list_uid = self.view.widget(cx, ids!(perms_list)).widget_uid();
        let diff_list_uid = self.view.widget(cx, ids!(diff_list)).widget_uid();

        while let Some(subview) = self.view.draw_walk(cx, scope, walk).step() {
            let uid = subview.widget_uid();
            if let Some(mut list) = subview.as_flat_list().borrow_mut() {
                self.draw_flat_list(cx, uid, perms_list_uid, &mut list);
                continue;
            }
            if let Some(mut list) = subview.as_portal_list().borrow_mut() {
                if uid == diff_list_uid {
                    self.draw_diff_list(cx, &mut list);
                } else {
                    self.draw_console_list(cx, &mut list);
                }
            }
        }
        DrawStep::done()
    }
}

impl MiniAppsScreen {
    /// Re-reads what the create bar's text would do, the same way the
    /// runtime will when Generate is pressed.
    fn reclassify(&mut self, cx: &mut Cx, text: &str) {
        let apps: Vec<(MiniAppId, String)> = with_a2app(|state| {
            state.registry.iter().map(|a| (a.id.clone(), a.name.clone())).collect()
        }).unwrap_or_default();
        let target = match a2app_agent::intent::classify(text, &apps) {
            a2app_agent::intent::Intent::Modify(id) => {
                apps.iter().find(|(app_id, _)| *app_id == id).map(|(id, name)| (id.clone(), name.clone()))
            }
            a2app_agent::intent::Intent::Create => None,
        };
        if target != self.modify_target {
            self.force_create = false;
        }
        self.modify_target = target;
        self.show_intent_hint(cx);
    }

    fn show_intent_hint(&mut self, cx: &mut Cx) {
        let Some((_, name)) = &self.modify_target else {
            self.view.widget(cx, ids!(intent_hint)).set_visible(cx, false);
            self.view.redraw(cx);
            return;
        };
        let (label, switch) = if self.force_create {
            (format!("Creates a new app; \"{name}\" stays as it is"), format!("Rewrite \"{name}\" instead"))
        } else {
            (format!("Rewrites \"{name}\" (the current version is kept)"), String::from("Create a new app instead"))
        };
        self.view.label(cx, ids!(intent_label)).set_text(cx, &label);
        self.view.button(cx, ids!(intent_switch_button)).set_text(cx, &switch);
        self.view.widget(cx, ids!(intent_hint)).set_visible(cx, true);
        self.view.redraw(cx);
    }

    fn set_pane(&mut self, cx: &mut Cx, pane: Pane) {
        // A hidden editor would still get key events.
        if self.pane == Pane::Edit && pane != Pane::Edit {
            cx.set_key_focus(Area::Empty);
        }
        self.pane = pane;
        let show = |p: Pane| self.pane == p;
        self.view.widget(cx, ids!(list_pane)).set_visible(cx, show(Pane::List));
        self.view.widget(cx, ids!(info_pane)).set_visible(cx, show(Pane::Info));
        self.view.widget(cx, ids!(providers_pane)).set_visible(cx, show(Pane::Providers));
        self.view.widget(cx, ids!(source_pane)).set_visible(cx, show(Pane::Source));
        self.view.widget(cx, ids!(diff_pane)).set_visible(cx, show(Pane::Diff));
        self.view.widget(cx, ids!(edit_pane)).set_visible(cx, show(Pane::Edit));
        self.view.widget(cx, ids!(access_pane)).set_visible(cx, show(Pane::Access));
        self.view.widget(cx, ids!(sharing_pane)).set_visible(cx, show(Pane::Sharing));
        self.view.widget(cx, ids!(inspector_pane)).set_visible(cx, show(Pane::Inspector));
        self.view.widget(cx, ids!(background_pane)).set_visible(cx, show(Pane::Background));
        self.view.redraw(cx);
    }

    fn show_info(&mut self, cx: &mut Cx, app_id: MiniAppId) {
        self.info_app = Some(app_id);
        self.refresh_info(cx);
        self.set_pane(cx, Pane::Info);
    }

    /// Re-reads the info app's version list and stock state.
    fn refresh_info(&mut self, cx: &mut Cx) {
        let Some(app_id) = self.info_app.clone() else { return };
        self.versions = persistence::list_versions(&app_id);
        self.versions.reverse();
        let (current, builtin, source) = with_a2app(|state| {
            state.registry.get(&app_id).map(|m| (m.current_version.clone(), m.builtin, m.source.clone()))
        }).flatten().unwrap_or_default();
        self.current_stamp = current;
        self.reset_available = builtin
            && a2app_core::builtin::stock(&app_id).is_some_and(|stock| stock.source != source);
        self.view.redraw(cx);
    }

    fn info_name(&self) -> String {
        let app_id = self.info_app.clone().unwrap_or_default();
        with_a2app(|state| state.registry.get(&app_id).map(|a| a.name.clone()))
            .flatten()
            .unwrap_or(app_id)
    }

    /// Opens the room picker; `on_picked` gets the chosen room's id.
    /// Asks before anything is deleted; `then` runs only on confirmation.
    fn confirm_delete(&self, cx: &mut Cx, title: String, body: String, verb: &'static str, then: impl FnOnce(&mut Cx) + 'static) {
        cx.action(ConfirmDeleteAction::Show(RefCell::new(Some(ConfirmationModalContent {
            title_text: Cow::Owned(title),
            body_text: Cow::Owned(body),
            accept_button_text: Some(Cow::Borrowed(verb)),
            on_accept_clicked: Some(Box::new(then)),
            ..Default::default()
        }))));
    }

    fn pick_room(&self, cx: &mut Cx, title: String, on_picked: impl FnOnce(&mut Cx, OwnedRoomId) + 'static) {
        cx.action(RoomPickerModalAction::Show(RefCell::new(Some(RoomPickerContent {
            title: Cow::Owned(title),
            on_picked: Some(Box::new(move |cx, room| on_picked(cx, room.room_id().clone()))),
        }))));
    }

    fn show_source(&mut self, cx: &mut Cx, app_id: &str) {
        let Some(Some((name, source))) = with_a2app(|state| {
            state.registry.get(app_id).map(|a| (a.name.clone(), a.source.clone()))
        }) else { return };
        self.view.label(cx, ids!(source_title)).set_text(cx, &format!("{name} · Splash source"));
        self.view.code_view(cx, ids!(source_code_view)).set_text(cx, &source);
        self.set_pane(cx, Pane::Source);
    }

    /// The source pane, showing one archived version instead of the working copy.
    fn show_version_source(&mut self, cx: &mut Cx, stamp: &str) {
        let Some(app_id) = self.info_app.clone() else { return };
        let Some((version, source)) = persistence::load_version(&app_id, stamp) else {
            enqueue_popup_notification("Couldn't load that version.", PopupKind::Error, Some(4.0));
            return;
        };
        let when = a2app_core::versions::label_for(version.at_unix, crate::a2app::runtime::utc_offset_secs());
        self.view.label(cx, ids!(source_title))
            .set_text(cx, &format!("{} · version from {when}", version.name));
        self.view.code_view(cx, ids!(source_code_view)).set_text(cx, &source);
        self.set_pane(cx, Pane::Source);
    }

    /// The diff pane: an archived version against the working copy.
    fn show_diff(&mut self, cx: &mut Cx, stamp: &str) {
        let Some(app_id) = self.info_app.clone() else { return };
        let Some((version, old)) = persistence::load_version(&app_id, stamp) else {
            enqueue_popup_notification("Couldn't load that version.", PopupKind::Error, Some(4.0));
            return;
        };
        let Some(Some((name, current))) = with_a2app(|state| {
            state.registry.get(&app_id).map(|a| (a.name.clone(), a.source.clone()))
        }) else { return };
        self.diff_lines = line_diff(&old, &current);
        let added = self.diff_lines.iter().filter(|l| matches!(l, DiffLine::Added(_))).count();
        let removed = self.diff_lines.iter().filter(|l| matches!(l, DiffLine::Removed(_))).count();
        let when = a2app_core::versions::label_for(version.at_unix, crate::a2app::runtime::utc_offset_secs());
        self.view.label(cx, ids!(diff_title)).set_text(cx, &format!("{name} · changes since {when}"));
        let summary = if added == 0 && removed == 0 {
            String::from("The current source is identical to that version.")
        } else {
            format!("+{added} added, -{removed} removed. Red is that version, green is the current source.")
        };
        self.view.label(cx, ids!(diff_summary)).set_text(cx, &summary);
        self.set_pane(cx, Pane::Diff);
    }

    fn show_editor(&mut self, cx: &mut Cx, app_id: &str) {
        let Some(Some((name, source))) = with_a2app(|state| {
            state.registry.get(app_id).map(|a| (a.name.clone(), a.source.clone()))
        }) else { return };
        self.view.label(cx, ids!(edit_title)).set_text(cx, &format!("Edit {name}"));
        self.view.code_view(cx, ids!(source_editor)).set_text(cx, &source);
        self.set_pane(cx, Pane::Edit);
    }

    /// Opens the app where it belongs: a room-bound app in its room, a
    /// room app in a room the user picks, anything else in the host.
    /// Runs the app the way its scope calls for: in its bound room, in a
    /// room the user picks, or on its own.
    fn open_app(&mut self, cx: &mut Cx, app_id: MiniAppId) {
        let target = with_a2app(|state| {
            state.registry.get(&app_id).map(|m| (m.name.clone(), m.scope.clone(), m.runs_in()))
        }).flatten();
        match target {
            Some((_, A2AppScope::Room { room_id }, _)) => {
                if let Ok(room_id) = OwnedRoomId::try_from(room_id.as_str()) {
                    cx.action(A2AppOp::OpenInRoom { app_id, room_id });
                }
            }
            Some((name, _, RunsIn::Room)) => {
                self.pick_room(cx, format!("Run \"{name}\" in…"), move |cx, room_id| {
                    cx.action(A2AppOp::OpenInRoom { app_id, room_id });
                });
            }
            _ => cx.action(A2AppOp::OpenApp { app_id, room_id: None, in_room_pane: false }),
        }
    }

    fn use_provider(&mut self, cx: &mut Cx, provider_id: String) {
        let known = a2app_agent::providers::list()
            .into_iter()
            .find(|p| p.id == provider_id);
        let Some(p) = known else { return };
        if !p.active {
            match a2app_agent::providers::set_active(&provider_id) {
                Ok(()) => {
                    a2app_agent::providers::clear_session();
                    enqueue_popup_notification(
                        format!("{} selected for new AI sessions. Restart existing agents to use it.", p.label),
                        PopupKind::Success, Some(5.0),
                    );
                }
                Err(e) => enqueue_popup_notification(e, PopupKind::Error, Some(5.0)),
            }
        }
        self.view.redraw(cx);
    }

    fn enter_key(&mut self, cx: &mut Cx, provider_id: String) {
        self.view.label(cx, ids!(key_entry_label))
            .set_text(cx, &format!("API key for {}", a2app_agent::providers::label_for(&provider_id)));
        self.key_entry = Some(provider_id);
        self.view.widget(cx, ids!(key_entry_section)).set_visible(cx, true);
        self.view.text_input(cx, ids!(key_input)).set_text(cx, "");
        self.view.redraw(cx);
    }

    fn close_key_entry(&mut self, cx: &mut Cx) {
        self.key_entry = None;
        self.view.text_input(cx, ids!(key_input)).set_text(cx, "");
        self.view.widget(cx, ids!(key_entry_section)).set_visible(cx, false);
        self.view.redraw(cx);
    }

    /// Refreshes all code-set labels/visibility from the a2app state.
    fn populate_before_draw(&mut self, cx: &mut Cx2d) {
        match self.pane {
            Pane::List => {
                let (any_apps, console) = with_a2app(|state| {
                    (
                        state.registry.iter().next().is_some(),
                        (state.console.active, state.console.status.clone(),
                         state.generation.is_some(), state.failed_request.is_some()),
                    )
                }).unwrap_or((false, (false, String::new(), false, false)));
                self.view.widget(cx, ids!(no_apps_label)).set_visible(cx, !any_apps);
                let (read, read_mode, write, write_mode, write_enabled, rooms, spaces) = with_a2app(|state| (
                    state.permissions.global_policy(RoomAccess::Read), state.permissions.policy_mode(RoomAccess::Read),
                    state.permissions.write_policy_when_enabled(), state.permissions.policy_mode(RoomAccess::Write),
                    state.permissions.matrix_write(),
                    state.permissions.room_rules().len(), state.permissions.space_rules().len(),
                )).unwrap_or((PolicyDecision::Ask, RoomPolicyMode::Standard, PolicyDecision::Ask, RoomPolicyMode::Standard, false, 0, 0));
                self.view.drop_down(cx, ids!(global_read)).set_selected_item(cx, global_policy_index(read, read_mode));
                self.view.drop_down(cx, ids!(global_write)).set_selected_item(cx, global_policy_index(write, write_mode));
                self.set_write_controls_enabled(cx, write_enabled);
                self.view.label(cx, ids!(protection_summary)).set_text(cx, &format!("{rooms} room rules · {spaces} space rules. Rules are saved until you change them."));
                let status = with_a2app(|state| state.policy_spaces_status.clone()).flatten();
                self.view.widget(cx, ids!(protection_status)).set_visible(cx, spaces > 0 && status.is_some());
                if let Some(status) = status {
                    self.view.label(cx, ids!(protection_status)).set_text(cx, &status);
                }
                let (active, status, running, can_retry) = console;
                self.view.widget(cx, ids!(console_section)).set_visible(cx, active);
                if active {
                    self.view.label(cx, ids!(console_status)).set_text(cx, &status);
                    self.view.widget(cx, ids!(stop_button)).set_visible(cx, running);
                    self.view.widget(cx, ids!(retry_button)).set_visible(cx, !running && can_retry);
                    self.view.widget(cx, ids!(new_prompt_button)).set_visible(cx, !running);
                }
            }
            Pane::Info => {
                let Some(app_id) = self.info_app.clone() else { return };
                let info = with_a2app(|state| {
                    state.registry.get(&app_id).map(|m| (
                        m.icon.clone(),
                        m.name.clone(),
                        m.builtin,
                        m.scope.clone(),
                        m.runs_in(),
                        state.permissions.is_restricted(&app_id),
                    ))
                }).flatten();
                let Some((icon, name, builtin, scope, runs_in, restricted)) = info else { return };
                self.view.label(cx, ids!(info_glyph)).set_text(cx, &icon);
                self.view.label(cx, ids!(info_name)).set_text(cx, &name);
                let origin = if builtin { "Built-in mini-app" } else { "Your mini-app" };
                let place = match (&scope, runs_in) {
                    (A2AppScope::Room { room_id }, _) => format!("runs in {}", room_label(cx, room_id)),
                    (_, RunsIn::Room) => String::from("runs in a room"),
                    (_, RunsIn::Rooms) => String::from("works across your rooms"),
                    (_, RunsIn::Spaces) => String::from("works across your spaces"),
                    (_, RunsIn::Account) => String::from("account-wide"),
                };
                let kind = format!("{origin} · {place}");
                self.view.label(cx, ids!(info_kind)).set_text(cx, &kind);
                let open_label = run_label(&scope, runs_in);
                self.view.button(cx, ids!(info_open_button)).set_text(cx, open_label);
                self.view.widget(cx, ids!(info_open_room_button)).set_visible(cx, open_label == "Open");
                self.view.widget(cx, ids!(restricted_banner)).set_visible(cx, restricted);
                let running = crate::a2app::runtime::with_a2app(|state| {
                    state.is_running(&app_id)
                }).unwrap_or(false);
                self.view.widget(cx, ids!(info_force_stop_button)).set_visible(cx, running);
                self.view.widget(cx, ids!(perm_hint)).set_visible(cx, running);
                self.view.widget(cx, ids!(info_uninstall_button)).set_visible(cx, !builtin);
                let bytes = persistence::app_data_bytes(&app_id);
                let used = if bytes < 1024 {
                    format!("{bytes} bytes")
                } else if bytes < 1024 * 1024 {
                    format!("{:.1} KB", bytes as f64 / 1024.0)
                } else {
                    format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
                };
                self.view.label(cx, ids!(info_storage_label)).set_text(cx, &used);
                let legacy_data = std::fs::read_dir(a2app_core::app_sandbox_dir(&app_id))
                    .map(|mut entries| entries.next().is_some()).unwrap_or(false);
                self.view.widget(cx, ids!(info_legacy_storage)).set_visible(cx, legacy_data);
                let declares_any = with_a2app(|state| {
                    state.registry.get(&app_id).is_some_and(|m| !m.permissions.is_empty())
                }).unwrap_or(false);
                self.view.widget(cx, ids!(no_perms_label)).set_visible(cx, !declares_any);
                self.view.widget(cx, ids!(no_versions_label)).set_visible(cx, self.versions.is_empty());
                self.view.widget(cx, ids!(info_reset_button)).set_visible(cx, self.reset_available);
            }
            Pane::Providers => {
                let blocker = a2app_agent::blocker();
                self.view.widget(cx, ids!(providers_blocker)).set_visible(cx, blocker.is_some());
                if let Some(blocker) = blocker {
                    self.view.label(cx, ids!(providers_blocker)).set_text(cx, &blocker.headline());
                }
            }
            Pane::Access => {
                let write_enabled = with_a2app(|state| state.permissions.matrix_write()).unwrap_or(false);
                self.set_write_controls_enabled(cx, write_enabled);
                let empty = self.access_rows(cx).is_empty();
                self.view.widget(cx, ids!(access_empty)).set_visible(cx, empty);
                if matches!(self.access_editor, Some(AccessEditor::Protection)) {
                    let (read, write) = with_a2app(|state| (
                        global_policy_label(state.permissions.global_policy(RoomAccess::Read), state.permissions.policy_mode(RoomAccess::Read)),
                        global_policy_label(state.permissions.global_policy(RoomAccess::Write), state.permissions.policy_mode(RoomAccess::Write)),
                    )).unwrap_or(("Ask", "All writes disabled"));
                    self.view.label(cx, ids!(access_baseline)).set_text(cx, &format!(
                        "Global read: {} · Global write: {}. Change global defaults on the Mini Apps main screen; a global block overrides every allowance.",
                        read, write,
                    ));
                }
                if let Some(AccessEditor::App { app_id, perm, cap_id }) = &self.access_editor {
                    let (state, group) = with_a2app(|runtime| (
                        cap_id.as_deref().map(|id| runtime.permissions.capability_state(app_id, id))
                            .unwrap_or_else(|| runtime.permissions.state(app_id, *perm)),
                        runtime.permissions.state(app_id, *perm),
                    )).unwrap_or((GrantState::Ask, GrantState::Ask));
                    let baseline = match state {
                        GrantState::Granted => "Broad override: allowed everywhere.",
                        GrantState::Denied => "Broad override: blocked everywhere, including saved allowances.",
                        GrantState::Ask if cap_id.is_some() && group == GrantState::Granted => "Broad override: follows the permission group, which allows everywhere.",
                        GrantState::Ask if cap_id.is_some() && group == GrantState::Denied => "Broad override: follows the permission group, which blocks everywhere.",
                        GrantState::Ask => "Broad override: asks / uses defaults. Scoped allowances are listed below.",
                    };
                    self.view.label(cx, ids!(access_baseline)).set_text(cx, baseline);
                }
            }
            Pane::Source | Pane::Diff | Pane::Edit | Pane::Sharing | Pane::Inspector | Pane::Background => {}
        }
    }

    fn draw_flat_list(&mut self, cx: &mut Cx2d, uid: WidgetUid, perms_list_uid: WidgetUid, list: &mut FlatList) {
        // Which list this is depends on which pane is visible; hidden panes
        // don't draw, so only one of these runs per draw pass.
        match self.pane {
            Pane::List => {
                let rows = mini_app_rows(cx, |_| true);
                for row_data in &rows {
                    let item_live_id = LiveId::from_str(&row_data.id);
                    let Some(item) = list.item(cx, item_live_id, id!(mini_app_row)) else { continue };
                    if let Some(mut row) = item.borrow_mut::<MiniAppRow>() {
                        row.populate(cx, row_data);
                    }
                    item.draw_all(cx, &mut Scope::empty());
                }
            }
            Pane::Info => {
                let Some(app_id) = self.info_app.clone() else { return };
                // The perms and versions FlatLists both land here; tell them
                // apart by widget identity.
                if uid == perms_list_uid {
                    let rows: Vec<(Permission, Effective)> = with_a2app(|state| {
                        state.registry.get(&app_id).map(|m| {
                            m.permissions.iter()
                                .filter_map(|p| Permission::from_str(p))
                                .map(|p| (p, state.permissions.effective(m, p)))
                                .collect()
                        }).unwrap_or_default()
                    }).unwrap_or_default();
                    for (perm, effective) in rows {
                        let item_live_id = LiveId::from_str(perm.as_str());
                        let Some(item) = list.item(cx, item_live_id, id!(permission_row)) else { continue };
                        if let Some(mut row) = item.borrow_mut::<MiniAppPermissionRow>() {
                            row.populate(cx, &app_id, perm, effective);
                        }
                        item.draw_all(cx, &mut Scope::empty());

                        // The single abilities this group unlocks, each with
                        // its own override.
                        let caps: Vec<(&'static a2app_core::capabilities::Capability, GrantState, Effective)> =
                            with_a2app(|state| {
                                state.registry.get(&app_id).map(|m| {
                                    a2app_core::capabilities::in_group(perm)
                                        .filter(|c| c.is_available() && m.declares_capability(c))
                                        .map(|c| (
                                            c,
                                            state.permissions.capability_state(&app_id, c.id),
                                            state.permissions.effective_capability(m, c),
                                        ))
                                        .collect()
                                }).unwrap_or_default()
                            }).unwrap_or_default();
                        for (cap, own, cap_effective) in caps {
                            let cap_item_id = LiveId::from_str(cap.id);
                            let Some(item) = list.item(cx, cap_item_id, id!(capability_row)) else { continue };
                            if let Some(mut row) = item.borrow_mut::<MiniAppCapabilityRow>() {
                                row.populate(cx, &app_id, cap, own, cap_effective);
                            }
                            item.draw_all(cx, &mut Scope::empty());
                        }
                    }
                } else {
                    for version in &self.versions {
                        let item_live_id = LiveId::from_str(&version.stamp);
                        let Some(item) = list.item(cx, item_live_id, id!(version_row)) else { continue };
                        let current = self.current_stamp.as_deref() == Some(version.stamp.as_str());
                        if let Some(mut row) = item.borrow_mut::<MiniAppVersionRow>() {
                            row.populate(cx, &app_id, version, current);
                        }
                        item.draw_all(cx, &mut Scope::empty());
                    }
                }
            }
            Pane::Access => {
                for (index, (key, label)) in self.access_rows(cx).into_iter().enumerate() {
                    let Some(item) = list.item(cx, LiveId::from_str(&format!("access-{index}")), id!(access_rule)) else { continue };
                    if let Some(mut row) = item.borrow_mut::<MiniAppAccessRuleRow>() {
                        row.key = Some(key.clone());
                        row.view.label(cx, ids!(rule_label)).set_text(cx, &label);
                        row.view.widget(cx, ids!(rule_edit)).set_visible(cx, matches!(key, AccessRuleKey::Room(_) | AccessRuleKey::Space(_)));
                        let write_enabled = with_a2app(|state| state.permissions.matrix_write()).unwrap_or(false);
                        let read_only = !write_enabled && matches!(key, AccessRuleKey::Room(_) | AccessRuleKey::Space(_));
                        let has_read_rule = with_a2app(|state| match &key {
                            AccessRuleKey::Room(id) => state.permissions.room_rules().get(id),
                            AccessRuleKey::Space(id) => state.permissions.space_rules().get(id),
                            _ => None,
                        }.is_some_and(|policy| policy.read != PolicyDecision::Ask)).unwrap_or(false);
                        row.view.button(cx, ids!(rule_edit)).set_text(cx, if read_only { "Edit read rule" } else { "Edit" });
                        row.view.button(cx, ids!(rule_remove)).set_text(cx, if read_only { "Remove read rule" } else { "Remove" });
                        row.view.button(cx, ids!(rule_remove)).set_enabled(cx, !read_only || has_read_rule);
                    }
                    item.draw_all(cx, &mut Scope::empty());
                }
            }
            Pane::Providers => {
                let configured = a2app_agent::providers::list();
                let draw_row = |cx: &mut Cx2d, list: &mut FlatList, id: &str, label: &str,
                                    detail: &str, has_key: bool, active: bool, editable: bool| {
                    let item_live_id = LiveId::from_str(id);
                    let Some(item) = list.item(cx, item_live_id, id!(provider_row)) else { return };
                    if let Some(mut row) = item.borrow_mut::<MiniAppProviderRow>() {
                        row.populate(cx, id, label, detail, has_key, active, editable);
                    }
                    item.draw_all(cx, &mut Scope::empty());
                };
                // The full catalog first, each row showing its own state...
                for spec in a2app_agent::providers::CATALOG {
                    match configured.iter().find(|p| p.id == spec.id) {
                        Some(p) => draw_row(cx, list, spec.id, spec.label, &p.detail(), true, p.active, p.editable()),
                        None => draw_row(cx, list, spec.id, spec.label, "Not set up", false, false, false),
                    }
                }
                // ...then anything configured outside the catalog (a local
                // Ollama, or a ROBRIX_AGENT_CMD override).
                for p in configured.iter().filter(|p| !a2app_agent::providers::CATALOG.iter().any(|s| s.id == p.id)) {
                    draw_row(cx, list, &p.id, &p.label, &p.detail(), true, p.active, p.editable());
                }
            }
            Pane::Source | Pane::Diff | Pane::Edit | Pane::Sharing | Pane::Inspector | Pane::Background => {}
        }
    }

    fn draw_console_list(&mut self, cx: &mut Cx2d, list: &mut PortalList) {
        let count = with_a2app(|state| state.console.lines.len()).unwrap_or(0);
        list.set_item_range(cx, 0, count);
        while let Some(item_id) = list.next_visible_item(cx) {
            let Some(line) = with_a2app(|state| state.console.lines.get(item_id).cloned()).flatten() else { continue };
            let item = list.item(cx, item_id, id!(ConsoleLine));
            item.set_text(cx, &line);
            item.draw_all(cx, &mut Scope::empty());
        }
    }

    fn draw_diff_list(&mut self, cx: &mut Cx2d, list: &mut PortalList) {
        list.set_item_range(cx, 0, self.diff_lines.len());
        while let Some(item_id) = list.next_visible_item(cx) {
            let Some(line) = self.diff_lines.get(item_id) else { continue };
            let (template, text) = match line {
                DiffLine::Context(text) => (id!(DiffContext), text),
                DiffLine::Added(text) => (id!(DiffAdded), text),
                DiffLine::Removed(text) => (id!(DiffRemoved), text),
            };
            let item = list.item(cx, item_id, template);
            item.label(cx, ids!(line)).set_text(cx, text);
            item.draw_all(cx, &mut Scope::empty());
        }
    }
}

#[cfg(test)]
mod picker_tests {
    use super::*;
    use crate::a2app::room_app_picker::{RoomAppPickerAction, RoomAppPickerWidgetRefExt};
    use crate::home::navigation_tab_bar::NavigationBarAction;
    use crate::utils::RoomNameId;

    #[test]
    fn picker_dsl_and_actions_stay_scoped_to_their_surface() {
        // Register and instantiate the real DSL without starting App, a window,
        // or the Matrix/a2app runtimes, so no user session or storage is touched.
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let (screen, picker) = cx.with_vm(|vm| {
            makepad_widgets::script_mod(vm);
            makepad_code_editor::script_mod(vm);
            crate::shared::script_mod(vm);
            crate::a2app::script_mod(vm);
            let value = script_eval!(vm, { mod.widgets.MiniAppsScreen {} });
            let screen = WidgetRef::script_from_value(vm, value);
            let value = script_eval!(vm, { mod.widgets.RoomAppPicker {} });
            (screen, WidgetRef::script_from_value(vm, value))
        });
        assert!(screen.borrow::<MiniAppsScreen>().is_some());
        assert!(!picker.button(&cx, ids!(all_apps_button)).is_empty());

        let screen_row = screen.flat_list(&cx, ids!(apps_list))
            .item(&mut cx, id!(test_app), id!(mini_app_row)).unwrap();
        screen_row.borrow_mut::<MiniAppRow>().unwrap().app_id = "screen-app".into();
        let picker_row = picker.portal_list(&cx, ids!(apps_list))
            .item(&mut cx, 0, id!(app_row));
        picker_row.borrow_mut::<MiniAppRow>().unwrap().app_id = "picker-app".into();

        let clicked = |cx: &mut Cx, widget: &WidgetRef| {
            let button_uid = widget.button(cx, ids!(row_open_button)).widget_uid();
            cx.capture_actions(|cx| cx.widget_action(button_uid, ButtonAction::Clicked(Default::default())))
        };
        let clicks = clicked(&mut cx, &picker_row);
        let row_actions = cx.capture_actions(|cx| {
            picker.handle_event(cx, &Event::Actions(clicks), &mut Scope::empty());
        });
        assert!(matches!(
            row_actions.find_widget_action(picker_row.widget_uid()).cast(),
            MiniAppRowAction::OpenApp(id) if id == "picker-app"
        ));
        let unrelated = cx.capture_actions(|cx| {
            screen.handle_event(cx, &Event::Actions(row_actions), &mut Scope::empty());
        });
        assert!(unrelated.is_empty(), "picker rows must not launch from a hidden management screen");

        let clicks = clicked(&mut cx, &screen_row);
        let row_actions = cx.capture_actions(|cx| {
            screen.handle_event(cx, &Event::Actions(clicks), &mut Scope::empty());
        });
        let launched = cx.capture_actions(|cx| {
            screen.handle_event(cx, &Event::Actions(row_actions), &mut Scope::empty());
        });
        assert_eq!(launched.len(), 1);
        assert!(matches!(launched[0].downcast_ref(),
            Some(A2AppOp::OpenApp { app_id, room_id: None, .. }) if app_id == "screen-app"));

        let context = RoomNameId::empty(OwnedRoomId::try_from("!picker:example.org").unwrap());
        picker.as_room_app_picker().show(&mut cx, context.clone(), false);
        let button_uid = picker.button(&cx, ids!(all_apps_button)).widget_uid();
        let clicks = cx.capture_actions(|cx| {
            cx.widget_action(button_uid, ButtonAction::Clicked(Default::default()));
        });
        let navigation = cx.capture_actions(|cx| {
            picker.handle_event(cx, &Event::Actions(clicks), &mut Scope::empty());
        });
        assert_eq!(navigation.len(), 2);
        assert!(matches!(navigation[0].downcast_ref(), Some(RoomAppPickerAction::Close)));
        assert!(matches!(navigation[1].downcast_ref(), Some(NavigationBarAction::GoToMiniApps)));

        picker.as_room_app_picker().show(&mut cx, context, false);
        let dismiss = cx.capture_actions(|cx| cx.action(ModalAction::Dismissed));
        let response = cx.capture_actions(|cx| {
            picker.handle_event(cx, &Event::Actions(dismiss), &mut Scope::empty());
        });
        assert!(response.is_empty(), "dismissal must not re-emit Close in a loop");
    }
}

#[derive(Clone, Debug)]
pub enum AccessRuleKey {
    Room(String),
    Space(String),
    Grant(u64),
    Network(u64),
    Legacy { subject: String, perm: Permission },
}

#[derive(Clone)]
enum AccessEditor {
    Protection,
    App { app_id: String, perm: Permission, cap_id: Option<String> },
}

#[derive(Script, ScriptHook, Widget)]
pub struct MiniAppAccessRuleRow {
    #[deref] view: View,
    #[rust] key: Option<AccessRuleKey>,
}

impl Widget for MiniAppAccessRuleRow {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        self.view.handle_event(cx, event, scope);
        let Event::Actions(actions) = event else { return };
        let Some(key) = self.key.clone() else { return };
        if self.view.button(cx, ids!(rule_edit)).clicked(actions) {
            cx.action(MiniAppsScreenAction::EditAccessRule(key));
        } else if self.view.button(cx, ids!(rule_remove)).clicked(actions) {
            cx.action(MiniAppsScreenAction::RemoveAccessRule(key));
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        self.view.draw_walk(cx, scope, walk)
    }
}

fn policy_index(decision: PolicyDecision) -> usize {
    match decision { PolicyDecision::Ask => 0, PolicyDecision::Allow => 1, PolicyDecision::Deny => 2 }
}

fn policy_from_index(index: usize) -> PolicyDecision {
    match index { 1 => PolicyDecision::Allow, 2 => PolicyDecision::Deny, _ => PolicyDecision::Ask }
}

fn global_policy_index(decision: PolicyDecision, mode: RoomPolicyMode) -> usize {
    if decision == PolicyDecision::Deny { 3 }
    else if mode == RoomPolicyMode::WhitelistOnly { 2 }
    else { policy_index(decision) }
}

fn global_policy_action(access: RoomAccess, index: usize) -> A2AppOp {
    match index {
        2 => A2AppOp::SetPolicyMode { access, mode: RoomPolicyMode::WhitelistOnly },
        _ => A2AppOp::SetGlobalPolicy {
            access,
            decision: match index { 1 => PolicyDecision::Allow, 3 => PolicyDecision::Deny, _ => PolicyDecision::Ask },
        },
    }
}

fn global_policy_label(decision: PolicyDecision, mode: RoomPolicyMode) -> &'static str {
    if decision == PolicyDecision::Deny { "Block all" }
    else if mode == RoomPolicyMode::WhitelistOnly { "Only allowlisted rooms and spaces" }
    else { match decision { PolicyDecision::Allow => "Allow everywhere", _ => "Ask unless a rule allows" } }
}

fn policy_label(decision: PolicyDecision) -> &'static str {
    match decision { PolicyDecision::Ask => "Ask / no rule", PolicyDecision::Allow => "Allow", PolicyDecision::Deny => "Block" }
}

fn scope_label(cx: &mut Cx, scope: &RoomScope) -> String {
    match scope {
        RoomScope::AllRooms => "All rooms".into(),
        RoomScope::Selection { rooms, spaces } => {
            let targets = if cx.has_global::<RoomsListRef>() {
                cx.get_global::<RoomsListRef>().permission_targets()
            } else { Vec::new() };
            let label = |id: &str| targets.iter().find(|(target_id, _, _)| target_id == id)
                .map(|(_, name, _)| name.clone()).unwrap_or_else(|| id.to_owned());
            rooms.iter().map(|id| label(id))
                .chain(spaces.iter().map(|id| format!("Space: {}", label(id))))
                .collect::<Vec<_>>().join(", ")
        }
    }
}

impl MiniAppsScreen {
    fn set_write_controls_enabled(&mut self, cx: &mut Cx, enabled: bool) {
        self.view.check_box(cx, ids!(global_write_enabled)).set_active(cx, enabled, Animate::No);
        self.view.widget(cx, ids!(global_write)).set_disabled(cx, !enabled);
        self.view.widget(cx, ids!(policy_write)).set_disabled(cx, !enabled);
        self.view.widget(cx, ids!(policy_write_disabled)).set_visible(cx, !enabled);
        if matches!(self.access_editor, Some(AccessEditor::Protection)) {
            self.view.button(cx, ids!(access_save)).set_text(cx, if enabled { "Save rules for selection" } else { "Save read rules for selection" });
        }
    }

    fn show_access_editor(&mut self, cx: &mut Cx, editor: AccessEditor) {
        let (title, hint, room, protection) = match &editor {
            AccessEditor::Protection => (
                "Room and space protection".to_string(),
                "Select one or more rooms or spaces, then save read and write rules. Spaces cover their nested rooms. Any matching block wins, including a global block. Allow skips prompts; Ask removes this rule and follows other rules. In allowlist-only mode, rooms without a matching Allow are blocked. These protections apply to all mini-apps and agents.".to_string(),
                None, true,
            ),
            AccessEditor::App { app_id, perm, cap_id } => {
                let room = agent_room_of(app_id).map(str::to_owned).or_else(|| with_a2app(|state| state.registry.get(app_id).and_then(|m| match &m.scope {
                    A2AppScope::Room { room_id } => Some(room_id.clone()), _ => None,
                })).flatten());
                let title = cap_id.as_deref().and_then(a2app_core::capabilities::by_id)
                    .map(|cap| cap.title).unwrap_or(perm.title()).to_string();
                let mut hint = "Allow this ability in selected rooms or spaces for a session or always. Scoped allowances replace a broad group Allow/Block. Existing scoped rules remain until removed below. Global room and space blocks always win.".to_string();
                if *perm == Permission::Network {
                    hint = format!("{}\n\nChoose the allowed websites below. Private data also needs a source sharing rule in Manage data sharing rules.\n\n{hint}", perm.blurb());
                }
                (title, hint, room, false)
            }
        };
        self.view.label(cx, ids!(access_title)).set_text(cx, &title);
        self.view.label(cx, ids!(access_hint)).set_text(cx, &hint);
        let agent = matches!(&editor, AccessEditor::App { app_id, .. } if is_agent_subject(app_id));
        let network = matches!(&editor, AccessEditor::App { perm: Permission::Network, .. });
        self.view.permission_scope_editor(cx, ids!(access_scope)).configure(
            cx, room.as_deref(), room.as_deref(), None, network, protection,
        );
        if !protection {
            self.view.permission_scope_editor(cx, ids!(access_scope)).enable_room_session_picker(cx);
        }
        self.view.widget(cx, ids!(agent_selector)).set_visible(cx, agent);
        self.view.widget(cx, ids!(access_baseline)).set_visible(cx, true);
        self.view.widget(cx, ids!(policy_choices)).set_visible(cx, protection);
        self.view.widget(cx, ids!(access_ask)).set_visible(cx, !protection);
        self.view.widget(cx, ids!(access_block)).set_visible(cx, !protection);
        self.view.button(cx, ids!(access_save)).set_text(cx, if protection { "Save rules for selection" } else { "Allow selected" });
        self.view.drop_down(cx, ids!(policy_read)).set_selected_item(cx, 0);
        self.view.drop_down(cx, ids!(policy_write)).set_selected_item(cx, 0);
        self.access_editor = Some(editor);
        self.set_write_controls_enabled(cx, with_a2app(|state| state.permissions.matrix_write()).unwrap_or(false));
        self.set_pane(cx, Pane::Access);
    }

    fn handle_access_editor(&mut self, cx: &mut Cx, actions: &Actions) {
        if self.pane != Pane::Access { return }
        let Some(editor) = self.access_editor.clone() else { return };
        if self.view.button(cx, ids!(access_back)).clicked(actions) {
            let list = matches!(&editor, AccessEditor::Protection)
                || matches!(&editor, AccessEditor::App { app_id, .. } if is_agent_subject(app_id));
            self.set_pane(cx, if list { Pane::List } else { Pane::Info });
            return;
        }
        if self.view.button(cx, ids!(access_save)).clicked(actions) {
            let selection = match self.view.permission_scope_editor(cx, ids!(access_scope)).selection() {
                Ok(selection) => selection,
                Err(error) => {
                    enqueue_popup_notification(error, PopupKind::Warning, Some(5.0));
                    return;
                }
            };
            match &editor {
                AccessEditor::Protection => {
                    let write_enabled = with_a2app(|state| state.permissions.matrix_write()).unwrap_or(false);
                    self.save_policy_selection(cx, &selection.scope, write_enabled);
                }
                AccessEditor::App { app_id, perm, cap_id } => {
                    if let Some(network) = selection.network {
                        cx.action(A2AppOp::GrantNetwork {
                            app_id: app_id.clone(), network,
                            scope: selection.scope, duration: selection.duration, origin_room: selection.origin_room,
                        });
                    } else {
                        cx.action(A2AppOp::GrantScoped {
                            app_id: app_id.clone(), perm: *perm, cap_id: cap_id.clone(),
                            scope: selection.scope, duration: selection.duration, origin_room: selection.origin_room,
                        });
                    }
                }
            }
            self.view.redraw(cx);
        }
        let blocked = self.view.button(cx, ids!(access_block)).clicked(actions);
        let reset = self.view.button(cx, ids!(access_ask)).clicked(actions);
        if (blocked || reset) && let AccessEditor::App { app_id, perm, cap_id } = editor {
            let state = if blocked { GrantState::Denied } else { GrantState::Ask };
            if let Some(cap_id) = cap_id {
                cx.action(A2AppOp::SetCapability { app_id, cap_id, state });
            } else {
                cx.action(A2AppOp::SetPermission { app_id, perm, state });
            }
        }
    }

    fn save_policy_selection(&self, cx: &mut Cx, scope: &RoomScope, write_enabled: bool) {
        let read = policy_from_index(self.view.drop_down(cx, ids!(policy_read)).selected_item());
        let write = write_enabled.then(|| policy_from_index(self.view.drop_down(cx, ids!(policy_write)).selected_item()));
        self.apply_policy_scope(cx, scope, read, write);
    }

    fn apply_policy_scope(&self, cx: &mut Cx, scope: &RoomScope, read: PolicyDecision, write: Option<PolicyDecision>) {
        let RoomScope::Selection { rooms, spaces } = scope else { return };
        for (access, decision) in [(RoomAccess::Read, Some(read)), (RoomAccess::Write, write)] {
            let Some(decision) = decision else { continue };
            for room_id in rooms {
                cx.action(A2AppOp::SetRoomPolicy { room_id: room_id.clone(), access, decision });
            }
            for space_id in spaces {
                cx.action(A2AppOp::SetSpacePolicy { space_id: space_id.clone(), access, decision });
            }
        }
    }

    fn edit_access_rule(&mut self, cx: &mut Cx, key: &AccessRuleKey) {
        let (scope, policy) = match key {
            AccessRuleKey::Room(id) => (RoomScope::room(id), with_a2app(|state| state.permissions.room_rules().get(id).copied()).flatten()),
            AccessRuleKey::Space(id) => (RoomScope::Selection { rooms: Vec::new(), spaces: vec![id.clone()] }, with_a2app(|state| state.permissions.space_rules().get(id).copied()).flatten()),
            _ => return,
        };
        self.show_access_editor(cx, AccessEditor::Protection);
        self.view.permission_scope_editor(cx, ids!(access_scope)).set_scope(cx, &scope);
        if let Some(policy) = policy {
            self.view.drop_down(cx, ids!(policy_read)).set_selected_item(cx, policy_index(policy.read));
            self.view.drop_down(cx, ids!(policy_write)).set_selected_item(cx, policy_index(policy.write));
        }
    }

    fn remove_access_rule(&mut self, cx: &mut Cx, key: &AccessRuleKey) {
        let write = with_a2app(|state| state.permissions.matrix_write()).unwrap_or(false).then_some(PolicyDecision::Ask);
        match key {
            AccessRuleKey::Room(id) => self.apply_policy_scope(cx, &RoomScope::room(id), PolicyDecision::Ask, write),
            AccessRuleKey::Space(id) => self.apply_policy_scope(cx, &RoomScope::Selection { rooms: Vec::new(), spaces: vec![id.clone()] }, PolicyDecision::Ask, write),
            AccessRuleKey::Grant(id) => cx.action(A2AppOp::RevokeScopedGrant(*id)),
            AccessRuleKey::Network(id) => cx.action(A2AppOp::RevokeNetworkGrant(*id)),
            AccessRuleKey::Legacy { subject, perm } => cx.action(A2AppOp::ClearLegacyGrants { subject: subject.clone(), perm: *perm }),
        }
        self.view.redraw(cx);
    }

    fn access_rows(&self, cx: &mut Cx) -> Vec<(AccessRuleKey, String)> {
        match &self.access_editor {
            Some(AccessEditor::Protection) => {
                let rules = with_a2app(|state| {
                    state.permissions.room_rules().iter().map(|(id, policy)| (false, id.clone(), *policy))
                        .chain(state.permissions.space_rules().iter().map(|(id, policy)| (true, id.clone(), *policy)))
                        .collect::<Vec<_>>()
                }).unwrap_or_default();
                rules.into_iter().map(|(space, id, policy)| {
                    let scope = if space { RoomScope::Selection { rooms: Vec::new(), spaces: vec![id.clone()] } } else { RoomScope::room(&id) };
                    let label = format!("{}\nRead: {} · Write: {}", scope_label(cx, &scope), policy_label(policy.read), policy_label(policy.write));
                    (if space { AccessRuleKey::Space(id) } else { AccessRuleKey::Room(id) }, label)
                }).collect()
            }
            Some(AccessEditor::App { app_id, perm, cap_id }) => {
                let (grants, network) = with_a2app(|state| (
                    state.permissions.scoped_grants(app_id).into_iter()
                        .filter(|grant| grant.permission == perm.as_str() && (cap_id.is_none() || grant.capability.is_none() || grant.capability == *cap_id))
                        .cloned().collect::<Vec<_>>(),
                    state.permissions.network_grants(app_id).into_iter().cloned().collect::<Vec<_>>(),
                )).unwrap_or_default();
                let mut rows = grants.into_iter().map(|grant| {
                    let ability = grant.capability.as_deref().map(|id| {
                        a2app_core::capabilities::by_id(id).map(|cap| cap.title.to_string())
                            .or_else(|| id.strip_prefix("tool:").and_then(|key| serde_json::from_str::<(String, String)>(key).ok()).map(|(name, _)| format!("Tool: {name}")))
                            .unwrap_or_else(|| id.to_string())
                    }).unwrap_or_else(|| "Entire permission group".to_string());
                    let mut label = format!("Allow: {ability} · {}\n{}", scope_label(cx, &grant.scope), duration_label(grant.duration));
                    if let Some(origin) = grant.origin_room { label.push_str(&format!(" · source: {}", room_label(cx, &origin))); }
                    (AccessRuleKey::Grant(grant.id), label)
                }).collect::<Vec<_>>();
                if *perm == Permission::Network {
                    rows.extend(network.into_iter().map(|grant| (
                        AccessRuleKey::Network(grant.id),
                        format!("{}\n{} · {}", network_scope_label(&grant.network), scope_label(cx, &grant.scope), duration_label(grant.duration)),
                    )));
                }
                let legacy = with_a2app(|state| match perm {
                    Permission::Network => state.permissions.host_grants(app_id),
                    Permission::MatrixRoomsRead => state.permissions.room_read_grants(app_id),
                    Permission::MatrixRoomsSend => state.permissions.room_send_grants(app_id),
                    Permission::McpTools => state.permissions.tool_grants(app_id),
                    _ => Vec::new(),
                }).unwrap_or_default();
                if !legacy.is_empty() {
                    let count = legacy.len();
                    let names = legacy.into_iter().map(|value| match perm {
                        Permission::Network => format!("{value} and subdomains"),
                        Permission::MatrixRoomsRead | Permission::MatrixRoomsSend => room_label(cx, &value),
                        _ => value,
                    }).collect::<Vec<_>>().join(", ");
                    rows.push((
                        AccessRuleKey::Legacy { subject: app_id.clone(), perm: *perm },
                        format!("Earlier saved allowances ({count}): {names}\nThese still allow access independently of the scoped rules above. Remove revokes all listed earlier allowances."),
                    ));
                }
                rows
            }
            None => Vec::new(),
        }
    }
}

impl MiniAppsScreen {
    fn show_agent_permissions(&mut self, cx: &mut Cx) {
        #[cfg(unix)]
        { self.agent_rooms = with_a2app(|state| state.ai_rooms.keys().map(ToString::to_string).collect()).unwrap_or_default(); }
        #[cfg(not(unix))]
        { self.agent_rooms.clear(); }
        self.agent_rooms.sort_by_key(|id| room_label(cx, id).to_lowercase());
        if self.agent_rooms.is_empty() {
            enqueue_popup_notification("Open an AI room first to manage its agent's permissions.", PopupKind::Info, Some(5.0));
            return;
        }
        let mut abilities = Vec::new();
        for perm in Permission::ALL {
            let caps = a2app_core::capabilities::in_group(perm)
                .filter(|cap| crate::a2app::ai::tools::AI_ROOM_SESSION_CAP_IDS.contains(&cap.id))
                .collect::<Vec<_>>();
            if caps.is_empty() { continue }
            abilities.push((perm, None, format!("{} · entire group", perm.title())));
            for cap in caps {
                abilities.push((perm, Some(cap.id.to_string()), format!("{}: {}", perm.title(), cap.title)));
            }
        }
        self.agent_abilities = abilities;
        let room_names = self.agent_rooms.iter().map(|id| room_label(cx, id)).collect();
        self.view.drop_down(cx, ids!(agent_choice)).set_labels(cx, room_names);
        self.view.drop_down(cx, ids!(agent_choice)).set_selected_item(cx, 0);
        self.view.drop_down(cx, ids!(agent_ability)).set_labels(cx, self.agent_abilities.iter().map(|(_, _, label)| label.clone()).collect());
        self.view.drop_down(cx, ids!(agent_ability)).set_selected_item(cx, 0);
        self.select_agent_access(cx);
    }

    fn select_agent_access(&mut self, cx: &mut Cx) {
        let room_index = self.view.drop_down(cx, ids!(agent_choice)).selected_item();
        let ability_index = self.view.drop_down(cx, ids!(agent_ability)).selected_item();
        let Some(room_id) = self.agent_rooms.get(room_index) else { return };
        let Some((perm, cap_id, _)) = self.agent_abilities.get(ability_index) else { return };
        self.show_access_editor(cx, AccessEditor::App { app_id: agent_subject(room_id), perm: *perm, cap_id: cap_id.clone() });
    }
}

fn background_app_open_action(binding: &a2app_core::background::JobBinding) -> Result<A2AppOp, String> {
    use a2app_core::background::JobContext;
    let room_id = binding.context.room_id().map(|room| OwnedRoomId::try_from(room)
        .map_err(|_| "The task's room or space identity is invalid.".to_string())).transpose()?;
    match (&binding.context, room_id) {
        (JobContext::Room { .. }, Some(room_id)) => Ok(A2AppOp::OpenInRoom { app_id: binding.app_id.clone(), room_id }),
        (_, room_id) => Ok(A2AppOp::OpenApp { app_id: binding.app_id.clone(), room_id, in_room_pane: false }),
    }
}

#[cfg(test)]
mod access_tests {
    use super::*;

    #[test]
    fn background_task_configuration_opens_the_exact_context_and_preserves_room_review_surface() {
        use a2app_core::background::{JobBinding, JobContext};
        let mut binding = JobBinding { account: "@alice:example.org".into(), app_id: "reminder".into(), context: JobContext::Room { room_id: "!room:example.org".into() } };
        assert!(matches!(background_app_open_action(&binding).unwrap(), A2AppOp::OpenInRoom { app_id, room_id }
            if app_id == "reminder" && room_id.as_str() == "!room:example.org"));
        binding.context = JobContext::Space { space_id: "!space:example.org".into() };
        assert!(matches!(background_app_open_action(&binding).unwrap(), A2AppOp::OpenApp { app_id, room_id: Some(room_id), in_room_pane: false }
            if app_id == "reminder" && room_id.as_str() == "!space:example.org"));
        binding.context = JobContext::Account;
        assert!(matches!(background_app_open_action(&binding).unwrap(), A2AppOp::OpenApp { room_id: None, in_room_pane: false, .. }));
        binding.context = JobContext::Room { room_id: "invalid".into() };
        assert!(background_app_open_action(&binding).is_err(), "an invalid target must not fall back to the account context");
    }

    #[test]
    fn background_tasks_open_from_main_screen_and_return_without_activation() {
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let widget = cx.with_vm(|vm| {
            makepad_widgets::script_mod(vm);
            makepad_code_editor::script_mod(vm);
            crate::shared::script_mod(vm);
            crate::a2app::script_mod(vm);
            let value = script_eval!(vm, { mod.widgets.MiniAppsScreen {} });
            WidgetRef::script_from_value(vm, value)
        });
        let mut screen = widget.borrow_mut::<MiniAppsScreen>().unwrap();
        let uid = screen.view.button(&cx, ids!(background_tasks_button)).widget_uid();
        let click = cx.capture_actions(|cx| cx.widget_action(uid, ButtonAction::Clicked(Default::default())));
        let emitted = cx.capture_actions(|cx| screen.handle_event(cx, &Event::Actions(click), &mut Scope::empty()));
        assert!(screen.view.widget(&cx, ids!(background_pane)).visible());
        assert!(!screen.view.widget(&cx, ids!(list_pane)).visible());
        assert!(screen.view.background_tasks(&cx, ids!(background_tasks)).borrow().is_some());
        assert!(!emitted.iter().any(|action| matches!(action.downcast_ref::<A2AppOp>(), Some(A2AppOp::SaveBackgroundTask { .. }))));
        let uid = screen.view.button(&cx, ids!(background_back)).widget_uid();
        let click = cx.capture_actions(|cx| cx.widget_action(uid, ButtonAction::Clicked(Default::default())));
        screen.handle_event(&mut cx, &Event::Actions(click), &mut Scope::empty());
        assert!(screen.view.widget(&cx, ids!(list_pane)).visible());
        assert!(!screen.view.widget(&cx, ids!(background_pane)).visible());
    }

    #[test]
    fn miniapp_network_settings_require_a_destination_scope() {
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let widget = cx.with_vm(|vm| {
            makepad_widgets::script_mod(vm);
            makepad_code_editor::script_mod(vm);
            crate::shared::script_mod(vm);
            crate::a2app::script_mod(vm);
            let value = script_eval!(vm, { mod.widgets.MiniAppsScreen {} });
            WidgetRef::script_from_value(vm, value)
        });
        let mut screen = widget.borrow_mut::<MiniAppsScreen>().unwrap();
        screen.show_access_editor(&mut cx, AccessEditor::App {
            app_id: "miniapp".into(), perm: Permission::Network, cap_id: None,
        });
        assert!(screen.view.permission_scope_editor(&cx, ids!(access_scope)).selection().is_err(),
            "a mini-app network allowance must require an explicit URL scope");
    }

    #[test]
    fn saving_read_rules_while_writes_are_disabled_preserves_room_and_space_writes() {
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let widget = cx.with_vm(|vm| {
            makepad_widgets::script_mod(vm);
            makepad_code_editor::script_mod(vm);
            crate::shared::script_mod(vm);
            crate::a2app::script_mod(vm);
            let value = script_eval!(vm, { mod.widgets.MiniAppsScreen {} });
            WidgetRef::script_from_value(vm, value)
        });
        let mut screen = widget.borrow_mut::<MiniAppsScreen>().unwrap();
        screen.show_access_editor(&mut cx, AccessEditor::Protection);
        assert!(screen.view.widget(&cx, ids!(policy_write)).disabled(&cx));
        screen.view.permission_scope_editor(&cx, ids!(access_scope)).set_scope(&mut cx, &RoomScope::Selection {
            rooms: vec!["!room:example.org".into()], spaces: vec!["!space:example.org".into()],
        });
        screen.view.drop_down(&cx, ids!(policy_read)).set_selected_item(&mut cx, 1);
        screen.view.drop_down(&cx, ids!(policy_write)).set_selected_item(&mut cx, 2);
        let uid = screen.view.button(&cx, ids!(access_save)).widget_uid();
        let click = cx.capture_actions(|cx| cx.widget_action(uid, ButtonAction::Clicked(Default::default())));
        let actions = cx.capture_actions(|cx| screen.handle_access_editor(cx, &click));
        let ops = actions.iter().filter_map(|a| a.downcast_ref::<A2AppOp>()).collect::<Vec<_>>();
        assert_eq!(ops.len(), 2);
        assert!(ops.iter().any(|op| matches!(op, A2AppOp::SetRoomPolicy { room_id, access: RoomAccess::Read, decision: PolicyDecision::Allow } if room_id == "!room:example.org")));
        assert!(ops.iter().any(|op| matches!(op, A2AppOp::SetSpacePolicy { space_id, access: RoomAccess::Read, decision: PolicyDecision::Allow } if space_id == "!space:example.org")));
        assert!(!ops.iter().any(|op| matches!(op, A2AppOp::SetGlobalPolicy { .. })));
        let mut permissions = a2app_core::permissions::PermissionStore::default();
        permissions.set_room_policy("!room:example.org", RoomAccess::Write, PolicyDecision::Allow);
        permissions.set_space_policy("!space:example.org", RoomAccess::Write, PolicyDecision::Deny);
        for op in ops {
            match op {
                A2AppOp::SetRoomPolicy { room_id, access, decision } => permissions.set_room_policy(room_id, *access, *decision),
                A2AppOp::SetSpacePolicy { space_id, access, decision } => permissions.set_space_policy(space_id, *access, *decision),
                _ => panic!("saving read rules changed an unrelated setting"),
            }
        }
        let removed = cx.capture_actions(|cx| {
            screen.remove_access_rule(cx, &AccessRuleKey::Room("!room:example.org".into()));
            screen.remove_access_rule(cx, &AccessRuleKey::Space("!space:example.org".into()));
        });
        for op in removed.iter().filter_map(|action| action.downcast_ref::<A2AppOp>()) {
            match op {
                A2AppOp::SetRoomPolicy { room_id, access: RoomAccess::Read, decision } => permissions.set_room_policy(room_id, RoomAccess::Read, *decision),
                A2AppOp::SetSpacePolicy { space_id, access: RoomAccess::Read, decision } => permissions.set_space_policy(space_id, RoomAccess::Read, *decision),
                _ => panic!("removing read rules changed a paused write rule"),
            }
        }
        assert_eq!(permissions.room_rules()["!room:example.org"].write, PolicyDecision::Allow);
        assert_eq!(permissions.space_rules()["!space:example.org"].write, PolicyDecision::Deny);
    }

    #[test]
    fn write_master_disables_controls_and_reenables_saved_selection() {
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let widget = cx.with_vm(|vm| {
            makepad_widgets::script_mod(vm);
            makepad_code_editor::script_mod(vm);
            crate::shared::script_mod(vm);
            crate::a2app::script_mod(vm);
            let value = script_eval!(vm, { mod.widgets.MiniAppsScreen {} });
            WidgetRef::script_from_value(vm, value)
        });
        let mut screen = widget.borrow_mut::<MiniAppsScreen>().unwrap();
        screen.show_access_editor(&mut cx, AccessEditor::Protection);
        screen.view.drop_down(&cx, ids!(global_write)).set_selected_item(&mut cx, 2);
        screen.view.drop_down(&cx, ids!(policy_read)).set_selected_item(&mut cx, 1);
        screen.view.drop_down(&cx, ids!(policy_write)).set_selected_item(&mut cx, 2);
        let master = screen.view.check_box(&cx, ids!(global_write_enabled)).widget_uid();
        for enabled in [false, true] {
            let change = cx.capture_actions(|cx| cx.widget_action(master, CheckBoxAction::Change(enabled)));
            let actions = cx.capture_actions(|cx| screen.handle_event(cx, &Event::Actions(change), &mut Scope::empty()));
            assert!(actions.iter().any(|action| matches!(action.downcast_ref::<A2AppOp>(), Some(A2AppOp::SetMatrixWrite(value)) if *value == enabled)));
            assert_eq!(screen.view.widget(&cx, ids!(global_write)).disabled(&cx), !enabled);
            assert_eq!(screen.view.widget(&cx, ids!(policy_write)).disabled(&cx), !enabled);
            assert!(!screen.view.widget(&cx, ids!(policy_read)).disabled(&cx));
            assert_eq!(screen.view.drop_down(&cx, ids!(global_write)).selected_item(), 2);
            assert_eq!(screen.view.drop_down(&cx, ids!(policy_write)).selected_item(), 2);
            let default_write = screen.view.drop_down(&cx, ids!(global_write)).widget_uid();
            let change = cx.capture_actions(|cx| cx.widget_action(default_write, DropDownAction::Select(2)));
            let actions = cx.capture_actions(|cx| screen.handle_event(cx, &Event::Actions(change), &mut Scope::empty()));
            assert_eq!(actions.iter().any(|action| matches!(action.downcast_ref::<A2AppOp>(),
                Some(A2AppOp::SetPolicyMode { access: RoomAccess::Write, mode: RoomPolicyMode::WhitelistOnly }))), enabled,
                "a disabled default-write control cannot enable writing through a queued selection");
        }
        let saved = cx.capture_actions(|cx| screen.save_policy_selection(cx, &RoomScope::Selection {
            rooms: vec!["!room:example.org".into()], spaces: vec!["!space:example.org".into()],
        }, true));
        let ops = saved.iter().filter_map(|action| action.downcast_ref::<A2AppOp>()).collect::<Vec<_>>();
        assert_eq!(ops.len(), 4);
        assert!(ops.iter().any(|op| matches!(op, A2AppOp::SetRoomPolicy { access: RoomAccess::Write, decision: PolicyDecision::Deny, .. })));
        assert!(ops.iter().any(|op| matches!(op, A2AppOp::SetSpacePolicy { access: RoomAccess::Write, decision: PolicyDecision::Deny, .. })));
        assert!(!ops.iter().any(|op| matches!(op, A2AppOp::SetGlobalPolicy { .. } | A2AppOp::SetPolicyMode { .. })));
    }
}

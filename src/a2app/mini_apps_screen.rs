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
use a2app_core::manifest::{A2AppScope, MiniAppId, RunsIn};
use a2app_core::permissions::{Effective, GrantState, Permission};
use a2app_core::persistence;
use a2app_core::versions::AppVersion;

use crate::a2app::runtime::{room_display_name, with_a2app, A2AppOp, A2AppRuntimeAction};
use crate::home::rooms_list::RoomsListRef;
use crate::shared::popup_list::{enqueue_popup_notification, PopupKind};
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
            View {
                width: Fill, height: Fit
                flow: Down
                spacing: 2
                matrix_write_toggle := ToggleFlat {
                    margin: Inset{left: 0.5, top: 4}
                    padding: Inset{left: 15}
                    active: false
                    draw_bg +: { size: 21 }
                    text: "Mini-apps can write to rooms"
                    draw_text +: { text_style: theme.font_bold {font_size: 11} }
                }
                mod.widgets.MiniAppNote {
                    margin: Inset{left: 42}
                    body: "<ul><li>Off: a killswitch, no mini-apps can write to any room.</li><li>On: mini-apps still ask permission normally.</li></ul>"
                }
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

                prompt_input := RobrixTextInput {
                    width: Fill, height: Fit
                    padding: 12
                    empty_text: "Describe a new mini-app or changes to one…"
                    draw_text +: { text_style: REGULAR_TEXT {font_size: 11.5} }
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
                    text: "Open in room…"
                }
                info_force_stop_button := RobrixNegativeIconButton {
                    visible: false,
                    padding: Inset{top: 9, bottom: 9, left: 12, right: 12},
                    draw_icon +: { svg: (ICON_FORBIDDEN) }
                    icon_walk: Walk{width: 14, height: 14, margin: Inset{right: 2}}
                    text: "Force Stop"
                }
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
                    flow: Flow.Right{wrap: true}
                    spacing: 8
                    align: Align{y: 0.5}
                    mod.widgets.MiniAppGroupLabel { text: "Source" }
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
                View {
                    width: Fill, height: Fit
                    flow: Flow.Right{wrap: true}
                    spacing: 8
                    align: Align{y: 0.5}
                    mod.widgets.MiniAppGroupLabel { text: "Share" }
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
                View {
                    width: Fill, height: Fit
                    flow: Flow.Right{wrap: true}
                    spacing: 8
                    align: Align{y: 0.5}
                    mod.widgets.MiniAppGroupLabel { text: "Storage" }
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
                text: "Keys are stored in octos's own config file; switching providers is a one-field edit. An Ollama server running locally is detected automatically and needs no key."
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
    OpenApp(MiniAppId),
    ShowInfo(MiniAppId),
    CyclePermission { app_id: MiniAppId, perm: Permission },
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
                cx.action(MiniAppsScreenAction::OpenApp(self.app_id.clone()));
            } else if self.view.button(cx, ids!(row_settings_button)).clicked(actions) {
                cx.action(MiniAppsScreenAction::ShowInfo(self.app_id.clone()));
            }
        }
    }
    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        self.view.draw_walk(cx, scope, walk)
    }
}

/// One list row's texts, gathered per draw without cloning any app's source.
struct MiniAppRowData {
    id: MiniAppId,
    icon: String,
    name: String,
    /// Where it runs, plus running / stopped.
    detail: String,
    summary: String,
    tint: u32,
    open_label: &'static str,
}

impl MiniAppRow {
    fn populate(&mut self, cx: &mut Cx, row: &MiniAppRowData) {
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

        if let Some(on) = self.view.check_box(cx, ids!(matrix_write_toggle)).changed(actions) {
            cx.action(A2AppOp::SetMatrixWrite(on));
        }

        for action in actions {
            // A /miniapp generation lands on this screen; make sure the
            // console (list pane) is showing, not a leftover info pane.
            if let Some(A2AppOp::StartGeneration { .. }) = action.downcast_ref::<A2AppOp>()
                && self.pane != Pane::List
            {
                self.set_pane(cx, Pane::List);
            }
            if let Some(A2AppRuntimeAction::VersionsChanged(app_id)) = action.downcast_ref()
                && self.info_app.as_ref() == Some(app_id)
            {
                self.refresh_info(cx);
                continue;
            }
            match action.downcast_ref::<MiniAppsScreenAction>() {
                Some(MiniAppsScreenAction::OpenApp(app_id)) => {
                    self.open_app(cx, app_id.clone());
                    continue;
                }
                Some(MiniAppsScreenAction::ShowInfo(app_id)) => {
                    self.show_info(cx, app_id.clone());
                    continue;
                }
                Some(MiniAppsScreenAction::CyclePermission { app_id, perm }) => {
                    self.cycle_permission(cx, app_id, *perm);
                    continue;
                }
                Some(MiniAppsScreenAction::CycleCapability { app_id, cap_id }) => {
                    // Follows group -> Denied -> Allowed -> follows group.
                    let current = with_a2app(|state| state.permissions.capability_state(app_id, cap_id))
                        .unwrap_or_default();
                    let next = match current {
                        GrantState::Ask => GrantState::Denied,
                        GrantState::Denied => GrantState::Granted,
                        GrantState::Granted => GrantState::Ask,
                    };
                    cx.action(A2AppOp::SetCapability {
                        app_id: app_id.clone(),
                        cap_id: cap_id.clone(),
                        state: next,
                    });
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
                    match a2app_agent::providers::forget(id) {
                        Ok(()) => enqueue_popup_notification("Forgot that provider's key.", PopupKind::Success, Some(3.0)),
                        Err(e) => enqueue_popup_notification(e, PopupKind::Error, Some(5.0)),
                    }
                    self.view.redraw(cx);
                    continue;
                }
                Some(MiniAppsScreenAction::None) | None => {}
            }
        }

        // ----- list pane -----
        if let Some(text) = self.view.text_input(cx, ids!(prompt_input)).changed(actions) {
            self.reclassify(cx, &text);
        }
        if self.view.button(cx, ids!(intent_switch_button)).clicked(actions) {
            self.force_create = !self.force_create;
            self.show_intent_hint(cx);
        }
        if self.view.button(cx, ids!(generate_button)).clicked(actions) {
            let request = self.view.text_input(cx, ids!(prompt_input)).text();
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
            self.view.text_input(cx, ids!(prompt_input)).set_text(cx, "");
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
                cx.action(A2AppOp::OpenApp { app_id: app_id.clone(), room_id: None, in_room_pane: false });
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
                self.view.text_input(cx, ids!(prompt_input))
                    .set_text(cx, &format!("Change the {name} app: "));
                self.set_pane(cx, Pane::List);
            }
            if self.view.button(cx, ids!(info_force_stop_button)).clicked(actions) {
                cx.action(A2AppOp::ForceStop(app_id.clone()));
            }
            if self.view.button(cx, ids!(info_clear_data_button)).clicked(actions) {
                cx.action(A2AppOp::ClearData(app_id.clone()));
            }
            if self.view.button(cx, ids!(info_uninstall_button)).clicked(actions) {
                cx.action(A2AppOp::Uninstall(app_id.clone()));
                self.set_pane(cx, Pane::List);
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
        self.view.label(cx, ids!(source_title)).set_text(cx, &format!("{name} — Splash source"));
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

    /// Cycles a grant Allowed -> Asks -> Denied -> Allowed. Normal-tier
    /// permissions have no meaningful Ask state, so they skip it.
    fn cycle_permission(&mut self, cx: &mut Cx, app_id: &str, perm: Permission) {
        let current = with_a2app(|state| {
            state.registry.get(app_id)
                .map(|m| state.permissions.effective(m, perm))
        }).flatten();
        let next = match current {
            Some(Effective::Granted) => {
                if perm.tier() == a2app_core::permissions::Tier::Runtime {
                    GrantState::Ask
                } else {
                    GrantState::Denied
                }
            }
            Some(Effective::NeedsPrompt) => GrantState::Denied,
            Some(Effective::Denied) => GrantState::Granted,
            Some(Effective::Undeclared) | None => return,
        };
        cx.action(A2AppOp::SetPermission {
            app_id: app_id.to_string(),
            perm,
            state: next,
        });
    }

    /// Opens the app where it belongs: a room-bound app in its room, a
    /// room app in a room the user picks, anything else in the host.
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
        if p.external() {
            enqueue_popup_notification(
                "That one is chosen by how Robrix was started (ROBRIX_AGENT_CMD).",
                PopupKind::Info, Some(4.0),
            );
        } else if !p.active {
            match a2app_agent::providers::set_active(&provider_id) {
                Ok(()) => {
                    let in_use = a2app_agent::providers::in_use_id().as_deref() == Some(provider_id.as_str());
                    let note = if in_use {
                        format!("Now using {}.", p.label)
                    } else {
                        format!("{} is the saved default; this session still uses the agent chosen at launch.", p.label)
                    };
                    enqueue_popup_notification(note, PopupKind::Success, Some(4.0));
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
                let writes_on = with_a2app(|state| state.permissions.matrix_write()).unwrap_or(false);
                self.view.check_box(cx, ids!(matrix_write_toggle)).set_active(cx, writes_on, Animate::No);
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
            Pane::Source | Pane::Diff | Pane::Edit => {}
        }
    }

    fn draw_flat_list(&mut self, cx: &mut Cx2d, uid: WidgetUid, perms_list_uid: WidgetUid, list: &mut FlatList) {
        // Which list this is depends on which pane is visible; hidden panes
        // don't draw, so only one of these runs per draw pass.
        match self.pane {
            Pane::List => {
                // Texts only; cloning whole manifests here would copy every
                // app's source per draw.
                let rows: Vec<MiniAppRowData> = with_a2app(|state| {
                    state.registry.iter().map(|m| {
                        let running = state.is_running(&m.id);
                        let (mut detail, open_label) = match (&m.scope, m.runs_in()) {
                            (A2AppScope::Room { room_id }, _) => (format!("Runs in {}", room_label(cx, room_id)), "Run in room"),
                            (_, RunsIn::Room) => (String::from("Runs in a room"), "Run in room…"),
                            (_, RunsIn::Rooms) => (String::from("Works across your rooms"), "Open"),
                            (_, RunsIn::Spaces) => (String::from("Works across your spaces"), "Open"),
                            (_, RunsIn::Account) => (String::from("Account-wide"), "Open"),
                        };
                        if state.permissions.is_restricted(&m.id) {
                            detail.push_str(" · stopped for abuse");
                        } else if running {
                            detail.push_str(" · running");
                        }
                        MiniAppRowData {
                            id: m.id.clone(),
                            icon: m.icon.clone(),
                            name: m.name.clone(),
                            detail,
                            summary: m.description.clone(),
                            tint: m.tint,
                            open_label,
                        }
                    }).collect()
                }).unwrap_or_default();
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
            Pane::Source | Pane::Diff | Pane::Edit => {}
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

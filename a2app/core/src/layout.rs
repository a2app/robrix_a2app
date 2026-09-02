//! Where a docked mini-app pane sits, kept per (app, room) instance.

use serde::{Deserialize, Serialize};

/// Which edge of the room screen a pane is docked to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum PaneSide {
    Top,
    Bottom,
    Left,
    #[default]
    Right,
}

impl PaneSide {
    /// The side after this one in the cycle. A narrow window has no room
    /// for a side pane, so there the cycle is just top and bottom.
    pub fn next(self, vertical_ok: bool) -> Self {
        match (self, vertical_ok) {
            (PaneSide::Right, true) => PaneSide::Bottom,
            (PaneSide::Bottom, true) => PaneSide::Left,
            (PaneSide::Left, true) => PaneSide::Top,
            (PaneSide::Top, true) => PaneSide::Right,
            (PaneSide::Top, false) => PaneSide::Bottom,
            (_, false) => PaneSide::Top,
        }
    }

    pub fn is_vertical(self) -> bool {
        matches!(self, PaneSide::Left | PaneSide::Right)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            PaneSide::Top => "top",
            PaneSide::Bottom => "bottom",
            PaneSide::Left => "left",
            PaneSide::Right => "right",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "top" => Some(PaneSide::Top),
            "bottom" => Some(PaneSide::Bottom),
            "left" => Some(PaneSide::Left),
            "right" => Some(PaneSide::Right),
            _ => None,
        }
    }
}

/// A pane's dock position; survives room switches and restarts.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct PaneLayout {
    pub side: PaneSide,
    pub minimized: bool,
    /// The edge's extent along its resizable axis.
    pub edge_size: f64,
}

impl Default for PaneLayout {
    fn default() -> Self {
        Self { side: PaneSide::Right, minimized: false, edge_size: 300.0 }
    }
}

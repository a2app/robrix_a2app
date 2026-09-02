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
    pub fn next(self) -> Self {
        match self {
            PaneSide::Right => PaneSide::Bottom,
            PaneSide::Bottom => PaneSide::Left,
            PaneSide::Left => PaneSide::Top,
            PaneSide::Top => PaneSide::Right,
        }
    }

    pub fn is_vertical(self) -> bool {
        matches!(self, PaneSide::Left | PaneSide::Right)
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

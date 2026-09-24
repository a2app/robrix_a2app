//! Which side of a room's timeline a mini-app pane is docked to.

/// Which edge of the room screen a pane is docked to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum PaneSide {
    Top,
    Bottom,
    Left,
    #[default]
    Right,
}

impl PaneSide {
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

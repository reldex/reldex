//! The workspace's own layout (`SPEC.md` §20/§24.16, `phase-1.md` M6.2): which
//! worksheet and profile are active, pane sizes, and window geometry. One
//! row, because there is exactly one workspace per store file.
//!
//! Non-transactional state only — see `crate::worksheet`'s module
//! documentation. Nothing here is a session or a transaction.

use crate::ids::{ProfileId, WorksheetId};

/// Pane sizes the shell remembers across restarts. Each is `None` until the
/// user resizes that pane at least once, so a fresh workspace uses the
/// shell's own defaults rather than a guessed number.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PaneSizes {
    /// The object browser's width, in pixels.
    pub object_browser_width: Option<u32>,
    /// The result grid pane's height, in pixels.
    pub result_pane_height: Option<u32>,
}

/// The main window's last position and size. `None` fields mean "not saved
/// yet"; the shell's own defaults apply.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WindowGeometry {
    /// Left edge, in screen coordinates.
    pub x: Option<i32>,
    /// Top edge, in screen coordinates.
    pub y: Option<i32>,
    /// Width, in pixels.
    pub width: Option<u32>,
    /// Height, in pixels.
    pub height: Option<u32>,
    /// Whether the window was maximized.
    pub maximized: bool,
}

/// The workspace's layout: what was active, and how the shell was arranged.
///
/// `active_worksheet`/`active_profile` name a worksheet or profile that may
/// since have been deleted; the store then reports `None` for that field
/// rather than a dangling id (the schema's own foreign keys clear it when
/// the row they name is deleted — see `docs/decisions/
/// 0006-local-persistence-settings-profiles-sqlite.md`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Layout {
    /// The worksheet on top when the workspace was last saved.
    pub active_worksheet: Option<WorksheetId>,
    /// The profile shown as active (for example in a status bar) when the
    /// workspace was last saved.
    pub active_profile: Option<ProfileId>,
    /// Pane sizes.
    pub panes: PaneSizes,
    /// Window geometry.
    pub window: WindowGeometry,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_default_layout_has_nothing_saved_yet() {
        let layout = Layout::default();
        assert_eq!(layout.active_worksheet, None);
        assert_eq!(layout.active_profile, None);
        assert_eq!(layout.panes, PaneSizes::default());
        assert_eq!(layout.window, WindowGeometry::default());
        assert!(!layout.window.maximized);
    }
}

//! Workspace state (`SPEC.md` §20/§24.16, `phase-1.md` M6.2): one open
//! worksheet's tab — its text, caret and scroll position, tab order and the
//! profile it is (or was) attached to — persisted so the workspace can be
//! restored after a restart.
//!
//! # Non-transactional state only
//!
//! A restored worksheet is exactly that: text in an editor and a place in
//! the tab bar. It is never a session, a connection or a transaction.
//! [`Worksheet`] has no field for a session id, a "connected" flag or a
//! transaction state, and cannot be given one — restoring it must never
//! imply that a database session exists, let alone one with an open
//! transaction (`SPEC.md` §20/§24.16 — "save and restore non-transactional
//! workspace state" — and ADR-0002 D2, where transaction state lives in
//! `DatabaseSession`, not in anything this crate persists). Reopening the
//! database connection, if any, is entirely the caller's decision, made after the
//! workspace is restored, not by this crate.
//!
//! [`crate::Profile`] is the same story one level up: a saved profile is
//! configuration, never a live session.

use std::fmt;

use crate::ids::{ProfileId, WorksheetId};
use crate::time::UnixTimeMs;

/// The largest a worksheet's text may be, in UTF-8 bytes.
///
/// The same bound as [`crate::history::MAX_STATEMENT_BYTES`] and for the same
/// reason: generous for any real script, and only there so a paste accident
/// cannot put an unbounded blob in a row that otherwise has no size cap.
pub const MAX_WORKSHEET_TEXT_BYTES: usize = 1024 * 1024;

/// The longest a worksheet's title may be, in characters.
pub const MAX_TITLE_CHARS: usize = 200;

/// Why a worksheet's state was refused and not saved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum WorksheetError {
    /// The title is longer than [`MAX_TITLE_CHARS`] characters.
    TitleTooLong {
        /// The title's length in characters.
        chars: usize,
        /// The cap.
        max: usize,
    },
    /// The title contains a NUL or another control character. A tab title is
    /// shown on one line.
    TitleControlCharacter,
    /// The text is longer than [`MAX_WORKSHEET_TEXT_BYTES`].
    TextTooLong {
        /// The text's length in bytes.
        bytes: usize,
        /// The cap.
        max: usize,
    },
}

impl fmt::Display for WorksheetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TitleTooLong { chars, max } => write!(
                f,
                "the worksheet's title is {chars} characters, more than the {max}-character cap"
            ),
            Self::TitleControlCharacter => {
                f.write_str("the worksheet's title contains a control character")
            }
            Self::TextTooLong { bytes, max } => write!(
                f,
                "the worksheet's text is {bytes} bytes, more than the {max}-byte cap"
            ),
        }
    }
}

impl std::error::Error for WorksheetError {}

/// A worksheet's editable state: its tab title and editor contents.
///
/// Text is stored verbatim, exactly as the editor holds it (Thai, emoji, any
/// other Unicode text; any whitespace, including the control characters an
/// editor legitimately contains). Only the title — shown on one line in a tab
/// bar — refuses control characters.
///
/// `Debug` prints [`WorksheetState::text`]'s length, never its text — the
/// same style as [`crate::ProfileEndpoint`]'s connect string and
/// [`crate::HistoryEntry`]'s statement: worksheet text can legitimately
/// contain `IDENTIFIED BY "…"`. [`Worksheet`]'s own (derived) `Debug`
/// inherits this redaction through its `state` field.
#[derive(Clone, PartialEq, Eq)]
pub struct WorksheetState {
    /// The tab's title. May be empty (an untitled worksheet).
    pub title: String,
    /// The editor's contents, verbatim.
    pub text: String,
    /// The caret position. Opaque to the store — interpreted by the editor,
    /// which chooses the unit (for example a UTF-16 code-unit offset, to
    /// match `QQuickTextDocument`).
    pub caret: u32,
    /// The scroll position, in the editor's own unit.
    pub scroll: u32,
}

impl fmt::Debug for WorksheetState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorksheetState")
            .field("title", &self.title)
            .field("text", &format!("<redacted, {} bytes>", self.text.len()))
            .field("caret", &self.caret)
            .field("scroll", &self.scroll)
            .finish()
    }
}

impl WorksheetState {
    /// Checks the state before it is saved.
    ///
    /// # Errors
    ///
    /// [`WorksheetError`] naming the first rule the state breaks.
    pub fn validate(&self) -> Result<(), WorksheetError> {
        let chars = self.title.chars().count();
        if chars > MAX_TITLE_CHARS {
            return Err(WorksheetError::TitleTooLong {
                chars,
                max: MAX_TITLE_CHARS,
            });
        }
        if self.title.chars().any(char::is_control) {
            return Err(WorksheetError::TitleControlCharacter);
        }
        if self.text.len() > MAX_WORKSHEET_TEXT_BYTES {
            return Err(WorksheetError::TextTooLong {
                bytes: self.text.len(),
                max: MAX_WORKSHEET_TEXT_BYTES,
            });
        }
        Ok(())
    }
}

/// One open worksheet tab: its identity, the profile it is attached to (if
/// any), its editable state, its place in the tab bar, and when it was
/// created and last saved.
///
/// Never a session and never a transaction — see the module documentation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worksheet {
    id: WorksheetId,
    profile: Option<ProfileId>,
    state: WorksheetState,
    tab_order: u32,
    created_at: UnixTimeMs,
    updated_at: UnixTimeMs,
}

impl Worksheet {
    /// A new worksheet, created and updated now.
    ///
    /// # Errors
    ///
    /// [`WorksheetError`] if the state does not validate.
    pub fn new(
        id: WorksheetId,
        profile: Option<ProfileId>,
        state: WorksheetState,
        tab_order: u32,
    ) -> Result<Self, WorksheetError> {
        state.validate()?;
        let now = UnixTimeMs::now();
        Ok(Self {
            id,
            profile,
            state,
            tab_order,
            created_at: now,
            updated_at: now,
        })
    }

    /// Reassembles a stored worksheet. Validates, so a row edited outside
    /// Reldex cannot produce a worksheet [`Worksheet::new`] would have
    /// refused.
    pub(crate) fn from_stored(
        id: WorksheetId,
        profile: Option<ProfileId>,
        state: WorksheetState,
        tab_order: u32,
        created_at: UnixTimeMs,
        updated_at: UnixTimeMs,
    ) -> Result<Self, WorksheetError> {
        state.validate()?;
        Ok(Self {
            id,
            profile,
            state,
            tab_order,
            created_at,
            updated_at,
        })
    }

    /// Replaces the editable state and stamps the update time.
    ///
    /// # Errors
    ///
    /// [`WorksheetError`] if the new state does not validate; the worksheet
    /// is then unchanged.
    pub fn update_state(&mut self, state: WorksheetState) -> Result<(), WorksheetError> {
        state.validate()?;
        self.state = state;
        self.updated_at = UnixTimeMs::now();
        Ok(())
    }

    /// Moves the worksheet to a new position in the tab bar.
    pub fn set_tab_order(&mut self, tab_order: u32) {
        self.tab_order = tab_order;
        self.updated_at = UnixTimeMs::now();
    }

    /// Attaches or detaches the worksheet's profile.
    pub fn set_profile(&mut self, profile: Option<ProfileId>) {
        self.profile = profile;
        self.updated_at = UnixTimeMs::now();
    }

    /// The worksheet's id, stable across restarts.
    #[must_use]
    pub const fn id(&self) -> WorksheetId {
        self.id
    }

    /// The profile it is attached to, if any.
    #[must_use]
    pub const fn profile(&self) -> Option<ProfileId> {
        self.profile
    }

    /// The editable state.
    #[must_use]
    pub const fn state(&self) -> &WorksheetState {
        &self.state
    }

    /// Its place in the tab bar.
    #[must_use]
    pub const fn tab_order(&self) -> u32 {
        self.tab_order
    }

    /// When it was created.
    #[must_use]
    pub const fn created_at(&self) -> UnixTimeMs {
        self.created_at
    }

    /// When it was last saved.
    #[must_use]
    pub const fn updated_at(&self) -> UnixTimeMs {
        self.updated_at
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(text: &str) -> WorksheetState {
        WorksheetState {
            title: "scratch".to_owned(),
            text: text.to_owned(),
            caret: 0,
            scroll: 0,
        }
    }

    #[test]
    fn new_stamps_both_timestamps_equal() {
        let worksheet =
            Worksheet::new(WorksheetId::new_random(), None, state("select 1;"), 0).expect("valid");
        assert_eq!(worksheet.created_at(), worksheet.updated_at());
    }

    #[test]
    fn update_state_validates_first_and_stamps_the_update() {
        let mut worksheet =
            Worksheet::new(WorksheetId::new_random(), None, state("select 1;"), 0).expect("valid");
        let before = worksheet.clone();
        let mut bad = state("select 1;");
        bad.title = "tab\ttitle".to_owned();
        assert_eq!(
            worksheet.update_state(bad),
            Err(WorksheetError::TitleControlCharacter)
        );
        assert_eq!(worksheet, before);
        worksheet.update_state(state("select 2;")).expect("valid");
        assert_eq!(worksheet.state().text, "select 2;");
        assert!(worksheet.updated_at() >= before.updated_at());
        assert_eq!(worksheet.created_at(), before.created_at());
    }

    #[test]
    fn a_title_over_the_cap_and_text_over_the_cap_are_refused() {
        let mut long_title = state("x");
        long_title.title = "t".repeat(MAX_TITLE_CHARS + 1);
        assert_eq!(
            long_title.validate(),
            Err(WorksheetError::TitleTooLong {
                chars: MAX_TITLE_CHARS + 1,
                max: MAX_TITLE_CHARS,
            })
        );
        let long_text = state(&"x".repeat(MAX_WORKSHEET_TEXT_BYTES + 1));
        assert_eq!(
            long_text.validate(),
            Err(WorksheetError::TextTooLong {
                bytes: MAX_WORKSHEET_TEXT_BYTES + 1,
                max: MAX_WORKSHEET_TEXT_BYTES,
            })
        );
    }

    #[test]
    fn an_empty_title_and_thai_and_emoji_text_are_accepted() {
        let mut untitled = state("select ผู้ใช้ from dual; -- 🎉");
        untitled.title = String::new();
        assert_eq!(untitled.validate(), Ok(()));
    }

    #[test]
    fn set_tab_order_and_set_profile_stamp_the_update_time() {
        let mut worksheet =
            Worksheet::new(WorksheetId::new_random(), None, state("select 1;"), 0).expect("valid");
        let before = worksheet.updated_at();
        worksheet.set_tab_order(3);
        assert_eq!(worksheet.tab_order(), 3);
        assert!(worksheet.updated_at() >= before);
        let profile = ProfileId::new_random();
        worksheet.set_profile(Some(profile));
        assert_eq!(worksheet.profile(), Some(profile));
    }

    #[test]
    fn debug_redacts_the_worksheet_text_on_both_state_and_worksheet() {
        let secret_looking = "ALTER USER app_owner IDENTIFIED BY \"Hunter2\"";
        let mut worksheet_state = state(secret_looking);
        worksheet_state.title = "scratch".to_owned();
        let printed = format!("{worksheet_state:?}");
        assert!(!printed.contains("Hunter2"), "{printed}");
        assert!(!printed.contains(secret_looking), "{printed}");
        assert!(
            printed.contains(&format!("<redacted, {} bytes>", secret_looking.len())),
            "{printed}"
        );
        // The title is not redacted — only the editor text is sensitive here.
        assert!(printed.contains("scratch"), "{printed}");

        let worksheet =
            Worksheet::new(WorksheetId::new_random(), None, worksheet_state, 0).expect("valid");
        let printed = format!("{worksheet:?}");
        assert!(!printed.contains("Hunter2"), "{printed}");
        assert!(!printed.contains(secret_looking), "{printed}");
        assert!(
            printed.contains(&format!("<redacted, {} bytes>", secret_looking.len())),
            "{printed}"
        );
    }
}

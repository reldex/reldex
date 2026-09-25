//! Workspace state CRUD (M6.2): [`Store::save_worksheet`],
//! [`Store::load_worksheets`], [`Store::delete_worksheet`],
//! [`Store::save_layout`], [`Store::load_layout`].
//!
//! Non-transactional state only — see `crate::worksheet`'s module
//! documentation. Nothing here reads or writes a session id or a
//! transaction flag; there is no column for either.

use rusqlite::{OptionalExtension, TransactionBehavior, params};

use crate::ids::{ProfileId, WorksheetId};
use crate::layout::{Layout, PaneSizes, WindowGeometry};
use crate::time::UnixTimeMs;
use crate::worksheet::{Worksheet, WorksheetState};

use super::{Loaded, RejectReason, RejectedRow, Store, StoreError, StoreTable};

impl Store {
    /// Saves `worksheet`: inserts it if its id is new, otherwise replaces its
    /// state, profile and tab order in place. Its creation time is kept as
    /// first stored, like [`Store::update_profile`].
    ///
    /// # Errors
    ///
    /// [`StoreError::InvalidWorksheet`] if the state does not validate;
    /// [`StoreError::ProfileNotFound`] if it names a profile that does not
    /// exist; an SQLite failure. Nothing is written on error.
    pub fn save_worksheet(&mut self, worksheet: &Worksheet) -> Result<(), StoreError> {
        worksheet.state().validate()?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let profile_id = worksheet.profile().map(|profile| profile.to_string());
        if let Some(profile) = worksheet.profile() {
            let exists = transaction
                .query_row(
                    "SELECT 1 FROM profile WHERE id = ?1",
                    params![profile.to_string()],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if !exists {
                return Err(StoreError::ProfileNotFound(profile));
            }
        }
        transaction.execute(
            "INSERT INTO worksheet (id, profile_id, title, text, caret, scroll, tab_order, \
             created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
             ON CONFLICT (id) DO UPDATE SET profile_id = excluded.profile_id, \
             title = excluded.title, text = excluded.text, caret = excluded.caret, \
             scroll = excluded.scroll, tab_order = excluded.tab_order, \
             updated_at = excluded.updated_at",
            params![
                worksheet.id().to_string(),
                profile_id,
                worksheet.state().title,
                worksheet.state().text,
                worksheet.state().caret,
                worksheet.state().scroll,
                worksheet.tab_order(),
                worksheet.created_at().as_millis(),
                worksheet.updated_at().as_millis(),
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Every open worksheet, in tab order. A row that does not decode is
    /// reported in [`Loaded::rejected`] rather than hiding every other
    /// worksheet.
    ///
    /// # Errors
    ///
    /// An SQLite failure.
    pub fn load_worksheets(&self) -> Result<Loaded<Vec<Worksheet>>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT id, profile_id, title, text, caret, scroll, tab_order, created_at, \
             updated_at FROM worksheet ORDER BY tab_order, id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, u32>(4)?,
                row.get::<_, u32>(5)?,
                row.get::<_, u32>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, i64>(8)?,
            ))
        })?;
        let mut loaded = Loaded {
            value: Vec::new(),
            rejected: Vec::new(),
        };
        for row in rows {
            let (id, profile_id, title, text, caret, scroll, tab_order, created_at, updated_at) =
                row?;
            match decode_worksheet(
                &id,
                &profile_id,
                title,
                text,
                caret,
                scroll,
                tab_order,
                created_at,
                updated_at,
            ) {
                Ok(worksheet) => loaded.value.push(worksheet),
                Err(detail) => loaded.rejected.push(RejectedRow {
                    table: StoreTable::Worksheet,
                    key: id,
                    reason: RejectReason::Undecodable(detail),
                }),
            }
        }
        Ok(loaded)
    }

    /// Deletes a worksheet. Its worksheet-scoped setting overrides go with it
    /// (`worksheet_setting`'s `ON DELETE CASCADE`), and a `layout` row
    /// naming it as active has that field cleared, never left dangling.
    /// Returns whether it existed.
    ///
    /// # Errors
    ///
    /// An SQLite failure.
    pub fn delete_worksheet(&mut self, id: WorksheetId) -> Result<bool, StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let deleted = transaction.execute(
            "DELETE FROM worksheet WHERE id = ?1",
            params![id.to_string()],
        )?;
        transaction.commit()?;
        Ok(deleted > 0)
    }

    /// Saves the workspace's layout, replacing whatever was saved before —
    /// there is exactly one layout row.
    ///
    /// # Errors
    ///
    /// [`StoreError::WorksheetNotFound`] / [`StoreError::ProfileNotFound`] if
    /// `layout` names one that does not exist; an SQLite failure. Nothing is
    /// written on error.
    pub fn save_layout(&mut self, layout: &Layout) -> Result<(), StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(worksheet) = layout.active_worksheet {
            let exists = transaction
                .query_row(
                    "SELECT 1 FROM worksheet WHERE id = ?1",
                    params![worksheet.to_string()],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if !exists {
                return Err(StoreError::WorksheetNotFound(worksheet));
            }
        }
        if let Some(profile) = layout.active_profile {
            let exists = transaction
                .query_row(
                    "SELECT 1 FROM profile WHERE id = ?1",
                    params![profile.to_string()],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if !exists {
                return Err(StoreError::ProfileNotFound(profile));
            }
        }
        transaction.execute(
            "INSERT INTO layout (id, active_worksheet_id, active_profile_id, \
             object_browser_width, result_pane_height, window_x, window_y, window_width, \
             window_height, window_maximized, updated_at) \
             VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) \
             ON CONFLICT (id) DO UPDATE SET \
             active_worksheet_id = excluded.active_worksheet_id, \
             active_profile_id = excluded.active_profile_id, \
             object_browser_width = excluded.object_browser_width, \
             result_pane_height = excluded.result_pane_height, \
             window_x = excluded.window_x, window_y = excluded.window_y, \
             window_width = excluded.window_width, window_height = excluded.window_height, \
             window_maximized = excluded.window_maximized, updated_at = excluded.updated_at",
            params![
                layout.active_worksheet.map(|id| id.to_string()),
                layout.active_profile.map(|id| id.to_string()),
                layout.panes.object_browser_width,
                layout.panes.result_pane_height,
                layout.window.x,
                layout.window.y,
                layout.window.width,
                layout.window.height,
                i64::from(layout.window.maximized),
                UnixTimeMs::now().as_millis(),
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// The workspace's layout, if it has ever been saved.
    ///
    /// # Errors
    ///
    /// [`StoreError::InvalidRow`] if the row does not decode; an SQLite
    /// failure.
    pub fn load_layout(&self) -> Result<Option<Layout>, StoreError> {
        self.connection
            .query_row(
                "SELECT active_worksheet_id, active_profile_id, object_browser_width, \
                 result_pane_height, window_x, window_y, window_width, window_height, \
                 window_maximized FROM layout WHERE id = 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<u32>>(2)?,
                        row.get::<_, Option<u32>>(3)?,
                        row.get::<_, Option<i32>>(4)?,
                        row.get::<_, Option<i32>>(5)?,
                        row.get::<_, Option<u32>>(6)?,
                        row.get::<_, Option<u32>>(7)?,
                        row.get::<_, i64>(8)?,
                    ))
                },
            )
            .optional()?
            .map(
                |(
                    active_worksheet_id,
                    active_profile_id,
                    object_browser_width,
                    result_pane_height,
                    x,
                    y,
                    width,
                    height,
                    maximized,
                )| {
                    let active_worksheet = active_worksheet_id
                        .map(|text| {
                            WorksheetId::parse(&text)
                                .map_err(|error| format!("active_worksheet_id: {error}"))
                        })
                        .transpose()?;
                    let active_profile = active_profile_id
                        .map(|text| {
                            ProfileId::parse(&text)
                                .map_err(|error| format!("active_profile_id: {error}"))
                        })
                        .transpose()?;
                    Ok(Layout {
                        active_worksheet,
                        active_profile,
                        panes: PaneSizes {
                            object_browser_width,
                            result_pane_height,
                        },
                        window: WindowGeometry {
                            x,
                            y,
                            width,
                            height,
                            maximized: maximized != 0,
                        },
                    })
                },
            )
            .transpose()
            .map_err(|detail: String| StoreError::InvalidRow { detail })
    }
}

#[allow(clippy::too_many_arguments)]
fn decode_worksheet(
    id: &str,
    profile_id: &Option<String>,
    title: String,
    text: String,
    caret: u32,
    scroll: u32,
    tab_order: u32,
    created_at: i64,
    updated_at: i64,
) -> Result<Worksheet, String> {
    let worksheet_id = WorksheetId::parse(id).map_err(|error| format!("id: {error}"))?;
    let profile = profile_id
        .as_deref()
        .map(|text| ProfileId::parse(text).map_err(|error| format!("profile_id: {error}")))
        .transpose()?;
    Worksheet::from_stored(
        worksheet_id,
        profile,
        WorksheetState {
            title,
            text,
            caret,
            scroll,
        },
        tab_order,
        UnixTimeMs::from_millis(created_at),
        UnixTimeMs::from_millis(updated_at),
    )
    .map_err(|error| error.to_string())
}

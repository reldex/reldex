//! Query history CRUD (M4.10): [`Store::record_history`], [`Store::history`],
//! [`Store::clear_history`].

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::history::{HistoryEntry, HistoryId, HistoryPage, HistoryRecord};
use crate::ids::ProfileId;
use crate::settings::{EntryLimit, HISTORY_MAX_ENTRIES_PER_PROFILE};
use crate::time::UnixTimeMs;

use super::{Store, StoreError, codec};

impl Store {
    /// Records one statement's outcome for `entry.profile`, then trims the
    /// profile's history back to `history.max_entries_per_profile` (oldest
    /// first) — insert and trim are the same `IMMEDIATE` transaction, so a
    /// reader never sees the bound momentarily exceeded.
    ///
    /// **Cost: O(1) amortized**, independent of both the limit and the
    /// table's size — not O(min(rows, limit)) per insert. `history_meta`
    /// (schema 4) keeps a running per-profile row count maintained in this
    /// same transaction; a steady-state insert deletes exactly one row (the
    /// single oldest one pushed past the bound), by an index-bound
    /// `ORDER BY id ASC LIMIT k` — never a full "keep the newest N" scan. If
    /// the limit was just lowered, the excess is caught up in one O(k) pass
    /// on this insert, then every later insert is O(1) again; see the
    /// ADR-0006 amendment for the measurement table this replaces a slower
    /// design over.
    ///
    /// Never captures a bind value: [`HistoryEntry`] has no field for one.
    /// The statement text is stored verbatim by design — see
    /// `crate::history`'s module documentation for why the
    /// credential-pattern guard does not run on it.
    ///
    /// # Errors
    ///
    /// [`StoreError::InvalidHistory`] if the entry does not validate;
    /// [`StoreError::ProfileNotFound`] if the profile does not exist; an
    /// SQLite failure. Nothing is written on error.
    pub fn record_history(&mut self, entry: HistoryEntry) -> Result<HistoryId, StoreError> {
        entry.validate()?;
        let (outcome, native_code) = codec::encode_history_outcome(&entry.outcome);
        let elapsed_ms = i64::try_from(entry.elapsed_ms).unwrap_or(i64::MAX);
        let row_count = entry
            .row_count
            .map(|count| i64::try_from(count).unwrap_or(i64::MAX));
        let profile_key = entry.profile.to_string();

        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let exists = transaction
            .query_row(
                "SELECT 1 FROM profile WHERE id = ?1",
                params![profile_key],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !exists {
            return Err(StoreError::ProfileNotFound(entry.profile));
        }
        transaction.execute(
            "INSERT INTO history (profile_id, executed_at, statement, outcome, native_code, \
             elapsed_ms, row_count) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                profile_key,
                entry.executed_at.as_millis(),
                entry.statement,
                outcome,
                native_code,
                elapsed_ms,
                row_count,
            ],
        )?;
        let id = HistoryId::from_row_id(transaction.last_insert_rowid());

        // One upsert, no read-then-write race: `count` becomes this
        // profile's true post-insert total in the same statement, `RETURNING`
        // it so trimming below never needs a second round trip to learn it.
        let count: i64 = transaction.query_row(
            "INSERT INTO history_meta (profile_id, count) VALUES (?1, 1) \
             ON CONFLICT (profile_id) DO UPDATE SET count = count + 1 \
             RETURNING count",
            params![profile_key],
            |row| row.get(0),
        )?;

        if let Some(limit) = current_max_entries(&transaction)? {
            let limit = i64::from(limit);
            if count > limit {
                let excess = count - limit;
                transaction.execute(
                    "DELETE FROM history WHERE id IN ( \
                         SELECT id FROM history WHERE profile_id = ?1 ORDER BY id ASC LIMIT ?2)",
                    params![profile_key, excess],
                )?;
                transaction.execute(
                    "UPDATE history_meta SET count = count - ?2 WHERE profile_id = ?1",
                    params![profile_key, excess],
                )?;
            }
        }
        transaction.commit()?;
        Ok(id)
    }

    /// One profile's history, newest first.
    ///
    /// # Errors
    ///
    /// [`StoreError::InvalidRow`] if a row does not decode — unreachable for
    /// a file this crate wrote, since the schema's own `CHECK` keeps
    /// `outcome`/`native_code` consistent, but possible for a file a newer
    /// Reldex wrote a new outcome word into; an SQLite failure.
    pub fn history(
        &self,
        profile: ProfileId,
        page: HistoryPage,
    ) -> Result<Vec<HistoryRecord>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT id, executed_at, statement, outcome, native_code, elapsed_ms, row_count \
             FROM history WHERE profile_id = ?1 AND (?2 IS NULL OR id < ?2) \
             ORDER BY id DESC LIMIT ?3",
        )?;
        let before = page.before.map(HistoryId::as_row_id);
        let rows = statement.query_map(
            params![profile.to_string(), before, i64::from(page.limit)],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, Option<i64>>(6)?,
                ))
            },
        )?;
        let mut records = Vec::new();
        for row in rows {
            let (id, executed_at, statement, outcome_word, native_code, elapsed_ms, row_count) =
                row?;
            let outcome = codec::decode_history_outcome(&outcome_word, native_code)
                .map_err(|detail| StoreError::InvalidRow { detail })?;
            records.push(HistoryRecord {
                id: HistoryId::from_row_id(id),
                profile,
                executed_at: UnixTimeMs::from_millis(executed_at),
                statement,
                outcome,
                elapsed_ms: u64::try_from(elapsed_ms).unwrap_or(0),
                row_count: row_count.map(|count| u64::try_from(count).unwrap_or(0)),
            });
        }
        Ok(records)
    }

    /// Deletes every history entry for `profile`. Returns how many were
    /// removed.
    ///
    /// Also drops `profile`'s `history_meta` row rather than zeroing it in
    /// place: the next [`Store::record_history`] recreates it at count 1
    /// through the same upsert an unseen profile would take, so there is one
    /// code path for "no counter yet", not two.
    ///
    /// # Errors
    ///
    /// An SQLite failure.
    pub fn clear_history(&mut self, profile: ProfileId) -> Result<usize, StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let profile_key = profile.to_string();
        let deleted = transaction.execute(
            "DELETE FROM history WHERE profile_id = ?1",
            params![profile_key],
        )?;
        transaction.execute(
            "DELETE FROM history_meta WHERE profile_id = ?1",
            params![profile_key],
        )?;
        transaction.commit()?;
        Ok(deleted)
    }
}

/// The application-level `history.max_entries_per_profile` value in force,
/// read on `connection` so it sees the same uncommitted transaction the
/// insert it bounds ran in. `None` means unlimited (no trim); no stored
/// override falls back to the registry's own default, never a hard-coded
/// number, so a registry change cannot silently disagree with this.
fn current_max_entries(connection: &Connection) -> Result<Option<u32>, StoreError> {
    let stored: Option<Option<i64>> = connection
        .query_row(
            "SELECT int_value FROM setting WHERE scope = 'application' AND scope_id = '' \
             AND setting_key = ?1",
            params![HISTORY_MAX_ENTRIES_PER_PROFILE.id().storage_key()],
            |row| row.get::<_, Option<i64>>(0),
        )
        .optional()?;
    Ok(match stored {
        None => match HISTORY_MAX_ENTRIES_PER_PROFILE.default_value() {
            EntryLimit::Count(count) => Some(count.get()),
            EntryLimit::Unlimited => None,
        },
        Some(None) => None,
        Some(Some(value)) => Some(u32::try_from(value).unwrap_or(u32::MAX)),
    })
}

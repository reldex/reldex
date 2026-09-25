//! Metadata preparation (M2.11 family 4): turning a vendor-neutral
//! `reldex_db_driver_api::MetadataRequest` into a `Statement` plus its
//! declared column contract, through a concrete `MetadataCatalog`.
//!
//! # The composition root's driver choice
//!
//! `reldex_db_driver_api::MetadataCatalog` is a trait a real session's
//! connection normally hands out
//! (`DatabaseConnection::metadata_catalog`); there is no live connection
//! here. This crate is the composition root (`ARCHITECTURE.md` §2), so it
//! constructs `reldex_driver_oracle_thin::OracleMetadataCatalog` directly —
//! sound because that type is stateless (a zero-sized struct; every method
//! just builds a `Statement` from its argument, `crates/drivers/oracle-thin/
//! src/metadata.rs`) — the same pattern `crate::workspace`'s
//! `DriverBinding` uses for `connection_params`.
//!
//! # What this does not do
//!
//! It never touches the network and never opens a session: it only *prepares*
//! a statement. Running it is the UI's job, on its own metadata session
//! (`phase-1.md` M6.1) — through the ordinary [`crate::reldex_session_execute`]
//! path once that path carries binds (M2.11 does not add bind support to
//! `reldex_session_execute`; every metadata statement has binds — `schema`,
//! `table`, the name filter and the limit are *always* bind values, never
//! interpolated, `crates/drivers/oracle-thin/src/metadata.rs`'s own module
//! documentation — so this family is not yet end-to-end runnable through this
//! header. [`reldex_metadata_query_bind_count`] at least lets a caller see
//! that a statement needs binds rather than being silently surprised when
//! executing it returns no rows or the wrong ones).

use reldex_db_driver_api::{
    DbError, MetadataCatalog, MetadataObjectKind, MetadataRequest, NativeError,
};
use reldex_driver_oracle_thin::OracleMetadataCatalog;

use crate::batch::{ReldexColumnInfo, ResultColumns};
use crate::error::{ReldexError, ReldexErrorKind, set_last_argument_error, set_last_error};
use crate::status::{ReldexStatus, entry, entry_value};
use crate::strings::{CStruct, OwnedStr, ReldexStr, read_in_struct};

/// Which shape of [`MetadataRequest`] a [`ReldexMetadataRequest`] describes.
///
/// `0` is reserved for a value this header predates.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReldexMetadataRequestKind {
    /// A value this header does not know.
    Unknown = 0,
    /// [`MetadataRequest::Schemas`].
    Schemas = 1,
    /// [`MetadataRequest::ObjectsOfKind`].
    ObjectsOfKind = 2,
    /// [`MetadataRequest::ColumnsOf`].
    ColumnsOf = 3,
}

/// One of the nine object groups `SPEC.md` §16 lists besides schemas.
///
/// `0` is reserved for a value this header predates.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReldexMetadataObjectKind {
    /// A value this header does not know.
    Unknown = 0,
    /// Tables.
    Tables = 1,
    /// Views.
    Views = 2,
    /// PL/SQL package specifications.
    Packages = 3,
    /// PL/SQL package bodies.
    PackageBodies = 4,
    /// Stored procedures.
    Procedures = 5,
    /// Stored functions.
    Functions = 6,
    /// Triggers.
    Triggers = 7,
    /// Sequences.
    Sequences = 8,
    /// Synonyms.
    Synonyms = 9,
}

impl ReldexMetadataObjectKind {
    const fn to_object_kind(self) -> Option<MetadataObjectKind> {
        Some(match self {
            Self::Unknown => return None,
            Self::Tables => MetadataObjectKind::Tables,
            Self::Views => MetadataObjectKind::Views,
            Self::Packages => MetadataObjectKind::Packages,
            Self::PackageBodies => MetadataObjectKind::PackageBodies,
            Self::Procedures => MetadataObjectKind::Procedures,
            Self::Functions => MetadataObjectKind::Functions,
            Self::Triggers => MetadataObjectKind::Triggers,
            Self::Sequences => MetadataObjectKind::Sequences,
            Self::Synonyms => MetadataObjectKind::Synonyms,
        })
    }

    fn from_i32(value: i32) -> Option<Self> {
        [
            Self::Tables,
            Self::Views,
            Self::Packages,
            Self::PackageBodies,
            Self::Procedures,
            Self::Functions,
            Self::Triggers,
            Self::Sequences,
            Self::Synonyms,
        ]
        .into_iter()
        .find(|&candidate| candidate as i32 == value)
    }
}

/// A vendor-neutral metadata request, as input.
///
/// `schema`/`table`/`name_filter` are read only while this struct is passed
/// in — nothing here borrows past the call.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ReldexMetadataRequest {
    /// `sizeof(ReldexMetadataRequest)`.
    pub struct_size: u32,
    /// A [`ReldexMetadataRequestKind`].
    pub kind: i32,
    /// A [`ReldexMetadataObjectKind`]. `RELDEX_METADATA_REQUEST_KIND_OBJECTS_OF_KIND`
    /// only.
    pub object_kind: i32,
    /// The schema to list or own the table.
    /// `RELDEX_METADATA_REQUEST_KIND_OBJECTS_OF_KIND`/`..._COLUMNS_OF` only.
    pub schema: ReldexStr,
    /// The table (or view) to describe. `..._COLUMNS_OF` only.
    pub table: ReldexStr,
    /// A server-side, case-insensitive "contains" filter, when
    /// `has_name_filter`. `..._SCHEMAS`/`..._OBJECTS_OF_KIND` only.
    pub name_filter: ReldexStr,
    /// Whether `name_filter` applies.
    pub has_name_filter: bool,
    /// The maximum number of rows to report; must be above zero.
    /// `..._SCHEMAS`/`..._OBJECTS_OF_KIND` only.
    pub limit: u32,
}

// SAFETY: `#[repr(C)]`, `struct_size` first, every other field an integer,
// `bool` or `ReldexStr` — all valid as zero.
unsafe impl CStruct for ReldexMetadataRequest {
    const MIN_SIZE: usize = size_of::<Self>();
}

/// Builds a [`MetadataRequest`] from the caller's struct, or reports why not.
fn build_request(request: &ReldexMetadataRequest) -> Result<MetadataRequest, ReldexStatus> {
    // SAFETY: the struct was just read from caller memory by `read_in_struct`,
    // which promises `schema`/`table`/`name_filter` are readable for the
    // lengths they declare; used only for the duration of this function.
    let schema = unsafe { request.schema.as_str() };
    // SAFETY: as above — `table` is read from the same just-validated struct.
    let table = unsafe { request.table.as_str() };
    // SAFETY: as above — `name_filter` is read from the same just-validated
    // struct.
    let name_filter = unsafe { request.name_filter.as_str() };

    if request.kind == ReldexMetadataRequestKind::Schemas as i32 {
        let Some(limit) = std::num::NonZeroU32::new(request.limit) else {
            set_last_argument_error("reldex_metadata_prepare: `limit` must be above zero");
            return Err(ReldexStatus::InvalidArgument);
        };
        let mut built = MetadataRequest::schemas(limit);
        if request.has_name_filter {
            let Some(filter) = name_filter else {
                set_last_argument_error(
                    "reldex_metadata_prepare: `name_filter` is not valid UTF-8",
                );
                return Err(ReldexStatus::InvalidArgument);
            };
            built = built.with_name_filter(filter);
        }
        Ok(built)
    } else if request.kind == ReldexMetadataRequestKind::ObjectsOfKind as i32 {
        let Some(schema) = schema else {
            set_last_argument_error("reldex_metadata_prepare: `schema` is not valid UTF-8");
            return Err(ReldexStatus::InvalidArgument);
        };
        let Some(kind) = ReldexMetadataObjectKind::from_i32(request.object_kind)
            .and_then(ReldexMetadataObjectKind::to_object_kind)
        else {
            set_last_argument_error(
                "reldex_metadata_prepare: `object_kind` is not a ReldexMetadataObjectKind this \
                 build knows",
            );
            return Err(ReldexStatus::InvalidArgument);
        };
        let Some(limit) = std::num::NonZeroU32::new(request.limit) else {
            set_last_argument_error("reldex_metadata_prepare: `limit` must be above zero");
            return Err(ReldexStatus::InvalidArgument);
        };
        let mut built = MetadataRequest::objects_of_kind(schema, kind, limit);
        if request.has_name_filter {
            let Some(filter) = name_filter else {
                set_last_argument_error(
                    "reldex_metadata_prepare: `name_filter` is not valid UTF-8",
                );
                return Err(ReldexStatus::InvalidArgument);
            };
            built = built.with_name_filter(filter);
        }
        Ok(built)
    } else if request.kind == ReldexMetadataRequestKind::ColumnsOf as i32 {
        let (Some(schema), Some(table)) = (schema, table) else {
            set_last_argument_error(
                "reldex_metadata_prepare: `schema`/`table` are not valid UTF-8",
            );
            return Err(ReldexStatus::InvalidArgument);
        };
        Ok(MetadataRequest::columns_of(schema, table))
    } else {
        set_last_argument_error(
            "reldex_metadata_prepare: `kind` is not a ReldexMetadataRequestKind this build knows",
        );
        Err(ReldexStatus::InvalidArgument)
    }
}

/// A prepared metadata statement: its SQL text, its declared column contract,
/// and the classifier that corrects one of its own permission-ambiguous
/// errors.
///
/// Opaque, owned by the caller from [`reldex_metadata_prepare`] until
/// [`reldex_metadata_query_release`].
pub struct ReldexMetadataQuery {
    sql: OwnedStr,
    columns: std::sync::Arc<ResultColumns>,
    classifier: reldex_db_driver_api::MetadataErrorClassifier,
    bind_count: usize,
}

impl Drop for ReldexMetadataQuery {
    fn drop(&mut self) {
        crate::counters::destroyed(crate::counters::Kind::WorkspaceObject);
    }
}

/// Prepares `request` as a statement plus its column contract.
///
/// Allocates the query this returns through `out`; release it with
/// [`reldex_metadata_query_release`]. `request` is read only for the
/// duration of this call.
///
/// # Safety
///
/// `request` must be null, or aligned with `struct_size` set and every
/// `ReldexStr` field pointing at readable bytes. `out` must be null (to
/// validate without keeping the result) or point at a writable pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_metadata_prepare(
    request: *const ReldexMetadataRequest,
    out: *mut *mut ReldexMetadataQuery,
) -> ReldexStatus {
    entry(|| {
        // SAFETY: delegated to this function's contract for `request`.
        let Some(request) = (unsafe { read_in_struct(request) }) else {
            return set_last_argument_error(
                "reldex_metadata_prepare: `request` is null, unaligned, or its struct_size is \
                 too small",
            );
        };
        if !out.is_null() && !out.is_aligned() {
            return set_last_argument_error("reldex_metadata_prepare: `out` is unaligned");
        }
        let built = match build_request(&request) {
            Ok(built) => built,
            Err(status) => return status,
        };
        let catalog = OracleMetadataCatalog;
        let prepared = match catalog.prepare(built) {
            Ok(prepared) => prepared,
            Err(error) => {
                set_last_error(error);
                return ReldexStatus::Error;
            }
        };
        let bind_count = match prepared.statement().binds() {
            reldex_db_driver_api::Binds::None => 0,
            reldex_db_driver_api::Binds::Positional(binds) => binds.len(),
            reldex_db_driver_api::Binds::Named(binds) => binds.len(),
            // `Binds` is `#[non_exhaustive]`.
            _ => 0,
        };
        let (statement, columns, classifier) = prepared.into_parts();
        crate::counters::created(crate::counters::Kind::WorkspaceObject);
        let query = Box::new(ReldexMetadataQuery {
            sql: OwnedStr::new(statement.sql().to_owned()),
            columns: ResultColumns::new(columns),
            classifier,
            bind_count,
        });
        if !out.is_null() {
            // SAFETY: checked non-null and aligned above.
            unsafe { out.write(Box::into_raw(query)) };
        } else {
            drop(query);
        }
        ReldexStatus::Ok
    })
}

/// The prepared statement's SQL text.
///
/// # Safety
///
/// `query` must be a live [`ReldexMetadataQuery`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_metadata_query_sql(query: *const ReldexMetadataQuery) -> ReldexStr {
    entry_value(ReldexStr::empty(), || {
        if query.is_null() {
            return ReldexStr::empty();
        }
        // SAFETY: delegated to this function's contract.
        unsafe { &*query }.sql.as_reldex_str()
    })
}

/// How many bind placeholders the prepared statement has. Every metadata
/// statement has at least one (`crates/drivers/oracle-thin/src/metadata.rs`'s
/// module documentation: schema, table, the name filter and the limit are
/// always bind values, never interpolated) — see the module documentation for
/// why this crate cannot yet execute one through
/// [`crate::reldex_session_execute`].
///
/// # Safety
///
/// `query` must be a live [`ReldexMetadataQuery`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_metadata_query_bind_count(
    query: *const ReldexMetadataQuery,
) -> usize {
    entry_value(0, || {
        if query.is_null() {
            return 0;
        }
        // SAFETY: delegated to this function's contract.
        unsafe { &*query }.bind_count
    })
}

/// How many columns the prepared statement's result is declared to have.
///
/// # Safety
///
/// `query` must be a live [`ReldexMetadataQuery`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_metadata_query_column_count(
    query: *const ReldexMetadataQuery,
) -> usize {
    entry_value(0, || {
        if query.is_null() {
            return 0;
        }
        // SAFETY: delegated to this function's contract.
        unsafe { &*query }.columns.len()
    })
}

/// Describes declared column `index` of the prepared statement's contract.
///
/// # Safety
///
/// `query` must be a live [`ReldexMetadataQuery`]; `out` must be null or point
/// at a writable [`ReldexColumnInfo`] with `struct_size` set.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_metadata_query_column(
    query: *const ReldexMetadataQuery,
    index: usize,
    out: *mut ReldexColumnInfo,
) -> ReldexStatus {
    entry(|| {
        if query.is_null() {
            return set_last_argument_error("reldex_metadata_query_column: `query` is null");
        }
        // SAFETY: delegated to this function's contract for `query`.
        let query = unsafe { &*query };
        let Some(info) = query.columns.info(index) else {
            set_last_error(DbError::internal(format!(
                "reldex-ffi: reldex_metadata_query_column: column {index} is out of range; the \
                 contract has {} columns",
                query.columns.len()
            )));
            return ReldexStatus::NotFound;
        };
        // SAFETY: delegated to this function's contract for `out`.
        if unsafe { crate::strings::write_out_struct(out, info) } {
            ReldexStatus::Ok
        } else {
            set_last_argument_error(
                "reldex_metadata_query_column: `out` is null, unaligned, or too small",
            )
        }
    })
}

/// Builds an error of `kind_in`/`native_code_in`/`message_in` and runs it
/// through this query's classifier — [`reldex_db_driver_api::PreparedMetadataQuery::reclassify_error`]
/// — returning the (possibly corrected) result as a new, owned
/// [`ReldexError`]. `kind_in` is a [`crate::ReldexErrorKind`]; passing
/// `RELDEX_ERROR_KIND_UNKNOWN` is refused, because a caller building an error
/// has no "unknown to this header" case to name.
///
/// # Safety
///
/// `query` must be a live [`ReldexMetadataQuery`]. `message_in` must point at
/// `message_in.len` readable bytes of UTF-8 text.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_metadata_query_reclassify_error(
    query: *const ReldexMetadataQuery,
    kind_in: i32,
    native_code_in: i32,
    has_native_code_in: bool,
    message_in: ReldexStr,
) -> *mut ReldexError {
    entry_value(std::ptr::null_mut(), || {
        if query.is_null() {
            set_last_argument_error("reldex_metadata_query_reclassify_error: `query` is null");
            return std::ptr::null_mut();
        }
        let Some(kind) =
            ReldexErrorKind::from_i32(kind_in).and_then(ReldexErrorKind::to_error_kind)
        else {
            set_last_argument_error(
                "reldex_metadata_query_reclassify_error: `kind_in` is not a ReldexErrorKind this \
                 build accepts as input",
            );
            return std::ptr::null_mut();
        };
        // SAFETY: delegated to this function's contract for `message_in`.
        let Some(message) = (unsafe { message_in.as_str() }) else {
            set_last_argument_error(
                "reldex_metadata_query_reclassify_error: `message_in` is not valid UTF-8",
            );
            return std::ptr::null_mut();
        };
        let mut built = DbError::new(kind, message.to_owned());
        if has_native_code_in {
            built = built.with_native(NativeError::new(native_code_in, String::new()));
        }
        // SAFETY: delegated to this function's contract for `query`.
        let query = unsafe { &*query };
        let corrected = (query.classifier)(&built).unwrap_or(built);
        Box::into_raw(Box::new(ReldexError::from_db_error(&corrected)))
    })
}

/// Releases a prepared metadata query.
///
/// # Safety
///
/// `query` must be null (a no-op) or a pointer [`reldex_metadata_prepare`]
/// handed out that has not already been released.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_metadata_query_release(query: *mut ReldexMetadataQuery) {
    entry_value((), || {
        if query.is_null() {
            return;
        }
        // SAFETY: delegated to this function's contract.
        drop(unsafe { Box::from_raw(query) });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn str_of(text: &str) -> ReldexStr {
        ReldexStr {
            ptr: text.as_ptr(),
            len: text.len(),
        }
    }

    #[test]
    fn a_schemas_request_prepares_a_statement_with_binds_and_columns() {
        let request = ReldexMetadataRequest {
            struct_size: u32::try_from(size_of::<ReldexMetadataRequest>())
                .expect("ReldexMetadataRequest's size fits in u32"),
            kind: ReldexMetadataRequestKind::Schemas as i32,
            object_kind: 0,
            schema: ReldexStr::empty(),
            table: ReldexStr::empty(),
            name_filter: ReldexStr::empty(),
            has_name_filter: false,
            limit: 100,
        };
        let mut query: *mut ReldexMetadataQuery = std::ptr::null_mut();
        // SAFETY: `request` is a real, fully-initialized local; `query` is a
        // real local pointer.
        let status = unsafe { reldex_metadata_prepare(std::ptr::from_ref(&request), &mut query) };
        assert_eq!(status, ReldexStatus::Ok);
        assert!(!query.is_null());

        // SAFETY: `query` is live.
        let sql = unsafe { reldex_metadata_query_sql(query) };
        // SAFETY: `sql` borrows from `query`, which is still live.
        let sql_text =
            unsafe { sql.as_str() }.expect("the prepared statement's SQL is valid UTF-8");
        assert!(sql_text.contains("ALL_USERS"));
        // SAFETY: `query` is live.
        assert!(unsafe { reldex_metadata_query_bind_count(query) } > 0);
        // SAFETY: `query` is live.
        let columns = unsafe { reldex_metadata_query_column_count(query) };
        assert!(columns > 0);
        let mut info = ReldexColumnInfo::default();
        // SAFETY: `query` is live; `info` is a real local with `struct_size`
        // set.
        let status =
            unsafe { reldex_metadata_query_column(query, 0, std::ptr::from_mut(&mut info)) };
        assert_eq!(status, ReldexStatus::Ok);
        // SAFETY: `info.name` borrows from `query`, still live.
        assert_eq!(unsafe { info.name.as_str() }, Some("name"));

        // SAFETY: `query` is live; the message is real UTF-8.
        let error = unsafe {
            reldex_metadata_query_reclassify_error(
                query,
                ReldexErrorKind::Other as i32,
                123,
                true,
                str_of("no such object"),
            )
        };
        assert!(!error.is_null());
        // SAFETY: `error` is the pointer this call just returned, owned.
        unsafe { crate::error::reldex_error_free(error) };

        // SAFETY: `query` is the pointer `reldex_metadata_prepare` returned.
        unsafe { reldex_metadata_query_release(query) };
    }

    #[test]
    fn an_unknown_request_kind_is_refused() {
        let request = ReldexMetadataRequest {
            struct_size: u32::try_from(size_of::<ReldexMetadataRequest>())
                .expect("ReldexMetadataRequest's size fits in u32"),
            kind: 0,
            object_kind: 0,
            schema: ReldexStr::empty(),
            table: ReldexStr::empty(),
            name_filter: ReldexStr::empty(),
            has_name_filter: false,
            limit: 1,
        };
        let mut query: *mut ReldexMetadataQuery = std::ptr::null_mut();
        // SAFETY: as above.
        let status = unsafe { reldex_metadata_prepare(std::ptr::from_ref(&request), &mut query) };
        assert_eq!(status, ReldexStatus::InvalidArgument);
        assert!(query.is_null());
        crate::error::take_last_error();
    }
}

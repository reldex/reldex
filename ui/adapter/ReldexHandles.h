#pragma once

// Small RAII wrappers and argument helpers for the reldex-ffi C ABI.
//
// Four objects cross the boundary with caller ownership (ADR-0003 D3): a
// fetched `ReldexBatch*`, a `ReldexError*`, a `ReldexTextArena*`, and a
// `SERVER_OUTPUT` event's `ReldexServerOutputLines*`. Each is
// released exactly once by a `std::unique_ptr` with a custom deleter, so no
// early return, exception, or destruction order can leak or double-free one --
// `reldex.h` says releasing twice is undefined behaviour, like every `free`.
//
// Every non-opaque struct the ABI reads or writes begins with `struct_size`,
// which the caller must set before *every* call. The `makeX()` helpers below
// are the only place that is done, so a forgotten field cannot become a silent
// `RELDEX_STATUS_INVALID_ARGUMENT`.

#include <reldex.h>

#include <cstdint>
#include <memory>

namespace reldex {

struct BatchDeleter
{
    void operator()(ReldexBatch *batch) const noexcept { reldex_batch_release(batch); }
};

struct ErrorDeleter
{
    void operator()(ReldexError *error) const noexcept { reldex_error_free(error); }
};

struct ArenaDeleter
{
    void operator()(ReldexTextArena *arena) const noexcept { reldex_text_arena_release(arena); }
};

struct LinesDeleter
{
    void operator()(ReldexServerOutputLines *lines) const noexcept
    {
        reldex_server_output_lines_release(lines);
    }
};

struct HubDeleter
{
    void operator()(ReldexHub *hub) const noexcept { reldex_hub_destroy(hub); }
};

struct WorkspaceDeleter
{
    void operator()(ReldexWorkspace *workspace) const noexcept { reldex_workspace_close(workspace); }
};

struct ProfileListDeleter
{
    void operator()(ReldexProfileList *list) const noexcept { reldex_profile_list_release(list); }
};

struct ConnectSummaryDeleter
{
    void operator()(ReldexConnectSummary *summary) const noexcept
    {
        reldex_connect_summary_release(summary);
    }
};

struct SecretDeleter
{
    void operator()(ReldexSecret *secret) const noexcept { reldex_secret_release(secret); }
};

struct MetadataQueryDeleter
{
    void operator()(ReldexMetadataQuery *query) const noexcept
    {
        reldex_metadata_query_release(query);
    }
};

/// Owns one fetched batch. Released exactly once, when this goes out of scope.
using BatchHandle = std::unique_ptr<ReldexBatch, BatchDeleter>;

/// Owns one failure handed out by an event or by `reldex_last_error_take()`.
using ErrorHandle = std::unique_ptr<ReldexError, ErrorDeleter>;

/// Owns one formatting arena. Views taken from it die with it.
using ArenaHandle = std::unique_ptr<ReldexTextArena, ArenaDeleter>;

/// Owns one `SERVER_OUTPUT` event's lines.
using LinesHandle = std::unique_ptr<ReldexServerOutputLines, LinesDeleter>;

/// Owns a profile list handed out by a workspace reply (`ProfileFetched` /
/// `ProfilesListed`).
using ProfileListHandle = std::unique_ptr<ReldexProfileList, ProfileListDeleter>;

/// Owns a connect summary handed out by `ConnectParamsBuilt`.
using ConnectSummaryHandle = std::unique_ptr<ReldexConnectSummary, ConnectSummaryDeleter>;

/// Owns a secret handed out by `CredentialGot`/`PasswordResolved`, or built
/// locally with `reldex_secret_from_utf8`. Zeroizes on release
/// (`reldex_secret_release`, ADR-0007 S4 "Hygiene").
using SecretHandle = std::unique_ptr<ReldexSecret, SecretDeleter>;

/// Owns the hub.
///
/// A handle rather than a raw pointer so a `Bridge` constructor that fails
/// part of the way through -- or unwinds -- cannot leak the hub and the
/// sessions behind it. The deleter is only `reldex_hub_destroy`; the ordered
/// teardown D5 rule 2 requires (unregister the waker, release every batch,
/// drain) happens in `~Bridge` before this handle is reset.
using HubHandle = std::unique_ptr<ReldexHub, HubDeleter>;

/// Owns the workspace's service thread (settings/profiles/credentials/
/// history/worksheets/layout, `reldex_workspace_open`/`_close`).
/// `reldex_workspace_close` never blocks and must not be followed by a drain
/// (`reldex.h`'s own doc comment); a handle is still the right shape here so
/// a `ConnectionManager` constructor that fails partway through cannot leak
/// the workspace or its service thread.
using WorkspaceHandle = std::unique_ptr<ReldexWorkspace, WorkspaceDeleter>;

/// Owns one prepared metadata statement (M6.1), from `reldex_metadata_prepare`
/// until it goes out of scope.
using MetadataQueryHandle = std::unique_ptr<ReldexMetadataQuery, MetadataQueryDeleter>;

template<typename T>
[[nodiscard]] inline T sized() noexcept
{
    T value {};
    value.struct_size = static_cast<std::uint32_t>(sizeof(T));
    return value;
}

[[nodiscard]] inline ReldexEvent makeEvent() noexcept { return sized<ReldexEvent>(); }
[[nodiscard]] inline ReldexColumnView makeColumnView() noexcept { return sized<ReldexColumnView>(); }
[[nodiscard]] inline ReldexColumnInfo makeColumnInfo() noexcept { return sized<ReldexColumnInfo>(); }
[[nodiscard]] inline ReldexErrorView makeErrorView() noexcept { return sized<ReldexErrorView>(); }
[[nodiscard]] inline ReldexArenaView makeArenaView() noexcept { return sized<ReldexArenaView>(); }
[[nodiscard]] inline ReldexLiveCounts makeLiveCounts() noexcept { return sized<ReldexLiveCounts>(); }
[[nodiscard]] inline ReldexWorkspaceReply makeWorkspaceReply() noexcept
{
    return sized<ReldexWorkspaceReply>();
}
[[nodiscard]] inline ReldexProfileView makeProfileView() noexcept { return sized<ReldexProfileView>(); }
[[nodiscard]] inline ReldexProfileDetails makeProfileDetails() noexcept
{
    return sized<ReldexProfileDetails>();
}
[[nodiscard]] inline ReldexConnectSummaryView makeConnectSummaryView() noexcept
{
    return sized<ReldexConnectSummaryView>();
}
[[nodiscard]] inline ReldexOpenOptions makeOpenOptions() noexcept
{
    ReldexOpenOptions options = sized<ReldexOpenOptions>();
    options.mock = sized<ReldexMockScenarioConfig>();
    return options;
}
[[nodiscard]] inline ReldexMetadataRequest makeMetadataRequest() noexcept
{
    return sized<ReldexMetadataRequest>();
}
[[nodiscard]] inline ReldexFormatOptions makeFormatOptions() noexcept
{
    ReldexFormatOptions options = sized<ReldexFormatOptions>();
    // Everything past `struct_size` documents a zero default except this one:
    // `max_fraction_digits` 0 and -1 both mean "as stored", and -1 is the
    // value reldex-ffi's own Default uses, so it is what a reader of both
    // sides will expect to see.
    options.max_fraction_digits = -1;
    return options;
}

/// Whether row `row` of `view` is SQL NULL. A pure pointer read -- this is on
/// the per-cell path and must never become an FFI call (ADR-0003 D4/D5 rule 4).
[[nodiscard]] inline bool isNullAt(const ReldexColumnView &view, std::size_t row) noexcept
{
    if (view.null_bits == nullptr || row >= view.row_count) {
        return false;
    }
    const std::size_t word = row / 64U;
    if (word >= view.null_word_count) {
        return false;
    }
    return ((view.null_bits[word] >> (row % 64U)) & 1U) != 0U;
}

/// Whether a column's bytes can be handed to `QString::fromUtf8` as they are.
///
/// Only the two kinds `reldex.h` documents as UTF-8 text. Everything else --
/// including `UNSUPPORTED`, which the header explicitly says is "not text" even
/// though it is stored as bytes -- goes through the bulk formatter, which has
/// an answer for every kind.
[[nodiscard]] inline bool isDirectUtf8(std::int32_t kind) noexcept
{
    return kind == RELDEX_COLUMN_KIND_TEXT || kind == RELDEX_COLUMN_KIND_JSON;
}

} // namespace reldex

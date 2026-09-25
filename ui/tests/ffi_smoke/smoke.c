/*
 * ui/tests/ffi_smoke/smoke.c -- a plain, Qt-free smoke test for reldex-ffi
 * (M1.4, ADR-0003 D10 item 2: "ui/tests/ffi_smoke -- a plain C program
 * linking the cdylib, driving the mock driver end to end. No Qt.").
 *
 * This file is compiled twice from one CMakeLists.txt: once as C11
 * (this target) and once as C++17 (a copy with a .cpp extension, so the
 * same logic proves the header is C++-clean too -- see
 * ui/tests/ffi_smoke/CMakeLists.txt). Keep it valid as both languages:
 * no C++-only or C-only constructs, no designated initializers (C++17
 * does not have them), plain field assignment after memset instead.
 *
 * Threading: deliberately does not use C11 <threads.h> (missing on MSVC
 * and macOS's default toolchain) or a platform condvar. The waker sets a
 * plain flag; the main thread polls it with a short sleep under a
 * generous hang guard. This is the simplification the M1.4 brief
 * explicitly allows, and it still proves the waker actually fires --
 * see wait_for_wake() and g_wake_count below. No check here ever asserts
 * an upper bound on how *fast* anything happens, only that it happens
 * within HANG_GUARD_SECONDS -- a stalled/deadlocked boundary should fail
 * the run rather than hang the CI job forever.
 */

#if !defined(_WIN32) && !defined(_POSIX_C_SOURCE)
/* Exposes nanosleep() from <time.h> under strict conformance modes
 * (e.g. -std=c11 without GNU extensions) on glibc; harmless everywhere
 * else. Must be defined before any system header is included. */
#define _POSIX_C_SOURCE 199309L
#endif

#include <reldex.h>

#include <inttypes.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#ifdef _WIN32
#include <windows.h>
#else
#include <unistd.h>
#endif

/* ---------------------------------------------------------------------
 * Small portable helpers: sleep, a hang-guarded wait, and a pass/fail
 * checklist. None of these assert anything about speed.
 * --------------------------------------------------------------------- */

#define HANG_GUARD_SECONDS 60.0

static void smoke_sleep_ms(unsigned ms)
{
#ifdef _WIN32
    Sleep(ms);
#else
    struct timespec ts;
    ts.tv_sec = (time_t)(ms / 1000u);
    ts.tv_nsec = (long)(ms % 1000u) * 1000000L;
    nanosleep(&ts, NULL);
#endif
}

/* Seconds of WALL-CLOCK time since some fixed point in this process.
 *
 * Deliberately not clock(): that is CPU time, and every wait here is spent
 * sleeping, so a clock()-based guard barely advances and a genuine deadlock
 * would hang CI instead of failing it. Monotonic, so a clock adjustment
 * mid-run cannot make a guard fire early or never. */
static double smoke_now_seconds(void)
{
#ifdef _WIN32
    return (double)GetTickCount64() / 1000.0;
#else
    struct timespec now;
    clock_gettime(CLOCK_MONOTONIC, &now);
    return (double)now.tv_sec + (double)now.tv_nsec / 1e9;
#endif
}

static long g_checks_run = 0;
static long g_checks_failed = 0;

/* Prints one line per check, as the M1.4 brief requires, and keeps a
 * running tally so main() can pick the final exit code. */
static void smoke_check(bool condition, const char *what)
{
    g_checks_run += 1;
    if (condition) {
        printf("[PASS] %s\n", what);
    } else {
        g_checks_failed += 1;
        printf("[FAIL] %s\n", what);
    }
    fflush(stdout);
}

/* For a precondition whose failure would make every later dereference
 * undefined (a null hub, a batch that should have been non-null) --
 * prints, then exits immediately rather than limping on into a crash
 * that would obscure the real failure. */
static void smoke_require(bool condition, const char *what)
{
    smoke_check(condition, what);
    if (!condition) {
        fprintf(stderr, "smoke: fatal precondition failed, stopping: %s\n", what);
        exit(1);
    }
}

/* The waker: sets a flag and counts the call. MUST NOT call any
 * reldex_* function (ADR-0003 D5 rule 1 / the header's "THE WAKER"
 * summary) -- and it does not; it only touches its own two counters. */
static volatile int g_wake_flag = 0;
static volatile long g_wake_count = 0;

/* This target is built twice: as C11, and as C++17 from a generated .cpp.
 * In the C++17 build the header must have given us ReldexWakeFnNoexcept --
 * including on MSVC, where __cplusplus stays at 199711L unless
 * /Zc:__cplusplus is passed. Failing to compile is the point: a silent
 * fallback would leave ADR-0003 A24's claim untested on Windows. */
#ifdef __cplusplus
#ifndef RELDEX_HAVE_WAKE_FN_NOEXCEPT
#error "reldex.h did not define ReldexWakeFnNoexcept; this target is built as C++17 (see ui/tests/ffi_smoke/CMakeLists.txt), so the header's guard is wrong -- most likely it tests __cplusplus without _MSVC_LANG"
#endif
#endif

#ifdef __cplusplus
extern "C"
#endif
void
smoke_wake(void *user_data)
#if defined(RELDEX_HAVE_WAKE_FN_NOEXCEPT)
/* Letting an exception out of the trampoline is undefined behaviour and
 * catch_unwind does NOT contain it, so a C++ adapter should say so to the
 * compiler. The header's ReldexWakeFnNoexcept below holds this pointer and
 * converts to ReldexWakeFn implicitly; drop the noexcept and it stops
 * compiling. */
    noexcept
#endif
{
    (void)user_data;
    g_wake_flag = 1;
    g_wake_count += 1;
}

#if defined(RELDEX_HAVE_WAKE_FN_NOEXCEPT)
static ReldexWakeFnNoexcept smoke_waker = smoke_wake;
/* Positive proof that the noexcept branch above was really taken, rather
 * than the whole thing having been preprocessed away: this fails to compile
 * if smoke_wake lost its noexcept, and it is only reachable at all when the
 * header defined the alias. */
static_assert(noexcept(smoke_wake(NULL)), "the waker trampoline must be noexcept");
#else
static ReldexWakeFn smoke_waker = smoke_wake;
#endif

/* Waits for the next wake, resetting the flag first is the CALLER's job
 * (see the call sites): the pattern throughout this file is
 * "reset flag -> submit one request -> wait_for_wake() -> drain to
 * empty", which keeps every wait aligned with the waker's edge-trigger
 * (empty -> non-empty) semantics -- reusing a flag that might already be
 * set from an earlier, undrained cycle would let a wait return early for
 * the wrong reason. Returns true once woken, false if the hang guard
 * elapsed first. */
static bool wait_for_wake(void)
{
    double start = smoke_now_seconds();
    while (!g_wake_flag) {
        double elapsed = smoke_now_seconds() - start;
        if (elapsed > HANG_GUARD_SECONDS) {
            return false;
        }
        smoke_sleep_ms(5);
    }
    return true;
}

/* The live-object counts right now; see reldex_live_counts in the header. */
static ReldexLiveCounts live_counts(void)
{
    ReldexLiveCounts counts;
    memset(&counts, 0, sizeof(counts));
    counts.struct_size = sizeof(counts);
    ReldexStatus status = reldex_live_counts(&counts);
    smoke_check(status == RELDEX_STATUS_OK, "reldex_live_counts succeeds");
    return counts;
}

static uint64_t next_request_id(void)
{
    static uint64_t counter = 0;
    counter += 1;
    return counter;
}

/* ---------------------------------------------------------------------
 * Event helpers.
 * --------------------------------------------------------------------- */

/* Submits nothing itself -- the caller must reset g_wake_flag and submit
 * its request first. Waits for the wake, then takes exactly one event
 * (the queue is expected to be empty before every submission in this
 * harness, so one wake means exactly one event is ready). */
static bool wait_and_take_event(ReldexHub *hub, ReldexEvent *out)
{
    memset(out, 0, sizeof(*out));
    out->struct_size = sizeof(*out);
    if (!wait_for_wake()) {
        return false;
    }
    return reldex_hub_next_event(hub, out) != 0;
}

static void release_event_batch(const ReldexEvent *event)
{
    if (event->batch != NULL) {
        reldex_batch_release(event->batch);
    }
}

/* Reads an error's view, prints its key fields, and frees it. Returns
 * the view by value so the caller can assert on it before it is gone
 * (the strings inside stop being valid once reldex_error_free runs, so
 * this only returns the scalar fields, not the ReldexStr pointers). */
typedef struct {
    int32_t kind;
    bool has_native;
    int32_t native_code;
    bool has_line_column;
    uint32_t line;
    uint32_t column;
} ErrorSummary;

static ErrorSummary read_and_free_error(ReldexError *error)
{
    ErrorSummary summary;
    ReldexErrorView view;
    memset(&summary, 0, sizeof(summary));
    memset(&view, 0, sizeof(view));
    view.struct_size = sizeof(view);

    smoke_require(error != NULL, "an error pointer to read is non-null");
    ReldexStatus status = reldex_error_view(error, &view);
    smoke_check(status == RELDEX_STATUS_OK, "reldex_error_view succeeds");

    if (view.has_native) {
        printf("  error: kind=%d native_code=%d\n", (int)view.kind, (int)view.native_code);
    } else {
        printf("  error: kind=%d native_code=(none)\n", (int)view.kind);
    }
    printf(
        "  error: message=\"%.*s\" native_message=\"%.*s\"\n",
        (int)view.message.len,
        (const char *)view.message.ptr,
        (int)view.native_message.len,
        (const char *)view.native_message.ptr);

    summary.kind = view.kind;
    summary.has_native = view.has_native;
    summary.native_code = view.native_code;
    summary.has_line_column = view.has_line_column;
    summary.line = view.line;
    summary.column = view.column;

    reldex_error_free(error);
    return summary;
}

/* ---------------------------------------------------------------------
 * A private temp directory for the on-disk workspace store (M2.11
 * families 5-7): reldex_workspace_open's `path` is a SQLite file path, not
 * a directory, so this harness makes one directory it fully owns, points
 * the store at a file inside it, and best-effort removes what it created
 * afterwards. Not a general recursive rmdir -- only the exact sibling
 * files SQLite is documented to create next to the main one.
 * --------------------------------------------------------------------- */

#ifdef _WIN32
#define RELDEX_PATH_SEP '\\'
#else
#define RELDEX_PATH_SEP '/'
#endif

#ifdef _WIN32
static bool make_temp_dir(char *out, size_t out_size)
{
    char base[MAX_PATH];
    DWORD base_len = GetTempPathA((DWORD)sizeof(base), base);
    if (base_len == 0 || base_len >= sizeof(base)) {
        return false;
    }
    for (unsigned attempt = 0; attempt < 8; attempt += 1) {
        unsigned unique =
            (unsigned)GetCurrentProcessId() * 2654435761u + (unsigned)GetTickCount() + attempt;
        int written = snprintf(out, out_size, "%sreldex_smoke_%08x", base, unique);
        if (written <= 0 || (size_t)written >= out_size) {
            return false;
        }
        if (CreateDirectoryA(out, NULL)) {
            return true;
        }
    }
    return false;
}

static void remove_temp_file_and_dir(const char *dir, const char *db_file)
{
    const char *suffixes[] = {"", "-wal", "-shm", "-journal"};
    char path[MAX_PATH];
    for (size_t i = 0; i < sizeof(suffixes) / sizeof(suffixes[0]); i += 1) {
        if ((size_t)snprintf(path, sizeof(path), "%s%s", db_file, suffixes[i]) < sizeof(path)) {
            DeleteFileA(path);
        }
    }
    RemoveDirectoryA(dir);
}
#else
static bool make_temp_dir(char *out, size_t out_size)
{
    const char *base = getenv("TMPDIR");
    if (base == NULL || base[0] == '\0') {
        base = "/tmp";
    }
    int written = snprintf(out, out_size, "%s/reldex_smoke_XXXXXX", base);
    if (written <= 0 || (size_t)written >= out_size) {
        return false;
    }
    return mkdtemp(out) != NULL;
}

static void remove_temp_file_and_dir(const char *dir, const char *db_file)
{
    const char *suffixes[] = {"", "-wal", "-shm", "-journal"};
    char path[4096];
    for (size_t i = 0; i < sizeof(suffixes) / sizeof(suffixes[0]); i += 1) {
        if ((size_t)snprintf(path, sizeof(path), "%s%s", db_file, suffixes[i]) < sizeof(path)) {
            remove(path);
        }
    }
    rmdir(dir);
}
#endif

/* ---------------------------------------------------------------------
 * Workspace waker and reply helpers (M2.11 families 5-7). Mirrors the
 * hub's waker/event pair above exactly: a plain flag the waker sets, a
 * hang-guarded wait, and "drain to empty" after every submission -- but
 * against ReldexWorkspace's own, independent queue, never the hub's.
 * --------------------------------------------------------------------- */

static volatile int g_workspace_wake_flag = 0;
static volatile long g_workspace_wake_count = 0;

#ifdef __cplusplus
extern "C"
#endif
void
smoke_workspace_wake(void *user_data)
{
    (void)user_data;
    g_workspace_wake_flag = 1;
    g_workspace_wake_count += 1;
}

static ReldexWorkspaceWakeFn smoke_workspace_waker = smoke_workspace_wake;

static bool wait_for_workspace_wake(void)
{
    double start = smoke_now_seconds();
    while (!g_workspace_wake_flag) {
        double elapsed = smoke_now_seconds() - start;
        if (elapsed > HANG_GUARD_SECONDS) {
            return false;
        }
        smoke_sleep_ms(5);
    }
    return true;
}

/* Same edge-triggered contract as wait_and_take_event: the caller resets
 * g_workspace_wake_flag and submits exactly one request first. */
static bool wait_and_take_workspace_reply(ReldexWorkspace *workspace, ReldexWorkspaceReply *out)
{
    memset(out, 0, sizeof(*out));
    out->struct_size = sizeof(*out);
    if (!wait_for_workspace_wake()) {
        return false;
    }
    return reldex_workspace_next_reply(workspace, out) != 0;
}

/* Releases every object a ReldexWorkspaceReply may own EXCEPT `error`,
 * which each call site checks and frees explicitly (read_and_free_error),
 * matching the rest of this file's discipline of never hiding an error
 * behind a generic cleanup helper. Safe to call on a reply whose owned
 * pointers have already been released and nulled out by hand. */
static void release_workspace_reply_objects(const ReldexWorkspaceReply *reply)
{
    if (reply->profile_list != NULL) {
        reldex_profile_list_release(reply->profile_list);
    }
    if (reply->connect != NULL) {
        reldex_connect_summary_release(reply->connect);
    }
    if (reply->secret != NULL) {
        reldex_secret_release(reply->secret);
    }
    if (reply->history_list != NULL) {
        reldex_history_list_release(reply->history_list);
    }
    if (reply->worksheet_list != NULL) {
        reldex_worksheet_list_release(reply->worksheet_list);
    }
}

static struct ReldexStr str_of(const char *text)
{
    struct ReldexStr s;
    s.ptr = (const uint8_t *)text;
    s.len = strlen(text);
    return s;
}

/* A profile with sane defaults for every field, varying only the port and
 * whether the endpoint names a service name or a SID -- the same shape
 * crates/ffi/src/workspace.rs's own `#[cfg(test)]` fixture uses, so this
 * harness exercises exactly the inputs already proven to round-trip
 * through Profile::create and the store. */
static ReldexProfileDetails make_profile_details(uint16_t port, bool sid)
{
    ReldexProfileDetails details;
    memset(&details, 0, sizeof(details));
    details.struct_size = sizeof(details);
    details.name = str_of("Orders (smoke)");
    details.database_type = RELDEX_DATABASE_TYPE_ORACLE;
    details.environment = RELDEX_ENVIRONMENT_KIND_TEST;
    details.environment_label = str_of("");
    details.treat_as_production = false;
    details.endpoint_kind = RELDEX_ENDPOINT_KIND_HOST_PORT;
    details.host = str_of("db.example.internal");
    details.port = port;
    details.service_target_kind =
        sid ? RELDEX_SERVICE_TARGET_KIND_SID : RELDEX_SERVICE_TARGET_KIND_SERVICE_NAME;
    details.service_name_or_sid = str_of("ORDERS");
    details.connect_string = str_of("");
    details.auth_kind = RELDEX_AUTH_KIND_PASSWORD;
    details.username = str_of("app_owner");
    details.password_storage = RELDEX_PASSWORD_STORAGE_KIND_CREDENTIAL_STORE;
    details.role = RELDEX_SESSION_ROLE_KIND_NORMAL;
    details.transport = RELDEX_TRANSPORT_KIND_PLAIN;
    details.ca_directory = str_of("");
    details.allow_unenforced_certificate_pin = false;
    return details;
}

/* M2.11 families 5-7: settings, profiles, credentials, history, worksheets
 * and layout, all through one ReldexWorkspace on its own service thread.
 * Run after the hub above is fully torn down (see the call site), so this
 * function's own reldex_live_counts bookkeeping starts from a clean slate
 * and its final check at the bottom is unambiguous. */
static void smoke_run_workspace_checks(void)
{
    char temp_dir[512];
    bool have_temp_dir = make_temp_dir(temp_dir, sizeof(temp_dir));
    smoke_require(have_temp_dir, "a private temp directory for the on-disk store was created");

    char db_path[600];
    int path_written = snprintf(
        db_path, sizeof(db_path), "%s%creldex_smoke.sqlite3", temp_dir, RELDEX_PATH_SEP);
    smoke_require(
        path_written > 0 && (size_t)path_written < sizeof(db_path),
        "the store's file path fits in the buffer");

    ReldexLiveCounts workspace_baseline = live_counts();

    struct ReldexStr path;
    path.ptr = (const uint8_t *)db_path;
    path.len = (size_t)path_written;

    ReldexWorkspace *workspace = NULL;
    uint64_t open_request = next_request_id();
    g_workspace_wake_flag = 0;
    ReldexStatus status = reldex_workspace_open(
        path, /* in_memory */ false, /* use_memory_credential_store */ true, open_request,
        &workspace);
    smoke_require(status == RELDEX_STATUS_OK, "reldex_workspace_open is accepted");
    smoke_require(workspace != NULL, "reldex_workspace_open returns a handle immediately");

    status = reldex_workspace_set_waker(workspace, smoke_workspace_waker, NULL);
    smoke_require(status == RELDEX_STATUS_OK, "reldex_workspace_set_waker(smoke_workspace_wake) succeeds");

    ReldexWorkspaceReply reply;
    smoke_require(wait_and_take_workspace_reply(workspace, &reply), "the workspace's OPENED reply arrives");
    smoke_check(reply.kind == RELDEX_WORKSPACE_REPLY_KIND_OPENED, "the workspace's first reply is OPENED");
    smoke_check(reply.request == open_request, "the OPENED reply carries the open request id");
    smoke_check(reply.error == NULL, "the on-disk store opened without an error");
    if (reply.error != NULL) {
        read_and_free_error(reply.error);
    }
    release_workspace_reply_objects(&reply);

    /* ---- Settings (family 5): resolve, set, resolve, clear, resolve. ---- */
    {
        uint64_t r1 = next_request_id();
        g_workspace_wake_flag = 0;
        status = reldex_workspace_resolve_setting(workspace, r1, RELDEX_SETTING_ID_FETCH_ROWS, NULL, NULL);
        smoke_require(status == RELDEX_STATUS_OK, "resolve_setting(FETCH_ROWS) is accepted");
        smoke_require(wait_and_take_workspace_reply(workspace, &reply), "the FETCH_ROWS resolve reply arrives");
        smoke_check(reply.kind == RELDEX_WORKSPACE_REPLY_KIND_SETTING_RESOLVED, "the reply is SETTING_RESOLVED");
        smoke_check(reply.error == NULL, "resolving FETCH_ROWS did not fail");
        smoke_check(
            reply.setting_source == RELDEX_SETTING_LEVEL_BUILT_IN,
            "with no profile/worksheet layer, FETCH_ROWS resolves from BUILT_IN");
        smoke_check(reply.setting_value.kind == RELDEX_VALUE_KIND_COUNT, "FETCH_ROWS is a Count setting");
        printf("  FETCH_ROWS built-in default = %u\n", (unsigned)reply.setting_value.count_value);
        release_workspace_reply_objects(&reply);

        ReldexSettingValue new_value;
        memset(&new_value, 0, sizeof(new_value));
        new_value.struct_size = sizeof(new_value);
        new_value.kind = RELDEX_VALUE_KIND_COUNT;
        new_value.count_value = 250;

        uint64_t r2 = next_request_id();
        g_workspace_wake_flag = 0;
        status = reldex_workspace_set_setting(
            workspace, r2, RELDEX_SETTING_ID_FETCH_ROWS, RELDEX_SETTING_LEVEL_APPLICATION, NULL,
            &new_value);
        smoke_require(status == RELDEX_STATUS_OK, "set_setting(FETCH_ROWS, Application) is accepted");
        smoke_require(wait_and_take_workspace_reply(workspace, &reply), "the set_setting reply arrives");
        smoke_check(reply.error == NULL, "setting FETCH_ROWS did not fail");
        release_workspace_reply_objects(&reply);

        uint64_t r3 = next_request_id();
        g_workspace_wake_flag = 0;
        status = reldex_workspace_resolve_setting(workspace, r3, RELDEX_SETTING_ID_FETCH_ROWS, NULL, NULL);
        smoke_require(status == RELDEX_STATUS_OK, "re-resolving FETCH_ROWS is accepted");
        smoke_require(wait_and_take_workspace_reply(workspace, &reply), "the re-resolve reply arrives");
        smoke_check(
            reply.setting_source == RELDEX_SETTING_LEVEL_APPLICATION,
            "FETCH_ROWS now resolves from APPLICATION");
        smoke_check(reply.setting_value.count_value == 250, "FETCH_ROWS resolves to the value just set");
        release_workspace_reply_objects(&reply);

        uint64_t r4 = next_request_id();
        g_workspace_wake_flag = 0;
        status = reldex_workspace_clear_setting(
            workspace, r4, RELDEX_SETTING_ID_FETCH_ROWS, RELDEX_SETTING_LEVEL_APPLICATION, NULL);
        smoke_require(status == RELDEX_STATUS_OK, "clear_setting(FETCH_ROWS, Application) is accepted");
        smoke_require(wait_and_take_workspace_reply(workspace, &reply), "the clear_setting reply arrives");
        smoke_check(reply.kind == RELDEX_WORKSPACE_REPLY_KIND_SETTING_CLEARED, "the reply is SETTING_CLEARED");
        smoke_check(reply.found, "a value existed at Application scope before it was cleared");
        release_workspace_reply_objects(&reply);

        uint64_t r5 = next_request_id();
        g_workspace_wake_flag = 0;
        status = reldex_workspace_resolve_setting(workspace, r5, RELDEX_SETTING_ID_FETCH_ROWS, NULL, NULL);
        smoke_require(status == RELDEX_STATUS_OK, "resolving FETCH_ROWS after clearing is accepted");
        smoke_require(wait_and_take_workspace_reply(workspace, &reply), "the post-clear resolve reply arrives");
        smoke_check(
            reply.setting_source == RELDEX_SETTING_LEVEL_BUILT_IN,
            "FETCH_ROWS reverts to BUILT_IN once cleared");
        release_workspace_reply_objects(&reply);
    }

    /* ---- Profiles (family 5): create, list, get, update, and refuse a
     * credential-looking endpoint. Password auth + CredentialStore storage,
     * so the same two profiles also serve the credentials/resolve_password
     * sections below. ---- */
    uint8_t service_name_profile_id[16];
    uint8_t sid_profile_id[16];
    {
        ReldexProfileDetails service_name_details = make_profile_details(1521, false);
        uint64_t r1 = next_request_id();
        g_workspace_wake_flag = 0;
        status = reldex_workspace_create_profile(workspace, r1, &service_name_details);
        smoke_require(status == RELDEX_STATUS_OK, "create_profile(service name) is accepted");
        smoke_require(
            wait_and_take_workspace_reply(workspace, &reply),
            "the service-name profile's ProfileSaved reply arrives");
        smoke_check(reply.kind == RELDEX_WORKSPACE_REPLY_KIND_PROFILE_SAVED, "the reply is PROFILE_SAVED");
        smoke_require(reply.error == NULL, "creating the service-name profile did not fail");
        memcpy(service_name_profile_id, reply.id, sizeof(service_name_profile_id));
        release_workspace_reply_objects(&reply);

        ReldexProfileDetails sid_details = make_profile_details(1522, true);
        uint64_t r2 = next_request_id();
        g_workspace_wake_flag = 0;
        status = reldex_workspace_create_profile(workspace, r2, &sid_details);
        smoke_require(status == RELDEX_STATUS_OK, "create_profile(SID) is accepted");
        smoke_require(
            wait_and_take_workspace_reply(workspace, &reply), "the SID profile's ProfileSaved reply arrives");
        smoke_require(reply.error == NULL, "creating the SID profile did not fail");
        memcpy(sid_profile_id, reply.id, sizeof(sid_profile_id));
        release_workspace_reply_objects(&reply);

        uint64_t r3 = next_request_id();
        g_workspace_wake_flag = 0;
        status = reldex_workspace_list_profiles(workspace, r3);
        smoke_require(status == RELDEX_STATUS_OK, "list_profiles is accepted");
        smoke_require(wait_and_take_workspace_reply(workspace, &reply), "the ProfilesListed reply arrives");
        smoke_check(reply.kind == RELDEX_WORKSPACE_REPLY_KIND_PROFILES_LISTED, "the reply is PROFILES_LISTED");
        smoke_require(reply.profile_list != NULL, "the profile list is present");
        smoke_check(reldex_profile_list_count(reply.profile_list) == 2, "both profiles are listed");
        release_workspace_reply_objects(&reply);

        uint64_t r4 = next_request_id();
        g_workspace_wake_flag = 0;
        status = reldex_workspace_get_profile(workspace, r4, service_name_profile_id);
        smoke_require(status == RELDEX_STATUS_OK, "get_profile is accepted");
        smoke_require(wait_and_take_workspace_reply(workspace, &reply), "the ProfileFetched reply arrives");
        smoke_check(reply.found, "the service-name profile is found by id");
        smoke_require(reply.profile_list != NULL, "a fetched profile still hands back a (1-entry) list");
        smoke_check(reldex_profile_list_count(reply.profile_list) == 1, "the fetched list has exactly one entry");
        {
            ReldexProfileView view;
            memset(&view, 0, sizeof(view));
            view.struct_size = sizeof(view);
            bool got = reldex_profile_list_get(reply.profile_list, 0, &view);
            smoke_check(got, "reldex_profile_list_get reads the one entry");
            smoke_check(
                view.host.ptr != NULL && view.host.ptr[view.host.len] == '\0',
                "the fetched profile's host is a usable C string");
            smoke_check(view.port == 1521, "the fetched profile's port round-trips");
            smoke_check(
                view.endpoint_kind == RELDEX_ENDPOINT_KIND_HOST_PORT,
                "the fetched profile's endpoint kind round-trips");
            printf(
                "  profile: name=\"%.*s\" host=\"%.*s\" port=%u\n",
                (int)view.name.len, (const char *)view.name.ptr, (int)view.host.len,
                (const char *)view.host.ptr, (unsigned)view.port);
        }
        release_workspace_reply_objects(&reply);

        /* update_profile: rename the service-name profile in place. */
        ReldexProfileDetails renamed = service_name_details;
        renamed.name = str_of("Orders (renamed)");
        uint64_t r5 = next_request_id();
        g_workspace_wake_flag = 0;
        status = reldex_workspace_update_profile(workspace, r5, service_name_profile_id, &renamed);
        smoke_require(status == RELDEX_STATUS_OK, "update_profile is accepted");
        smoke_require(wait_and_take_workspace_reply(workspace, &reply), "the update reply arrives");
        smoke_check(reply.error == NULL, "renaming the profile did not fail");
        release_workspace_reply_objects(&reply);

        /* A credential-looking connect string is refused, not stored --
         * value-free (the pattern is named, never the text). */
        ReldexProfileDetails credential_looking = make_profile_details(1521, false);
        credential_looking.endpoint_kind = RELDEX_ENDPOINT_KIND_CONNECT_STRING;
        credential_looking.connect_string = str_of("scott/tiger@//db.example.internal:1521/ORDERS");
        uint64_t r6 = next_request_id();
        g_workspace_wake_flag = 0;
        status = reldex_workspace_create_profile(workspace, r6, &credential_looking);
        smoke_require(status == RELDEX_STATUS_OK, "create_profile(credential-looking) is accepted at the ABI level");
        smoke_require(wait_and_take_workspace_reply(workspace, &reply), "the refused create's reply arrives");
        smoke_check(reply.error != NULL, "a credential-looking endpoint is refused, not stored");
        if (reply.error != NULL) {
            read_and_free_error(reply.error);
        }
        release_workspace_reply_objects(&reply);

        uint64_t r7 = next_request_id();
        g_workspace_wake_flag = 0;
        status = reldex_workspace_list_profiles(workspace, r7);
        smoke_require(status == RELDEX_STATUS_OK, "re-listing profiles is accepted");
        smoke_require(wait_and_take_workspace_reply(workspace, &reply), "the re-list reply arrives");
        smoke_check(
            reldex_profile_list_count(reply.profile_list) == 2,
            "the refused profile was never stored (still 2 profiles)");
        release_workspace_reply_objects(&reply);
    }

    /* ---- Connection parameters (family 5): map a service-name profile and
     * a SID profile through the real Oracle driver binding. Dedicated,
     * External-auth profiles: connection_params needs no password for
     * External authentication, so this section can pass password = NULL
     * and still succeed. Deliberately separate from the Password-auth
     * profiles above -- build_connect_params(NULL) on THOSE would (and
     * should) come back as ConnectError::PasswordRequired. */
    {
        ReldexProfileDetails connect_service_details = make_profile_details(1521, false);
        connect_service_details.auth_kind = RELDEX_AUTH_KIND_EXTERNAL;
        uint8_t connect_service_id[16];
        uint64_t r0 = next_request_id();
        g_workspace_wake_flag = 0;
        status = reldex_workspace_create_profile(workspace, r0, &connect_service_details);
        smoke_require(status == RELDEX_STATUS_OK, "create_profile(connect-params, service name) is accepted");
        smoke_require(wait_and_take_workspace_reply(workspace, &reply), "its ProfileSaved reply arrives");
        smoke_require(reply.error == NULL, "creating the External-auth service-name profile did not fail");
        memcpy(connect_service_id, reply.id, sizeof(connect_service_id));
        release_workspace_reply_objects(&reply);

        uint64_t r1 = next_request_id();
        g_workspace_wake_flag = 0;
        status = reldex_workspace_build_connect_params(workspace, r1, connect_service_id, NULL);
        smoke_require(status == RELDEX_STATUS_OK, "build_connect_params(service name) is accepted");
        smoke_require(
            wait_and_take_workspace_reply(workspace, &reply),
            "the service-name ConnectParamsBuilt reply arrives");
        smoke_check(
            reply.kind == RELDEX_WORKSPACE_REPLY_KIND_CONNECT_PARAMS_BUILT,
            "the reply is CONNECT_PARAMS_BUILT");
        smoke_require(
            reply.error == NULL,
            "building connect params for the External-auth service-name profile did not fail");
        smoke_require(reply.connect != NULL, "the connect summary is present");
        {
            ReldexConnectSummaryView connect_view;
            memset(&connect_view, 0, sizeof(connect_view));
            connect_view.struct_size = sizeof(connect_view);
            bool got_view = reldex_connect_summary_view(reply.connect, &connect_view);
            smoke_check(got_view, "reldex_connect_summary_view succeeds");
            smoke_check(
                connect_view.endpoint_kind == RELDEX_ENDPOINT_KIND_HOST_PORT,
                "the service-name profile maps to a HOST_PORT endpoint");
            smoke_check(connect_view.port == 1521, "the mapped endpoint's port round-trips");
            smoke_check(connect_view.rewrite_trigger_ddl, "REWRITE_TRIGGER_DDL defaults to on");
        }
        reldex_connect_summary_release(reply.connect);
        reply.connect = NULL;
        release_workspace_reply_objects(&reply);

        ReldexProfileDetails connect_sid_details = make_profile_details(1522, true);
        connect_sid_details.auth_kind = RELDEX_AUTH_KIND_EXTERNAL;
        uint8_t connect_sid_id[16];
        uint64_t r2 = next_request_id();
        g_workspace_wake_flag = 0;
        status = reldex_workspace_create_profile(workspace, r2, &connect_sid_details);
        smoke_require(status == RELDEX_STATUS_OK, "create_profile(connect-params, SID) is accepted");
        smoke_require(wait_and_take_workspace_reply(workspace, &reply), "its ProfileSaved reply arrives");
        smoke_require(reply.error == NULL, "creating the External-auth SID profile did not fail");
        memcpy(connect_sid_id, reply.id, sizeof(connect_sid_id));
        release_workspace_reply_objects(&reply);

        uint64_t r3 = next_request_id();
        g_workspace_wake_flag = 0;
        status = reldex_workspace_build_connect_params(workspace, r3, connect_sid_id, NULL);
        smoke_require(status == RELDEX_STATUS_OK, "build_connect_params(SID) is accepted");
        smoke_require(
            wait_and_take_workspace_reply(workspace, &reply), "the SID ConnectParamsBuilt reply arrives");
        smoke_require(reply.error == NULL, "building connect params for the SID profile did not fail");
        smoke_require(reply.connect != NULL, "the connect summary is present");
        {
            ReldexConnectSummaryView connect_view;
            memset(&connect_view, 0, sizeof(connect_view));
            connect_view.struct_size = sizeof(connect_view);
            bool got_view = reldex_connect_summary_view(reply.connect, &connect_view);
            smoke_check(got_view, "reldex_connect_summary_view succeeds for the SID profile");
            smoke_check(
                connect_view.connect_string.ptr != NULL && connect_view.connect_string.len > 0,
                "a SID profile maps onto a connect string -- there is no vendor-neutral SID endpoint shape");
            printf(
                "  SID connect string: %.*s\n", (int)connect_view.connect_string.len,
                (const char *)connect_view.connect_string.ptr);
        }
        reldex_connect_summary_release(reply.connect);
        reply.connect = NULL;
        release_workspace_reply_objects(&reply);
    }

    /* ---- Credentials (family 6): put/get/delete, keyed by profile id. ---- */
    {
        uint64_t r1 = next_request_id();
        g_workspace_wake_flag = 0;
        status = reldex_workspace_credential_get(workspace, r1, service_name_profile_id);
        smoke_require(status == RELDEX_STATUS_OK, "credential_get (nothing stored yet) is accepted");
        smoke_require(wait_and_take_workspace_reply(workspace, &reply), "the first credential_get reply arrives");
        smoke_check(!reply.found, "no password is stored yet");
        smoke_check(reply.secret == NULL, "no secret comes back when nothing is stored");
        release_workspace_reply_objects(&reply);

        static const char password_text[] = "reldex-smoke-marker-9f2c";
        uint64_t r2 = next_request_id();
        g_workspace_wake_flag = 0;
        status = reldex_workspace_credential_put(workspace, r2, service_name_profile_id, str_of(password_text));
        smoke_require(status == RELDEX_STATUS_OK, "credential_put is accepted");
        smoke_require(wait_and_take_workspace_reply(workspace, &reply), "the credential_put reply arrives");
        smoke_check(reply.error == NULL, "storing the password did not fail");
        release_workspace_reply_objects(&reply);

        uint64_t r3 = next_request_id();
        g_workspace_wake_flag = 0;
        status = reldex_workspace_credential_get(workspace, r3, service_name_profile_id);
        smoke_require(status == RELDEX_STATUS_OK, "credential_get (now stored) is accepted");
        smoke_require(wait_and_take_workspace_reply(workspace, &reply), "the second credential_get reply arrives");
        smoke_check(reply.found, "the password is now found");
        smoke_require(reply.secret != NULL, "a secret comes back once a password is stored");
        {
            ReldexStr exposed = reldex_secret_expose(reply.secret);
            /* ReldexSecret's text is the one documented exception to the
             * outbound-NUL-termination promise (it borrows from a
             * zeroize-backed Secret) -- compare by length, never assume
             * exposed.ptr[exposed.len] == '\0'. */
            smoke_check(
                exposed.len == sizeof(password_text) - 1
                    && memcmp(exposed.ptr, password_text, exposed.len) == 0,
                "the exposed secret's text matches what was stored");
        }
        reldex_secret_release(reply.secret);
        reply.secret = NULL;
        release_workspace_reply_objects(&reply);

        uint64_t r4 = next_request_id();
        g_workspace_wake_flag = 0;
        status = reldex_workspace_credential_delete(workspace, r4, service_name_profile_id);
        smoke_require(status == RELDEX_STATUS_OK, "credential_delete is accepted");
        smoke_require(wait_and_take_workspace_reply(workspace, &reply), "the credential_delete reply arrives");
        smoke_check(reply.kind == RELDEX_WORKSPACE_REPLY_KIND_CREDENTIAL_DELETED, "the reply is CREDENTIAL_DELETED");
        smoke_check(reply.found, "the deleted credential had existed");
        release_workspace_reply_objects(&reply);
    }

    /* ---- resolve_password (family 6): PromptRequired/NotStored, then
     * FromStore once a password is put. ---- */
    {
        uint64_t r1 = next_request_id();
        g_workspace_wake_flag = 0;
        status = reldex_workspace_resolve_password(workspace, r1, service_name_profile_id);
        smoke_require(status == RELDEX_STATUS_OK, "resolve_password (nothing stored) is accepted");
        smoke_require(wait_and_take_workspace_reply(workspace, &reply), "the resolve_password reply arrives");
        smoke_check(reply.kind == RELDEX_WORKSPACE_REPLY_KIND_PASSWORD_RESOLVED, "the reply is PASSWORD_RESOLVED");
        smoke_check(
            reply.password_source_kind == RELDEX_PASSWORD_SOURCE_KIND_PROMPT_REQUIRED,
            "with nothing stored, a password prompt is required");
        smoke_check(
            reply.prompt_reason == RELDEX_PROMPT_REASON_KIND_NOT_STORED,
            "the reason is that the store holds nothing for this profile");
        release_workspace_reply_objects(&reply);

        static const char password_text[] = "reldex-smoke-marker-2nd";
        uint64_t r2 = next_request_id();
        g_workspace_wake_flag = 0;
        status = reldex_workspace_credential_put(workspace, r2, service_name_profile_id, str_of(password_text));
        smoke_require(status == RELDEX_STATUS_OK, "credential_put (for resolve_password) is accepted");
        smoke_require(wait_and_take_workspace_reply(workspace, &reply), "the credential_put reply arrives");
        release_workspace_reply_objects(&reply);

        uint64_t r3 = next_request_id();
        g_workspace_wake_flag = 0;
        status = reldex_workspace_resolve_password(workspace, r3, service_name_profile_id);
        smoke_require(status == RELDEX_STATUS_OK, "resolve_password (now stored) is accepted");
        smoke_require(wait_and_take_workspace_reply(workspace, &reply), "the second resolve_password reply arrives");
        smoke_check(
            reply.password_source_kind == RELDEX_PASSWORD_SOURCE_KIND_FROM_STORE,
            "once stored, resolve_password reports FromStore");
        smoke_require(reply.secret != NULL, "a FromStore resolution carries a secret");
        reldex_secret_release(reply.secret);
        reply.secret = NULL;
        release_workspace_reply_objects(&reply);
    }

    /* ---- History (family 7): record, list, clear. ---- */
    {
        uint64_t r1 = next_request_id();
        g_workspace_wake_flag = 0;
        status = reldex_workspace_record_history(
            workspace, r1, service_name_profile_id, 1700000000000ULL,
            str_of("SELECT 1 FROM dual"), RELDEX_HISTORY_OUTCOME_KIND_SUCCEEDED, 0, false, 5, 1,
            true);
        smoke_require(status == RELDEX_STATUS_OK, "record_history (succeeded) is accepted");
        smoke_require(wait_and_take_workspace_reply(workspace, &reply), "the first record_history reply arrives");
        smoke_check(reply.kind == RELDEX_WORKSPACE_REPLY_KIND_HISTORY_RECORDED, "the reply is HISTORY_RECORDED");
        smoke_check(reply.error == NULL, "recording a succeeded statement did not fail");
        release_workspace_reply_objects(&reply);

        uint64_t r2 = next_request_id();
        g_workspace_wake_flag = 0;
        status = reldex_workspace_record_history(
            workspace, r2, service_name_profile_id, 1700000005000ULL,
            str_of("SELECT * FROM nonexistent_table"), RELDEX_HISTORY_OUTCOME_KIND_FAILED, 942,
            true, 3, 0, false);
        smoke_require(status == RELDEX_STATUS_OK, "record_history (failed) is accepted");
        smoke_require(wait_and_take_workspace_reply(workspace, &reply), "the second record_history reply arrives");
        smoke_check(reply.error == NULL, "recording a failed statement's outcome did not itself fail");
        release_workspace_reply_objects(&reply);

        uint64_t r3 = next_request_id();
        g_workspace_wake_flag = 0;
        status = reldex_workspace_list_history(workspace, r3, service_name_profile_id, 10, 0, false);
        smoke_require(status == RELDEX_STATUS_OK, "list_history is accepted");
        smoke_require(wait_and_take_workspace_reply(workspace, &reply), "the list_history reply arrives");
        smoke_check(reply.kind == RELDEX_WORKSPACE_REPLY_KIND_HISTORY_LISTED, "the reply is HISTORY_LISTED");
        smoke_require(reply.history_list != NULL, "the history list is present");
        smoke_check(reldex_history_list_count(reply.history_list) == 2, "both recorded statements are listed");
        {
            ReldexHistoryRecordView record_view;
            memset(&record_view, 0, sizeof(record_view));
            record_view.struct_size = sizeof(record_view);
            bool got_record = reldex_history_list_get(reply.history_list, 0, &record_view);
            smoke_check(got_record, "reldex_history_list_get reads the newest entry");
            printf(
                "  history[0]: outcome=%d statement=\"%.*s\"\n", (int)record_view.outcome_kind,
                (int)record_view.statement.len, (const char *)record_view.statement.ptr);
        }
        release_workspace_reply_objects(&reply);

        uint64_t r4 = next_request_id();
        g_workspace_wake_flag = 0;
        status = reldex_workspace_clear_history(workspace, r4, service_name_profile_id);
        smoke_require(status == RELDEX_STATUS_OK, "clear_history is accepted");
        smoke_require(wait_and_take_workspace_reply(workspace, &reply), "the clear_history reply arrives");
        smoke_check(reply.kind == RELDEX_WORKSPACE_REPLY_KIND_HISTORY_CLEARED, "the reply is HISTORY_CLEARED");
        smoke_check(reply.count == 2, "clear_history reports how many entries were removed");
        release_workspace_reply_objects(&reply);

        uint64_t r5 = next_request_id();
        g_workspace_wake_flag = 0;
        status = reldex_workspace_list_history(workspace, r5, service_name_profile_id, 10, 0, false);
        smoke_require(status == RELDEX_STATUS_OK, "re-listing history after clearing is accepted");
        smoke_require(
            wait_and_take_workspace_reply(workspace, &reply), "the post-clear list_history reply arrives");
        smoke_check(reldex_history_list_count(reply.history_list) == 0, "the history is empty after clearing");
        release_workspace_reply_objects(&reply);
    }

    /* ---- Worksheets and layout (family 7). ---- */
    {
        uint64_t r0 = next_request_id();
        g_workspace_wake_flag = 0;
        status = reldex_workspace_load_layout(workspace, r0);
        smoke_require(status == RELDEX_STATUS_OK, "load_layout (nothing saved yet) is accepted");
        smoke_require(wait_and_take_workspace_reply(workspace, &reply), "the first load_layout reply arrives");
        smoke_check(!reply.found, "no layout has been saved yet");
        release_workspace_reply_objects(&reply);

        uint8_t worksheet_id[16];
        reldex_workspace_new_worksheet_id(worksheet_id);

        uint64_t r1 = next_request_id();
        g_workspace_wake_flag = 0;
        status = reldex_workspace_save_worksheet(
            workspace, r1, worksheet_id, NULL, str_of("scratch"), str_of("select 1 from dual;"), 0,
            0, 0);
        smoke_require(status == RELDEX_STATUS_OK, "save_worksheet is accepted");
        smoke_require(wait_and_take_workspace_reply(workspace, &reply), "the save_worksheet reply arrives");
        smoke_check(reply.kind == RELDEX_WORKSPACE_REPLY_KIND_WORKSHEET_SAVED, "the reply is WORKSHEET_SAVED");
        smoke_check(reply.error == NULL, "saving the worksheet did not fail");
        release_workspace_reply_objects(&reply);

        uint64_t r2 = next_request_id();
        g_workspace_wake_flag = 0;
        status = reldex_workspace_load_worksheets(workspace, r2);
        smoke_require(status == RELDEX_STATUS_OK, "load_worksheets is accepted");
        smoke_require(wait_and_take_workspace_reply(workspace, &reply), "the load_worksheets reply arrives");
        smoke_require(reply.worksheet_list != NULL, "the worksheet list is present");
        smoke_check(reldex_worksheet_list_count(reply.worksheet_list) == 1, "the saved worksheet is loaded back");
        {
            ReldexWorksheetView worksheet_view;
            memset(&worksheet_view, 0, sizeof(worksheet_view));
            worksheet_view.struct_size = sizeof(worksheet_view);
            bool got_worksheet = reldex_worksheet_list_get(reply.worksheet_list, 0, &worksheet_view);
            smoke_check(got_worksheet, "reldex_worksheet_list_get reads the one entry");
            smoke_check(
                worksheet_view.text.len == strlen("select 1 from dual;")
                    && memcmp(worksheet_view.text.ptr, "select 1 from dual;", worksheet_view.text.len)
                           == 0,
                "the loaded worksheet's text matches what was saved");
        }
        release_workspace_reply_objects(&reply);

        ReldexLayout layout;
        memset(&layout, 0, sizeof(layout));
        layout.struct_size = sizeof(layout);
        layout.has_active_worksheet = true;
        memcpy(layout.active_worksheet, worksheet_id, sizeof(layout.active_worksheet));
        layout.has_object_browser_width = true;
        layout.object_browser_width = 240;
        layout.window_maximized = true;

        uint64_t r3 = next_request_id();
        g_workspace_wake_flag = 0;
        status = reldex_workspace_save_layout(workspace, r3, &layout);
        smoke_require(status == RELDEX_STATUS_OK, "save_layout is accepted");
        smoke_require(wait_and_take_workspace_reply(workspace, &reply), "the save_layout reply arrives");
        smoke_check(reply.error == NULL, "saving the layout did not fail");
        release_workspace_reply_objects(&reply);

        uint64_t r4 = next_request_id();
        g_workspace_wake_flag = 0;
        status = reldex_workspace_load_layout(workspace, r4);
        smoke_require(status == RELDEX_STATUS_OK, "load_layout (now saved) is accepted");
        smoke_require(wait_and_take_workspace_reply(workspace, &reply), "the second load_layout reply arrives");
        smoke_check(reply.found, "the saved layout is found");
        smoke_check(reply.layout.has_active_worksheet, "the loaded layout's active worksheet flag round-trips");
        smoke_check(
            memcmp(reply.layout.active_worksheet, worksheet_id, sizeof(worksheet_id)) == 0,
            "the loaded layout's active worksheet id round-trips");
        smoke_check(reply.layout.object_browser_width == 240, "the loaded layout's object browser width round-trips");
        smoke_check(reply.layout.window_maximized, "the loaded layout's maximized flag round-trips");
        release_workspace_reply_objects(&reply);

        uint64_t r5 = next_request_id();
        g_workspace_wake_flag = 0;
        status = reldex_workspace_delete_worksheet(workspace, r5, worksheet_id);
        smoke_require(status == RELDEX_STATUS_OK, "delete_worksheet is accepted");
        smoke_require(wait_and_take_workspace_reply(workspace, &reply), "the delete_worksheet reply arrives");
        smoke_check(reply.kind == RELDEX_WORKSPACE_REPLY_KIND_WORKSHEET_DELETED, "the reply is WORKSHEET_DELETED");
        smoke_check(reply.found, "the deleted worksheet had existed");
        release_workspace_reply_objects(&reply);
    }

    /* ---- Cleanup: delete the two CRUD/credentials profiles. The two
     * connect-params-only profiles above are left in the store -- harmless,
     * since the whole temp file is removed at the end of this function. ---- */
    {
        uint64_t r1 = next_request_id();
        g_workspace_wake_flag = 0;
        status = reldex_workspace_delete_profile(workspace, r1, service_name_profile_id);
        smoke_require(status == RELDEX_STATUS_OK, "delete_profile(service name) is accepted");
        smoke_require(wait_and_take_workspace_reply(workspace, &reply), "the first delete_profile reply arrives");
        smoke_check(reply.kind == RELDEX_WORKSPACE_REPLY_KIND_PROFILE_DELETED, "the reply is PROFILE_DELETED");
        smoke_check(reply.found, "the deleted profile had existed");
        release_workspace_reply_objects(&reply);

        uint64_t r2 = next_request_id();
        g_workspace_wake_flag = 0;
        status = reldex_workspace_delete_profile(workspace, r2, sid_profile_id);
        smoke_require(status == RELDEX_STATUS_OK, "delete_profile(SID) is accepted");
        smoke_require(wait_and_take_workspace_reply(workspace, &reply), "the second delete_profile reply arrives");
        smoke_check(reply.found, "the second deleted profile had existed");
        release_workspace_reply_objects(&reply);

        uint64_t r3 = next_request_id();
        g_workspace_wake_flag = 0;
        status = reldex_workspace_get_profile(workspace, r3, service_name_profile_id);
        smoke_require(status == RELDEX_STATUS_OK, "get_profile after deletion is accepted");
        smoke_require(wait_and_take_workspace_reply(workspace, &reply), "the post-deletion get_profile reply arrives");
        smoke_check(!reply.found, "the deleted profile is no longer found");
        release_workspace_reply_objects(&reply);
    }

    /* ---- Teardown, mirroring the hub's own waker-then-destroy order. ---- */
    status = reldex_workspace_set_waker(workspace, NULL, NULL);
    smoke_check(status == RELDEX_STATUS_OK, "reldex_workspace_set_waker(NULL) unregisters cleanly");
    reldex_workspace_close(workspace);
    printf("reldex_workspace_close returned; process is intact\n");

    {
        double counts_started = smoke_now_seconds();
        ReldexLiveCounts final_counts = live_counts();
        while (final_counts.misc_objects != workspace_baseline.misc_objects) {
            double elapsed = smoke_now_seconds() - counts_started;
            if (elapsed > HANG_GUARD_SECONDS) {
                break;
            }
            smoke_sleep_ms(5);
            final_counts = live_counts();
        }
        printf("live at end (post-workspace): misc=%zu\n", final_counts.misc_objects);
        smoke_check(
            final_counts.misc_objects == workspace_baseline.misc_objects,
            "no workspace-related object (the handle itself, or anything it handed out) is left alive");
    }

    if (have_temp_dir) {
        remove_temp_file_and_dir(temp_dir, db_path);
    }
}

/* ---------------------------------------------------------------------
 * main(): the full sequence the M1.4 brief lists, in order.
 * --------------------------------------------------------------------- */

int main(void)
{
    printf("reldex-ffi C smoke harness starting\n");

    /* 1. ABI version check. */
    uint32_t version = reldex_abi_version();
    uint32_t major = version >> 16;
    uint32_t minor = version & 0xFFFFu;
    printf(
        "reldex-ffi ABI %u.%u (header expects major %u, minor >= %u)\n",
        major, minor, (unsigned)RELDEX_ABI_VERSION_MAJOR, (unsigned)RELDEX_ABI_VERSION_MINOR);
    smoke_require(major == (uint32_t)RELDEX_ABI_VERSION_MAJOR, "ABI major version matches the header");

    /* 1b. Live-object baseline. Everything this harness is handed must come
     * back by the end; reldex_live_counts is how that is asserted without a
     * sanitizer. */
    ReldexLiveCounts baseline;
    memset(&baseline, 0, sizeof(baseline));
    baseline.struct_size = sizeof(baseline);
    ReldexStatus counts_status = reldex_live_counts(&baseline);
    smoke_require(counts_status == RELDEX_STATUS_OK, "reldex_live_counts succeeds");
    printf(
        "live at start: hubs=%zu sessions=%zu batches=%zu errors=%zu arenas=%zu misc=%zu\n",
        baseline.hubs, baseline.sessions, baseline.batches, baseline.errors, baseline.arenas,
        baseline.misc_objects);

    /* 1c. Statement splitting (M2.11 family 3). Pure and synchronous -- no
     * hub, and `out` is the caller's own array throughout (nothing here is
     * allocated by the library, so there is nothing to release). */
    {
        static const char split_text[] =
            "select 1 from dual;\n"
            "select 2 from dual;\n";
        struct ReldexStr text;
        text.ptr = (const uint8_t *)split_text;
        text.len = sizeof(split_text) - 1; /* exclude the trailing NUL */

        size_t total = reldex_split_statements(text, NULL, 0);
        smoke_check(total == 2, "reldex_split_statements finds 2 statements in a 2-statement script");

        ReldexStatementSpan spans[4];
        for (size_t i = 0; i < sizeof(spans) / sizeof(spans[0]); i += 1) {
            memset(&spans[i], 0, sizeof(spans[i]));
            spans[i].struct_size = sizeof(spans[i]);
        }
        size_t reported = reldex_split_statements(text, spans, sizeof(spans) / sizeof(spans[0]));
        smoke_check(reported == total, "a second call with enough capacity reports the same total");
        for (size_t i = 0; i < total; i += 1) {
            smoke_check(
                spans[i].kind == RELDEX_SPLIT_KIND_PLAIN,
                "each statement in the script is plain, not a block");
            smoke_check(
                spans[i].ended_by == RELDEX_ENDED_BY_TERMINATOR,
                "each statement ends at its `;` terminator");
            smoke_check(spans[i].terminated, "each statement's terminator was found");
            smoke_check(spans[i].content_start < spans[i].content_end, "each statement's content range is non-empty");
            smoke_check(spans[i].content_end <= spans[i].full_end, "content_end never runs past full_end");
            smoke_check(spans[i].full_end <= text.len, "full_end never runs past the input");
            printf(
                "  statement %zu: [%zu,%zu) full=%zu line=%u col=%u\n",
                i, spans[i].content_start, spans[i].content_end, spans[i].full_end,
                (unsigned)spans[i].start_line, (unsigned)spans[i].start_column);
        }
        smoke_check(spans[1].start_line == 2, "the second statement starts on line 2");

        /* A capacity smaller than the total: `out` holds only what fits,
         * exactly like snprintf's contract -- the return value is still the
         * true total, not what was written. */
        ReldexStatementSpan one_span;
        memset(&one_span, 0, sizeof(one_span));
        one_span.struct_size = sizeof(one_span);
        size_t still_total = reldex_split_statements(text, &one_span, 1);
        smoke_check(still_total == 2, "an undersized capacity still reports the true total (snprintf contract)");
        smoke_check(one_span.content_start == spans[0].content_start, "the one span written matches the first statement");
    }

    /* 1d. Metadata (M2.11 family 4): prepares a request into a statement
     * plus its column contract. Pure and synchronous like splitting, but
     * DOES hand back a caller-owned ReldexMetadataQuery -- released at the
     * end of this block, or it would show up in reldex_live_counts'
     * misc_objects forever. */
    {
        ReldexMetadataRequest request;
        memset(&request, 0, sizeof(request));
        request.struct_size = sizeof(request);
        request.kind = RELDEX_METADATA_REQUEST_KIND_SCHEMAS;
        request.limit = 50;

        ReldexMetadataQuery *query = NULL;
        ReldexStatus meta_status = reldex_metadata_prepare(&request, &query);
        smoke_require(meta_status == RELDEX_STATUS_OK, "reldex_metadata_prepare accepts a SCHEMAS request");
        smoke_require(query != NULL, "reldex_metadata_prepare returns a query");
        smoke_check(live_counts().misc_objects >= 1, "the prepared query is counted as live");

        ReldexStr sql = reldex_metadata_query_sql(query);
        smoke_check(sql.ptr != NULL && sql.len > 0, "the prepared query has SQL text");
        smoke_check(sql.ptr[sql.len] == '\0', "the prepared query's SQL text is NUL-terminated");
        printf("  metadata SQL: %.*s\n", (int)sql.len, (const char *)sql.ptr);

        size_t binds = reldex_metadata_query_bind_count(query);
        smoke_check(binds >= 1, "a SCHEMAS query has at least one bind placeholder");

        size_t columns = reldex_metadata_query_column_count(query);
        smoke_require(columns > 0, "a SCHEMAS query declares at least one column");
        for (size_t col = 0; col < columns; col += 1) {
            ReldexColumnInfo info;
            memset(&info, 0, sizeof(info));
            info.struct_size = sizeof(info);
            ReldexStatus col_status = reldex_metadata_query_column(query, col, &info);
            smoke_check(col_status == RELDEX_STATUS_OK, "reldex_metadata_query_column succeeds");
            smoke_check(
                info.name.ptr != NULL && info.name.ptr[info.name.len] == '\0',
                "the declared column's name is a usable C string");
            printf("  metadata column %zu: \"%s\"\n", col, (const char *)info.name.ptr);
        }

        /* Error reclassification: a driver-shaped error goes in, a (possibly
         * recategorised) error comes back -- never invented from nothing, so
         * the native code the driver actually reported survives the trip. */
        struct ReldexStr message;
        static const char raw_message[] = "ORA-00942: table or view does not exist";
        message.ptr = (const uint8_t *)raw_message;
        message.len = sizeof(raw_message) - 1;
        ReldexError *reclassified = reldex_metadata_query_reclassify_error(
            query, RELDEX_ERROR_KIND_OTHER, 942, true, message);
        smoke_require(reclassified != NULL, "reldex_metadata_query_reclassify_error returns an error");
        ErrorSummary reclass_summary = read_and_free_error(reclassified);
        smoke_check(
            reclass_summary.has_native && reclass_summary.native_code == 942,
            "the reclassified error keeps the driver's native code");

        reldex_metadata_query_release(query);
        smoke_check(
            live_counts().misc_objects == baseline.misc_objects,
            "releasing the query returns misc_objects to its baseline");
    }

    /* 2. Hub create. */
    ReldexHub *hub = reldex_hub_create();
    smoke_require(hub != NULL, "reldex_hub_create returns a non-null hub");
    smoke_check(live_counts().hubs == baseline.hubs + 1, "the new hub is counted as live");

    /* 3. Set waker. The callback above touches nothing but its own
     * flag/counter, so it satisfies "must not call back into reldex_*". */
    ReldexStatus status = reldex_hub_set_waker(hub, smoke_waker, NULL);
    smoke_require(status == RELDEX_STATUS_OK, "reldex_hub_set_waker(smoke_wake) succeeds");

    /* 4-5-6. Session open (mock scenario), wait for wake, drain events. */
    ReldexOpenOptions open_options_a;
    memset(&open_options_a, 0, sizeof(open_options_a));
    open_options_a.struct_size = sizeof(open_options_a);
    open_options_a.driver = RELDEX_DRIVER_KIND_MOCK;
    open_options_a.mock.struct_size = sizeof(open_options_a.mock);
    open_options_a.mock.scenario = RELDEX_MOCK_SCENARIO_S14;
    open_options_a.mock.rows = 24;

    uint64_t session_a = 0;
    uint64_t open_a_request = next_request_id();
    g_wake_flag = 0;
    status = reldex_hub_open_session(hub, &open_options_a, open_a_request, &session_a);
    smoke_require(status == RELDEX_STATUS_OK, "reldex_hub_open_session(session A) is accepted");
    smoke_require(session_a != 0, "session A has a non-zero id");

    ReldexEvent event;
    bool woke = wait_for_wake();
    smoke_require(woke, "the waker fires after opening session A (wait for wake)");
    smoke_require(g_wake_count > 0, "the waker flag has actually been set at least once");
    memset(&event, 0, sizeof(event));
    event.struct_size = sizeof(event);
    bool took = reldex_hub_next_event(hub, &event) != 0;
    smoke_require(took, "an event is available immediately after the wake (drain events)");
    smoke_check(event.kind == RELDEX_EVENT_KIND_OPENED, "session A's event is OPENED");
    smoke_check(event.request == open_a_request, "session A's OPENED event carries the request id");
    smoke_check(event.session == session_a, "session A's OPENED event carries the session id");
    smoke_check(event.error == NULL, "session A opened without an error");
    if (event.error != NULL) {
        read_and_free_error(event.error);
    }
    /* Queue must be empty again before the next submission, so the next
     * wake is a genuine empty -> non-empty edge. */
    smoke_check(!reldex_hub_next_event(hub, &event), "the queue is empty after draining session A's OPENED event");

    /* 7. Execute a query. */
    uint64_t execute_a_request = next_request_id();
    g_wake_flag = 0;
    ReldexStr generated_sql = reldex_mock_statement(RELDEX_MOCK_STATEMENT_GENERATED_QUERY);
    status = reldex_session_execute(hub, session_a, execute_a_request, generated_sql, 0);
    smoke_require(status == RELDEX_STATUS_OK, "reldex_session_execute(GENERATED_QUERY) is accepted on session A");
    smoke_require(wait_and_take_event(hub, &event), "session A's EXECUTED event arrives");
    smoke_check(event.kind == RELDEX_EVENT_KIND_EXECUTED, "session A's event is EXECUTED");
    smoke_check(event.request == execute_a_request, "session A's EXECUTED event carries the request id");
    smoke_check(event.error == NULL, "the generated query executed without an error");
    smoke_require(event.has_result, "the generated query produced a result set");
    uint64_t result_a = event.result;
    size_t column_count_a = event.column_count;
    smoke_check(column_count_a == 3, "the S14 shape has 3 columns (ID, NAME, CREATED)");
    smoke_check(!reldex_hub_next_event(hub, &event), "the queue is empty after draining session A's EXECUTED event");

    /* 7b. The result's columns, described from the EXECUTED event alone --
     * no batch has been fetched yet. This is what lets a grid put its header
     * row up immediately instead of resetting the model when rows arrive. */
    smoke_check(
        reldex_session_result_column_count(hub, session_a, result_a) == column_count_a,
        "reldex_session_result_column_count matches the EXECUTED event's column_count");
    for (size_t col = 0; col < column_count_a; col += 1) {
        ReldexColumnInfo early;
        memset(&early, 0, sizeof(early));
        early.struct_size = sizeof(early);
        ReldexStatus early_status = reldex_session_result_column(hub, session_a, result_a, col, &early);
        smoke_check(early_status == RELDEX_STATUS_OK, "reldex_session_result_column succeeds before any fetch");
        smoke_check(early.name.ptr != NULL && early.name.len > 0, "the early column description has a name");
        smoke_check(early.name.ptr[early.name.len] == '\0', "the early column name is NUL-terminated");
        printf(
            "  early column %zu: name=\"%s\" kind=%d\n",
            col, (const char *)early.name.ptr, (int)early.kind);
    }
    {
        ReldexColumnInfo missing;
        memset(&missing, 0, sizeof(missing));
        missing.struct_size = sizeof(missing);
        ReldexStatus missing_status =
            reldex_session_result_column(hub, session_a, result_a, column_count_a, &missing);
        smoke_check(missing_status == RELDEX_STATUS_NOT_FOUND, "an out-of-range column is refused, not invented");
        ReldexError *stale = reldex_last_error_take();
        if (stale != NULL) {
            reldex_error_free(stale);
        }
    }

    /* 8. Fetch all batches, exercising the column-view contract on the
     * first non-trivial batch: row counts, a text column via
     * offsets/data, a NUMBER mirror, the null bitmap, column names as
     * NUL-terminated C strings, and a formatted column via a
     * ReldexTextArena. */
    bool did_full_batch_checks = false;
    uint64_t total_rows_fetched = 0;
    int fetch_iterations = 0;
    const int max_fetch_iterations = 32; /* generous safety cap, not a timing bound */

    for (;;) {
        fetch_iterations += 1;
        smoke_require(
            fetch_iterations <= max_fetch_iterations,
            "the fetch loop terminates within a sane number of iterations");

        uint64_t fetch_request = next_request_id();
        g_wake_flag = 0;
        status = reldex_session_fetch(hub, session_a, fetch_request, result_a, 10);
        smoke_require(status == RELDEX_STATUS_OK, "reldex_session_fetch is accepted on session A");
        smoke_require(wait_and_take_event(hub, &event), "session A's FETCHED event arrives");
        smoke_check(event.kind == RELDEX_EVENT_KIND_FETCHED, "session A's event is FETCHED");
        smoke_check(event.request == fetch_request, "session A's FETCHED event carries the request id");
        smoke_check(event.has_result && event.result == result_a, "session A's FETCHED event carries the result id");
        smoke_check(event.error == NULL, "the fetch did not fail");

        if (event.row_count == 0) {
            /* Exhausted. A batch may or may not be handed out here; if it
             * is, it is still ours to release. */
            release_event_batch(&event);
            smoke_check(!reldex_hub_next_event(hub, &event), "the queue is empty after draining the exhausting FETCHED event");
            break;
        }

        total_rows_fetched += event.row_count;
        smoke_require(event.batch != NULL, "a FETCHED event with row_count > 0 carries a batch");
        ReldexBatch *batch = event.batch;

        size_t row_count = reldex_batch_row_count(batch);
        size_t column_count = reldex_batch_column_count(batch);
        smoke_check(row_count == event.row_count, "reldex_batch_row_count matches the event's row_count");
        smoke_check(column_count == column_count_a, "reldex_batch_column_count matches the EXECUTED event's column_count");

        if (!did_full_batch_checks) {
            did_full_batch_checks = true;

            /* Column names as NUL-terminated C strings, and locate the
             * TEXT and NUMBER columns by kind rather than assuming an
             * exact name/order (S14 is documented as ID/NAME/CREATED,
             * but this keeps the harness honest about what the contract
             * actually promises: kind, not position). */
            long text_column = -1;
            long number_column = -1;
            for (size_t col = 0; col < column_count; col += 1) {
                ReldexColumnInfo info;
                memset(&info, 0, sizeof(info));
                info.struct_size = sizeof(info);
                ReldexStatus info_status = reldex_batch_column_info(batch, col, &info);
                smoke_check(info_status == RELDEX_STATUS_OK, "reldex_batch_column_info succeeds");
                printf(
                    "  column %zu: name=\"%s\" kind=%d nullable=%d\n",
                    col, (const char *)info.name.ptr, (int)info.kind, (int)info.nullable);
                if (info.kind == RELDEX_COLUMN_KIND_TEXT && text_column < 0) {
                    text_column = (long)col;
                }
                if (info.kind == RELDEX_COLUMN_KIND_NUMBER && number_column < 0) {
                    number_column = (long)col;
                }
            }
            smoke_require(text_column >= 0, "the S14 shape has a TEXT column (NAME)");
            smoke_require(number_column >= 0, "the S14 shape has a NUMBER column (ID)");

            /* Text column via its offsets/data view, plus the null
             * bitmap (S14's NAME column includes NULL rows). */
            ReldexColumnView text_view;
            memset(&text_view, 0, sizeof(text_view));
            text_view.struct_size = sizeof(text_view);
            ReldexStatus view_status = reldex_batch_column(batch, (size_t)text_column, &text_view);
            smoke_check(view_status == RELDEX_STATUS_OK, "reldex_batch_column succeeds for the TEXT column");
            smoke_check(text_view.offsets != NULL, "the TEXT column has an offsets array");
            smoke_check(text_view.data != NULL || text_view.data_len == 0, "the TEXT column's data pointer is consistent with data_len");
            smoke_check(
                text_view.null_word_count == 0 || text_view.null_bits != NULL,
                "the TEXT column's null bitmap pointer is consistent with null_word_count");

            size_t null_rows_seen = 0;
            for (size_t row = 0; row < text_view.row_count; row += 1) {
                size_t start = text_view.offsets[row];
                size_t end = text_view.offsets[row + 1];
                smoke_check(start <= end && end <= text_view.data_len, "row's text offsets stay within data_len");
                bool is_null = false;
                if (text_view.null_bits != NULL) {
                    uint64_t word = text_view.null_bits[row / 64];
                    is_null = ((word >> (row % 64)) & 1u) != 0;
                }
                if (is_null) {
                    null_rows_seen += 1;
                } else if (row == 0) {
                    printf(
                        "  NAME[0] = \"%.*s\"\n",
                        (int)(end - start), (const char *)text_view.data + start);
                }
            }
            printf("  NAME column: %zu of %zu rows are SQL NULL\n", null_rows_seen, text_view.row_count);

            /* The plain view of a NUMBER column costs nothing: no mirror is
             * built and `fixed` stays NULL, because the adapter reads cells
             * through the bulk formatter and never looks at it. Only the
             * stride is reported, so a caller can size its own buffer. */
            ReldexColumnView plain_number_view;
            memset(&plain_number_view, 0, sizeof(plain_number_view));
            plain_number_view.struct_size = sizeof(plain_number_view);
            view_status = reldex_batch_column(batch, (size_t)number_column, &plain_number_view);
            smoke_check(view_status == RELDEX_STATUS_OK, "reldex_batch_column succeeds for the NUMBER column");
            smoke_check(plain_number_view.fixed == NULL, "reldex_batch_column builds no NUMBER mirror");
            smoke_check(plain_number_view.fixed_len == 0, "a NULL fixed array reports a zero fixed_len");
            smoke_check(
                plain_number_view.fixed_stride == sizeof(ReldexNumber),
                "the NUMBER column's fixed_stride matches sizeof(ReldexNumber) even with no mirror");

            /* Asking for the element array explicitly is the one call that
             * allocates: 46 bytes per row, retained for the batch's life. */
            ReldexColumnView number_view;
            memset(&number_view, 0, sizeof(number_view));
            number_view.struct_size = sizeof(number_view);
            view_status = reldex_batch_column_fixed(batch, (size_t)number_column, &number_view);
            smoke_check(view_status == RELDEX_STATUS_OK, "reldex_batch_column_fixed succeeds for the NUMBER column");
            smoke_check(
                number_view.fixed_stride == sizeof(ReldexNumber),
                "the mirrored NUMBER column's fixed_stride matches sizeof(ReldexNumber)");
            smoke_check(number_view.fixed_len == number_view.row_count, "the NUMBER column's fixed_len matches row_count");
            smoke_require(number_view.fixed != NULL, "reldex_batch_column_fixed builds the element array");
            const ReldexNumber *numbers = (const ReldexNumber *)number_view.fixed;
            bool numbers_well_formed = true;
            for (size_t row = 0; row < number_view.row_count; row += 1) {
                if (numbers[row].digit_count > RELDEX_NUMBER_MAX_DIGITS) {
                    numbers_well_formed = false;
                    break;
                }
            }
            smoke_check(numbers_well_formed, "every NUMBER's digit_count is within RELDEX_NUMBER_MAX_DIGITS");

            /* A formatted column through a ReldexTextArena (the bulk
             * formatter, ADR-0003 D4) -- one call for the whole visible
             * window, not per cell. */
            ReldexTextArena *arena = reldex_text_arena_create();
            smoke_require(arena != NULL, "reldex_text_arena_create succeeds");
            ReldexStatus format_status = reldex_batch_format_column(
                batch, (size_t)number_column, 0, row_count, NULL, arena);
            smoke_check(format_status == RELDEX_STATUS_OK, "reldex_batch_format_column succeeds with default options");
            size_t formatted_count = reldex_text_arena_count(arena);
            smoke_check(formatted_count == row_count, "the arena holds one formatted string per row");
            ReldexArenaView arena_view;
            memset(&arena_view, 0, sizeof(arena_view));
            arena_view.struct_size = sizeof(arena_view);
            ReldexStatus arena_status = reldex_text_arena_view(arena, &arena_view);
            smoke_check(arena_status == RELDEX_STATUS_OK, "reldex_text_arena_view succeeds");
            smoke_check(arena_view.data != NULL, "the arena's data pointer is never null");
            if (arena_view.count > 0) {
                size_t start = arena_view.offsets[0];
                size_t end = arena_view.offsets[1];
                printf(
                    "  formatted NUMBER[0] = \"%.*s\"\n",
                    (int)(end - start), (const char *)arena_view.data + start);
            }
            reldex_text_arena_release(arena);
        }

        reldex_batch_release(batch);
        smoke_check(!reldex_hub_next_event(hub, &event), "the queue is empty after draining a FETCHED event");
    }
    smoke_check(did_full_batch_checks, "at least one non-empty batch was fetched and fully checked");
    smoke_check(total_rows_fetched == open_options_a.mock.rows, "the total fetched row count matches the configured scenario size");

    /* Close the result set before running the failing statement, so the
     * failure test below starts from a clean result-less session. */
    uint64_t close_result_a_request = next_request_id();
    g_wake_flag = 0;
    status = reldex_session_close_result(hub, session_a, close_result_a_request, result_a);
    smoke_require(status == RELDEX_STATUS_OK, "reldex_session_close_result is accepted");
    smoke_require(wait_and_take_event(hub, &event), "session A's RESULT_CLOSED event arrives");
    smoke_check(event.kind == RELDEX_EVENT_KIND_RESULT_CLOSED, "session A's event is RESULT_CLOSED");
    smoke_check(event.has_result && event.result == result_a, "RESULT_CLOSED names the result that ended");
    smoke_check(event.error == NULL, "closing the result set did not fail");
    smoke_check(
        reldex_session_result_column_count(hub, session_a, result_a) == 0,
        "a closed result has no columns to describe");
    smoke_check(!reldex_hub_next_event(hub, &event), "the queue is empty after draining RESULT_CLOSED");

    /* 8b. A result set with columns and no rows. The case a grid that builds
     * its header from the first batch gets wrong: there is never a batch with
     * a row in it, yet the result has three named columns. */
    {
        uint64_t empty_request = next_request_id();
        g_wake_flag = 0;
        ReldexStr empty_sql = reldex_mock_statement(RELDEX_MOCK_STATEMENT_EMPTY_QUERY);
        smoke_check(empty_sql.len > 0, "the EMPTY_QUERY statement text is available");
        status = reldex_session_execute(hub, session_a, empty_request, empty_sql, 0);
        smoke_require(status == RELDEX_STATUS_OK, "reldex_session_execute(EMPTY_QUERY) is accepted");
        smoke_require(wait_and_take_event(hub, &event), "the empty query's EXECUTED event arrives");
        smoke_check(event.error == NULL, "the empty query executed without an error");
        smoke_require(event.has_result, "an empty result is still a result");
        uint64_t empty_result = event.result;
        smoke_check(event.column_count == 3, "the empty result still reports 3 columns");

        ReldexColumnInfo empty_info;
        memset(&empty_info, 0, sizeof(empty_info));
        empty_info.struct_size = sizeof(empty_info);
        ReldexStatus empty_status =
            reldex_session_result_column(hub, session_a, empty_result, 1, &empty_info);
        smoke_check(empty_status == RELDEX_STATUS_OK, "an empty result's columns are still described");
        smoke_check(
            empty_info.name.ptr != NULL && empty_info.name.ptr[empty_info.name.len] == '\0',
            "the empty result's column name is a usable C string");
        printf("  empty result column 1: \"%s\"\n", (const char *)empty_info.name.ptr);

        uint64_t empty_fetch = next_request_id();
        g_wake_flag = 0;
        status = reldex_session_fetch(hub, session_a, empty_fetch, empty_result, 10);
        smoke_require(status == RELDEX_STATUS_OK, "reldex_session_fetch is accepted on the empty result");
        smoke_require(wait_and_take_event(hub, &event), "the empty result's FETCHED event arrives");
        smoke_check(event.kind == RELDEX_EVENT_KIND_FETCHED, "the empty result's event is FETCHED");
        smoke_check(event.row_count == 0, "the empty result is exhausted on its first fetch");
        smoke_check(event.has_result && event.result == empty_result, "the empty result's FETCHED event names it");
        release_event_batch(&event);

        uint64_t empty_close = next_request_id();
        g_wake_flag = 0;
        status = reldex_session_close_result(hub, session_a, empty_close, empty_result);
        smoke_require(status == RELDEX_STATUS_OK, "closing the empty result is accepted");
        smoke_require(wait_and_take_event(hub, &event), "the empty result's RESULT_CLOSED event arrives");
        smoke_check(event.error == NULL, "closing the empty result did not fail");
        smoke_check(!reldex_hub_next_event(hub, &event), "the queue is empty after the empty result is closed");
    }

    /* 9. An execute that fails: check the error view (kind, native
     * code, message, position). */
    uint64_t failing_request = next_request_id();
    g_wake_flag = 0;
    ReldexStr failing_sql = reldex_mock_statement(RELDEX_MOCK_STATEMENT_FAILING);
    status = reldex_session_execute(hub, session_a, failing_request, failing_sql, 0);
    smoke_require(status == RELDEX_STATUS_OK, "reldex_session_execute(FAILING) is accepted");
    smoke_require(wait_and_take_event(hub, &event), "the failing statement's EXECUTED event arrives");
    smoke_check(event.kind == RELDEX_EVENT_KIND_EXECUTED, "the failing statement's event is EXECUTED");
    smoke_check(event.request == failing_request, "the failing statement's event carries the request id");
    smoke_require(event.error != NULL, "the failing statement produced an error");
    {
        ErrorSummary summary = read_and_free_error(event.error);
        smoke_check(summary.kind == RELDEX_ERROR_KIND_SYNTAX, "the failing statement's error kind is SYNTAX");
        smoke_check(summary.has_native && summary.native_code == 942, "the failing statement's native code is 942 (ORA-00942)");
        smoke_check(summary.has_line_column && summary.line == 1 && summary.column == 15, "the failing statement's position is line 1, column 15");
    }
    smoke_check(!reldex_hub_next_event(hub, &event), "the queue is empty after draining the failing statement's event");

    /* 10. struct_size forward/backward compatibility. */

    /* 10a. A smaller ReldexOpenOptions (as an older header would build):
     * only struct_size and driver are within its declared size, so the
     * mock config beyond it must read as the documented defaults
     * (0 rows -> the library's own default of 1,000, scenario 0 -> S14). */
    ReldexOpenOptions small_open_options;
    memset(&small_open_options, 0, sizeof(small_open_options));
    small_open_options.struct_size = (uint32_t)(sizeof(uint32_t) + sizeof(int32_t));
    small_open_options.driver = RELDEX_DRIVER_KIND_MOCK;

    uint64_t session_b = 0;
    uint64_t open_b_request = next_request_id();
    g_wake_flag = 0;
    status = reldex_hub_open_session(hub, &small_open_options, open_b_request, &session_b);
    smoke_require(status == RELDEX_STATUS_OK, "a smaller (older-header-shaped) ReldexOpenOptions is accepted");
    smoke_require(wait_and_take_event(hub, &event), "session B's OPENED event arrives");
    smoke_check(event.kind == RELDEX_EVENT_KIND_OPENED, "session B's event is OPENED");
    smoke_check(event.session == session_b, "session B's OPENED event carries its session id");
    smoke_check(event.error == NULL, "session B opened with the documented defaults");
    smoke_check(!reldex_hub_next_event(hub, &event), "the queue is empty after draining session B's OPENED event");

    /* 10b. A ReldexOpenOptions too small even for struct_size + driver:
     * refused outright, no session, no event. */
    ReldexOpenOptions broken_open_options;
    memset(&broken_open_options, 0, sizeof(broken_open_options));
    broken_open_options.struct_size = 4; /* smaller than struct_size + driver */
    status = reldex_hub_open_session(hub, &broken_open_options, next_request_id(), NULL);
    smoke_check(status == RELDEX_STATUS_INVALID_ARGUMENT, "a too-small ReldexOpenOptions is refused with INVALID_ARGUMENT");
    ReldexError *broken_error = reldex_last_error_take();
    smoke_check(broken_error != NULL, "the refusal records a thread-local last error");
    if (broken_error != NULL) {
        reldex_error_free(broken_error);
    }

    /* 10c. A larger, zero-padded ReldexEvent (as a newer header might
     * build): the library must still fill it correctly and report back
     * how much of it is actually valid via struct_size. */
    uint64_t execute_b_request = next_request_id();
    g_wake_flag = 0;
    status = reldex_session_execute(hub, session_b, execute_b_request, generated_sql, 0);
    smoke_require(status == RELDEX_STATUS_OK, "reldex_session_execute(GENERATED_QUERY) is accepted on session B");
    smoke_require(wait_for_wake(), "session B's EXECUTED wake arrives");
    ReldexEvent large_event;
    memset(&large_event, 0, sizeof(large_event));
    large_event.struct_size = (uint32_t)(sizeof(large_event) + 64); /* claims to be from a newer, larger header */
    took = reldex_hub_next_event(hub, &large_event) != 0;
    smoke_require(took, "an oversized (zero-padded) ReldexEvent is still accepted");
    smoke_check(
        large_event.struct_size == (uint32_t)sizeof(ReldexEvent),
        "the library reports back how much of an oversized struct is valid");
    smoke_check(large_event.request == execute_b_request, "the oversized struct's request id is correct");
    smoke_check(large_event.kind == RELDEX_EVENT_KIND_EXECUTED, "the oversized struct's kind is correct");
    smoke_check(large_event.error == NULL, "session B's query executed without error");
    smoke_require(large_event.has_result, "session B's query produced a result set");
    uint64_t result_b = large_event.result;
    smoke_check(!reldex_hub_next_event(hub, &event), "the queue is empty after draining session B's EXECUTED event");

    /* 10d. An undersized ReldexEvent must be refused WITHOUT consuming
     * the event (so a mis-sized struct can never silently drop a
     * batch), and *out* must be left completely untouched. Then the
     * same still-queued event must be retrievable with a correctly
     * sized struct. */
    uint64_t fetch_b_request = next_request_id();
    g_wake_flag = 0;
    status = reldex_session_fetch(hub, session_b, fetch_b_request, result_b, 10);
    smoke_require(status == RELDEX_STATUS_OK, "reldex_session_fetch is accepted on session B");
    smoke_require(wait_for_wake(), "session B's FETCHED wake arrives");

    ReldexEvent small_event;
    memset(&small_event, 0, sizeof(small_event));
    small_event.struct_size = 8;
    small_event.request = 0xDEADBEEFu;
    small_event.session = 0xFEEDFACEu;
    took = reldex_hub_next_event(hub, &small_event) != 0;
    smoke_check(!took, "an undersized ReldexEvent is refused (returns false)");
    smoke_check(
        small_event.request == 0xDEADBEEFu && small_event.session == 0xFEEDFACEu,
        "a refused (undersized) call leaves *out* completely untouched");
    ReldexError *small_error = reldex_last_error_take();
    smoke_check(small_error != NULL, "the undersized-struct refusal records why");
    if (small_error != NULL) {
        reldex_error_free(small_error);
    }

    memset(&event, 0, sizeof(event));
    event.struct_size = sizeof(event);
    took = reldex_hub_next_event(hub, &event) != 0;
    smoke_require(took, "the same event is retrievable with a correctly sized struct");
    smoke_check(event.request == fetch_b_request, "the recovered event carries the original request id");
    smoke_check(event.kind == RELDEX_EVENT_KIND_FETCHED, "the recovered event is FETCHED");
    release_event_batch(&event);
    smoke_check(!reldex_hub_next_event(hub, &event), "the queue is empty after draining session B's FETCHED event");

    /* Tidy up session B: close its result and the session itself. */
    uint64_t close_result_b_request = next_request_id();
    g_wake_flag = 0;
    status = reldex_session_close_result(hub, session_b, close_result_b_request, result_b);
    smoke_require(status == RELDEX_STATUS_OK, "reldex_session_close_result is accepted on session B");
    smoke_require(wait_and_take_event(hub, &event), "session B's RESULT_CLOSED event arrives");
    smoke_check(event.error == NULL, "closing session B's result set did not fail");
    smoke_check(!reldex_hub_next_event(hub, &event), "the queue is empty after draining session B's RESULT_CLOSED event");

    uint64_t close_b_request = next_request_id();
    g_wake_flag = 0;
    status = reldex_session_close(hub, session_b, close_b_request, RELDEX_CLOSE_DISPOSITION_ROLLBACK);
    smoke_require(status == RELDEX_STATUS_OK, "reldex_session_close is accepted on session B");
    smoke_require(wait_and_take_event(hub, &event), "session B's SESSION_CLOSED event arrives");
    smoke_check(event.kind == RELDEX_EVENT_KIND_SESSION_CLOSED, "session B's event is SESSION_CLOSED");
    smoke_check(event.close_outcome == RELDEX_CLOSE_OUTCOME_CLOSED, "session B closed cleanly (no open transaction)");
    smoke_check(!event.session_still_open, "session B is reported as no longer open");
    /* M2.11 family 1: a session that actually closes pushes exactly one
     * TERMINAL event right after SESSION_CLOSED, in the same batch (before
     * any wake), so draining "the" close reply now means draining both --
     * see reldex_session_close's SPEC.md §10 contract in the header. */
    memset(&event, 0, sizeof(event));
    event.struct_size = sizeof(event);
    took = reldex_hub_next_event(hub, &event) != 0;
    smoke_require(took, "session B's TERMINAL event follows SESSION_CLOSED in the same batch");
    smoke_check(event.kind == RELDEX_EVENT_KIND_TERMINAL, "session B's second queued event is TERMINAL");
    smoke_check(event.session == session_b, "session B's TERMINAL event carries its session id");
    smoke_check(!event.transaction_possibly_lost, "session B's clean close does not report a possibly-lost transaction");
    smoke_check(!reldex_hub_next_event(hub, &event), "the queue is empty after draining session B's SESSION_CLOSED+TERMINAL events");

    /* 11. Close session A. */
    uint64_t close_a_request = next_request_id();
    g_wake_flag = 0;
    status = reldex_session_close(hub, session_a, close_a_request, RELDEX_CLOSE_DISPOSITION_ROLLBACK);
    smoke_require(status == RELDEX_STATUS_OK, "reldex_session_close is accepted on session A");
    smoke_require(wait_and_take_event(hub, &event), "session A's SESSION_CLOSED event arrives");
    smoke_check(event.kind == RELDEX_EVENT_KIND_SESSION_CLOSED, "session A's event is SESSION_CLOSED");
    smoke_check(event.close_outcome == RELDEX_CLOSE_OUTCOME_CLOSED, "session A closed cleanly (no open transaction)");
    smoke_check(!event.session_still_open, "session A is reported as no longer open");
    memset(&event, 0, sizeof(event));
    event.struct_size = sizeof(event);
    took = reldex_hub_next_event(hub, &event) != 0;
    smoke_require(took, "session A's TERMINAL event follows SESSION_CLOSED in the same batch");
    smoke_check(event.kind == RELDEX_EVENT_KIND_TERMINAL, "session A's second queued event is TERMINAL");
    smoke_check(event.session == session_a, "session A's TERMINAL event carries its session id");
    smoke_check(!event.transaction_possibly_lost, "session A's clean close does not report a possibly-lost transaction");

    /* 11b. Server output control (M2.11 family 2), on a dedicated session C
     * so the carefully sequenced session A/B flow above stays untouched.
     *
     * The mock driver's only FFI-reachable scenario (S14) does not
     * advertise the server_output capability (`Scenario::default()` in
     * crates/drivers/mock/src/scenario.rs turns on savepoints, exact
     * transaction state, LOB streaming and error positions, but not this
     * one), and `Scenario::set_capabilities` -- the only way to turn it on
     * -- is a Rust-only test API, not part of this ABI. So the reachable,
     * correct behaviour to exercise here is the REFUSAL path: every mode
     * comes back as RELDEX_EVENT_KIND_SERVER_OUTPUT_CONFIGURED carrying an
     * UNSUPPORTED error, never silently ignored. Exercising the success
     * path (an in-force mode/buffer size actually read back) needs either a
     * capability knob added to the mock driver's FFI surface or a real
     * Oracle session -- noted as a smoke-harness gap in the PR description,
     * not fixed here. */
    {
        ReldexOpenOptions open_options_c;
        memset(&open_options_c, 0, sizeof(open_options_c));
        open_options_c.struct_size = sizeof(open_options_c);
        open_options_c.driver = RELDEX_DRIVER_KIND_MOCK;
        open_options_c.mock.struct_size = sizeof(open_options_c.mock);
        open_options_c.mock.scenario = RELDEX_MOCK_SCENARIO_S14;

        uint64_t session_c = 0;
        uint64_t open_c_request = next_request_id();
        g_wake_flag = 0;
        status = reldex_hub_open_session(hub, &open_options_c, open_c_request, &session_c);
        smoke_require(status == RELDEX_STATUS_OK, "reldex_hub_open_session(session C) is accepted");
        smoke_require(wait_and_take_event(hub, &event), "session C's OPENED event arrives");
        smoke_check(event.error == NULL, "session C opened without an error");

        struct {
            int32_t mode;
            uint64_t buffer_bytes;
            const char *what;
        } modes[] = {
            {RELDEX_SERVER_OUTPUT_MODE_ENABLED_UNLIMITED, 0, "ENABLED_UNLIMITED"},
            {RELDEX_SERVER_OUTPUT_MODE_ENABLED_BYTES, 20000, "ENABLED_BYTES(20000)"},
            {RELDEX_SERVER_OUTPUT_MODE_DISABLED, 0, "DISABLED"},
        };
        for (size_t i = 0; i < sizeof(modes) / sizeof(modes[0]); i += 1) {
            uint64_t set_request = next_request_id();
            g_wake_flag = 0;
            status = reldex_session_set_server_output(
                hub, session_c, set_request, modes[i].mode, modes[i].buffer_bytes);
            smoke_require(status == RELDEX_STATUS_OK, "reldex_session_set_server_output is accepted");
            smoke_require(wait_and_take_event(hub, &event), "session C's SERVER_OUTPUT_CONFIGURED event arrives");
            smoke_check(
                event.kind == RELDEX_EVENT_KIND_SERVER_OUTPUT_CONFIGURED,
                "session C's event is SERVER_OUTPUT_CONFIGURED");
            smoke_check(event.request == set_request, "session C's SERVER_OUTPUT_CONFIGURED event carries the request id");
            smoke_check(
                event.error != NULL,
                "the mock driver's only FFI-reachable scenario does not advertise server_output, "
                "so setting it is refused, not silently accepted");
            printf("  server output %s -> refused (capability not advertised): ", modes[i].what);
            if (event.error != NULL) {
                ErrorSummary summary = read_and_free_error(event.error);
                smoke_check(
                    summary.kind == RELDEX_ERROR_KIND_UNSUPPORTED,
                    "the refusal's error kind is UNSUPPORTED");
            } else {
                printf("\n");
            }
            smoke_check(!reldex_hub_next_event(hub, &event), "the queue is empty after draining SERVER_OUTPUT_CONFIGURED");
        }

        uint64_t close_c_request = next_request_id();
        g_wake_flag = 0;
        status = reldex_session_close(hub, session_c, close_c_request, RELDEX_CLOSE_DISPOSITION_ROLLBACK);
        smoke_require(status == RELDEX_STATUS_OK, "reldex_session_close is accepted on session C");
        smoke_require(wait_and_take_event(hub, &event), "session C's SESSION_CLOSED event arrives");
        smoke_check(event.kind == RELDEX_EVENT_KIND_SESSION_CLOSED, "session C's event is SESSION_CLOSED");
        memset(&event, 0, sizeof(event));
        event.struct_size = sizeof(event);
        took = reldex_hub_next_event(hub, &event) != 0;
        smoke_require(took, "session C's TERMINAL event follows SESSION_CLOSED in the same batch");
        smoke_check(event.kind == RELDEX_EVENT_KIND_TERMINAL, "session C's second queued event is TERMINAL");
        smoke_check(!reldex_hub_next_event(hub, &event), "the queue is empty after draining session C's close");
    }

    /* 12. Final drain: nothing more should be queued. */
    memset(&event, 0, sizeof(event));
    event.struct_size = sizeof(event);
    smoke_check(!reldex_hub_next_event(hub, &event), "the queue is empty after both sessions are closed");
    smoke_check(reldex_hub_pending_events(hub) == 0, "reldex_hub_pending_events reports zero backlog at teardown");

    /* 13. Unregister the waker, then destroy the hub -- in that order,
     * per the header's contract. */
    status = reldex_hub_set_waker(hub, NULL, NULL);
    smoke_check(status == RELDEX_STATUS_OK, "reldex_hub_set_waker(NULL) unregisters cleanly");

    reldex_hub_destroy(hub);
    printf("reldex_hub_destroy returned; process is intact\n");

    /* 14. Nothing this harness was handed is still alive. A hub's teardown
     * finishes on its session pump threads, so the counts fall shortly after
     * the call returns -- waited for under the same hang guard as everything
     * else, which asserts no upper bound on how fast it happens. */
    {
        double counts_started = smoke_now_seconds();
        ReldexLiveCounts final_counts = live_counts();
        while (final_counts.hubs != baseline.hubs
               || final_counts.sessions != baseline.sessions
               || final_counts.batches != baseline.batches
               || final_counts.errors != baseline.errors
               || final_counts.arenas != baseline.arenas
               || final_counts.misc_objects != baseline.misc_objects) {
            double elapsed = smoke_now_seconds() - counts_started;
            if (elapsed > HANG_GUARD_SECONDS) {
                break;
            }
            smoke_sleep_ms(5);
            final_counts = live_counts();
        }
        printf(
            "live at end (pre-workspace): hubs=%zu sessions=%zu batches=%zu errors=%zu arenas=%zu misc=%zu\n",
            final_counts.hubs, final_counts.sessions, final_counts.batches,
            final_counts.errors, final_counts.arenas, final_counts.misc_objects);
        smoke_check(final_counts.hubs == baseline.hubs, "no hub is left alive");
        smoke_check(final_counts.sessions == baseline.sessions, "no session is left alive");
        smoke_check(final_counts.batches == baseline.batches, "no batch is left alive");
        smoke_check(final_counts.errors == baseline.errors, "no error object is left alive");
        smoke_check(final_counts.arenas == baseline.arenas, "no text arena is left alive");
        smoke_check(
            final_counts.misc_objects == baseline.misc_objects,
            "no M2.11 object (metadata query, profile/history/worksheet list, secret, "
            "connect summary or workspace handle) is left alive before the workspace section");
    }

    /* 15. Settings, profiles, credentials, history, worksheets and layout
     * (M2.11 families 5-7), all through one ReldexWorkspace on its own
     * service thread -- deliberately exercised only after the hub above is
     * fully torn down, so this section's own live-object bookkeeping starts
     * from a clean slate. */
    smoke_run_workspace_checks();

    printf(
        "\nreldex-ffi C smoke harness: %ld/%ld checks passed (%ld waker call(s) observed, "
        "%ld workspace waker call(s) observed)\n",
        g_checks_run - g_checks_failed, g_checks_run, g_wake_count, g_workspace_wake_count);

    return g_checks_failed == 0 ? 0 : 1;
}

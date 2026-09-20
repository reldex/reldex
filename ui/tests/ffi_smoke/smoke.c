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

#ifdef __cplusplus
extern "C"
#endif
void
smoke_wake(void *user_data)
#if defined(__cplusplus) && __cplusplus >= 201703L
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

#if defined(__cplusplus) && __cplusplus >= 201703L
static ReldexWakeFnNoexcept smoke_waker = smoke_wake;
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
    clock_t start = clock();
    while (!g_wake_flag) {
        double elapsed = (double)(clock() - start) / (double)CLOCKS_PER_SEC;
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
        "live at start: hubs=%zu sessions=%zu batches=%zu errors=%zu arenas=%zu\n",
        baseline.hubs, baseline.sessions, baseline.batches, baseline.errors, baseline.arenas);

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
    smoke_check(!reldex_hub_next_event(hub, &event), "the queue is empty after draining session B's SESSION_CLOSED event");

    /* 11. Close session A. */
    uint64_t close_a_request = next_request_id();
    g_wake_flag = 0;
    status = reldex_session_close(hub, session_a, close_a_request, RELDEX_CLOSE_DISPOSITION_ROLLBACK);
    smoke_require(status == RELDEX_STATUS_OK, "reldex_session_close is accepted on session A");
    smoke_require(wait_and_take_event(hub, &event), "session A's SESSION_CLOSED event arrives");
    smoke_check(event.kind == RELDEX_EVENT_KIND_SESSION_CLOSED, "session A's event is SESSION_CLOSED");
    smoke_check(event.close_outcome == RELDEX_CLOSE_OUTCOME_CLOSED, "session A closed cleanly (no open transaction)");
    smoke_check(!event.session_still_open, "session A is reported as no longer open");

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
        clock_t counts_started = clock();
        ReldexLiveCounts final_counts = live_counts();
        while (final_counts.hubs != baseline.hubs
               || final_counts.sessions != baseline.sessions
               || final_counts.batches != baseline.batches
               || final_counts.errors != baseline.errors
               || final_counts.arenas != baseline.arenas) {
            double elapsed = (double)(clock() - counts_started) / (double)CLOCKS_PER_SEC;
            if (elapsed > HANG_GUARD_SECONDS) {
                break;
            }
            smoke_sleep_ms(5);
            final_counts = live_counts();
        }
        printf(
            "live at end: hubs=%zu sessions=%zu batches=%zu errors=%zu arenas=%zu\n",
            final_counts.hubs, final_counts.sessions, final_counts.batches,
            final_counts.errors, final_counts.arenas);
        smoke_check(final_counts.hubs == baseline.hubs, "no hub is left alive");
        smoke_check(final_counts.sessions == baseline.sessions, "no session is left alive");
        smoke_check(final_counts.batches == baseline.batches, "no batch is left alive");
        smoke_check(final_counts.errors == baseline.errors, "no error object is left alive");
        smoke_check(final_counts.arenas == baseline.arenas, "no text arena is left alive");
    }

    printf(
        "\nreldex-ffi C smoke harness: %ld/%ld checks passed (%ld waker call(s) observed)\n",
        g_checks_run - g_checks_failed, g_checks_run, g_wake_count);

    return g_checks_failed == 0 ? 0 : 1;
}

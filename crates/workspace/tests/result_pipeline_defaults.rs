//! The result settings' built-in defaults and the values `reldex-db-core`
//! falls back on when nothing is resolved are one number each, kept in two
//! crates that may not depend on each other (ADR-0006 "Crate boundary").
//! This pins them together, so changing one without the other fails here
//! (M5.2 review).

use reldex_db_core::{DEFAULT_FETCHES_IN_FLIGHT, DEFAULT_ROUND_TRIP_BYTES, ResultPolicy};
use reldex_db_driver_api::DEFAULT_FETCH_ROWS;
use reldex_workspace::settings::{
    ByteLimit, FETCH_ROWS, FETCHES_IN_FLIGHT, RESULTS_ROUND_TRIP_BYTES,
};

#[test]
fn the_result_pipeline_defaults_are_the_settings_built_in_defaults() {
    let fetch_rows = usize::try_from(FETCH_ROWS.default_value()).expect("fits");
    let in_flight = usize::try_from(FETCHES_IN_FLIGHT.default_value()).expect("fits");
    let ByteLimit::Bytes(budget) = RESULTS_ROUND_TRIP_BYTES.default_value() else {
        panic!("results.round_trip_bytes refuses \"no limit\", so its default is a number");
    };
    let budget = usize::try_from(budget.get()).expect("fits");
    assert_eq!(fetch_rows, DEFAULT_FETCH_ROWS.get());
    assert_eq!(in_flight, DEFAULT_FETCHES_IN_FLIGHT.get());
    assert_eq!(budget, DEFAULT_ROUND_TRIP_BYTES.get());

    // And they are what a policy built from nothing else uses.
    let policy = ResultPolicy::new(reldex_db_core::ResultCaps::new(
        reldex_db_core::Sourced::new(
            reldex_db_core::Cap::Unlimited,
            reldex_db_core::CapSource::BuiltIn,
        ),
        reldex_db_core::Sourced::new(
            reldex_db_core::Cap::Unlimited,
            reldex_db_core::CapSource::BuiltIn,
        ),
    ));
    assert_eq!(policy.fetch_rows().get(), fetch_rows);
    assert_eq!(policy.fetches_in_flight().get(), in_flight);
    assert_eq!(policy.round_trip_bytes().get(), budget);
}

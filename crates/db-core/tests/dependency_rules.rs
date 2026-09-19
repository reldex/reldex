//! Architecture dependency-direction test (Phase 0 Workstream A).
//!
//! `docs/architecture/ARCHITECTURE.md` §2 requires that `reldex-db-core`
//! depend only on `reldex-db-driver-api` *as a build dependency*, never on a
//! concrete `reldex-driver-*` crate, and that no `reldex-driver-*` crate
//! depend back on `reldex-db-core`. This test reads the workspace dependency
//! graph from `cargo metadata` and asserts both directions of that rule so a
//! violation fails CI instead of silently landing.
//!
//! One deliberate exception: `reldex-db-core`'s own `[dev-dependencies]` on
//! `reldex-driver-mock` is allowed. `docs/architecture/ARCHITECTURE.md` §4
//! is explicit that the mock driver exists so core logic is testable without
//! a database, and `crates/db-core/tests/` is exactly where that happens; a
//! `dev-dependency` never reaches the compiled product, so it does not
//! violate the layering rule this test enforces. Only a normal dependency on
//! a driver crate would.

use std::process::Command;

#[test]
fn db_core_and_driver_crates_do_not_cross_depend() {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());

    let output = Command::new(cargo)
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .output()
        .expect("failed to spawn `cargo metadata`");

    assert!(
        output.status.success(),
        "`cargo metadata` failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let metadata: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("`cargo metadata` produced invalid JSON");

    let packages = metadata["packages"]
        .as_array()
        .expect("`cargo metadata` output has no `packages` array");

    assert!(
        !packages.is_empty(),
        "`cargo metadata` reported no workspace packages"
    );

    let mut checked_db_core = false;
    let mut checked_a_driver = false;

    for package in packages {
        let name = package["name"]
            .as_str()
            .expect("package entry has no `name`");
        let deps: Vec<(&str, Option<&str>)> = package["dependencies"]
            .as_array()
            .expect("package entry has no `dependencies` array")
            .iter()
            .filter_map(|dep| Some((dep["name"].as_str()?, dep["kind"].as_str())))
            .collect();

        if name == "reldex-db-core" {
            checked_db_core = true;
            for (dep, kind) in &deps {
                // See the module documentation: a `dev` dependency on the
                // mock driver is the intended way to test this crate.
                if *kind == Some("dev") {
                    continue;
                }
                assert!(
                    !dep.starts_with("reldex-driver-"),
                    "reldex-db-core must not have a non-dev dependency on driver crate `{dep}` \
                     (docs/architecture/ARCHITECTURE.md §2)"
                );
            }
        }

        if name.starts_with("reldex-driver-") {
            checked_a_driver = true;
            assert!(
                !deps.iter().any(|(dep, _)| *dep == "reldex-db-core"),
                "driver crate `{name}` must not depend on reldex-db-core \
                 (docs/architecture/ARCHITECTURE.md §2)"
            );
        }
    }

    assert!(checked_db_core, "expected to find package `reldex-db-core`");
    assert!(
        checked_a_driver,
        "expected to find at least one `reldex-driver-*` package"
    );
}

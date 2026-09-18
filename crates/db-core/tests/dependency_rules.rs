//! Architecture dependency-direction test (Phase 0 Workstream A).
//!
//! `docs/architecture/ARCHITECTURE.md` §2 requires that `reldex-db-core`
//! depend only on `reldex-db-driver-api`, never on a concrete
//! `reldex-driver-*` crate, and that no `reldex-driver-*` crate depend back
//! on `reldex-db-core`. This test reads the workspace dependency graph from
//! `cargo metadata` and asserts both directions of that rule so a violation
//! fails CI instead of silently landing.

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
        let deps: Vec<&str> = package["dependencies"]
            .as_array()
            .expect("package entry has no `dependencies` array")
            .iter()
            .filter_map(|dep| dep["name"].as_str())
            .collect();

        if name == "reldex-db-core" {
            checked_db_core = true;
            for dep in &deps {
                assert!(
                    !dep.starts_with("reldex-driver-"),
                    "reldex-db-core must not depend on driver crate `{dep}` \
                     (docs/architecture/ARCHITECTURE.md §2)"
                );
            }
        }

        if name.starts_with("reldex-driver-") {
            checked_a_driver = true;
            assert!(
                !deps.contains(&"reldex-db-core"),
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

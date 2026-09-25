//! The crate's dependency rule (ADR-0006 "Crate boundary"), checked on the
//! workspace's own dependency graph from `cargo metadata`, so a violation
//! fails CI instead of silently landing:
//!
//! - `reldex-workspace`'s normal dependencies are exactly the vendor-neutral
//!   driver contract, SQLite and UUIDs (plus `std`, which `cargo metadata`
//!   does not list). Not `reldex-db-core` — nothing here needs a session or a
//!   thread — and no concrete driver: the binding that names one belongs to
//!   the composition root (M2.11). A dev-dependency never reaches the
//!   product and is not checked.
//! - Nothing below it depends back on it: `reldex-db-core`, the driver
//!   contract and every driver stay free of SQLite and of this crate.

use std::process::Command;

const ALLOWED: [&str; 3] = ["reldex-db-driver-api", "rusqlite", "uuid"];

#[test]
fn the_workspace_crate_depends_only_on_the_contract_sqlite_and_uuid() {
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

    let mut checked_self = false;
    let mut checked_below = 0;
    for package in packages {
        let name = package["name"]
            .as_str()
            .expect("package entry has no `name`");
        let normal: Vec<&str> = package["dependencies"]
            .as_array()
            .expect("package entry has no `dependencies` array")
            .iter()
            .filter(|dep| dep["kind"].is_null())
            .filter_map(|dep| dep["name"].as_str())
            .collect();
        if name == "reldex-workspace" {
            checked_self = true;
            let mut sorted = normal.clone();
            sorted.sort_unstable();
            assert_eq!(
                sorted, ALLOWED,
                "reldex-workspace's normal dependencies changed; see ADR-0006 \
                 \"Crate boundary\" before adding one"
            );
        }
        if name == "reldex-db-core"
            || name == "reldex-db-driver-api"
            || name.starts_with("reldex-driver-")
        {
            checked_below += 1;
            for forbidden in ["reldex-workspace", "rusqlite"] {
                assert!(
                    !package["dependencies"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(|dep| dep["name"].as_str() == Some(forbidden)),
                    "`{name}` must not depend on `{forbidden}` (ADR-0006 \"Crate boundary\")"
                );
            }
        }
    }
    assert!(checked_self, "expected to find package `reldex-workspace`");
    assert!(
        checked_below >= 3,
        "expected db-core, the driver contract and at least one driver"
    );
}

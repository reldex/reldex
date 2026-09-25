//! The crate's dependency rule (ADR-0007 S1, S6), checked on the workspace's
//! own dependency graph from `cargo metadata`, so a violation fails CI
//! instead of silently landing:
//!
//! - `reldex-secrets`' normal dependencies are exactly the profile model
//!   (`reldex-workspace`, for `CredentialKey` and `Profile`), the driver
//!   contract (`reldex-db-driver-api`, for `Secret`), `zeroize`, and — on
//!   Windows only — `windows-sys`. No `reldex-db-core`, no driver, no UI.
//! - Nothing below it depends back on it: the profile store that must never
//!   hold a secret (`reldex-workspace`), `reldex-db-core`, the driver
//!   contract, the SQL text crate and every driver stay free of this crate
//!   and of the platform API.
//! - The driver contract's only production dependency is `zeroize`
//!   (ADR-0007 S6, revisiting ADR-0002's "no `zeroize` for now").
//!
//! A dev-dependency never reaches the product and is not checked.

use std::process::Command;

fn packages() -> Vec<serde_json::Value> {
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
    metadata["packages"]
        .as_array()
        .expect("`cargo metadata` output has no `packages` array")
        .clone()
}

/// A package's normal (non-dev, non-build) dependencies, as
/// `(name, target)` where `target` is the `cfg(...)` it is limited to.
fn normal_dependencies(package: &serde_json::Value) -> Vec<(String, Option<String>)> {
    let mut deps: Vec<(String, Option<String>)> = package["dependencies"]
        .as_array()
        .expect("package entry has no `dependencies` array")
        .iter()
        .filter(|dep| dep["kind"].is_null())
        .map(|dep| {
            (
                dep["name"].as_str().expect("dependency name").to_owned(),
                dep["target"].as_str().map(str::to_owned),
            )
        })
        .collect();
    deps.sort();
    deps
}

#[test]
fn the_secrets_crate_depends_only_on_what_adr_0007_lists() {
    let packages = packages();
    let secrets = packages
        .iter()
        .find(|package| package["name"] == "reldex-secrets")
        .expect("expected to find package `reldex-secrets`");
    assert_eq!(
        normal_dependencies(secrets),
        [
            ("reldex-db-driver-api".to_owned(), None),
            ("reldex-workspace".to_owned(), None),
            ("windows-sys".to_owned(), Some("cfg(windows)".to_owned())),
            ("zeroize".to_owned(), None),
        ],
        "reldex-secrets' normal dependencies changed; see ADR-0007 before adding one"
    );
}

#[test]
fn nothing_below_the_secrets_crate_depends_on_it() {
    let mut checked = 0;
    for package in packages() {
        let name = package["name"]
            .as_str()
            .expect("package entry has no `name`");
        let below = matches!(
            name,
            "reldex-workspace" | "reldex-db-core" | "reldex-db-driver-api" | "reldex-sql-text"
        ) || name.starts_with("reldex-driver-");
        if !below {
            continue;
        }
        checked += 1;
        for forbidden in ["reldex-secrets", "windows-sys"] {
            assert!(
                !package["dependencies"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|dep| dep["name"].as_str() == Some(forbidden)),
                "`{name}` must not depend on `{forbidden}` (ADR-0007 S1)"
            );
        }
    }
    assert!(
        checked >= 5,
        "expected workspace, db-core, the contract, sql-text and a driver"
    );
}

#[test]
fn the_driver_contracts_only_production_dependency_is_zeroize() {
    let packages = packages();
    let contract = packages
        .iter()
        .find(|package| package["name"] == "reldex-db-driver-api")
        .expect("expected to find package `reldex-db-driver-api`");
    assert_eq!(
        normal_dependencies(contract),
        [("zeroize".to_owned(), None)],
        "the driver contract gained a production dependency; see ADR-0002 D1 and ADR-0007 S6"
    );
}

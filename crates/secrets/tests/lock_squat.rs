//! The store's lock squatted by another program (ADR-0007 S4).
//!
//! Every Credential Manager call takes a per-user named mutex,
//! `Local\Reldex.CredentialStore.<user SID>`. Any process of the same user
//! can create an object of that name first, with a security descriptor that
//! lets nobody open it. That must not look like "the credential store
//! refused access" (`Denied`), and must not hang: every call fails at once
//! with `Locked { code: 5 }`, and the connect flow prompts.
//!
//! The squatter is a PowerShell process (in every Windows install and on the
//! CI runner), so this test needs no `unsafe` of its own. It computes the
//! lock's name independently from the documented scheme, which also pins
//! that scheme. A test binary of its own, because while the squatter holds
//! the real lock every other Credential Manager test would fail; cargo runs
//! test binaries one at a time.

#![cfg(windows)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use reldex_secrets::{
    CredentialError, CredentialStore, PasswordSource, PromptReason, Secret,
    WindowsCredentialManager, resolve_password,
};
use reldex_workspace::{
    Authentication, DatabaseType, Environment, PasswordStorage, Profile, ProfileDetails,
    ProfileEndpoint, ServiceTarget,
};

const STORE: WindowsCredentialManager = WindowsCredentialManager::new();

/// Creates the user's lock with a protected, empty DACL (`D:P`) — nobody may
/// open it — owns it, says `HELD`, and waits for a line on stdin.
const SQUATTER: &str = r#"
$sid = [System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value
$security = New-Object System.Security.AccessControl.MutexSecurity
$security.SetSecurityDescriptorSddlForm('D:P')
$created = $false
$mutex = New-Object System.Threading.Mutex($true, ('Local\Reldex.CredentialStore.' + $sid), [ref]$created, $security)
if (-not $created) { [Console]::Out.WriteLine('EXISTS'); exit 2 }
[Console]::Out.WriteLine('HELD')
[void][Console]::In.ReadLine()
$mutex.ReleaseMutex()
"#;

/// Deletes the profile's entry however the test ends.
struct Cleanup(reldex_secrets::CredentialKey);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = STORE.delete(&self.0);
    }
}

/// Kills the squatter however the test ends.
struct Squatter(Child);

impl Drop for Squatter {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn saved_password_profile() -> Profile {
    Profile::create(ProfileDetails::new(
        "Squat check",
        DatabaseType::Oracle,
        Environment::Development,
        ProfileEndpoint::HostPort {
            host: "db.example.internal".to_owned(),
            port: 1521,
            target: ServiceTarget::ServiceName("ORDERS".to_owned()),
        },
        Authentication::Password {
            username: "app_owner".to_owned(),
            storage: PasswordStorage::CredentialStore,
        },
    ))
    .expect("valid profile")
}

#[test]
fn a_squatted_lock_fails_every_call_as_locked_and_the_connect_flow_prompts() {
    let profile = saved_password_profile();
    let key = profile.credential_key();
    let _cleanup = Cleanup(key);

    let mut squatter = Squatter(
        Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                SQUATTER,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("powershell.exe starts"),
    );
    let mut first_line = String::new();
    BufReader::new(squatter.0.stdout.as_mut().expect("piped stdout"))
        .read_line(&mut first_line)
        .expect("the squatter reports");
    assert_eq!(
        first_line.trim(),
        "HELD",
        "the squatter could not create the lock (is a Reldex process using it?)"
    );

    let started = Instant::now();
    let locked = Err(CredentialError::Locked { code: 5 });
    assert_eq!(STORE.get(&key).map(|found| found.is_some()), locked);
    assert_eq!(
        STORE.put(&key, &Secret::new("never written")),
        Err(CredentialError::Locked { code: 5 })
    );
    assert_eq!(STORE.delete(&key), Err(CredentialError::Locked { code: 5 }));
    assert!(
        matches!(
            resolve_password(&profile, &STORE),
            PasswordSource::PromptRequired(PromptReason::StoreFailed(CredentialError::Locked {
                code: 5
            }))
        ),
        "a squatted lock means a prompt"
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "refused at once, not after the lock's wait"
    );
    assert_eq!(
        CredentialError::Locked { code: 5 }.to_string(),
        "the credential store is locked by another process (platform error 5)"
    );

    // The squatter lets go and exits: the store works again.
    squatter
        .0
        .stdin
        .as_mut()
        .expect("piped stdin")
        .write_all(b"\n")
        .expect("release the squatter");
    let status = squatter.0.wait().expect("squatter exits");
    assert!(status.success(), "squatter exit: {status:?}");
    STORE
        .put(&key, &Secret::new("after the squat"))
        .expect("put");
    assert_eq!(
        STORE.get(&key).expect("get").expect("present").expose(),
        "after the squat"
    );
    STORE.delete(&key).expect("delete");
}

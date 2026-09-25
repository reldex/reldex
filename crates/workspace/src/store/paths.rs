//! Where the store lives by default.
//!
//! One file, `reldex.sqlite3`, in a directory named after the application id
//! `com.reldex.reldex` (owner decision 2026-09-20, `phase-1.md` §C.3 item 5)
//! under the platform's per-user data directory:
//!
//! | Platform | Directory |
//! | --- | --- |
//! | Windows | `%LOCALAPPDATA%\com.reldex.reldex` |
//! | macOS | `$HOME/Library/Application Support/com.reldex.reldex` |
//! | Linux and other Unix | `$XDG_DATA_HOME/com.reldex.reldex`, else `$HOME/.local/share/com.reldex.reldex` |
//! | Android, iOS | none — the platform layer passes an explicit path |
//!
//! Windows uses the *local*, not the roaming, application data folder: the
//! file will hold query history (M4.10) and grows, and the credentials its
//! profiles refer to are per machine too (M2.10), so a profile that roamed
//! without its password would only be half there.
//!
//! This is ~40 lines, written here rather than taken from the `dirs` or
//! `directories` crates, neither of which is in the dependency graph
//! (`AGENTS.md`: no production dependency without a reason). The rules are
//! the platforms' documented ones, and only environment variables are read.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The application id, and the name of the data directory.
pub const APP_ID: &str = "com.reldex.reldex";

/// The store's file name inside the data directory.
pub const STORE_FILE_NAME: &str = "reldex.sqlite3";

/// The platform families with different conventions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Platform {
    Windows,
    MacOs,
    Unix,
    /// No per-user data directory Reldex can derive (Android, iOS, others).
    Unsupported,
}

impl Platform {
    pub(crate) const fn current() -> Self {
        if cfg!(target_os = "windows") {
            Self::Windows
        } else if cfg!(target_os = "macos") {
            Self::MacOs
        } else if cfg!(any(target_os = "android", target_os = "ios")) {
            Self::Unsupported
        } else if cfg!(unix) {
            Self::Unix
        } else {
            Self::Unsupported
        }
    }
}

/// An environment variable's value, if set to an absolute path. A relative
/// value is ignored, as the XDG specification requires and as is only safe
/// elsewhere: a relative data directory would move with the working
/// directory.
fn absolute(value: Option<OsString>) -> Option<PathBuf> {
    let path = PathBuf::from(value?);
    path.is_absolute().then_some(path)
}

/// The data directory for `platform`, reading variables through `var`.
pub(crate) fn data_dir_for(
    platform: Platform,
    var: impl Fn(&str) -> Option<OsString>,
) -> Option<PathBuf> {
    let base = match platform {
        Platform::Windows => absolute(var("LOCALAPPDATA"))?,
        Platform::MacOs => absolute(var("HOME"))?
            .join("Library")
            .join("Application Support"),
        Platform::Unix => match absolute(var("XDG_DATA_HOME")) {
            Some(data_home) => data_home,
            None => absolute(var("HOME"))?.join(".local").join("share"),
        },
        Platform::Unsupported => return None,
    };
    Some(base.join(APP_ID))
}

/// The directory the default store lives in, on this platform.
#[must_use]
pub fn default_data_dir() -> Option<PathBuf> {
    data_dir_for(Platform::current(), |name| std::env::var_os(name))
}

/// The default store file, on this platform.
#[must_use]
pub fn default_store_path() -> Option<PathBuf> {
    default_data_dir().map(|dir| dir.join(STORE_FILE_NAME))
}

/// Whether `path` names an in-memory database rather than a file.
pub(crate) fn is_memory(path: &Path) -> bool {
    path.as_os_str() == ":memory:"
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let map: HashMap<String, OsString> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), OsString::from(v)))
            .collect();
        move |name| map.get(name).cloned()
    }

    // Absolute on the platform running the test, so `is_absolute` holds for
    // every row of the table whatever the host is.
    fn root() -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(r"C:\Users\u")
        } else {
            PathBuf::from("/home/u")
        }
    }

    #[test]
    fn windows_uses_local_app_data() {
        let base = root().join("AppData").join("Local");
        let dir = data_dir_for(
            Platform::Windows,
            env(&[
                ("LOCALAPPDATA", base.to_str().expect("utf-8")),
                ("APPDATA", "ignored"),
            ]),
        );
        assert_eq!(dir, Some(base.join(APP_ID)));
        assert_eq!(data_dir_for(Platform::Windows, env(&[])), None);
    }

    #[test]
    fn macos_uses_application_support() {
        let home = root();
        let dir = data_dir_for(
            Platform::MacOs,
            env(&[("HOME", home.to_str().expect("utf-8"))]),
        );
        assert_eq!(
            dir,
            Some(
                home.join("Library")
                    .join("Application Support")
                    .join(APP_ID)
            )
        );
    }

    #[test]
    fn unix_prefers_an_absolute_xdg_data_home_then_home() {
        let home = root();
        let home_text = home.to_str().expect("utf-8");
        let data_home = home.join("data");
        let data_home_text = data_home.to_str().expect("utf-8");
        assert_eq!(
            data_dir_for(
                Platform::Unix,
                env(&[("XDG_DATA_HOME", data_home_text), ("HOME", home_text)])
            ),
            Some(data_home.join(APP_ID))
        );
        let fallback = Some(home.join(".local").join("share").join(APP_ID));
        assert_eq!(
            data_dir_for(Platform::Unix, env(&[("HOME", home_text)])),
            fallback
        );
        assert_eq!(
            data_dir_for(
                Platform::Unix,
                env(&[("XDG_DATA_HOME", "relative/dir"), ("HOME", home_text)])
            ),
            fallback,
            "a relative XDG_DATA_HOME is ignored"
        );
        assert_eq!(data_dir_for(Platform::Unix, env(&[])), None);
    }

    #[test]
    fn mobile_has_no_derived_directory() {
        assert_eq!(
            data_dir_for(Platform::Unsupported, env(&[("HOME", "/x")])),
            None
        );
    }

    #[test]
    fn the_default_path_ends_in_the_app_id_and_file_name() {
        if let Some(path) = default_store_path() {
            assert!(path.ends_with(Path::new(APP_ID).join(STORE_FILE_NAME)));
        }
    }
}

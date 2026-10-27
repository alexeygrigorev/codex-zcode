//! Resolve ZCode configuration beside the Node runtime's home directory.

use std::path::PathBuf;

pub(crate) fn home_dir() -> Option<PathBuf> {
    // Node's os.homedir() honors USERPROFILE on Windows, unlike dirs.
    #[cfg(windows)]
    if let Some(home) = std::env::var_os("USERPROFILE") {
        return Some(PathBuf::from(home));
    }
    dirs::home_dir()
}

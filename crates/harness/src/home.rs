//! Where the harness checkout is, for the tools that read its instance or its state.

use std::ffi::OsString;
use std::path::PathBuf;

/// The value of `name`, or `None` when it is unset or empty, as the scripts' `${NAME:-…}` reads it.
pub fn env_nonempty(name: &str) -> Option<OsString> {
    std::env::var_os(name).filter(|value| !value.is_empty())
}

/// `HARNESS_HOME`, else the checkout whose `target/<profile>/` holds this binary.
pub fn harness_home() -> Option<PathBuf> {
    if let Some(home) = env_nonempty("HARNESS_HOME") {
        return Some(PathBuf::from(home));
    }
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    let target = exe.parent()?.parent()?;
    (target.file_name()? == "target").then(|| target.parent().map(PathBuf::from))?
}

//! Where jefe keeps its config and state.

use std::path::PathBuf;

fn home() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from)
}

/// `$JEFE_CONFIG`, else `$XDG_CONFIG_HOME/jefe/jefe.toml` (or `~/.config/...`).
pub fn config_file() -> PathBuf {
    if let Some(p) = std::env::var_os("JEFE_CONFIG") {
        return PathBuf::from(p);
    }
    std::env::var_os("XDG_CONFIG_HOME")
        .map_or_else(|| home().join(".config"), PathBuf::from)
        .join("jefe/jefe.toml")
}

/// `$JEFE_STATE_DIR`, else `$XDG_STATE_HOME/jefe` (or `~/.local/state/jefe`).
pub fn state_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("JEFE_STATE_DIR") {
        return PathBuf::from(p);
    }
    std::env::var_os("XDG_STATE_HOME")
        .map_or_else(|| home().join(".local/state"), PathBuf::from)
        .join("jefe")
}

/// Every job's directory lives here.
pub fn jobs_dir() -> PathBuf {
    state_dir().join("jobs")
}

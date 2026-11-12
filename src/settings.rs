//! Per-user settings (%LOCALAPPDATA%/anvil/settings.json): choices that belong to the machine, not to a project.

use crate::util;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// The vcpkg chosen with `anvil vcpkg use`.
    pub vcpkg_root: Option<PathBuf>,

    /// vcpkg checkouts added with `anvil vcpkg add`, beyond the ones found automatically.
    pub vcpkg_extra: Vec<PathBuf>,

    /// The toolchain used when a project does not name one.
    pub toolchain: Option<String>,
}

impl Settings {
    pub fn load() -> Settings {
        let text = std::fs::read_to_string(Settings::file()).unwrap_or_default();
        serde_json::from_str(&text).unwrap_or_default()
    }

    pub fn save(&self) -> Result<()> {
        let text = serde_json::to_string_pretty(self)?;
        util::write_atomic(&Settings::file(), text.as_bytes())
    }

    fn file() -> PathBuf {
        util::user_data_dir().join("settings.json")
    }
}

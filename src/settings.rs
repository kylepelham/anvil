//! Per-user settings (%LOCALAPPDATA%/anvil/settings.json): choices that belong to the machine, not to a project.

use crate::util::{self, slash};
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

// A broken settings file is said once per run, however often the settings are read
static WARNED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// The vcpkg chosen with `anvil vcpkg use`.
    pub vcpkg_root: Option<PathBuf>,

    /// vcpkg checkouts added with `anvil vcpkg add`, beyond the ones found automatically.
    pub vcpkg_extra: Vec<PathBuf>,

    /// The toolchain used when a project does not name one.
    pub toolchain: Option<String>,

    /// The file exists but does not parse: the defaults stand in, and saving would lose what it holds.
    #[serde(skip)]
    unreadable: bool,
}

impl Settings {
    pub fn load() -> Settings {
        let file = Settings::file();
        let Ok(text) = std::fs::read_to_string(&file) else {
            return Settings::default();
        };

        match serde_json::from_str(&text) {
            Ok(settings) => settings,
            Err(error) => {
                if !WARNED.swap(true, Ordering::Relaxed) {
                    crate::ui::warn(&format!("{} is not valid ({error}); using the default settings", slash(&file)));
                }
                Settings {
                    unreadable: true,
                    ..Settings::default()
                }
            }
        }
    }

    pub fn save(&self) -> Result<()> {
        if self.unreadable {
            bail!(
                "{} is not valid JSON, and saving would overwrite it: fix or delete it, then run this again",
                slash(&Settings::file())
            );
        }

        let text = serde_json::to_string_pretty(self)?;
        util::write_atomic(&Settings::file(), text.as_bytes())
    }

    fn file() -> PathBuf {
        util::user_data_dir().join("settings.json")
    }
}

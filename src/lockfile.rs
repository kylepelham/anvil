//! anvil.lock: the exact versions a project builds with, so every machine and every later build gets the same ones.
//! It pins each git dependency to a commit and the vcpkg ports tree to a baseline commit (which fixes every port's
//! version), and lists the port versions installed per triplet for review. Written beside anvil.toml whenever it
//! changes; `anvil update` re-resolves, `--locked` refuses to change it.

use crate::util::{self, slash};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const LOCK_NAME: &str = "anvil.lock";

const LOCK_VERSION: u32 = 1;

const HEADER: &str = "# Written by Anvil: the exact versions this project builds with. Commit it.\n\
                      # `anvil update` moves branches and tags to their newest commits and vcpkg to its newest ports.\n\n";

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lock {
    #[serde(default)]
    pub version: u32,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vcpkg: Option<VcpkgLock>,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub git: BTreeMap<String, GitLock>,
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VcpkgLock {
    /// The ports tree commit versions come from (vcpkg's builtin-baseline).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline: Option<String>,

    /// Installed port versions per triplet, for review: the baseline is what fixes them.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub ports: BTreeMap<String, BTreeMap<String, String>>,
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitLock {
    pub url: String,
    /// The tag, branch or commit anvil.toml asks for ("HEAD" for none).
    pub reference: String,
    pub commit: String,
}

/// What `anvil update` re-resolves.
#[derive(Debug, Clone, Default)]
pub enum Refresh {
    #[default]
    Nothing,
    Everything,
    /// Git dependencies by name, and `vcpkg` for the baseline.
    Only(Vec<String>),
}

pub struct LockFile {
    path: PathBuf,
    lock: Lock,
    original: Lock,

    /// Fail rather than change the file.
    locked: bool,
    refresh: Refresh,

    /// Git dependencies this load used: the others are gone from anvil.toml.
    used_git: Vec<String>,
}

impl Refresh {
    fn covers(&self, name: &str) -> bool {
        match self {
            Refresh::Nothing => false,
            Refresh::Everything => true,
            Refresh::Only(names) => names.iter().any(|wanted| wanted == name),
        }
    }
}

impl Lock {
    /// What `anvil update <name>` accepts: the git dependencies, and `vcpkg`.
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.git.keys().cloned().collect();
        if self.vcpkg.is_some() {
            names.push("vcpkg".to_string());
        }
        names
    }
}

impl LockFile {
    pub fn load(root: &Path, locked: bool, refresh: Refresh) -> Result<LockFile> {
        let path = root.join(LOCK_NAME);
        let lock = match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).with_context(|| format!("{} is not a valid lock file", slash(&path)))?,
            Err(_) => Lock::default(),
        };

        if locked && !matches!(refresh, Refresh::Nothing) {
            bail!("--locked and `anvil update` contradict each other");
        }

        Ok(LockFile {
            path,
            original: lock.clone(),
            lock,
            locked,
            refresh,
            used_git: Vec::new(),
        })
    }

    /// The commit a git dependency is locked to, unless anvil.toml asks for something else now or it is refreshed.
    pub fn git_commit(&self, name: &str, url: &str, reference: &str) -> Option<String> {
        if self.refresh.covers(name) {
            return None;
        }

        let entry = self.lock.git.get(name)?;
        (entry.url == url && entry.reference == reference).then(|| entry.commit.clone())
    }

    pub fn set_git(&mut self, name: &str, url: &str, reference: &str, commit: &str) {
        self.used_git.push(name.to_string());
        self.lock.git.insert(
            name.to_string(),
            GitLock {
                url: url.to_string(),
                reference: reference.to_string(),
                commit: commit.to_string(),
            },
        );
    }

    /// The vcpkg ports tree commit the project is locked to, unless refreshed.
    pub fn vcpkg_baseline(&self) -> Option<String> {
        if self.refresh.covers("vcpkg") {
            return None;
        }
        self.lock.vcpkg.as_ref()?.baseline.clone()
    }

    pub fn set_vcpkg(&mut self, baseline: Option<String>, triplet: &str, ports: BTreeMap<String, String>) {
        let vcpkg = self.lock.vcpkg.get_or_insert_with(VcpkgLock::default);
        vcpkg.baseline = baseline;
        if ports.is_empty() {
            vcpkg.ports.remove(triplet);
        } else {
            vcpkg.ports.insert(triplet.to_string(), ports);
        }
    }

    /// Writes the file when it changed (or says what would change, under --locked). Without `uses_vcpkg` the
    /// vcpkg entry goes.
    pub fn finish(mut self, uses_vcpkg: bool) -> Result<()> {
        let used = std::mem::take(&mut self.used_git);
        self.lock.git.retain(|name, _| used.contains(name));
        if !uses_vcpkg {
            self.lock.vcpkg = None;
        }

        let empty = self.lock.git.is_empty() && self.lock.vcpkg.is_none();
        if self.lock == self.original && (self.path.is_file() || empty) {
            return Ok(());
        }

        // Port lists are for review; only pins matter to --locked
        if self.locked && self.pins_changed() {
            bail!(
                "{} is out of date and --locked forbids changing it:\n{}\nRun a build without --locked (or `anvil update`) and commit the file.",
                slash(&self.path),
                self.describe_changes().join("\n")
            );
        }
        if self.locked {
            return Ok(());
        }

        if empty {
            let _ = std::fs::remove_file(&self.path);
            return Ok(());
        }

        self.lock.version = LOCK_VERSION;
        let text = format!("{HEADER}{}", toml::to_string(&self.lock)?);
        for change in self.describe_changes() {
            crate::ui::status("Locked", change.trim());
        }
        util::write_if_changed(&self.path, text.as_bytes())?;
        Ok(())
    }

    fn pins_changed(&self) -> bool {
        let baseline = |lock: &Lock| lock.vcpkg.as_ref().and_then(|vcpkg| vcpkg.baseline.clone());
        self.lock.git != self.original.git || baseline(&self.lock) != baseline(&self.original)
    }

    /// One line per pin or port that changed.
    fn describe_changes(&self) -> Vec<String> {
        let mut lines = Vec::new();
        let short = |commit: &str| commit.chars().take(12).collect::<String>();

        for (name, entry) in &self.lock.git {
            match self.original.git.get(name) {
                Some(old) if old.commit == entry.commit => {}
                Some(old) => lines.push(format!(
                    "  {name}: {} -> {} ({})",
                    short(&old.commit),
                    short(&entry.commit),
                    entry.reference
                )),
                None => lines.push(format!("  {name}: {} ({})", short(&entry.commit), entry.reference)),
            }
        }
        for name in self.original.git.keys().filter(|name| !self.lock.git.contains_key(*name)) {
            lines.push(format!("  {name}: removed"));
        }

        let old_vcpkg = self.original.vcpkg.clone().unwrap_or_default();
        let new_vcpkg = self.lock.vcpkg.clone().unwrap_or_default();
        if old_vcpkg.baseline != new_vcpkg.baseline {
            if let Some(baseline) = &new_vcpkg.baseline {
                lines.push(format!("  vcpkg baseline: {}", short(baseline)));
            }
        }
        for (triplet, ports) in &new_vcpkg.ports {
            let old_ports = old_vcpkg.ports.get(triplet).cloned().unwrap_or_default();
            for (port, version) in ports {
                match old_ports.get(port) {
                    Some(old) if old == version => {}
                    Some(old) => lines.push(format!("  {port} ({triplet}): {old} -> {version}")),
                    None => lines.push(format!("  {port} ({triplet}): {version}")),
                }
            }
        }

        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let mut lock = Lock {
            version: LOCK_VERSION,
            ..Default::default()
        };
        lock.git.insert(
            "json".into(),
            GitLock {
                url: "https://github.com/nlohmann/json".into(),
                reference: "v3.11.3".into(),
                commit: "9cca280a4d0ccf0c08f47a99aa71d1b0e52f8d03".into(),
            },
        );
        let mut ports = BTreeMap::new();
        ports.insert("fmt".to_string(), "11.0.2".to_string());
        lock.vcpkg = Some(VcpkgLock {
            baseline: Some("abc".into()),
            ports: [("x64-windows".to_string(), ports)].into_iter().collect(),
        });

        let text = toml::to_string(&lock).unwrap();
        let read: Lock = toml::from_str(&text).unwrap();
        assert_eq!(read, lock);
    }

    #[test]
    fn refresh_and_mismatch_unlock() {
        let mut file = LockFile {
            path: PathBuf::from("anvil.lock"),
            lock: Lock::default(),
            original: Lock::default(),
            locked: false,
            refresh: Refresh::Nothing,
            used_git: Vec::new(),
        };
        file.set_git("lib", "https://x/lib", "main", "1234");
        assert_eq!(file.git_commit("lib", "https://x/lib", "main").as_deref(), Some("1234"));
        assert_eq!(file.git_commit("lib", "https://x/lib", "v2"), None);

        file.refresh = Refresh::Only(vec!["lib".into()]);
        assert_eq!(file.git_commit("lib", "https://x/lib", "main"), None);
    }
}

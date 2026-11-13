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
    /// Whose anvil.toml declares it: the project's (or a workspace member's) name, or the [deps] name of the
    /// dependency that does. None in locks written before Anvil recorded it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
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

/// What the loaded manifests declare, whether or not this build resolved it.
#[derive(Debug, Default)]
pub struct Declared {
    /// Git dependencies: name, url and reference.
    pub git: Vec<(String, String, String)>,
    /// Every [deps] and [dev-deps] name of the loaded packages.
    pub names: Vec<String>,
    /// Their path and git dependencies.
    pub packages: Vec<String>,
    /// Packages loaded this run (the project, members, and path and git dependencies by [deps] name): their own
    /// declarations are known.
    pub loaded: Vec<String>,
    /// A vcpkg dependency, or the project's own vcpkg.json.
    pub vcpkg: bool,
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

    /// Under --locked, fails before any git or vcpkg work when the build would have to add or change a pin.
    pub fn check_can_pin(&self, what: &str) -> Result<()> {
        if !self.locked {
            return Ok(());
        }

        bail!(
            "{} has no pin for {what}, and --locked forbids adding one. Run a build without --locked (or `anvil update`) and commit the file.",
            slash(&self.path)
        )
    }

    /// Pins a git dependency; `parent` is whose anvil.toml declares it (see `GitLock::parent`).
    pub fn set_git(&mut self, name: &str, url: &str, reference: &str, commit: &str, parent: Option<&str>) {
        self.used_git.push(name.to_string());
        self.lock.git.insert(
            name.to_string(),
            GitLock {
                url: url.to_string(),
                reference: reference.to_string(),
                commit: commit.to_string(),
                parent: parent.map(str::to_string),
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

    /// Does `anvil update` re-resolve this name (a git dependency, or `vcpkg`)?
    pub fn refreshes(&self, name: &str) -> bool {
        self.refresh.covers(name)
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

    /// Writes the file when it changed (or says what would change, under --locked). See `keeps_pin` for the pins
    /// that stay; the vcpkg entry stays while any [deps] entry is a vcpkg port.
    pub fn finish(mut self, declared: &Declared) -> Result<()> {
        let used = std::mem::take(&mut self.used_git);
        let kept: Vec<String> = self
            .lock
            .git
            .keys()
            .filter(|name| self.keeps_pin(name, &used, declared, 0))
            .cloned()
            .collect();
        self.stale_pin_error(&kept, declared)?;
        self.lock.git.retain(|name, _| kept.contains(name));
        if !declared.vcpkg {
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

    /// A pin stays when this build used it; when a loaded package declares it with the same url and reference (behind
    /// an `if`, or another platform's); or when the package that declares it was not loaded this run and is itself
    /// still declared or pinned, so its declarations are unknown.
    fn keeps_pin(&self, name: &str, used: &[String], declared: &Declared, depth: usize) -> bool {
        let Some(pin) = self.lock.git.get(name) else { return false };
        if used.iter().any(|used| used == name) {
            return true;
        }

        // Declared as it was pinned (a changed url or reference would need a new pin)
        let declarations: Vec<_> = declared.git.iter().filter(|(declared, _, _)| declared == name).collect();
        if !declarations.is_empty() {
            return declarations
                .iter()
                .any(|(_, url, reference)| *url == pin.url && *reference == pin.reference);
        }

        // From a lock that does not say who declared it: kept while some path or git package was not loaded
        let Some(parent) = &pin.parent else {
            return declared.packages.iter().any(|package| !declared.loaded.contains(package));
        };

        // Declared by a package this run did not load (a chain of them at most as long as the lock)
        if declared.loaded.contains(parent) || depth > self.lock.git.len() {
            return false;
        }
        declared.names.contains(parent) || self.keeps_pin(parent, used, declared, depth + 1)
    }

    /// Under --locked, a declared git dependency this run did not fetch (behind an `if`) whose url or reference no
    /// longer matches its pin.
    fn stale_pin_error(&self, kept: &[String], declared: &Declared) -> Result<()> {
        if !self.locked {
            return Ok(());
        }

        for (name, url, reference) in &declared.git {
            let Some(pin) = self.lock.git.get(name).filter(|_| !kept.contains(name)) else {
                continue;
            };
            bail!(
                "{} pins git dependency `{name}` to {} @ {}, but anvil.toml asks for {url} @ {reference}, and --locked forbids changing the pin. Run a build without --locked (or `anvil update {name}`) and commit the file.",
                slash(&self.path),
                pin.url,
                pin.reference
            );
        }

        Ok(())
    }

    /// A pin was added, removed or moved (`parent` is bookkeeping: a lock written before it had it still holds).
    fn pins_changed(&self) -> bool {
        let baseline = |lock: &Lock| lock.vcpkg.as_ref().and_then(|vcpkg| vcpkg.baseline.clone());
        let pins = |lock: &Lock| -> Vec<(String, String, String, String)> {
            let entries = lock.git.iter();
            entries
                .map(|(name, pin)| (name.clone(), pin.url.clone(), pin.reference.clone(), pin.commit.clone()))
                .collect()
        };
        pins(&self.lock) != pins(&self.original) || baseline(&self.lock) != baseline(&self.original)
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
                parent: Some("engine".into()),
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
        file.set_git("lib", "https://x/lib", "main", "1234", None);
        assert_eq!(file.git_commit("lib", "https://x/lib", "main").as_deref(), Some("1234"));
        assert_eq!(file.git_commit("lib", "https://x/lib", "v2"), None);

        file.refresh = Refresh::Only(vec!["lib".into()]);
        assert_eq!(file.git_commit("lib", "https://x/lib", "main"), None);
    }

    fn pin(name: &str, parent: Option<&str>) -> GitLock {
        GitLock {
            url: format!("https://x/{name}"),
            reference: "main".into(),
            commit: "1234".into(),
            parent: parent.map(str::to_string),
        }
    }

    fn declared_git(name: &str) -> (String, String, String) {
        (name.to_string(), format!("https://x/{name}"), "main".to_string())
    }

    #[test]
    fn pins_stay_only_while_something_declares_them() {
        let mut lock = Lock::default();
        for (name, parent) in [
            ("used", Some("app")),
            ("gated", Some("app")),
            ("moved", Some("app")),
            ("removed", Some("app")),
            ("of_unloaded", Some("opt")),
            ("of_loaded", Some("alpha")),
            ("of_removed", Some("gone")),
            ("of_kept_pin", Some("of_unloaded")),
            ("legacy", None),
        ] {
            lock.git.insert(name.to_string(), pin(name, parent));
        }
        let file = LockFile {
            path: PathBuf::from("anvil.lock"),
            original: lock.clone(),
            lock,
            locked: false,
            refresh: Refresh::Nothing,
            used_git: Vec::new(),
        };

        // The project `app` declares `gated` (behind an `if`), `moved` at another url, and `opt` (a path dependency
        // not loaded this run); `alpha` was loaded and no longer declares `of_loaded`. `legacy` comes from a lock that
        // does not say who declared it, so it stays while some package (`opt`) was not loaded.
        let mut declared = Declared {
            git: vec![declared_git("gated"), ("moved".into(), "https://y/moved".into(), "main".into())],
            names: vec!["gated".into(), "moved".into(), "opt".into(), "alpha".into()],
            packages: vec!["opt".into(), "alpha".into()],
            loaded: vec!["app".into(), "alpha".into()],
            vcpkg: false,
        };
        let used = vec!["used".to_string()];
        let kept: Vec<String> = file
            .lock
            .git
            .keys()
            .filter(|name| file.keeps_pin(name, &used, &declared, 0))
            .cloned()
            .collect();
        assert_eq!(kept, ["gated", "legacy", "of_kept_pin", "of_unloaded", "used"]);
        declared.loaded.push("opt".into());
        assert!(!file.keeps_pin("legacy", &used, &declared, 0));
        declared.loaded.pop();

        // --locked names the pin whose url changed
        let locked = LockFile { locked: true, ..file };
        let error = locked.stale_pin_error(&kept, &declared).unwrap_err().to_string();
        assert!(error.contains("`moved`") && error.contains("https://y/moved"), "{error}");
    }

    #[test]
    fn unused_declared_pins_stay() {
        let root = std::env::temp_dir().join(format!("anvil-lock-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join(LOCK_NAME);

        // A lock with a pin for `extra` (behind an `if` this build) and one nothing declares any more
        let mut lock = Lock {
            version: LOCK_VERSION,
            vcpkg: Some(VcpkgLock {
                baseline: Some("abc".into()),
                ports: BTreeMap::new(),
            }),
            ..Default::default()
        };
        lock.git.insert("extra".into(), pin("extra", Some("app")));
        lock.git.insert("gone".into(), pin("gone", Some("app")));
        std::fs::write(&path, format!("{HEADER}{}", toml::to_string(&lock).unwrap())).unwrap();

        // Only the undeclared pin goes; the baseline stays for an unused vcpkg dependency
        let declared = Declared {
            git: vec![declared_git("extra")],
            names: vec!["extra".into()],
            packages: vec!["extra".into()],
            loaded: vec!["app".into()],
            vcpkg: true,
        };
        LockFile::load(&root, false, Refresh::Nothing).unwrap().finish(&declared).unwrap();
        let read: Lock = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(read.git.keys().collect::<Vec<_>>(), ["extra"]);
        assert_eq!(read.vcpkg.and_then(|vcpkg| vcpkg.baseline).as_deref(), Some("abc"));

        // --locked accepts the same lock when this build uses none of it
        LockFile::load(&root, true, Refresh::Nothing).unwrap().finish(&declared).unwrap();

        let _ = std::fs::remove_dir_all(&root);
    }
}

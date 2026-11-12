//! vcpkg as a first-class part of the build: every vcpkg on the machine is found automatically, one is chosen (the
//! project's, the user's pick, VCPKG_ROOT, then the best found), packages install on demand only when the
//! requested set changes, and their headers, libraries and DLLs feed the build directly with no CMake toolchain file.

use crate::platform::Os;
use crate::settings::Settings;
use crate::toolchain::Toolchain;
use crate::util::{self, slash};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

const VCPKG_REPOSITORY: &str = "https://github.com/microsoft/vcpkg.git";

/// One vcpkg checkout or installation.
#[derive(Debug, Clone)]
pub struct Instance {
    pub root: PathBuf,

    /// vcpkg.exe, or None when it still needs bootstrapping.
    pub executable: Option<PathBuf>,

    /// How it was found: "VCPKG_ROOT", "PATH", "Visual Studio", "added", "anvil", or a well-known folder.
    pub source: String,

    /// A git checkout with its own ports/ tree (classic); Visual Studio's copy has none and needs a baseline.
    pub has_ports: bool,
}

/// A package a project asked for.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Request {
    pub name: String,
    pub features: Vec<String>,
    pub default_features: bool,
}

/// What `install` needs beyond the requests.
pub struct InstallConfig<'a> {
    pub project_root: &'a Path,
    pub out_root: &'a Path,
    pub triplet: String,
    pub manifest: Option<PathBuf>,
    /// `[vcpkg] baseline` in anvil.toml.
    pub baseline: Option<String>,
    /// The baseline anvil.lock pins.
    pub locked_baseline: Option<String>,
    pub overlay_ports: Vec<PathBuf>,
    pub overlay_triplets: Vec<PathBuf>,
}

/// The result of an install.
pub struct Installed {
    pub tree: crate::vcpkg_tree::InstalledTree,
    /// The project's own vcpkg.json, when it has one.
    pub own_manifest: Option<PathBuf>,
    /// The baseline Anvil's generated manifest pins (none with the project's own).
    pub baseline: Option<String>,
}

impl Instance {
    /// Every vcpkg this machine has, without duplicates, in discovery order.
    pub fn discover() -> Vec<Instance> {
        let mut candidates: Vec<(PathBuf, String)> = Vec::new();

        // Explicit places first
        if let Some(root) = std::env::var_os("VCPKG_ROOT") {
            candidates.push((PathBuf::from(root), "VCPKG_ROOT".into()));
        }
        if let Some(executable) = util::which("vcpkg") {
            if let Some(folder) = executable.parent() {
                candidates.push((folder.to_path_buf(), "PATH".into()));
            }
        }
        for added in Settings::load().vcpkg_extra {
            candidates.push((added, "added".into()));
        }
        candidates.push((util::user_data_dir().join("vcpkg"), "anvil".into()));

        // Visual Studio's bundled copies
        for visual_studio in all_visual_studios() {
            candidates.push((visual_studio.join("VC").join("vcpkg"), "Visual Studio".into()));
        }

        // Well-known folders
        let home = util::home_dir();
        let mut well_known = vec![
            home.join("vcpkg"),
            home.join(".vcpkg-clion").join("vcpkg"),
            home.join("scoop/apps/vcpkg/current"),
        ];
        if cfg!(windows) {
            for drive in ["C:", "D:"] {
                for folder in ["vcpkg", "src/vcpkg", "dev/vcpkg", "tools/vcpkg"] {
                    well_known.push(PathBuf::from(format!("{drive}/{folder}")));
                }
            }
        } else {
            well_known.extend(["/opt/vcpkg", "/usr/local/vcpkg", "/usr/local/share/vcpkg"].map(PathBuf::from));
        }

        for folder in well_known {
            let label = slash(&folder);
            candidates.push((folder, label));
        }

        // Keep each real checkout once
        let mut instances: Vec<Instance> = Vec::new();
        for (root, source) in candidates {
            let Some(instance) = Instance::at(&root, &source) else { continue };
            if instances
                .iter()
                .all(|known| util::path_key(&known.root) != util::path_key(&instance.root))
            {
                instances.push(instance);
            }
        }

        instances
    }

    /// The vcpkg at `root`, if one is there.
    pub fn at(root: &Path, source: &str) -> Option<Instance> {
        let root = util::absolute(&std::env::current_dir().ok()?, root);
        if !root.join(".vcpkg-root").is_file() {
            return None;
        }

        let executable = root.join(if cfg!(windows) { "vcpkg.exe" } else { "vcpkg" });

        Some(Instance {
            executable: executable.is_file().then_some(executable),
            has_ports: root.join("ports").is_dir(),
            source: source.to_string(),
            root,
        })
    }

    /// The vcpkg to use: the project's `[vcpkg] root`, the user's `anvil vcpkg use` choice, VCPKG_ROOT, then the
    /// first found checkout with its own ports, then any. With none at all, one is set up in the Anvil data folder.
    pub fn select(project_root_override: Option<&Path>) -> Result<Instance> {
        if let Some(root) = project_root_override {
            return Instance::at(root, "anvil.toml").with_context(|| format!("[vcpkg] root = \"{}\" is not a vcpkg checkout", slash(root)));
        }

        if let Some(chosen) = Settings::load().vcpkg_root {
            if let Some(instance) = Instance::at(&chosen, "your choice") {
                return Ok(instance);
            }
            crate::ui::warn(&format!(
                "the chosen vcpkg {} is gone; picking another (`anvil vcpkg use` to choose)",
                slash(&chosen)
            ));
        }

        let instances = Instance::discover();
        let preferred = instances
            .iter()
            .find(|instance| instance.source == "VCPKG_ROOT")
            .or_else(|| instances.iter().find(|instance| instance.has_ports))
            .or_else(|| instances.first());

        match preferred {
            Some(instance) => Ok(instance.clone()),
            None => Instance::setup(),
        }
    }

    /// Clones and bootstraps a private vcpkg in the Anvil data folder (first use on a machine without one).
    pub fn setup() -> Result<Instance> {
        let root = util::user_data_dir().join("vcpkg");

        if !root.join(".vcpkg-root").is_file() {
            crate::ui::status("Setting up", &format!("vcpkg in {} (one time)", slash(&root)));
            let git = util::which("git").context("no vcpkg was found and git is not installed to fetch one; install git or vcpkg")?;

            let status = Command::new(git)
                .args(["clone", VCPKG_REPOSITORY])
                .arg(&root)
                .status()
                .context("running git clone")?;
            if !status.success() {
                bail!("git clone {VCPKG_REPOSITORY} failed");
            }
        }

        let mut instance = Instance::at(&root, "anvil").context("the vcpkg clone has no .vcpkg-root")?;
        instance.ensure_bootstrapped()?;

        Ok(instance)
    }

    /// The vcpkg program, bootstrapping it first when the checkout has none yet.
    pub fn ensure_bootstrapped(&mut self) -> Result<PathBuf> {
        if let Some(executable) = &self.executable {
            return Ok(executable.clone());
        }

        crate::ui::status("Bootstrapping", &format!("vcpkg in {}", slash(&self.root)));
        let status = if cfg!(windows) {
            Command::new("cmd.exe")
                .arg("/c")
                .arg(self.root.join("bootstrap-vcpkg.bat"))
                .arg("-disableMetrics")
                .status()
        } else {
            Command::new("sh")
                .arg(self.root.join("bootstrap-vcpkg.sh"))
                .arg("-disableMetrics")
                .status()
        };

        if !status.context("running bootstrap-vcpkg")?.success() {
            bail!("bootstrapping vcpkg in {} failed", slash(&self.root));
        }

        let refreshed = Instance::at(&self.root, &self.source)
            .and_then(|instance| instance.executable)
            .context("bootstrap did not produce vcpkg")?;
        self.executable = Some(refreshed.clone());

        Ok(refreshed)
    }

    /// The vcpkg tool's version line ("2025-12-16-...").
    pub fn version(&self) -> Option<String> {
        let output = Command::new(self.executable.as_ref()?).arg("version").output().ok()?;
        let text = String::from_utf8_lossy(&output.stdout);
        let first_line = text.lines().next()?;

        Some(first_line.rsplit(' ').next()?.to_string())
    }

    /// Does this checkout have a port by that name? (Unknown for instances without a ports tree.)
    pub fn has_port(&self, name: &str) -> Option<bool> {
        self.has_ports.then(|| self.root.join("ports").join(name).is_dir())
    }

    /// Installs the requested packages into `<out>/vcpkg/installed/<triplet>`, skipping vcpkg entirely when nothing
    /// changed since the last install.
    pub fn install(&mut self, requests: &[Request], config: &InstallConfig) -> Result<Installed> {
        // One install root per triplet: in manifest mode vcpkg removes whatever the root holds for other triplets
        let install_root = config.out_root.join("vcpkg").join("installed").join(&config.triplet);
        let (manifest_folder, manifest_text, features, baseline) = self.prepare_manifest(requests, config)?;

        // Skip vcpkg when the inputs match the last successful install
        let stamp = self.install_stamp(&manifest_folder, &format!("{manifest_text}|{}", features.join(",")), config);
        let stamp_file = install_root.join(".anvil.stamp");
        let up_to_date = std::fs::read_to_string(&stamp_file).ok() == Some(stamp.clone()) && install_root.join(&config.triplet).is_dir();

        if !up_to_date {
            self.run_install(&manifest_folder, &install_root, &features, config)?;
            util::write_atomic(&stamp_file, stamp.as_bytes())?;
        }

        let tree = crate::vcpkg_tree::InstalledTree::load(&install_root, &config.triplet)?;
        let own_manifest = Some(manifest_folder.join("vcpkg.json")).filter(|_| !manifest_folder.starts_with(config.out_root));
        Ok(Installed {
            tree,
            own_manifest,
            baseline,
        })
    }

    /// Runs vcpkg with the given arguments in the foreground (for `anvil vcpkg -- ...`).
    pub fn run_passthrough(&mut self, args: &[String]) -> Result<i32> {
        let executable = self.ensure_bootstrapped()?;
        let status = Command::new(executable)
            .args(args)
            .env("VCPKG_ROOT", &self.root)
            .status()
            .context("running vcpkg")?;

        Ok(status.code().unwrap_or(1))
    }

    /// The manifest folder to install from: the project's own vcpkg.json (checked to contain every request, with the
    /// manifest features that hold the ones it lists only there), or one generated from anvil.toml with a pinned
    /// baseline. Also returns that manifest's text, the features to enable, and the baseline it pins.
    fn prepare_manifest(&mut self, requests: &[Request], config: &InstallConfig) -> Result<(PathBuf, String, Vec<String>, Option<String>)> {
        let own_manifest = config
            .manifest
            .clone()
            .or_else(|| Some(config.project_root.join("vcpkg.json")).filter(|path| path.is_file()));

        if let Some(manifest) = own_manifest {
            let text = std::fs::read_to_string(&manifest).with_context(|| format!("reading {}", manifest.display()))?;
            let features = manifest_features_for(&manifest, &text, requests)?;

            let folder = manifest.parent().unwrap().to_path_buf();
            return Ok((folder, text, features, None));
        }

        // Generate one from the requests
        let mut dependencies = Vec::new();
        for request in requests {
            if request.features.is_empty() && request.default_features {
                dependencies.push(serde_json::Value::String(request.name.clone()));
                continue;
            }

            let mut entry = serde_json::Map::new();
            entry.insert("name".into(), request.name.clone().into());
            if !request.features.is_empty() {
                entry.insert("features".into(), request.features.clone().into());
            }
            if !request.default_features {
                entry.insert("default-features".into(), false.into());
            }
            dependencies.push(serde_json::Value::Object(entry));
        }

        let mut manifest = serde_json::Map::new();
        manifest.insert("dependencies".into(), dependencies.into());

        // A baseline pins versions: anvil.toml's, else the lock's, else the ports tree's current commit (or for
        // instances without ports, Visual Studio's, the registry's), which the lock then keeps
        let folder = config.out_root.join("vcpkg");
        let manifest_file = folder.join("vcpkg.json");
        let baseline = config.baseline.clone().or(config.locked_baseline.clone()).or_else(|| {
            if self.has_ports {
                self.ports_commit()
            } else {
                self.baseline_for_portless(&manifest_file)
            }
        });
        if let Some(baseline) = &baseline {
            manifest.insert("builtin-baseline".into(), baseline.clone().into());
        }

        let text = serde_json::to_string_pretty(&serde_json::Value::Object(manifest))? + "\n";
        util::write_if_changed(&manifest_file, text.as_bytes())?;

        Ok((folder, text, Vec::new(), baseline))
    }

    /// The commit the ports tree is checked out at, when it is a git checkout.
    fn ports_commit(&self) -> Option<String> {
        let output = Command::new("git").arg("-C").arg(&self.root).args(["rev-parse", "HEAD"]).output().ok()?;
        let commit = String::from_utf8_lossy(&output.stdout).trim().to_string();
        (output.status.success() && commit.len() == 40).then_some(commit)
    }

    /// A baseline for an instance without a ports tree: the one an earlier run generated, else the one vcpkg itself
    /// picks (`x-update-baseline --add-initial-baseline`), so it stays fixed from then on.
    fn baseline_for_portless(&mut self, manifest_file: &Path) -> Option<String> {
        let read_baseline = |file: &Path| -> Option<String> {
            let json: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(file).ok()?).ok()?;
            json.get("builtin-baseline")?.as_str().map(|baseline| baseline.to_string())
        };

        if let Some(existing) = read_baseline(manifest_file) {
            return Some(existing);
        }

        // Let vcpkg write the current registry baseline into a fresh manifest
        let executable = self.ensure_bootstrapped().ok()?;
        util::write_if_changed(manifest_file, b"{ \"dependencies\": [] }\n").ok()?;
        Command::new(executable)
            .args(["x-update-baseline", "--add-initial-baseline"])
            .arg(format!("--x-manifest-root={}", manifest_file.parent()?.display()))
            .env("VCPKG_ROOT", &self.root)
            .output()
            .ok()?;

        read_baseline(manifest_file)
    }

    /// Everything that decides what an install produces.
    fn install_stamp(&self, manifest_folder: &Path, manifest_text: &str, config: &InstallConfig) -> String {
        let mut text = format!(
            "{}|{}|{}|{}",
            slash(&self.root),
            config.triplet,
            manifest_text,
            util::path_key(manifest_folder)
        );

        // vcpkg-configuration.json beside the manifest changes registries and overlays
        if let Ok(configuration) = std::fs::read_to_string(manifest_folder.join("vcpkg-configuration.json")) {
            text.push_str(&configuration);
        }

        // Overlay folders: every file's time stamp
        for folder in config.overlay_ports.iter().chain(&config.overlay_triplets) {
            for entry in walkdir::WalkDir::new(folder).into_iter().flatten() {
                text.push_str(&format!("|{}={:?}", slash(entry.path()), util::mtime(entry.path())));
            }
        }

        // The vcpkg tool and the ports tree's revision
        if let Some(executable) = &self.executable {
            text.push_str(&format!("|{:?}", util::mtime(executable)));
        }
        text.push_str(&format!("|{:?}", util::mtime(&self.root.join(".git").join("HEAD"))));

        format!("{:016x}", util::hash_bytes(text.as_bytes()))
    }

    fn run_install(&mut self, manifest_folder: &Path, install_root: &Path, features: &[String], config: &InstallConfig) -> Result<()> {
        let executable = self.ensure_bootstrapped()?;
        crate::ui::status(
            "vcpkg",
            &format!("installing packages for {} (using {})", config.triplet, slash(&self.root)),
        );

        let mut command = Command::new(executable);
        command
            .arg("install")
            .arg(format!("--triplet={}", config.triplet))
            .arg(format!("--x-manifest-root={}", manifest_folder.display()))
            .arg(format!("--x-install-root={}", install_root.display()))
            .arg("--no-print-usage")
            .env("VCPKG_ROOT", &self.root);
        for feature in features {
            command.arg(format!("--x-feature={feature}"));
        }
        for folder in &config.overlay_ports {
            command.arg(format!("--overlay-ports={}", folder.display()));
        }
        for folder in &config.overlay_triplets {
            command.arg(format!("--overlay-triplets={}", folder.display()));
        }

        let status = command.status().context("running vcpkg install")?;
        if !status.success() {
            bail!("vcpkg install failed (see its output above)");
        }

        Ok(())
    }
}

/// The default triplet for what a toolchain builds.
pub fn default_triplet(toolchain: &Toolchain, static_runtime: bool) -> String {
    let architecture = toolchain.platform.cpu.name();

    match toolchain.platform.os {
        Os::Windows if !toolchain.is_msvc_abi() => format!("{architecture}-mingw-dynamic"),
        Os::Windows => {
            let suffix = if static_runtime { "-static" } else { "" };
            format!("{architecture}-windows{suffix}")
        }
        Os::Macos => format!("{architecture}-osx"),
        Os::Linux => format!("{architecture}-linux"),
    }
}

/// Every Visual Studio installation (not just the newest), for their bundled vcpkg.
pub fn all_visual_studios() -> Vec<PathBuf> {
    if !cfg!(windows) {
        return Vec::new();
    }

    let program_files = std::env::var("ProgramFiles(x86)").unwrap_or_else(|_| r"C:\Program Files (x86)".into());
    let vswhere = PathBuf::from(program_files).join(r"Microsoft Visual Studio\Installer\vswhere.exe");

    let Ok(output) = Command::new(vswhere)
        .args(["-all", "-products", "*", "-prerelease", "-property", "installationPath"])
        .output()
    else {
        return Vec::new();
    };

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| PathBuf::from(line.trim()))
        .filter(|folder| folder.is_dir())
        .collect()
}

/// The manifest features to enable so every request is installed: none for those in "dependencies", the first
/// feature that lists one otherwise. A request listed nowhere may still come in through a listed package; whether it
/// did is checked after the install, for the packages a target links.
fn manifest_features_for(manifest: &Path, text: &str, requests: &[Request]) -> Result<Vec<String>> {
    let json: serde_json::Value = serde_json::from_str(text).with_context(|| format!("{} is not valid JSON", manifest.display()))?;
    let listed = dependency_names(json.get("dependencies"));

    let mut features = Vec::new();
    for name in requests.iter().map(|request| request.name.as_str()) {
        if listed.iter().any(|listed| listed == name) {
            continue;
        }

        let holder = json.get("features").and_then(|features| features.as_object()).and_then(|features| {
            features
                .iter()
                .find(|(_, feature)| dependency_names(feature.get("dependencies")).iter().any(|listed| listed == name))
                .map(|(feature_name, _)| feature_name.clone())
        });
        if let Some(feature) = holder.filter(|feature| !features.contains(feature)) {
            features.push(feature);
        }
    }

    Ok(features)
}

/// Port names in a vcpkg.json "dependencies" list: "name" strings and { "name": ... } objects.
pub fn dependency_names(dependencies: Option<&serde_json::Value>) -> Vec<String> {
    dependencies
        .and_then(|dependencies| dependencies.as_array())
        .map(|dependencies| {
            dependencies
                .iter()
                .filter_map(|dependency| dependency.as_str().or_else(|| dependency.get("name").and_then(|name| name.as_str())))
                .map(|name| name.to_string())
                .collect()
        })
        .unwrap_or_default()
}

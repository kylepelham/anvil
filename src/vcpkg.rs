//! vcpkg as a first-class part of the build: every vcpkg on the machine is found automatically, one is chosen (the
//! project's, the user's pick, VCPKG_ROOT, then the best found), packages install on demand only when the
//! requested set changes, and their headers, libraries and DLLs feed the build directly with no CMake toolchain file.

use crate::manifest::VcpkgSpec;
use crate::platform::Os;
use crate::settings::Settings;
use crate::toolchain::Toolchain;
use crate::util::{self, slash};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

const VCPKG_REPOSITORY: &str = "https://github.com/microsoft/vcpkg.git";

// The install stamp's name, in each triplet's install folder
const STAMP_NAME: &str = ".anvil.stamp";

// The processors vcpkg's triplet names start with
const KNOWN_CPUS: &[&str] = &[
    "x64",
    "x86",
    "arm64",
    "arm",
    "arm64ec",
    "wasm32",
    "ppc64le",
    "s390x",
    "riscv64",
    "loongarch64",
];

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
    /// `[project]` name and version, which the generated manifest carries (vcpkg names them in its errors).
    pub project_name: &'a str,
    pub project_version: &'a str,
    pub out_root: &'a Path,
    pub triplet: String,
    /// The project's own vcpkg.json (checked by `own_manifest`).
    pub manifest: Option<PathBuf>,
    /// `[vcpkg] baseline` in anvil.toml.
    pub baseline: Option<String>,
    /// The baseline anvil.lock pins.
    pub locked_baseline: Option<String>,
    /// `anvil update`: take the newest baseline, not one an earlier run stored.
    pub refresh_baseline: bool,
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
        Instance::select_with(project_root_override, None)
    }

    /// `select`, trying `remembered` (the one the last install used) before searching the machine, which runs
    /// vswhere and reads PATH.
    pub fn select_with(project_root_override: Option<&Path>, remembered: Option<&Path>) -> Result<Instance> {
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

        let from_environment = std::env::var_os("VCPKG_ROOT").and_then(|root| Instance::at(Path::new(&root), "VCPKG_ROOT"));
        if let Some(instance) = from_environment {
            return Ok(instance);
        }
        if let Some(instance) = remembered.and_then(|root| Instance::at(root, "the last install")) {
            return Ok(instance);
        }

        let instances = Instance::discover();
        let preferred = instances.iter().find(|instance| instance.has_ports).or_else(|| instances.first());

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

        // Skip vcpkg when the inputs match the last successful install; the stamp's first line is the vcpkg it used
        let stamp = format!(
            "{}\n{}",
            slash(&self.root),
            self.install_stamp(&manifest_folder, &format!("{manifest_text}|{}", features.join(",")), config)
        );
        let stamp_file = install_root.join(STAMP_NAME);
        let up_to_date = std::fs::read_to_string(&stamp_file).ok() == Some(stamp.clone()) && install_root.join(&config.triplet).is_dir();

        if !up_to_date {
            self.check_requests(requests, &manifest_folder, config)?;
            if let Some(baseline) = baseline.as_deref().filter(|_| config.manifest.is_none()) {
                self.check_baseline(baseline, config)?;
            }
            self.run_install(&manifest_folder, &install_root, &features, config)?;
            util::write_atomic(&stamp_file, stamp.as_bytes())?;
        }

        let tree = crate::vcpkg_tree::InstalledTree::load(&install_root, &config.triplet)?;
        Ok(Installed {
            tree,
            own_manifest: config.manifest.clone(),
            baseline,
        })
    }

    /// Whether `name` is a port this instance knows: in its ports tree, an overlay folder, or one that
    /// the vcpkg-configuration.json beside the project's own vcpkg.json adds (vcpkg reads none without one). None
    /// when it cannot tell: no ports tree, or registries.
    pub fn knows_port(&self, name: &str, overlay_ports: &[PathBuf], configuration_folder: Option<&Path>) -> Option<bool> {
        let overlays = all_overlay_ports(overlay_ports, configuration_folder)?;
        if overlays.iter().any(|folder| overlay_has_port(folder, name)) {
            return Some(true);
        }

        self.has_port(name)
    }

    /// "vcpkg has no port `x`", with the closest port name.
    pub fn unknown_port_error(&self, name: &str, overlay_ports: &[PathBuf]) -> anyhow::Error {
        let mut port_names: Vec<String> = Vec::new();
        for folder in std::iter::once(&self.root.join("ports")).chain(overlay_ports) {
            let entries = std::fs::read_dir(folder).into_iter().flatten().flatten();
            port_names.extend(entries.map(|entry| entry.file_name().to_string_lossy().into_owned()));
        }

        let hint = util::suggest(name, port_names.iter().map(|port| port.as_str()))
            .map(|close| format!(" Did you mean `{close}`?"))
            .unwrap_or_default();
        let overlays = if overlay_ports.is_empty() { "" } else { " or the overlay ports" };
        anyhow::anyhow!(
            "vcpkg has no port `{name}` (in {}{overlays}).{hint} Search with `anvil vcpkg search {name}`.",
            slash(&self.root.join("ports"))
        )
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
        if let Some(manifest) = &config.manifest {
            let text = std::fs::read_to_string(manifest).with_context(|| format!("reading {}", manifest.display()))?;
            let port_defaults = |name: &str| self.port_default_features(name, &config.overlay_ports, &config.triplet);
            let features = manifest_features_for(manifest, &text, requests, &config.triplet, &port_defaults)?;

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

        // Named after the project, so vcpkg's messages ("needed by ...") say whose it is
        let mut manifest = serde_json::Map::new();
        manifest.insert("name".into(), manifest_name(config.project_name).into());
        manifest.insert("version-string".into(), config.project_version.into());
        manifest.insert("dependencies".into(), dependencies.into());

        // A baseline pins versions: anvil.toml's, else the lock's, else the ports tree's current commit (or for
        // instances without ports, Visual Studio's, the registry's), which the lock then keeps
        let folder = config.out_root.join("vcpkg");
        let manifest_file = folder.join("vcpkg.json");
        let baseline = config.baseline.clone().or(config.locked_baseline.clone()).or_else(|| {
            if self.has_ports {
                self.ports_commit()
            } else {
                self.baseline_for_portless(&manifest_file, config.refresh_baseline)
            }
        });
        if let Some(baseline) = &baseline {
            manifest.insert("builtin-baseline".into(), baseline.clone().into());
        }

        let text = serde_json::to_string_pretty(&serde_json::Value::Object(manifest))? + "\n";
        util::write_if_changed(&manifest_file, text.as_bytes())?;

        Ok((folder, text, Vec::new(), baseline))
    }

    /// A port's default features for a triplet, from its vcpkg.json in an overlay folder or the ports tree. None when
    /// it cannot be read (registries, Visual Studio's copy).
    fn port_default_features(&self, name: &str, overlay_ports: &[PathBuf], triplet: &str) -> Option<Vec<String>> {
        let candidates = overlay_ports
            .iter()
            .flat_map(|folder| [folder.join(name).join("vcpkg.json"), folder.join("vcpkg.json")])
            .chain([self.root.join("ports").join(name).join("vcpkg.json")]);

        for file in candidates {
            let Some(json) = std::fs::read_to_string(&file)
                .ok()
                .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            else {
                continue;
            };
            if json.get("name").and_then(|own| own.as_str()) == Some(name) {
                return Some(feature_names(json.get("default-features"), triplet));
            }
        }

        None
    }

    /// The commit the ports tree is checked out at, when it is a git checkout.
    fn ports_commit(&self) -> Option<String> {
        let output = Command::new("git").arg("-C").arg(&self.root).args(["rev-parse", "HEAD"]).output().ok()?;
        let commit = String::from_utf8_lossy(&output.stdout).trim().to_string();
        (output.status.success() && commit.len() == 40).then_some(commit)
    }

    /// A baseline for an instance without a ports tree: the one an earlier run generated (unless refreshing), else the
    /// one vcpkg itself picks (`x-update-baseline --add-initial-baseline`), so it stays fixed from then on.
    fn baseline_for_portless(&mut self, manifest_file: &Path, refresh: bool) -> Option<String> {
        let read_baseline = |file: &Path| -> Option<String> {
            let json: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(file).ok()?).ok()?;
            json.get("builtin-baseline")?.as_str().map(|baseline| baseline.to_string())
        };

        if let Some(existing) = read_baseline(manifest_file).filter(|_| !refresh) {
            return Some(existing);
        }

        // Let vcpkg write the current registry baseline into a fresh manifest
        let executable = self.ensure_bootstrapped().ok()?;
        util::write_if_changed(manifest_file, b"{ \"dependencies\": [] }\n").ok()?;
        Command::new(executable)
            .args(["x-update-baseline", "--add-initial-baseline", "--x-wait-for-lock"])
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
        text.push_str(&format!("|{:?}", checkout_commit(&self.root)));

        format!("{:016x}", util::hash_bytes(text.as_bytes()))
    }

    /// Fails with a suggestion for a port name that neither the ports tree nor an overlay has.
    fn check_requests(&self, requests: &[Request], manifest_folder: &Path, config: &InstallConfig) -> Result<()> {
        for request in requests {
            if self.knows_port(&request.name, &config.overlay_ports, Some(manifest_folder)) == Some(false) {
                return Err(self
                    .unknown_port_error(&request.name, &config.overlay_ports)
                    .context(format!("`{}` is a vcpkg port in anvil.toml's [deps]", request.name)));
            }
        }

        Ok(())
    }

    /// A baseline the checkout lacks fails inside vcpkg with a raw `git show` error: say what to do instead.
    fn check_baseline(&self, baseline: &str, config: &InstallConfig) -> Result<()> {
        if !self.has_ports || !self.root.join(".git").exists() {
            return Ok(());
        }

        let present = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(["cat-file", "-e", &format!("{baseline}^{{commit}}")])
            .output()
            .is_ok_and(|output| output.status.success());
        if present {
            return Ok(());
        }

        let root = slash(&self.root);
        if config.baseline.as_deref() == Some(baseline) {
            bail!(
                "[vcpkg] baseline = \"{baseline}\" is a ports tree commit the vcpkg checkout at {root} does not have. Run `git -C {root} fetch` to get it, or change the baseline."
            );
        }
        bail!(
            "anvil.lock pins vcpkg baseline {baseline}, a ports tree commit the vcpkg checkout at {root} does not have (the lock was written with a newer or other vcpkg). Run `git -C {root} pull` to get it, or `anvil update vcpkg` to lock this checkout's ports instead (and commit anvil.lock)."
        )
    }

    fn run_install(&mut self, manifest_folder: &Path, install_root: &Path, features: &[String], config: &InstallConfig) -> Result<()> {
        let executable = self.ensure_bootstrapped()?;
        crate::ui::status(
            "vcpkg",
            &format!("installing packages for {} (using {})", config.triplet, slash(&self.root)),
        );

        // Another build may be using this vcpkg: wait for its lock rather than fail
        let mut command = Command::new(executable);
        command
            .arg("install")
            .arg(format!("--triplet={}", config.triplet))
            .arg(format!("--x-manifest-root={}", manifest_folder.display()))
            .arg(format!("--x-install-root={}", install_root.display()))
            .arg("--no-print-usage")
            .arg("--x-wait-for-lock")
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

        // -q: vcpkg's output only when it fails
        if crate::ui::quiet() {
            let output = command.output().context("running vcpkg install")?;
            if !output.status.success() {
                crate::ui::print_block(&String::from_utf8_lossy(&output.stdout));
                crate::ui::print_block(&String::from_utf8_lossy(&output.stderr));
                bail!("vcpkg install failed (its output is above)");
            }
            return Ok(());
        }

        let status = command.status().context("running vcpkg install")?;
        if !status.success() {
            bail!("vcpkg install failed (see its output above)");
        }

        Ok(())
    }
}

/// The vcpkg the last install into `out_root` for `triplet` used (the stamp's first line).
pub fn remembered_root(out_root: &Path, triplet: &str) -> Option<PathBuf> {
    let stamp_file = out_root.join("vcpkg").join("installed").join(triplet).join(STAMP_NAME);
    let text = std::fs::read_to_string(stamp_file).ok()?;

    text.lines().next().filter(|line| !line.is_empty()).map(PathBuf::from)
}

/// The project's own vcpkg.json: `[vcpkg] manifest` (a folder, or the vcpkg.json in one), else one beside anvil.toml.
pub fn own_manifest(project_root: &Path, configured: Option<&Path>) -> Result<Option<PathBuf>> {
    let Some(configured) = configured else {
        return Ok(Some(project_root.join("vcpkg.json")).filter(|file| file.is_file()));
    };

    // vcpkg reads only a file named vcpkg.json, from the folder it is given
    let file = if configured.is_dir() {
        configured.join("vcpkg.json")
    } else {
        configured.to_path_buf()
    };
    if file.file_name().is_none_or(|name| name != "vcpkg.json") {
        bail!(
            "[vcpkg] manifest = \"{}\": vcpkg reads only a file named vcpkg.json; name the folder that holds it (or rename the file)",
            slash(configured)
        );
    }
    if !file.is_file() {
        bail!("[vcpkg] manifest: {} does not exist", slash(&file));
    }

    Ok(Some(file))
}

/// The commit a git checkout is at, read from its files (HEAD, the branch it names, or packed-refs): a pull or a
/// checkout changes it, which a time stamp of HEAD misses when HEAD names a branch.
fn checkout_commit(root: &Path) -> Option<String> {
    let git_folder = root.join(".git");
    let head = std::fs::read_to_string(git_folder.join("HEAD")).ok()?;
    let Some(reference) = head.trim().strip_prefix("ref: ") else {
        return Some(head.trim().to_string());
    };

    if let Ok(commit) = std::fs::read_to_string(git_folder.join(reference)) {
        return Some(commit.trim().to_string());
    }
    let packed = std::fs::read_to_string(git_folder.join("packed-refs")).ok()?;
    packed
        .lines()
        .filter_map(|line| line.split_once(' '))
        .find(|(_, name)| *name == reference)
        .map(|(commit, _)| commit.to_string())
}

/// A vcpkg manifest name from the project's: lowercase letters and digits joined by single dashes.
fn manifest_name(project_name: &str) -> String {
    let mut name = String::new();
    for character in project_name.chars() {
        if character.is_ascii_alphanumeric() {
            name.push(character.to_ascii_lowercase());
        } else if !name.is_empty() && !name.ends_with('-') {
            name.push('-');
        }
    }
    let name = name.trim_end_matches('-');

    // Names vcpkg reserves (Windows device names among them)
    let device = name.len() == 4 && (name.starts_with("lpt") || name.starts_with("com")) && name.ends_with(|last: char| last.is_ascii_digit());
    let reserved = device || ["core", "default", "prn", "aux", "nul", "con"].contains(&name);
    match name {
        "" => "anvil-project".to_string(),
        _ if reserved => format!("{name}-project"),
        _ => name.to_string(),
    }
}

/// Overlay port folders: anvil.toml's and those vcpkg-configuration.json in `configuration_folder` names. None when
/// that file adds registries, whose ports Anvil cannot list.
fn all_overlay_ports(overlay_ports: &[PathBuf], configuration_folder: Option<&Path>) -> Option<Vec<PathBuf>> {
    let mut folders = overlay_ports.to_vec();

    let Some(configuration_folder) = configuration_folder else {
        return Some(folders);
    };
    let configuration_file = configuration_folder.join("vcpkg-configuration.json");
    let Ok(text) = std::fs::read_to_string(&configuration_file) else {
        return Some(folders);
    };
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    if json.get("registries").is_some() || json.get("default-registry").is_some() {
        return None;
    }

    let listed = json.get("overlay-ports").and_then(|list| list.as_array()).into_iter().flatten();
    folders.extend(
        listed
            .filter_map(|folder| folder.as_str())
            .map(|folder| util::absolute(configuration_folder, Path::new(folder))),
    );
    Some(folders)
}

/// An overlay folder holds the port: as a subfolder, or it is that port's folder itself.
fn overlay_has_port(folder: &Path, name: &str) -> bool {
    if folder.join(name).is_dir() {
        return true;
    }

    let own_name = std::fs::read_to_string(folder.join("vcpkg.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|json| json.get("name")?.as_str().map(|name| name.to_string()));
    own_name.as_deref() == Some(name)
}

/// The triplet for a build: `[vcpkg] triplet` when it fits the machine and compiler built for, else Anvil's pick for
/// them, with `[vcpkg] linkage`.
pub fn choose_triplet(spec: &VcpkgSpec, toolchain: &Toolchain, static_runtime: bool) -> Result<String> {
    let linkage = match spec.linkage.as_deref() {
        None => None,
        Some("static") => Some(true),
        Some("dynamic") => Some(false),
        Some(other) => bail!("[vcpkg] linkage = \"{other}\": expected \"static\" (static libraries) or \"dynamic\" (DLLs or shared libraries)"),
    };

    let Some(triplet) = &spec.triplet else {
        return default_triplet(toolchain, static_runtime, linkage);
    };
    if linkage.is_some() {
        bail!("[vcpkg] has both `triplet` and `linkage`: keep `linkage`, which picks the triplet for each machine and compiler");
    }

    // A triplet named <cpu>-<system>... must be for the machine and the compiler's ABI (custom names pass)
    let mut parts = triplet.split('-');
    let (Some(cpu), Some(system)) = (parts.next(), parts.next()) else {
        return Ok(triplet.clone());
    };
    if !KNOWN_CPUS.contains(&cpu) {
        return Ok(triplet.clone());
    }

    let platform = &toolchain.platform;
    let wanted_system = match platform.os {
        Os::Windows if toolchain.is_msvc_abi() => "windows",
        Os::Windows => "mingw",
        Os::Linux => "linux",
        Os::Macos => "osx",
    };
    let fits_system = system == wanted_system || (wanted_system == "windows" && system == "uwp");
    if cpu == platform.cpu.name() && fits_system {
        // Its C runtime must be the profile's too (LNK2038 otherwise)
        if let Some(static_crt) = named_static_crt(triplet).filter(|static_crt| *static_crt != static_runtime) {
            return Err(runtime_mismatch(triplet, static_crt, toolchain));
        }
        return Ok(triplet.clone());
    }

    // What Anvil would pick, keeping the triplet's linkage
    let implied_linkage = if triplet.contains("static") {
        Some(true)
    } else if triplet.contains("dynamic") {
        Some(false)
    } else {
        None
    };
    let pick = default_triplet(toolchain, static_runtime, implied_linkage)?;
    let instead = match implied_linkage {
        Some(true) => "replace it with `linkage = \"static\"`, which picks the static triplet for each machine and compiler",
        _ => "remove it, and Anvil picks the triplet for each machine and compiler",
    };
    bail!(
        "[vcpkg] triplet = \"{triplet}\" is not for this build ({} with {}): its libraries would not link. This build needs \"{pick}\"; {instead}.",
        platform.name(),
        toolchain.family.name()
    )
}

/// A custom `[vcpkg] triplet` (one whose name does not say its C runtime) must use the profile's C runtime too:
/// VCPKG_CRT_LINKAGE from its file, in the overlay triplet folders, else the vcpkg's triplets/ or triplets/community/.
/// Passes when no file is found.
pub fn check_triplet_runtime(
    triplet: &str,
    toolchain: &Toolchain,
    static_runtime: bool,
    overlay_triplets: &[PathBuf],
    vcpkg_root: &Path,
) -> Result<()> {
    if !toolchain.is_msvc_abi() || named_static_crt(triplet).is_some() {
        return Ok(());
    }

    let file_name = format!("{triplet}.cmake");
    let folders = overlay_triplets
        .iter()
        .cloned()
        .chain([vcpkg_root.join("triplets"), vcpkg_root.join("triplets").join("community")]);
    let Some(text) = folders
        .map(|folder| folder.join(&file_name))
        .find_map(|file| std::fs::read_to_string(file).ok())
    else {
        return Ok(());
    };

    match crt_linkage_of(&text) {
        Some(static_crt) if static_crt != static_runtime => Err(runtime_mismatch(triplet, static_crt, toolchain)),
        _ => Ok(()),
    }
}

/// Whether a Windows triplet of vcpkg's (`x64-windows`, `x64-windows-static`, `x64-windows-static-md`,
/// `x64-uwp`, ...) builds against the static C runtime: `-static` without `-md`. None for other names.
fn named_static_crt(triplet: &str) -> Option<bool> {
    let parts: Vec<&str> = triplet.split('-').collect();
    if parts.len() < 2 || !["windows", "uwp"].contains(&parts[1]) || !KNOWN_CPUS.contains(&parts[0]) {
        return None;
    }

    let rest = &parts[2..];
    Some(rest.contains(&"static") && !rest.contains(&"md"))
}

/// `set(VCPKG_CRT_LINKAGE static|dynamic)` in a triplet file: true for static.
fn crt_linkage_of(text: &str) -> Option<bool> {
    let pattern = regex::Regex::new(r"(?i)set\s*\(\s*VCPKG_CRT_LINKAGE\s+(\w+)").unwrap();
    let value = pattern.captures(text)?.get(1)?.as_str().to_ascii_lowercase();

    Some(value == "static")
}

/// A triplet whose C runtime is not the profile's: its libraries would not link.
fn runtime_mismatch(triplet: &str, static_crt: bool, toolchain: &Toolchain) -> anyhow::Error {
    let architecture = toolchain.platform.cpu.name();
    if static_crt {
        return anyhow::anyhow!(
            "[vcpkg] triplet = \"{triplet}\" builds its libraries against the static C runtime (/MT), but this profile uses the DLL runtime (/MD): they would not link (LNK2038). Set `static-runtime = true` in the profile, or replace the triplet with `linkage = \"static\"`, which picks static libraries on the profile's runtime ({architecture}-windows-static-md here, {architecture}-windows-static with static-runtime)."
        );
    }
    anyhow::anyhow!(
        "[vcpkg] triplet = \"{triplet}\" builds its libraries against the DLL C runtime (/MD), but this profile has `static-runtime = true` (/MT): they would not link (LNK2038). Remove the triplet (Anvil picks {architecture}-windows-static for static-runtime profiles), or remove `static-runtime` from the profile."
    )
}

/// The default triplet for what a toolchain builds; `static_libraries` asks for static libraries or DLLs instead of
/// the system's usual kind.
fn default_triplet(toolchain: &Toolchain, static_runtime: bool, static_libraries: Option<bool>) -> Result<String> {
    let architecture = toolchain.platform.cpu.name();

    Ok(match (toolchain.platform.os, static_libraries) {
        (Os::Windows, _) if !toolchain.is_msvc_abi() => {
            let linkage = if static_libraries.unwrap_or(static_runtime) {
                "static"
            } else {
                "dynamic"
            };
            format!("{architecture}-mingw-{linkage}")
        }
        (Os::Windows, Some(false)) if static_runtime => {
            bail!("[vcpkg] linkage = \"dynamic\" and a profile with static-runtime: vcpkg's DLLs use the DLL runtime")
        }
        (Os::Windows, _) if static_runtime => format!("{architecture}-windows-static"),
        (Os::Windows, Some(true)) => format!("{architecture}-windows-static-md"),
        (Os::Windows, _) => format!("{architecture}-windows"),
        (Os::Macos, Some(false)) => format!("{architecture}-osx-dynamic"),
        (Os::Macos, _) => format!("{architecture}-osx"),
        (Os::Linux, Some(false)) => format!("{architecture}-linux-dynamic"),
        (Os::Linux, _) => format!("{architecture}-linux"),
    })
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
fn manifest_features_for(
    manifest: &Path,
    text: &str,
    requests: &[Request],
    triplet: &str,
    port_defaults: &dyn Fn(&str) -> Option<Vec<String>>,
) -> Result<Vec<String>> {
    let json: serde_json::Value = serde_json::from_str(text).with_context(|| format!("{} is not valid JSON", manifest.display()))?;
    check_requested_features(manifest, &json, requests, triplet, port_defaults)?;
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

/// The project's own vcpkg.json decides what installs (CMake reads it too, and vcpkg takes port features only from a
/// manifest), so the port features anvil.toml asks for must be in it, or be the port's defaults it keeps: say exactly
/// what to add. Only entries whose "platform" holds for the triplet count.
fn check_requested_features(
    manifest: &Path,
    json: &serde_json::Value,
    requests: &[Request],
    triplet: &str,
    port_defaults: &dyn Fn(&str) -> Option<Vec<String>>,
) -> Result<()> {
    let file = slash(manifest);

    for request in requests {
        let name = &request.name;
        let entries = find_dependencies(json, name, triplet);
        let mut entry_features = Vec::new();
        for entry in &entries {
            entry_features.extend(feature_names(entry.get("features"), triplet));
        }

        // Default features stay unless every entry turns them off (a port listed nowhere comes in with them)
        let keeps_defaults = entries.is_empty()
            || entries
                .iter()
                .any(|entry| entry.get("default-features").and_then(|value| value.as_bool()) != Some(false));
        let defaults = port_defaults(name);
        let is_default = |feature: &String| keeps_defaults && defaults.as_ref().is_some_and(|defaults| defaults.contains(feature));

        let missing: Vec<String> = request
            .features
            .iter()
            .filter(|feature| !entry_features.contains(feature) && !is_default(feature))
            .map(|feature| format!("\"{feature}\""))
            .collect();
        if !missing.is_empty() {
            let list = missing.join(", ");
            let fix = if entries.is_empty() {
                format!("add {{ \"name\": \"{name}\", \"features\": [{list}] }} to its \"dependencies\"")
            } else if entry_features.is_empty() {
                format!("add \"features\": [{list}] to {name} in vcpkg.json")
            } else {
                format!("add {list} to the \"features\" of {name} in vcpkg.json")
            };
            bail!("anvil.toml asks for {name} with features [{list}], which {file} does not turn on (with a vcpkg.json, it decides what installs): {fix}");
        }

        // Default features installed anyway only cost build time: say so when the port has some
        let has_defaults = defaults.is_none_or(|defaults| !defaults.is_empty());
        if !request.default_features && !entries.is_empty() && keeps_defaults && has_defaults {
            crate::ui::warn(&format!(
                "anvil.toml asks for {name} without its default features, but {file} keeps them, so they install (with a vcpkg.json, it decides what installs): add \"default-features\": false to {name} in vcpkg.json"
            ));
        }
    }

    Ok(())
}

/// A port's entries in a vcpkg.json whose "platform" holds for the triplet: those in "dependencies", else those in
/// manifest features'.
fn find_dependencies<'a>(json: &'a serde_json::Value, name: &str, triplet: &str) -> Vec<&'a serde_json::Value> {
    let applies = |dependency: &&'a serde_json::Value| {
        let own_name = dependency.as_str().or_else(|| dependency.get("name").and_then(|name| name.as_str()));
        let platform = dependency.get("platform").and_then(|platform| platform.as_str());
        own_name == Some(name) && platform.is_none_or(|platform| platform_matches(platform, triplet))
    };
    let in_list = |list: Option<&'a serde_json::Value>| -> Vec<&'a serde_json::Value> {
        list.and_then(|list| list.as_array())
            .map(|list| list.iter().filter(applies).collect())
            .unwrap_or_default()
    };

    let listed = in_list(json.get("dependencies"));
    if !listed.is_empty() {
        return listed;
    }
    let features = json.get("features").and_then(|features| features.as_object());
    features
        .into_iter()
        .flat_map(|features| features.values())
        .flat_map(|feature| in_list(feature.get("dependencies")))
        .collect()
}

/// Feature names in a "features" or "default-features" list ("name" strings and { "name", "platform" } objects) that
/// apply to the triplet.
fn feature_names(list: Option<&serde_json::Value>, triplet: &str) -> Vec<String> {
    let items = list.and_then(|list| list.as_array()).into_iter().flatten();

    items
        .filter_map(|item| {
            let platform = item.get("platform").and_then(|platform| platform.as_str());
            if platform.is_some_and(|platform| !platform_matches(platform, triplet)) {
                return None;
            }
            item.as_str()
                .or_else(|| item.get("name").and_then(|name| name.as_str()))
                .map(str::to_string)
        })
        .collect()
}

/// Whether a vcpkg platform expression (`"windows & !static"`, `"(linux | osx), x64"`) holds for a triplet, judged
/// from its name: `!`/`not`, `&`/`and`, `|`, `,` (or, loosest) and parentheses over vcpkg's identifiers. One that does
/// not parse holds, so nothing is dropped by mistake.
pub fn platform_matches(expression: &str, triplet: &str) -> bool {
    let mut tokens = Vec::new();
    let mut characters = expression.chars().peekable();
    while let Some(&character) = characters.peek() {
        if character.is_whitespace() {
            characters.next();
        } else if character.is_ascii_alphanumeric() || character == '_' {
            let mut word = String::new();
            while let Some(&next) = characters.peek().filter(|next| next.is_ascii_alphanumeric() || **next == '_') {
                word.push(next);
                characters.next();
            }
            tokens.push(word);
        } else {
            tokens.push(character.to_string());
            characters.next();
        }
    }

    let facts = TripletFacts::from_name(triplet);
    let mut parser = PlatformParser {
        tokens: &tokens,
        position: 0,
        facts: &facts,
    };
    match parser.parse_list() {
        Some(value) if parser.position == tokens.len() => value,
        _ => true,
    }
}

/// What a triplet's name says about it, for platform expressions.
struct TripletFacts {
    cpu: String,
    system: String,
    static_libraries: bool,
    static_crt: bool,
}

impl TripletFacts {
    fn from_name(triplet: &str) -> TripletFacts {
        const SYSTEMS: &[&str] = &[
            "windows",
            "uwp",
            "mingw",
            "xbox",
            "linux",
            "android",
            "osx",
            "ios",
            "tvos",
            "watchos",
            "visionos",
            "freebsd",
            "openbsd",
            "netbsd",
            "emscripten",
            "qnx",
        ];
        let parts: Vec<&str> = triplet.split('-').collect();
        let system = parts.iter().skip(1).find(|part| SYSTEMS.contains(part)).copied().unwrap_or_default();

        // Windows triplets are DLLs unless they say static; the others static libraries unless they say dynamic
        let windows_like = ["windows", "uwp", "mingw", "xbox"].contains(&system);
        let static_libraries = if windows_like {
            parts.contains(&"static")
        } else {
            !parts.contains(&"dynamic")
        };

        TripletFacts {
            cpu: parts[0].to_string(),
            system: system.to_string(),
            static_libraries,
            static_crt: named_static_crt(triplet).unwrap_or(false),
        }
    }

    fn identifier(&self, name: &str) -> bool {
        let cpu = self.cpu.as_str();
        let system = self.system.as_str();
        match name {
            "x64" | "x86" | "arm64" | "arm64ec" | "wasm32" | "mips64" => cpu == name,
            "arm" => cpu == "arm" || cpu == "arm64",
            "arm32" => cpu == "arm",
            "windows" => ["windows", "uwp", "mingw", "xbox"].contains(&system),
            "apple" => ["osx", "ios", "tvos", "watchos", "visionos"].contains(&system),
            "bsd" => ["freebsd", "openbsd", "netbsd"].contains(&system),
            "static" => self.static_libraries,
            "staticcrt" => self.static_crt,
            "native" => {
                let host = match std::env::consts::ARCH {
                    "x86_64" => "x64",
                    "aarch64" => "arm64",
                    other => other,
                };
                let host_system = if cfg!(windows) {
                    "windows"
                } else if cfg!(target_os = "macos") {
                    "osx"
                } else {
                    "linux"
                };
                cpu == host && (system == host_system || (cfg!(windows) && self.identifier("windows")))
            }
            other => system == other,
        }
    }
}

/// A recursive-descent reader of one platform expression; None for a syntax error.
struct PlatformParser<'a> {
    tokens: &'a [String],
    position: usize,
    facts: &'a TripletFacts,
}

impl PlatformParser<'_> {
    /// `a, b`: the loosest or.
    fn parse_list(&mut self) -> Option<bool> {
        let mut value = self.parse_or()?;
        while self.take(",") {
            value |= self.parse_or()?;
        }
        Some(value)
    }

    fn parse_or(&mut self) -> Option<bool> {
        let mut value = self.parse_and()?;
        while self.take("|") {
            value |= self.parse_and()?;
        }
        Some(value)
    }

    fn parse_and(&mut self) -> Option<bool> {
        let mut value = self.parse_unary()?;
        while self.take("&") || self.take("and") {
            value &= self.parse_unary()?;
        }
        Some(value)
    }

    fn parse_unary(&mut self) -> Option<bool> {
        if self.take("!") || self.take("not") {
            return Some(!self.parse_unary()?);
        }
        if self.take("(") {
            let value = self.parse_list()?;
            return self.take(")").then_some(value);
        }

        let word = self.tokens.get(self.position)?;
        if !word.starts_with(|first: char| first.is_ascii_alphanumeric()) {
            return None;
        }
        self.position += 1;
        Some(self.facts.identifier(word))
    }

    fn take(&mut self, token: &str) -> bool {
        let found = self.tokens.get(self.position).is_some_and(|next| next == token);
        if found {
            self.position += 1;
        }
        found
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_names_are_valid() {
        assert_eq!(manifest_name("My App_2"), "my-app-2");
        assert_eq!(manifest_name("core"), "core-project");
        assert_eq!(manifest_name("com1"), "com1-project");
        assert_eq!(manifest_name("compiler"), "compiler");
        assert_eq!(manifest_name("__"), "anvil-project");
    }

    #[test]
    fn checkout_commit_follows_branches() {
        let root = std::env::temp_dir().join(format!("anvil-vcpkg-head-{}", std::process::id()));
        let git = root.join(".git");
        std::fs::create_dir_all(git.join("refs").join("heads")).unwrap();

        // A branch only in packed-refs, then a loose one that wins
        std::fs::write(git.join("HEAD"), "ref: refs/heads/master\n").unwrap();
        std::fs::write(git.join("packed-refs"), "# pack-refs\naaa refs/heads/other\nbbb refs/heads/master\n").unwrap();
        assert_eq!(checkout_commit(&root).as_deref(), Some("bbb"));
        std::fs::write(git.join("refs").join("heads").join("master"), "ccc\n").unwrap();
        assert_eq!(checkout_commit(&root).as_deref(), Some("ccc"));

        // A detached HEAD is the commit
        std::fs::write(git.join("HEAD"), "ddd\n").unwrap();
        assert_eq!(checkout_commit(&root).as_deref(), Some("ddd"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn platform_expressions() {
        let holds = |expression: &str, triplet: &str| platform_matches(expression, triplet);

        assert!(holds("windows", "x64-windows"));
        assert!(holds("windows", "x64-mingw-dynamic") && holds("mingw", "x64-mingw-dynamic"));
        assert!(!holds("windows", "x64-linux"));
        assert!(holds("!windows", "arm64-osx") && holds("apple & arm", "arm64-osx"));
        assert!(holds("windows & static", "x64-windows-static-md") && !holds("staticcrt", "x64-windows-static-md"));
        assert!(holds("staticcrt", "x64-windows-static") && !holds("static", "x64-windows"));
        assert!(holds("static", "x64-linux") && !holds("static", "x64-linux-dynamic"));
        assert!(holds("(windows & arm64) | (linux & x64)", "x64-linux"));
        assert!(!holds("(windows & arm64) | (linux & x64)", "x64-windows"));
        assert!(holds("linux, windows", "x64-windows") && holds("not linux and x64", "x64-windows"));
        assert!(!holds("uwp | xbox", "x64-windows"));

        // A syntax error keeps the entry
        assert!(holds("windows &", "x64-linux"));
    }

    #[test]
    fn triplet_runtimes() {
        assert_eq!(named_static_crt("x64-windows"), Some(false));
        assert_eq!(named_static_crt("x64-windows-static"), Some(true));
        assert_eq!(named_static_crt("x64-windows-static-md"), Some(false));
        assert_eq!(named_static_crt("x86-windows-static-release"), Some(true));
        assert_eq!(named_static_crt("x64-linux"), None);
        assert_eq!(named_static_crt("my-triplet"), None);

        assert_eq!(
            crt_linkage_of("set(VCPKG_TARGET_ARCHITECTURE x64)\nset( VCPKG_CRT_LINKAGE Static )\n"),
            Some(true)
        );
        assert_eq!(crt_linkage_of("set(VCPKG_CRT_LINKAGE dynamic)"), Some(false));
        assert_eq!(crt_linkage_of("set(VCPKG_LIBRARY_LINKAGE static)"), None);
    }

    #[test]
    fn requested_features_follow_platforms_and_defaults() {
        let manifest = Path::new("vcpkg.json");
        let check = |json: &str, request: Request, triplet: &str, defaults: Option<Vec<String>>| {
            let json: serde_json::Value = serde_json::from_str(json).unwrap();
            check_requested_features(manifest, &json, &[request], triplet, &|_: &str| defaults.clone())
        };
        let request = |name: &str, features: &[&str], default_features: bool| Request {
            name: name.into(),
            features: features.iter().map(|feature| feature.to_string()).collect(),
            default_features,
        };

        // Two entries for one port: only the one for this platform counts
        let curl = r#"{ "dependencies": [ { "name": "curl", "features": ["openssl"], "platform": "linux" },
                                         { "name": "curl", "features": ["schannel"], "platform": "windows" } ] }"#;
        assert!(check(curl, request("curl", &["schannel"], true), "x64-windows", None).is_ok());
        assert!(check(curl, request("curl", &["openssl"], true), "x64-windows", None).is_err());
        assert!(check(curl, request("curl", &["openssl"], true), "x64-linux", None).is_ok());

        // A default feature of the port is on while the entry keeps the defaults
        let plain = r#"{ "dependencies": [ "curl" ] }"#;
        let defaults = Some(vec!["ssl".to_string()]);
        assert!(check(plain, request("curl", &["ssl"], true), "x64-windows", defaults.clone()).is_ok());
        let no_defaults = r#"{ "dependencies": [ { "name": "curl", "default-features": false } ] }"#;
        assert!(check(no_defaults, request("curl", &["ssl"], true), "x64-windows", defaults).is_err());

        // default-features = false against a vcpkg.json that keeps them is not an error
        assert!(check(
            r#"{ "dependencies": [ "fmt" ] }"#,
            request("fmt", &[], false),
            "x64-windows",
            Some(Vec::new())
        )
        .is_ok());
    }
}

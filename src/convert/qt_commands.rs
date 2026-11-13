//! Qt's CMake commands (Qt6CoreMacros, Qt5CoreMacros). Anvil runs moc, uic and rcc by itself for every target that
//! uses Qt (crate::qt), as AUTOMOC, AUTOUIC and AUTORCC do, so these mostly name targets and the files to generate
//! from: the `.ui` and `.qrc` files they list become the target's sources.

use super::interp::Interpreter;
use super::model::{self, Item, TargetKind};
use super::parse::Command;
use crate::util::slash;

// Commands with nothing to convert: they turn on what Anvil always does, or deploy
const IGNORED_COMMANDS: &[&str] = &[
    "qt_standard_project_setup",
    "qt_finalize_target",
    "qt_finalize_executable",
    "qt_policy",
    "qt_import_plugins",
    "qt_disable_unicode_defines",
    "qt_allow_non_utf8_sources",
    "qt_generate_deploy_app_script",
    "qt_generate_deploy_qml_app_script",
    "qt_generate_deploy_script",
    "qt_deploy_runtime_dependencies",
    "qt_deploy_qt_conf",
    "qt_deploy_qml_imports",
    "qt_deploy_translations",
];

// Commands with a handler below
const HANDLED_COMMANDS: &[&str] = &[
    "qt_add_executable",
    "qt_add_library",
    "qt_add_resources",
    "qt_add_big_resources",
    "qt_wrap_ui",
    "qt_wrap_cpp",
    "qt_add_qml_module",
];

/// A Qt command's versionless name: `qt6_add_resources` and `qt5_wrap_ui` as `qt_add_resources` and `qt_wrap_ui`.
fn versionless(name: &str) -> Option<String> {
    let rest = name
        .strip_prefix("qt5_")
        .or_else(|| name.strip_prefix("qt6_"))
        .or_else(|| name.strip_prefix("qt_"))?;
    Some(format!("qt_{rest}"))
}

/// Is this one of Qt's commands the converter knows?
pub(super) fn is_qt_command(name: &str) -> bool {
    versionless(name).is_some_and(|name| HANDLED_COMMANDS.contains(&name.as_str()) || IGNORED_COMMANDS.contains(&name.as_str()))
}

/// The Qt package find_package found that a target namespace stands for: `Qt6::` and `Qt5::` their own,
/// versionless `Qt::` the first found. Qt is the installed CMake package, never vcpkg's `qt` or `qt5` meta-ports.
pub(super) fn qt_package<'a>(namespace: &str, packages: &'a [String]) -> Option<&'a String> {
    let namespace = namespace.to_lowercase();
    let major = match namespace.as_str() {
        "qt" => None,
        "qt5" | "qt6" => Some(&namespace[2..]),
        _ => return None,
    };
    packages.iter().find(|package| {
        let lower = package.to_lowercase();
        match major {
            Some(major) => lower.starts_with(&format!("qt{major}")),
            None => lower.starts_with("qt5") || lower.starts_with("qt6"),
        }
    })
}

/// Does a target link Qt (`Qt6::Widgets`, versionless `Qt::Core`, `${Qt5Widgets_LIBRARIES}`)? Lowercase.
pub(super) fn is_qt_link(link: &str) -> bool {
    ["qt::", "qt5::", "qt6::"].iter().any(|namespace| link.starts_with(namespace))
}

impl Interpreter {
    /// Runs one of Qt's commands (is_qt_command).
    pub(super) fn command_qt(&mut self, command: &Command, args: &[String]) {
        let Some(name) = versionless(&command.name) else { return };
        let args = model::join_genex_fragments(args.to_vec());

        match name.as_str() {
            // add_executable/add_library, with Qt's own words left out
            "qt_add_executable" => {
                let args: Vec<String> = args.into_iter().filter(|word| word != "MANUAL_FINALIZATION").collect();
                self.command_add_executable(&args);
            }
            "qt_add_library" => self.qt_add_library(args),

            // Files to generate from: the variable lists them, and where it goes into sources Anvil runs the tool
            "qt_wrap_ui" | "qt_wrap_cpp" => self.qt_list_files(&args, &["OPTIONS", "TARGET", "DEPENDS"]),
            "qt_add_resources" | "qt_add_big_resources" => self.qt_add_resources(command, &args),
            "qt_add_qml_module" => self.qt_add_qml_module(command, &args),
            _ => {}
        }
    }

    /// qt_add_library(name [STATIC|SHARED|MODULE|INTERFACE|OBJECT] [MANUAL_FINALIZATION] sources...): without a
    /// kind, shared as Qt is (BUILD_SHARED_LIBS does not decide it).
    fn qt_add_library(&mut self, args: Vec<String>) {
        let mut args: Vec<String> = args.into_iter().filter(|word| word != "MANUAL_FINALIZATION").collect();
        let kinds = ["STATIC", "SHARED", "MODULE", "INTERFACE", "OBJECT", "ALIAS", "IMPORTED"];
        if !args.iter().any(|word| kinds.contains(&word.as_str())) && !args.is_empty() {
            args.insert(1, "SHARED".to_string());
        }
        self.command_add_library(&args);
    }

    /// qt_wrap_ui(VAR forms... [OPTIONS ...]), qt_wrap_cpp(VAR headers... [TARGET t] [OPTIONS ...]),
    /// qt_add_resources(VAR files.qrc... [OPTIONS ...]): VAR gains the files themselves.
    fn qt_list_files(&mut self, args: &[String], keywords: &[&str]) {
        let Some((variable, rest)) = args.split_first() else { return };
        let files = rest.iter().take_while(|word| !keywords.contains(&word.as_str()));

        let mut value: Vec<String> = self
            .get_variable(variable)
            .filter(|value| !value.is_empty())
            .map(|value| value.split(';').map(String::from).collect())
            .unwrap_or_default();
        value.extend(files.map(|file| slash(&self.resolve_path(file))));
        self.set_variable(variable, &value.join(";"));
    }

    /// qt_add_resources: the variable form lists `.qrc` files; Qt 6's target form (`qt_add_resources(app images
    /// PREFIX / FILES a.png)`) writes a `.qrc` of its own, which Anvil does not.
    fn qt_add_resources(&mut self, command: &Command, args: &[String]) {
        let Some(first) = args.first() else { return };
        let target = self.aliases.get(first).cloned().unwrap_or_else(|| first.clone());
        if !self.targets.contains_key(&target) {
            self.qt_list_files(args, &["OPTIONS"]);
            return;
        }

        // The target form: a note saying what to write
        let resource = args.get(1).cloned().unwrap_or_default();
        let keywords = ["PREFIX", "BASE", "LANG", "OUTPUT_TARGETS", "BIG_RESOURCES", "OPTIONS", "FILES"];
        let after = |keyword: &str| -> Vec<String> {
            args.iter()
                .skip_while(|word| *word != keyword)
                .skip(1)
                .take_while(|word| !keywords.contains(&word.as_str()))
                .cloned()
                .collect()
        };
        if let Some(variable) = after("OUTPUT_TARGETS").first() {
            self.set_variable(variable, "");
        }
        let prefix = after("PREFIX").first().cloned().unwrap_or_else(|| "/".to_string());
        let files = after("FILES");
        let shown: Vec<&str> = files.iter().take(4).map(String::as_str).collect();
        let more = if files.len() > shown.len() { ", ..." } else { "" };
        let note = format!(
            "{}({target} {resource} ...) is not converted: Anvil compiles `.qrc` files only; write {resource}.qrc listing {}{more} under prefix \"{prefix}\", and add it to sources",
            command.name,
            shown.join(", ")
        );
        self.targets.get_mut(&target).unwrap().notes.push(note);
    }

    /// qt_add_qml_module(target URI uri [SOURCES ...] [QML_FILES ...] ...): its C++ sources are the target's; its QML
    /// module (type registration, qmlcachegen, the module's resources) is not converted.
    fn qt_add_qml_module(&mut self, command: &Command, args: &[String]) {
        let Some(first) = args.first() else { return };
        let target = self.aliases.get(first).cloned().unwrap_or_else(|| first.clone());

        // A module with no target yet makes its own library
        if !self.targets.contains_key(&target) {
            let kind = match args.iter().any(|word| word == "STATIC") {
                true => "STATIC",
                false => "SHARED",
            };
            self.command_add_library(&[target.clone(), kind.to_string()]);
        }

        // Its C++ sources
        let keywords = [
            "URI",
            "VERSION",
            "SOURCES",
            "QML_FILES",
            "RESOURCES",
            "IMPORTS",
            "OPTIONAL_IMPORTS",
            "DEPENDENCIES",
            "OUTPUT_DIRECTORY",
            "RESOURCE_PREFIX",
            "PLUGIN_TARGET",
            "CLASS_NAME",
            "OUTPUT_TARGETS",
            "TYPEINFO",
            "DESIGNER_SUPPORTED",
            "NO_PLUGIN",
            "STATIC",
            "SHARED",
        ];
        let after = |keyword: &str| -> Vec<String> {
            args.iter()
                .skip_while(|word| *word != keyword)
                .skip(1)
                .take_while(|word| !keywords.contains(&word.as_str()) && !word.starts_with("NO_"))
                .cloned()
                .collect()
        };
        let sources = after("SOURCES");
        if !sources.is_empty() && self.targets[&target].kind != TargetKind::Interface {
            let mut target_args = vec![target.clone(), "PRIVATE".to_string()];
            target_args.extend(sources);
            self.command_target_sources(command, &target_args);
        }
        if let Some(variable) = after("OUTPUT_TARGETS").first() {
            self.set_variable(variable, "");
        }

        // What it does besides, for the user to redo
        let uri = after("URI").first().cloned().unwrap_or_default();
        let qml_files = after("QML_FILES");
        let shown: Vec<&str> = qml_files.iter().take(4).map(String::as_str).collect();
        let more = if qml_files.len() > shown.len() { ", ..." } else { "" };
        let note = format!(
            "{}(URI {uri}) is not converted: Anvil bundles QML in `.qrc` files only (no qmlcachegen or type registration); list {}{more} in a .qrc in sources, load it from qrc:/, and register C++ types with qmlRegisterType or context properties",
            command.name,
            shown.join(", ")
        );
        self.targets.get_mut(&target).unwrap().notes.push(note);
    }

    /// AUTOMOC, AUTOUIC or AUTORCC on a target: on is what Anvil does anyway; off becomes `qt = false`, which turns
    /// off all three.
    pub(super) fn qt_generator_property(&mut self, target: &str, property: &str, items: &[String]) {
        if model::constant_truth(&items.join(";")) != Some(false) {
            return;
        }

        let condition = self.current_condition();
        let target = self.targets.get_mut(target).unwrap();
        target.settings.push(Item {
            condition,
            public: false,
            value: ("qt", "false".to_string()),
        });
        if property != "AUTOMOC" {
            target
                .notes
                .push(format!("{property} OFF became `qt = false`, which turns off moc, uic and rcc together"));
        }
    }

    /// find_package(QT NAMES Qt6 Qt5 ...), as Qt Creator's projects start: Qt 6 (the first of the names Anvil
    /// reads), with `QT_VERSION_MAJOR` set for the find_package(Qt${QT_VERSION_MAJOR} ...) after it. The arguments
    /// to find it with, or None for any other package.
    pub(super) fn qt_find_package_names(&mut self, args: &[String]) -> Option<Vec<String>> {
        let (requested, rest) = args.split_first()?;
        let names_at = rest.iter().position(|word| word == "NAMES")?;
        let is_keyword = |word: &String| word.chars().all(|character| character.is_ascii_uppercase() || character == '_');
        let names: Vec<&String> = rest[names_at + 1..].iter().take_while(|word| !is_keyword(word)).collect();
        let chosen = names.iter().find(|name| matches!(name.as_str(), "Qt6" | "Qt5"))?.to_string();

        self.set_variable(&format!("{requested}_VERSION_MAJOR"), &chosen[2..]);
        self.set_variable(&format!("{requested}_FOUND"), "ON");

        let mut rewritten = vec![chosen];
        rewritten.extend(rest[..names_at].iter().cloned());
        rewritten.extend(rest[names_at + 1 + names.len()..].iter().cloned());
        Some(rewritten)
    }
}

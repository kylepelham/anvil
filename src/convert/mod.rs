//! Converts CMake projects to anvil.toml.

mod builtins;
mod check_script;
mod emit;
mod interp;
mod model;
mod parse;
mod target_commands;
mod test_groups;

use crate::util::slash;
use anyhow::{anyhow, bail, Result};
use std::path::{Path, PathBuf};

pub struct Conversion {
    pub toml: String,
    pub warnings: Vec<String>,
    pub target_count: usize,
    pub dependency_count: usize,
    pub todo_count: usize,
    /// Files the project needs that CMake generated at configure time (export headers).
    pub files: Vec<(PathBuf, String)>,
    /// tests.toml files, beside the CMake files that added their tests.
    pub tests_files: Vec<(PathBuf, String)>,
}

pub fn convert(folder: &Path) -> Result<Conversion> {
    if !folder.join("CMakeLists.txt").is_file() {
        bail!("no CMakeLists.txt in {}", slash(folder));
    }

    let mut interpreter = interp::Interpreter::new(folder);
    interpreter.run_root()?;
    interpreter.finish_targets();

    let mut conversion = emit::emit(interpreter)?;
    unmark_values(&mut conversion, folder);
    check_output(&conversion)?;

    Ok(conversion)
}

// Values named in one warning before "and N more"
const LISTED_VALUES: usize = 3;

/// Values holding the converter's own markers (a platform value or check result it lost track of, `SYS_${SYS}` after
/// string(TOUPPER)): a warning per file, and written as the `${NAME}` they stand for rather than as control characters.
fn unmark_values(conversion: &mut Conversion, folder: &Path) {
    let marker = regex::Regex::new(r"\\u0001(?i:option-|symbol-|sym:)?([A-Za-z0-9_]*)(?:\\u0002)?").unwrap();
    let mut files: Vec<(String, &mut String)> = vec![("anvil.toml".to_string(), &mut conversion.toml)];
    files.extend(
        conversion
            .tests_files
            .iter_mut()
            .map(|(path, text)| (crate::util::rel(path, folder), text)),
    );

    for (file, text) in files {
        let Ok(document) = toml::from_str::<toml::Value>(text) else { continue };
        let mut found = Vec::new();
        find_markers(&document, "", &mut found);
        let Some((place, _)) = found.first() else { continue };

        // Each value once
        let mut values: Vec<String> = Vec::new();
        for (_, value) in &found {
            let value = format!("`{}`", readable_markers(value));
            if !values.contains(&value) {
                values.push(value);
            }
        }
        let count = values.len();
        if count > LISTED_VALUES {
            values.truncate(LISTED_VALUES);
            values.push(format!("and {} more", count - LISTED_VALUES));
        }
        let (holds, what) = if count == 1 {
            ("holds a value", "it is")
        } else {
            ("hold values", "they are")
        };
        conversion.warnings.push(format!(
            "{file}, [{place}]: {} {holds} the converter could not work out; write what {what} on each platform",
            values.join(", ")
        ));

        *text = marker.replace_all(text, "$${$1}").replace("\\u0002", "").replace("\\u0003", " ");
    }
}

/// A value with its markers written as `${NAME}`.
fn readable_markers(text: &str) -> String {
    let marker = regex::Regex::new("\u{1}(?i:option-|symbol-|sym:)?([A-Za-z0-9_]*)\u{2}?").unwrap();
    marker.replace_all(text, "$${$1}").replace('\u{2}', "").replace('\u{3}', " ")
}

/// Every string or key in a TOML document holding a marker character, with the table it is in.
fn find_markers(value: &toml::Value, place: &str, found: &mut Vec<(String, String)>) {
    match value {
        toml::Value::String(text) if text.contains(MARKER_CHARACTERS) => found.push((place.to_string(), text.clone())),
        toml::Value::Table(table) => {
            for (key, child) in table {
                if key.contains(MARKER_CHARACTERS) {
                    found.push((place.to_string(), key.clone()));
                }
                let child_place = if place.is_empty() { key.clone() } else { format!("{place}.{key}") };
                find_markers(child, &child_place, found);
            }
        }
        toml::Value::Array(items) => items.iter().for_each(|item| find_markers(item, place, found)),
        _ => {}
    }
}

/// The written files must load: valid TOML, keys Anvil knows, and conditions over words it knows. Anything else is a
/// bug in the converter, stopped here rather than left for `anvil build` to find.
fn check_output(conversion: &Conversion) -> Result<()> {
    const BUG: &str = "a bug in `anvil convert`: please report it with the CMake project";

    // anvil.toml: the words its conditions may use
    let manifest =
        crate::manifest::parse_str(&conversion.toml).map_err(|error| anyhow!("the converted anvil.toml does not load ({BUG}):\n{error}"))?;
    let options: Vec<String> = manifest.options.keys().cloned().collect();
    let profiles: Vec<String> = manifest.profile.keys().cloned().collect();
    crate::cond::check_names(&profiles, &options).map_err(|error| anyhow!("the converted anvil.toml: {error} ({BUG})"))?;

    let mut known: Vec<&str> = crate::cond::BUILTIN_WORDS.to_vec();
    known.extend(["debug", "release"]);
    known.extend(options.iter().chain(&profiles).map(String::as_str));

    let mut files = vec![("anvil.toml".to_string(), conversion.toml.as_str())];
    for (path, text) in &conversion.tests_files {
        crate::manifest::parse_tests(text).map_err(|error| anyhow!("the converted {} does not load ({BUG}):\n{error}", slash(path)))?;
        files.push((slash(path), text.as_str()));
    }

    // Every condition, in every file; no value may hold the converter's own markers (a platform value or check result
    // it lost track of)
    for (file, text) in files {
        let document: toml::Value = toml::from_str(text)?;
        let mut found = Vec::new();
        find_markers(&document, "", &mut found);
        if let Some((place, value)) = found.first() {
            bail!(
                "the converted {file}, [{place}]: `{}` holds a platform value or check result the converter lost track of ({BUG})",
                readable_markers(value)
            );
        }
        let mut conditions = Vec::new();
        collect_conditions(&document, "", &mut conditions);

        for (place, condition) in conditions.iter().filter(|(_, condition)| !condition.contains('{')) {
            let parsed = crate::cond::parse(condition).map_err(|error| anyhow!("the converted {file}, [{place}]: {error} ({BUG})"))?;
            let mut words = Vec::new();
            condition_words(&parsed, &mut words);
            if let Some(word) = words.iter().find(|word| !known.contains(&word.as_str())) {
                bail!(
                    "the converted {file}, [{place}]: condition \"{condition}\" uses `{word}`, which is no option, profile or condition word ({BUG})"
                );
            }
        }
    }

    for (path, text) in &conversion.files {
        if text.contains(MARKER_CHARACTERS) {
            bail!("the converted {}: a value the converter could not work out ({BUG})", slash(path));
        }
    }

    Ok(())
}

// The characters the converter marks options, platform values and spaces with inside values
const MARKER_CHARACTERS: [char; 3] = ['\u{1}', '\u{2}', '\u{3}'];

/// Every condition in a TOML document with the table it is in: `if` values, and the keys of `when` tables.
fn collect_conditions(value: &toml::Value, place: &str, conditions: &mut Vec<(String, String)>) {
    // (tables of names, where `when` and `if` would be a target or dependency called that)
    const NAME_TABLES: &[&str] = &["target", "preset", "deps", "profile", "options"];

    match value {
        toml::Value::Table(table) => {
            for (key, child) in table {
                let child_place = if place.is_empty() { key.clone() } else { format!("{place}.{key}") };
                let named = NAME_TABLES.contains(&place);
                match (key.as_str(), child) {
                    ("if", toml::Value::String(condition)) if !named => conditions.push((place.to_string(), condition.clone())),
                    ("when", toml::Value::Table(blocks)) if !named => {
                        conditions.extend(blocks.keys().map(|condition| (child_place.clone(), condition.clone())));
                    }
                    _ => {}
                }
                collect_conditions(child, &child_place, conditions);
            }
        }
        toml::Value::Array(items) => {
            for item in items {
                collect_conditions(item, place, conditions);
            }
        }
        _ => {}
    }
}

fn condition_words(condition: &crate::cond::Cond, words: &mut Vec<String>) {
    use crate::cond::Cond;

    match condition {
        Cond::Atom(word) => words.push(word.clone()),
        Cond::Not(inner) => condition_words(inner, words),
        Cond::And(left, right) | Cond::Or(left, right) => {
            condition_words(left, words);
            condition_words(right, words);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn msvc_runtime_library_becomes_profile_settings() {
        let cmake = r#"
cmake_minimum_required(VERSION 3.20)
project(runtime CXX)
set(CMAKE_MSVC_RUNTIME_LIBRARY "MultiThreaded$<$<CONFIG:Debug>:Debug>")
add_executable(app main.cpp)
"#;
        let conversion = convert_files("runtime", &[("CMakeLists.txt", cmake), ("main.cpp", "int main() {}\n")]);
        let toml = &conversion.toml;

        assert!(
            toml.contains("[profile.debug]\nstatic-runtime = true\n\n[profile.release]\nstatic-runtime = true\n"),
            "{toml}"
        );
    }

    #[test]
    fn markers_never_reach_the_output() {
        let conversion = Conversion {
            toml: "[project]\nname = \"p\"\n\n[target.app]\nkind = \"exe\"\ndefines = [\"SYS_\\u0001SYM:CMAKE_SYSTEM_NAME\"]\n".into(),
            warnings: Vec::new(),
            target_count: 1,
            dependency_count: 0,
            todo_count: 0,
            files: Vec::new(),
            tests_files: Vec::new(),
        };

        let error = check_output(&conversion).unwrap_err().to_string();
        assert!(
            error.contains("[target.app.defines]") && error.contains("`SYS_${CMAKE_SYSTEM_NAME}`"),
            "{error}"
        );

        // Converting writes them as the variable they stand for, with a warning
        let mut conversion = conversion;
        unmark_values(&mut conversion, Path::new("C:/p"));
        assert!(
            conversion.toml.contains(r#"defines = ["SYS_${CMAKE_SYSTEM_NAME}"]"#),
            "{}",
            conversion.toml
        );
        assert!(
            conversion.warnings[0].starts_with("anvil.toml, [target.app.defines]: `SYS_${CMAKE_SYSTEM_NAME}` holds a value"),
            "{:?}",
            conversion.warnings
        );
        check_output(&conversion).unwrap();
    }

    /// Converts a project made of the given files, in a fresh temporary folder.
    fn convert_files(name: &str, files: &[(&str, &str)]) -> Conversion {
        let folder = std::env::temp_dir().join(format!("anvil-convert-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        for (path, text) in files {
            let file = folder.join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, text).unwrap();
        }

        let conversion = convert(&folder).unwrap();
        let _ = std::fs::remove_dir_all(&folder);
        conversion
    }

    #[test]
    fn steps_go_to_the_target_that_uses_their_outputs() {
        let cmake = r#"
cmake_minimum_required(VERSION 3.20)
project(fonts CXX)
set(ROOT "${CMAKE_CURRENT_SOURCE_DIR}/tools/..")
add_executable(cook main.cpp)
set(FONTS A)
foreach(font A B C)
  if(font IN_LIST FONTS)
    set(cooked ${ROOT}/assets/cooked/${font}.f)
    set(source ${ROOT}/assets/${font}.ttf)
  else()
    set(cooked ${ROOT}/plug/cooked/${font}.f)
    set(source ${ROOT}/plug/${font}.ttf)
  endif()
  add_custom_command(OUTPUT ${cooked} COMMAND $<TARGET_FILE:cook> ${source} ${cooked} DEPENDS cook ${source})
  list(APPEND COOKED ${cooked})
endforeach()
add_custom_target(fonts ALL DEPENDS ${COOKED})
"#;
        let conversion = convert_files("steps", &[("CMakeLists.txt", cmake), ("main.cpp", "int main() {}\n")]);
        let toml = &conversion.toml;

        // The custom target owns the steps, not the program they run; IN_LIST picks A's folder
        assert!(toml.contains("[[target.fonts.run]]"), "{toml}");
        assert!(!toml.contains("[[target.cook.run]]"), "{toml}");
        assert!(toml.contains(r#"for-each = ["plug/B.ttf", "plug/C.ttf"]"#), "{toml}");
        assert!(toml.contains(r#"output = "assets/cooked/A.f""#), "{toml}");
        assert!(!toml.contains(".."), "{toml}");
    }

    #[test]
    fn find_path_becomes_the_package_with_the_header() {
        let cmake = r#"
cmake_minimum_required(VERSION 3.20)
project(header CXX)
find_path(CGLTF_INCLUDE_DIRS "cgltf.h" REQUIRED)
add_executable(app main.cpp)
target_include_directories(app PRIVATE ${CGLTF_INCLUDE_DIRS})
"#;
        let manifest = r#"{ "name": "header", "dependencies": ["cgltf"] }"#;
        let files = [("CMakeLists.txt", cmake), ("main.cpp", "int main() {}\n"), ("vcpkg.json", manifest)];
        let toml = convert_files("findpath", &files).toml;

        assert!(toml.contains(r#"cgltf = "vcpkg""#), "{toml}");
        assert!(toml.contains(r#"deps = ["cgltf"]"#), "{toml}");
        assert!(!toml.contains("NOTFOUND") && !toml.contains("found-header"), "{toml}");
    }

    #[test]
    fn configure_time_values_are_not_frozen() {
        let cmake = r#"
cmake_minimum_required(VERSION 3.20)
project(hashes CXX)
function(source_hash target define)
  set(combined "${CMAKE_CXX_COMPILER_ID} ${CMAKE_CXX_COMPILER_VERSION}")
  foreach(f ${ARGN})
    file(SHA256 ${f} h)
    string(APPEND combined " ${h}")
  endforeach()
  string(SHA256 all "${combined}")
  string(SUBSTRING ${all} 0 16 short)
  target_compile_definitions(${target} PRIVATE ${define}=0x${short}ull)
endfunction()
add_executable(app main.cpp)
file(GLOB headers ${CMAKE_CURRENT_SOURCE_DIR}/inc/*.h)
source_hash(app APP_HASH ${headers} ${CMAKE_CURRENT_SOURCE_DIR}/cmake/../main.cpp ${CMAKE_CURRENT_SOURCE_DIR}/CMakeLists.txt)
string(TIMESTAMP stamp "%Y")
target_compile_definitions(app PRIVATE BUILD_YEAR=${stamp})
target_compile_options(app PRIVATE -DDOCS_DIR=${CMAKE_CURRENT_SOURCE_DIR}/cmake/../docs)
"#;
        let files = [
            ("CMakeLists.txt", cmake),
            ("main.cpp", "int main() {}\n"),
            ("inc/a.h", "\n"),
            ("inc/b.h", "\n"),
        ];
        let toml = convert_files("hashes", &files).toml;

        // The compiler, the glob (not the two files it found), and the target's flags for the CMake file
        assert!(
            toml.contains(r#""APP_HASH=0x{hash:compiler,{root}/inc/*.h,{root}/main.cpp,flags}ull""#),
            "{toml}"
        );
        assert!(!toml.contains("BUILD_YEAR="), "{toml}");
        assert!(toml.contains("# TODO: `BUILD_YEAR` is left out"), "{toml}");
        assert!(toml.contains("DOCS_DIR={root}/docs"), "{toml}");
    }

    #[test]
    fn interface_definitions_are_not_the_targets_own() {
        let cmake = r#"
cmake_minimum_required(VERSION 3.20)
project(module CXX)
add_library(game SHARED game.cpp)
target_compile_definitions(game PRIVATE "GAME_API=__declspec(dllexport)" INTERFACE "GAME_API=__declspec(dllimport)" PUBLIC GAME_SHARED)
"#;
        let toml = convert_files(
            "interface",
            &[
                ("CMakeLists.txt", cmake),
                (
                    "game.cpp", "int x;
",
                ),
            ],
        )
        .toml;

        assert!(
            toml.contains("[target.game]\nkind = \"shared\"\ndefines = [\"GAME_API=__declspec(dllexport)\"]"),
            "{toml}"
        );
        assert!(toml.contains("[target.game.public]\ndefines = [\"GAME_SHARED\"]"), "{toml}");
        assert!(
            toml.contains("[target.game.interface]\ndefines = [\"GAME_API=__declspec(dllimport)\"]"),
            "{toml}"
        );
    }

    #[test]
    fn options_in_generator_expressions_stay_conditions() {
        let cmake = r#"
cmake_minimum_required(VERSION 3.20)
project(genex CXX)
option(WITH_VIEW "Use the view" OFF)
option(WITH_LOG "Log" OFF)
add_executable(app main.cpp)
target_link_libraries(app PRIVATE $<$<AND:$<PLATFORM_ID:Windows>,$<BOOL:${WITH_VIEW}>>:shlwapi>)
target_compile_definitions(app PRIVATE $<$<NOT:${WITH_LOG}>:NO_LOG>)
"#;
        let toml = convert_files("genex", &[("CMakeLists.txt", cmake), ("main.cpp", "int main() {}\n")]).toml;

        assert!(toml.contains(r#"[target.app.when."windows & WITH_VIEW"]"#), "{toml}");
        assert!(toml.contains(r#"libs = ["shlwapi"]"#), "{toml}");
        assert!(toml.contains(r#"[target.app.when."!WITH_LOG"]"#), "{toml}");
        assert!(!toml.contains("option-"), "{toml}");
    }

    #[test]
    fn a_projects_shape_stays_short() {
        let root = r#"cmake_minimum_required(VERSION 3.20)
project(shape CXX)
enable_testing()
function(path_args var path)
  set(args)
  if(path STREQUAL "cpu")
    set(args --cpu)
  endif()
  set(${var} ${args} PARENT_SCOPE)
endfunction()
function(add_plugin name)
  add_library(${name} SHARED plugins/${name}.cpp)
  target_compile_definitions(${name} PRIVATE PLUGIN_API=1 NOMINMAX)
  target_include_directories(${name} PRIVATE ${CMAKE_SOURCE_DIR}/sdk)
  target_link_libraries(${name} PRIVATE core)
endfunction()
add_library(core STATIC core/core.cpp)
add_library(render STATIC render/render.cpp)
add_executable(app app/main.cpp)
target_link_libraries(app PRIVATE core render)
target_link_options(app PRIVATE "LINKER:--whole-archive" $<TARGET_FILE:core> $<TARGET_FILE:render> "LINKER:--no-whole-archive")
add_plugin(alpha)
add_plugin(beta)
add_plugin(gamma)
add_subdirectory(tests)
"#;
        let tests = r#"set(args --stale)
foreach(path IN ITEMS rt cpu gpu)
  path_args(path_args ${path})
  foreach(view IN ITEMS street arch hall)
    add_test(NAME stills_${path}_${view} COMMAND app --out ${CMAKE_BINARY_DIR}/s/stills_${path}_${view} --view ${view} -- ${path_args})
  endforeach()
endforeach()
"#;
        let files = [
            ("CMakeLists.txt", root),
            ("tests/CMakeLists.txt", tests),
            ("core/core.cpp", "\n"),
            ("render/render.cpp", "\n"),
            ("app/main.cpp", "int main() {}\n"),
            ("plugins/alpha.cpp", "\n"),
            ("plugins/beta.cpp", "\n"),
            ("plugins/gamma.cpp", "\n"),
        ];
        let conversion = convert_files("shape", &files);
        let toml = &conversion.toml;

        // Libraries linked whole, and the helper's targets sharing a preset
        assert!(toml.contains(r#"whole-archive = ["core", "render"]"#), "{toml}");
        assert!(!toml.contains("--whole-archive"), "{toml}");
        assert!(toml.contains("[preset.add-plugin]"), "{toml}");
        assert!(toml.contains(r#"preset = "add-plugin""#), "{toml}");

        // The tests in tests/tests.toml: one [[test]], the loops as axes; `set(args)` in the function hides the `args`
        // outside it, as in CMake
        let [(path, tests)] = conversion.tests_files.as_slice() else {
            panic!("{:?}", conversion.tests_files)
        };
        assert!(path.ends_with("tests/tests.toml"));
        assert_eq!(tests.matches("[[test]]").count(), 1, "{tests}");
        assert!(tests.contains(r#"name = "stills_{path}_{view}""#), "{tests}");
        assert!(tests.contains(r#"matrix.view = ["street", "arch", "hall"]"#), "{tests}");
        assert!(tests.contains(r#"{ path = "cpu", args = ["--cpu"] }"#), "{tests}");
        assert!(!tests.contains("--stale"), "{tests}");
    }

    #[test]
    fn what_is_written_loads() {
        let cmake = r#"cmake_minimum_required(VERSION 3.20)
project(loads CXX)
option(OPT_A "a" OFF)
option(WITH_FMT "fmt" OFF)
option(INSTALL_IT "install" ON)
add_executable(app app/main.cpp)
if(CMAKE_SIZEOF_VOID_P EQUAL 4)
  target_compile_definitions(app PRIVATE BITS32)
endif()
if(OPT_A)
  target_compile_definitions(app PRIVATE HAS_A)
endif()
set(OPT_A ON)
if(WITH_FMT)
  find_package(fmt REQUIRED)
  target_link_libraries(app PRIVATE fmt::fmt)
endif()
set_target_properties(app PROPERTIES MSVC_RUNTIME_LIBRARY "MultiThreaded$<$<BOOL:${OPT_A}>:DLL>
")
target_link_options(app PRIVATE /SUBSYSTEM:WINDOWS /ENTRY:mainCRTStartup)
if(INSTALL_IT)
  file(WRITE ${CMAKE_BINARY_DIR}/config.cmake "include(x)\n")
endif()
add_library(one STATIC lib/a.cpp lib/b.cpp)
add_library(two SHARED lib/a.cpp lib/b.cpp)
add_custom_command(OUTPUT ${CMAKE_BINARY_DIR}/gen.cpp COMMAND cmake -E echo "int x = 1" > ${CMAKE_BINARY_DIR}/gen.cpp)
add_custom_command(OUTPUT ${CMAKE_BINARY_DIR}/list.txt COMMAND tool --list | sort > ${CMAKE_BINARY_DIR}/list.txt)
add_custom_target(lists ALL DEPENDS ${CMAKE_BINARY_DIR}/list.txt)
enable_testing()
add_test(NAME t COMMAND app)
set_tests_properties(t PROPERTIES ENVIRONMENT "A=1;B=3;A=2")
"#;
        let files = [
            ("CMakeLists.txt", cmake),
            ("app/main.cpp", "int main() {}\n"),
            ("lib/a.cpp", "\n"),
            ("lib/b.cpp", "\n"),
        ];
        let conversion = convert_files("loads", &files);
        let toml = &conversion.toml;

        // Conditions over known words only; an option set() later still declared where a condition uses it
        assert!(toml.contains(r#"[target.app.when."x86 | arm"]"#), "{toml}");
        assert!(toml.contains("OPT_A = {"), "{toml}");
        assert!(toml.contains("INSTALL_IT = {"), "{toml}");
        assert!(!toml.contains('\u{1}'), "{toml}");

        // The dependency only where it is used; main() as a windows program's entry
        assert!(toml.contains(r#"fmt = { vcpkg = "fmt", if = "WITH_FMT" }"#), "{toml}");
        assert!(toml.contains(r#"gui = "main""#), "{toml}");

        // Two targets in one folder list their sources; echo > file is a [[write]], a pipe runs through the shell
        assert_eq!(toml.matches(r#"sources = ["lib/a.cpp", "lib/b.cpp"]"#).count(), 2, "{toml}");
        assert!(toml.contains(r#"text = "int x = 1\n""#), "{toml}");
        assert!(toml.contains("shell = true"), "{toml}");

        // The last value of a variable set twice
        assert!(
            toml.contains(r#"env = { A = "2", B = "3" }"#) || toml.contains(r#"env = { B = "3", A = "2" }"#),
            "{toml}"
        );
    }

    #[test]
    fn checks_and_dependent_options_become_conditions() {
        let cmake = r#"
cmake_minimum_required(VERSION 3.20)
project(checks C)
include(CheckCCompilerFlag)
include(CheckIncludeFile)
include(CMakeDependentOption)
check_c_compiler_flag(-Wstrict-prototypes HAVE_STRICT)
check_include_file(unistd.h HAVE_UNISTD_H)
check_include_file(stdio.h HAVE_STDIO_H)
cmake_dependent_option(USE_WAYLAND "Wayland" ON "UNIX;NOT APPLE" OFF)
cmake_dependent_option(USE_NEVER "Never" ON "ANDROID" OFF)
add_executable(app main.c)
if(HAVE_STRICT)
  target_compile_options(app PRIVATE -Wstrict-prototypes)
endif()
target_compile_definitions(app PRIVATE $<$<BOOL:${HAVE_UNISTD_H}>:HAS_UNISTD> $<$<BOOL:${HAVE_STDIO_H}>:HAS_STDIO>)
if(USE_WAYLAND)
  target_compile_definitions(app PRIVATE WAYLAND)
endif()
if(USE_NEVER)
  target_compile_definitions(app PRIVATE NEVER)
endif()
"#;
        let toml = convert_files("checks", &[("CMakeLists.txt", cmake), ("main.c", "int main(void) { return 0; }\n")]).toml;

        // unistd.h: MinGW has it too, so not under MSVC only
        let not_msvc = "[target.app.when.\"!msvc\"]\ndefines = [\"HAS_UNISTD\"]\nflags = [\"-Wstrict-prototypes\"]";
        assert!(toml.contains(not_msvc), "{toml}");
        assert!(toml.contains(r#"defines = ["HAS_STDIO"]"#), "{toml}");
        assert!(toml.contains(r#"[target.app.when."USE_WAYLAND & unix & !macos"]"#), "{toml}");
        assert!(!toml.contains("NEVER"), "{toml}");
    }

    #[test]
    fn configure_file_becomes_a_write() {
        let cmake = r#"
cmake_minimum_required(VERSION 3.20)
project(conf VERSION 1.2 LANGUAGES C)
include(CheckIncludeFile)
option(WITH_FOO "Foo" OFF)
set(NAME conf)
check_include_file(unistd.h HAVE_UNISTD_H)
configure_file(config.h.in ${CMAKE_BINARY_DIR}/gen/config.h)
configure_file(conf.pc.in conf.pc @ONLY)
add_executable(app main.c)
target_include_directories(app PRIVATE ${CMAKE_BINARY_DIR}/gen)
"#;
        let template = "#define NAME \"@NAME@ ${PROJECT_VERSION}\"\n#cmakedefine HAVE_UNISTD_H 1\n#cmakedefine01 WITH_FOO\n";
        let files = [
            ("CMakeLists.txt", cmake),
            ("config.h.in", template),
            ("conf.pc.in", "prefix=@CMAKE_INSTALL_PREFIX@\n"),
            ("main.c", "int main(void) { return 0; }\n"),
        ];
        let conversion = convert_files("configure", &files);
        let toml = &conversion.toml;

        // Platform results as #if, options as one write each way; the install-only .pc says nothing
        assert!(toml.contains(r#"path = "{out}/gen/config.h""#), "{toml}");
        assert!(toml.contains("#define NAME \"conf 1.2\""), "{toml}");
        assert!(
            toml.contains("#if (defined(_MSC_VER) || (defined(RC_INVOKED) && !defined(__GNUC__)))\n/* #undef HAVE_UNISTD_H */\n#else\n#define HAVE_UNISTD_H 1\n#endif"),
            "{toml}"
        );
        assert!(toml.contains("if = \"WITH_FOO\"") && toml.contains("#define WITH_FOO 1"), "{toml}");
        assert!(toml.contains("if = \"!WITH_FOO\"") && toml.contains("#define WITH_FOO 0"), "{toml}");
        assert!(toml.contains("WITH_FOO = { default = false"), "{toml}");
        assert!(!toml.contains("conf.pc"), "{toml}");
        assert!(
            !conversion.warnings.iter().any(|warning| warning.contains("configure_file")),
            "{:?}",
            conversion.warnings
        );
    }

    #[test]
    fn packages_and_programs_that_are_not_there() {
        let cmake = r#"
cmake_minimum_required(VERSION 3.20)
project(pkgs C)
find_package(ZLIB)
find_package(HarfBuzz)
find_program(NOPE_EXE nope-not-here-xyz)
add_executable(app main.c)
if(ZLIB_FOUND)
  target_link_libraries(app PRIVATE ${ZLIB_LIBRARIES})
  target_include_directories(app PRIVATE ${ZLIB_INCLUDE_DIRS})
endif()
if(HarfBuzz_FOUND)
  target_compile_definitions(app PRIVATE HAS_HARFBUZZ)
endif()
if(NOPE_EXE)
  target_compile_definitions(app PRIVATE HAS_NOPE)
endif()
add_custom_command(OUTPUT ${CMAKE_BINARY_DIR}/doc.txt COMMAND ${NOPE_EXE} -o doc.txt)
add_custom_target(docs ALL DEPENDS ${CMAKE_BINARY_DIR}/doc.txt)
"#;
        let manifest = r#"{ "name": "pkgs", "dependencies": ["zlib"] }"#;
        let files = [
            ("CMakeLists.txt", cmake),
            ("main.c", "int main(void) { return 0; }\n"),
            ("vcpkg.json", manifest),
        ];
        let conversion = convert_files("packages", &files);
        let toml = &conversion.toml;

        assert!(toml.contains(r#"deps = ["zlib"]"#), "{toml}");
        assert!(!toml.contains("header-deps"), "{toml}");
        assert!(
            !toml.contains("HARFBUZZ") && !toml.contains("HAS_NOPE") && !toml.contains("NOTFOUND"),
            "{toml}"
        );
        assert!(
            conversion
                .warnings
                .iter()
                .any(|warning| warning.contains("which was not found when converting")),
            "{:?}",
            conversion.warnings
        );
    }

    #[test]
    fn arguments_cache_and_global_state() {
        let root = r#"
cmake_minimum_required(VERSION 3.25)
project(state C)
function(helper)
  cmake_parse_arguments(P "" "NAME" "SOURCES" "${ARGN}")
  add_executable(${P_NAME} ${P_SOURCES})
endfunction()
helper(NAME app SOURCES main.c)
set(PLUGINS "" CACHE INTERNAL "")
add_subdirectory(plugins)
set_property(GLOBAL APPEND PROPERTY KINDS a)
set_property(GLOBAL APPEND PROPERTY KINDS b)
get_property(kinds GLOBAL PROPERTY KINDS)
block(PROPAGATE FROM_BLOCK)
  set(FROM_BLOCK yes)
  set(INSIDE_ONLY yes)
endblock()
target_compile_definitions(app PRIVATE "PLUGINS=${PLUGINS}" "KINDS=${kinds}" "FROM_BLOCK=${FROM_BLOCK}" "INSIDE=${INSIDE_ONLY}")
"#;
        let plugins = "set(PLUGINS ${PLUGINS} pa CACHE INTERNAL \"\")\nset(PLUGINS ${PLUGINS} pb CACHE INTERNAL \"\")\n";
        let files = [
            ("CMakeLists.txt", root),
            ("plugins/CMakeLists.txt", plugins),
            ("main.c", "int main(void) { return 0; }\n"),
        ];
        let toml = convert_files("state", &files).toml;

        assert!(toml.contains("[target.app]"), "{toml}");
        assert!(toml.contains(r#""PLUGINS=pa", "pb""#), "{toml}");
        assert!(toml.contains(r#""KINDS=a", "b""#), "{toml}");
        assert!(toml.contains(r#""FROM_BLOCK=yes", "INSIDE=""#), "{toml}");
    }
}

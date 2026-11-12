//! Converts CMake projects to anvil.toml.

mod builtins;
mod check_script;
mod emit;
mod interp;
mod model;
mod parse;
mod target_commands;
mod test_groups;

use anyhow::{bail, Result};
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
        bail!("no CMakeLists.txt in {}", crate::util::slash(folder));
    }

    let mut interpreter = interp::Interpreter::new(folder);
    interpreter.run_root()?;

    emit::emit(interpreter)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}

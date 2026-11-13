//! Export macro headers: what `export-header` writes, and what `anvil convert` writes for generate_export_header
//! when the header carries the project's own content.

/// The macro base: `LIB` for `LIB_EXPORT`, else the macro itself. `<BASE>_SHARED` and `<BASE>_BUILDING` switch it.
pub fn base(macro_name: &str) -> &str {
    macro_name.strip_suffix("_EXPORT").unwrap_or(macro_name)
}

/// The header: dllexport while building the shared library, dllimport when using it, nothing when static.
pub fn text(macro_name: &str, custom_content: &str) -> String {
    let base = base(macro_name);

    format!(
        r#"// Written by Anvil: the export macros of a library that builds static or shared.
#pragma once

#if defined({base}_SHARED) && defined(_WIN32)
#  ifdef {base}_BUILDING
#    define {macro_name} __declspec(dllexport)
#  else
#    define {macro_name} __declspec(dllimport)
#  endif
#elif defined({base}_SHARED)
#  define {macro_name} __attribute__((visibility("default")))
#else
#  define {macro_name}
#endif

#if defined(_MSC_VER)
#  define {base}_DEPRECATED __declspec(deprecated)
#else
#  define {base}_DEPRECATED __attribute__((__deprecated__))
#endif

#define {base}_NO_EXPORT
#define {base}_DEPRECATED_EXPORT {macro_name} {base}_DEPRECATED
#define {base}_DEPRECATED_NO_EXPORT {base}_NO_EXPORT {base}_DEPRECATED
{custom_content}"#
    )
}

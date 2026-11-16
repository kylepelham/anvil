//! Targets of CMake's own find modules (FindZLIB's ZLIB::ZLIB, FindOpenSSL's OpenSSL::SSL) and of Qt's FindWrap*
//! modules, which installed packages link but no package file defines: their headers and libraries looked for as the
//! modules look for them, in the packages' prefixes, then the system's. And what a library archive says of itself:
//! the DLL an import library loads, and the compiler its C++ code was built with.

use crate::cmake_package::Session;
use crate::platform::Os;
use crate::util::slash;
use crate::vcpkg_tree::{ImportedFiles, ImportedTarget};
use std::path::{Path, PathBuf};

/// A find module's target: the header and the libraries the module looks for (release names, then debug ones), the
/// targets it links, the system libraries a static build of it needs on Windows, and whether packages link it only
/// where it exists (`$<TARGET_NAME_IF_EXISTS:...>`, Qt's optional dependencies).
pub(crate) struct FindModule {
    pub(crate) target: &'static str,
    header: Option<&'static str>,
    names: &'static [&'static str],
    debug_names: &'static [&'static str],
    links: &'static [&'static str],
    windows_static_libraries: &'static [&'static str],
    pub(crate) optional: bool,
}

// The find modules' targets Anvil looks for itself (Threads, OpenMP, OpenGL and Vulkan are crate::vcpkg_tree's)
const FIND_MODULES: &[FindModule] = &[
    FindModule {
        target: "ZLIB::ZLIB",
        header: Some("zlib.h"),
        names: &["z", "zlib", "zdll", "zlib1", "zlibstatic"],
        debug_names: &["zlibd", "zlibd1", "zd", "zdlld", "zlibstaticd"],
        links: &[],
        windows_static_libraries: &[],
        optional: false,
    },
    FindModule {
        target: "PNG::PNG",
        header: Some("png.h"),
        names: &["png", "libpng", "png16", "libpng16", "png16_static", "libpng16_static"],
        debug_names: &["pngd", "libpngd", "png16d", "libpng16d", "png16_staticd", "libpng16_staticd"],
        links: &["ZLIB::ZLIB"],
        windows_static_libraries: &[],
        optional: false,
    },
    FindModule {
        target: "JPEG::JPEG",
        header: Some("jpeglib.h"),
        names: &["jpeg", "libjpeg", "jpeg-static"],
        debug_names: &["jpegd", "libjpegd", "jpeg-staticd"],
        links: &[],
        windows_static_libraries: &[],
        optional: false,
    },
    FindModule {
        target: "BZip2::BZip2",
        header: Some("bzlib.h"),
        names: &["bz2", "bzip2", "libbz2"],
        debug_names: &["bz2d", "bzip2d", "libbz2d"],
        links: &[],
        windows_static_libraries: &[],
        optional: false,
    },
    FindModule {
        target: "LibLZMA::LibLZMA",
        header: Some("lzma.h"),
        names: &["lzma", "liblzma"],
        debug_names: &["lzmad", "liblzmad"],
        links: &[],
        windows_static_libraries: &[],
        optional: false,
    },
    FindModule {
        target: "OpenSSL::Crypto",
        header: Some("openssl/crypto.h"),
        names: &["crypto", "libcrypto", "libeay32"],
        debug_names: &["cryptod", "libcryptod", "libeay32d"],
        links: &[],
        windows_static_libraries: &["ws2_32", "crypt32", "advapi32", "user32"],
        optional: false,
    },
    FindModule {
        target: "OpenSSL::SSL",
        header: Some("openssl/ssl.h"),
        names: &["ssl", "libssl", "ssleay32"],
        debug_names: &["ssld", "libssld", "ssleay32d"],
        links: &["OpenSSL::Crypto"],
        windows_static_libraries: &[],
        optional: false,
    },
    FindModule {
        target: "CURL::libcurl",
        header: Some("curl/curl.h"),
        names: &["curl", "libcurl", "libcurl_imp", "curllib"],
        debug_names: &["curl-d", "libcurl-d", "libcurl_imp-d", "curld", "libcurld"],
        links: &[],
        windows_static_libraries: &["ws2_32", "crypt32", "bcrypt", "advapi32", "wldap32", "normaliz"],
        optional: false,
    },
    FindModule {
        target: "XKB::XKB",
        header: Some("xkbcommon/xkbcommon.h"),
        names: &["xkbcommon"],
        debug_names: &[],
        links: &[],
        windows_static_libraries: &[],
        optional: false,
    },
    FindModule {
        target: "Vulkan::Headers",
        header: Some("vulkan/vulkan.h"),
        names: &[],
        debug_names: &[],
        links: &[],
        windows_static_libraries: &[],
        optional: false,
    },
    // Qt's: wrappers of the modules above, and checks that need no library where Anvil builds
    FindModule {
        target: "WrapVulkanHeaders::WrapVulkanHeaders",
        header: Some("vulkan/vulkan.h"),
        names: &[],
        debug_names: &[],
        links: &[],
        windows_static_libraries: &[],
        optional: true,
    },
    FindModule {
        target: "WrapOpenGL::WrapOpenGL",
        header: None,
        names: &[],
        debug_names: &[],
        links: &["OpenGL::GL"],
        windows_static_libraries: &[],
        optional: false,
    },
    FindModule {
        target: "WrapZLIB::WrapZLIB",
        header: None,
        names: &[],
        debug_names: &[],
        links: &["ZLIB::ZLIB"],
        windows_static_libraries: &[],
        optional: false,
    },
    FindModule {
        target: "WrapPNG::WrapPNG",
        header: None,
        names: &[],
        debug_names: &[],
        links: &["PNG::PNG"],
        windows_static_libraries: &[],
        optional: false,
    },
    FindModule {
        target: "WrapJpeg::WrapJpeg",
        header: None,
        names: &[],
        debug_names: &[],
        links: &["JPEG::JPEG"],
        windows_static_libraries: &[],
        optional: false,
    },
    FindModule {
        target: "WrapOpenSSL::WrapOpenSSL",
        header: None,
        names: &[],
        debug_names: &[],
        links: &["OpenSSL::SSL"],
        windows_static_libraries: &[],
        optional: false,
    },
    FindModule {
        target: "WrapOpenSSLHeaders::WrapOpenSSLHeaders",
        header: Some("openssl/ssl.h"),
        names: &[],
        debug_names: &[],
        links: &[],
        windows_static_libraries: &[],
        optional: false,
    },
    FindModule {
        target: "WrapAtomic::WrapAtomic",
        header: None,
        names: &[],
        debug_names: &[],
        links: &[],
        windows_static_libraries: &[],
        optional: false,
    },
    FindModule {
        target: "WrapRt::WrapRt",
        header: None,
        names: &[],
        debug_names: &[],
        links: &[],
        windows_static_libraries: &[],
        optional: false,
    },
];

/// Packages CMake finds only through its own Find module (no config file), with their pkg-config module and vcpkg
/// port: what a `cmake` dependency on them should be instead.
const MODULE_ONLY_PACKAGES: &[(&str, Option<&str>, &str)] = &[
    ("OpenSSL", Some("openssl"), "openssl"),
    ("PNG", Some("libpng"), "libpng"),
    ("ZLIB", Some("zlib"), "zlib"),
    ("Protobuf", Some("protobuf"), "protobuf"),
    ("JPEG", Some("libjpeg"), "libjpeg-turbo"),
    ("BZip2", Some("bzip2"), "bzip2"),
    ("CURL", Some("libcurl"), "curl"),
    ("LibLZMA", Some("liblzma"), "liblzma"),
    ("TIFF", Some("libtiff-4"), "tiff"),
    ("Freetype", Some("freetype2"), "freetype"),
    ("EXPAT", Some("expat"), "expat"),
    ("LibXml2", Some("libxml-2.0"), "libxml2"),
    ("SQLite3", Some("sqlite3"), "sqlite3"),
    ("GIF", None, "giflib"),
    ("X11", Some("x11"), "libx11"),
];

/// The compiler a library's C++ code was built for: MSVC, or GCC and Clang off MSVC's ABI (MinGW, Unix).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum CxxAbi {
    Msvc,
    Gnu,
}

/// Where a find module's files are looked for, and for which build.
pub(crate) struct Search<'a> {
    pub(crate) session: &'a Session<'a>,
    /// Install prefixes, in order (the linking package's first).
    pub(crate) prefixes: Vec<PathBuf>,
    pub(crate) os: Os,
    pub(crate) msvc_abi: bool,
    pub(crate) debug: bool,
    /// Debian's multiarch folder (x86_64-linux-gnu), on Linux.
    pub(crate) architecture: Option<&'static str>,
    pub(crate) pointer_size: u32,
}

/// The find module that defines target `name`, when Anvil knows it.
pub(crate) fn find_module(name: &str) -> Option<&'static FindModule> {
    FIND_MODULES.iter().find(|module| module.target == name)
}

impl FindModule {
    /// What the module looks for, for messages: "zlib.h and a library z, zlib or zdll".
    pub(crate) fn wanted(&self) -> String {
        let library = match self.names {
            [] => None,
            [only] => Some(format!("a library {only}")),
            [first @ .., last] => Some(format!("a library {} or {last}", first.join(", "))),
        };

        match (self.header, library) {
            (Some(header), Some(library)) => format!("{header} and {library}"),
            (Some(header), None) => header.to_string(),
            (None, Some(library)) => library,
            (None, None) => "files".to_string(),
        }
    }
}

/// The pkg-config module and vcpkg port of a package CMake finds only with its own Find module (any case).
pub(crate) fn module_only_package(name: &str) -> Option<(&'static str, Option<&'static str>, &'static str)> {
    MODULE_ONLY_PACKAGES
        .iter()
        .copied()
        .find(|(package, _, _)| package.eq_ignore_ascii_case(name))
}

/// A find module's target made from the files found for it, and a note when a DLL it loads was found nowhere. None
/// when its header or library is not there.
pub(crate) fn module_target(module: &FindModule, search: &Search) -> Option<(ImportedTarget, Option<String>)> {
    let mut target = ImportedTarget::default();
    target.name = module.target.to_string();
    target.kind = "INTERFACE".to_string();
    target.links = module.links.iter().map(|link| link.to_string()).collect();
    target.link_items = target.links.clone();

    // Its header's folder (Vulkan's in the SDK VULKAN_SDK names first)
    if let Some(header) = module.header {
        let mut prefixes = search.prefixes.clone();
        if header.starts_with("vulkan/") {
            if let Some(sdk) = search.session.variable("VULKAN_SDK").filter(|sdk| !sdk.is_empty()) {
                prefixes.insert(0, PathBuf::from(sdk));
            }
        }
        let folder = prefixes
            .iter()
            .map(|prefix| prefix.join("include"))
            .find(|folder| search.session.is_file(&folder.join(header)));
        match folder {
            Some(folder) => target.include_values.push(slash(&folder)),
            None if module.names.is_empty() => return None,
            None => {}
        }
    }
    if module.names.is_empty() {
        return Some((target, None));
    }

    // Its library: an import library's DLL is copied beside the programs; a static library brings the system
    // libraries it needs
    let library = find_library(module, search)?;
    let mut files = ImportedFiles {
        languages: Some(vec!["C".to_string()]),
        ..Default::default()
    };
    let mut note = None;
    let name = library.file_name().unwrap_or_default().to_string_lossy().to_ascii_lowercase();
    let import = match search.os {
        Os::Windows if (name.ends_with(".lib") || name.ends_with(".dll.a")) && search.session.watch(&library) => import_library_dll(&library),
        _ => None,
    };
    target.has_library = true;
    if let Some(dll) = import {
        target.kind = "SHARED".to_string();
        files.import_library = Some(library.clone());
        files.location = dll_beside(&library, &dll, search.session);
        // Elsewhere on PATH, it loads: the result then depends on PATH
        let on_path = || search.session.variable("PATH").is_some() && crate::util::which(&dll).is_some();
        if files.location.is_none() && !on_path() {
            note = Some(format!(
                "{} loads {dll}, which is neither in the package's bin folder nor on PATH: programs will not start without it",
                slash(&library)
            ));
        }
    } else if name.ends_with(".so") || name.ends_with(".dylib") {
        target.kind = "SHARED".to_string();
        files.location = Some(library);
    } else {
        target.kind = "STATIC".to_string();
        files.location = Some(library);
        if search.os == Os::Windows {
            target
                .link_items
                .extend(module.windows_static_libraries.iter().map(|library| library.to_string()));
        }
    }
    target.files.push(files);

    Some((target, note))
}

/// The first library file of the module's names in the prefixes' library folders: for a build linking debug libraries,
/// vcpkg's debug/lib first and the debug names before the others.
fn find_library(module: &FindModule, search: &Search) -> Option<PathBuf> {
    let names: Vec<&str> = match search.debug {
        true => module.debug_names.iter().chain(module.names).copied().collect(),
        false => module.names.to_vec(),
    };

    for prefix in &search.prefixes {
        let mut folders = Vec::new();
        if search.debug {
            folders.push(prefix.join("debug/lib"));
        }
        if let Some(architecture) = search.architecture {
            folders.push(prefix.join("lib").join(architecture));
        }
        if search.pointer_size == 8 && search.os != Os::Windows {
            folders.push(prefix.join("lib64"));
        }
        folders.push(prefix.join("lib"));

        for folder in &folders {
            for name in &names {
                if let Some(file) = library_files(name, search.os, search.msvc_abi)
                    .into_iter()
                    .map(|file| folder.join(file))
                    .find(|file| search.session.is_file(file))
                {
                    return Some(file);
                }
            }
        }
    }

    None
}

/// The files a library of this name may be, in the order the linker would take them.
fn library_files(name: &str, os: Os, msvc_abi: bool) -> Vec<String> {
    match os {
        Os::Windows if msvc_abi => vec![format!("{name}.lib")],
        Os::Windows => vec![format!("lib{name}.dll.a"), format!("lib{name}.a"), format!("{name}.lib")],
        Os::Linux => vec![format!("lib{name}.so"), format!("lib{name}.a")],
        Os::Macos => vec![format!("lib{name}.dylib"), format!("lib{name}.a")],
    }
}

/// The DLL an import library loads, in the package's bin folder (`debug/bin` for `debug/lib`) or beside the library.
fn dll_beside(library: &Path, dll: &str, session: &Session) -> Option<PathBuf> {
    let folder = library.parent()?;
    [folder.parent().map(|parent| parent.join("bin")), Some(folder.to_path_buf())]
        .into_iter()
        .flatten()
        .map(|candidate| candidate.join(dll))
        .find(|file| session.is_file(file))
}

/// The DLL an import library loads: MSVC's (`zlib.lib`, short import members) or MinGW's (`libz.dll.a`, whose
/// objects name it). None for a static library or a file that is no archive.
pub(crate) fn import_library_dll(file: &Path) -> Option<String> {
    let bytes = std::fs::read(file).ok()?;
    let mingw = file.to_string_lossy().to_ascii_lowercase().ends_with(".dll.a");

    for (name, data) in archive_members(&bytes) {
        if name.starts_with('/') {
            continue;
        }

        // A short import member: its header, the symbol's name, then the DLL's
        if data.starts_with(&[0, 0, 0xff, 0xff]) && data.len() > 20 {
            let mut strings = data[20..].split(|byte| *byte == 0);
            let _symbol = strings.next()?;
            return strings.next().map(|dll| String::from_utf8_lossy(dll).into_owned());
        }

        // dlltool's objects hold the DLL's name as a string
        if mingw {
            if let Some(dll) = dll_name_in(data) {
                return Some(dll);
            }
        }
    }

    None
}

/// The first `<name>.dll` NUL-terminated string in an object's bytes.
fn dll_name_in(data: &[u8]) -> Option<String> {
    let lower = data.to_ascii_lowercase();
    let end = lower.windows(5).position(|window| window == b".dll\0")? + 4;
    let start = data[..end]
        .iter()
        .rposition(|byte| !(byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'+')))
        .map_or(0, |position| position + 1);

    (end > start + 4).then(|| String::from_utf8_lossy(&data[start..end]).into_owned())
}

/// The compiler a library archive's C++ code was built for, by the names in its symbol table: MSVC's decorated
/// names (`?name@@...`) or the Itanium ABI's of GCC and Clang (`_ZN...`). None for an archive of C functions only,
/// which every compiler links, or a file that is no archive.
pub(crate) fn cxx_abi(file: &Path) -> Option<CxxAbi> {
    use std::io::Read;

    // The first member: the symbol table (a count, the members' offsets, then the names), big-endian in both formats
    let mut archive = std::fs::File::open(file).ok()?;
    let mut header = [0u8; 68];
    archive.read_exact(&mut header).ok()?;
    if &header[..8] != b"!<arch>\n" || header[8..24].trim_ascii() != b"/" {
        return None;
    }
    let size: usize = std::str::from_utf8(&header[56..66]).ok()?.trim().parse().ok()?;
    let mut table = vec![0u8; size];
    archive.read_exact(&mut table).ok()?;
    let count = u32::from_be_bytes(table.get(..4)?.try_into().ok()?) as usize;
    let names = table.get(4 + count * 4..)?;

    names.split(|byte| *byte == 0).find_map(|name| {
        let name = name.strip_prefix(b"__imp_").unwrap_or(name);
        match name {
            [b'?', ..] => Some(CxxAbi::Msvc),
            [b'_', b'Z', ..] | [b'_', b'_', b'Z', ..] => Some(CxxAbi::Gnu),
            _ => None,
        }
    })
}

/// An archive's members: each one's name field and data.
fn archive_members(bytes: &[u8]) -> Vec<(String, &[u8])> {
    let mut members = Vec::new();
    if !bytes.starts_with(b"!<arch>\n") {
        return members;
    }

    let mut offset = 8;
    while offset + 60 <= bytes.len() {
        let header = &bytes[offset..offset + 60];
        let Some(size) = std::str::from_utf8(&header[48..58])
            .ok()
            .and_then(|size| size.trim().parse::<usize>().ok())
        else {
            break;
        };
        let start = offset + 60;
        let end = (start + size).min(bytes.len());
        members.push((String::from_utf8_lossy(&header[..16]).trim().to_string(), &bytes[start..end]));
        offset = end + (size & 1);
    }

    members
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An archive of these members (name, data), with a symbol table of `symbols` first.
    fn archive(symbols: &[&str], members: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut bytes = b"!<arch>\n".to_vec();
        let mut add = |name: &str, data: &[u8]| {
            bytes.extend(format!("{name:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n", 0, 0, 0, 644, data.len()).as_bytes());
            bytes.extend(data);
            if data.len() % 2 == 1 {
                bytes.push(b'\n');
            }
        };

        let mut table = (symbols.len() as u32).to_be_bytes().to_vec();
        table.extend(std::iter::repeat_n(0u8, symbols.len() * 4));
        for symbol in symbols {
            table.extend(symbol.as_bytes());
            table.push(0);
        }
        add("/", &table);
        for (name, data) in members {
            add(name, data);
        }

        bytes
    }

    #[test]
    fn reads_import_libraries_and_symbol_tables() {
        let folder = std::env::temp_dir().join(format!("anvil-find-module-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();

        // MSVC's short import member: header, symbol, DLL
        let mut short = vec![0, 0, 0xff, 0xff, 0, 0, 0x64, 0x86];
        short.extend([0u8; 12]);
        short.extend(b"deflate\0zlibd1.dll\0");
        let import = folder.join("zlibd.lib");
        std::fs::write(&import, archive(&["__imp_deflate", "deflate"], &[("zlibd1.dll/", short)])).unwrap();
        assert_eq!(import_library_dll(&import).as_deref(), Some("zlibd1.dll"));
        assert_eq!(cxx_abi(&import), None);

        // MinGW's: the DLL's name in an object; a static library is no import library
        let mingw = folder.join("libz.dll.a");
        std::fs::write(
            &mingw,
            archive(&["_ZN2qt6assertEv"], &[("d000001.o/", b"\x4c\x01junk\0libzlib1.dll\0".to_vec())]),
        )
        .unwrap();
        assert_eq!(import_library_dll(&mingw).as_deref(), Some("libzlib1.dll"));
        assert_eq!(cxx_abi(&mingw), Some(CxxAbi::Gnu));
        let static_library = folder.join("png.lib");
        std::fs::write(
            &static_library,
            archive(&["?qt_assert@@YAXPEBD00H@Z"], &[("png.obj/", b"code.dll\0".to_vec())]),
        )
        .unwrap();
        assert_eq!(import_library_dll(&static_library), None);
        assert_eq!(cxx_abi(&static_library), Some(CxxAbi::Msvc));
        assert_eq!(cxx_abi(&folder.join("missing.lib")), None);

        std::fs::remove_dir_all(&folder).ok();
    }

    #[test]
    fn knows_module_only_packages() {
        assert_eq!(module_only_package("openssl").map(|(_, pc, _)| pc), Some(Some("openssl")));
        assert_eq!(module_only_package("PNG").map(|(_, pc, _)| pc), Some(Some("libpng")));
        assert!(module_only_package("fmt").is_none());
        assert!(find_module("ZLIB::ZLIB").is_some() && find_module("fmt::fmt").is_none());
    }
}

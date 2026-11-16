//! `delay-load`: DLLs a Windows program or shared library loads on its first call into them instead of at start. The
//! names a manifest may give, and the check that a linked binary really delay-loads each: the linker drops an entry
//! the binary imports nothing from (a misspelt name, an import library not linked), link.exe with a warning (LNK4199)
//! and lld-link silently.

use anyhow::{bail, Context, Result};
use std::collections::HashSet;
use std::path::Path;

// The PE data directory listing the delay-loaded DLLs, and the size of one entry of that list
const DELAY_IMPORT_DIRECTORY: usize = 13;
const DELAY_DESCRIPTOR_SIZE: usize = 32;

// The size of one section header
const SECTION_HEADER_SIZE: usize = 40;

/// Checks a `delay-load` list: DLL file names (".dll" in any case), without a folder, each once.
pub fn check_names(names: &[String]) -> Result<()> {
    let mut seen = HashSet::new();
    for name in names {
        let lowered = name.to_ascii_lowercase();
        let file_name = name.rsplit(['/', '\\', ':']).next().unwrap_or(name);

        if name.trim().is_empty() || name.trim() != name {
            bail!("delay-load = [\"{name}\"]: write the DLL's file name alone, such as \"nvml.dll\"");
        }
        if file_name != name {
            bail!("delay-load = [\"{name}\"]: name the DLL's file without a folder (\"{file_name}\"); Windows finds it at run time as any DLL");
        }
        if let Some(stem) = [".lib", ".a"].iter().find_map(|extension| lowered.strip_suffix(extension)) {
            bail!(
                "delay-load = [\"{name}\"]: name the DLL (\"{}.dll\"), not its import library, which `libs` or `deps` links",
                &name[..stem.len()]
            );
        }
        if !lowered.ends_with(".dll") || lowered == ".dll" {
            bail!("delay-load = [\"{name}\"]: name the DLL's file with its extension (\"{name}.dll\")");
        }
        if !seen.insert(lowered) {
            bail!("delay-load = [\"{name}\"]: listed twice");
        }
    }

    Ok(())
}

/// Checks that a linked binary delay-loads every DLL it was told to: each entry is a DLL's name and the error to show
/// when the binary does not.
pub fn check_linked(binary: &Path, entries: &[(String, String)]) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }

    let bytes = std::fs::read(binary).with_context(|| format!("reading {}", binary.display()))?;
    let Some(loaded) = delay_imports(&bytes) else {
        bail!(
            "{} is not a Windows binary Anvil can read, so its delay-loaded DLLs cannot be checked",
            binary.display()
        );
    };
    for (name, message) in entries {
        if !loaded.iter().any(|dll| dll.eq_ignore_ascii_case(name)) {
            bail!("{message}");
        }
    }

    Ok(())
}

/// The DLLs a PE file (program or DLL) delay-loads, from its delay-import table; None when it is no PE file.
fn delay_imports(bytes: &[u8]) -> Option<Vec<String>> {
    let read_u16 = |offset: usize| bytes.get(offset..offset + 2).map(|field| u16::from_le_bytes([field[0], field[1]]));
    let read_u32 = |offset: usize| bytes.get(offset..offset + 4).map(|field| u32::from_le_bytes(field.try_into().unwrap()));
    let read_u64 = |offset: usize| bytes.get(offset..offset + 8).map(|field| u64::from_le_bytes(field.try_into().unwrap()));

    // The PE header, then the optional header: 32-bit (PE32) or 64-bit (PE32+)
    let header = read_u32(0x3c)? as usize;
    if bytes.get(header..header + 4)? != b"PE\0\0" {
        return None;
    }
    let coff = header + 4;
    let section_count = read_u16(coff + 2)? as usize;
    let optional = coff + 20;
    let sections = optional + read_u16(coff + 16)? as usize;
    let (image_base, directories) = match read_u16(optional)? {
        0x10b => (read_u32(optional + 28)? as u64, optional + 96),
        0x20b => (read_u64(optional + 24)?, optional + 112),
        _ => return None,
    };

    // The delay-import table, if the binary has one
    let directory_count = read_u32(directories - 4)? as usize;
    let table = match directory_count > DELAY_IMPORT_DIRECTORY {
        true => read_u32(directories + DELAY_IMPORT_DIRECTORY * 8)?,
        false => 0,
    };
    if table == 0 {
        return Some(Vec::new());
    }

    // Addresses in the loaded image to offsets in the file, through the section that holds them
    let file_offset = |address: u32| -> Option<usize> {
        (0..section_count).find_map(|index| {
            let section = sections + index * SECTION_HEADER_SIZE;
            let size = read_u32(section + 8)?.max(read_u32(section + 16)?);
            let start = read_u32(section + 12)?;
            let raw_start = read_u32(section + 20)?;
            (address >= start && address - start < size).then(|| (address - start + raw_start) as usize)
        })
    };

    // One entry per DLL, up to an empty one; an entry without its address flag (bit 0) gives addresses, not offsets
    let mut names = Vec::new();
    let mut entry = file_offset(table)?;
    loop {
        let attributes = read_u32(entry)?;
        let name = read_u32(entry + 4)?;
        if name == 0 {
            break;
        }

        let name = match attributes & 1 {
            1 => name,
            _ => (name as u64).checked_sub(image_base)? as u32,
        };
        let start = file_offset(name)?;
        let length = bytes.get(start..)?.iter().position(|byte| *byte == 0)?;
        names.push(String::from_utf8_lossy(&bytes[start..start + length]).into_owned());
        entry += DELAY_DESCRIPTOR_SIZE;
    }

    Some(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| name.to_string()).collect()
    }

    /// A minimal 64-bit PE file whose delay-import table names `dlls`.
    fn pe_file(dlls: &[&str]) -> Vec<u8> {
        let mut bytes = vec![0u8; 0x400];
        let mut write = |offset: usize, field: &[u8]| bytes[offset..offset + field.len()].copy_from_slice(field);

        // Headers: one section at address 0x1000, file offset 0x200
        write(0x3c, &0x40u32.to_le_bytes());
        write(0x40, b"PE\0\0");
        write(0x46, &1u16.to_le_bytes());
        write(0x54, &240u16.to_le_bytes());
        write(0x58, &0x20bu16.to_le_bytes());
        write(0x58 + 108, &16u32.to_le_bytes());
        write(0x58 + 112 + 13 * 8, &0x1000u32.to_le_bytes());
        let section = 0x58 + 240;
        write(section + 8, &0x200u32.to_le_bytes());
        write(section + 12, &0x1000u32.to_le_bytes());
        write(section + 16, &0x200u32.to_le_bytes());
        write(section + 20, &0x200u32.to_le_bytes());

        // The table, then the names after it
        let mut name_address = 0x1100u32;
        for (index, dll) in dlls.iter().enumerate() {
            let entry = 0x200 + index * DELAY_DESCRIPTOR_SIZE;
            write(entry, &1u32.to_le_bytes());
            write(entry + 4, &name_address.to_le_bytes());
            let name_offset = (name_address - 0x1000 + 0x200) as usize;
            write(name_offset, dll.as_bytes());
            name_address += dll.len() as u32 + 1;
        }

        bytes
    }

    #[test]
    fn names_are_dll_files_listed_once() {
        assert!(check_names(&names(&["nvml.dll", "Helper.DLL"])).is_ok());

        let error = |list: &[&str]| check_names(&names(list)).unwrap_err().to_string();
        assert!(error(&["nvml"]).contains("(\"nvml.dll\")"), "{}", error(&["nvml"]));
        assert!(error(&["nvml.lib"]).contains("not its import library"));
        assert!(error(&["bin/nvml.dll"]).contains("without a folder (\"nvml.dll\")"));
        assert!(error(&["C:\\x\\nvml.dll"]).contains("without a folder"));
        assert!(error(&["nvml.dll", "NVML.dll"]).contains("delay-load = [\"NVML.dll\"]: listed twice"));
        assert!(error(&[""]).contains("file name alone"));
        assert!(error(&[" nvml.dll"]).contains("file name alone"));
        assert!(error(&[".dll"]).contains("with its extension"));
    }

    #[test]
    fn the_delay_import_table_names_the_dlls() {
        assert_eq!(
            delay_imports(&pe_file(&["helper.dll", "nvml.dll"])),
            Some(names(&["helper.dll", "nvml.dll"]))
        );
        assert_eq!(delay_imports(&pe_file(&[])), Some(Vec::new()));
        assert_eq!(delay_imports(b"not a binary"), None);
    }
}

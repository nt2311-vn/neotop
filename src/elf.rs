//! elf.rs — ELF (Linux) / Mach-O (macOS) parser for Go/Rust detection.
//! Linux: reads ELF from `/proc/<pid>/exe`. macOS: reads Mach-O from exe path.

#[cfg(target_os = "linux")]
use std::fs::File;
#[cfg(target_os = "macos")]
use std::fs::File;
#[cfg(target_os = "macos")]
use std::io::Read;
#[cfg(target_os = "linux")]
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::groups::Lang;

/// Hard cap on bytes scanned for the Rust-content heuristic. 8 MiB
/// covers nearly every release binary's rodata; bigger binaries
/// give up and stay classified as Native rather than burn I/O.
const MAX_RODATA_SCAN: u64 = 8 * 1024 * 1024;

/// Cap on the section header table size (entries × entry-size).
/// Real binaries top out at a few hundred sections; anything past
/// 1 MiB is malformed or hostile.
const MAX_SHTAB_BYTES: u64 = 1024 * 1024;

/// Inspect the ELF at `exe_path` and return the language it was
/// built with, if we can prove it. Returns `None` for any error
/// (file unreadable, not ELF, 32-bit, big-endian, malformed) — the
/// caller treats all of those as "no upgrade, stay Native".
#[cfg(target_os = "linux")]
pub(crate) fn detect_native_lang(exe_path: &Path) -> Option<Lang> {
    let mut f = File::open(exe_path).ok()?;
    let mut hdr = [0u8; 64];
    f.read_exact(&mut hdr).ok()?;

    // Magic + class + data encoding gates: we only handle ELF64
    // little-endian, which is x86_64 / aarch64 / riscv64. ELF32
    // and big-endian go through fast.
    if &hdr[..4] != b"\x7fELF" {
        return None;
    }
    if hdr[4] != 2 || hdr[5] != 1 {
        return None;
    }

    let read_u16 = |b: &[u8]| u16::from_le_bytes([b[0], b[1]]);
    let read_u32 = |b: &[u8]| u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    let read_u64 = |b: &[u8]| u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]);

    // ELF64 header layout offsets we care about.
    let e_shoff = read_u64(&hdr[40..48]);
    let e_shentsize = u64::from(read_u16(&hdr[58..60]));
    let e_shnum = u64::from(read_u16(&hdr[60..62]));
    let e_shstrndx = u64::from(read_u16(&hdr[62..64]));

    // Section header entries are exactly 64 bytes in ELF64. Reject
    // anything else to keep the offsets below unambiguous.
    if e_shentsize != 64 || e_shnum == 0 || e_shstrndx >= e_shnum {
        return None;
    }
    let shtab_bytes = e_shentsize.saturating_mul(e_shnum);
    if shtab_bytes > MAX_SHTAB_BYTES {
        return None;
    }

    let mut shtab = vec![0u8; usize::try_from(shtab_bytes).ok()?];
    f.seek(SeekFrom::Start(e_shoff)).ok()?;
    f.read_exact(&mut shtab).ok()?;

    // Locate the section-name string table itself, then read it.
    let strtab_hdr_off = usize::try_from(e_shstrndx * e_shentsize).ok()?;
    let strtab_offset = read_u64(&shtab[strtab_hdr_off + 24..strtab_hdr_off + 32]);
    let strtab_size = read_u64(&shtab[strtab_hdr_off + 32..strtab_hdr_off + 40]);
    if strtab_size == 0 || strtab_size > MAX_SHTAB_BYTES {
        return None;
    }
    let mut strtab = vec![0u8; usize::try_from(strtab_size).ok()?];
    f.seek(SeekFrom::Start(strtab_offset)).ok()?;
    f.read_exact(&mut strtab).ok()?;

    // First pass: by section name. Go's .note.go.buildid /
    // .gopclntab are unambiguous — bail immediately on a hit. For
    // Rust we collect rodata-shaped sections to scan in pass two
    // and remember whether the symbol table looks rust-mangled.
    let mut rodata_chunks: Vec<(u64, u64)> = Vec::new();
    let mut symtab: Option<(u64, u64)> = None;
    let mut symstrtab: Option<(u64, u64)> = None;

    for i in 0..e_shnum {
        let off = usize::try_from(i * e_shentsize).ok()?;
        let name_off = read_u32(&shtab[off..off + 4]) as usize;
        let sh_type = read_u32(&shtab[off + 4..off + 8]);
        let sh_link = read_u32(&shtab[off + 40..off + 44]);
        let sh_offset = read_u64(&shtab[off + 24..off + 32]);
        let sh_size = read_u64(&shtab[off + 32..off + 40]);
        let name = name_at(&strtab, name_off);

        if name == b".note.go.buildid"
            || name == b".gopclntab"
            || name == b".gosymtab"
            || name == b".go.buildinfo"
        {
            return Some(Lang::Go);
        }
        // Anything that smells like read-only data goes on the
        // scan list. We keep the list bounded by MAX_RODATA_SCAN
        // when we actually read it.
        if name == b".rodata"
            || name.starts_with(b".rodata.")
            || name == b".rdata"
            || name == b".data.rel.ro"
        {
            rodata_chunks.push((sh_offset, sh_size));
        }
        // SHT_SYMTAB == 2, SHT_DYNSYM == 11. Both carry mangled
        // function names; the linked SHT_STRTAB holds the strings.
        if (sh_type == 2 || sh_type == 11) && u64::from(sh_link) < e_shnum {
            let link_off = usize::try_from(u64::from(sh_link) * e_shentsize).ok()?;
            let link_offset = read_u64(&shtab[link_off + 24..link_off + 32]);
            let link_size = read_u64(&shtab[link_off + 32..link_off + 40]);
            symtab = Some((sh_offset, sh_size));
            symstrtab = Some((link_offset, link_size));
        }
    }

    if rust_in_rodata(&mut f, &rodata_chunks) {
        return Some(Lang::Rust);
    }
    if rust_in_symtab(&mut f, symtab, symstrtab) {
        return Some(Lang::Rust);
    }
    None
}

/// Pass two: search rodata for the libstd panic-location prefix.
/// This is the cheapest signal that holds even after stripping —
/// the strings are still embedded because panic backtraces need
/// them. We bail out as soon as we find a hit.
#[cfg(target_os = "linux")]
fn rust_in_rodata(f: &mut File, chunks: &[(u64, u64)]) -> bool {
    let mut budget = MAX_RODATA_SCAN;
    for (offset, size) in chunks {
        if budget == 0 {
            break;
        }
        let take = (*size).min(budget);
        let Ok(take_usz) = usize::try_from(take) else {
            continue;
        };
        let mut buf = vec![0u8; take_usz];
        if f.seek(SeekFrom::Start(*offset)).is_err() {
            continue;
        }
        if f.read_exact(&mut buf).is_err() {
            continue;
        }
        budget = budget.saturating_sub(take);
        if contains(&buf, b"library/std/src/") || contains(&buf, b"/rustc/") {
            return true;
        }
    }
    false
}

/// Pass three: rust-mangled symbol prefix. Only triggers on
/// unstripped binaries (debug builds, packagers that keep symbols),
/// so stripped release Rust binaries already had to pass through
/// pass two.
#[cfg(target_os = "linux")]
fn rust_in_symtab(f: &mut File, symtab: Option<(u64, u64)>, symstrtab: Option<(u64, u64)>) -> bool {
    let (Some((_, sym_size)), Some((str_off, str_size))) = (symtab, symstrtab) else {
        return false;
    };
    if str_size == 0 || str_size > MAX_SHTAB_BYTES || sym_size == 0 {
        return false;
    }
    let Ok(str_size_usz) = usize::try_from(str_size) else {
        return false;
    };
    let mut sbuf = vec![0u8; str_size_usz];
    if f.seek(SeekFrom::Start(str_off)).is_err() || f.read_exact(&mut sbuf).is_err() {
        return false;
    }
    // v0 mangling: every Rust symbol starts with `_R`. Legacy
    // mangling: rust-specific symbols often end in `..llvm.<hash>`.
    contains(&sbuf, b"_RNv") || contains(&sbuf, b"..llvm.")
}

/// Read a NUL-terminated name out of the section-name string table.
fn name_at(strtab: &[u8], off: usize) -> &[u8] {
    if off >= strtab.len() {
        return &[];
    }
    let s = &strtab[off..];
    s.iter().position(|&b| b == 0).map_or(s, |end| &s[..end])
}

/// Plain byte-substring search. We don't depend on `memchr`, so this
/// is a hand-rolled scan with an early-exit on the first byte. Good
/// enough for ≤8 MiB rodata blobs at first-scan time; result is
/// cached afterwards.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || needle.len() > haystack.len() {
        return false;
    }
    let first = needle[0];
    let last_start = haystack.len() - needle.len();
    let mut i = 0;
    while i <= last_start {
        if haystack[i] == first && &haystack[i..i + needle.len()] == needle {
            return true;
        }
        i += 1;
    }
    false
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_at_returns_slice_until_nul() {
        let strtab = b"\0.text\0.rodata\0.note.go.buildid\0";
        assert_eq!(name_at(strtab, 1), b".text");
        assert_eq!(name_at(strtab, 7), b".rodata");
        assert_eq!(name_at(strtab, 15), b".note.go.buildid");
    }

    #[test]
    fn name_at_handles_out_of_range_offset() {
        let strtab = b"\0.text\0";
        assert_eq!(name_at(strtab, 999), b"");
    }

    #[test]
    fn contains_finds_present_substring() {
        assert!(contains(b"hello world", b"world"));
        assert!(contains(
            b"AAA library/std/src/main.rs ZZZ",
            b"library/std/src/"
        ));
    }

    #[test]
    fn contains_rejects_missing_substring() {
        assert!(!contains(b"hello world", b"WORLD"));
        assert!(!contains(b"short", b"longer needle"));
        assert!(!contains(b"", b"x"));
        assert!(!contains(b"x", b""));
    }

    #[test]
    fn detect_native_lang_returns_none_for_missing_file() {
        // Touching real /proc paths would make the test
        // host-dependent. Pointing at a non-existent file exercises
        // the "I/O error → None" path without that hazard.
        let p = Path::new("/proc/self/this-does-not-exist-zzz");
        assert!(detect_native_lang(p).is_none());
    }

    #[test]
    fn detect_native_lang_returns_none_for_non_elf() {
        // /etc/hostname is a tiny text file, definitely not ELF —
        // first 4 bytes won't match the magic, so we exit cleanly.
        let p = Path::new("/etc/hostname");
        assert!(detect_native_lang(p).is_none());
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn detect_native_lang(exe_path: &Path) -> Option<Lang> {
    // macOS Mach-O parsing for Go/Rust detection
    let mut f = std::fs::File::open(exe_path).ok()?;
    let mut hdr = [0u8; 32];
    f.read_exact(&mut hdr).ok()?;

    // Mach-O magic: 0xFEEDFACE (32-bit) or 0xFEEDFACF (64-bit)
    let magic = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
    if magic != 0xFEEDFACE && magic != 0xFEEDFACF {
        return None;
    }

    // Read first 8KB of file for string search
    let mut buf = vec![0u8; 8192];
    if f.read_exact(&mut buf).is_err() {
        return None;
    }

    // Go detection: look for common Go runtime strings
    if contains(&buf, b"go.buildid")
        || contains(&buf, b"runtime.g")
        || contains(&buf, b"runtime.main")
        || contains(&buf, b"Go build ID")
    {
        return Some(Lang::Go);
    }

    // Rust detection: look for common Rust std strings
    if contains(&buf, b"library/std/src/") || contains(&buf, b"/rustc/") || contains(&buf, b"std::")
    {
        return Some(Lang::Rust);
    }

    None
}

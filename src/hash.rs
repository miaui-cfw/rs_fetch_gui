//! The RetroAchievements hash of a game, computed on the PC exactly as minarch computes it on the console
//! (rcheevos' rc_hash_generate via ra_offline.c). Direct ports of rcheevos' src/hash/*.c, by way of ra_fetch.py,
//! checked file for file against the console's own rcheevos through the web manager's /api/hash.
//!
//! The entry point is hash_rom(); the console table maps a folder tag to how its games are hashed.

#![allow(dead_code)]

use std::collections::HashMap;
use std::fs::File;
use std::io::{Cursor, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use anyhow::{Result, anyhow, bail};
use md5::{Digest, Md5};

/// Matches rcheevos' own cap on how much of a file it will hash.
pub(crate) const MAX_BUFFER_SIZE: u64 = 64 * 1024 * 1024;

// ===========================================================================
// console table
//
// The key is the tag in parentheses at the end of a Roms/ sub-folder name, eg.
// "Nintendo Game Boy (GB)" -> "GB". That is exactly what minarch's getEmuName()
// pulls out and hands to RA_loadGame(), so the two sides agree by construction.
//
// The value is (rc_console_id, hash method). The console id is only here for
// reference/reporting - the hash method is what actually matters.
// ===========================================================================

#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Method {
    Whole,
    Nes,
    Snes,
    Lynx,
    Pce,
    A7800,
    N64,
    Psx,
    SegaCd,
    Arcade,
    /// a PC Engine CD disc (.cue / .chd) in the PCE folder: minarch's ra_consoleIdForTag() makes it
    /// RC_CONSOLE_PC_ENGINE_CD, never the cartridge hash
    PceCd,
}

/// Everything the mapping dropdown offers. A7800 is in here even though no
/// tag in the table reaches it, which is exactly the point of being able to
/// map a folder by hand.
pub(crate) const ALL_METHODS: &[Method] = &[
    Method::A7800,
    Method::Arcade,
    Method::Whole,
    Method::Nes,
    Method::Snes,
    Method::Lynx,
    Method::Pce,
    Method::N64,
    Method::Psx,
    Method::SegaCd,
];

impl Method {
    /// What the hash routine is called. Short, for the inline label.
    fn name(self) -> &'static str {
        match self {
            Method::A7800 => "a7800",
            Method::Arcade => "arcade",
            Method::Whole => "generic",
            Method::Nes => "nes",
            Method::Snes => "snes",
            Method::Lynx => "lynx",
            Method::Pce => "pce",
            Method::N64 => "n64",
            Method::Psx => "psx",
            Method::SegaCd => "segacd",
            Method::PceCd => "pcecd",
        }
    }

    /// What it actually does, for someone choosing from the dropdown.
    fn description(self) -> &'static str {
        match self {
            Method::A7800 => "a7800 - skips a 128 byte header",
            Method::Whole => "generic - hash the whole file",
            Method::Nes => "nes - skips an iNES / FDS header",
            Method::Snes => "snes - skips a 512 byte copier header",
            Method::Lynx => "lynx - skips a 64 byte LNX header",
            Method::Pce => "pce - skips a 512 byte header",
            Method::N64 => "n64 - byteswaps to z64 first",
            Method::Psx => "psx - hash executable and contents",
            Method::SegaCd => "segacd - hash some sectors",
            Method::Arcade => "arcade - special mame md5 method",
            Method::PceCd => "pcecd - header sector title + boot program",
        }
    }
}

pub(crate) const CONSOLES: &[(&str, u32, Method)] = &[
    // --- plain whole-file md5 -------------------------------------------
    ("CPC", 37, Method::Whole),    // Amstrad CPC
    ("STELLA", 25, Method::Whole), // Atari 2600
    ("VICE", 30, Method::Whole),   // Commodore 64
    ("BMSX", 29, Method::Whole),   // MSX
    ("NGP", 14, Method::Whole),    // Neo Geo Pocket
    ("NGPC", 14, Method::Whole),   // Neo Geo Pocket Color (same RA console)
    ("GB", 4, Method::Whole),
    ("GBC", 6, Method::Whole),
    ("GBA", 5, Method::Whole),
    ("SGB", 4, Method::Whole), // Super Game Boy runs GB roms -> GB achievements
    ("VB", 28, Method::Whole), // Virtual Boy
    ("PKM", 24, Method::Whole), // Pokemon Mini
    ("GG", 15, Method::Whole), // Game Gear
    ("SMS", 11, Method::Whole), // Master System
    ("MD", 1, Method::Whole),  // Mega Drive
    ("MD32X", 10, Method::Whole), // Sega 32X
    ("T80", 65, Method::Whole), // TIC-80
    ("WATA", 63, Method::Whole), // Watara Supervision
    ("FUSE", 59, Method::Whole), // ZX Spectrum
    // --- header strippers -----------------------------------------------
    ("FC", 7, Method::Nes),     // NES/Famicom (iNES + FDS headers)
    ("SFC", 3, Method::Snes),   // SNES (512-byte copier header)
    ("SUPA", 3, Method::Snes),  // SNES MSU-1
    ("HANDY", 13, Method::Lynx), // Atari Lynx (64-byte LNX header)
    ("PCE", 8, Method::Pce),    // PC Engine (512-byte header)
    // --- special --------------------------------------------------------
    ("N64", 2, Method::N64),    // byteswapped to z64 before hashing
    ("PS", 12, Method::Psx),    // boot executable name + contents
    ("MDCD", 9, Method::SegaCd), // first 512 bytes of sector 0
    ("MAME", 27, Method::Arcade), // hash is the romset filename
    // the arcade folders: one RA system, several cores (minarch's
    // common/utils.c RA_ARCADE_TAGS lists the same tags)
    ("KMMAME", 27, Method::Arcade),
    ("KMMAMECPS", 27, Method::Arcade),
    ("MAME2003P", 27, Method::Arcade),
    ("FBANEO", 27, Method::Arcade),
    ("FBNEOCPS", 27, Method::Arcade),
    ("FBNEOGEO", 27, Method::Arcade),
];

/// Folders whose systems this tool can't hash. Listed explicitly so they're
/// reported as skipped rather than silently producing a wrong hash.
/// The arcade folders: one RetroAchievements system, several cores (minarch's common/utils.c RA_ARCADE_TAGS).
pub(crate) const ARCADE_TAG_LIST: &[&str] = &["MAME", "KMMAME", "KMMAMECPS", "MAME2003P", "FBANEO", "FBNEOCPS", "FBNEOGEO"];

pub(crate) const UNSUPPORTED: &[(&str, &str)] = &[
    ("HATARI", "Atari ST"),
    ("UAE", "Amiga"),
    ("DOOM", "Doom engine"),
    ("DOSBOX", "MS-DOS (needs the whole game folder hashed)"),
    ("NEOCD", "Neo Geo CD"),
    ("NDS", "Nintendo DS"),
    ("PSP", "PSP"),
    ("X68", "Sharp X68000"),
    ("FAKE8", "Pico-8"),
    ("ECWOLF", "Wolfenstein"),
];

/// never ROMs
pub(crate) const IGNORED_EXTS: &[&str] = &[
    ".txt", ".xml", ".json", ".dat", ".nfo", ".sh", ".cfg", ".ini", ".png", ".jpg",
    ".jpeg", ".gif", ".bmp", ".webp", ".srm", ".sav", ".state", ".rtc", ".bak", ".db", ".m3u",
    ".pak", ".part",
];

/// disc images we can read directly
pub(crate) const DISC_EXTS: &[&str] = &[".cue", ".bin", ".iso", ".img", ".chd"];

/// The extensions of the core each system's pak runs (its .info supported_extensions, read off a console). minarch
/// uses them twice: a core that lists "zip" gets the zip as it is (so the zip is what gets hashed), and for the
/// others the first file inside the zip with one of these extensions is unpacked and hashed.
pub(crate) const CORE_EXTS: &[(&str, &str)] = &[
    ("CPC", "dsk|sna|zip|tap|cdt|voc|cpr|m3u"),
    ("STELLA", "a26|bin"),
    ("VICE", "d64|d71|d80|d81|d82|g64|g41|x64|t64|tap|prg|p00|crt|bin|zip|gz|d6z|d7z|d8z|g6z|g4z|x6z|cmd|m3u|vfl|vsf|nib|nbz|d2m|d4m"),
    ("BMSX", "rom|ri|mx1|mx2|col|dsk|cas|sg|sc|sf|m3u"),
    ("NGP", "ngp|ngc|ngpc|npc"),
    ("NGPC", "ngp|ngc|ngpc|npc"),
    ("GB", "gb|gbc|dmg"),
    ("GBC", "gb|gbc|dmg"),
    ("GBA", "gba|bin"),
    ("SGB", "gb|gbc|gba"),
    ("VB", "vb|vboy|bin"),
    ("PKM", "min"),
    ("GG", "mdx|md|smd|gen|bin|cue|iso|sms|bms|gg|sg|68k|sgd|chd|m3u"),
    ("SMS", "mdx|md|smd|gen|bin|cue|iso|sms|bms|gg|sg|68k|sgd|chd|m3u"),
    ("MD", "mdx|md|smd|gen|bin|cue|iso|sms|bms|gg|sg|68k|sgd|chd|m3u"),
    ("MD32X", "bin|gen|smd|md|32x|cue|iso|chd|sms|gg|sg|sc|m3u|68k|sgd|pco"),
    ("T80", "tic"),
    ("WATA", "bin|sv"),
    ("FUSE", "tzx|tap|z80|rzx|scl|trd|dsk|zip"),
    ("FC", "fds|nes|unif|unf"),
    ("SFC", "smc|fig|sfc|gd3|gd7|dx2|bsx|swc"),
    ("SUPA", "smc|fig|sfc|gd3|gd7|dx2|bsx|swc"),
    ("HANDY", "lnx|o"),
    ("PCE", "pce|cue|ccd|chd|toc|m3u"),
    ("N64", "n64|v64|z64|bin|u1|ndd"),
    ("PS", "bin|cue|img|mdf|pbp|toc|cbn|m3u|ccd|chd|iso|exe"),
    ("MDCD", "mdx|md|smd|gen|bin|cue|iso|sms|bms|gg|sg|68k|sgd|chd|m3u"),
];

pub(crate) fn core_exts(tag: &str) -> Vec<&'static str> {
    CORE_EXTS
        .iter()
        .find(|(t, _)| t.eq_ignore_ascii_case(tag))
        .map(|(_, e)| e.split('|').collect())
        .unwrap_or_default()
}

/// How this particular file is hashed: the folder's method, except that a disc image in the PCE folder is a
/// PC Engine CD game (minarch: .cue, .chd, .ccd and .m3u; only .cue and .chd can be hashed, as in rcheevos).
pub(crate) fn method_for_file(method: Method, path: &Path) -> Method {
    if method == Method::Pce && matches!(ext_of(path).as_str(), ".cue" | ".chd" | ".ccd" | ".m3u") {
        Method::PceCd
    } else {
        method
    }
}

pub(crate) fn console_for(tag: &str) -> Option<(u32, Method)> {
    CONSOLES
        .iter()
        .find(|(t, _, _)| *t == tag)
        .map(|(_, id, m)| (*id, *m))
}

pub(crate) fn unsupported_for(tag: &str) -> Option<&'static str> {
    UNSUPPORTED
        .iter()
        .find(|(t, _)| *t == tag)
        .map(|(_, why)| *why)
}

/// Lowercased extension *with* the leading dot, matching Path.suffix.lower().
pub(crate) fn ext_of(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{}", e.to_lowercase()))
        .unwrap_or_default()
}

pub(crate) fn file_name(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string()
}

// ===========================================================================
// md5 helpers
// ===========================================================================

pub(crate) fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

pub(crate) fn md5_of(chunks: &[&[u8]]) -> String {
    let mut h = Md5::new();
    for c in chunks {
        h.update(c);
    }
    hex(&h.finalize())
}

// ===========================================================================
// cartridge hashes (ports of rcheevos/src/hash/rc_hash_rom.c)
// ===========================================================================

/// Plain md5 of the file from `header` onwards, capped the way rcheevos caps it.
pub(crate) fn hash_skipping(path: &Path, header: u64) -> Result<Option<String>> {
    let mut f = File::open(path)?;
    if header > 0 {
        f.seek(SeekFrom::Start(header))?;
    }
    let mut h = Md5::new();
    let mut remaining = MAX_BUFFER_SIZE;
    let mut buf = vec![0u8; 65536];
    while remaining > 0 {
        let want = std::cmp::min(65536, remaining) as usize;
        let n = f.read(&mut buf[..want])?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        remaining -= n as u64;
    }
    Ok(Some(hex(&h.finalize())))
}

pub(crate) fn hash_whole(path: &Path) -> Result<Option<String>> {
    hash_skipping(path, 0)
}

/// Reads up to `n` bytes from the start of a file, tolerating a short file.
pub(crate) fn head_of(path: &Path, n: usize) -> Result<Vec<u8>> {
    let mut f = File::open(path)?;
    let mut buf = vec![0u8; n];
    let mut filled = 0;
    while filled < n {
        let got = f.read(&mut buf[filled..])?;
        if got == 0 {
            break;
        }
        filled += got;
    }
    buf.truncate(filled);
    Ok(buf)
}

pub(crate) fn file_size(path: &Path) -> Result<u64> {
    Ok(std::fs::metadata(path)?.len())
}

pub(crate) fn hash_nes(path: &Path) -> Result<Option<String>> {
    let head = head_of(path, 16)?;
    let size = file_size(path)?;
    if size > 16 && head.len() >= 4 && (&head[..4] == b"NES\x1a" || &head[..4] == b"FDS\x1a") {
        return hash_skipping(path, 16); // ignore the 16-byte header
    }
    hash_whole(path)
}

pub(crate) fn hash_snes(path: &Path) -> Result<Option<String>> {
    let size = file_size(path)?;
    // the core looks for anything 512 bytes over a multiple of 8KB
    if size - (size / 0x2000) * 0x2000 == 512 {
        return hash_skipping(path, 512); // ignore the copier header
    }
    hash_whole(path)
}

pub(crate) fn hash_lynx(path: &Path) -> Result<Option<String>> {
    let size = file_size(path)?;
    if size > 64 && head_of(path, 5)? == b"LYNX\x00" {
        return hash_skipping(path, 64);
    }
    hash_whole(path)
}

pub(crate) fn hash_pce(path: &Path) -> Result<Option<String>> {
    // 512 bytes more than a multiple of 8KB means there's a header.
    // Deliberately a bitwise AND, as in the Python and in rcheevos.
    if file_size(path)? & 512 != 0 {
        return hash_skipping(path, 512);
    }
    hash_whole(path)
}

/// Atari 7800. No tag in the console table maps here, exactly as in the Python
/// script - it is kept so that adding one row is all it would take.
#[allow(dead_code)]
pub(crate) fn hash_7800(path: &Path) -> Result<Option<String>> {
    let size = file_size(path)?;
    let head = head_of(path, 10)?;
    if size > 128 && head.len() >= 10 && &head[1..10] == b"ATARI7800" {
        return hash_skipping(path, 128);
    }
    hash_whole(path)
}

/// N64 images come in three byte orders; everything is hashed as z64.
pub(crate) fn hash_n64(path: &Path) -> Result<Option<String>> {
    let mut f = File::open(path)?;
    let mut first = [0u8; 1];
    if f.read(&mut first)? == 0 {
        return Ok(None);
    }

    let swap: usize = match first[0] {
        0x80 => 0,          // z64, big endian (native)
        0x37 => 2,          // v64, 16-bit byteswapped
        0x40 => 4,          // n64, 32-bit little endian
        0xE8 | 0x22 => 0,   // ndd, left alone
        _ => return Ok(None), // not an N64 rom
    };

    f.seek(SeekFrom::Start(0))?;
    let mut h = Md5::new();
    let mut remaining = std::cmp::min(file_size(path)?, MAX_BUFFER_SIZE);
    let mut buf = vec![0u8; 65536];
    while remaining > 0 {
        let want = std::cmp::min(65536, remaining) as usize;
        let n = f.read(&mut buf[..want])?;
        if n == 0 {
            break;
        }
        remaining -= n as u64;
        if swap > 0 {
            // rcheevos byteswaps whole words only; a ragged tail is left be
            let usable = n - (n % swap);
            for word in buf[..usable].chunks_mut(swap) {
                word.reverse();
            }
        }
        h.update(&buf[..n]);
    }
    Ok(Some(hex(&h.finalize())))
}

/// FBNeo loads other systems' games from folders with these names; rcheevos
/// puts the folder in the hash then (rc_hash_arcade)
pub(crate) const FBNEO_FOLDERS: &[&str] = &[
    "nes", "fds", "sms", "msx", "ngp", "pce", "chf", "sgx", "tg16", "msx1", "neocd", "coleco",
    "sg1000", "genesis", "gamegear", "megadriv", "pcengine", "channelf", "spectrum", "megadrive",
    "supergrafx", "zxspectrum", "mastersystem", "colecovision",
];

/// Arcade sets are identified by romset name, not contents: the md5 of the
/// file name without its extension ("sf2.zip" -> md5("sf2")), never of the
/// zip's bytes. A port of rcheevos' rc_hash_arcade, folder rule and all.
pub(crate) fn hash_arcade(path: &Path) -> Result<Option<String>> {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let Some(dot) = name.rfind('.') else {
        return Ok(Some(md5_of(&[name.as_bytes()]))); // an unpacked romset folder
    };
    let stem = &name[..dot];
    let folder = path
        .parent()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if !folder.is_empty()
        && folder.len() < 16
        && FBNEO_FOLDERS.contains(&folder.as_str())
        && folder.len() + stem.len() + 1 < 128
    {
        return Ok(Some(md5_of(&[folder.as_bytes(), b"_", stem.as_bytes()])));
    }
    Ok(Some(md5_of(&[stem.as_bytes()])))
}

// ===========================================================================
// disc images (ports of rcheevos/src/hash/rc_cdreader.c + rc_hash_disc.c)
// ===========================================================================

pub(crate) const SYNC_PATTERN: [u8; 12] = [
    0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00,
];

/// ISO9660 lookups, shared by the plain-image and CHD backends.
pub(crate) trait DiscImage {
    fn read_sector(&mut self, sector: i64, length: usize) -> Result<Vec<u8>>;

    fn first_sector(&self) -> i64 {
        0
    }

    /// Walk the ISO9660 root directory. Returns (sector, size).
    fn find_file(&mut self, path: &str) -> Result<Option<(i64, u32)>> {
        let path = path.trim_start_matches('\\');
        let (name, rest) = match path.split_once('\\') {
            Some((a, b)) => (a, Some(b)),
            None => (path, None),
        };
        if let Some(rest) = rest {
            let Some(found) = self.find_file(name)? else {
                return Ok(None);
            };
            return self.scan(found.0, 1, rest);
        }

        // the primary volume descriptor points at the root directory record
        let pvd = self.read_sector(self.first_sector() + 16, 256)?;
        if pvd.len() < 170 {
            return Ok(None);
        }
        let sector =
            pvd[158] as i64 | ((pvd[159] as i64) << 8) | ((pvd[160] as i64) << 16);
        let block = pvd[128] as u32 | ((pvd[129] as u32) << 8);
        let num_sectors = if block == 0 {
            1
        } else {
            let length = pvd[166] as u32
                | ((pvd[167] as u32) << 8)
                | ((pvd[168] as u32) << 16)
                | ((pvd[169] as u32) << 24);
            std::cmp::max(1, length / block)
        };
        self.scan(sector, num_sectors, name)
    }

    fn scan(&mut self, sector: i64, num_sectors: u32, name: &str) -> Result<Option<(i64, u32)>> {
        let target: Vec<u8> = name.bytes().collect();
        let tlen = target.len();
        let mut sector = sector;
        let mut num_sectors = num_sectors;
        let mut buf = self.read_sector(sector, 2048)?;
        let mut pos = 0usize;

        loop {
            if pos >= buf.len() || buf[pos] == 0 {
                if num_sectors > 1 {
                    num_sectors -= 1;
                    sector += 1;
                    buf = self.read_sector(sector, 2048)?;
                    if !buf.is_empty() {
                        pos = 0;
                        continue;
                    }
                }
                return Ok(None);
            }

            let rec_len = buf[pos] as usize;
            let name_len = if pos + 32 < buf.len() { buf[pos + 32] as usize } else { 0 };
            let entry_end = pos + 33 + tlen;
            let entry: &[u8] = if entry_end <= buf.len() {
                &buf[pos + 33..entry_end]
            } else {
                &[]
            };
            // records are "NAME;version" for files, bare "NAME" for directories
            let terminated =
                name_len == tlen || (entry_end < buf.len() && buf[entry_end] == b';');

            if terminated
                && entry.len() == tlen
                && entry.eq_ignore_ascii_case(&target)
                && pos + 14 <= buf.len()
            {
                let found = buf[pos + 2] as i64
                    | ((buf[pos + 3] as i64) << 8)
                    | ((buf[pos + 4] as i64) << 16);
                let size = buf[pos + 10] as u32
                    | ((buf[pos + 11] as u32) << 8)
                    | ((buf[pos + 12] as u32) << 16)
                    | ((buf[pos + 13] as u32) << 24);
                return Ok(Some((found, size)));
            }

            if rec_len == 0 {
                return Ok(None);
            }
            pos += rec_len;
        }
    }

    fn read_file(&mut self, sector: i64, size: u32) -> Result<Vec<u8>> {
        let mut size = std::cmp::min(size as u64, MAX_BUFFER_SIZE);
        let mut sector = sector;
        let mut out = Vec::new();
        while size > 0 {
            let want = std::cmp::min(size, 2048) as usize;
            let chunk = self.read_sector(sector, want)?;
            if chunk.is_empty() {
                break;
            }
            let take = std::cmp::min(size as usize, chunk.len());
            out.extend_from_slice(&chunk[..take]);
            size = size.saturating_sub(chunk.len() as u64);
            sector += 1;
        }
        Ok(out)
    }
}

/// Plain CD image: raw 2352/2336 or cooked 2048.
pub(crate) struct Disc {
    f: File,
    sector_size: i64,
    header_size: i64,
    first_sector: i64,
    /// where the track starts in the file (a cue sheet's INDEX 01 when several tracks share one .bin)
    base: i64,
}

impl Disc {
    fn open(path: &Path) -> Result<Self> {
        let mut d = Disc {
            f: File::open(path)?,
            sector_size: 0,
            header_size: 0,
            first_sector: 0,
            base: 0,
        };
        d.detect()?;
        Ok(d)
    }

    /// A track whose layout the cue sheet states (PC Engine data tracks have no ISO9660 volume to detect it by).
    fn track(path: &Path, base: i64, sector_size: i64, header_size: i64) -> Result<Self> {
        Ok(Disc { f: File::open(path)?, sector_size, header_size, first_sector: 0, base })
    }

    fn detect(&mut self) -> Result<()> {
        // Sector 16 holds a volume descriptor containing "CD001". Try each
        // layout until one lines up - the .cue, if any, may well be lying.
        for size in [2352i64, 2336] {
            self.f.seek(SeekFrom::Start((16 * size) as u64))?;
            let mut head = [0u8; 32];
            let n = read_up_to(&mut self.f, &mut head)?;
            if n >= 32 && head[..12] == SYNC_PATTERN {
                self.sector_size = size;
                self.header_size = if &head[25..30] == b"CD001" { 24 } else { 16 };
                // the raw header carries a BCD MSF address; turn it back into
                // a sector index so we know where this track starts
                let (m, s, fr) = (bcd(head[12]), bcd(head[13]), bcd(head[14]));
                self.first_sector = ((m * 60) + s) * 75 + fr - 150 - 16;
                return Ok(());
            }
        }

        self.f.seek(SeekFrom::Start(16 * 2048))?;
        let mut head = [0u8; 32];
        let n = read_up_to(&mut self.f, &mut head)?;
        if n >= 6 && &head[1..6] == b"CD001" {
            self.sector_size = 2048;
            self.header_size = 0;
            self.first_sector = 0;
        }
        Ok(())
    }

    fn valid(&self) -> bool {
        self.sector_size != 0
    }
}

pub(crate) fn bcd(b: u8) -> i64 {
    ((b >> 4) * 10 + (b & 0x0F)) as i64
}

pub(crate) fn read_up_to(f: &mut File, buf: &mut [u8]) -> Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = f.read(&mut buf[filled..])?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    Ok(filled)
}

impl DiscImage for Disc {
    fn first_sector(&self) -> i64 {
        self.first_sector
    }

    fn read_sector(&mut self, sector: i64, length: usize) -> Result<Vec<u8>> {
        if sector < self.first_sector {
            return Ok(Vec::new());
        }
        let start = self.base + (sector - self.first_sector) * self.sector_size + self.header_size;
        if start < 0 {
            return Ok(Vec::new());
        }
        self.f.seek(SeekFrom::Start(start as u64))?;
        let want = std::cmp::min(length, 2048);
        let mut buf = vec![0u8; want];
        let n = read_up_to(&mut self.f, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }
}

// ===========================================================================
// CHD images (ports of libchdr: chd.c's v5 map, huffman.c, the cd codecs)
//
// Only what hashing needs is here: CHD v5, its hunk map, and the data-track
// codecs. FLAC hunks (cdfl/flac) are not decoded - chdman only picks those for
// audio tracks, and we never read one. Anything unsupported is an error, which
// the caller turns into a "skipped" line rather than a wrong hash.
// ===========================================================================

pub(crate) const CD_FRAME_SIZE: usize = 2448;
pub(crate) const CD_SECTOR_DATA: usize = 2352;
pub(crate) const CD_SUBCODE_DATA: usize = 96;

// compression types in a decoded v5 map entry
pub(crate) const C_NONE: u32 = 4;
pub(crate) const C_SELF: u32 = 5;
pub(crate) const C_PARENT: u32 = 6;
pub(crate) const C_RLE_SMALL: u32 = 7;
pub(crate) const C_RLE_LARGE: u32 = 8;
pub(crate) const C_SELF_0: u32 = 9;
pub(crate) const C_SELF_1: u32 = 10;
pub(crate) const C_PARENT_SELF: u32 = 11;
pub(crate) const C_PARENT_0: u32 = 12;
pub(crate) const C_PARENT_1: u32 = 13;

pub(crate) const CHD_CODEC_NONE: u32 = 0;

pub(crate) const fn tag4(s: &[u8; 4]) -> u32 {
    ((s[0] as u32) << 24) | ((s[1] as u32) << 16) | ((s[2] as u32) << 8) | (s[3] as u32)
}

/// MSB-first bit reader, equivalent to libchdr's bitstream.c.
pub(crate) struct BitReader {
    data: Vec<u8>,
    pos: usize,
    limit: usize,
}

impl BitReader {
    fn new(data: Vec<u8>) -> Self {
        let limit = data.len() * 8;
        let mut data = data;
        data.extend_from_slice(&[0u8; 8]); // padding keeps reads past the end cheap
        BitReader { data, pos: 0, limit }
    }

    fn read(&mut self, numbits: usize) -> u32 {
        if numbits == 0 {
            return 0;
        }
        let start = self.pos;
        let end = start + numbits;
        let first = start >> 3;
        let last = (end + 7) >> 3;
        let mut chunk: u64 = 0;
        for i in first..last {
            chunk = (chunk << 8) | *self.data.get(i).unwrap_or(&0) as u64;
        }
        let shift = (last - first) * 8 - (start - (first << 3)) - numbits;
        self.pos = end;
        ((chunk >> shift) & ((1u64 << numbits) - 1)) as u32
    }

    fn overflowed(&self) -> bool {
        self.pos > self.limit
    }
}

/// Canonical huffman decoder matching libchdr's huffman.c.
pub(crate) struct Huffman {
    numcodes: usize,
    maxbits: usize,
    numbits: Vec<usize>,
    lookup: HashMap<(usize, u32), u32>,
}

impl Huffman {
    fn new(numcodes: usize, maxbits: usize) -> Self {
        Huffman {
            numcodes,
            maxbits,
            numbits: vec![0; numcodes],
            lookup: HashMap::new(),
        }
    }

    fn import_tree_rle(&mut self, bits: &mut BitReader) -> Result<()> {
        let width = if self.maxbits >= 16 {
            5
        } else if self.maxbits >= 8 {
            4
        } else {
            3
        };
        let mut cur = 0usize;
        while cur < self.numcodes {
            let nodebits = bits.read(width) as usize;
            if nodebits != 1 {
                // a non-one value is just raw
                self.numbits[cur] = nodebits;
                cur += 1;
                continue;
            }
            let nodebits = bits.read(width) as usize; // one is an escape code
            if nodebits == 1 {
                // a double one is a single one
                self.numbits[cur] = 1;
                cur += 1;
                continue;
            }
            let repeat = bits.read(width) as usize + 3;
            if cur + repeat > self.numcodes {
                bail!("bad huffman tree");
            }
            for _ in 0..repeat {
                self.numbits[cur] = nodebits;
                cur += 1;
            }
        }
        self.assign_canonical_codes()
    }

    fn assign_canonical_codes(&mut self) -> Result<()> {
        let mut histo = [0u32; 33];
        for &n in &self.numbits {
            if n > self.maxbits {
                bail!("huffman inconsistency");
            }
            histo[n] += 1;
        }

        let mut curstart: u32 = 0;
        for codelen in (1..=32usize).rev() {
            let nextstart = (curstart + histo[codelen]) >> 1;
            if codelen != 1 && nextstart * 2 != curstart + histo[codelen] {
                bail!("huffman inconsistency");
            }
            histo[codelen] = curstart;
            curstart = nextstart;
        }

        self.lookup.clear();
        for (code, &n) in self.numbits.iter().enumerate() {
            if n > 0 {
                self.lookup.insert((n, histo[n]), code as u32);
                histo[n] += 1;
            }
        }
        Ok(())
    }

    fn decode_one(&self, bits: &mut BitReader) -> Result<u32> {
        // codes are prefix-free, so the first length that matches is the answer
        let mut code: u32 = 0;
        for length in 1..=self.maxbits {
            code = (code << 1) | bits.read(1);
            if let Some(&hit) = self.lookup.get(&(length, code)) {
                return Ok(hit);
            }
        }
        bail!("bad huffman code")
    }
}

/// Raw inflate, tolerant of a truncated tail the way zlib's decompressobj is.
pub(crate) fn inflate_raw(src: &[u8], outlen: usize) -> Vec<u8> {
    let mut dec = flate2::read::DeflateDecoder::new(src);
    let mut out = vec![0u8; outlen];
    let mut filled = 0usize;
    while filled < outlen {
        match dec.read(&mut out[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(_) => break,
        }
    }
    out.truncate(filled);
    out
}

/// Re-derive the encoder's dictionary size, as libchdr's codec_lzma does.
///
/// The decoder can't read it back from the stream, so both sides compute it
/// from the hunk size. This mirrors LzmaEncProps_Normalize() and
/// LzmaEnc_WriteProperties() at level 6; a smaller value here would silently
/// corrupt output.
pub(crate) fn lzma_dict_size(reduce_size: u64) -> u32 {
    let mut dict_size: u64 = 1 << 26; // level 6 default
    if dict_size > reduce_size {
        dict_size = std::cmp::min(dict_size, std::cmp::max(reduce_size, 1 << 12));
    }

    if dict_size >= (1 << 21) {
        let mask: u64 = (1 << 20) - 1;
        return std::cmp::max((dict_size + mask) & !mask, dict_size) as u32;
    }

    let mut i: u32 = 22;
    loop {
        let aligned: u64 = (2 + (i as u64 & 1)) << (i >> 1);
        i += 1;
        if aligned >= dict_size {
            return aligned as u32;
        }
    }
}

/// Raw LZMA1. lzma-rs only speaks the LZMA_ALONE container, so the 13-byte
/// header libchdr omits is synthesised here: lc=3 lp=0 pb=2 is what chdman
/// encodes with, which packs into the single properties byte 0x5D.
pub(crate) fn lzma_raw(src: &[u8], outlen: usize, dict_size: u32) -> Result<Vec<u8>> {
    let mut stream = Vec::with_capacity(13 + src.len());
    stream.push(0x5Du8); // (pb*5 + lp)*9 + lc = (2*5+0)*9+3 = 93
    stream.extend_from_slice(&dict_size.to_le_bytes());
    stream.extend_from_slice(&(outlen as u64).to_le_bytes());
    stream.extend_from_slice(src);

    let mut out = Vec::with_capacity(outlen);
    lzma_rs::lzma_decompress(&mut Cursor::new(stream), &mut out)
        .map_err(|e| anyhow!("lzma: {e}"))?;
    Ok(out)
}

pub(crate) fn zstd_raw(src: &[u8], outlen: usize) -> Result<Vec<u8>> {
    let mut dec = ruzstd::decoding::StreamingDecoder::new(Cursor::new(src))
        .map_err(|e| anyhow!("zstd: {e}"))?;
    let mut out = Vec::with_capacity(outlen);
    dec.read_to_end(&mut out).map_err(|e| anyhow!("zstd: {e}"))?;
    Ok(out)
}

/// The cdzl/cdlz/cdzs framing from libchdr's cd_codec_decompress().
///
/// Sector data and subcode are compressed as two separate streams, and whether
/// each sector's sync header was stripped is flagged in a leading bitmap.
pub(crate) fn cd_decompress(
    src: &[u8],
    destlen: usize,
    base_decode: &dyn Fn(&[u8], usize) -> Result<Vec<u8>>,
) -> Result<Vec<u8>> {
    let frames = destlen / CD_FRAME_SIZE;
    let complen_bytes = if destlen < 65536 { 2 } else { 3 };
    let ecc_bytes = (frames + 7) / 8;
    let header_bytes = ecc_bytes + complen_bytes;
    if src.len() < header_bytes {
        bail!("truncated cd hunk");
    }

    let mut complen_base: usize = 0;
    for i in 0..complen_bytes {
        complen_base = (complen_base << 8) | src[ecc_bytes + i] as usize;
    }
    if src.len() < header_bytes + complen_base {
        bail!("truncated cd hunk");
    }

    let base = base_decode(
        &src[header_bytes..header_bytes + complen_base],
        frames * CD_SECTOR_DATA,
    )?;
    // never read for hashing, so a failure here is not fatal
    let mut sub = inflate_raw(&src[header_bytes + complen_base..], frames * CD_SUBCODE_DATA);
    sub.resize(frames * CD_SUBCODE_DATA, 0);

    let mut out = vec![0u8; destlen];
    for frame in 0..frames {
        let at = frame * CD_FRAME_SIZE;
        let bs = frame * CD_SECTOR_DATA;
        if bs + CD_SECTOR_DATA <= base.len() {
            out[at..at + CD_SECTOR_DATA].copy_from_slice(&base[bs..bs + CD_SECTOR_DATA]);
        }
        let ss = frame * CD_SUBCODE_DATA;
        out[at + CD_SECTOR_DATA..at + CD_FRAME_SIZE]
            .copy_from_slice(&sub[ss..ss + CD_SUBCODE_DATA]);
        if src[frame >> 3] & (1 << (frame & 7)) != 0 {
            // sync was stripped; restore it. libchdr also rebuilds the ECC/EDC
            // tail here, which no hash ever reads, so we don't.
            out[at..at + 12].copy_from_slice(&SYNC_PATTERN);
        }
    }
    Ok(out)
}

#[derive(Clone, Copy)]
pub(crate) enum Codec {
    None,
    Cdzl,
    Cdlz { dict_size: u32 },
    Cdzs,
    Zlib,
    Lzma { dict_size: u32 },
    Unsupported([u8; 4]),
}

/// Read-only CHD v5 hunk reader.
pub(crate) struct ChdFile {
    f: File,
    hunkbytes: usize,
    unitbytes: usize,
    metaoffset: u64,
    compressed: bool,
    map: Vec<(u32, u64, u64)>, // (type, length, offset)
    decoders: [Codec; 4],
    cache: Option<(usize, Vec<u8>)>,
}

impl ChdFile {
    fn open(path: &Path) -> Result<Self> {
        let mut f = File::open(path)?;
        let mut raw = [0u8; 124];
        let n = read_up_to(&mut f, &mut raw)?;
        if n < 124 || &raw[..8] != b"MComprHD" {
            bail!("not a CHD file");
        }

        let be = |a: usize, b: usize| -> u64 {
            let mut v: u64 = 0;
            for i in a..b {
                v = (v << 8) | raw[i] as u64;
            }
            v
        };

        let version = be(12, 16) as u32;
        if version != 5 {
            bail!("CHD v{version} is not supported (only v5)");
        }

        let compressors = [
            be(16, 20) as u32,
            be(20, 24) as u32,
            be(24, 28) as u32,
            be(28, 32) as u32,
        ];
        let logicalbytes = be(32, 40);
        let mapoffset = be(40, 48);
        let metaoffset = be(48, 56);
        let hunkbytes = be(56, 60) as usize;
        let unitbytes = be(60, 64) as usize;
        if hunkbytes == 0 || unitbytes == 0 {
            bail!("invalid CHD header");
        }

        let hunkcount = ((logicalbytes + hunkbytes as u64 - 1) / hunkbytes as u64) as usize;
        let compressed = compressors[0] != CHD_CODEC_NONE;

        let mut chd = ChdFile {
            f,
            hunkbytes,
            unitbytes,
            metaoffset,
            compressed,
            map: Vec::new(),
            decoders: [Codec::None; 4],
            cache: None,
        };
        chd.read_map(mapoffset, hunkcount)?;
        for i in 0..4 {
            chd.decoders[i] = chd.make_decoder(compressors[i]);
        }
        Ok(chd)
    }

    // -- map ------------------------------------------------------------

    fn read_map(&mut self, mapoffset: u64, hunkcount: usize) -> Result<()> {
        if !self.compressed {
            self.f.seek(SeekFrom::Start(mapoffset))?;
            let mut raw = vec![0u8; hunkcount * 4];
            read_up_to(&mut self.f, &mut raw)?;
            for i in 0..hunkcount {
                let unit = u32::from_be_bytes([
                    raw[i * 4],
                    raw[i * 4 + 1],
                    raw[i * 4 + 2],
                    raw[i * 4 + 3],
                ]) as u64;
                self.map
                    .push((C_NONE, self.hunkbytes as u64, unit * self.hunkbytes as u64));
            }
            return Ok(());
        }

        self.f.seek(SeekFrom::Start(mapoffset))?;
        let mut head = [0u8; 16];
        read_up_to(&mut self.f, &mut head)?;
        let mapbytes = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as usize;
        let mut firstoffs: u64 = 0;
        for i in 4..10 {
            firstoffs = (firstoffs << 8) | head[i] as u64;
        }
        let (lengthbits, selfbits, parentbits) =
            (head[12] as usize, head[13] as usize, head[14] as usize);
        if lengthbits.max(selfbits).max(parentbits) > 32 {
            bail!("invalid CHD map");
        }

        self.f.seek(SeekFrom::Start(mapoffset + 16))?;
        let mut mapbuf = vec![0u8; mapbytes];
        read_up_to(&mut self.f, &mut mapbuf)?;
        let mut bits = BitReader::new(mapbuf);

        // pass 1: the compression type of every hunk, huffman + RLE coded
        let mut decoder = Huffman::new(16, 8);
        decoder.import_tree_rle(&mut bits)?;

        let mut types = vec![0u32; hunkcount];
        let mut repcount: u32 = 0;
        let mut lastcomp: u32 = 0;
        for hunk in 0..hunkcount {
            if repcount > 0 {
                types[hunk] = lastcomp;
                repcount -= 1;
                continue;
            }
            let val = decoder.decode_one(&mut bits)?;
            if val == C_RLE_SMALL {
                types[hunk] = lastcomp;
                repcount = 2 + decoder.decode_one(&mut bits)?;
            } else if val == C_RLE_LARGE {
                types[hunk] = lastcomp;
                repcount = 2 + 16 + (decoder.decode_one(&mut bits)? << 4);
                repcount += decoder.decode_one(&mut bits)?;
            } else {
                lastcomp = val;
                types[hunk] = val;
            }
        }

        // pass 2: lengths and offsets, continuing in the same bitstream
        let mut curoffset = firstoffs;
        let mut last_self: u64 = 0;
        let mut last_parent: u64 = 0;
        let units_per_hunk = (self.hunkbytes / self.unitbytes) as u64;
        for hunk in 0..hunkcount {
            let mut kind = types[hunk];
            let mut offset = curoffset;
            let mut length: u64 = 0;

            if kind <= 3 {
                // one of the four codecs
                length = bits.read(lengthbits) as u64;
                curoffset += length;
                bits.read(16); // crc16, unused
            } else if kind == C_NONE {
                length = self.hunkbytes as u64;
                curoffset += length;
                bits.read(16);
            } else if kind == C_SELF {
                offset = bits.read(selfbits) as u64;
                last_self = offset;
            } else if kind == C_PARENT {
                offset = bits.read(parentbits) as u64;
                last_parent = offset;
            } else if kind == C_SELF_0 || kind == C_SELF_1 {
                if kind == C_SELF_1 {
                    last_self += 1;
                }
                kind = C_SELF;
                offset = last_self;
            } else if kind == C_PARENT_SELF {
                kind = C_PARENT;
                offset = (hunk as u64 * self.hunkbytes as u64) / self.unitbytes as u64;
                last_parent = offset;
            } else if kind == C_PARENT_0 || kind == C_PARENT_1 {
                if kind == C_PARENT_1 {
                    last_parent += units_per_hunk;
                }
                kind = C_PARENT;
                offset = last_parent;
            }

            self.map.push((kind, length, offset));
        }

        if bits.overflowed() {
            bail!("CHD map overran its buffer");
        }
        Ok(())
    }

    // -- codecs ---------------------------------------------------------

    fn make_decoder(&self, tag: u32) -> Codec {
        if tag == CHD_CODEC_NONE {
            return Codec::None;
        }
        if tag == tag4(b"cdzl") {
            return Codec::Cdzl;
        }
        if tag == tag4(b"cdlz") {
            let frames = self.hunkbytes / CD_FRAME_SIZE;
            return Codec::Cdlz {
                dict_size: lzma_dict_size((frames * CD_SECTOR_DATA) as u64),
            };
        }
        if tag == tag4(b"cdzs") {
            return Codec::Cdzs;
        }
        if tag == tag4(b"zlib") {
            return Codec::Zlib;
        }
        if tag == tag4(b"lzma") {
            return Codec::Lzma {
                dict_size: lzma_dict_size(self.hunkbytes as u64),
            };
        }
        Codec::Unsupported(tag.to_be_bytes())
    }

    fn decode(&self, codec: Codec, src: &[u8]) -> Result<Vec<u8>> {
        let hunkbytes = self.hunkbytes;
        match codec {
            Codec::None => bail!("hunk references an unconfigured codec"),
            Codec::Cdzl => cd_decompress(src, hunkbytes, &|d, n| Ok(inflate_raw(d, n))),
            Codec::Cdlz { dict_size } => {
                cd_decompress(src, hunkbytes, &move |d, n| lzma_raw(d, n, dict_size))
            }
            Codec::Cdzs => cd_decompress(src, hunkbytes, &|d, n| zstd_raw(d, n)),
            Codec::Zlib => Ok(inflate_raw(src, hunkbytes)),
            Codec::Lzma { dict_size } => lzma_raw(src, hunkbytes, dict_size),
            Codec::Unsupported(name) => {
                let name = String::from_utf8_lossy(&name).to_string();
                bail!("hunk uses the {name} codec, which isn't supported")
            }
        }
    }

    // -- reading --------------------------------------------------------

    fn read_hunk(&mut self, hunknum: usize) -> Result<Vec<u8>> {
        self.read_hunk_depth(hunknum, 0)
    }

    fn read_hunk_depth(&mut self, hunknum: usize, depth: u32) -> Result<Vec<u8>> {
        if let Some((n, data)) = &self.cache {
            if *n == hunknum {
                return Ok(data.clone());
            }
        }
        if hunknum >= self.map.len() {
            bail!("hunk out of range");
        }
        if depth > 8 {
            bail!("self-referencing hunk loop");
        }

        let (kind, length, offset) = self.map[hunknum];

        if kind == C_SELF {
            return self.read_hunk_depth(offset as usize, depth + 1);
        }
        if kind == C_PARENT {
            bail!("CHD needs its parent file, which isn't supported");
        }

        let data = if kind == C_NONE {
            if offset == 0 && !self.compressed {
                vec![0u8; self.hunkbytes]
            } else {
                self.f.seek(SeekFrom::Start(offset))?;
                let mut buf = vec![0u8; self.hunkbytes];
                let n = read_up_to(&mut self.f, &mut buf)?;
                for b in buf.iter_mut().skip(n) {
                    *b = 0;
                }
                buf
            }
        } else if kind <= 3 {
            let codec = self.decoders[kind as usize];
            self.f.seek(SeekFrom::Start(offset))?;
            let mut buf = vec![0u8; length as usize];
            let n = read_up_to(&mut self.f, &mut buf)?;
            buf.truncate(n);
            let data = self.decode(codec, &buf)?;
            if data.len() < self.hunkbytes {
                bail!("short hunk");
            }
            data
        } else {
            bail!("unknown hunk type {kind}");
        };

        self.cache = Some((hunknum, data.clone()));
        Ok(data)
    }

    fn get_metadata(&mut self, tag: u32, index: u32) -> Result<Option<Vec<u8>>> {
        let mut offset = self.metaoffset;
        let mut index = index;
        let mut seen = 0;
        while offset != 0 && seen < 65536 {
            seen += 1;
            self.f.seek(SeekFrom::Start(offset))?;
            let mut head = [0u8; 16];
            if read_up_to(&mut self.f, &mut head)? < 16 {
                return Ok(None);
            }
            let metatag = u32::from_be_bytes([head[0], head[1], head[2], head[3]]);
            let length =
                (u32::from_be_bytes([head[4], head[5], head[6], head[7]]) & 0x00FF_FFFF) as usize;
            let mut nxt: u64 = 0;
            for i in 8..16 {
                nxt = (nxt << 8) | head[i] as u64;
            }
            if metatag == tag {
                if index == 0 {
                    let mut buf = vec![0u8; length];
                    let n = read_up_to(&mut self.f, &mut buf)?;
                    buf.truncate(n);
                    return Ok(Some(buf));
                }
                index -= 1;
            }
            offset = nxt;
        }
        Ok(None)
    }
}

// CD track types, as named in CHD metadata
pub(crate) const CHD_TRACK_AUDIO: &str = "AUDIO";

pub(crate) struct ChdTrack {
    ttype: String,
    pregap: i64,
    start: i64,
}

/// CD image inside a CHD. Mirrors nextui's chd_reader.c.
pub(crate) struct ChdDisc {
    chd: ChdFile,
    frame_size: usize,
    frames_per_hunk: usize,
    track_start: i64,
    track_pregap: i64,
    is_data: bool,
    header_size: usize,
    data_size: usize,
}

impl ChdDisc {
    fn open(path: &Path, track: usize) -> Result<Self> {
        let mut chd = ChdFile::open(path)?;
        let frame_size = if chd.unitbytes != 0 {
            chd.unitbytes
        } else {
            CD_FRAME_SIZE
        };
        let frames_per_hunk = chd.hunkbytes / frame_size;
        if frames_per_hunk == 0 {
            bail!("CHD hunk is smaller than one CD frame");
        }

        let tracks = Self::read_tracks(&mut chd)?;
        if tracks.is_empty() {
            bail!("CHD has no CD track metadata");
        }

        let chosen = if track == 1 {
            // rcheevos asks for track 1; for a data hash we want the first
            // data track, which is the same thing on every PSX disc
            tracks
                .iter()
                .find(|t| t.ttype != CHD_TRACK_AUDIO)
                .unwrap_or(&tracks[0])
        } else {
            &tracks[std::cmp::min(track, tracks.len()) - 1]
        };

        let (header_size, data_size) = Self::sector_format(&chosen.ttype);
        Ok(ChdDisc {
            track_start: chosen.start,
            track_pregap: chosen.pregap,
            is_data: chosen.ttype != CHD_TRACK_AUDIO,
            header_size,
            data_size,
            frame_size,
            frames_per_hunk,
            chd,
        })
    }

    fn read_tracks(chd: &mut ChdFile) -> Result<Vec<ChdTrack>> {
        let re = regex::Regex::new(
            r"TRACK:(?P<track>\d+)\s+TYPE:(?P<type>\S+)\s+SUBTYPE:(?P<subtype>\S+)\s+FRAMES:(?P<frames>\d+)(?:\s+PREGAP:(?P<pregap>\d+))?",
        )?;
        let tags = [tag4(b"CHT2"), tag4(b"CHTR"), tag4(b"CHGT")];

        let mut tracks = Vec::new();
        let mut cumulative: i64 = 0;
        for index in 0..99u32 {
            let mut meta = None;
            for t in tags {
                if let Some(m) = chd.get_metadata(t, index)? {
                    meta = Some(m);
                    break;
                }
            }
            let Some(meta) = meta else { break };

            let head: Vec<u8> = meta.split(|&b| b == 0).next().unwrap_or(&[]).to_vec();
            let text = String::from_utf8_lossy(&head).to_string();
            let Some(c) = re.captures(&text) else { break };

            let frames: i64 = c["frames"].parse().unwrap_or(0);
            tracks.push(ChdTrack {
                ttype: c["type"].to_string(),
                pregap: c
                    .name("pregap")
                    .and_then(|m| m.as_str().parse().ok())
                    .unwrap_or(0),
                start: cumulative,
            });
            // CHD pads each track's allocation to a 4-frame boundary
            cumulative += frames + ((-frames).rem_euclid(4));
        }
        Ok(tracks)
    }

    fn sector_format(track_type: &str) -> (usize, usize) {
        match track_type {
            "MODE1_RAW" | "MODE1/2352" => (16, 2048), // sync(12) + header(4)
            "MODE2_RAW" | "MODE2/2352" | "MODE2_FORM_MIX" => (24, 2048), // + subheader(8)
            "MODE2_FORM1" | "MODE2_FORM2" => (8, 2048),
            CHD_TRACK_AUDIO => (0, 2352),
            _ => (0, 2048), // MODE1, MODE2: cooked
        }
    }
}

impl DiscImage for ChdDisc {
    fn read_sector(&mut self, sector: i64, length: usize) -> Result<Vec<u8>> {
        let mut frame = self.track_start + sector;
        // pregap frames are allocated in the CHD even when they're silence,
        // and FRAMES counts only what follows them
        if self.is_data && self.track_pregap != 0 {
            frame += self.track_pregap;
        }
        if frame < 0 {
            return Ok(Vec::new());
        }

        let hunknum = (frame as usize) / self.frames_per_hunk;
        let within = (frame as usize) % self.frames_per_hunk;
        let hunk = self.chd.read_hunk(hunknum)?;
        let at = within * self.frame_size;

        let (mut header_size, mut data_size) = (self.header_size, self.data_size);
        let end = std::cmp::min(at + self.frame_size, hunk.len());
        if at >= hunk.len() {
            return Ok(Vec::new());
        }
        let mut src: &[u8] = &hunk[at..end];
        if src.len() < 16 {
            return Ok(Vec::new());
        }

        // confirm the layout per sector; discs with mixed modes do exist
        if self.frame_size >= 2352 && header_size > 0 {
            if src[0] == 0x00 && src[1] == 0xFF && src[11] == 0x00 {
                let mode = src[15];
                if mode == 1 || mode == 2 {
                    let f = if mode == 1 { (16, 2048) } else { (24, 2048) };
                    header_size = f.0;
                    data_size = f.1;
                }
            } else if self.frame_size == 2448
                && src.len() > 107
                && src[96] == 0x00
                && src[97] == 0xFF
                && src[107] == 0x00
            {
                let mode = src[96 + 15];
                src = &src[96..]; // subcode-first layout
                if mode == 1 || mode == 2 {
                    let f = if mode == 1 { (16, 2048) } else { (24, 2048) };
                    header_size = f.0;
                    data_size = f.1;
                }
            }
        }

        if header_size >= src.len() {
            return Ok(Vec::new());
        }
        let take = std::cmp::min(length, data_size);
        let stop = std::cmp::min(header_size + take, src.len());
        Ok(src[header_size..stop].to_vec())
    }
}

// ===========================================================================
// disc-based hashes
// ===========================================================================

/// Pull the first FILE out of a .cue sheet.
pub(crate) fn cue_first_bin(cue: &Path) -> Option<PathBuf> {
    let bytes = std::fs::read(cue).ok()?;
    let text = String::from_utf8_lossy(&bytes).to_string();

    let quoted = regex::Regex::new(r#"FILE\s+"([^"]+)""#).ok()?;
    let bare = regex::Regex::new(r"FILE\s+(\S+)").ok()?;
    let name = quoted
        .captures(&text)
        .or_else(|| bare.captures(&text))
        .map(|c| c[1].to_string())?;

    let parent = cue.parent()?;
    let target = parent.join(&name);
    if target.exists() {
        return Some(target);
    }
    // cue sheets are often written on another OS with different casing
    for entry in std::fs::read_dir(parent).ok()?.flatten() {
        if file_name(&entry.path()).to_lowercase() == name.to_lowercase() {
            return Some(entry.path());
        }
    }
    None
}

/// Open a disc image. Returns Err if a CHD can't be read, Ok(None) if the
/// image simply isn't one we recognise.
pub(crate) fn open_disc(path: &Path) -> Result<Option<Box<dyn DiscImage>>> {
    let ext = ext_of(path);
    if ext == ".chd" {
        return Ok(Some(Box::new(ChdDisc::open(path, 1)?)));
    }

    let mut path = path.to_path_buf();
    if ext == ".cue" {
        match cue_first_bin(&path) {
            Some(p) => path = p,
            None => return Ok(None),
        }
    }

    let disc = Disc::open(&path)?;
    if !disc.valid() {
        return Ok(None);
    }
    Ok(Some(Box::new(disc)))
}

pub(crate) fn hash_psx(path: &Path) -> Result<Option<String>> {
    let Some(mut disc) = open_disc(path)? else {
        return Ok(None);
    };

    let mut exe_name = String::new();
    let mut found: Option<(i64, u32)> = None;

    if let Some(cnf) = disc.find_file("SYSTEM.CNF")? {
        let raw = disc.read_sector(cnf.0, 2047)?;
        let data: Vec<u8> = raw.split(|&b| b == 0).next().unwrap_or(&[]).to_vec();
        let text = String::from_utf8_lossy(&data).to_string();
        let splitter = regex::Regex::new(r"[\s;]")?;
        for line in text.lines() {
            if !line.trim_start().starts_with("BOOT") {
                continue;
            }
            let Some((_, value)) = line.split_once('=') else {
                continue;
            };
            let mut value = value.trim().to_string();
            if value.to_lowercase().starts_with("cdrom:") {
                value = value[6..].to_string();
            }
            let value = value.trim_start_matches('\\').to_string();
            // strip the ";1" ISO9660 version suffix and anything after it
            exe_name = splitter.split(&value).next().unwrap_or("").to_string();
            found = disc.find_file(&exe_name)?;
            break;
        }
    }

    if found.is_none() {
        found = disc.find_file("PSX.EXE")?;
        if found.is_some() {
            exe_name = "PSX.EXE".to_string();
        }
    }
    let Some((sector, mut size)) = found else {
        return Ok(None);
    };

    let head = disc.read_sector(sector, 32)?;
    if head.len() < 32 {
        return Ok(None);
    }
    if &head[..7] == b"PS-X EX" {
        // the PS-X EXE header states the payload size 28 bytes in, not
        // counting the 2048-byte header itself - which we do want hashed
        size = u32::from_le_bytes([head[28], head[29], head[30], head[31]]) + 2048;
    }

    // a few games share one engine and differ only in data files, so the
    // boot file name (their serial) goes into the hash too
    let name_bytes: Vec<u8> = exe_name
        .chars()
        .map(|c| if c.is_ascii() { c as u8 } else { b'?' })
        .collect();
    let body = disc.read_file(sector, size)?;
    Ok(Some(md5_of(&[&name_bytes, &body])))
}

pub(crate) fn hash_segacd(path: &Path) -> Result<Option<String>> {
    let Some(mut disc) = open_disc(path)? else {
        return Ok(None);
    };
    let header = disc.read_sector(0, 512)?;
    if header.len() < 512 {
        return Ok(None);
    }
    if &header[..16] != b"SEGADISCSYSTEM  " && &header[..16] != b"SEGA SEGASATURN " {
        return Ok(None);
    }
    Ok(Some(md5_of(&[&header])))
}


/// The first data track of a cue sheet (rcheevos' RC_HASH_CDTRACK_FIRST_DATA): its file, where it starts in that
/// file and its sector layout. A PC Engine CD opens with an audio track, so the first FILE won't do.
pub(crate) fn cue_first_data_track(cue: &Path) -> Result<Option<Disc>> {
    let text = String::from_utf8_lossy(&std::fs::read(cue)?).to_string();
    let parent = cue.parent().unwrap_or(Path::new("."));
    let mut file: Option<String> = None;
    let mut mode: Option<String> = None;
    for line in text.lines() {
        let line = line.trim();
        let word = line.split_whitespace().next().unwrap_or("").to_ascii_uppercase();
        match word.as_str() {
            "FILE" => {
                let rest = line[4..].trim();
                let name = match rest.strip_prefix('"') {
                    Some(q) => q.split('"').next().unwrap_or(""),
                    None => rest.split_whitespace().next().unwrap_or(""),
                };
                file = Some(name.to_string());
                mode = None;
            }
            "TRACK" => mode = line.split_whitespace().nth(2).map(|m| m.to_ascii_uppercase()),
            "INDEX" if line.split_whitespace().nth(1) == Some("01") => {
                let Some(m) = mode.as_deref().filter(|m| *m != "AUDIO") else { continue };
                let (sector_size, header) = match m {
                    "MODE1/2048" => (2048, 0),
                    "MODE2/2336" => (2336, 8),
                    "MODE2/2352" => (2352, 24),
                    _ => (2352, 16), // MODE1/2352
                };
                let msf: Vec<i64> = line.split_whitespace().nth(2).unwrap_or("").split(':').filter_map(|x| x.parse().ok()).collect();
                let frames = if msf.len() == 3 { (msf[0] * 60 + msf[1]) * 75 + msf[2] } else { 0 };
                let Some(name) = file.as_deref() else { return Ok(None) };
                let mut target = parent.join(name);
                if !target.exists() {
                    // cue sheets are often written on another OS with different casing
                    let lower = name.to_lowercase();
                    match std::fs::read_dir(parent)?.flatten().find(|e| e.file_name().to_string_lossy().to_lowercase() == lower) {
                        Some(e) => target = e.path(),
                        None => return Ok(None),
                    }
                }
                return Ok(Some(Disc::track(&target, frames * sector_size, sector_size, header)?));
            }
            _ => {}
        }
    }
    Ok(None)
}

/// rcheevos rc_hash_pce_cd: on the first data track, sector 1 holds the boot header ("PC Engine CD-ROM SYSTEM"
/// 32 bytes in); the hash is the 22-byte title at its end plus the boot program it points at. GameExpress discs
/// are plain ISO9660 instead, and hash their BOOT.BIN.
pub(crate) fn hash_pce_cd(path: &Path) -> Result<Option<String>> {
    let mut disc: Box<dyn DiscImage> = match ext_of(path).as_str() {
        ".chd" => Box::new(ChdDisc::open(path, 1)?), // track 1 resolves to the first data track
        ".cue" => match cue_first_data_track(path)? {
            Some(d) => Box::new(d),
            None => return Ok(None),
        },
        _ => return Ok(None), // .ccd / .m3u: rcheevos can't hash them as PC Engine CD either
    };
    let first = disc.first_sector();
    let head = disc.read_sector(first + 1, 128)?;
    if head.len() >= 128 && &head[32..55] == b"PC Engine CD-ROM SYSTEM" {
        let sector = ((head[0] as i64) << 16) | ((head[1] as i64) << 8) | head[2] as i64;
        let mut md5 = Md5::new();
        md5.update(&head[106..128]);
        for i in 0..head[3] as i64 {
            md5.update(&disc.read_sector(first + sector + i, 2048)?);
        }
        return Ok(Some(hex(&md5.finalize())));
    }
    if let Some((sector, size)) = disc.find_file("BOOT.BIN")? {
        if (size as u64) < MAX_BUFFER_SIZE {
            return Ok(Some(md5_of(&[&disc.read_file(sector, size)?])));
        }
    }
    Ok(None)
}

/// A zipped game bigger than this isn't unpacked to be hashed.
pub(crate) const MAX_UNZIP: u64 = 96 * 1024 * 1024;

/// The first file in a zip with a game's extension, unpacked into `dir`: what
/// minarch does for a core without zip support (most of them) before it
/// hashes. Only the plain zip minarch reads: stored or deflated, no ZIP64.
/// None when there's no such file.
pub(crate) fn unzip_first_rom(zip: &Path, dir: &Path, exts: &[&str]) -> Result<Option<PathBuf>> {
    let le16 = |b: &[u8], at: usize| u16::from_le_bytes([b[at], b[at + 1]]) as usize;
    let le32 = |b: &[u8], at: usize| u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]) as u64;
    let mut f = File::open(zip)?;
    let size = f.metadata()?.len();
    // the end-of-central-directory record, within the last 64 KiB + 22
    let tail_len = size.min(65536 + 22);
    f.seek(SeekFrom::Start(size - tail_len))?;
    let mut tail = vec![0u8; tail_len as usize];
    f.read_exact(&mut tail)?;
    let Some(eocd) = (0..tail.len().saturating_sub(21)).rev().find(|&i| tail[i..i + 4] == [0x50, 0x4b, 0x05, 0x06])
    else {
        bail!("not a zip");
    };
    let entries = le16(&tail, eocd + 10);
    let (cd_size, cd_offset) = (le32(&tail, eocd + 12), le32(&tail, eocd + 16));
    if cd_size > 64 * 1024 * 1024 || cd_offset + cd_size > size {
        bail!("zip directory out of range");
    }
    f.seek(SeekFrom::Start(cd_offset))?;
    let mut cd = vec![0u8; cd_size as usize];
    f.read_exact(&mut cd)?;
    let mut at = 0;
    for _ in 0..entries {
        if at + 46 > cd.len() || cd[at..at + 4] != [0x50, 0x4b, 0x01, 0x02] {
            bail!("bad zip directory");
        }
        let method = le16(&cd, at + 10);
        let (csize, usize_) = (le32(&cd, at + 20), le32(&cd, at + 24));
        let (n, e, c) = (le16(&cd, at + 28), le16(&cd, at + 30), le16(&cd, at + 32));
        let local = le32(&cd, at + 42);
        let name = String::from_utf8_lossy(&cd[at + 46..(at + 46 + n).min(cd.len())]).into_owned();
        at += 46 + n + e + c;
        let ext = Path::new(&name)
            .extension()
            .map(|x| format!(".{}", x.to_string_lossy().to_lowercase()))
            .unwrap_or_default();
        if name.ends_with('/') || IGNORED_EXTS.contains(&ext.as_str()) || ext == ".zip" {
            continue;
        }
        // as minarch: only a file the core can load (when its extensions are known)
        if !exts.is_empty() && !exts.iter().any(|e| ext.get(1..) == Some(*e)) {
            continue;
        }
        // the first game file decides, as in minarch
        if csize == 0xFFFF_FFFF || usize_ == 0xFFFF_FFFF || usize_ > MAX_UNZIP || (method != 0 && method != 8) {
            return Ok(None);
        }
        let mut head = [0u8; 30];
        f.seek(SeekFrom::Start(local))?;
        f.read_exact(&mut head)?;
        if head[..4] != [0x50, 0x4b, 0x03, 0x04] {
            bail!("bad zip entry");
        }
        f.seek(SeekFrom::Start(local + 30 + le16(&head, 26) as u64 + le16(&head, 28) as u64))?;
        let data = (&mut f).take(csize);
        let base = Path::new(&name).file_name().map(|b| b.to_os_string()).unwrap_or_else(|| "rom".into());
        let out_path = dir.join(base);
        let mut out = File::create(&out_path)?;
        let written = if method == 8 {
            std::io::copy(&mut flate2::read::DeflateDecoder::new(data).take(MAX_UNZIP), &mut out)?
        } else {
            std::io::copy(&mut data.take(MAX_UNZIP), &mut out)?
        };
        if written != usize_ {
            bail!("zip entry is {written} bytes, the zip says {usize_}");
        }
        return Ok(Some(out_path));
    }
    Ok(None)
}

/// The hash minarch computes for this file of the system `tag`. A zip is the game inside it, unpacked as
/// minarch unpacks it, except for arcade (the zip IS the romset and only its name counts) and for a core that
/// loads zips itself (FUSE, CPC, VICE: minarch hands it the zip, so the zip is hashed as it is).
pub(crate) fn hash_rom(tag: &str, method: Method, path: &Path) -> Result<Option<String>> {
    let method = method_for_file(method, path);
    let is_zip = path
        .extension()
        .map(|e| e.eq_ignore_ascii_case("zip"))
        .unwrap_or(false);
    let exts = core_exts(tag);
    if !is_zip || method == Method::Arcade || exts.contains(&"zip") {
        return hash_file(method, path);
    }
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "ra_fetch_{}_{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir)?;
    let result = match unzip_first_rom(path, &dir, &exts) {
        Ok(Some(rom)) => hash_file(method, &rom),
        Ok(None) => Ok(None),
        Err(e) => Err(e),
    };
    let _ = std::fs::remove_dir_all(&dir);
    result
}

pub(crate) fn hash_file(method: Method, path: &Path) -> Result<Option<String>> {
    match method {
        Method::Whole => hash_whole(path),
        Method::Nes => hash_nes(path),
        Method::Snes => hash_snes(path),
        Method::Lynx => hash_lynx(path),
        Method::Pce => hash_pce(path),
        Method::A7800 => hash_7800(path),
        Method::N64 => hash_n64(path),
        Method::Psx => hash_psx(path),
        Method::SegaCd => hash_segacd(path),
        Method::Arcade => hash_arcade(path),
        Method::PceCd => hash_pce_cd(path),
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    /// rcheevos' own rc_hash_arcade (built from minarch's copy) gave these.
    #[test]
    fn arcade_hash_matches_rcheevos() {
        let cases = [
            ("Arcade (KMMAME)/sf2.zip", "a7946d9b269b67f7a6b1148d3f5d8155"),
            ("Neo Geo (FBNEOGEO)/kof98.zip", "c0959b94e46f1811952055be7f59cba6"),
            ("nes/smb.zip", "bb23ace7414f08c52f38e2565a06148e"),
            ("NES/smb.zip", "bb23ace7414f08c52f38e2565a06148e"),
            ("NeoCD/aof.zip", "60acac6374faf41422a2638d5b7912e3"),
            ("Genesis/a.b.zip", "4b8d948f1a1c9b838af6de14f8d86f1e"),
            ("msx1/king.zip", "32b3d68c1086b70d59d410e7fc27c146"),
            ("abcdefghijklmno/x.zip", "9dd4e461268c8034f5c8564e155c67a6"),
            ("averyveryverylongfolder/x.zip", "9dd4e461268c8034f5c8564e155c67a6"),
            ("Arcade (MAME)/mslug", "b43c8b4ec999588c04dad79bb8bcc745"),
        ];
        for (path, want) in cases {
            let got = hash_arcade(&Path::new("/card/Roms").join(path)).unwrap().unwrap();
            assert_eq!(got, want, "{path}");
        }
    }

    /// A zip as zip tools write one: local headers, then the central directory.
    fn make_zip(entries: &[(&str, &[u8], bool)]) -> Vec<u8> {
        use std::io::Write;
        let (mut out, mut cd) = (Vec::new(), Vec::new());
        for (name, data, deflate) in entries {
            let body = if *deflate {
                let mut e = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
                e.write_all(data).unwrap();
                e.finish().unwrap()
            } else {
                data.to_vec()
            };
            let method: u16 = if *deflate { 8 } else { 0 };
            let at = out.len() as u32;
            let mut h = Vec::new();
            h.extend_from_slice(&[0x50, 0x4b, 0x03, 0x04, 20, 0, 0, 0]);
            h.extend_from_slice(&method.to_le_bytes());
            h.extend_from_slice(&[0; 8]); // time, date, crc (not checked)
            h.extend_from_slice(&(body.len() as u32).to_le_bytes());
            h.extend_from_slice(&(data.len() as u32).to_le_bytes());
            h.extend_from_slice(&(name.len() as u16).to_le_bytes());
            h.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&h);
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(&body);
            cd.extend_from_slice(&[0x50, 0x4b, 0x01, 0x02, 20, 0, 20, 0, 0, 0]);
            cd.extend_from_slice(&method.to_le_bytes());
            cd.extend_from_slice(&[0; 8]);
            cd.extend_from_slice(&(body.len() as u32).to_le_bytes());
            cd.extend_from_slice(&(data.len() as u32).to_le_bytes());
            cd.extend_from_slice(&(name.len() as u16).to_le_bytes());
            cd.extend_from_slice(&[0; 12]); // extra, comment, disk, attributes
            cd.extend_from_slice(&at.to_le_bytes());
            cd.extend_from_slice(name.as_bytes());
        }
        let cd_at = out.len() as u32;
        out.extend_from_slice(&cd);
        out.extend_from_slice(&[0x50, 0x4b, 0x05, 0x06, 0, 0, 0, 0]);
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(cd.len() as u32).to_le_bytes());
        out.extend_from_slice(&cd_at.to_le_bytes());
        out.extend_from_slice(&[0, 0]);
        out
    }

    #[test]
    fn zipped_games_hash_the_game_inside_but_arcade_hashes_the_name() {
        let dir = std::env::temp_dir().join(format!("ra_fetch_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let game: Vec<u8> = (0..70_000u32).map(|i| (i * 7 % 251) as u8).collect();
        let want = md5_of(&[&game]);
        for deflate in [false, true] {
            let zip = dir.join(format!("Tetris{}.zip", deflate as u8));
            std::fs::write(&zip, make_zip(&[("readme.txt", b"hi", false), ("Tetris.gb", &game, deflate)])).unwrap();
            assert_eq!(hash_rom("GB", Method::Whole, &zip).unwrap().unwrap(), want, "deflate {deflate}");
            // the same zip in an arcade folder is a romset: its name
            assert_eq!(
                hash_rom("MAME", Method::Arcade, &zip).unwrap().unwrap(),
                md5_of(&[format!("Tetris{}", deflate as u8).as_bytes()])
            );
        }
        // a .md inside is a Mega Drive game, not something to skip
        let zip = dir.join("Sonic.zip");
        std::fs::write(&zip, make_zip(&[("Sonic.md", &game, true)])).unwrap();
        assert_eq!(hash_rom("MD", Method::Whole, &zip).unwrap().unwrap(), want);
        // ...but not a Game Boy one: minarch only unpacks a file the core loads
        assert_eq!(hash_rom("GB", Method::Whole, &zip).unwrap(), None);
        // the core's extensions pick the file, whatever comes first
        let zip = dir.join("Pick.zip");
        std::fs::write(&zip, make_zip(&[("manual.pdf", b"pdf", false), ("Pick.gb", &game, true)])).unwrap();
        assert_eq!(hash_rom("GB", Method::Whole, &zip).unwrap().unwrap(), want);
        // a core that loads zips itself (FUSE) is handed the zip, so the zip is what minarch hashes
        let bytes = std::fs::read(&zip).unwrap();
        assert_eq!(hash_rom("FUSE", Method::Whole, &zip).unwrap().unwrap(), md5_of(&[&bytes]));
        // nothing but a readme: no hash rather than a wrong one
        let zip = dir.join("Empty.zip");
        std::fs::write(&zip, make_zip(&[("readme.txt", b"hi", false)])).unwrap();
        assert_eq!(hash_rom("GB", Method::Whole, &zip).unwrap(), None);
        // the temporary folders are gone again
        let left = std::fs::read_dir(std::env::temp_dir())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(&format!("ra_fetch_{}_", std::process::id())))
            .count();
        assert_eq!(left, 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }


    /// Builds the cdzs framing libchdr produces - the ecc bitmap, the
    /// big-endian compressed length, a zstd sector stream and a raw-deflate
    /// subcode stream - and checks `cd_decompress` puts the hunk back
    /// together. No card here carries a zstd CHD (chdman only picks that
    /// codec when asked), so this is the only coverage the path gets.
    #[test]
    fn cdzs_round_trip() {
        const FRAMES: usize = 4;
        let destlen = FRAMES * CD_FRAME_SIZE;

        // a recognisable hunk: real sector headers, varied payload
        let mut want = vec![0u8; destlen];
        for f in 0..FRAMES {
            let at = f * CD_FRAME_SIZE;
            want[at..at + 12].copy_from_slice(&SYNC_PATTERN);
            want[at + 12] = f as u8;
            for i in 16..CD_SECTOR_DATA {
                want[at + i] = ((f * 31 + i * 7) % 251) as u8;
            }
            for i in 0..CD_SUBCODE_DATA {
                want[at + CD_SECTOR_DATA + i] = ((f + i) % 97) as u8;
            }
        }

        // split into the two streams exactly as the encoder would, with every
        // sector's sync stripped and flagged
        let mut sectors = Vec::with_capacity(FRAMES * CD_SECTOR_DATA);
        let mut subcode = Vec::with_capacity(FRAMES * CD_SUBCODE_DATA);
        for f in 0..FRAMES {
            let at = f * CD_FRAME_SIZE;
            let mut sector = want[at..at + CD_SECTOR_DATA].to_vec();
            sector[..12].fill(0); // sync removed, bitmap says to restore it
            sectors.extend_from_slice(&sector);
            subcode.extend_from_slice(&want[at + CD_SECTOR_DATA..at + CD_FRAME_SIZE]);
        }

        let zs = zstd::encode_all(&sectors[..], 3).expect("zstd encode");
        let mut enc = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut enc, &subcode).unwrap();
        let sub = enc.finish().unwrap();

        let ecc_bytes = FRAMES.div_ceil(8);
        let complen_bytes = if destlen < 65536 { 2 } else { 3 };
        let mut src = vec![0xFFu8; ecc_bytes]; // every sector had its sync stripped
        let len = zs.len();
        for i in (0..complen_bytes).rev() {
            src.push(((len >> (8 * i)) & 0xFF) as u8);
        }
        src.extend_from_slice(&zs);
        src.extend_from_slice(&sub);

        let got = cd_decompress(&src, destlen, &|d, n| zstd_raw(d, n)).expect("cdzs decode");
        assert_eq!(got.len(), destlen);
        assert_eq!(got, want, "cdzs hunk did not round-trip");
    }

    /// The same framing with zlib, which real cards do use - a control that
    /// proves the harness above matches how cdzl hunks are really laid out.
    #[test]
    fn cdzl_round_trip_control() {
        const FRAMES: usize = 4;
        let destlen = FRAMES * CD_FRAME_SIZE;
        let mut want = vec![0u8; destlen];
        for f in 0..FRAMES {
            let at = f * CD_FRAME_SIZE;
            want[at..at + 12].copy_from_slice(&SYNC_PATTERN);
            for i in 16..CD_SECTOR_DATA {
                want[at + i] = ((f * 17 + i * 3) % 241) as u8;
            }
        }
        let mut sectors = Vec::new();
        let mut subcode = Vec::new();
        for f in 0..FRAMES {
            let at = f * CD_FRAME_SIZE;
            let mut sector = want[at..at + CD_SECTOR_DATA].to_vec();
            sector[..12].fill(0);
            sectors.extend_from_slice(&sector);
            subcode.extend_from_slice(&want[at + CD_SECTOR_DATA..at + CD_FRAME_SIZE]);
        }
        let mut e1 = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut e1, &sectors).unwrap();
        let zs = e1.finish().unwrap();
        let mut e2 = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut e2, &subcode).unwrap();
        let sub = e2.finish().unwrap();

        let ecc_bytes = FRAMES.div_ceil(8);
        let mut src = vec![0xFFu8; ecc_bytes];
        for i in (0..2).rev() {
            src.push(((zs.len() >> (8 * i)) & 0xFF) as u8);
        }
        src.extend_from_slice(&zs);
        src.extend_from_slice(&sub);

        let got = cd_decompress(&src, destlen, &|d, n| Ok(inflate_raw(d, n))).expect("cdzl decode");
        assert_eq!(got, want);
    }

    /// Guards the dictionary size libchdr leaves out of the stream and both
    /// sides have to re-derive. Values taken from the Python implementation,
    /// which is itself a port of LzmaEncProps_Normalize at level 6.
    #[test]
    fn lzma_dict_size_matches_level_6() {
        assert_eq!(lzma_dict_size(4 * CD_SECTOR_DATA as u64), 12288);
        assert_eq!(lzma_dict_size(1 << 26), 1 << 26);
        assert_eq!(lzma_dict_size(46_061_568), 46_137_344);
        assert_eq!(lzma_dict_size(4096), 4096);
        assert_eq!(lzma_dict_size(1 << 21), 1 << 21);
    }
}

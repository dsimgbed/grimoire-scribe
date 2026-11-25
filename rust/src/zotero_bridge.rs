//! The Grimoire × Zotero bridge (docs/bridge-contract.md, `schema_version: 1`). This is the Rust
//! side of the on-device handoff: a small directory of bounded JSON documents that the trampoline
//! and zotero-for-kindle also read and write. Nothing here opens a socket, runs a command, reads a
//! Zotero credential file, or follows a link. If the bridge directory is absent every call is a
//! no-op (an empty read, a refused write): Grimoire stays the notebook it was.
//!
//! The files, all under `$GZ_BRIDGE_DIR` (default `/mnt/us/grimoire_zotero_bridge`):
//!
//!   handoff.json        one-shot workspace switch, consumed by the target at startup
//!   return_context.json the essay to come back to, written by zotero-for-kindle
//!   library_index.json  a read-only catalog snapshot exported by zotero-for-kindle
//!   inbox/<epoch>-<nonce>.json  one citation per file, consumed and unlinked here
//!   bridge.log          short, non-sensitive diagnostics
//!
//! The fourth file Grimoire owns is a notebook sidecar, `notebooks/<id>/zoterolinks.ini`: the
//! Zotero references a page cites, with their page areas. `zotero_references` groups them into the
//! reference nodes the Index/Graph draws; `zotero_target_*` makes and reads the `z:` nav target a
//! tap on one of those nodes carries.
//!
//! Single-threaded, like the rest of the app: the pending inbox is a thread-local, not a mutex.

use crate::prelude::*;

use std::cell::RefCell;
use std::io::Read;
use std::os::unix::fs::MetadataExt;

// ---- names, sizes and exit codes (the contract's numbers) ----

/// The one schema this build speaks.
pub const SCHEMA_VERSION: i32 = 1;
pub const DEFAULT_BRIDGE_DIR: &str = "/mnt/us/grimoire_zotero_bridge";
pub const BRIDGE_DIR_ENV: &str = "GZ_BRIDGE_DIR";
pub const HANDOFF_FILE: &str = "handoff.json";
pub const RETURN_CONTEXT_FILE: &str = "return_context.json";
pub const LIBRARY_INDEX_FILE: &str = "library_index.json";
pub const INBOX_DIR: &str = "inbox";
pub const BRIDGE_LOG_FILE: &str = "bridge.log";
pub const LEGACY_INBOX_FILE: &str = "citation_inbox.json";
/// handoff.json, return_context.json and each inbox event.
pub const BRIDGE_JSON_MAX: usize = 256 * 1024;
pub const LIBRARY_INDEX_MAX: usize = 10 * 1024 * 1024;
/// bridge.log is diagnostic only; it is never allowed to grow without bound.
pub const BRIDGE_LOG_MAX: usize = 64 * 1024;
/// A diagnostic line, after the prefix.
pub const BRIDGE_LOG_LINE_MAX: usize = 200;
/// How long a handoff stays fresh (seconds). Older payloads are stale and are discarded.
pub const HANDOFF_MAX_AGE_SECS: i64 = 30 * 24 * 60 * 60;

pub const EXIT_CLEAN: i32 = 0;
/// Grimoire → zotero-for-kindle, after the notebook is saved and the display/input are closed.
pub const EXIT_HANDOFF_TO_ZOTERO: i32 = 42;
/// zotero-for-kindle → Grimoire (written here for completeness; Zotero produces it).
pub const EXIT_HANDOFF_TO_GRIMOIRE: i32 = 43;
/// Zotero's self-update restart (also produced by Zotero).
pub const EXIT_ZOTERO_RESTART: i32 = 75;

pub const TARGET_ZOTERO: &str = "zotero";
pub const TARGET_GRIMOIRE: &str = "grimoire";
pub const ACTION_OPEN_ITEM: &str = "open_item";
pub const ACTION_RETURN_ESSAY: &str = "return_essay";
/// A Zotero item's nav target in Grimoire's links.ini / the Graph.
pub const ZOTERO_TARGET_PREFIX: &str = "z:";
/// The largest Zotero page number the contract allows (1-based).
pub const ZOTERO_PAGE_MAX: u32 = 1_000_000;

/// zoterolinks.ini, a backwards-compatible per-notebook sidecar (never part of links.ini).
pub const ZOTEROLINKS_FILE: &str = "zoterolinks.ini";
pub const ZOTEROLINKS_HEADER: &str = "# Grimoire: Zotero reference links (docs/bridge-contract.md)\n";
pub const ZOTEROLINKS_MAX: usize = 4000;
pub const ZOTEROLINKS_PER_PAGE: usize = 256;
pub const ZOTEROLINKS_FILE_MAX: usize = 1 << 20;
pub const ZOTEROLINKS_UNKNOWN_MAX: usize = 16 * 1024;
pub const ZREF_PREFIX: &str = "zref.";
/// The pseudo-notebook an anchor belongs to in the Graph's foreign column.
pub const ZOTERO_NOTEBOOK_KEY: &str = "zotero";

// ---- the bridge directory ----

static BRIDGE_DIR_OVERRIDE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Thread-safe test override for the bridge directory; `None` restores `GZ_BRIDGE_DIR` / default.
pub fn bridge_set_dir(dir: Option<&str>) {
    if let Ok(mut g) = BRIDGE_DIR_OVERRIDE.lock() {
        *g = dir.map(|s| s.trim_end_matches('/').to_string()).filter(|s| !s.is_empty());
    }
    library_index_invalidate_cache();
}

/// The directory both apps use: test override when set, `GZ_BRIDGE_DIR` when it names one,
/// the on-device default else. An empty override is the same as an unset one.
pub fn bridge_dir() -> String {
    if let Ok(g) = BRIDGE_DIR_OVERRIDE.lock() {
        if let Some(ref d) = *g {
            return d.clone();
        }
    }
    let from_env = std::env::var_os(BRIDGE_DIR_ENV)
        .map(|v| v.to_string_lossy().into_owned())
        .unwrap_or_default();
    let dir = if from_env.is_empty() { DEFAULT_BRIDGE_DIR } else { from_env.as_str() };
    dir.trim_end_matches('/').to_string()
}
/// Alias with a name that reads as the path it returns.
pub fn bridge_dir_path() -> String {
    bridge_dir()
}
/// The bridge directory as a `String` in `dir`, for callers that do not read the environment.
pub fn bridge_dir_in(dir: &str) -> String {
    dir.trim_end_matches('/').to_string()
}

/// Whether the bridge is there. A link at the name is not the bridge: it is refused rather than
/// followed, so an absent bridge and a planted link both leave Grimoire standalone.
pub fn bridge_exists() -> bool {
    bridge_exists_in(&bridge_dir())
}
pub fn bridge_exists_in(dir: &str) -> bool {
    let d = bridge_dir_in(dir);
    std::fs::symlink_metadata(&d).is_ok_and(|m| !m.file_type().is_symlink() && m.is_dir())
}

/// A file of the bridge, by its fixed name. No caller-supplied component can traverse out of the
/// directory: only the names this module owns (and a validated inbox name) are accepted.
pub fn bridge_path(name: &str) -> Option<String> {
    bridge_path_in(&bridge_dir(), name)
}
pub fn bridge_path_in(dir: &str, name: &str) -> Option<String> {
    let known = matches!(name, HANDOFF_FILE | RETURN_CONTEXT_FILE | LIBRARY_INDEX_FILE | BRIDGE_LOG_FILE | LEGACY_INBOX_FILE);
    if !known || name.contains('/') || name.contains('\\') || name.contains('\0') {
        return None;
    }
    let d = bridge_dir_in(dir);
    if d.is_empty() || d.len() + 1 + name.len() >= 1024 {
        return None;
    }
    Some(format!("{d}/{name}"))
}
pub fn inbox_dir() -> String {
    format!("{}/{}", bridge_dir(), INBOX_DIR)
}
pub fn inbox_dir_in(dir: &str) -> String {
    format!("{}/{INBOX_DIR}", bridge_dir_in(dir))
}
/// The path of one inbox file, after its name passed `inbox_name_ok`.
pub fn inbox_path(name: &str) -> Option<String> {
    inbox_path_in(&bridge_dir(), name)
}
pub fn inbox_path_in(dir: &str, name: &str) -> Option<String> {
    if !inbox_name_ok(name) {
        return None;
    }
    let d = inbox_dir_in(dir);
    (d.len() + 1 + name.len() < 1024).then(|| format!("{d}/{name}"))
}

// ---- safe reads and atomic writes ----

/// Why a bridge read did not produce bytes. All of these are "no answer" to the bridge, never a
/// partial document; `Invalid` and `TooLarge` ask the caller to unlink the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BridgeReadError {
    /// nothing at the name
    Absent,
    IO,
    /// a link, a folder, or anything but a plain file
    NotFile,
    /// larger than the cap for its kind
    TooLarge,
    /// not UTF-8, or carrying a NUL
    Invalid,
}

/// A bridge document read strictly: plain file, no link, at most `cap` bytes, valid UTF-8, no NUL.
/// One byte past `cap` is read so a file that grew after its `fstat` is refused as `TooLarge`.
pub fn bridge_read(path: &str, cap: usize) -> Result<Option<Vec<u8>>, BridgeReadError> {
    let (f, md) = match open_read_nofollow(path) {
        Ok(x) => x,
        Err(e) => {
            let absent = e.kind() == std::io::ErrorKind::NotFound
                && std::fs::symlink_metadata(path).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound);
            return Err(if absent { BridgeReadError::Absent } else { BridgeReadError::NotFile });
        }
    };
    if !md.is_file() {
        return Err(BridgeReadError::NotFile);
    }
    if md.len() > cap as u64 {
        return Err(BridgeReadError::TooLarge);
    }
    let mut buf = Vec::new();
    if buf.try_reserve_exact(cap.min(md.len() as usize).saturating_add(1)).is_err() {
        return Err(BridgeReadError::IO);
    }
    if f.take(cap as u64 + 1).read_to_end(&mut buf).is_err() || buf.len() > cap {
        return Err(BridgeReadError::TooLarge);
    }
    if buf.contains(&0) || std::str::from_utf8(&buf).is_err() {
        return Err(BridgeReadError::Invalid);
    }
    Ok(Some(buf))
}

/// A short, fixed diagnostic. No field of any bridge document is ever passed here: the log must
/// stay safe to paste into a public issue.
pub fn bridge_log(message: &str) {
    let Some(path) = bridge_path(BRIDGE_LOG_FILE) else { return };
    bridge_log_in(&path, message);
}
fn bridge_log_in(path: &str, message: &str) {
    let dir = dirname(path);
    if !ensure_dir(&dir) {
        return;
    }
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink() || m.len() > BRIDGE_LOG_MAX as u64) {
        return;
    }
    let Ok(c) = std::ffi::CString::new(path) else { return };
    // SAFETY: open/close on a path we own and a descriptor the open returns; the bytes are a
    // bounded slice we own. O_NOFOLLOW refuses a link planted at the log's name.
    unsafe {
        let fd = libc::open(c.as_ptr(), libc::O_WRONLY | libc::O_CREAT | libc::O_APPEND | libc::O_NOFOLLOW | libc::O_CLOEXEC, 0o644);
        if fd < 0 {
            return;
        }
        let line = format!("{}\n", message.chars().take(BRIDGE_LOG_LINE_MAX).collect::<String>());
        let b = line.as_bytes();
        let mut left = b;
        while !left.is_empty() {
            let n = libc::write(fd, left.as_ptr() as *const libc::c_void, left.len());
            if n <= 0 {
                break;
            }
            left = &left[n as usize..];
        }
        libc::close(fd);
    }
}
/// A generic line for a document that failed validation. It names neither the document's contents
/// nor where in it the fault was.
pub fn bridge_log_invalid(name: &str) {
    bridge_log(&format!("bridge: removed invalid {name}"));
}

/// Whether every component of `dir` is a real directory and never a symlink; creates missing
/// trailing components safely. Matches C++ `checkComponents`.
pub(crate) fn ensure_dir(dir: &str) -> bool {
    let d = bridge_dir_in(dir);
    if d.is_empty() || d.len() >= 1000 || d.contains('\0') {
        return false;
    }
    let mut cur = if d.starts_with('/') { String::from("/") } else { String::new() };
    for part in d.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            return false;
        }
        if !cur.is_empty() && !cur.ends_with('/') {
            cur.push('/');
        }
        cur.push_str(part);
        match std::fs::symlink_metadata(&cur) {
            Ok(md) => {
                if md.file_type().is_symlink() || !md.is_dir() {
                    return false;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if std::fs::create_dir(&cur).is_err() {
                    // Race with concurrent creator: re-check below.
                }
                match std::fs::symlink_metadata(&cur) {
                    Ok(md) if !md.file_type().is_symlink() && md.is_dir() => {}
                    _ => return false,
                }
            }
            Err(_) => return false,
        }
    }
    !cur.is_empty()
}

/// Whether `dir` already exists on disk and every component of `dir` is a real non-symlink directory.
pub(crate) fn check_dir_existing(dir: &str) -> bool {
    let d = bridge_dir_in(dir);
    if d.is_empty() || d.len() >= 1000 || d.contains('\0') {
        return false;
    }
    let mut cur = if d.starts_with('/') { String::from("/") } else { String::new() };
    for part in d.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            return false;
        }
        if !cur.is_empty() && !cur.ends_with('/') {
            cur.push('/');
        }
        cur.push_str(part);
        match std::fs::symlink_metadata(&cur) {
            Ok(md) if !md.file_type().is_symlink() && md.is_dir() => {}
            _ => return false,
        }
    }
    !cur.is_empty()
}

pub(crate) static WRITE_NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Write a bridge JSON document atomically: `<path>.tmp.<pid>.<nonce>` with O_EXCL|O_NOFOLLOW,
/// fsync, rename, best-effort fsync of the parent directory. The target directory is made when
/// missing and must not itself be a link. Returns 0 on success.
pub fn bridge_write_atomic(path: &str, data: &[u8]) -> i32 {
    if path.contains('\0') || std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        return -1;
    }
    let dir = dirname(path);
    if !ensure_dir(&dir) {
        return -1;
    }
    let pid = std::process::id();
    let nonce = WRITE_NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let stamp = now_ms() as u64;
    let tmp = format!("{path}.tmp.{pid}.{:x}.{:x}", stamp ^ nonce, nonce);
    let Ok(ctmp) = std::ffi::CString::new(tmp.as_str()) else { return -1 };
    let Ok(cpath) = std::ffi::CString::new(path) else { return -1 };
    // SAFETY: every path is a CString we own, every descriptor is the one the open returns and is
    // closed here, and `data` is a slice whose length is passed to write.
    unsafe {
        let fd = libc::open(
            ctmp.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o644,
        );
        if fd < 0 {
            qlog!("bridge: temp open failed: %s", &strerror(errno()));
            return -1;
        }
        let mut left = data;
        while !left.is_empty() {
            let n = libc::write(fd, left.as_ptr() as *const libc::c_void, left.len());
            if n < 0 {
                if errno() == libc::EINTR {
                    continue;
                }
                qlog!("bridge: temp write failed: %s", &strerror(errno()));
                libc::close(fd);
                libc::unlink(ctmp.as_ptr());
                return -1;
            }
            left = &left[n as usize..];
        }
        work_pump();
        if libc::fsync(fd) != 0 {
            qlog!("bridge: temp fsync failed: %s", &strerror(errno()));
            libc::close(fd);
            libc::unlink(ctmp.as_ptr());
            return -1;
        }
        work_pump();
        if libc::close(fd) != 0 {
            libc::unlink(ctmp.as_ptr());
            return -1;
        }
        if libc::rename(ctmp.as_ptr(), cpath.as_ptr()) != 0 {
            qlog!("bridge: rename failed: %s", &strerror(errno()));
            libc::unlink(ctmp.as_ptr());
            return -1;
        }
        work_pump();
        if let Ok(cdir) = std::ffi::CString::new(dir.as_str()) {
            let dfd = libc::open(cdir.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
            if dfd >= 0 {
                // FAT/FUSE may answer EINVAL or EBADF here: the rename is already durable enough.
                let _ = libc::fsync(dfd);
                libc::close(dfd);
            }
        }
    }
    work_pump_mark();
    0
}

/// Remove a bridge file by its fixed name (a link is removed, never followed).
pub fn bridge_remove(name: &str) -> i32 {
    let Some(path) = bridge_path(name) else { return -1 };
    if std::fs::symlink_metadata(&path).is_err() {
        return 0;
    }
    (std::fs::remove_file(&path).is_ok()) as i32 - 1
}

// ---- strict field rules ----

fn key_ok(s: &str, max: usize) -> bool {
    !s.is_empty()
        && s.len() <= max
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}
/// `item_key` / `attachment_key`: 1..=32 of `[A-Za-z0-9_-]`.
pub fn item_key_valid(s: &str) -> bool {
    key_ok(s, 32)
}
pub fn attachment_key_valid(s: &str) -> bool {
    key_ok(s, 32)
}
/// `notebook_id`: 1..=63 of `[A-Za-z0-9_-]`. An empty id is allowed only where the contract says
/// "normal start" (a handoff targeting Grimoire), checked at that call site.
pub fn notebook_id_valid(s: &str) -> bool {
    key_ok(s, 63)
}
pub fn nonce_valid(s: &str) -> bool {
    key_ok(s, 64)
}
/// `id` of an inbox event: 1..=64 of `[A-Za-z0-9_-]`.
pub fn event_id_valid(s: &str) -> bool {
    key_ok(s, 64)
}
pub fn valid_page_id(_v: u32) -> bool {
    true // every u32 is a stable Grimoire page id; 0 means unspecified here
}
pub fn valid_page_index(_v: u32) -> bool {
    true
}
/// `page` (Zotero): 0 = unspecified, 1..=1000000 = a 1-based page number.
pub fn valid_zotero_page(v: u32) -> bool {
    v <= ZOTERO_PAGE_MAX
}
/// `locator`: bounded, no NUL and no ASCII control byte.
pub fn locator_valid(s: &str) -> bool {
    s.len() <= 512 && !s.bytes().any(|b| b == 0 || b < 0x20)
}
/// A `short_cite`, `citation_key`, `authors`, `year` or `item_type`: bounded, no NUL; controls are
/// not kept as controls.
pub fn bridge_text_valid(s: &str, max: usize) -> bool {
    s.len() <= max && !s.bytes().any(|b| b == 0 || b < 0x20)
}
pub fn title_valid(s: &str) -> bool {
    s.len() <= 512 && !s.bytes().any(|b| b == 0)
}
pub fn quote_valid(s: &str, max: usize) -> bool {
    s.len() <= max && !s.bytes().any(|b| b == 0)
}
/// Strip NULs, turn ASCII controls into spaces, collapse runs of spaces, and cut on a UTF-8
/// boundary. Used for every free-text field from a bridge document before it reaches a label.
pub fn clean_text(s: &str, max: usize) -> String {
    let mut out = String::with_capacity(s.len().min(max));
    let mut space = false;
    for ch in s.chars() {
        if ch == '\0' {
            continue;
        }
        if ch.is_control() {
            space = true;
            continue;
        }
        if ch.is_whitespace() {
            space = true;
            continue;
        }
        if space && !out.is_empty() {
            if out.len() + 1 + ch.len_utf8() > max {
                break;
            }
            out.push(' ');
        } else if out.len() + ch.len_utf8() > max {
            break;
        }
        space = false;
        out.push(ch);
    }
    out
}

// ---- a small JSON writer (no new crate) ----

/// A JSON string with the escapes the reader decodes, including control bytes and a quote.
pub fn json_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if c < '\u{20}' => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
/// A JSON string field, `name` included.
pub fn json_field(name: &str, value: &str) -> String {
    format!("{}:{}", json_quote(name), json_quote(value))
}
/// A JSON integer field.
pub fn json_int_field(name: &str, value: i64) -> String {
    format!("{}:{}", json_quote(name), value)
}
/// A JSON boolean field.
pub fn json_bool_field(name: &str, value: bool) -> String {
    format!("{}:{}", json_quote(name), value)
}
/// A JSON string array field.
pub fn json_array_field(name: &str, values: &[String]) -> String {
    let mut out = format!("{}:[", json_quote(name));
    for (i, v) in values.iter().enumerate() {
        if i != 0 {
            out.push(',');
        }
        out.push_str(&json_quote(v));
    }
    out.push(']');
    out
}
/// The one helper that joins object or array members.
pub fn json_members(members: &[String]) -> String {
    format!("{{{}}}", members.join(","))
}

// ---- uppercase percent encoding for one-line INI values ----

fn pct_byte(b: u8) -> bool {
    b <= 0x20 || b == 0x7f || b == b'%' || b == b','
}
/// Uppercase `%XX` for control bytes, `%`, `,`, a space and DEL; all other UTF-8 bytes stay raw.
pub fn pct_encode(s: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = Vec::with_capacity(s.len());
    for &b in s.as_bytes() {
        if pct_byte(b) {
            out.push(b'%');
            out.push(HEX[(b >> 4) as usize]);
            out.push(HEX[(b & 15) as usize]);
        } else {
            out.push(b);
        }
    }
    // SAFETY: s is valid UTF-8, and pct_byte only matches ASCII bytes (<= 0x20, 0x7f, '%', ',')
    // which are replaced by ASCII '%XX'. Non-ASCII multi-byte sequences remain unchanged and intact.
    String::from_utf8(out).unwrap()
}
/// The inverse of `pct_encode`: `None` for a truncated or non-hex escape, or bytes that are not
/// UTF-8. Lowercase hex is accepted on read, as RFC 3986 requires.
pub fn pct_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            if i + 2 >= b.len() {
                return None;
            }
            let hi = (b[i + 1] as char).to_digit(16)?;
            let lo = (b[i + 2] as char).to_digit(16)?;
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

// ---- JSON readers shared by the documents ----

pub(crate) fn parse_json(text: &[u8]) -> Option<(Vec<JsonTok>, i32)> {
    if text.is_empty() || text.len() > (i32::MAX / 2) as usize {
        return None;
    }
    // A caller-owned, dynamically sized token array: a 10 MiB catalog does not fit json_min's
    // usual fixed cap. Start at text.len()/4 + 128 and retry once at text.len()/2 + 128 if dense.
    let max_cap = (text.len() / 2 + 128).clamp(128, 6_000_000);
    let mut cap = (text.len() / 4 + 128).clamp(128, max_cap);
    loop {
        let mut toks = vec![JsonTok::default(); cap];
        let n = json_parse(text, &mut toks);
        if n >= 0 {
            return Some((toks, n));
        }
        if cap >= max_cap {
            return None;
        }
        cap = max_cap;
    }
}

pub(crate) fn jtok<'a>(text: &[u8], toks: &'a [JsonTok], obj: i32, key: &str) -> Option<(i32, &'a JsonTok)> {
    let t = json_get(text, toks, obj, key);
    if t < 0 {
        return None;
    }
    Some((t, &toks[t as usize]))
}
pub(crate) fn jstr(text: &[u8], toks: &[JsonTok], obj: i32, key: &str, cap: usize) -> Option<String> {
    let (_, tok) = jtok(text, toks, obj, key)?;
    let mut out = Vec::new();
    if !json_string(text, tok, &mut out, cap) {
        return None;
    }
    Some(String::from_utf8_lossy(&out).into_owned())
}
/// Check whether a raw JSON string token contains an unescaped `\u0000` escape (which `json_string`
/// otherwise truncates at).
pub(crate) fn tok_has_escaped_nul(text: &[u8], tok: &JsonTok) -> bool {
    if tok.type_ != JsonType::String || tok.start < 0 || tok.end < tok.start {
        return false;
    }
    let s = &text[tok.start as usize..tok.end as usize];
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'\\' {
            if i + 5 < s.len() && s[i + 1] == b'u' && &s[i + 2..i + 6] == b"0000" {
                return true;
            }
            i += 2;
        } else {
            i += 1;
        }
    }
    false
}
/// Strict UTF-8 string field: rejects NUL (`\u0000` included), invalid UTF-8, and decoded length `> max`.
pub(crate) fn jstr_strict(text: &[u8], toks: &[JsonTok], obj: i32, key: &str, max: usize) -> Option<String> {
    let (_, tok) = jtok(text, toks, obj, key)?;
    if tok_has_escaped_nul(text, tok) {
        return None;
    }
    let mut out = Vec::new();
    if !json_string(text, tok, &mut out, max.saturating_add(6)) {
        return None;
    }
    if out.len() > max || out.contains(&0) {
        return None;
    }
    String::from_utf8(out).ok()
}
/// Strict local filesystem path field: UTF-8 `<= max` bytes, rejects NUL, `\r`, `\n`, and ASCII control bytes.
pub(crate) fn jclean_path(text: &[u8], toks: &[JsonTok], obj: i32, key: &str, max: usize) -> Option<String> {
    let s = jstr_strict(text, toks, obj, key, max)?;
    if s.bytes().any(|b| b == 0 || b == b'\r' || b == b'\n' || b < 0x20 || b == 0x7f) {
        return None;
    }
    Some(s)
}
pub(crate) fn jint(text: &[u8], toks: &[JsonTok], obj: i32, key: &str) -> Option<i32> {
    let (_, tok) = jtok(text, toks, obj, key)?;
    let mut v = 0;
    json_int(text, tok, &mut v).then_some(v)
}
/// Parse an optional unsigned 32-bit integer field (`0..=u32::MAX`).
/// Missing or `null` yields `Some(0)`; negative, non-integer, or overflowing values yield `None`.
pub(crate) fn ju32_opt(text: &[u8], toks: &[JsonTok], obj: i32, key: &str) -> Option<u32> {
    let Some((_, tok)) = jtok(text, toks, obj, key) else { return Some(0) };
    if tok.type_ == JsonType::Null {
        return Some(0);
    }
    if tok.type_ != JsonType::Number {
        return None;
    }
    let s = std::str::from_utf8(&text[tok.start as usize..tok.end as usize]).ok()?;
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse::<u32>().ok()
}
/// Parse an optional non-negative 64-bit integer field (`0..=i64::MAX`) from an integer JSON number.
/// Missing or `null` yields `Some(0)`; negative, floating-point, non-number, or overflowing values yield `None`.
pub(crate) fn ji64_nonneg_opt(text: &[u8], toks: &[JsonTok], obj: i32, key: &str) -> Option<i64> {
    let Some((_, tok)) = jtok(text, toks, obj, key) else { return Some(0) };
    if tok.type_ == JsonType::Null {
        return Some(0);
    }
    if tok.type_ != JsonType::Number {
        return None;
    }
    let s = std::str::from_utf8(&text[tok.start as usize..tok.end as usize]).ok()?;
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse::<i64>().ok()
}
pub(crate) fn jint64(text: &[u8], toks: &[JsonTok], obj: i32, key: &str) -> Option<i64> {
    let (_, tok) = jtok(text, toks, obj, key)?;
    if tok.type_ != JsonType::Number {
        return None;
    }
    let s = std::str::from_utf8(&text[tok.start as usize..tok.end as usize]).ok()?;
    let whole = s.split(['.', 'e', 'E']).next().unwrap_or(s);
    whole.parse::<i64>().ok()
}
pub(crate) fn jbool(text: &[u8], toks: &[JsonTok], obj: i32, key: &str) -> Option<bool> {
    let (_, tok) = jtok(text, toks, obj, key)?;
    let mut v = false;
    json_bool(tok, &mut v).then_some(v)
}
pub(crate) fn jstrings(text: &[u8], toks: &[JsonTok], obj: i32, key: &str, max_items: usize, cap: usize) -> Vec<String> {
    let Some((t, tok)) = jtok(text, toks, obj, key) else { return Vec::new() };
    if tok.type_ != JsonType::Array {
        return Vec::new();
    }
    let mut out = Vec::new();
    for i in 0..tok.size.min(max_items as i32) {
        let at = json_item(toks, t, i);
        let Some(item) = toks.get(at.max(0) as usize).filter(|_| at >= 0) else { continue };
        let mut b = Vec::new();
        if json_string(text, item, &mut b, cap) {
            let cleaned = clean_text(&String::from_utf8_lossy(&b), cap);
            if !cleaned.is_empty() {
                out.push(cleaned);
            }
        }
    }
    out
}
pub(crate) fn schema_ok(text: &[u8], toks: &[JsonTok], obj: i32) -> bool {
    jint(text, toks, obj, "schema_version") == Some(SCHEMA_VERSION)
}

// ---- handoff.json ----

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandoffTarget {
    Zotero,
    Grimoire,
}
impl HandoffTarget {
    pub fn as_str(self) -> &'static str {
        match self {
            HandoffTarget::Zotero => TARGET_ZOTERO,
            HandoffTarget::Grimoire => TARGET_GRIMOIRE,
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            TARGET_ZOTERO => Some(HandoffTarget::Zotero),
            TARGET_GRIMOIRE => Some(HandoffTarget::Grimoire),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandoffAction {
    OpenItem,
    ReturnEssay,
}
impl HandoffAction {
    pub fn as_str(self) -> &'static str {
        match self {
            HandoffAction::OpenItem => ACTION_OPEN_ITEM,
            HandoffAction::ReturnEssay => ACTION_RETURN_ESSAY,
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            ACTION_OPEN_ITEM => Some(HandoffAction::OpenItem),
            ACTION_RETURN_ESSAY => Some(HandoffAction::ReturnEssay),
            _ => None,
        }
    }
}

/// The Zotero item to open. Every field is optional but `item_key` when a handoff targets Zotero.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ZoteroPlace {
    pub item_key: String,
    pub attachment_key: String,
    pub page: u32,
    pub locator: String,
}
pub type ZoteroHandoff = ZoteroPlace;
pub type HandoffZotero = ZoteroPlace;

/// The Grimoire page a handoff opens: an empty `notebook_id` means the normal start/last notebook.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GrimoirePlace {
    pub notebook_id: String,
    pub page_id: u32,
    pub page_index: u32,
}
pub type GrimoireHandoff = GrimoirePlace;
pub type HandoffGrimoire = GrimoirePlace;

/// The `return_to` block of a Grimoire → Zotero handoff.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReturnPlace {
    pub target: String,
    pub notebook_id: String,
    pub page_id: u32,
    pub page_index: u32,
}
pub type ReturnTo = ReturnPlace;

/// One parsed `handoff.json`. The target and action are kept as the exact strings of the document
/// (a future version may carry another one); `target_enum` / `action_enum` classify this version's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Handoff {
    pub schema_version: i32,
    pub nonce: String,
    pub created_at: i64,
    pub source: String,
    pub target: String,
    pub action: String,
    pub zotero: ZoteroPlace,
    pub grimoire: GrimoirePlace,
    pub return_to: Option<ReturnPlace>,
}
impl Default for Handoff {
    fn default() -> Self {
        Handoff {
            schema_version: SCHEMA_VERSION,
            nonce: String::new(),
            created_at: 0,
            source: TARGET_GRIMOIRE.to_string(),
            target: TARGET_ZOTERO.to_string(),
            action: ACTION_OPEN_ITEM.to_string(),
            zotero: ZoteroPlace::default(),
            grimoire: GrimoirePlace::default(),
            return_to: None,
        }
    }
}
impl Handoff {
    pub fn target_enum(&self) -> Option<HandoffTarget> {
        HandoffTarget::parse(&self.target)
    }
    pub fn action_enum(&self) -> Option<HandoffAction> {
        HandoffAction::parse(&self.action)
    }
    pub fn new_to_zotero(item_key: &str, attachment_key: &str, page: u32, locator: &str, return_to: Option<ReturnPlace>) -> Self {
        let mut h = Handoff::default();
        h.nonce = new_nonce();
        h.created_at = now_secs();
        h.zotero = ZoteroPlace {
            item_key: item_key.to_string(),
            attachment_key: attachment_key.to_string(),
            page,
            locator: clean_text(locator, 512),
        };
        if let Some(r) = return_to.as_ref() {
            h.grimoire = GrimoirePlace { notebook_id: r.notebook_id.clone(), page_id: r.page_id, page_index: r.page_index };
        }
        h.return_to = return_to;
        h
    }
    pub fn new_to_grimoire(notebook_id: &str, page_id: u32, page_index: u32) -> Self {
        let mut h = Handoff::default();
        h.nonce = new_nonce();
        h.created_at = now_secs();
        h.target = TARGET_GRIMOIRE.to_string();
        h.action = ACTION_RETURN_ESSAY.to_string();
        h.grimoire = GrimoirePlace { notebook_id: notebook_id.to_string(), page_id, page_index };
        h
    }
    /// The `return_to` block of a Grimoire page, for `new_to_zotero`.
    pub fn return_to(notebook_id: &str, page_id: u32, page_index: u32) -> ReturnPlace {
        ReturnPlace { target: TARGET_GRIMOIRE.to_string(), notebook_id: notebook_id.to_string(), page_id, page_index }
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}
fn new_nonce() -> String {
    let n = WRITE_NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{}-{:x}{:x}", now_ms(), std::process::id(), n)
}

/// Every field rule of the contract, checked again after parsing and before writing.
pub fn handoff_validate(h: &Handoff) -> bool {
    if h.schema_version != SCHEMA_VERSION || !nonce_valid(&h.nonce) || h.created_at < 0 {
        return false;
    }
    let target = h.target_enum();
    let action = h.action_enum();
    let Some(target) = target else { return false };
    if h.source != TARGET_GRIMOIRE && h.source != TARGET_ZOTERO {
        return false;
    }
    if !match (target, action) {
        (HandoffTarget::Zotero, Some(HandoffAction::OpenItem)) => true,
        (HandoffTarget::Grimoire, Some(HandoffAction::ReturnEssay)) => true,
        _ => false,
    } {
        return false;
    }
    if target == HandoffTarget::Zotero {
        if !item_key_valid(&h.zotero.item_key) {
            return false;
        }
    } else if !h.zotero.item_key.is_empty() && !item_key_valid(&h.zotero.item_key) {
        return false;
    }
    if (!h.zotero.attachment_key.is_empty() && !attachment_key_valid(&h.zotero.attachment_key))
        || !valid_zotero_page(h.zotero.page)
        || !locator_valid(&h.zotero.locator)
    {
        return false;
    }
    if !h.grimoire.notebook_id.is_empty() && !notebook_id_valid(&h.grimoire.notebook_id) {
        return false;
    }
    if let Some(r) = h.return_to.as_ref() {
        if r.target != TARGET_GRIMOIRE || !notebook_id_valid(&r.notebook_id) {
            return false;
        }
    }
    true
}

/// Whether a handoff is still fresh enough to act on.
pub fn handoff_fresh(h: &Handoff, now: i64) -> bool {
    h.created_at >= 0 && now >= h.created_at && now - h.created_at <= HANDOFF_MAX_AGE_SECS
}

/// The exact JSON of a handoff, with the contract's fields and order.
pub fn handoff_json(h: &Handoff) -> String {
    let mut z = vec![json_field("item_key", &h.zotero.item_key)];
    if !h.zotero.attachment_key.is_empty() {
        z.push(json_field("attachment_key", &h.zotero.attachment_key));
    }
    z.push(json_int_field("page", h.zotero.page as i64));
    z.push(json_field("locator", &h.zotero.locator));
    let mut g = vec![json_field("notebook_id", &h.grimoire.notebook_id), json_int_field("page_id", h.grimoire.page_id as i64), json_int_field("page_index", h.grimoire.page_index as i64)];
    if h.grimoire.notebook_id.is_empty() {
        g.truncate(1);
        g.push(json_int_field("page_id", 0));
        g.push(json_int_field("page_index", 0));
    }
    let mut m = vec![
        json_int_field("schema_version", SCHEMA_VERSION as i64),
        json_field("nonce", &h.nonce),
        json_int_field("created_at", h.created_at),
        json_field("source", &h.source),
        json_field("target", &h.target),
        json_field("action", &h.action),
        format!("\"zotero\":{}", json_members(&z)),
        format!("\"grimoire\":{}", json_members(&g)),
    ];
    if let Some(r) = h.return_to.as_ref() {
        let members = vec![
            json_field("target", &r.target),
            json_field("notebook_id", &r.notebook_id),
            json_int_field("page_id", r.page_id as i64),
            json_int_field("page_index", r.page_index as i64),
        ];
        m.push(format!("\"return_to\":{}", json_members(&members)));
    }
    json_members(&m)
}

/// Parse and validate one handoff document.
pub fn handoff_parse(bytes: &[u8]) -> Option<Handoff> {
    let (toks, _) = parse_json(bytes)?;
    let obj = 0;
    if !schema_ok(bytes, &toks, obj) {
        return None;
    }
    let mut h = Handoff::default();
    h.schema_version = SCHEMA_VERSION;
    h.nonce = jstr(bytes, &toks, obj, "nonce", 65)?;
    h.created_at = jint64(bytes, &toks, obj, "created_at").unwrap_or(0);
    h.source = jstr(bytes, &toks, obj, "source", 16).unwrap_or_else(|| TARGET_GRIMOIRE.to_string());
    h.target = jstr(bytes, &toks, obj, "target", 16)?;
    h.action = jstr(bytes, &toks, obj, "action", 24)?;
    if let Some((zt, _)) = jtok(bytes, &toks, obj, "zotero") {
        h.zotero.item_key = jstr(bytes, &toks, zt, "item_key", 33).unwrap_or_default();
        h.zotero.attachment_key = jstr(bytes, &toks, zt, "attachment_key", 33).unwrap_or_default();
        h.zotero.page = ju32_opt(bytes, &toks, zt, "page")?;
        h.zotero.locator = jstr(bytes, &toks, zt, "locator", 513).unwrap_or_default();
    }
    if let Some((gt, _)) = jtok(bytes, &toks, obj, "grimoire") {
        h.grimoire.notebook_id = jstr(bytes, &toks, gt, "notebook_id", 64).unwrap_or_default();
        h.grimoire.page_id = ju32_opt(bytes, &toks, gt, "page_id")?;
        h.grimoire.page_index = ju32_opt(bytes, &toks, gt, "page_index")?;
    }
    if let Some((rt, _)) = jtok(bytes, &toks, obj, "return_to") {
        let target = jstr(bytes, &toks, rt, "target", 16).unwrap_or_default();
        let notebook_id = jstr(bytes, &toks, rt, "notebook_id", 64).unwrap_or_default();
        let page_id = ju32_opt(bytes, &toks, rt, "page_id")?;
        let page_index = ju32_opt(bytes, &toks, rt, "page_index")?;
        // An empty return_to notebook_id means "no prior essay to return to"; ignore it
        // instead of rejecting the handoff.
        if !notebook_id.is_empty() {
            h.return_to = Some(ReturnPlace { target, notebook_id, page_id, page_index });
        }
    }
    handoff_validate(&h).then_some(h)
}

/// Write a handoff into `dir` (atomic). A handoff that does not validate is never written.
pub fn handoff_write_in(dir: &str, h: &Handoff) -> i32 {
    if !handoff_validate(h) {
        return -1;
    }
    let Some(path) = bridge_path_in(dir, HANDOFF_FILE) else { return -1 };
    bridge_write_atomic(&path, handoff_json(h).as_bytes())
}
/// Write a handoff into the active bridge directory.
pub fn handoff_write(h: &Handoff) -> i32 {
    handoff_write_in(&bridge_dir(), h)
}
/// Make and write a Grimoire → Zotero handoff in one call.
pub fn handoff_write_to_zotero(dir: &str, item_key: &str, attachment_key: &str, page: u32, locator: &str, return_to: Option<ReturnPlace>) -> i32 {
    handoff_write_in(dir, &Handoff::new_to_zotero(item_key, attachment_key, page, locator, return_to))
}
/// Make and write a Zotero → Grimoire handoff in one call.
pub fn handoff_write_to_grimoire(dir: &str, notebook_id: &str, page_id: u32, page_index: u32) -> i32 {
    handoff_write_in(dir, &Handoff::new_to_grimoire(notebook_id, page_id, page_index))
}
/// Whether a handoff is at the bridge's name.
pub fn handoff_exists() -> bool {
    let Some(path) = bridge_path(HANDOFF_FILE) else { return false };
    std::fs::symlink_metadata(path).is_ok()
}

/// Read a handoff without consuming it. A malformed, oversized or stale document is deleted here
/// so it can never poison a later startup, and a short generic line is logged.
pub fn handoff_peek() -> Option<Handoff> {
    handoff_peek_in(&bridge_dir())
}
pub fn handoff_peek_in(dir: &str) -> Option<Handoff> {
    let Some(path) = bridge_path_in(dir, HANDOFF_FILE) else { return None };
    let bytes = match bridge_read(&path, BRIDGE_JSON_MAX) {
        Ok(Some(b)) => b,
        Ok(None) => return None,
        Err(BridgeReadError::Absent) => return None,
        Err(_) => {
            let _ = std::fs::remove_file(&path);
            bridge_log_invalid(HANDOFF_FILE);
            return None;
        }
    };
    let Some(h) = handoff_parse(&bytes) else {
        let _ = std::fs::remove_file(&path);
        bridge_log_invalid(HANDOFF_FILE);
        return None;
    };
    if !handoff_fresh(&h, now_secs()) {
        let _ = std::fs::remove_file(&path);
        bridge_log("bridge: removed stale handoff.json");
        return None;
    }
    Some(h)
}
/// Read and immediately unlink a handoff. Its single-shot nature is the contract's.
pub fn handoff_take() -> Option<Handoff> {
    handoff_take_in(&bridge_dir())
}
pub fn handoff_take_in(dir: &str) -> Option<Handoff> {
    let h = handoff_peek_in(dir);
    let Some(path) = bridge_path_in(dir, HANDOFF_FILE) else { return h };
    if std::fs::symlink_metadata(&path).is_ok() {
        let _ = std::fs::remove_file(&path);
    }
    h
}
pub fn handoff_consume() -> Option<Handoff> {
    handoff_take()
}
/// Forget any handoff, without acting on it.
pub fn handoff_clear() -> i32 {
    bridge_remove(HANDOFF_FILE)
}

// ---- return_context.json ----

/// The last Grimoire essay/page zotero-for-kindle knows about.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ZoteroReturnContext {
    pub schema_version: i32,
    pub updated_at: i64,
    pub notebook_id: String,
    pub page_id: u32,
    pub page_index: u32,
}
pub type ReturnContext = ZoteroReturnContext;

impl ZoteroReturnContext {
    pub fn new(notebook_id: &str, page_id: u32, page_index: u32) -> Self {
        ZoteroReturnContext { schema_version: SCHEMA_VERSION, updated_at: now_secs(), notebook_id: notebook_id.to_string(), page_id, page_index }
    }
}

pub fn return_context_valid(r: &ZoteroReturnContext) -> bool {
    r.schema_version == SCHEMA_VERSION
        && r.updated_at >= 0
        && !r.notebook_id.is_empty()
        && notebook_id_valid(&r.notebook_id)
}
pub fn return_context_json(r: &ZoteroReturnContext) -> String {
    json_members(&[
        json_int_field("schema_version", SCHEMA_VERSION as i64),
        json_int_field("updated_at", r.updated_at),
        json_field("notebook_id", &r.notebook_id),
        json_int_field("page_id", r.page_id as i64),
        json_int_field("page_index", r.page_index as i64),
    ])
}
pub fn return_context_parse(bytes: &[u8]) -> Option<ZoteroReturnContext> {
    let (toks, _) = parse_json(bytes)?;
    if !schema_ok(bytes, &toks, 0) {
        return None;
    }
    let r = ZoteroReturnContext {
        schema_version: SCHEMA_VERSION,
        updated_at: jint64(bytes, &toks, 0, "updated_at").unwrap_or(0),
        notebook_id: jstr(bytes, &toks, 0, "notebook_id", 64).unwrap_or_default(),
        page_id: ju32_opt(bytes, &toks, 0, "page_id")?,
        page_index: ju32_opt(bytes, &toks, 0, "page_index")?,
    };
    return_context_valid(&r).then_some(r)
}
pub fn return_context_write_in(dir: &str, r: &ZoteroReturnContext) -> i32 {
    if !return_context_valid(r) {
        return -1;
    }
    let Some(path) = bridge_path_in(dir, RETURN_CONTEXT_FILE) else { return -1 };
    bridge_write_atomic(&path, return_context_json(r).as_bytes())
}
pub fn return_context_write(r: &ZoteroReturnContext) -> i32 {
    return_context_write_in(&bridge_dir(), r)
}
/// Save the essay/page a handoff asked to return to (only a valid `return_to` block does this).
pub fn return_context_save_in(dir: &str, h: &Handoff) -> i32 {
    let Some(r) = h.return_to.as_ref() else { return 0 };
    if r.target != TARGET_GRIMOIRE || !notebook_id_valid(&r.notebook_id) {
        return -1;
    }
    return_context_write_in(dir, &ZoteroReturnContext::new(&r.notebook_id, r.page_id, r.page_index))
}
pub fn return_context_save(h: &Handoff) -> i32 {
    return_context_save_in(&bridge_dir(), h)
}
pub fn return_context_load() -> Option<ZoteroReturnContext> {
    return_context_load_in(&bridge_dir())
}
pub fn return_context_load_in(dir: &str) -> Option<ZoteroReturnContext> {
    let path = bridge_path_in(dir, RETURN_CONTEXT_FILE)?;
    let bytes = match bridge_read(&path, BRIDGE_JSON_MAX) {
        Ok(Some(b)) => b,
        _ => return None,
    };
    return_context_parse(&bytes)
}

// ---- library_index.json ----

/// One Zotero collection entry from `zk-sync export-index` (`docs/unified-ink-sync-contract.md` §3.1).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ZoteroCollection {
    pub key: String,
    pub name: String,
    pub parent_key: String,
}

/// One compact catalog item. The bounded strings are the contract's; the small arrays hold the
/// collection keys/names and tag names a search or filter matches on.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ZoteroLibraryItem {
    pub item_key: String,
    pub attachment_key: String,
    pub citation_key: String,
    pub title: String,
    pub authors: String,
    pub year: String,
    pub item_type: String,
    pub collections: Vec<String>,
    pub collection_keys: Vec<String>,
    pub tags: Vec<String>,
    pub has_pdf: bool,
    pub has_epub: bool,
    pub has_snapshot: bool,
    pub pdf_cached: bool,
    pub pdf_local_path: String,
    pub last_opened: i64,
}
pub type LibraryItem = ZoteroLibraryItem;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ZoteroLibraryIndex {
    pub schema_version: i32,
    pub exported_at: i64,
    pub item_count: usize,
    pub collections: Vec<ZoteroCollection>,
    pub items: Vec<ZoteroLibraryItem>,
}
pub type LibraryIndex = ZoteroLibraryIndex;

fn jcollection_keys(text: &[u8], toks: &[JsonTok], obj: i32, key: &str, max_items: usize) -> Vec<String> {
    let Some((t, tok)) = jtok(text, toks, obj, key) else { return Vec::new() };
    if tok.type_ != JsonType::Array {
        return Vec::new();
    }
    let mut out = Vec::new();
    for i in 0..tok.size.min(max_items as i32) {
        let at = json_item(toks, t, i);
        let Some(item) = toks.get(at.max(0) as usize).filter(|_| at >= 0) else { continue };
        if tok_has_escaped_nul(text, item) {
            continue;
        }
        let mut b = Vec::new();
        if json_string(text, item, &mut b, 38) && b.len() <= 32 {
            if let Ok(s) = String::from_utf8(b) {
                if item_key_valid(&s) {
                    out.push(s);
                }
            }
        }
    }
    out
}

pub fn library_index_parse(bytes: &[u8]) -> Option<ZoteroLibraryIndex> {
    let (toks, _) = parse_json(bytes)?;
    if !schema_ok(bytes, &toks, 0) {
        return None;
    }
    let exported_at = jint64(bytes, &toks, 0, "exported_at").unwrap_or(0);
    if exported_at < 0 {
        return None;
    }
    let item_count = ju32_opt(bytes, &toks, 0, "item_count")? as usize;
    let mut out = ZoteroLibraryIndex {
        schema_version: SCHEMA_VERSION,
        exported_at,
        item_count,
        collections: Vec::new(),
        items: Vec::new(),
    };
    if let Some((carr, ctok)) = jtok(bytes, &toks, 0, "collections") {
        if ctok.type_ != JsonType::Null {
            if ctok.type_ != JsonType::Array {
                return None;
            }
            for i in 0..ctok.size.min(10_000) {
                let ct = json_item(&toks, carr, i);
                if ct < 0 {
                    break;
                }
                if toks[ct as usize].type_ != JsonType::Object || out.collections.try_reserve(1).is_err() {
                    continue;
                }
                let Some(ckey) = jstr_strict(bytes, &toks, ct, "key", 32) else { continue };
                if !item_key_valid(&ckey) {
                    continue;
                }
                let parent_key = match jtok(bytes, &toks, ct, "parent_key") {
                    None => String::new(),
                    Some((_, pt)) if pt.type_ == JsonType::Null => String::new(),
                    Some(_) => match jstr_strict(bytes, &toks, ct, "parent_key", 32) {
                        Some(pk) if pk.is_empty() || item_key_valid(&pk) => pk,
                        _ => continue,
                    },
                };
                let name = clean_text(&jstr(bytes, &toks, ct, "name", 520).unwrap_or_default(), 256);
                out.collections.push(ZoteroCollection { key: ckey, name, parent_key });
            }
        }
    }
    let Some((arr, atok)) = jtok(bytes, &toks, 0, "items") else { return Some(out) };
    if atok.type_ != JsonType::Array {
        return None;
    }
    for i in 0..atok.size.min(100_000) {
        let it = json_item(&toks, arr, i);
        if it < 0 {
            break;
        }
        if toks[it as usize].type_ != JsonType::Object || out.items.try_reserve(1).is_err() {
            continue;
        }
        let item_key = jstr_strict(bytes, &toks, it, "item_key", 32).unwrap_or_default();
        let attachment_key = match jtok(bytes, &toks, it, "attachment_key") {
            None => String::new(),
            Some((_, pt)) if pt.type_ == JsonType::Null => String::new(),
            Some(_) => match jstr_strict(bytes, &toks, it, "attachment_key", 32) {
                Some(ak) => ak,
                None => continue,
            },
        };
        if !item_key_valid(&item_key) || (!attachment_key.is_empty() && !attachment_key_valid(&attachment_key)) {
            continue;
        }
        let mut pdf_cached = jbool(bytes, &toks, it, "pdf_cached").unwrap_or(false);
        let pdf_local_path = match jtok(bytes, &toks, it, "pdf_local_path") {
            None => String::new(),
            Some((_, pt)) if pt.type_ == JsonType::Null => String::new(),
            Some(_) => match jclean_path(bytes, &toks, it, "pdf_local_path", 1024) {
                Some(p) => p,
                None => {
                    pdf_cached = false;
                    String::new()
                }
            },
        };
        let last_opened = ji64_nonneg_opt(bytes, &toks, it, "last_opened").unwrap_or(0);
        let item = ZoteroLibraryItem {
            item_key,
            attachment_key,
            citation_key: clean_text(&jstr(bytes, &toks, it, "citation_key", 257).unwrap_or_default(), 256),
            title: clean_text(&jstr(bytes, &toks, it, "title", 513).unwrap_or_default(), 512),
            authors: clean_text(&jstr(bytes, &toks, it, "authors", 257).unwrap_or_default(), 256),
            year: clean_text(&jstr(bytes, &toks, it, "year", 257).unwrap_or_default(), 256),
            item_type: clean_text(&jstr(bytes, &toks, it, "item_type", 257).unwrap_or_default(), 256),
            collections: jstrings(bytes, &toks, it, "collections", 64, 257),
            collection_keys: jcollection_keys(bytes, &toks, it, "collection_keys", 64),
            tags: jstrings(bytes, &toks, it, "tags", 64, 257),
            has_pdf: jbool(bytes, &toks, it, "has_pdf").unwrap_or(false),
            has_epub: jbool(bytes, &toks, it, "has_epub").unwrap_or(false),
            has_snapshot: jbool(bytes, &toks, it, "has_snapshot").unwrap_or(false),
            pdf_cached,
            pdf_local_path,
            last_opened,
        };
        out.items.push(item);
        if out.items.len() > 100_000 {
            break;
        }
    }
    Some(out)
}

struct CachedLibraryIndex {
    path: String,
    ino: u64,
    len: u64,
    mtime: i64,
    mtime_nsec: i64,
    index: std::sync::Arc<ZoteroLibraryIndex>,
}

static INDEX_CACHE: std::sync::Mutex<Option<CachedLibraryIndex>> = std::sync::Mutex::new(None);
static INDEX_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Monotonic generation counter for the in-memory `library_index.json` cache.
pub fn zotero_index_generation() -> u64 {
    INDEX_GENERATION.load(std::sync::atomic::Ordering::Acquire)
}

/// Clears the in-memory `INDEX_CACHE` and bumps `zotero_index_generation()`.
pub fn library_index_invalidate_cache() {
    if let Ok(mut g) = INDEX_CACHE.lock() {
        *g = None;
    }
    INDEX_GENERATION.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
}

/// Invalidates `INDEX_CACHE` and reloads `library_index.json` from the active bridge directory.
pub fn library_index_reload() -> Option<std::sync::Arc<ZoteroLibraryIndex>> {
    library_index_invalidate_cache();
    library_index_load()
}

pub fn library_index_load() -> Option<std::sync::Arc<ZoteroLibraryIndex>> {
    let dir = bridge_dir();
    let path = bridge_path_in(&dir, LIBRARY_INDEX_FILE)?;
    let md = match std::fs::symlink_metadata(&path) {
        Ok(m) if !m.file_type().is_symlink() && m.is_file() => m,
        _ => {
            if let Ok(mut g) = INDEX_CACHE.lock() {
                if g.is_some() {
                    *g = None;
                    INDEX_GENERATION.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                }
            }
            return None;
        }
    };
    if let Ok(g) = INDEX_CACHE.lock() {
        if let Some(ref c) = *g {
            if c.path == path
                && c.ino == md.ino()
                && c.len == md.len()
                && c.mtime == md.mtime()
                && c.mtime_nsec == md.mtime_nsec()
            {
                return Some(std::sync::Arc::clone(&c.index));
            }
        }
    }
    let parsed = library_index_load_in(&dir)?;
    let arc = std::sync::Arc::new(parsed);
    if let Ok(mut g) = INDEX_CACHE.lock() {
        *g = Some(CachedLibraryIndex {
            path,
            ino: md.ino(),
            len: md.len(),
            mtime: md.mtime(),
            mtime_nsec: md.mtime_nsec(),
            index: std::sync::Arc::clone(&arc),
        });
    }
    INDEX_GENERATION.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    Some(arc)
}
pub fn library_index_load_in(dir: &str) -> Option<ZoteroLibraryIndex> {
    let path = bridge_path_in(dir, LIBRARY_INDEX_FILE)?;
    let bytes = match bridge_read(&path, LIBRARY_INDEX_MAX) {
        Ok(Some(b)) => b,
        _ => return None,
    };
    library_index_parse(&bytes)
}
/// The item with this key, case sensitive (Zotero keys are case sensitive).
pub fn library_index_find<'a>(index: &'a ZoteroLibraryIndex, item_key: &str) -> Option<&'a ZoteroLibraryItem> {
    index.items.iter().find(|i| i.item_key == item_key)
}
fn item_matches(item: &ZoteroLibraryItem, needle: &str) -> bool {
    let has = |s: &str| s.to_lowercase().contains(needle);
    has(&item.item_key)
        || has(&item.citation_key)
        || has(&item.title)
        || has(&item.authors)
        || has(&item.year)
        || has(&item.item_type)
        || item.collections.iter().any(|s| has(s))
        || item.tags.iter().any(|s| has(s))
}
/// Filter catalog items by optional collection (matching exact `collection_keys` or case-insensitive
/// `collections` name) and case-insensitive whitespace-separated query terms across `title`, `authors`,
/// `year`, `citation_key`, `item_key`, `collections`, and `tags`.
pub fn library_index_filter<'a>(
    idx: &'a ZoteroLibraryIndex,
    query: &str,
    collection: &str,
) -> Vec<&'a ZoteroLibraryItem> {
    let coll = collection.trim();
    let coll_lower = coll.to_lowercase();
    let terms: Vec<String> = query.split_whitespace().map(|t| t.to_lowercase()).collect();
    idx.items
        .iter()
        .filter(|item| {
            if !coll.is_empty() {
                let by_key = item.collection_keys.iter().any(|k| k == coll);
                let by_name = item
                    .collections
                    .iter()
                    .any(|c| c.eq_ignore_ascii_case(coll) || c.to_lowercase() == coll_lower);
                if !by_key && !by_name {
                    return false;
                }
            }
            terms.iter().all(|term| item_matches(item, term))
        })
        .collect()
}
/// Indices of the items matching a case-insensitive query across all the catalog's text fields.
pub fn library_index_search_indices(index: &ZoteroLibraryIndex, query: &str) -> Vec<usize> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return (0..index.items.len()).collect();
    }
    index.items.iter().enumerate().filter(|(_, i)| item_matches(i, &needle)).map(|(i, _)| i).collect()
}
/// The matching items themselves, in the catalog's order.
pub fn library_index_search(index: &ZoteroLibraryIndex, query: &str) -> Vec<ZoteroLibraryItem> {
    library_index_search_indices(index, query).into_iter().filter_map(|i| index.items.get(i).cloned()).collect()
}

// ---- inbox ----

/// One appended citation event.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ZoteroCitation {
    pub schema_version: i32,
    pub id: String,
    pub created_at: i64,
    pub item_key: String,
    pub attachment_key: String,
    pub short_cite: String,
    pub title: String,
    pub authors: String,
    pub year: String,
    pub page: u32,
    pub locator: String,
    pub quote: String,
    pub note: String,
}
pub type CitationEvent = ZoteroCitation;
pub type InboxEvent = ZoteroCitation;

pub fn citation_valid(c: &ZoteroCitation) -> bool {
    c.schema_version == SCHEMA_VERSION
        && event_id_valid(&c.id)
        && c.created_at >= 0
        && item_key_valid(&c.item_key)
        && (c.attachment_key.is_empty() || attachment_key_valid(&c.attachment_key))
        && valid_zotero_page(c.page)
        && locator_valid(&c.locator)
        && bridge_text_valid(&c.short_cite, 256)
        && title_valid(&c.title)
        && bridge_text_valid(&c.authors, 256)
        && bridge_text_valid(&c.year, 256)
        && quote_valid(&c.quote, 4096)
        && quote_valid(&c.note, 4096)
}
pub fn citation_json(c: &ZoteroCitation) -> String {
    json_members(&[
        json_int_field("schema_version", SCHEMA_VERSION as i64),
        json_field("id", &c.id),
        json_int_field("created_at", c.created_at),
        json_field("item_key", &c.item_key),
        json_field("attachment_key", &c.attachment_key),
        json_field("short_cite", &c.short_cite),
        json_field("title", &c.title),
        json_field("authors", &c.authors),
        json_field("year", &c.year),
        json_int_field("page", c.page as i64),
        json_field("locator", &c.locator),
        json_field("quote", &c.quote),
        json_field("note", &c.note),
    ])
}
pub fn citation_parse(bytes: &[u8]) -> Option<ZoteroCitation> {
    let (toks, _) = parse_json(bytes)?;
    if !schema_ok(bytes, &toks, 0) {
        return None;
    }
    let c = ZoteroCitation {
        schema_version: SCHEMA_VERSION,
        id: jstr(bytes, &toks, 0, "id", 65)?,
        created_at: jint64(bytes, &toks, 0, "created_at").unwrap_or(0),
        item_key: jstr(bytes, &toks, 0, "item_key", 33)?,
        attachment_key: jstr(bytes, &toks, 0, "attachment_key", 33).unwrap_or_default(),
        short_cite: clean_text(&jstr(bytes, &toks, 0, "short_cite", 257).unwrap_or_default(), 256),
        title: clean_text(&jstr(bytes, &toks, 0, "title", 513).unwrap_or_default(), 512),
        authors: clean_text(&jstr(bytes, &toks, 0, "authors", 257).unwrap_or_default(), 256),
        year: clean_text(&jstr(bytes, &toks, 0, "year", 257).unwrap_or_default(), 256),
        page: ju32_opt(bytes, &toks, 0, "page")?,
        locator: jstr(bytes, &toks, 0, "locator", 513).unwrap_or_default(),
        quote: clean_text(&jstr(bytes, &toks, 0, "quote", 4097).unwrap_or_default(), 4096),
        note: clean_text(&jstr(bytes, &toks, 0, "note", 4097).unwrap_or_default(), 4096),
    };
    citation_valid(&c).then_some(c)
}
/// Write one citation event atomically (normally done by zotero-for-kindle; useful for tests and
/// for a future Grimoire-side "file a citation" action).
pub fn citation_write_in(dir: &str, c: &ZoteroCitation) -> i32 {
    if !citation_valid(c) {
        return -1;
    }
    let name = format!("{}-{}", c.created_at.max(0), c.id);
    let Some(path) = inbox_path_in(dir, &format!("{name}.json")) else { return -1 };
    bridge_write_atomic(&path, citation_json(c).as_bytes())
}

/// `^[0-9]+-[A-Za-z0-9_-]+\.json$`, with no path component and no symlink (the latter is checked at
/// the name when the file is opened).
pub fn inbox_name_ok(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".json") else { return false };
    let Some((epoch, nonce)) = stem.split_once('-') else { return false };
    !epoch.is_empty()
        && epoch.bytes().all(|b| b.is_ascii_digit())
        && key_ok(nonce, 64)
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0')
}

/// The names of the plain files in inbox/, in lexicographical order. Links and folders are left
/// alone (they are not consumed): they are not append-safe citation files.
pub fn inbox_list() -> Vec<String> {
    inbox_list_in(&bridge_dir())
}
pub fn inbox_list_in(dir: &str) -> Vec<String> {
    let d = inbox_dir_in(dir);
    let Ok(rd) = std::fs::read_dir(&d) else { return Vec::new() };
    let mut names: Vec<String> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            if !inbox_name_ok(&name) {
                return None;
            }
            let md = std::fs::symlink_metadata(e.path()).ok()?;
            (md.is_file() && !md.file_type().is_symlink()).then_some(name)
        })
        .collect();
    names.sort();
    names
}

fn read_inbox_file(path: &str) -> Result<Option<ZoteroCitation>, BridgeReadError> {
    let bytes = match bridge_read(path, BRIDGE_JSON_MAX)? {
        Some(b) => b,
        None => return Ok(None),
    };
    Ok(citation_parse(&bytes))
}

/// Read the valid events without unlinking them. Malformed and oversized files are removed so a
/// single bad producer can never poison the queue for the next startup.
pub fn inbox_peek() -> Vec<ZoteroCitation> {
    inbox_peek_in(&bridge_dir())
}
pub fn inbox_peek_in(dir: &str) -> Vec<ZoteroCitation> {
    let mut out = Vec::new();
    for name in inbox_list_in(dir) {
        let Some(path) = inbox_path_in(dir, &name) else { continue };
        match read_inbox_file(&path) {
            Ok(Some(c)) => out.push(c),
            Ok(None) => {
                let _ = std::fs::remove_file(&path);
                bridge_log_invalid(INBOX_DIR);
            }
            Err(_) => {
                let _ = std::fs::remove_file(&path);
                bridge_log_invalid(INBOX_DIR);
            }
        }
    }
    // A single epoch can hold several nonces; the file order already is chronological. Keep the
    // order stable by id for events that share a timestamp.
    out.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
    out
}

/// Remove one event, named by its `id` or by its file name.
pub fn inbox_remove(id: &str) -> i32 {
    inbox_remove_in(&bridge_dir(), id)
}
pub fn inbox_remove_in(dir: &str, id: &str) -> i32 {
    if id.is_empty() || id.contains('/') || id.contains('\\') {
        return -1;
    }
    let mut gone = 0;
    for name in inbox_list_in(dir) {
        let Some(path) = inbox_path_in(dir, &name) else { continue };
        let match_ = name == id || name.strip_suffix(".json").and_then(|s| s.split_once('-')).is_some_and(|(_, n)| n == id);
        if match_ && std::fs::remove_file(&path).is_ok() {
            gone += 1;
        }
    }
    if gone == 0 && name_eq_legacy(id) {
        let Some(path) = bridge_path_in(dir, LEGACY_INBOX_FILE) else { return -1 };
        return bridge_remove(LEGACY_INBOX_FILE) + if std::fs::symlink_metadata(&path).is_err() { 1 } else { 0 } - 1;
    }
    if gone == 0 { -1 } else { 0 }
}
fn name_eq_legacy(id: &str) -> bool {
    id == LEGACY_INBOX_FILE || id == "legacy" || id == "citation_inbox"
}

/// Read every valid event and unlink every consumed file. Also drains the optional legacy
/// `citation_inbox.json`, so an older producer's queue cannot sit beside the new one forever.
pub fn inbox_drain() -> Vec<ZoteroCitation> {
    inbox_drain_in(&bridge_dir())
}
pub fn inbox_drain_in(dir: &str) -> Vec<ZoteroCitation> {
    let mut out = inbox_peek_in(dir);
    for name in inbox_list_in(dir) {
        if let Some(path) = inbox_path_in(dir, &name) {
            let _ = std::fs::remove_file(&path);
        }
    }
    if let Some(legacy) = legacy_drain_in(dir) {
        out.extend(legacy);
    }
    out.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
    out
}
/// Read and unlink the oldest valid event only.
pub fn inbox_take_one() -> Option<ZoteroCitation> {
    let first = inbox_peek().into_iter().next()?;
    if inbox_remove(&first.id) == 0 {
        Some(first)
    } else {
        None
    }
}
pub fn inbox_count() -> usize {
    inbox_list().len()
}

fn legacy_drain_in(dir: &str) -> Option<Vec<ZoteroCitation>> {
    let path = bridge_path_in(dir, LEGACY_INBOX_FILE)?;
    let bytes = match bridge_read(&path, BRIDGE_JSON_MAX) {
        Ok(Some(b)) => b,
        _ => return None,
    };
    let (toks, _) = parse_json(&bytes)?;
    if !schema_ok(&bytes, &toks, 0) {
        return None;
    }
    let (arr, atok) = jtok(&bytes, &toks, 0, "citations")?;
    if atok.type_ != JsonType::Array {
        return None;
    }
    let mut out = Vec::new();
    for i in 0..atok.size.min(10_000) {
        let t = json_item(&toks, arr, i);
        if t < 0 {
            continue;
        }
        let tok = &toks[t as usize];
        if tok.type_ != JsonType::Object {
            continue;
        }
        let mut c = ZoteroCitation::default();
        c.schema_version = SCHEMA_VERSION;
        c.id = jstr(&bytes, &toks, t, "id", 65).unwrap_or_else(|| format!("legacy-{i}"));
        c.created_at = jint64(&bytes, &toks, t, "created_at").unwrap_or(0);
        c.item_key = jstr(&bytes, &toks, t, "item_key", 33).unwrap_or_default();
        c.attachment_key = jstr(&bytes, &toks, t, "attachment_key", 33).unwrap_or_default();
        c.short_cite = jstr(&bytes, &toks, t, "short_cite", 257).unwrap_or_default();
        c.title = jstr(&bytes, &toks, t, "title", 513).unwrap_or_default();
        c.authors = jstr(&bytes, &toks, t, "authors", 257).unwrap_or_default();
        c.year = jstr(&bytes, &toks, t, "year", 257).unwrap_or_default();
        c.page = jint(&bytes, &toks, t, "page").unwrap_or(0).max(0) as u32;
        c.locator = jstr(&bytes, &toks, t, "locator", 513).unwrap_or_default();
        c.quote = jstr(&bytes, &toks, t, "quote", 4097).unwrap_or_default();
        c.note = jstr(&bytes, &toks, t, "note", 4097).unwrap_or_default();
        if citation_valid(&c) {
            out.push(c);
        }
    }
    let _ = std::fs::remove_file(&path);
    Some(out)
}

thread_local! {
    /// Events drained at startup, waiting for Track E's editor/zotero_sheet to turn them into
    /// links on a page. `pending_citations_take` hands them over once.
    static PENDING_CITATIONS: RefCell<Vec<ZoteroCitation>> = const { RefCell::new(Vec::new()) };
}
pub fn pending_citations_push(events: &[ZoteroCitation]) {
    PENDING_CITATIONS.with(|p| p.borrow_mut().extend_from_slice(events));
}
pub fn pending_citations() -> Vec<ZoteroCitation> {
    PENDING_CITATIONS.with(|p| p.borrow().clone())
}
pub fn pending_citations_take() -> Vec<ZoteroCitation> {
    PENDING_CITATIONS.with(|p| std::mem::take(&mut *p.borrow_mut()))
}

// ---- zoterolinks.ini ----

/// One Zotero reference cited on a page: the area of the badge and the item to open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ZoteroRef {
    pub pid: u32,
    pub seed: u32,
    pub area: Rect,
    pub item_key: String,
    pub attachment_key: String,
    pub page: u32,
    pub locator: String,
    pub short_cite: String,
    pub quote: String,
}
pub type ZoteroLink = ZoteroRef;

impl ZoteroRef {
    pub fn from_citation(c: &ZoteroCitation, pid: u32, seed: u32, area: Rect) -> Self {
        ZoteroRef {
            pid,
            seed,
            area,
            item_key: c.item_key.clone(),
            attachment_key: c.attachment_key.clone(),
            page: c.page,
            locator: clean_text(&c.locator, 512),
            short_cite: clean_text(&c.short_cite, 256),
            quote: clean_text(&c.quote, 4096),
        }
    }
}

/// A notebook's zoterolinks.ini. Carries the lines this version does not read, like weblinks.ini's.
#[derive(Clone, Debug, Default)]
pub struct ZoteroLinks {
    pub links: Vec<ZoteroRef>,
    pub unknown: Vec<u8>,
    pub writable: bool,
    pub skipped: usize,
    pub file: bool,
}
pub type ZoteroLinksFile = ZoteroLinks;

impl ZoteroLinks {
    pub fn is_empty(&self) -> bool {
        self.links.is_empty()
    }
    /// Add a link in page order; false at a cap or without memory. The later line for one page and
    /// seed replaces the earlier one.
    pub fn push(&mut self, l: ZoteroRef) -> bool {
        if !self.writable || self.links.len() >= ZOTEROLINKS_MAX || self.links.try_reserve(1).is_err() {
            return false;
        }
        if let Some(k) = self.links.iter().position(|x| x.pid == l.pid && x.seed == l.seed) {
            self.links[k] = l;
            return true;
        }
        let at = self.links.partition_point(|x| (x.pid, x.seed) <= (l.pid, l.seed));
        self.links.insert(at, l);
        true
    }
    pub fn remove(&mut self, pid: u32, seed: u32) -> bool {
        let Some(at) = self.links.iter().position(|l| l.pid == pid && l.seed == seed) else { return false };
        self.links.remove(at);
        true
    }
}
/// A stable seed for a citation's badge on a page, so a fill finds the same one again.
pub fn zotero_seed(pid: u32, item_key: &str) -> u32 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in item_key.as_bytes().iter().chain(&pid.to_le_bytes()) {
        h = (h ^ b as u64).wrapping_mul(0x100_0000_01b3);
    }
    (h as u32) | 0x8000_0000
}

pub fn zoterolinks_of(links: &ZoteroLinks, pid: u32) -> &[ZoteroRef] {
    let a = links.links.partition_point(|l| l.pid < pid);
    let b = links.links.partition_point(|l| l.pid <= pid);
    &links.links[a..b]
}

fn parse_area(field: &[u8]) -> Option<i32> {
    if field.is_empty() || field.len() > 4 || !field.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut v = 0i32;
    for &c in field {
        v = v * 10 + (c - b'0') as i32;
    }
    (v <= NAV_AREA_MAX).then_some(v)
}
fn parse_hex_seed(field: &[u8]) -> Option<u32> {
    if field.is_empty() || field.len() > 8 || !field.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    let mut v = 0u64;
    for &c in field {
        v = v * 16 + (c as char).to_digit(16)? as u64;
    }
    (v <= u32::MAX as u64).then_some(v as u32)
}
fn zref_parse(line: &[u8]) -> Option<ZoteroRef> {
    let rest = line.strip_prefix(ZREF_PREFIX.as_bytes())?;
    let eq = rest.iter().position(|&c| c == b'=')?;
    let pid = std::str::from_utf8(&rest[..eq]).ok()?.parse::<u32>().ok()?;
    if pid == 0 {
        return None;
    }
    let fields: Vec<&[u8]> = rest[eq + 1..].split(|&c| c == b',').collect();
    if fields.len() != 10 && fields.len() != 11 {
        return None;
    }
    let seed = parse_hex_seed(fields[0])?;
    let area = Rect::new(parse_area(fields[1])?, parse_area(fields[2])?, parse_area(fields[3])?, parse_area(fields[4])?);
    if !nav_area_valid(area) || area.w == 0 || area.h == 0 {
        return None;
    }
    let item_key = std::str::from_utf8(fields[5]).ok()?;
    let attachment_key = std::str::from_utf8(fields[6]).ok()?;
    let page = std::str::from_utf8(fields[7]).ok()?.parse::<u32>().ok()?;
    let locator = pct_decode(std::str::from_utf8(fields[8]).ok()?)?;
    let short_cite = pct_decode(std::str::from_utf8(fields[9]).ok()?)?;
    let quote = if fields.len() == 11 {
        let q = pct_decode(std::str::from_utf8(fields[10]).ok()?)?;
        clean_text(&q, 4096)
    } else {
        String::new()
    };
    if !item_key_valid(item_key)
        || (!attachment_key.is_empty() && !attachment_key_valid(attachment_key))
        || !valid_zotero_page(page)
        || !locator_valid(&locator)
        || !bridge_text_valid(&short_cite, 256)
        || !quote_valid(&quote, 4096)
    {
        return None;
    }
    Some(ZoteroRef { pid, seed, area, item_key: item_key.to_string(), attachment_key: attachment_key.to_string(), page, locator, short_cite, quote })
}

/// A carried line of another version's, capped like weblinks.ini's.
fn carry_zline(buf: &mut Vec<u8>, line: &[u8]) -> bool {
    if line.len() > 512 || buf.len() + line.len() + 1 > ZOTEROLINKS_UNKNOWN_MAX || buf.try_reserve(line.len() + 1).is_err() {
        return false;
    }
    buf.extend_from_slice(line);
    buf.push(b'\n');
    true
}

/// Parse zoterolinks.ini strictly. A line this version cannot read is carried verbatim; one past
/// the carrying cap is counted and leaves the file unwritable, so the version that wrote it can
/// still read it later.
pub fn zoterolinks_parse(text: &[u8]) -> Option<ZoteroLinks> {
    let mut out = ZoteroLinks { writable: true, ..Default::default() };
    for raw in text.split(|&c| c == b'\n') {
        let line = raw.strip_suffix(b"\r").unwrap_or(raw);
        if line.is_empty() || line[0] == b'#' {
            continue;
        }
        let Some(l) = zref_parse(line) else {
            if !carry_zline(&mut out.unknown, line) {
                out.skipped += 1;
                out.writable = false;
            }
            continue;
        };
        if out.links.len() >= ZOTEROLINKS_MAX && !out.links.iter().any(|x| x.pid == l.pid && x.seed == l.seed) {
            return None;
        }
        let mut run = 0;
        for x in &out.links {
            if x.pid == l.pid && x.seed != l.seed {
                run += 1;
            }
        }
        if run >= ZOTEROLINKS_PER_PAGE && !out.links.iter().any(|x| x.pid == l.pid && x.seed == l.seed) {
            return None;
        }
        out.push(l);
    }
    Some(out)
}

/// Load a notebook's zoterolinks.ini. Missing is an empty writable set; an unreadable file reads
/// as none and refuses writes so it is never overwritten.
pub fn zoterolinks_load(id: &str) -> ZoteroLinks {
    let Some(path) = crate::nav::path_for(id, ZOTEROLINKS_FILE) else {
        return ZoteroLinks { writable: false, file: true, ..Default::default() };
    };
    match crate::nav::read_sidecar(&path, ZOTEROLINKS_FILE_MAX) {
        Ok(None) => ZoteroLinks { writable: true, ..Default::default() },
        Ok(Some(bytes)) => match zoterolinks_parse(&bytes) {
            Some(mut links) => {
                links.file = true;
                links
            }
            None => ZoteroLinks { writable: false, file: true, ..Default::default() },
        },
        Err(()) => ZoteroLinks { writable: false, file: true, ..Default::default() },
    }
}
/// A cheap generation number for the sidecar: its length and modification time. Zero when absent.
pub fn zoterolinks_generation(id: &str) -> u32 {
    let Some(path) = crate::nav::path_for(id, ZOTEROLINKS_FILE) else { return 0 };
    let Ok(md) = std::fs::symlink_metadata(path) else { return 0 };
    if !md.is_file() {
        return 0;
    }
    let mut h: u64 = 0xcbf29ce484222325;
    for b in md.len().to_le_bytes().iter().chain(&md.mtime().to_le_bytes()).chain(&md.mtime_nsec().to_le_bytes()) {
        h = (h ^ *b as u64).wrapping_mul(0x100_0000_01b3);
    }
    (h as u32) | (h >> 32) as u32
}

fn zref_line(l: &ZoteroRef) -> String {
    let r = l.area;
    if l.quote.is_empty() {
        format!(
            "zref.{}={:08x},{},{},{},{},{},{},{},{},{}\n",
            l.pid,
            l.seed,
            r.x,
            r.y,
            r.w,
            r.h,
            l.item_key,
            l.attachment_key,
            l.page,
            pct_encode(&l.locator),
            pct_encode(&l.short_cite)
        )
    } else {
        format!(
            "zref.{}={:08x},{},{},{},{},{},{},{},{},{},{}\n",
            l.pid,
            l.seed,
            r.x,
            r.y,
            r.w,
            r.h,
            l.item_key,
            l.attachment_key,
            l.page,
            pct_encode(&l.locator),
            pct_encode(&l.short_cite),
            pct_encode(&l.quote)
        )
    }
}
/// The file's text: links of pages still in `nb`, then carried lines.
pub fn zoterolinks_text(links: &ZoteroLinks, nb: Option<&NotebookInfo>) -> Vec<u8> {
    let mut text = String::from(ZOTEROLINKS_HEADER);
    for l in &links.links {
        if nb.is_some_and(|nb| !has_page(nb, l.pid)) {
            continue;
        }
        text.push_str(&zref_line(l));
    }
    let mut bytes = text.into_bytes();
    bytes.extend_from_slice(&links.unknown);
    bytes
}
fn zoterolinks_save_to(path: &str, links: &mut ZoteroLinks, nb: Option<&NotebookInfo>) -> i32 {
    if !links.writable {
        return -1;
    }
    if let Some(nb) = nb {
        if nb.id.is_empty() || nb.npages() < 0 || nb.npages() > STORE_MAX_PAGES as i32 {
            return -1;
        }
    }
    let kept = |l: &ZoteroRef| nb.is_none_or(|nb| has_page(nb, l.pid));
    let n = links.links.iter().filter(|l| kept(l)).count();
    if !links.file && n == 0 && links.unknown.is_empty() {
        return 0;
    }
    if n > ZOTEROLINKS_MAX || links.skipped > 0 {
        return -1;
    }
    let text = zoterolinks_text(links, nb);
    if text.len() > ZOTEROLINKS_FILE_MAX {
        return -1;
    }
    if write_file_atomic(path, &text) != 0 {
        return -1;
    }
    links.file = true;
    links.links.retain(kept);
    0
}
/// Save zoterolinks.ini (atomic), leaving out links of pages no longer in `nb`.
pub fn zoterolinks_save(id: &str, links: &mut ZoteroLinks, nb: &NotebookInfo) -> i32 {
    if id.as_bytes() != nb.id.as_bytes() {
        return -1;
    }
    let Some(path) = crate::nav::path_for(id, ZOTEROLINKS_FILE) else { return -1 };
    zoterolinks_save_to(&path, links, Some(nb))
}
/// Save into a notebook folder not yet in the store (a notebook being built in a work folder).
pub fn zoterolinks_save_in(dir: &str, links: &mut ZoteroLinks, nb: &NotebookInfo) -> i32 {
    zoterolinks_save_to(&format!("{dir}/{ZOTEROLINKS_FILE}"), links, Some(nb))
}
/// Drop links of pages no longer in `nb`; returns how many went.
pub fn zoterolinks_prune(links: &mut ZoteroLinks, nb: &NotebookInfo) -> usize {
    let before = links.links.len();
    links.links.retain(|l| has_page(nb, l.pid));
    before - links.links.len()
}

// ---- reference nodes for the Index/Graph ----

/// One Zotero reference node: an item cited by one or more pages of the notebook. `pages` are the
/// citing essay pages; `first_seed` is the badge a broken-node repair starts from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ZoteroReferenceNode {
    pub item_key: String,
    pub attachment_key: String,
    pub page: u32,
    pub short_cite: String,
    pub pages: Vec<u32>,
    pub links: usize,
    pub first_seed: u32,
}
pub type ZoteroReference = ZoteroReferenceNode;

/// Group a notebook's links into one node per cited item, with every citing page.
pub fn zotero_references(links: &ZoteroLinks) -> Vec<ZoteroReferenceNode> {
    let mut out: Vec<ZoteroReferenceNode> = Vec::new();
    for l in &links.links {
        if !item_key_valid(&l.item_key) || l.pid == 0 {
            continue;
        }
        if let Some(n) = out.iter_mut().find(|n| n.item_key == l.item_key) {
            n.links += 1;
            n.first_seed = n.first_seed.min(l.seed);
            if !n.pages.contains(&l.pid) {
                n.pages.push(l.pid);
            }
            if n.short_cite.is_empty() && !l.short_cite.is_empty() {
                n.short_cite = l.short_cite.clone();
            }
            continue;
        }
        if out.try_reserve(1).is_err() {
            break;
        }
        out.push(ZoteroReferenceNode {
            item_key: l.item_key.clone(),
            attachment_key: l.attachment_key.clone(),
            page: l.page,
            short_cite: l.short_cite.clone(),
            pages: vec![l.pid],
            links: 1,
            first_seed: l.seed,
        });
    }
    out.sort_by(|a, b| a.item_key.cmp(&b.item_key).then(a.page.cmp(&b.page)));
    out
}
/// Read a notebook's sidecar and return its reference nodes. No file: no nodes, no log, no write.
pub fn zotero_reference_nodes(id: &str) -> Vec<ZoteroReferenceNode> {
    let links = zoterolinks_load(id);
    zotero_references(&links)
}

// ---- z: nav targets ----

/// A target for a reference node, within `NAV_TARGET_LEN`:
/// `z:<item_key>[@<page>][/<attachment_key>][#<locator_pct>]`. The optional parts are dropped, in
/// that order, when the target would not fit a links.ini field.
pub fn zotero_target(r: &ZoteroRef) -> Option<String> {
    if !item_key_valid(&r.item_key) {
        return None;
    }
    let mut t = format!("{ZOTERO_TARGET_PREFIX}{}", r.item_key);
    if valid_zotero_page(r.page) && r.page > 0 && t.len() + 1 + 7 < NAV_TARGET_LEN {
        t.push('@');
        t.push_str(&r.page.to_string());
    }
    if attachment_key_valid(&r.attachment_key) && t.len() + 1 + r.attachment_key.len() < NAV_TARGET_LEN {
        t.push('/');
        t.push_str(&r.attachment_key);
    }
    if locator_valid(&r.locator) && !r.locator.is_empty() {
        let enc = pct_encode(&r.locator);
        if t.len() + 1 + enc.len() < NAV_TARGET_LEN {
            t.push('#');
            t.push_str(&enc);
        }
    }
    (t.len() < NAV_TARGET_LEN).then_some(t)
}
pub fn zotero_target_encode(r: &ZoteroRef) -> String {
    zotero_target(r).unwrap_or_default()
}
pub fn zotero_target_from_parts(item_key: &str, attachment_key: &str, page: u32, locator: &str) -> Option<String> {
    zotero_target(&ZoteroRef {
        pid: 0,
        seed: 0,
        area: Rect::default(),
        item_key: item_key.to_string(),
        attachment_key: attachment_key.to_string(),
        page,
        locator: locator.to_string(),
        short_cite: String::new(),
        quote: String::new(),
    })
}
/// Read a `z:` target back. Also accepts the unambiguous field orders `z:ITEM:ATT` and
/// `z:ITEM:ATT:PAGE` (the order an earlier draft wrote), so a link made by that draft still opens.
pub fn zotero_target_parse(target: &str) -> Option<ZoteroRef> {
    let rest = target.strip_prefix(ZOTERO_TARGET_PREFIX)?;
    if rest.is_empty() || rest.len() >= NAV_TARGET_LEN {
        return None;
    }
    let (head, locator) = match rest.split_once('#') {
        Some((h, l)) => (h, pct_decode(l)?),
        None => (rest, String::new()),
    };
    if !locator_valid(&locator) {
        return None;
    }
    let (head, attachment) = match head.split_once('/') {
        Some((h, a)) => (h, a),
        None => (head, ""),
    };
    let (item, page) = match head.split_once('@') {
        Some((k, p)) => (k, p.parse::<u32>().ok()?),
        None => (head, 0),
    };
    let (item, attachment, page) = if item.contains(':') {
        // the draft form `z:ITEM:ATT[:PAGE]`
        let mut parts = head.split(':');
        let k = parts.next()?;
        let a = parts.next()?;
        let p = match parts.next() { Some(p) => p.parse::<u32>().ok()?, None => 0 };
        if parts.next().is_some() {
            return None;
        }
        (k, a, p)
    } else {
        (item, attachment, page)
    };
    if !item_key_valid(item) || (!attachment.is_empty() && !attachment_key_valid(attachment)) || !valid_zotero_page(page) {
        return None;
    }
    Some(ZoteroRef {
        pid: 0,
        seed: 0,
        area: Rect::default(),
        item_key: item.to_string(),
        attachment_key: attachment.to_string(),
        page,
        locator,
        short_cite: String::new(),
        quote: String::new(),
    })
}
pub fn zotero_target_decode(target: &str) -> Option<ZoteroRef> {
    zotero_target_parse(target)
}
/// Whether a nav target names a Zotero item.
pub fn is_zotero_target(target: &str) -> bool {
    target.starts_with(ZOTERO_TARGET_PREFIX)
}

// ---- app and startup integration ----

/// Write a prepared handoff, stop the main loop and set the trampoline's exit code. Everything
/// after this (saves, display wait, descriptor close) is still done by `mainloop::run` before it
/// returns the code. Returns -1 when the handoff could not be written: the loop keeps running.
pub fn app_request_handoff(a: &mut App, h: &Handoff) -> i32 {
    if handoff_write(h) != 0 {
        return -1;
    }
    a.running = false;
    a.exit_code = if h.target_enum() == Some(HandoffTarget::Zotero) { EXIT_HANDOFF_TO_ZOTERO } else { EXIT_CLEAN };
    0
}
/// The common Track E call: cite an item and leave for Zotero.
pub fn app_request_zotero_handoff(a: &mut App, item_key: &str, attachment_key: &str, page: u32, locator: &str, return_to: Option<ReturnPlace>) -> i32 {
    app_request_handoff(a, &Handoff::new_to_zotero(item_key, attachment_key, page, locator, return_to))
}
/// Leave a handoff for a Zotero process that will come back to this essay.
pub fn app_request_return_to_grimoire(a: &mut App, notebook_id: &str, page_id: u32, page_index: u32) -> i32 {
    app_request_handoff(a, &Handoff::new_to_grimoire(notebook_id, page_id, page_index))
}

/// Startup half of the handoff: consume a handoff that targets Grimoire, open the essay it names,
/// drain the citation inbox, and say what arrived. A Grimoire→Zotero handoff is left alone: it is
/// the trampoline's and Zotero's to consume. With no bridge directory every call is skipped.
pub fn bridge_startup(a: &mut App) -> i32 {
    if !bridge_exists() {
        return 0;
    }
    let mut return_essay = false;
    if let Some(h) = handoff_take() {
        if h.target_enum() == Some(HandoffTarget::Grimoire) {
            if h.action_enum() == Some(HandoffAction::ReturnEssay) || h.action == ACTION_RETURN_ESSAY {
                return_essay = true;
            }
            if !h.grimoire.notebook_id.is_empty() {
                editor_open_notebook_at(a, &h.grimoire.notebook_id, h.grimoire.page_id, false);
            }
            // TRANSLATORS: a startup toast after Zotero hands the open essay back to Grimoire.
            app_toast(a, tr("Back from Zotero."), 3000);
        }
    }
    let events = inbox_drain();
    if !events.is_empty() {
        let n = events.len();
        pending_citations_push(&events);
        // TRANSLATORS: a startup toast: how many citations Grimoire imported from Zotero while it was closed.
        let msg = cformat!(tr("%d Zotero citations imported."), n as i32);
        app_toast(a, &msg, 3000);
    }
    if return_essay {
        crate::editor::editor_zotero_return_handoff(a);
    }
    0
}
/// Alias that reads as what it does at startup.
pub fn zotero_startup(a: &mut App) -> i32 {
    bridge_startup(a)
}

/// Alias for pending_citations
pub fn bridge_pending_citations() -> Vec<ZoteroCitation> {
    pending_citations()
}

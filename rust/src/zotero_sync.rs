//! Headless `zk-sync` runner, non-blocking UI background worker, and per-notebook Zotero PDF
//! binding sidecar (`notebooks/<nb_id>/zotero_source.json`, `schema_version: 1`).
//!
//! Security & transport invariants (`docs/unified-ink-sync-contract.md`):
//! - Never invokes `sh -c` or a shell; spawns the configured `zk-sync` binary directly with
//!   explicit `.args(...)`, `Stdio::null()` stdin, non-blocking piped stdout/stderr, hard byte
//!   caps (`10 MiB` stdout, `64 KiB` stderr), and a bounded wall-clock timeout.
//! - Zero-Credential Invariant: all subprocess diagnostics are passed through
//!   `scrub_error_message` before reaching errors, logs, or e-ink toasts.
//! - File safety: rejects symlinks on bridge directories, cached PDFs, and notebook sidecars, and
//!   writes `zotero_source.json` atomically (`O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW`, `fsync`,
//!   `rename`).

use crate::fsview::StoragePath;
use crate::pdfimport::name_storable;
use crate::prelude::*;

use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ---- configuration & constants ----

pub const DEFAULT_ZK_SYNC_BIN: &str = "/mnt/us/zotero-bridge/bin/zk-sync";
pub const DEFAULT_ZK_HOME: &str = "/mnt/us/zotero-bridge";
pub const DEFAULT_ZK_CONFIG_DIR: &str = "/var/local/zotero-bridge";

pub const ZK_SYNC_BIN_ENV: &str = "GRIMOIRE_ZK_SYNC_BIN";
pub const ZK_HOME_ENV: &str = "ZK_HOME";
pub const ZK_CONFIG_DIR_ENV: &str = "ZK_CONFIG_DIR";

pub const ZK_STDOUT_MAX: usize = 10 * 1024 * 1024;
pub const ZK_STDERR_MAX: usize = 64 * 1024;
pub const ZK_LOCAL_TIMEOUT_MS: u64 = 15_000;
pub const ZK_NETWORK_TIMEOUT_MS: u64 = 60_000;
pub const ZK_ERROR_MAX_BYTES: usize = 240;

pub const ZOTERO_SOURCE_FILE: &str = "zotero_source.json";
pub const ZOTERO_SOURCE_MAX: usize = 2 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ZkSyncConfig {
    pub bin: String,
    pub zk_home: String,
    pub zk_config_dir: String,
    pub local_timeout_ms: u64,
    pub network_timeout_ms: u64,
}

impl Default for ZkSyncConfig {
    fn default() -> Self {
        Self {
            bin: DEFAULT_ZK_SYNC_BIN.to_string(),
            zk_home: DEFAULT_ZK_HOME.to_string(),
            zk_config_dir: DEFAULT_ZK_CONFIG_DIR.to_string(),
            local_timeout_ms: ZK_LOCAL_TIMEOUT_MS,
            network_timeout_ms: ZK_NETWORK_TIMEOUT_MS,
        }
    }
}

static TEST_CONFIG_OVERRIDE: Mutex<Option<ZkSyncConfig>> = Mutex::new(None);

fn env_or_default(var: &str, default: &str) -> String {
    let from_env = std::env::var_os(var)
        .map(|v| v.to_string_lossy().into_owned())
        .unwrap_or_default();
    if from_env.is_empty() {
        default.to_string()
    } else {
        from_env
    }
}

/// Thread-safe test override for `ZkSyncConfig`. Pass `None` to restore environment/default lookup.
pub fn zk_sync_set_test_config(cfg: Option<ZkSyncConfig>) {
    if let Ok(mut g) = TEST_CONFIG_OVERRIDE.lock() {
        *g = cfg;
    }
}

/// Resolve the active `ZkSyncConfig` (test override if set, else environment variables / Kindle defaults).
pub fn zk_sync_config() -> ZkSyncConfig {
    if let Ok(g) = TEST_CONFIG_OVERRIDE.lock() {
        if let Some(ref cfg) = *g {
            return cfg.clone();
        }
    }
    ZkSyncConfig {
        bin: env_or_default(ZK_SYNC_BIN_ENV, DEFAULT_ZK_SYNC_BIN),
        zk_home: env_or_default(ZK_HOME_ENV, DEFAULT_ZK_HOME),
        zk_config_dir: env_or_default(ZK_CONFIG_DIR_ENV, DEFAULT_ZK_CONFIG_DIR),
        local_timeout_ms: ZK_LOCAL_TIMEOUT_MS,
        network_timeout_ms: ZK_NETWORK_TIMEOUT_MS,
    }
}

fn bin_executable_regular(path: &str) -> bool {
    if path.is_empty() || path.contains('\0') {
        return false;
    }
    std::fs::symlink_metadata(path).is_ok_and(|md| {
        !md.file_type().is_symlink() && md.is_file() && (md.permissions().mode() & 0o111) != 0
    })
}

/// Whether both the bridge directory and a regular executable `zk-sync` binary exist.
pub fn zk_sync_available() -> bool {
    if !bridge_exists() {
        return false;
    }
    let cfg = zk_sync_config();
    bin_executable_regular(&cfg.bin)
}

// ---- errors & zero-credential scrubbing ----

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ZkSyncError {
    BridgeUnavailable,
    Unavailable(String),
    InvalidArgument(String),
    Timeout,
    OutputTooLarge,
    SyncFailed(String),
    CommandFailed { code: i32, message: String },
    InvalidOutput(String),
}

impl ZkSyncError {
    pub fn message(&self) -> String {
        match self {
            ZkSyncError::BridgeUnavailable => "Zotero bridge directory is unavailable.".to_string(),
            ZkSyncError::Unavailable(m) => {
                if m.is_empty() {
                    "zk-sync binary is unavailable.".to_string()
                } else {
                    format!("zk-sync unavailable: {m}")
                }
            }
            ZkSyncError::InvalidArgument(m) => format!("Invalid Zotero argument: {m}"),
            ZkSyncError::Timeout => "Zotero sync timed out.".to_string(),
            ZkSyncError::OutputTooLarge => "Zotero sync output exceeded size limit.".to_string(),
            ZkSyncError::SyncFailed(m) => {
                if m.is_empty() {
                    "Zotero sync failed.".to_string()
                } else {
                    m.clone()
                }
            }
            ZkSyncError::CommandFailed { code, message } => {
                if message.is_empty() {
                    format!("zk-sync exited with code {code}.")
                } else {
                    message.clone()
                }
            }
            ZkSyncError::InvalidOutput(m) => format!("Invalid zk-sync output: {m}"),
        }
    }
}

impl std::fmt::Display for ZkSyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for ZkSyncError {}

fn is_token_delim(b: u8) -> bool {
    b.is_ascii_whitespace()
        || matches!(b, b',' | b';' | b')' | b']' | b'}' | b'"' | b'\'' | b'>')
}

fn consume_secret_value(raw: &str, mut p: usize, out: &mut String) -> usize {
    let bytes = raw.as_bytes();
    if p >= bytes.len() {
        out.push_str("[REDACTED]");
        return p;
    }
    if bytes[p] == b'"' || bytes[p] == b'\'' {
        let quote = bytes[p];
        p += 1;
        while p < bytes.len() && bytes[p] != b'\r' && bytes[p] != b'\n' {
            if bytes[p] == b'\\' && p + 1 < bytes.len() {
                p += 2;
                continue;
            }
            if bytes[p] == quote {
                p += 1;
                break;
            }
            p += 1;
        }
        out.push(quote as char);
        out.push_str("[REDACTED]");
        out.push(quote as char);
        return p;
    }
    let start = p;
    while p < bytes.len() && !is_token_delim(bytes[p]) {
        p += 1;
    }
    let _ = start;
    out.push_str("[REDACTED]");
    p
}

fn scrub_keywords(raw: &str) -> String {
    const EXACT_KEYS: &[&str] = &[
        "zk_zotero_api_key",
        "zotero-api-key",
        "webdavpassword",
        "webdav_password",
        "webdavuser",
        "webdav_user",
        "webdavurl",
        "webdav_url",
        "apikey",
        "api_key",
        "password",
    ];
    const STOPWORDS: &[&str] = &["is", "was", "not", "missing", "required", "empty"];

    let bytes = raw.as_bytes();
    let mut out = String::with_capacity(raw.len());
    let mut i = 0;

    while i < bytes.len() {
        if !raw.is_char_boundary(i) {
            i += 1;
            continue;
        }

        // 1. Prefix marker: `ZK_WEBDAV_*`
        if i + 10 <= bytes.len() && raw[i..i + 10].eq_ignore_ascii_case("zk_webdav_") {
            let mut k = i + 10;
            while k < bytes.len() && (bytes[k].is_ascii_alphanumeric() || bytes[k] == b'_') {
                k += 1;
            }
            let mut p = k;
            let mut sep = String::new();
            if p < bytes.len() && (bytes[p] == b'"' || bytes[p] == b'\'') {
                sep.push(bytes[p] as char);
                p += 1;
            }
            while p < bytes.len() && (bytes[p] == b' ' || bytes[p] == b'\t') {
                sep.push(bytes[p] as char);
                p += 1;
            }
            if p < bytes.len() && (bytes[p] == b'=' || bytes[p] == b':') {
                sep.push(bytes[p] as char);
                p += 1;
                while p < bytes.len() && (bytes[p] == b' ' || bytes[p] == b'\t') {
                    sep.push(bytes[p] as char);
                    p += 1;
                }
                out.push_str(&raw[i..k]);
                out.push_str(&sep);
                i = consume_secret_value(raw, p, &mut out);
                continue;
            } else if !sep.is_empty() && p < bytes.len() && bytes[p] != b'\r' && bytes[p] != b'\n' {
                out.push_str(&raw[i..k]);
                out.push_str(&sep);
                i = consume_secret_value(raw, p, &mut out);
                continue;
            }
        }

        // 2. `Bearer <token>`
        let prev_word_char = i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
        if !prev_word_char
            && i + 6 < bytes.len()
            && raw[i..i + 6].eq_ignore_ascii_case("bearer")
            && (bytes[i + 6] == b' ' || bytes[i + 6] == b'\t')
        {
            let mut p = i + 6;
            while p < bytes.len() && (bytes[p] == b' ' || bytes[p] == b'\t') {
                p += 1;
            }
            out.push_str(&raw[i..i + 6]);
            out.push(' ');
            i = consume_secret_value(raw, p, &mut out);
            continue;
        }

        // 3. Exact sensitive key names
        let mut matched = false;
        for &kw in EXACT_KEYS {
            let k = i + kw.len();
            if k <= bytes.len()
                && raw[i..k].eq_ignore_ascii_case(kw)
                && (k == bytes.len() || (!bytes[k].is_ascii_alphanumeric() && bytes[k] != b'_'))
            {
                let mut p = k;
                let mut sep = String::new();
                if p < bytes.len() && (bytes[p] == b'"' || bytes[p] == b'\'') {
                    sep.push(bytes[p] as char);
                    p += 1;
                }
                while p < bytes.len() && (bytes[p] == b' ' || bytes[p] == b'\t') {
                    sep.push(bytes[p] as char);
                    p += 1;
                }
                if p < bytes.len() && (bytes[p] == b'=' || bytes[p] == b':') {
                    sep.push(bytes[p] as char);
                    p += 1;
                    while p < bytes.len() && (bytes[p] == b' ' || bytes[p] == b'\t') {
                        sep.push(bytes[p] as char);
                        p += 1;
                    }
                    out.push_str(&raw[i..k]);
                    out.push_str(&sep);
                    i = consume_secret_value(raw, p, &mut out);
                    matched = true;
                    break;
                } else if !sep.is_empty()
                    && sep.bytes().all(|b| b == b' ' || b == b'\t')
                    && p < bytes.len()
                    && bytes[p] != b'\r'
                    && bytes[p] != b'\n'
                {
                    let mut wend = p;
                    while wend < bytes.len() && !is_token_delim(bytes[wend]) {
                        wend += 1;
                    }
                    let next_tok = &raw[p..wend];
                    if !next_tok.is_empty()
                        && !STOPWORDS.iter().any(|sw| next_tok.eq_ignore_ascii_case(sw))
                    {
                        out.push_str(&raw[i..k]);
                        out.push(' ');
                        i = consume_secret_value(raw, p, &mut out);
                        matched = true;
                        break;
                    }
                }
            }
        }
        if matched {
            continue;
        }

        let ch = raw[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn is_url_delim(b: u8) -> bool {
    b.is_ascii_whitespace()
        || matches!(
            b,
            b'"' | b'\'' | b'<' | b'>' | b'(' | b')' | b'[' | b']' | b'{' | b'}' | b',' | b';'
        )
}

fn scrub_urls(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut pos = 0;

    while pos < bytes.len() {
        let Some(rel) = s[pos..].find("://") else {
            out.push_str(&s[pos..]);
            break;
        };
        let colon = pos + rel;
        let mut scheme_start = colon;
        while scheme_start > pos {
            let b = bytes[scheme_start - 1];
            if b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.') {
                scheme_start -= 1;
            } else {
                break;
            }
        }
        if scheme_start == colon || !bytes[scheme_start].is_ascii_alphabetic() {
            out.push_str(&s[pos..colon + 3]);
            pos = colon + 3;
            continue;
        }

        out.push_str(&s[pos..colon + 3]);
        let rest_start = colon + 3;
        let mut url_end = rest_start;
        while url_end < bytes.len() && !is_url_delim(bytes[url_end]) {
            url_end += 1;
        }
        // Trim trailing sentence period if any so `https://host/path.` preserves the trailing `.`
        while url_end > rest_start && bytes[url_end - 1] == b'.' {
            url_end -= 1;
        }
        let url_body = &s[rest_start..url_end];
        let auth_end = url_body.find(['/', '?', '#']).unwrap_or(url_body.len());
        let authority = &url_body[..auth_end];
        let tail = &url_body[auth_end..];

        if let Some(at_idx) = authority.rfind('@') {
            out.push_str("[REDACTED]@");
            out.push_str(&authority[at_idx + 1..]);
        } else {
            out.push_str(authority);
        }

        if let Some(q_idx) = tail.find('?') {
            out.push_str(&tail[..q_idx]);
            out.push_str("?[REDACTED]");
        } else {
            out.push_str(tail);
        }

        pos = url_end;
    }
    out
}

/// Scrub credentials, WebDAV URLs/userinfo, query tokens, API keys, and control characters,
/// capping length to `240` UTF-8 bytes (`Zero-Credential Invariant`).
pub fn scrub_error_message(raw: &str) -> String {
    let bounded = if raw.len() > ZK_STDERR_MAX {
        let mut end = ZK_STDERR_MAX;
        while end > 0 && !raw.is_char_boundary(end) {
            end -= 1;
        }
        &raw[..end]
    } else {
        raw
    };
    let pass1 = scrub_keywords(bounded);
    let pass2 = scrub_urls(&pass1);
    clean_text(&pass2, ZK_ERROR_MAX_BYTES)
}

// ---- subprocess runner (no shell, non-blocking poll, caps & timeout) ----

struct CmdOutput {
    code: i32,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn set_nonblocking(fd: i32) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags >= 0 {
            let _ = libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
}

/// Read all currently available bytes from a non-blocking pipe `fd` into `buf`, enforcing `cap`.
/// Returns `Ok(true)` while still open, `Ok(false)` on EOF, or `Err(())` if `cap` is exceeded.
fn drain_nonblocking_fd(fd: i32, buf: &mut Vec<u8>, cap: usize) -> Result<bool, ()> {
    let mut chunk = [0u8; 8192];
    loop {
        let n = unsafe { libc::read(fd, chunk.as_mut_ptr() as *mut libc::c_void, chunk.len()) };
        if n > 0 {
            let n = n as usize;
            if buf.len().saturating_add(n) > cap {
                return Err(());
            }
            if buf.try_reserve(n).is_err() {
                return Err(());
            }
            buf.extend_from_slice(&chunk[..n]);
        } else if n == 0 {
            return Ok(false);
        } else {
            let err = errno();
            if err == libc::EINTR {
                continue;
            }
            if err == libc::EAGAIN || err == libc::EWOULDBLOCK {
                return Ok(true);
            }
            return Ok(false);
        }
    }
}

fn run_zk_sync(cfg: &ZkSyncConfig, args: &[&str], timeout_ms: u64) -> Result<CmdOutput, ZkSyncError> {
    if !bin_executable_regular(&cfg.bin) {
        return Err(ZkSyncError::Unavailable(
            "zk-sync binary missing or not executable".to_string(),
        ));
    }
    if args.iter().any(|a| a.contains('\0'))
        || cfg.zk_home.contains('\0')
        || cfg.zk_config_dir.contains('\0')
    {
        return Err(ZkSyncError::InvalidArgument(
            "NUL byte in command argument or environment".to_string(),
        ));
    }

    let mut child = std::process::Command::new(&cfg.bin)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .env("ZK_HOME", &cfg.zk_home)
        .env("ZK_CONFIG_DIR", &cfg.zk_config_dir)
        .env("ZK_NO_UPDATES", "1")
        .spawn()
        .map_err(|e| ZkSyncError::Unavailable(scrub_error_message(&e.to_string())))?;

    let stdout_pipe = child
        .stdout
        .take()
        .ok_or_else(|| ZkSyncError::Unavailable("failed to capture stdout".to_string()))?;
    let stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| ZkSyncError::Unavailable("failed to capture stderr".to_string()))?;

    let out_fd = stdout_pipe.as_raw_fd();
    let err_fd = stderr_pipe.as_raw_fd();
    set_nonblocking(out_fd);
    set_nonblocking(err_fd);

    let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(1));
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut out_open = true;
    let mut err_open = true;

    while out_open || err_open {
        let now = Instant::now();
        if now >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(ZkSyncError::Timeout);
        }
        let rem_ms = deadline
            .saturating_duration_since(now)
            .as_millis()
            .clamp(1, 50) as i32;

        let mut pfds = [
            libc::pollfd {
                fd: if out_open { out_fd } else { -1 },
                events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
                revents: 0,
            },
            libc::pollfd {
                fd: if err_open { err_fd } else { -1 },
                events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
                revents: 0,
            },
        ];
        let _ = unsafe { libc::poll(pfds.as_mut_ptr(), 2, rem_ms) };

        if out_open {
            match drain_nonblocking_fd(out_fd, &mut stdout, ZK_STDOUT_MAX) {
                Ok(still_open) => out_open = still_open,
                Err(()) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(ZkSyncError::OutputTooLarge);
                }
            }
        }
        if err_open {
            match drain_nonblocking_fd(err_fd, &mut stderr, ZK_STDERR_MAX) {
                Ok(still_open) => err_open = still_open,
                Err(()) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(ZkSyncError::OutputTooLarge);
                }
            }
        }
    }

    drop(stdout_pipe);
    drop(stderr_pipe);

    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(ZkSyncError::Timeout);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ZkSyncError::Unavailable(scrub_error_message(&e.to_string())));
            }
        }
    };

    let code = status.code().unwrap_or(-1);
    Ok(CmdOutput { code, stdout, stderr })
}

fn combined_error_text(out: &CmdOutput) -> String {
    let err_s = String::from_utf8_lossy(&out.stderr);
    let out_s = String::from_utf8_lossy(&out.stdout);
    let raw = match (err_s.trim().is_empty(), out_s.trim().is_empty()) {
        (false, true) => err_s.into_owned(),
        (true, false) => out_s.into_owned(),
        (false, false) => format!("{} {}", err_s.trim(), out_s.trim()),
        (true, true) => String::new(),
    };
    scrub_error_message(&raw)
}

// ---- typed PDF geometry & zk-sync JSON wrappers (schema_version: 1) ----

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ZkPdfPageGeometry {
    pub page_index: u32,
    pub width_pt: f64,
    pub height_pt: f64,
    pub rotation: i32,
    pub page_label: String,
    pub crop_box: [f64; 4],
    pub user_to_page: [f64; 6],
}

impl ZkPdfPageGeometry {
    /// Map PDF user space `(u_x, u_y)` to top-down display page space `(q_x, q_y)` in points.
    pub fn user_to_display(&self, ux: f64, uy: f64) -> (f64, f64) {
        let [a, b, c, d, e, f] = self.user_to_page;
        (a * ux + c * uy + e, b * ux + d * uy + f)
    }

    /// Map top-down display page space `(q_x, q_y)` in points back to PDF user space `(u_x, u_y)`.
    pub fn display_to_user(&self, qx: f64, qy: f64) -> Option<(f64, f64)> {
        let [a, b, c, d, e, f] = self.user_to_page;
        let det = a * d - b * c;
        if !det.is_finite() || det.abs() < 1e-12 {
            return None;
        }
        let dx = qx - e;
        let dy = qy - f;
        Some(((d * dx - c * dy) / det, (-b * dx + a * dy) / det))
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ZkPdfInfo {
    pub schema_version: u32,
    pub attachment_key: String,
    pub parent_item_key: String,
    pub filename: String,
    pub cached: bool,
    pub fetched: bool,
    pub local_path: String,
    pub page_count: u32,
    pub pages: Vec<ZkPdfPageGeometry>,
}

fn jf64_tok(text: &[u8], tok: &JsonTok) -> Option<f64> {
    if tok.type_ != JsonType::Number {
        return None;
    }
    let s = std::str::from_utf8(&text[tok.start as usize..tok.end as usize]).ok()?;
    let v = s.parse::<f64>().ok()?;
    v.is_finite().then_some(if v == 0.0 { 0.0 } else { v })
}

fn jf64(text: &[u8], toks: &[JsonTok], obj: i32, key: &str) -> Option<f64> {
    let (_, tok) = crate::zotero_bridge::jtok(text, toks, obj, key)?;
    jf64_tok(text, tok)
}

fn jf64_fixed<const N: usize>(text: &[u8], toks: &[JsonTok], obj: i32, key: &str) -> Option<[f64; N]> {
    let (arr, atok) = crate::zotero_bridge::jtok(text, toks, obj, key)?;
    if atok.type_ != JsonType::Array || atok.size as usize != N {
        return None;
    }
    let mut out = [0.0f64; N];
    for i in 0..N {
        let it = json_item(toks, arr, i as i32);
        if it < 0 {
            return None;
        }
        out[i] = jf64_tok(text, &toks[it as usize])?;
    }
    Some(out)
}

fn ju32_req(text: &[u8], toks: &[JsonTok], obj: i32, key: &str) -> Option<u32> {
    let (_, tok) = crate::zotero_bridge::jtok(text, toks, obj, key)?;
    if tok.type_ != JsonType::Number {
        return None;
    }
    let s = std::str::from_utf8(&text[tok.start as usize..tok.end as usize]).ok()?;
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse::<u32>().ok()
}

fn valid_rotation(rot: i32) -> bool {
    matches!(rot, 0 | 90 | 180 | 270)
}

fn valid_cli_attachment_key(k: &str) -> bool {
    !k.starts_with('-') && attachment_key_valid(k)
}

fn parse_page_geometry(bytes: &[u8], toks: &[JsonTok], pt: i32) -> Option<ZkPdfPageGeometry> {
    if toks.get(pt as usize)?.type_ != JsonType::Object {
        return None;
    }
    let page_index = ju32_req(bytes, toks, pt, "page_index")?;
    if page_index > 100_000 {
        return None;
    }
    let width_pt = jf64(bytes, toks, pt, "width_pt")?;
    let height_pt = jf64(bytes, toks, pt, "height_pt")?;
    if !(width_pt > 0.0 && height_pt > 0.0) {
        return None;
    }
    let rotation = crate::zotero_bridge::jint(bytes, toks, pt, "rotation")?;
    if !valid_rotation(rotation) {
        return None;
    }
    let page_label = match crate::zotero_bridge::jtok(bytes, toks, pt, "page_label") {
        None => String::new(),
        Some((_, t)) if t.type_ == JsonType::Null => String::new(),
        Some(_) => clean_text(
            &crate::zotero_bridge::jstr_strict(bytes, toks, pt, "page_label", 128)?,
            128,
        ),
    };
    let crop_box = jf64_fixed::<4>(bytes, toks, pt, "crop_box")?;
    let user_to_page = jf64_fixed::<6>(bytes, toks, pt, "user_to_page")?;
    Some(ZkPdfPageGeometry {
        page_index,
        width_pt,
        height_pt,
        rotation,
        page_label,
        crop_box,
        user_to_page,
    })
}

fn parse_pages_array(bytes: &[u8], toks: &[JsonTok], obj: i32, expected_count: u32) -> Option<Vec<ZkPdfPageGeometry>> {
    if expected_count > 100_000 {
        return None;
    }
    let (arr, atok) = crate::zotero_bridge::jtok(bytes, toks, obj, "pages")?;
    if atok.type_ != JsonType::Array || atok.size < 0 || atok.size as u32 != expected_count {
        return None;
    }
    let mut pages = Vec::new();
    if pages.try_reserve_exact(expected_count as usize).is_err() {
        return None;
    }
    for i in 0..atok.size {
        let pt = json_item(toks, arr, i);
        if pt < 0 {
            return None;
        }
        pages.push(parse_page_geometry(bytes, toks, pt)?);
    }
    Some(pages)
}

/// Optionally pull remote Zotero changes (`pull`, falling back once to `sync` on exit `2`) and
/// atomically export `<bridge_dir>/library_index.json`, returning the freshly reloaded catalog.
pub fn zk_sync_refresh_index(sync_remote: bool) -> Result<Arc<ZoteroLibraryIndex>, ZkSyncError> {
    let bdir = bridge_dir();
    if bdir.is_empty() || !crate::zotero_bridge::ensure_dir(&bdir) {
        return Err(ZkSyncError::BridgeUnavailable);
    }
    let index_path = bridge_path_in(&bdir, LIBRARY_INDEX_FILE).ok_or(ZkSyncError::BridgeUnavailable)?;
    let cfg = zk_sync_config();

    if sync_remote {
        let mut pull_out = run_zk_sync(&cfg, &["pull"], cfg.network_timeout_ms)?;
        if pull_out.code == 2 {
            pull_out = run_zk_sync(&cfg, &["sync"], cfg.network_timeout_ms)?;
        }
        if pull_out.code != 0 {
            return Err(ZkSyncError::SyncFailed(combined_error_text(&pull_out)));
        }
    }

    let exp_out = run_zk_sync(&cfg, &["export-index", &index_path], cfg.local_timeout_ms)?;
    if exp_out.code != 0 {
        return Err(ZkSyncError::CommandFailed {
            code: exp_out.code,
            message: combined_error_text(&exp_out),
        });
    }
    if exp_out.stdout.contains(&0) || std::str::from_utf8(&exp_out.stdout).is_err() {
        return Err(ZkSyncError::InvalidOutput(
            "export-index stdout is not valid UTF-8".to_string(),
        ));
    }
    let (toks, _) = crate::zotero_bridge::parse_json(&exp_out.stdout).ok_or_else(|| {
        ZkSyncError::InvalidOutput("export-index stdout is not valid JSON".to_string())
    })?;
    if !crate::zotero_bridge::schema_ok(&exp_out.stdout, &toks, 0) {
        return Err(ZkSyncError::InvalidOutput(
            "export-index stdout missing schema_version 1".to_string(),
        ));
    }

    library_index_reload().ok_or_else(|| {
        ZkSyncError::InvalidOutput("exported library_index.json missing or invalid".to_string())
    })
}

/// Inspect cache status and per-page PDF geometry for `attachment_key` via `zk-sync pdf-info`,
/// optionally fetching the PDF first when `fetch == true`.
pub fn zk_sync_pdf_info(attachment_key: &str, fetch: bool) -> Result<ZkPdfInfo, ZkSyncError> {
    if !valid_cli_attachment_key(attachment_key) {
        return Err(ZkSyncError::InvalidArgument(
            "invalid attachment_key".to_string(),
        ));
    }
    let cfg = zk_sync_config();
    let out = if fetch {
        run_zk_sync(&cfg, &["pdf-info", attachment_key, "--fetch"], cfg.network_timeout_ms)?
    } else {
        run_zk_sync(&cfg, &["pdf-info", attachment_key], cfg.local_timeout_ms)?
    };
    if out.code != 0 {
        return Err(ZkSyncError::CommandFailed {
            code: out.code,
            message: combined_error_text(&out),
        });
    }
    if out.stdout.contains(&0) || std::str::from_utf8(&out.stdout).is_err() {
        return Err(ZkSyncError::InvalidOutput(
            "pdf-info stdout is not valid UTF-8".to_string(),
        ));
    }
    let bytes = &out.stdout;
    let (toks, _) = crate::zotero_bridge::parse_json(bytes).ok_or_else(|| {
        ZkSyncError::InvalidOutput("pdf-info stdout is not valid JSON".to_string())
    })?;
    if !crate::zotero_bridge::schema_ok(bytes, &toks, 0) {
        return Err(ZkSyncError::InvalidOutput(
            "pdf-info unsupported schema_version".to_string(),
        ));
    }
    let parsed_att = crate::zotero_bridge::jstr_strict(bytes, &toks, 0, "attachment_key", 32)
        .ok_or_else(|| ZkSyncError::InvalidOutput("missing attachment_key".to_string()))?;
    if !valid_cli_attachment_key(&parsed_att) || parsed_att != attachment_key {
        return Err(ZkSyncError::InvalidOutput(
            "mismatched or invalid attachment_key".to_string(),
        ));
    }
    let parent_item_key = match crate::zotero_bridge::jtok(bytes, &toks, 0, "parent_item_key") {
        None => String::new(),
        Some((_, t)) if t.type_ == JsonType::Null => String::new(),
        Some(_) => {
            let pk = crate::zotero_bridge::jstr_strict(bytes, &toks, 0, "parent_item_key", 32)
                .ok_or_else(|| ZkSyncError::InvalidOutput("invalid parent_item_key".to_string()))?;
            if !pk.is_empty() && !item_key_valid(&pk) {
                return Err(ZkSyncError::InvalidOutput("invalid parent_item_key".to_string()));
            }
            pk
        }
    };
    let filename = match crate::zotero_bridge::jtok(bytes, &toks, 0, "filename") {
        None => String::new(),
        Some((_, t)) if t.type_ == JsonType::Null => String::new(),
        Some(_) => crate::zotero_bridge::jclean_path(bytes, &toks, 0, "filename", 512)
            .ok_or_else(|| ZkSyncError::InvalidOutput("invalid filename".to_string()))?,
    };
    let cached = crate::zotero_bridge::jbool(bytes, &toks, 0, "cached")
        .ok_or_else(|| ZkSyncError::InvalidOutput("missing cached flag".to_string()))?;
    let fetched = crate::zotero_bridge::jbool(bytes, &toks, 0, "fetched")
        .ok_or_else(|| ZkSyncError::InvalidOutput("missing fetched flag".to_string()))?;
    let local_path = crate::zotero_bridge::jclean_path(bytes, &toks, 0, "local_path", 1024)
        .ok_or_else(|| ZkSyncError::InvalidOutput("invalid local_path".to_string()))?;
    let page_count = ju32_req(bytes, &toks, 0, "page_count")
        .ok_or_else(|| ZkSyncError::InvalidOutput("invalid page_count".to_string()))?;
    let pages = parse_pages_array(bytes, &toks, 0, page_count)
        .ok_or_else(|| ZkSyncError::InvalidOutput("invalid pages geometry".to_string()))?;

    if cached {
        if local_path.is_empty() {
            return Err(ZkSyncError::InvalidOutput(
                "cached PDF has empty local_path".to_string(),
            ));
        }
        match std::fs::symlink_metadata(&local_path) {
            Ok(md) if !md.file_type().is_symlink() && md.is_file() => {}
            _ => {
                return Err(ZkSyncError::InvalidOutput(
                    "cached PDF local_path is missing or not a regular file".to_string(),
                ));
            }
        }
    }

    Ok(ZkPdfInfo {
        schema_version: SCHEMA_VERSION as u32,
        attachment_key: parsed_att,
        parent_item_key,
        filename,
        cached,
        fetched,
        local_path,
        page_count,
        pages,
    })
}

// ---- Phase 4: two-way ink sync wrappers (`ink-pull`, `ink-upsert`, `push`) ----

use crate::zotero_ink::{
    apply_ink_sync, ink_sync_save, parse_ink_pull, parse_upsert_result, plan_ink_sync,
    serialize_upsert_payload, InkSyncState, InkSyncSummary, LocalBucket, PageStamp, ZkInkPull,
    ZkUpsertPayload, ZkUpsertResult,
};

pub const ZK_UPSERT_MAX_BYTES: usize = 10 * 1024 * 1024;

fn check_att_key(attachment_key: &str) -> Result<(), ZkSyncError> {
    if valid_cli_attachment_key(attachment_key) {
        Ok(())
    } else {
        Err(ZkSyncError::InvalidArgument("invalid attachment_key".to_string()))
    }
}

fn checked_stdout(out: &CmdOutput, what: &str) -> Result<(), ZkSyncError> {
    if out.code != 0 {
        return Err(ZkSyncError::CommandFailed {
            code: out.code,
            message: combined_error_text(out),
        });
    }
    if out.stdout.contains(&0) || std::str::from_utf8(&out.stdout).is_err() {
        return Err(ZkSyncError::InvalidOutput(format!("{what} stdout is not valid UTF-8")));
    }
    Ok(())
}

/// `zk-sync ink-pull <key> [--local-only]`: ink + highlight annotations for an attachment.
pub fn zk_sync_ink_pull(attachment_key: &str, local_only: bool) -> Result<ZkInkPull, ZkSyncError> {
    check_att_key(attachment_key)?;
    let cfg = zk_sync_config();
    let out = if local_only {
        run_zk_sync(&cfg, &["ink-pull", attachment_key, "--local-only"], cfg.local_timeout_ms)?
    } else {
        run_zk_sync(&cfg, &["ink-pull", attachment_key], cfg.network_timeout_ms)?
    };
    checked_stdout(&out, "ink-pull")?;
    let pull = parse_ink_pull(&out.stdout)
        .map_err(|e| ZkSyncError::InvalidOutput(scrub_error_message(&e)))?;
    if pull.attachment_key != attachment_key {
        return Err(ZkSyncError::InvalidOutput(
            "mismatched or invalid attachment_key".to_string(),
        ));
    }
    Ok(pull)
}

/// Removes the wrapped temp file on drop (every path).
struct TempFileGuard(std::ffi::CString);

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        unsafe {
            libc::unlink(self.0.as_ptr());
        }
    }
}

fn write_private_temp(data: &[u8]) -> Result<(TempFileGuard, String), ZkSyncError> {
    let pid = std::process::id();
    let nonce = crate::zotero_bridge::WRITE_NONCE.fetch_add(1, Ordering::Relaxed);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let dir = std::env::temp_dir();
    let path = dir.join(format!("grimoire-zk-ink-{pid}-{:x}{:x}.json", stamp, nonce));
    let path_s = path.to_string_lossy().into_owned();
    let cpath = std::ffi::CString::new(path_s.as_str())
        .map_err(|_| ZkSyncError::InvalidArgument("invalid temp path".to_string()))?;
    unsafe {
        let fd = libc::open(
            cpath.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600 as libc::c_uint,
        );
        if fd < 0 {
            return Err(ZkSyncError::Unavailable(format!(
                "open temp file failed: {}",
                strerror(errno())
            )));
        }
        let guard = TempFileGuard(cpath);
        let mut left = data;
        while !left.is_empty() {
            let n = libc::write(fd, left.as_ptr() as *const libc::c_void, left.len());
            if n < 0 {
                if errno() == libc::EINTR {
                    continue;
                }
                let msg = format!("write temp file failed: {}", strerror(errno()));
                libc::close(fd);
                return Err(ZkSyncError::Unavailable(msg));
            }
            left = &left[n as usize..];
        }
        if libc::fsync(fd) != 0 {
            let msg = format!("fsync temp file failed: {}", strerror(errno()));
            libc::close(fd);
            return Err(ZkSyncError::Unavailable(msg));
        }
        if libc::close(fd) != 0 {
            return Err(ZkSyncError::Unavailable("close temp file failed".to_string()));
        }
        Ok((guard, path_s))
    }
}

/// `zk-sync ink-upsert <key> <payload.json>`: write Grimoire-owned ink into the local Zotero DB.
pub fn zk_sync_ink_upsert(
    attachment_key: &str,
    payload: &ZkUpsertPayload,
) -> Result<ZkUpsertResult, ZkSyncError> {
    check_att_key(attachment_key)?;
    let json = serialize_upsert_payload(payload)
        .map_err(|e| ZkSyncError::InvalidArgument(scrub_error_message(&e)))?;
    if json.len() > ZK_UPSERT_MAX_BYTES {
        return Err(ZkSyncError::InvalidArgument("ink payload too large".to_string()));
    }
    let (_guard, path) = write_private_temp(json.as_bytes())?;
    let cfg = zk_sync_config();
    let out = run_zk_sync(&cfg, &["ink-upsert", attachment_key, &path], cfg.local_timeout_ms)?;
    checked_stdout(&out, "ink-upsert")?;
    let res = parse_upsert_result(&out.stdout)
        .map_err(|e| ZkSyncError::InvalidOutput(scrub_error_message(&e)))?;
    if !res.attachment_key.is_empty() && res.attachment_key != attachment_key {
        return Err(ZkSyncError::InvalidOutput(
            "mismatched or invalid attachment_key".to_string(),
        ));
    }
    Ok(res)
}

/// `zk-sync push`: send pending local changes to Zotero (plain-text output; exit 1 on failure).
pub fn zk_sync_push() -> Result<(), ZkSyncError> {
    let cfg = zk_sync_config();
    let out = run_zk_sync(&cfg, &["push"], cfg.network_timeout_ms)?;
    if out.code != 0 {
        return Err(ZkSyncError::CommandFailed {
            code: out.code,
            message: combined_error_text(&out),
        });
    }
    Ok(())
}

#[derive(Clone, Debug, Default)]
pub struct InkSyncJob {
    pub nb_id: String,
    pub attachment_key: String,
    pub state: InkSyncState,
    pub local: Vec<LocalBucket>,
    pub fresh_stamps: Vec<PageStamp>,
}

#[derive(Clone, Debug, Default)]
pub struct InkSyncOutcome {
    pub nb_id: String,
    pub state: InkSyncState,
    pub summary: InkSyncSummary,
    pub save_error: Option<String>,
}

fn ink_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Synchronous two-way ink sync: pull, plan, upsert, push, apply, persist state.
pub fn run_ink_sync(job: InkSyncJob) -> Result<InkSyncOutcome, ZkSyncError> {
    let att = job.attachment_key.as_str();
    let mut offline = false;
    let pull = match zk_sync_ink_pull(att, false) {
        Ok(p) => p,
        Err(e @ (ZkSyncError::InvalidArgument(_) | ZkSyncError::Unavailable(_))) => return Err(e),
        Err(_) => {
            offline = true;
            zk_sync_ink_pull(att, true)?
        }
    };
    let plan = plan_ink_sync(&job.state, &pull, &job.local);
    let result = if !plan.payload.is_empty() {
        Some(zk_sync_ink_upsert(att, &plan.payload)?)
    } else {
        None
    };
    let changed = result
        .as_ref()
        .is_some_and(|r| r.created + r.updated + r.deleted > 0);
    let dirty = changed || pull.ink.iter().any(|i| i.dirty);
    let mut pushed = false;
    if dirty && !offline {
        match zk_sync_push() {
            Ok(()) => pushed = true,
            Err(_) => offline = true,
        }
    }
    let now = ink_now_ms();
    let (mut st, mut summary) =
        apply_ink_sync(&job.state, &pull, &plan, result.as_ref(), &job.local, now);
    if pushed {
        if let Ok(fresh) = zk_sync_ink_pull(att, true) {
            for b in st.local.iter_mut() {
                if let Some(f) = fresh.ink.iter().find(|f| f.key == b.key) {
                    b.version = f.version;
                }
            }
            for r in st.remote_ink.iter_mut() {
                if let Some(f) = fresh.ink.iter().find(|f| f.key == r.key) {
                    r.version = f.version;
                }
            }
        }
    }
    // Page stamps: only trust a fresh stamp whose buckets were all actually synced or suppressed.
    let mut stamps: Vec<PageStamp> = st
        .page_stamps
        .iter()
        .filter(|s| !plan.stale_pages.contains(&s.page_id))
        .cloned()
        .collect();
    for fs in &job.fresh_stamps {
        let covered = fs.buckets.iter().all(|(bucket, hash, _, _, _)| {
            st.local
                .iter()
                .any(|b| b.page_id == fs.page_id && &b.bucket == bucket && &b.hash == hash)
                || st
                    .suppressed
                    .iter()
                    .any(|s| s.page_id == fs.page_id && &s.bucket == bucket && &s.hash == hash)
        });
        stamps.retain(|s| s.page_id != fs.page_id);
        if covered {
            stamps.push(fs.clone());
        }
    }
    st.page_stamps = stamps;
    st.attachment_key = att.to_string();
    st.schema_version = 1;
    st.last_sync_ms = now;
    summary.offline = offline;
    summary.pushed = pushed;
    let save_error = ink_sync_save(&job.nb_id, &st).err().map(|e| scrub_error_message(&e));
    Ok(InkSyncOutcome { nb_id: job.nb_id, state: st, summary, save_error })
}

// ---- non-blocking single-flight background runner for UI ----

#[derive(Clone, Debug)]
pub enum ZkBgDone {
    Refresh(Result<Arc<ZoteroLibraryIndex>, ZkSyncError>),
    PdfInfo {
        item: ZoteroLibraryItem,
        result: Result<ZkPdfInfo, ZkSyncError>,
    },
    InkSync {
        nb_id: String,
        result: Result<InkSyncOutcome, ZkSyncError>,
    },
}

enum ZkBgState {
    Running { id: u64, status: String },
    Done(ZkBgDone),
}

/// Handle namespace for the single-flight background runner (`zk_bg_*`).
#[derive(Clone, Copy, Debug, Default)]
pub struct ZkBgRunner;

static BG_NEXT_ID: AtomicU64 = AtomicU64::new(1);
static BG_LAST_SYNC_REMOTE: AtomicBool = AtomicBool::new(false);
static BG_STATE: Mutex<Option<ZkBgState>> = Mutex::new(None);

fn finish_bg(id: u64, done: ZkBgDone) {
    if let Ok(mut g) = BG_STATE.lock() {
        if matches!(*g, Some(ZkBgState::Running { id: cur, .. }) if cur == id) {
            *g = Some(ZkBgState::Done(done));
        }
    }
}

/// Start a single-flight background catalog refresh (`export-index` or `pull` + `export-index`).
/// Returns `false` if another background task is already running.
pub fn zk_bg_start_refresh(sync_remote: bool) -> bool {
    let id = BG_NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let status = if sync_remote {
        "Syncing Zotero library...".to_string()
    } else {
        "Refreshing Zotero catalog...".to_string()
    };
    {
        let Ok(mut g) = BG_STATE.lock() else { return false };
        if matches!(*g, Some(ZkBgState::Running { .. })) {
            return false;
        }
        BG_LAST_SYNC_REMOTE.store(sync_remote, Ordering::Relaxed);
        *g = Some(ZkBgState::Running { id, status });
    }
    let spawned = std::thread::Builder::new()
        .name("zk-bg-refresh".to_string())
        .spawn(move || {
            let res = zk_sync_refresh_index(sync_remote);
            finish_bg(id, ZkBgDone::Refresh(res));
        });
    if spawned.is_err() {
        if let Ok(mut g) = BG_STATE.lock() {
            *g = None;
        }
        return false;
    }
    true
}

/// Whether the most recently started background refresh requested remote WebDAV/Zotero sync.
pub fn zk_bg_last_refresh_was_sync() -> bool {
    BG_LAST_SYNC_REMOTE.load(Ordering::Relaxed)
}

/// Reset any finished or stale background runner state (used by test harnesses).
pub fn zk_bg_reset() {
    if let Ok(mut g) = BG_STATE.lock() {
        *g = None;
    }
}

/// Start a single-flight background `pdf-info` check (or `--fetch` download) for `item`.
/// Returns `false` if another background task is already running.
pub fn zk_bg_start_pdf_info(item: ZoteroLibraryItem, fetch: bool) -> bool {
    let id = BG_NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let status = if fetch {
        "Downloading PDF from Zotero...".to_string()
    } else {
        "Checking PDF...".to_string()
    };
    {
        let Ok(mut g) = BG_STATE.lock() else { return false };
        if matches!(*g, Some(ZkBgState::Running { .. })) {
            return false;
        }
        *g = Some(ZkBgState::Running { id, status });
    }
    let spawned = std::thread::Builder::new()
        .name("zk-bg-pdf-info".to_string())
        .spawn(move || {
            let att = item.attachment_key.clone();
            let res = zk_sync_pdf_info(&att, fetch);
            finish_bg(id, ZkBgDone::PdfInfo { item, result: res });
        });
    if spawned.is_err() {
        if let Ok(mut g) = BG_STATE.lock() {
            *g = None;
        }
        return false;
    }
    true
}

/// Start a single-flight background two-way ink sync. Returns `false` if another task is running.
pub fn zk_bg_start_ink_sync(job: InkSyncJob) -> bool {
    let id = BG_NEXT_ID.fetch_add(1, Ordering::Relaxed);
    {
        let Ok(mut g) = BG_STATE.lock() else { return false };
        if matches!(*g, Some(ZkBgState::Running { .. })) {
            return false;
        }
        *g = Some(ZkBgState::Running { id, status: "Syncing Zotero ink...".to_string() });
    }
    let spawned = std::thread::Builder::new()
        .name("zk-bg-ink-sync".to_string())
        .spawn(move || {
            let nb_id = job.nb_id.clone();
            let res = run_ink_sync(job);
            finish_bg(id, ZkBgDone::InkSync { nb_id, result: res });
        });
    if spawned.is_err() {
        if let Ok(mut g) = BG_STATE.lock() {
            *g = None;
        }
        return false;
    }
    true
}

/// Whether a background `zk-sync` task is currently executing.
pub fn zk_bg_busy() -> bool {
    BG_STATE
        .lock()
        .map(|g| matches!(*g, Some(ZkBgState::Running { .. })))
        .unwrap_or(false)
}

/// Human-readable status text while a background `zk-sync` task is running.
pub fn zk_bg_status_text() -> Option<String> {
    let g = BG_STATE.lock().ok()?;
    match *g {
        Some(ZkBgState::Running { ref status, .. }) => Some(status.clone()),
        _ => None,
    }
}

/// Take the completed background task result, if one has finished.
pub fn zk_bg_poll() -> Option<ZkBgDone> {
    let mut g = BG_STATE.lock().ok()?;
    if matches!(*g, Some(ZkBgState::Done(_))) {
        if let Some(ZkBgState::Done(done)) = g.take() {
            return Some(done);
        }
    }
    None
}

// ---- notebook Zotero binding sidecar (`notebooks/<nb_id>/zotero_source.json`) ----

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ZoteroNotebookSource {
    pub schema_version: u32,
    pub item_key: String,
    pub attachment_key: String,
    pub citation_key: String,
    pub title: String,
    pub authors: String,
    pub year: String,
    pub pdf_local_path: String,
    pub page_count: u32,
    pub pages: Vec<ZkPdfPageGeometry>,
}

pub(crate) fn notebook_dir_verified(nb_id: &str) -> Result<String, String> {
    if !notebook_id_valid(nb_id) {
        return Err("invalid notebook id".to_string());
    }
    let root = store_root();
    let trimmed = root.trim_end_matches('/');
    if trimmed.is_empty() {
        return Err("store root is not initialized".to_string());
    }
    let dir = format!("{trimmed}/notebooks/{nb_id}");
    if !crate::zotero_bridge::check_dir_existing(&dir) {
        return Err("notebook directory does not exist or is a symlink".to_string());
    }
    Ok(dir)
}

fn json_f64(v: f64) -> Result<String, String> {
    if !v.is_finite() {
        return Err("non-finite float in geometry".to_string());
    }
    let norm = if v == 0.0 { 0.0 } else { v };
    Ok(format!("{norm:?}"))
}

fn json_f64_slice(vals: &[f64]) -> Result<String, String> {
    let mut parts = Vec::with_capacity(vals.len());
    for &v in vals {
        parts.push(json_f64(v)?);
    }
    Ok(format!("[{}]", parts.join(",")))
}

fn serialize_page_geometry(pg: &ZkPdfPageGeometry) -> Result<String, String> {
    if pg.page_index > 100_000 {
        return Err("page_index out of range".to_string());
    }
    if !(pg.width_pt.is_finite() && pg.width_pt > 0.0 && pg.height_pt.is_finite() && pg.height_pt > 0.0) {
        return Err("page dimensions must be positive finite numbers".to_string());
    }
    if !valid_rotation(pg.rotation) {
        return Err("page rotation must be 0, 90, 180, or 270".to_string());
    }
    if pg.page_label.len() > 128 || pg.page_label.bytes().any(|b| b == 0 || b < 0x20 || b == 0x7f) {
        return Err("invalid page_label".to_string());
    }
    let w = json_f64(pg.width_pt)?;
    let h = json_f64(pg.height_pt)?;
    let crop = json_f64_slice(&pg.crop_box)?;
    let u2p = json_f64_slice(&pg.user_to_page)?;
    Ok(json_members(&[
        json_int_field("page_index", pg.page_index as i64),
        format!("\"width_pt\":{w}"),
        format!("\"height_pt\":{h}"),
        json_int_field("rotation", pg.rotation as i64),
        json_field("page_label", &pg.page_label),
        format!("\"crop_box\":{crop}"),
        format!("\"user_to_page\":{u2p}"),
    ]))
}

fn serialize_zotero_source(src: &ZoteroNotebookSource) -> Result<String, String> {
    if !valid_cli_attachment_key(&src.attachment_key) {
        return Err("invalid attachment_key".to_string());
    }
    if !src.item_key.is_empty() && !item_key_valid(&src.item_key) {
        return Err("invalid item_key".to_string());
    }
    if src.citation_key.len() > 256 || src.citation_key.contains('\0') {
        return Err("invalid citation_key".to_string());
    }
    if src.title.len() > 512 || src.title.contains('\0') {
        return Err("invalid title".to_string());
    }
    if src.authors.len() > 256 || src.authors.contains('\0') {
        return Err("invalid authors".to_string());
    }
    if src.year.len() > 256 || src.year.contains('\0') {
        return Err("invalid year".to_string());
    }
    if src.pdf_local_path.len() > 1024
        || src
            .pdf_local_path
            .bytes()
            .any(|b| b == 0 || b == b'\r' || b == b'\n' || b < 0x20 || b == 0x7f)
    {
        return Err("invalid pdf_local_path".to_string());
    }
    if src.pages.len() > 100_000 {
        return Err("too many pages".to_string());
    }
    let page_count = if src.page_count == 0 && !src.pages.is_empty() {
        src.pages.len() as u32
    } else {
        if src.page_count as usize != src.pages.len() {
            return Err("page_count does not match pages.len()".to_string());
        }
        src.page_count
    };

    let mut page_objs = Vec::with_capacity(src.pages.len());
    for pg in &src.pages {
        page_objs.push(serialize_page_geometry(pg)?);
    }

    let doc = json_members(&[
        json_int_field("schema_version", SCHEMA_VERSION as i64),
        json_field("item_key", &src.item_key),
        json_field("attachment_key", &src.attachment_key),
        json_field("citation_key", &clean_text(&src.citation_key, 256)),
        json_field("title", &clean_text(&src.title, 512)),
        json_field("authors", &clean_text(&src.authors, 256)),
        json_field("year", &clean_text(&src.year, 256)),
        json_field("pdf_local_path", &src.pdf_local_path),
        json_int_field("page_count", page_count as i64),
        format!("\"pages\":[{}]", page_objs.join(",")),
    ]);
    if doc.len() > ZOTERO_SOURCE_MAX {
        return Err("serialized zotero_source.json exceeds 2 MiB cap".to_string());
    }
    Ok(doc)
}

pub(crate) fn write_atomic_in_existing_dir(dir: &str, filename: &str, data: &[u8]) -> Result<(), String> {
    let path = format!("{dir}/{filename}");
    if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err("refusing to overwrite symlink at target path".to_string());
    }
    let pid = std::process::id();
    let nonce = crate::zotero_bridge::WRITE_NONCE.fetch_add(1, Ordering::Relaxed);
    let stamp = now_ms() as u64;
    let tmp = format!("{path}.tmp.{pid}.{:x}{:x}", stamp ^ nonce, nonce);
    let ctmp = std::ffi::CString::new(tmp.as_str()).map_err(|_| "invalid temp path".to_string())?;
    let cpath = std::ffi::CString::new(path.as_str()).map_err(|_| "invalid target path".to_string())?;

    unsafe {
        let fd = libc::open(
            ctmp.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o644,
        );
        if fd < 0 {
            return Err(format!("open temp file failed: {}", strerror(errno())));
        }
        let mut left = data;
        while !left.is_empty() {
            let n = libc::write(fd, left.as_ptr() as *const libc::c_void, left.len());
            if n < 0 {
                if errno() == libc::EINTR {
                    continue;
                }
                let msg = format!("write temp file failed: {}", strerror(errno()));
                libc::close(fd);
                libc::unlink(ctmp.as_ptr());
                return Err(msg);
            }
            left = &left[n as usize..];
        }
        if libc::fsync(fd) != 0 {
            let msg = format!("fsync temp file failed: {}", strerror(errno()));
            libc::close(fd);
            libc::unlink(ctmp.as_ptr());
            return Err(msg);
        }
        if libc::close(fd) != 0 {
            let msg = format!("close temp file failed: {}", strerror(errno()));
            libc::unlink(ctmp.as_ptr());
            return Err(msg);
        }
        if libc::rename(ctmp.as_ptr(), cpath.as_ptr()) != 0 {
            let msg = format!("rename temp file failed: {}", strerror(errno()));
            libc::unlink(ctmp.as_ptr());
            return Err(msg);
        }
        if let Ok(cdir) = std::ffi::CString::new(dir) {
            let dfd = libc::open(
                cdir.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            );
            if dfd >= 0 {
                let _ = libc::fsync(dfd);
                libc::close(dfd);
            }
        }
    }
    Ok(())
}

/// Atomically save `<store_root>/notebooks/<nb_id>/zotero_source.json`.
pub fn zotero_source_save(nb_id: &str, src: &ZoteroNotebookSource) -> Result<(), String> {
    let dir = notebook_dir_verified(nb_id)?;
    let serialized = serialize_zotero_source(src)?;
    write_atomic_in_existing_dir(&dir, ZOTERO_SOURCE_FILE, serialized.as_bytes())
}

fn parse_zotero_source(bytes: &[u8]) -> Option<ZoteroNotebookSource> {
    let (toks, _) = crate::zotero_bridge::parse_json(bytes)?;
    if !crate::zotero_bridge::schema_ok(bytes, &toks, 0) {
        return None;
    }
    let attachment_key = crate::zotero_bridge::jstr_strict(bytes, &toks, 0, "attachment_key", 32)?;
    if !valid_cli_attachment_key(&attachment_key) {
        return None;
    }
    let item_key = match crate::zotero_bridge::jtok(bytes, &toks, 0, "item_key") {
        None => String::new(),
        Some((_, t)) if t.type_ == JsonType::Null => String::new(),
        Some(_) => {
            let ik = crate::zotero_bridge::jstr_strict(bytes, &toks, 0, "item_key", 32)?;
            if !ik.is_empty() && !item_key_valid(&ik) {
                return None;
            }
            ik
        }
    };
    let citation_key = clean_text(
        &crate::zotero_bridge::jstr_strict(bytes, &toks, 0, "citation_key", 256).unwrap_or_default(),
        256,
    );
    let title = clean_text(
        &crate::zotero_bridge::jstr_strict(bytes, &toks, 0, "title", 512).unwrap_or_default(),
        512,
    );
    let authors = clean_text(
        &crate::zotero_bridge::jstr_strict(bytes, &toks, 0, "authors", 256).unwrap_or_default(),
        256,
    );
    let year = clean_text(
        &crate::zotero_bridge::jstr_strict(bytes, &toks, 0, "year", 256).unwrap_or_default(),
        256,
    );
    let pdf_local_path = match crate::zotero_bridge::jtok(bytes, &toks, 0, "pdf_local_path") {
        None => String::new(),
        Some((_, t)) if t.type_ == JsonType::Null => String::new(),
        Some(_) => crate::zotero_bridge::jclean_path(bytes, &toks, 0, "pdf_local_path", 1024)?,
    };
    let page_count = ju32_req(bytes, &toks, 0, "page_count")?;
    let pages = parse_pages_array(bytes, &toks, 0, page_count)?;

    Some(ZoteroNotebookSource {
        schema_version: SCHEMA_VERSION as u32,
        item_key,
        attachment_key,
        citation_key,
        title,
        authors,
        year,
        pdf_local_path,
        page_count,
        pages,
    })
}

/// Safely load `<store_root>/notebooks/<nb_id>/zotero_source.json` (rejecting symlinks and files `> 2 MiB`).
pub fn zotero_source_load(nb_id: &str) -> Option<ZoteroNotebookSource> {
    let dir = notebook_dir_verified(nb_id).ok()?;
    let path = format!("{dir}/{ZOTERO_SOURCE_FILE}");
    let bytes = match bridge_read(&path, ZOTERO_SOURCE_MAX) {
        Ok(Some(b)) => b,
        _ => return None,
    };
    parse_zotero_source(&bytes)
}

/// Find the newest valid notebook bound to `attachment_key` via `zotero_source.json`.
/// Uses `store_list`, so trashed notebooks and incomplete `.import-*` directories are ignored.
pub fn find_notebook_by_attachment_key(attachment_key: &str) -> Option<String> {
    if !valid_cli_attachment_key(attachment_key) {
        return None;
    }
    let mut notebooks = Vec::new();
    if store_list(&mut notebooks) != 0 {
        return None;
    }
    for nb in &notebooks {
        let id = nb.id.as_str();
        if let Some(src) = zotero_source_load(id) {
            if src.attachment_key == attachment_key {
                return Some(id.to_string());
            }
        }
    }
    None
}

/// Find the newest valid notebook bound to `item_key` via `zotero_source.json`.
/// Uses `store_list`, so trashed notebooks and incomplete `.import-*` directories are ignored.
pub fn find_notebook_by_item_key(item_key: &str) -> Option<String> {
    if !valid_cli_attachment_key(item_key) {
        return None;
    }
    let mut notebooks = Vec::new();
    if store_list(&mut notebooks) != 0 {
        return None;
    }
    for nb in &notebooks {
        let id = nb.id.as_str();
        if let Some(src) = zotero_source_load(id) {
            if src.item_key == item_key {
                return Some(id.to_string());
            }
        }
    }
    None
}

/// Find a bound Grimoire notebook by `attachment_key` first, falling back to `item_key`.
pub fn find_bound_notebook(attachment_key: &str, item_key: &str) -> Option<String> {
    if !attachment_key.is_empty() {
        if let Some(id) = find_notebook_by_attachment_key(attachment_key) {
            return Some(id);
        }
    }
    if !item_key.is_empty() {
        if let Some(id) = find_notebook_by_item_key(item_key) {
            return Some(id);
        }
    }
    None
}

fn truncate_utf8_bytes(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Resolve the display page label for a 1-based page number on a Zotero-bound PDF notebook.
/// Strips ASCII control characters (`< 0x20` and `0x7f`), trims whitespace, and caps at 64 UTF-8 bytes.
pub fn zotero_source_page_label(src: &ZoteroNotebookSource, page_one_based: u32) -> String {
    let p = page_one_based.max(1);
    let pg = src
        .pages
        .iter()
        .find(|pg| pg.page_index + 1 == p)
        .or_else(|| src.pages.get((p - 1) as usize));
    if let Some(pg) = pg {
        let cleaned: String = pg
            .page_label
            .chars()
            .filter(|&c| (c as u32) >= 0x20 && (c as u32) != 0x7f)
            .collect();
        let trimmed = cleaned.trim();
        if !trimmed.is_empty() {
            let capped = truncate_utf8_bytes(trimmed, 64).trim_end();
            if !capped.is_empty() {
                return capped.to_string();
            }
        }
    }
    p.to_string()
}

fn collapse_controls_and_trim(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = true;
    for ch in s.chars() {
        if ch == '\0' {
            continue;
        }
        if (ch as u32) < 0x20 || (ch as u32) == 0x7f || ch.is_whitespace() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.push(ch);
            prev_space = false;
        }
    }
    out.trim().to_string()
}

/// Build a bounded short citation label (<= 250 UTF-8 bytes) for a Zotero-bound PDF notebook page.
pub fn zotero_source_short_cite(src: &ZoteroNotebookSource, page_one_based: u32) -> String {
    let base_raw = if !src.citation_key.trim().is_empty() {
        src.citation_key.trim().to_string()
    } else if !src.authors.trim().is_empty() && !src.year.trim().is_empty() {
        format!("{} {}", src.authors.trim(), src.year.trim())
    } else if !src.authors.trim().is_empty() {
        src.authors.trim().to_string()
    } else if !src.title.trim().is_empty() {
        let clean_title = collapse_controls_and_trim(&src.title);
        if clean_title.chars().count() > 44 {
            let prefix: String = clean_title.chars().take(41).collect();
            format!("{}...", prefix.trim_end())
        } else {
            clean_title
        }
    } else if !src.item_key.trim().is_empty() {
        src.item_key.trim().to_string()
    } else {
        src.attachment_key.trim().to_string()
    };

    let base = collapse_controls_and_trim(&base_raw);
    let combined = if page_one_based > 0 {
        let label = zotero_source_page_label(src, page_one_based);
        let suffix = format!(", p. {label}");
        if base.is_empty() {
            format!("p. {label}")
        } else if base.len() + suffix.len() <= 250 {
            format!("{base}{suffix}")
        } else {
            let avail = 250usize.saturating_sub(suffix.len());
            let cut = truncate_utf8_bytes(&base, avail).trim_end();
            if cut.is_empty() {
                format!("p. {label}")
            } else {
                format!("{cut}{suffix}")
            }
        }
    } else {
        base
    };
    truncate_utf8_bytes(&combined, 250).trim_end().to_string()
}

/// Whether `item` points to an already-cached regular (non-symlink) PDF file on local disk.
pub fn zotero_pdf_on_disk(item: &ZoteroLibraryItem) -> bool {
    item.has_pdf
        && item.pdf_cached
        && !item.pdf_local_path.is_empty()
        && std::fs::symlink_metadata(&item.pdf_local_path)
            .is_ok_and(|md| !md.file_type().is_symlink() && md.is_file())
}

/// Build a clean, storable Grimoire notebook title for an imported Zotero PDF.
pub fn zotero_clean_notebook_title(item: &ZoteroLibraryItem, info: &ZkPdfInfo) -> String {
    let fname_stem = std::path::Path::new(&info.filename)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    let raw = if !item.title.trim().is_empty() {
        item.title.as_str()
    } else if !item.citation_key.trim().is_empty() {
        item.citation_key.as_str()
    } else if !fname_stem.trim().is_empty() {
        fname_stem
    } else {
        "Zotero PDF"
    };
    let mut cleaned = String::with_capacity(raw.len().min(STORE_NAME_LEN));
    let mut prev_space = true;
    for ch in raw.chars() {
        if ch == '\0' || ch.is_control() {
            if !prev_space {
                cleaned.push(' ');
                prev_space = true;
            }
            continue;
        }
        if ch.is_whitespace() {
            if !prev_space {
                cleaned.push(' ');
                prev_space = true;
            }
        } else {
            cleaned.push(ch);
            prev_space = false;
        }
    }
    let trimmed = cleaned.trim();
    let max_bytes = STORE_NAME_LEN.saturating_sub(1).min(120);
    let mut end = trimmed.len().min(max_bytes);
    while end > 0 && !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    let candidate = trimmed[..end].trim();
    if candidate.is_empty() || !name_storable(candidate) {
        "Zotero PDF".to_string()
    } else {
        candidate.to_string()
    }
}

fn sanitize_staged_pdf_filename(raw: &str) -> String {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or(raw);
    let stem = if base.len() > 4 && base[base.len() - 4..].eq_ignore_ascii_case(".pdf") {
        &base[..base.len() - 4]
    } else {
        base
    };
    let mut out = String::new();
    for ch in stem.chars() {
        if out.len() >= 56 {
            break;
        }
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            out.push(ch);
        } else if ch == ' ' && !out.ends_with('_') && !out.is_empty() {
            out.push('_');
        }
    }
    let trimmed = out.trim_matches(|c| c == '.' || c == '_' || c == '-' || c == ' ');
    let safe_stem = if trimmed.is_empty()
        || (trimmed.len() >= 4 && trimmed[trimmed.len() - 4..].eq_ignore_ascii_case(".sdr"))
    {
        "document"
    } else {
        trimmed
    };
    format!("{safe_stem}.pdf")
}

fn path_has_symlink_under(base_dir: &str, full_path: &str) -> bool {
    let prefix = format!("{}/", base_dir.trim_end_matches('/'));
    let Some(rel) = full_path.strip_prefix(&prefix) else {
        return false;
    };
    let mut cur = base_dir.trim_end_matches('/').to_string();
    if std::fs::symlink_metadata(&cur).is_ok_and(|m| m.file_type().is_symlink()) {
        return true;
    }
    for comp in rel.split('/') {
        if comp.is_empty() {
            continue;
        }
        cur.push('/');
        cur.push_str(comp);
        if std::fs::symlink_metadata(&cur).is_ok_and(|m| m.file_type().is_symlink()) {
            return true;
        }
    }
    false
}

/// Map `info.local_path` to a `StoragePath` accepted by `pdf_import_queue`:
/// - Zero-copy when `info.local_path` already resides inside `storage_root()` and passes `FsStorage`'s
///   non-symlink, zone-safe check.
/// - Stages a verified regular non-symlink copy into `<store_root>/zotero_cache/<attachment_key>/<safe>.pdf`
///   when `info.local_path` resides outside `storage_root()` (e.g. `/var/local/zotero-bridge/cache/...`)
///   or inside a dot-prefixed directory that `FsStorage` does not open directly.
pub fn zotero_pdf_storage_path(info: &ZkPdfInfo) -> Result<StoragePath, String> {
    use crate::fsview::{FsErr, FsStorage};
    use std::io::{Read, Write};
    use std::os::fd::FromRawFd;

    if !info.cached || info.local_path.is_empty() {
        return Err("PDF is not cached locally.".to_string());
    }
    if !valid_cli_attachment_key(&info.attachment_key) {
        return Err("Invalid Zotero attachment key.".to_string());
    }
    match std::fs::symlink_metadata(&info.local_path) {
        Ok(md) if !md.file_type().is_symlink() && md.is_file() => {}
        _ => return Err("Cached PDF is missing or not a regular file.".to_string()),
    }
    if let Some(parent) = std::path::Path::new(&info.local_path).parent() {
        if std::fs::symlink_metadata(parent).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err("Cached PDF directory is a symlink.".to_string());
        }
    }

    let storage_root = crate::pdfimport::storage_root()
        .ok_or_else(|| "Storage root is unavailable.".to_string())?;
    let root_trimmed = storage_root.trim_end_matches('/');
    let root_prefix = format!("{root_trimmed}/");

    if let Some(rel) = info.local_path.strip_prefix(&root_prefix) {
        if path_has_symlink_under(root_trimmed, &info.local_path) {
            return Err("Cached PDF path traverses a symlink.".to_string());
        }
        if let Some(sp) = StoragePath::new(rel.as_bytes()) {
            let fs = FsStorage::open(&storage_root, &store_root())
                .map_err(|_| "Storage root could not be opened.".to_string())?;
            match fs.open_file(&sp, true) {
                Ok(_fd) => return Ok(sp),
                Err(FsErr::Link) => return Err("Cached PDF path traverses a symlink.".to_string()),
                Err(_) => {}
            }
        }
    }

    // Stage external regular file into zotero_cache/<attachment_key>/<safe_filename>.pdf
    let c_src = std::ffi::CString::new(info.local_path.as_str())
        .map_err(|_| "Invalid PDF path.".to_string())?;
    // SAFETY: c_src is NUL-terminated; O_NOFOLLOW rejects symlinks atomically.
    let src_raw = unsafe {
        libc::open(
            c_src.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if src_raw < 0 {
        return Err("Cached PDF could not be opened.".to_string());
    }
    // SAFETY: src_raw was just opened and is owned exclusively here.
    let mut src_file = unsafe { std::fs::File::from_raw_fd(src_raw) };
    let mut src_st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: src_st is written by fstat on valid fd.
    if unsafe { libc::fstat(src_file.as_raw_fd(), &mut src_st) } != 0
        || (src_st.st_mode & libc::S_IFMT) != libc::S_IFREG
    {
        return Err("Cached PDF is not a regular file.".to_string());
    }
    let src_size = src_st.st_size.max(0) as u64;
    if src_size == 0 || src_size > 512 * 1024 * 1024 {
        return Err("Cached PDF size is invalid or exceeds 512 MB.".to_string());
    }

    let (cache_root, rel_prefix) = if let Some(grel) = crate::pdfimport::grimoire_rel() {
        let base = format!("{}/zotero_cache", store_root().trim_end_matches('/'));
        (base, format!("{grel}/zotero_cache"))
    } else {
        let base = format!("{root_trimmed}/zotero_cache");
        (base, "zotero_cache".to_string())
    };
    if !crate::zotero_bridge::ensure_dir(&cache_root) {
        return Err("Could not prepare Zotero PDF cache directory.".to_string());
    }
    let att_dir = format!("{cache_root}/{}", info.attachment_key);
    if !crate::zotero_bridge::ensure_dir(&att_dir) {
        return Err("Could not prepare Zotero attachment cache directory.".to_string());
    }

    let safe_name = sanitize_staged_pdf_filename(if !info.filename.is_empty() {
        &info.filename
    } else {
        &info.local_path
    });
    let dest_path = format!("{att_dir}/{safe_name}");
    if std::fs::symlink_metadata(&dest_path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err("Staged PDF destination is a symlink.".to_string());
    }

    let reuse = std::fs::symlink_metadata(&dest_path)
        .ok()
        .filter(|m| !m.file_type().is_symlink() && m.is_file() && m.len() == src_size)
        .and_then(|_| {
            let src_key = crate::md5::partial_md5_fd(src_file.as_raw_fd())?;
            let dst_f = std::fs::File::open(&dest_path).ok()?;
            let dst_key = crate::md5::partial_md5_fd(dst_f.as_raw_fd())?;
            Some(src_key == dst_key)
        })
        .unwrap_or(false);

    if !reuse {
        let tmp_path = format!(
            "{dest_path}.tmp.{}.{}",
            std::process::id(),
            BG_NEXT_ID.fetch_add(1, Ordering::Relaxed)
        );
        let c_tmp = std::ffi::CString::new(tmp_path.as_str())
            .map_err(|_| "Invalid temporary cache path.".to_string())?;
        // SAFETY: c_tmp is NUL-terminated; O_CREAT | O_EXCL | O_NOFOLLOW creates a fresh non-symlink file.
        let tmp_raw = unsafe {
            libc::open(
                c_tmp.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if tmp_raw < 0 {
            return Err("Could not create staged PDF file.".to_string());
        }
        // SAFETY: tmp_raw was just opened and is owned exclusively by tmp_file.
        let mut tmp_file = unsafe { std::fs::File::from_raw_fd(tmp_raw) };
        let copy_res = (|| -> std::io::Result<()> {
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let n = src_file.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                tmp_file.write_all(&buf[..n])?;
            }
            tmp_file.sync_all()?;
            Ok(())
        })();
        drop(tmp_file);
        if copy_res.is_err() || std::fs::rename(&tmp_path, &dest_path).is_err() {
            let _ = std::fs::remove_file(&tmp_path);
            return Err("Could not stage Zotero PDF into storage.".to_string());
        }
    }

    let sp_rel = format!("{rel_prefix}/{}/{safe_name}", info.attachment_key);
    let sp = StoragePath::new(sp_rel.as_bytes())
        .ok_or_else(|| "Staged PDF storage path is invalid.".to_string())?;
    let fs = FsStorage::open(&storage_root, &store_root())
        .map_err(|_| "Storage root could not be opened.".to_string())?;
    fs.open_file(&sp, true)
        .map_err(|_| "Staged PDF could not be verified in storage.".to_string())?;
    Ok(sp)
}

#[cfg(test)]
mod ink_tests {
    use super::*;

    #[test]
    fn upsert_unlinks_temp_file_on_failure_and_success() {
        let dir = std::env::temp_dir().join(format!("grimoire-zk-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let rec = dir.join("seen.txt");
        let bin = dir.join("zk-sync");
        // Records the payload path then exits 3 (failure path).
        std::fs::write(&bin, format!("#!/bin/sh\necho \"$3\" > '{}'\nexit 3\n", rec.display())).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        zk_sync_set_test_config(Some(ZkSyncConfig {
            bin: bin.to_string_lossy().into_owned(),
            zk_home: dir.to_string_lossy().into_owned(),
            zk_config_dir: dir.to_string_lossy().into_owned(),
            local_timeout_ms: 5000,
            network_timeout_ms: 5000,
        }));
        let r = zk_sync_ink_upsert("ABCD1234", &ZkUpsertPayload::default());
        zk_sync_set_test_config(None);
        assert!(matches!(r, Err(ZkSyncError::CommandFailed { code: 3, .. })));
        let seen = std::fs::read_to_string(&rec).unwrap();
        let p = seen.trim();
        assert!(p.contains("grimoire-zk-ink-"));
        assert!(!std::path::Path::new(p).exists());
        assert!(matches!(zk_sync_ink_pull("bad/key", true), Err(ZkSyncError::InvalidArgument(_))));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

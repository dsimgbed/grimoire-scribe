//! Two-way Zotero ink sync (Phase 4): the coordinate maps between a Scribe page and a PDF page's user
//! space, the Grimoire-owned ink "buckets" that become Zotero `ink` annotations, the `zk-sync ink-pull`
//! / `ink-upsert` JSON (docs/unified-ink-sync-contract.md 3.3, 3.4), the `zotero_ink_sync.json` sidecar,
//! the pure reconciliation (plan + apply), and the non-destructive overlays (remote ink, highlights,
//! underlines) that are painted as part of an image paper, never stored in `Page.strokes`.

use crate::json_min::{JsonTok, JsonType};
use crate::zotero_sync::{ZkPdfPageGeometry, ZoteroNotebookSource};
use crate::prelude::{
    is_floor_marker, paper_is_document, paper_is_image, rect_intersect, rect_is_empty, stroke_is_erase, Canvas, Page,
    PaperKind, Rect, Stroke, ToolParams, TOOL_HIGHLIGHTER,
};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

// ---------------------------------------------------------------------------------------------
// Coordinates
// ---------------------------------------------------------------------------------------------

pub const RDP_EPS_PT: f64 = 0.4;
pub const ZOTERO_HIGHLIGHTER_COLOR: &str = "#ffd400";
const BLACK: &str = "#000000";

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PageFit {
    pub scale: f64,
    pub ox: f64,
    pub oy: f64,
}

/// How the PDF page lands on a `page_w` x `page_h` Scribe page (the same fit the page renderer uses).
pub fn page_fit_for(geom: &ZkPdfPageGeometry, page_w: i32, page_h: i32) -> PageFit {
    let f = crate::pdfwork::fit_page(geom.width_pt, geom.height_pt, page_w, page_h);
    PageFit { scale: f.scale, ox: f.ox as f64, oy: f.oy as f64 }
}

/// Page px -> PDF user space.
pub fn grimoire_to_user(geom: &ZkPdfPageGeometry, fit: &PageFit, px: f64, py: f64) -> Option<(f64, f64)> {
    if !(fit.scale.is_finite() && fit.scale > 0.0) {
        return None;
    }
    let qx = (px - fit.ox) / fit.scale;
    let qy = (py - fit.oy) / fit.scale;
    geom.display_to_user(qx, qy)
}

/// PDF user space -> page px.
pub fn user_to_grimoire(geom: &ZkPdfPageGeometry, fit: &PageFit, ux: f64, uy: f64) -> (f64, f64) {
    let (qx, qy) = geom.user_to_display(ux, uy);
    (fit.ox + qx * fit.scale, fit.oy + qy * fit.scale)
}

/// Round to 2 decimals; None for NaN/Inf; never -0.0.
pub fn round2(v: f64) -> Option<f64> {
    if !v.is_finite() {
        return None;
    }
    let r = (v * 100.0).round() / 100.0;
    if !r.is_finite() {
        return None;
    }
    Some(if r == 0.0 { 0.0 } else { r })
}

fn point_seg_dist(p: (f64, f64), a: (f64, f64), b: (f64, f64)) -> f64 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let l2 = dx * dx + dy * dy;
    if l2 <= 1e-18 {
        return ((p.0 - a.0).powi(2) + (p.1 - a.1).powi(2)).sqrt();
    }
    let t = (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / l2).clamp(0.0, 1.0);
    let (cx, cy) = (a.0 + t * dx, a.1 + t * dy);
    ((p.0 - cx).powi(2) + (p.1 - cy).powi(2)).sqrt()
}

/// Ramer-Douglas-Peucker with an explicit stack; endpoints always kept.
pub fn rdp_simplify(pts: &[(f64, f64)], eps: f64) -> Vec<(f64, f64)> {
    let n = pts.len();
    if n <= 2 {
        return pts.to_vec();
    }
    let mut keep = vec![false; n];
    keep[0] = true;
    keep[n - 1] = true;
    let mut stack = vec![(0usize, n - 1)];
    while let Some((lo, hi)) = stack.pop() {
        if hi <= lo + 1 {
            continue;
        }
        let (mut best, mut at) = (-1.0f64, lo);
        for i in lo + 1..hi {
            let d = point_seg_dist(pts[i], pts[lo], pts[hi]);
            if d > best {
                best = d;
                at = i;
            }
        }
        if best > eps {
            keep[at] = true;
            stack.push((lo, at));
            stack.push((at, hi));
        }
    }
    pts.iter().zip(keep).filter(|(_, k)| *k).map(|(p, _)| *p).collect()
}

/// Mean pen width in page px.
pub fn stroke_width_px(params: &ToolParams, dpi: f32) -> f64 {
    ((params.size_min_mm as f64 + params.size_max_mm as f64) / 2.0) * dpi as f64 / 25.4
}

pub fn width_px_to_pt(px: f64, fit: &PageFit) -> f64 {
    let w = if fit.scale > 0.0 && px.is_finite() { px / fit.scale } else { 0.5 };
    round2(w.clamp(0.5, 200.0)).unwrap_or(0.5)
}

pub fn stroke_exportable(s: &Stroke) -> bool {
    !stroke_is_erase(s) && !is_floor_marker(s) && !s.pts.is_empty()
}

/// The highlighter's Zotero yellow, else black. (ext[0] is documented as "colour" but this version
/// writes 0 and gives it no meaning, docs/FORMAT.md, so it is not read.)
pub fn stroke_color_hex(s: &Stroke) -> String {
    if s.tool == TOOL_HIGHLIGHTER { ZOTERO_HIGHLIGHTER_COLOR } else { BLACK }.to_string()
}

/// Bounding box (x0,y0,x1,y1) of the four transformed corners of a user-space rect.
pub fn rect_user_to_grimoire(geom: &ZkPdfPageGeometry, fit: &PageFit, r: [f64; 4]) -> [f64; 4] {
    let corners = [(r[0], r[1]), (r[2], r[1]), (r[2], r[3]), (r[0], r[3])];
    let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for (ux, uy) in corners {
        let (x, y) = user_to_grimoire(geom, fit, ux, uy);
        x0 = x0.min(x);
        y0 = y0.min(y);
        x1 = x1.max(x);
        y1 = y1.max(y);
    }
    [x0, y0, x1, y1]
}

/// One stroke -> flat user-space path `[x0,y0,x1,y1,...]` (round2, simplified), or None.
fn stroke_user_path(s: &Stroke, geom: &ZkPdfPageGeometry, fit: &PageFit) -> Option<Vec<f64>> {
    let mut pts = Vec::with_capacity(s.pts.len());
    for p in &s.pts {
        let Some((ux, uy)) = grimoire_to_user(geom, fit, p.x as f64, p.y as f64) else { continue };
        let (Some(x), Some(y)) = (round2(ux), round2(uy)) else { continue };
        pts.push((x, y));
    }
    if pts.is_empty() {
        return None;
    }
    let mut pts = rdp_simplify(&pts, RDP_EPS_PT);
    if pts.len() == 1 {
        pts.push(pts[0]);
    }
    let mut flat = Vec::with_capacity(pts.len() * 2);
    for (x, y) in pts {
        flat.push(x);
        flat.push(y);
    }
    Some(flat)
}

// ---------------------------------------------------------------------------------------------
// Local buckets
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct LocalBucket {
    pub page_id: u32,
    pub page_index: u32,
    pub bucket: String,
    pub width: f64,
    pub color: String,
    /// None = page unchanged since the last sync (stamp cache), paths not loaded
    pub paths: Option<Vec<Vec<f64>>>,
    pub hash: String,
}

fn fnv1a(h: &mut u64, bytes: &[u8]) {
    for &b in bytes {
        *h ^= b as u64;
        *h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
}

fn fmt2(v: f64) -> String {
    match round2(v) {
        Some(r) => format!("{r:.2}"),
        None => "nan".to_string(),
    }
}

/// FNV-1a 64 over a canonical text form of the round2 values: 16 lowercase hex digits.
pub fn canonical_hash(page_index: u32, width: f64, color: &str, paths: &[Vec<f64>]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    fnv1a(&mut h, format!("{page_index}|{}|{color}", fmt2(width)).as_bytes());
    for p in paths {
        fnv1a(&mut h, b"|");
        for (i, v) in p.iter().enumerate() {
            if i > 0 {
                fnv1a(&mut h, b",");
            }
            fnv1a(&mut h, fmt2(*v).as_bytes());
        }
    }
    format!("{h:016x}")
}

pub fn page_local_buckets(page: &Page, page_id: u32, page_index: u32, geom: &ZkPdfPageGeometry, dpi: f32) -> Vec<LocalBucket> {
    let fit = page_fit_for(geom, page.w, page.h);
    let mut groups: BTreeMap<String, (f64, String, Vec<Vec<f64>>)> = BTreeMap::new();
    for s in &page.strokes {
        if !stroke_exportable(s) {
            continue;
        }
        let Some(path) = stroke_user_path(s, geom, &fit) else { continue };
        let wpt = width_px_to_pt(stroke_width_px(&s.params, dpi), &fit);
        let q = round2((wpt / 0.25).round() * 0.25).unwrap_or(0.5).max(0.25);
        let color = stroke_color_hex(s);
        let bucket = format!("{color}|{q:.2}");
        groups.entry(bucket).or_insert_with(|| (q, color, Vec::new())).2.push(path);
    }
    groups
        .into_iter()
        .map(|(bucket, (width, color, paths))| {
            let hash = canonical_hash(page_index, width, &color, &paths);
            LocalBucket { page_id, page_index, bucket, width, color, paths: Some(paths), hash }
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Pulled data / upsert payloads
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ZkInkAnnotation {
    pub key: String,
    pub version: u64,
    pub page_index: u32,
    pub paths: Vec<Vec<f64>>,
    pub width: f64,
    pub color: String,
    pub comment: String,
    pub tags: Vec<String>,
    pub sort_index: String,
    pub page_label: String,
    pub dirty: bool,
    pub conflict: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ZkHighlight {
    pub key: String,
    pub version: u64,
    /// "highlight" | "underline"
    pub kind: String,
    pub page_index: u32,
    pub color: String,
    pub rects: Vec<[f64; 4]>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ZkInkPull {
    pub attachment_key: String,
    pub synced: bool,
    pub ink: Vec<ZkInkAnnotation>,
    pub highlights: Vec<ZkHighlight>,
    pub other_counts: Vec<(String, u64)>,
}

// ---- JSON helpers ----

fn tok_f64(text: &[u8], tok: &JsonTok) -> Option<f64> {
    if tok.type_ != JsonType::Number {
        return None;
    }
    let s = std::str::from_utf8(&text[tok.start as usize..tok.end as usize]).ok()?;
    let v = s.parse::<f64>().ok()?;
    v.is_finite().then_some(if v == 0.0 { 0.0 } else { v })
}

fn tok_u64(text: &[u8], tok: &JsonTok) -> Option<u64> {
    if tok.type_ != JsonType::Number {
        return None;
    }
    let s = std::str::from_utf8(&text[tok.start as usize..tok.end as usize]).ok()?;
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse::<u64>().ok()
}

fn val(toks: &[JsonTok], obj: i32, key: &str, text: &[u8]) -> Option<i32> {
    let t = crate::json_min::json_get(text, toks, obj, key);
    (t >= 0).then_some(t)
}

fn o_f64(text: &[u8], toks: &[JsonTok], obj: i32, key: &str) -> Option<f64> {
    tok_f64(text, &toks[val(toks, obj, key, text)? as usize])
}

fn o_u64(text: &[u8], toks: &[JsonTok], obj: i32, key: &str) -> Option<u64> {
    tok_u64(text, &toks[val(toks, obj, key, text)? as usize])
}

fn o_u32(text: &[u8], toks: &[JsonTok], obj: i32, key: &str) -> Option<u32> {
    u32::try_from(o_u64(text, toks, obj, key)?).ok()
}

/// Missing or null -> Some(default); present but not a string -> None.
fn o_str_opt(text: &[u8], toks: &[JsonTok], obj: i32, key: &str, max: usize) -> Option<String> {
    match val(toks, obj, key, text) {
        None => Some(String::new()),
        Some(i) if toks[i as usize].type_ == JsonType::Null => Some(String::new()),
        Some(_) => crate::zotero_bridge::jstr_strict(text, toks, obj, key, max),
    }
}

fn o_bool_opt(text: &[u8], toks: &[JsonTok], obj: i32, key: &str) -> bool {
    crate::zotero_bridge::jbool(text, toks, obj, key).unwrap_or(false)
}

fn valid_zkey(k: &str) -> bool {
    k.len() == 8 && k.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// The value tokens of an array, in order.
fn children(toks: &[JsonTok], arr: i32) -> Vec<i32> {
    let mut out = Vec::new();
    let Some(a) = toks.get(arr as usize) else { return out };
    if a.type_ != JsonType::Array {
        return out;
    }
    let mut at = arr + 1;
    for _ in 0..a.size {
        if at < 0 || at as usize >= toks.len() {
            break;
        }
        out.push(at);
        at = toks[at as usize].next;
    }
    out
}

fn num_array(text: &[u8], toks: &[JsonTok], arr: i32) -> Option<Vec<f64>> {
    if toks.get(arr as usize)?.type_ != JsonType::Array {
        return None;
    }
    let kids = children(toks, arr);
    if kids.len() as i32 != toks[arr as usize].size {
        return None;
    }
    kids.into_iter().map(|k| tok_f64(text, &toks[k as usize])).collect()
}

fn parse_paths(text: &[u8], toks: &[JsonTok], obj: i32) -> Option<Vec<Vec<f64>>> {
    let a = val(toks, obj, "paths", text)?;
    if toks[a as usize].type_ != JsonType::Array {
        return None;
    }
    let mut paths = Vec::new();
    for k in children(toks, a) {
        let p = num_array(text, toks, k)?;
        if p.len() < 2 || p.len() % 2 != 0 {
            return None;
        }
        paths.push(p);
    }
    Some(paths)
}

fn parse_ink_entry(text: &[u8], toks: &[JsonTok], o: i32) -> Option<ZkInkAnnotation> {
    if toks.get(o as usize)?.type_ != JsonType::Object {
        return None;
    }
    let key = crate::zotero_bridge::jstr_strict(text, toks, o, "key", 16)?;
    if !valid_zkey(&key) {
        return None;
    }
    let paths = parse_paths(text, toks, o)?;
    if paths.is_empty() {
        return None;
    }
    let width = o_f64(text, toks, o, "width")?;
    let color = {
        let c = o_str_opt(text, toks, o, "color", 64)?;
        if c.is_empty() { BLACK.to_string() } else { c }
    };
    Some(ZkInkAnnotation {
        key,
        version: o_u64(text, toks, o, "version").unwrap_or(0),
        page_index: o_u32(text, toks, o, "pageIndex")?,
        paths,
        width,
        color,
        comment: o_str_opt(text, toks, o, "comment", 65536)?,
        tags: crate::zotero_bridge::jstrings(text, toks, o, "tags", 256, 256),
        sort_index: o_str_opt(text, toks, o, "sortIndex", 128)?,
        page_label: o_str_opt(text, toks, o, "pageLabel", 128)?,
        dirty: o_bool_opt(text, toks, o, "dirty"),
        conflict: o_bool_opt(text, toks, o, "conflict"),
    })
}

fn parse_highlight_entry(text: &[u8], toks: &[JsonTok], o: i32) -> Option<ZkHighlight> {
    if toks.get(o as usize)?.type_ != JsonType::Object {
        return None;
    }
    let key = crate::zotero_bridge::jstr_strict(text, toks, o, "key", 16)?;
    if !valid_zkey(&key) {
        return None;
    }
    let kind = crate::zotero_bridge::jstr_strict(text, toks, o, "type", 32)?;
    if kind != "highlight" && kind != "underline" {
        return None;
    }
    let ra = val(toks, o, "rects", text)?;
    if toks[ra as usize].type_ != JsonType::Array {
        return None;
    }
    let mut rects = Vec::new();
    for k in children(toks, ra) {
        let r = num_array(text, toks, k)?;
        if r.len() != 4 {
            return None;
        }
        rects.push([r[0], r[1], r[2], r[3]]);
    }
    if rects.is_empty() {
        return None;
    }
    let color = {
        let c = o_str_opt(text, toks, o, "color", 64)?;
        if c.is_empty() { ZOTERO_HIGHLIGHTER_COLOR.to_string() } else { c }
    };
    Some(ZkHighlight { key, version: o_u64(text, toks, o, "version").unwrap_or(0), kind, page_index: o_u32(text, toks, o, "pageIndex")?, color, rects })
}

/// Individually malformed entries are skipped, never fatal.
fn parse_ink_array(text: &[u8], toks: &[JsonTok], obj: i32, key: &str) -> Vec<ZkInkAnnotation> {
    let Some(a) = val(toks, obj, key, text) else { return Vec::new() };
    children(toks, a).into_iter().filter_map(|k| parse_ink_entry(text, toks, k)).collect()
}

fn parse_highlight_array(text: &[u8], toks: &[JsonTok], obj: i32, key: &str) -> Vec<ZkHighlight> {
    let Some(a) = val(toks, obj, key, text) else { return Vec::new() };
    children(toks, a).into_iter().filter_map(|k| parse_highlight_entry(text, toks, k)).collect()
}

pub fn parse_ink_pull(bytes: &[u8]) -> Result<ZkInkPull, String> {
    let (toks, _) = crate::zotero_bridge::parse_json(bytes).ok_or_else(|| "ink-pull: invalid JSON".to_string())?;
    if toks[0].type_ != JsonType::Object || !crate::zotero_bridge::schema_ok(bytes, &toks, 0) {
        return Err("ink-pull: unsupported schema_version".to_string());
    }
    let attachment_key = crate::zotero_bridge::jstr_strict(bytes, &toks, 0, "attachment_key", 32)
        .ok_or_else(|| "ink-pull: missing attachment_key".to_string())?;
    let synced = o_bool_opt(bytes, &toks, 0, "synced");
    let mut other_counts = Vec::new();
    if let Some(oc) = val(&toks, 0, "other_counts", bytes) {
        if toks[oc as usize].type_ == JsonType::Object {
            let mut at = oc + 1;
            for _ in 0..toks[oc as usize].size {
                if at < 0 || at as usize + 1 >= toks.len() {
                    break;
                }
                let (k, v) = (&toks[at as usize], &toks[at as usize + 1]);
                if let (Ok(name), Some(n)) = (std::str::from_utf8(&bytes[k.start as usize..k.end as usize]), tok_u64(bytes, v)) {
                    other_counts.push((name.to_string(), n));
                }
                at = v.next;
            }
        }
    }
    Ok(ZkInkPull {
        attachment_key,
        synced,
        ink: parse_ink_array(bytes, &toks, 0, "ink"),
        highlights: parse_highlight_array(bytes, &toks, 0, "highlights"),
        other_counts,
    })
}

fn json_str(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn json_num(v: f64) -> Result<String, String> {
    let r = round2(v).ok_or_else(|| "non-finite number".to_string())?;
    Ok(format!("{r:?}"))
}

fn json_paths(paths: &[Vec<f64>]) -> Result<String, String> {
    let mut o = String::from("[");
    for (i, p) in paths.iter().enumerate() {
        if i > 0 {
            o.push(',');
        }
        o.push('[');
        for (j, v) in p.iter().enumerate() {
            if j > 0 {
                o.push(',');
            }
            o.push_str(&json_num(*v)?);
        }
        o.push(']');
    }
    o.push(']');
    Ok(o)
}

fn json_tags(tags: &[String]) -> String {
    format!("[{}]", tags.iter().map(|t| json_str(t)).collect::<Vec<_>>().join(","))
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ZkUpsertAnnotation {
    pub key: String,
    pub page_index: u32,
    pub width: f64,
    pub color: String,
    pub comment: String,
    pub tags: Vec<String>,
    pub page_label: String,
    pub sort_index: String,
    pub paths: Vec<Vec<f64>>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ZkUpsertPayload {
    pub annotations: Vec<ZkUpsertAnnotation>,
    pub delete_keys: Vec<String>,
}

impl ZkUpsertPayload {
    pub fn is_empty(&self) -> bool {
        self.annotations.is_empty() && self.delete_keys.is_empty()
    }
}

pub const ZK_UPSERT_MAX: usize = 10 * 1024 * 1024;

pub fn serialize_upsert_payload(p: &ZkUpsertPayload) -> Result<String, String> {
    let mut anns = Vec::with_capacity(p.annotations.len());
    for a in &p.annotations {
        if !a.key.is_empty() && !valid_zkey(&a.key) {
            return Err("invalid annotation key".to_string());
        }
        let mut o = format!(
            "{{\"key\":{},\"pageIndex\":{},\"width\":{},\"color\":{},\"comment\":{},\"tags\":{}",
            json_str(&a.key),
            a.page_index,
            json_num(a.width)?,
            json_str(&a.color),
            json_str(&a.comment),
            json_tags(&a.tags)
        );
        if !a.page_label.is_empty() {
            o.push_str(&format!(",\"pageLabel\":{}", json_str(&a.page_label)));
        }
        if !a.sort_index.is_empty() {
            o.push_str(&format!(",\"sortIndex\":{}", json_str(&a.sort_index)));
        }
        o.push_str(&format!(",\"paths\":{}}}", json_paths(&a.paths)?));
        anns.push(o);
    }
    for k in &p.delete_keys {
        if !valid_zkey(k) {
            return Err("invalid delete key".to_string());
        }
    }
    let doc = format!(
        "{{\"schema_version\":1,\"annotations\":[{}],\"delete_keys\":{}}}",
        anns.join(","),
        json_tags(&p.delete_keys)
    );
    if doc.len() > ZK_UPSERT_MAX {
        return Err("ink-upsert payload exceeds 10 MiB".to_string());
    }
    Ok(doc)
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ZkUpsertResultRow {
    /// index into the annotations array; u32::MAX when the row has none (a delete_keys row)
    pub index: u32,
    pub key: String,
    pub action: String,
    pub version: u64,
    pub dirty: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ZkUpsertResult {
    pub attachment_key: String,
    pub created: u32,
    pub updated: u32,
    pub deleted: u32,
    pub unchanged: u32,
    pub results: Vec<ZkUpsertResultRow>,
}

pub fn parse_upsert_result(bytes: &[u8]) -> Result<ZkUpsertResult, String> {
    let (toks, _) = crate::zotero_bridge::parse_json(bytes).ok_or_else(|| "ink-upsert: invalid JSON".to_string())?;
    if toks[0].type_ != JsonType::Object || !crate::zotero_bridge::schema_ok(bytes, &toks, 0) {
        return Err("ink-upsert: unsupported schema_version".to_string());
    }
    let attachment_key = crate::zotero_bridge::jstr_strict(bytes, &toks, 0, "attachment_key", 32)
        .ok_or_else(|| "ink-upsert: missing attachment_key".to_string())?;
    let mut results = Vec::new();
    if let Some(a) = val(&toks, 0, "results", bytes) {
        for r in children(&toks, a) {
            if toks[r as usize].type_ != JsonType::Object {
                continue;
            }
            let Some(action) = crate::zotero_bridge::jstr_strict(bytes, &toks, r, "action", 32) else { continue };
            results.push(ZkUpsertResultRow {
                index: o_u32(bytes, &toks, r, "index").unwrap_or(u32::MAX),
                key: o_str_opt(bytes, &toks, r, "key", 16).unwrap_or_default(),
                action,
                version: o_u64(bytes, &toks, r, "version").unwrap_or(0),
                dirty: o_bool_opt(bytes, &toks, r, "dirty"),
            });
        }
    }
    Ok(ZkUpsertResult {
        attachment_key,
        created: o_u32(bytes, &toks, 0, "created").unwrap_or(0),
        updated: o_u32(bytes, &toks, 0, "updated").unwrap_or(0),
        deleted: o_u32(bytes, &toks, 0, "deleted").unwrap_or(0),
        unchanged: o_u32(bytes, &toks, 0, "unchanged").unwrap_or(0),
        results,
    })
}

// ---------------------------------------------------------------------------------------------
// Sidecar zotero_ink_sync.json
// ---------------------------------------------------------------------------------------------

pub const ZOTERO_INK_SYNC_FILE: &str = "zotero_ink_sync.json";
pub const ZOTERO_INK_SYNC_MAX: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct InkBinding {
    pub key: String,
    pub page_id: u32,
    pub page_index: u32,
    pub bucket: String,
    pub hash: String,
    pub version: u64,
}

/// The remote deleted a Grimoire-owned key: do not resurrect it while the local hash is unchanged.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct InkSuppressed {
    pub page_id: u32,
    pub bucket: String,
    pub hash: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PageStamp {
    pub page_id: u32,
    pub stamp: String,
    /// (bucket, hash, page_index, width, color)
    pub buckets: Vec<(String, String, u32, f64, String)>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct InkSyncState {
    pub schema_version: u32,
    pub attachment_key: String,
    pub last_sync_ms: u64,
    pub local: Vec<InkBinding>,
    pub suppressed: Vec<InkSuppressed>,
    pub page_stamps: Vec<PageStamp>,
    pub remote_ink: Vec<ZkInkAnnotation>,
    pub remote_highlights: Vec<ZkHighlight>,
}

fn ink_ann_json(a: &ZkInkAnnotation) -> Result<String, String> {
    Ok(format!(
        "{{\"key\":{},\"version\":{},\"pageIndex\":{},\"paths\":{},\"width\":{},\"color\":{},\"comment\":{},\"tags\":{},\"sortIndex\":{},\"pageLabel\":{},\"dirty\":{},\"conflict\":{}}}",
        json_str(&a.key),
        a.version,
        a.page_index,
        json_paths(&a.paths)?,
        json_num(a.width)?,
        json_str(&a.color),
        json_str(&a.comment),
        json_tags(&a.tags),
        json_str(&a.sort_index),
        json_str(&a.page_label),
        a.dirty,
        a.conflict
    ))
}

fn highlight_json(h: &ZkHighlight) -> Result<String, String> {
    let mut rects = Vec::new();
    for r in &h.rects {
        let mut parts = Vec::new();
        for v in r {
            parts.push(json_num(*v)?);
        }
        rects.push(format!("[{}]", parts.join(",")));
    }
    Ok(format!(
        "{{\"key\":{},\"version\":{},\"type\":{},\"pageIndex\":{},\"color\":{},\"rects\":[{}]}}",
        json_str(&h.key),
        h.version,
        json_str(&h.kind),
        h.page_index,
        json_str(&h.color),
        rects.join(",")
    ))
}

pub fn serialize_ink_sync_state(s: &InkSyncState) -> Result<String, String> {
    let local: Vec<String> = s
        .local
        .iter()
        .map(|b| {
            format!(
                "{{\"key\":{},\"page_id\":{},\"page_index\":{},\"bucket\":{},\"hash\":{},\"version\":{}}}",
                json_str(&b.key),
                b.page_id,
                b.page_index,
                json_str(&b.bucket),
                json_str(&b.hash),
                b.version
            )
        })
        .collect();
    let supp: Vec<String> = s
        .suppressed
        .iter()
        .map(|x| format!("{{\"page_id\":{},\"bucket\":{},\"hash\":{}}}", x.page_id, json_str(&x.bucket), json_str(&x.hash)))
        .collect();
    let mut stamps = Vec::new();
    for p in &s.page_stamps {
        let mut bs = Vec::new();
        for (bucket, hash, pi, w, c) in &p.buckets {
            bs.push(format!(
                "{{\"bucket\":{},\"hash\":{},\"page_index\":{},\"width\":{},\"color\":{}}}",
                json_str(bucket),
                json_str(hash),
                pi,
                json_num(*w)?,
                json_str(c)
            ));
        }
        stamps.push(format!("{{\"page_id\":{},\"stamp\":{},\"buckets\":[{}]}}", p.page_id, json_str(&p.stamp), bs.join(",")));
    }
    let ink = s.remote_ink.iter().map(ink_ann_json).collect::<Result<Vec<_>, _>>()?;
    let hl = s.remote_highlights.iter().map(highlight_json).collect::<Result<Vec<_>, _>>()?;
    let doc = format!(
        "{{\"schema_version\":1,\"attachment_key\":{},\"last_sync_ms\":{},\"local\":[{}],\"suppressed\":[{}],\"page_stamps\":[{}],\"remote_ink\":[{}],\"remote_highlights\":[{}]}}",
        json_str(&s.attachment_key),
        s.last_sync_ms,
        local.join(","),
        supp.join(","),
        stamps.join(","),
        ink.join(","),
        hl.join(",")
    );
    if doc.len() > ZOTERO_INK_SYNC_MAX {
        return Err("serialized zotero_ink_sync.json exceeds 8 MiB cap".to_string());
    }
    Ok(doc)
}

pub fn parse_ink_sync_state(bytes: &[u8]) -> Option<InkSyncState> {
    let (toks, _) = crate::zotero_bridge::parse_json(bytes)?;
    if toks[0].type_ != JsonType::Object || !crate::zotero_bridge::schema_ok(bytes, &toks, 0) {
        return None;
    }
    let attachment_key = crate::zotero_bridge::jstr_strict(bytes, &toks, 0, "attachment_key", 32)?;
    let objs = |key: &str| -> Vec<i32> { val(&toks, 0, key, bytes).map(|a| children(&toks, a)).unwrap_or_default() };
    let mut local = Vec::new();
    for o in objs("local") {
        let (Some(key), Some(bucket), Some(hash)) = (
            crate::zotero_bridge::jstr_strict(bytes, &toks, o, "key", 16),
            crate::zotero_bridge::jstr_strict(bytes, &toks, o, "bucket", 128),
            crate::zotero_bridge::jstr_strict(bytes, &toks, o, "hash", 64),
        ) else {
            continue;
        };
        let (Some(page_id), Some(page_index)) = (o_u32(bytes, &toks, o, "page_id"), o_u32(bytes, &toks, o, "page_index")) else { continue };
        if !valid_zkey(&key) {
            continue;
        }
        local.push(InkBinding { key, page_id, page_index, bucket, hash, version: o_u64(bytes, &toks, o, "version").unwrap_or(0) });
    }
    let mut suppressed = Vec::new();
    for o in objs("suppressed") {
        let (Some(bucket), Some(hash), Some(page_id)) = (
            crate::zotero_bridge::jstr_strict(bytes, &toks, o, "bucket", 128),
            crate::zotero_bridge::jstr_strict(bytes, &toks, o, "hash", 64),
            o_u32(bytes, &toks, o, "page_id"),
        ) else {
            continue;
        };
        suppressed.push(InkSuppressed { page_id, bucket, hash });
    }
    let mut page_stamps = Vec::new();
    for o in objs("page_stamps") {
        let (Some(page_id), Some(stamp)) = (o_u32(bytes, &toks, o, "page_id"), crate::zotero_bridge::jstr_strict(bytes, &toks, o, "stamp", 256)) else {
            continue;
        };
        let mut buckets = Vec::new();
        if let Some(a) = val(&toks, o, "buckets", bytes) {
            for b in children(&toks, a) {
                let (Some(bucket), Some(hash), Some(pi), Some(w), Some(c)) = (
                    crate::zotero_bridge::jstr_strict(bytes, &toks, b, "bucket", 128),
                    crate::zotero_bridge::jstr_strict(bytes, &toks, b, "hash", 64),
                    o_u32(bytes, &toks, b, "page_index"),
                    o_f64(bytes, &toks, b, "width"),
                    crate::zotero_bridge::jstr_strict(bytes, &toks, b, "color", 64),
                ) else {
                    continue;
                };
                buckets.push((bucket, hash, pi, w, c));
            }
        }
        page_stamps.push(PageStamp { page_id, stamp, buckets });
    }
    Some(InkSyncState {
        schema_version: 1,
        attachment_key,
        last_sync_ms: o_u64(bytes, &toks, 0, "last_sync_ms").unwrap_or(0),
        local,
        suppressed,
        page_stamps,
        remote_ink: parse_ink_array(bytes, &toks, 0, "remote_ink"),
        remote_highlights: parse_highlight_array(bytes, &toks, 0, "remote_highlights"),
    })
}

pub fn ink_sync_load(nb_id: &str) -> Option<InkSyncState> {
    let dir = crate::zotero_sync::notebook_dir_verified(nb_id).ok()?;
    let path = format!("{dir}/{ZOTERO_INK_SYNC_FILE}");
    match crate::zotero_bridge::bridge_read(&path, ZOTERO_INK_SYNC_MAX) {
        Ok(Some(b)) => parse_ink_sync_state(&b),
        _ => None,
    }
}

pub fn ink_sync_save(nb_id: &str, s: &InkSyncState) -> Result<(), String> {
    let dir = crate::zotero_sync::notebook_dir_verified(nb_id)?;
    let doc = serialize_ink_sync_state(s)?;
    crate::zotero_sync::write_atomic_in_existing_dir(&dir, ZOTERO_INK_SYNC_FILE, doc.as_bytes())
}

pub fn local_buckets_from_stamp(st: &PageStamp) -> Vec<LocalBucket> {
    st.buckets
        .iter()
        .map(|(bucket, hash, pi, w, c)| LocalBucket {
            page_id: st.page_id,
            page_index: *pi,
            bucket: bucket.clone(),
            width: *w,
            color: c.clone(),
            paths: None,
            hash: hash.clone(),
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Reconciliation
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq)]
pub struct InkPlan {
    pub payload: ZkUpsertPayload,
    /// (page_id, bucket, hash, page_index), parallel to payload.annotations
    pub ann_targets: Vec<(u32, String, String, u32)>,
    pub keep: Vec<InkBinding>,
    pub deleting: Vec<String>,
    pub preserved_remote: Vec<String>,
    pub suppressed: Vec<InkSuppressed>,
    pub conflicts: u32,
    pub stale_pages: Vec<u32>,
}

fn remote_hash(r: &ZkInkAnnotation) -> String {
    canonical_hash(r.page_index, r.width, &r.color, &r.paths)
}

fn push_stale(plan: &mut InkPlan, page_id: u32) {
    if !plan.stale_pages.contains(&page_id) {
        plan.stale_pages.push(page_id);
    }
}

/// Queue an annotation for `l`; with `key` "" it is a create. Paths not loaded -> the page goes stale.
fn plan_push(plan: &mut InkPlan, l: &LocalBucket, key: &str, comment: &str, tags: &[String]) -> bool {
    let Some(paths) = &l.paths else {
        push_stale(plan, l.page_id);
        return false;
    };
    plan.payload.annotations.push(ZkUpsertAnnotation {
        key: key.to_string(),
        page_index: l.page_index,
        width: l.width,
        color: l.color.clone(),
        comment: comment.to_string(),
        tags: tags.to_vec(),
        page_label: String::new(),
        sort_index: String::new(),
        paths: paths.clone(),
    });
    plan.ann_targets.push((l.page_id, l.bucket.clone(), l.hash.clone(), l.page_index));
    true
}

pub fn plan_ink_sync(state: &InkSyncState, pulled: &ZkInkPull, local: &[LocalBucket]) -> InkPlan {
    let mut plan = InkPlan::default();
    let find_local = |pid: u32, bucket: &str| local.iter().find(|l| l.page_id == pid && l.bucket == bucket);
    let mut bound: HashSet<(u32, &str)> = HashSet::new();
    for b in &state.local {
        bound.insert((b.page_id, b.bucket.as_str()));
        let l = find_local(b.page_id, &b.bucket);
        let r = pulled.ink.iter().find(|r| r.key == b.key);
        match (l, r) {
            (Some(l), Some(r)) => {
                let changed = remote_hash(r) != b.hash;
                if !changed {
                    if l.hash == b.hash {
                        plan.keep.push(InkBinding { version: r.version, ..b.clone() });
                    } else if plan_push(&mut plan, l, &b.key, &r.comment, &r.tags) {
                        // rebound from the result row
                    } else {
                        plan.keep.push(InkBinding { version: r.version, ..b.clone() });
                    }
                } else if l.hash == b.hash {
                    // Remote edit wins silently: the binding stays exactly as it is (its hash is the
                    // local content's), so the remote edit is never uploaded over and nothing is
                    // re-pushed while local stays untouched.
                    plan.keep.push(InkBinding { version: r.version, ..b.clone() });
                } else {
                    plan.preserved_remote.push(r.key.clone());
                    plan.conflicts += 1;
                    plan_push(&mut plan, l, "", "", &[]);
                }
            }
            (Some(l), None) => {
                if l.hash == b.hash {
                    plan.suppressed.push(InkSuppressed { page_id: b.page_id, bucket: b.bucket.clone(), hash: b.hash.clone() });
                } else {
                    plan_push(&mut plan, l, "", "", &[]);
                }
            }
            (None, Some(r)) => {
                if remote_hash(r) == b.hash {
                    plan.deleting.push(r.key.clone());
                    plan.payload.delete_keys.push(r.key.clone());
                } else {
                    plan.preserved_remote.push(r.key.clone());
                }
            }
            (None, None) => {}
        }
    }
    for l in local {
        if bound.contains(&(l.page_id, l.bucket.as_str())) {
            continue;
        }
        if let Some(s) = state.suppressed.iter().find(|s| s.page_id == l.page_id && s.bucket == l.bucket) {
            if s.hash == l.hash {
                plan.suppressed.push(s.clone());
                continue;
            }
        }
        plan_push(&mut plan, l, "", "", &[]);
    }
    // carried-over suppressions whose bucket is still unchanged were pushed above; avoid duplicates
    let mut seen = HashSet::new();
    plan.suppressed.retain(|s| seen.insert((s.page_id, s.bucket.clone(), s.hash.clone())));
    plan
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct InkSyncSummary {
    pub up: u32,
    pub down: u32,
    pub conflicts: u32,
    pub offline: bool,
    pub pushed: bool,
}

pub const UP_ARROW: &str = "\u{2191}";
pub const DOWN_ARROW: &str = "\u{2193}";

pub fn summary_toast(s: &InkSyncSummary) -> String {
    if s.offline {
        return "Offline \u{2014} saved locally".to_string();
    }
    if s.up == 0 && s.down == 0 && s.conflicts == 0 {
        return "Zotero: up to date".to_string();
    }
    let mut t = format!("Zotero synced: {UP_ARROW}{} {DOWN_ARROW}{}", s.up, s.down);
    match s.conflicts {
        0 => {}
        1 => t.push_str(" (1 conflict preserved)"),
        n => t.push_str(&format!(" ({n} conflicts preserved)")),
    }
    t
}

pub fn apply_ink_sync(
    state: &InkSyncState,
    pulled: &ZkInkPull,
    plan: &InkPlan,
    result: Option<&ZkUpsertResult>,
    local: &[LocalBucket],
    now_ms: u64,
) -> (InkSyncState, InkSyncSummary) {
    let _ = local;
    let mut next = InkSyncState {
        schema_version: 1,
        attachment_key: if pulled.attachment_key.is_empty() { state.attachment_key.clone() } else { pulled.attachment_key.clone() },
        last_sync_ms: now_ms,
        ..Default::default()
    };
    let mut up = 0;
    let settled = result.is_some() || plan.payload.is_empty();
    if settled {
        next.local = plan.keep.clone();
        next.suppressed = plan.suppressed.clone();
        if let Some(res) = result {
            up = res.created + res.updated + res.deleted;
            for row in &res.results {
                if row.index as usize >= plan.payload.annotations.len() {
                    continue;
                }
                if !matches!(row.action.as_str(), "created" | "updated" | "unchanged") || row.key.is_empty() {
                    continue;
                }
                let (page_id, bucket, hash, page_index) = plan.ann_targets[row.index as usize].clone();
                next.local.retain(|b| !(b.page_id == page_id && b.bucket == bucket) && b.key != row.key);
                next.local.push(InkBinding { key: row.key.clone(), page_id, page_index, bucket, hash, version: row.version });
            }
        }
    } else {
        // the upsert failed or was skipped: bindings stay as they were, refreshed from `keep`
        next.local = state
            .local
            .iter()
            .map(|b| plan.keep.iter().find(|k| k.key == b.key).cloned().unwrap_or_else(|| b.clone()))
            .collect();
        next.suppressed = state.suppressed.clone();
    }
    let bound: HashSet<&str> = next.local.iter().map(|b| b.key.as_str()).collect();
    let deleting: HashSet<&str> = if settled { plan.deleting.iter().map(|k| k.as_str()).collect() } else { HashSet::new() };
    next.remote_ink = pulled.ink.iter().filter(|r| !bound.contains(r.key.as_str()) && !deleting.contains(r.key.as_str())).cloned().collect();
    next.remote_highlights = pulled.highlights.clone();
    next.page_stamps = state.page_stamps.iter().filter(|p| !plan.stale_pages.contains(&p.page_id)).cloned().collect();

    let old_ink: HashMap<&str, u64> = state.remote_ink.iter().map(|r| (r.key.as_str(), r.version)).collect();
    let old_hl: HashMap<&str, u64> = state.remote_highlights.iter().map(|r| (r.key.as_str(), r.version)).collect();
    let down = next.remote_ink.iter().filter(|r| old_ink.get(r.key.as_str()) != Some(&r.version)).count()
        + next.remote_highlights.iter().filter(|r| old_hl.get(r.key.as_str()) != Some(&r.version)).count();
    let summary = InkSyncSummary { up, down: down as u32, conflicts: plan.conflicts, offline: false, pushed: false };
    (next, summary)
}

// ---------------------------------------------------------------------------------------------
// Page mapping
// ---------------------------------------------------------------------------------------------

/// Notebook page index -> 0-based PDF page index. When the number of document-role image papers equals
/// `page_count`, the k-th document page maps to k and every other page to None; otherwise page i maps
/// to Some(i) for i < page_count.
pub fn pdf_page_index_map(papers: &[PaperKind], page_count: u32) -> Vec<Option<u32>> {
    let docs = papers.iter().filter(|p| paper_is_document(**p)).count();
    if docs as u64 == page_count as u64 {
        let mut k = 0u32;
        papers
            .iter()
            .map(|p| {
                if paper_is_document(*p) {
                    k += 1;
                    Some(k - 1)
                } else {
                    None
                }
            })
            .collect()
    } else {
        (0..papers.len()).map(|i| (i < page_count as usize).then_some(i as u32)).collect()
    }
}

// ---------------------------------------------------------------------------------------------
// Overlays
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub enum OverlayPrim {
    /// px = px * mul / 255
    Wash { r: [f32; 4], mul: u8 },
    Underline { x0: f32, x1: f32, y: f32, thick: f32, level: u8 },
    /// darken-only (min) blending
    Ink { width_px: f32, level: u8, pts: Vec<(f32, f32)> },
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PageOverlay {
    pub page_w: i32,
    pub page_h: i32,
    pub prims: Vec<OverlayPrim>,
}

fn color_lum(hex: &str) -> f64 {
    let h = hex.strip_prefix('#').unwrap_or(hex);
    if h.len() == 6 && h.bytes().all(|b| b.is_ascii_hexdigit()) {
        let c = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).unwrap_or(0) as f64;
        (0.299 * c(0) + 0.587 * c(2) + 0.114 * c(4)) / 255.0
    } else {
        0.0
    }
}

fn highlight_mul(color: &str) -> u8 {
    let lum = if color.strip_prefix('#').is_some_and(|h| h.len() == 6 && h.bytes().all(|b| b.is_ascii_hexdigit())) {
        color_lum(color)
    } else {
        color_lum(ZOTERO_HIGHLIGHTER_COLOR)
    };
    (196.0 + lum * 18.0).round().clamp(196.0, 214.0) as u8
}

pub fn build_page_overlays(
    state: &InkSyncState,
    src: &ZoteroNotebookSource,
    papers: &[PaperKind],
    page_w: i32,
    page_h: i32,
) -> Vec<(u32, PageOverlay)> {
    let map = pdf_page_index_map(papers, src.page_count);
    // paper value -> the PDF pages (None = a page that maps nowhere) it is used for
    let mut uses: BTreeMap<u32, HashSet<Option<u32>>> = BTreeMap::new();
    for (i, p) in papers.iter().enumerate() {
        if paper_is_image(*p) {
            uses.entry(p.0).or_default().insert(map.get(i).copied().flatten());
        }
    }
    let mut out = Vec::new();
    for (paper, set) in uses {
        if set.len() != 1 {
            continue;
        }
        let Some(Some(pi)) = set.into_iter().next() else { continue };
        let Some(geom) = src.pages.iter().find(|g| g.page_index == pi) else { continue };
        let fit = page_fit_for(geom, page_w, page_h);
        let mut prims = Vec::new();
        for h in state.remote_highlights.iter().filter(|h| h.page_index == pi) {
            for r in &h.rects {
                let g = rect_user_to_grimoire(geom, &fit, *r);
                if !g.iter().all(|v| v.is_finite()) {
                    continue;
                }
                if h.kind == "underline" {
                    let hgt = g[3] - g[1];
                    prims.push(OverlayPrim::Underline {
                        x0: g[0] as f32,
                        x1: g[2] as f32,
                        y: g[3] as f32,
                        thick: (0.08 * hgt).max(2.5) as f32,
                        level: 70,
                    });
                } else {
                    prims.push(OverlayPrim::Wash { r: [g[0] as f32, g[1] as f32, g[2] as f32, g[3] as f32], mul: highlight_mul(&h.color) });
                }
            }
        }
        for a in state.remote_ink.iter().filter(|a| a.page_index == pi) {
            let level = (color_lum(&a.color) * 110.0) as u8;
            let width_px = (a.width * fit.scale).max(1.5) as f32;
            for path in &a.paths {
                let pts: Vec<(f32, f32)> = path
                    .chunks_exact(2)
                    .map(|c| user_to_grimoire(geom, &fit, c[0], c[1]))
                    .filter(|p| p.0.is_finite() && p.1.is_finite())
                    .map(|p| (p.0 as f32, p.1 as f32))
                    .collect();
                if !pts.is_empty() {
                    prims.push(OverlayPrim::Ink { width_px, level, pts });
                }
            }
        }
        if !prims.is_empty() {
            out.push((paper, PageOverlay { page_w, page_h, prims }));
        }
    }
    out
}

struct OverlayRegistry {
    nb: String,
    map: HashMap<u32, PageOverlay>,
}

static OVERLAYS: Mutex<Option<OverlayRegistry>> = Mutex::new(None);
static OVERLAY_GEN: AtomicU64 = AtomicU64::new(0);

fn overlays_lock() -> std::sync::MutexGuard<'static, Option<OverlayRegistry>> {
    OVERLAYS.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn overlay_set(nb_id: &str, overlays: Vec<(u32, PageOverlay)>) {
    *overlays_lock() = Some(OverlayRegistry { nb: nb_id.to_string(), map: overlays.into_iter().collect() });
    OVERLAY_GEN.fetch_add(1, Ordering::SeqCst);
}

pub fn overlay_clear() {
    *overlays_lock() = None;
    OVERLAY_GEN.fetch_add(1, Ordering::SeqCst);
}

pub fn overlay_generation() -> u64 {
    OVERLAY_GEN.load(Ordering::SeqCst)
}

pub fn overlay_prim_count(nb_id: &str, paper: u32) -> usize {
    overlays_lock().as_ref().filter(|r| r.nb == nb_id).and_then(|r| r.map.get(&paper)).map_or(0, |o| o.prims.len())
}

/// Blend one pixel toward `level` (darken only) by `cov` 0..1.
#[inline]
fn darken(c: &mut Canvas, x: i32, y: i32, level: u8, cov: f32) {
    let i = y as usize * c.stride.max(0) as usize + x as usize;
    if let Some(p) = c.px.get_mut(i) {
        let target = (*p).min(level) as f32;
        let cur = *p as f32;
        *p = (cur + (target - cur) * cov.clamp(0.0, 1.0)).round().clamp(0.0, 255.0) as u8;
    }
}

fn paint_prims(c: &mut Canvas, o: &PageOverlay, eff: Rect) {
    let sx = c.w as f32 / o.page_w.max(1) as f32;
    let sy = c.h as f32 / o.page_h.max(1) as f32;
    let (ex0, ey0, ex1, ey1) = (eff.x, eff.y, eff.x + eff.w, eff.y + eff.h);
    for prim in &o.prims {
        match prim {
            OverlayPrim::Wash { r, mul } => {
                let x0 = ((r[0] * sx).round() as i32).max(ex0);
                let y0 = ((r[1] * sy).round() as i32).max(ey0);
                let x1 = ((r[2] * sx).round() as i32).min(ex1);
                let y1 = ((r[3] * sy).round() as i32).min(ey1);
                for y in y0..y1 {
                    for x in x0..x1 {
                        let i = y as usize * c.stride.max(0) as usize + x as usize;
                        if let Some(p) = c.px.get_mut(i) {
                            *p = ((*p as u32 * *mul as u32 + 127) / 255) as u8;
                        }
                    }
                }
            }
            OverlayPrim::Underline { x0, x1, y, thick, level } => {
                let (fx0, fx1) = (x0 * sx, x1 * sx);
                let (fy0, fy1) = ((y - thick) * sy, *y * sy);
                let xs = (fx0.floor() as i32).max(ex0);
                let xe = (fx1.ceil() as i32).min(ex1);
                let ys = (fy0.floor() as i32).max(ey0);
                let ye = (fy1.ceil() as i32).min(ey1);
                for py in ys..ye {
                    let cy = ((py + 1) as f32).min(fy1) - (py as f32).max(fy0);
                    for px in xs..xe {
                        let cx = ((px + 1) as f32).min(fx1) - (px as f32).max(fx0);
                        if cx > 0.0 && cy > 0.0 {
                            darken(c, px, py, *level, cx * cy);
                        }
                    }
                }
            }
            OverlayPrim::Ink { width_px, level, pts } => {
                let r = (width_px * sx.min(sy) * 0.5).max(0.5);
                let mut prev: Option<(f32, f32)> = None;
                for &(px, py) in pts {
                    let cur = (px * sx, py * sy);
                    let a = prev.unwrap_or(cur);
                    prev = Some(cur);
                    let xs = ((a.0.min(cur.0) - r - 1.0).floor() as i32).max(ex0);
                    let xe = ((a.0.max(cur.0) + r + 1.0).ceil() as i32).min(ex1);
                    let ys = ((a.1.min(cur.1) - r - 1.0).floor() as i32).max(ey0);
                    let ye = ((a.1.max(cur.1) + r + 1.0).ceil() as i32).min(ey1);
                    if xs >= xe || ys >= ye {
                        continue;
                    }
                    let (dx, dy) = (cur.0 - a.0, cur.1 - a.1);
                    let l2 = dx * dx + dy * dy;
                    for y in ys..ye {
                        for x in xs..xe {
                            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
                            let t = if l2 > 1e-9 { (((fx - a.0) * dx + (fy - a.1) * dy) / l2).clamp(0.0, 1.0) } else { 0.0 };
                            let (qx, qy) = (a.0 + t * dx, a.1 + t * dy);
                            let d = ((fx - qx).powi(2) + (fy - qy).powi(2)).sqrt();
                            let cov = r + 0.5 - d;
                            if cov > 0.0 {
                                darken(c, x, y, *level, cov);
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Paint the registered overlay for (nb, paper) into `c`, clipped to `region` and `c.clip`. A no-op
/// (one lock and one lookup) when nothing is registered for it.
pub fn overlay_paint(c: &mut Canvas, nb: &str, paper: PaperKind, region: Rect) {
    let guard = overlays_lock();
    let Some(reg) = guard.as_ref() else { return };
    if reg.nb != nb {
        return;
    }
    let Some(o) = reg.map.get(&paper.0) else { return };
    let eff = rect_intersect(rect_intersect(region, c.clip), Rect { x: 0, y: 0, w: c.w, h: c.h });
    if rect_is_empty(eff) || o.page_w <= 0 || o.page_h <= 0 {
        return;
    }
    paint_prims(c, o, eff);
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prelude::{canvas_init, paper_image, PenSample, TOOL_ERASER, TOOL_PENCIL};

    fn geom(w: f64, h: f64, rot: i32, m: [f64; 6], crop: [f64; 4]) -> ZkPdfPageGeometry {
        ZkPdfPageGeometry { page_index: 0, width_pt: w, height_pt: h, rotation: rot, page_label: "1".into(), crop_box: crop, user_to_page: m }
    }

    fn all_geoms() -> Vec<(&'static str, ZkPdfPageGeometry)> {
        let (a4w, a4h) = (595.28, 841.89);
        vec![
            ("a4", geom(a4w, a4h, 0, [1.0, 0.0, 0.0, -1.0, 0.0, a4h], [0.0, 0.0, a4w, a4h])),
            ("letter-landscape", geom(792.0, 612.0, 0, [1.0, 0.0, 0.0, -1.0, 0.0, 612.0], [0.0, 0.0, 792.0, 612.0])),
            ("rot90", geom(792.0, 612.0, 90, [0.0, 1.0, 1.0, 0.0, 0.0, 0.0], [0.0, 0.0, 612.0, 792.0])),
            ("rot180", geom(612.0, 792.0, 180, [-1.0, 0.0, 0.0, 1.0, 612.0, 0.0], [0.0, 0.0, 612.0, 792.0])),
            ("rot270", geom(792.0, 612.0, 270, [0.0, -1.0, -1.0, 0.0, 792.0, 612.0], [0.0, 0.0, 612.0, 792.0])),
            ("cropped", geom(400.0, 500.0, 0, [1.0, 0.0, 0.0, -1.0, -50.0, 600.0], [50.0, 100.0, 450.0, 600.0])),
        ]
    }

    #[test]
    fn rotation_matrices_map_corners() {
        // 612x792 page, /Rotate 90: user bottom-right (612,0) lands at display bottom-left (0,612)
        let g = &all_geoms()[2].1;
        assert_eq!(g.user_to_display(612.0, 0.0), (0.0, 612.0));
        assert_eq!(g.user_to_display(0.0, 0.0), (0.0, 0.0));
        // 180: user bottom-left -> display top-right; top-left -> bottom-right
        let g = &all_geoms()[3].1;
        assert_eq!(g.user_to_display(0.0, 0.0), (612.0, 0.0));
        assert_eq!(g.user_to_display(0.0, 792.0), (612.0, 792.0));
        // 270: user bottom-left -> display bottom-right (792,612); top-left -> bottom-left (0,612)
        let g = &all_geoms()[4].1;
        assert_eq!(g.user_to_display(0.0, 0.0), (792.0, 612.0));
        assert_eq!(g.user_to_display(0.0, 792.0), (0.0, 612.0));
        // crop origin: the crop's bottom-left is the display bottom-left
        let g = &all_geoms()[5].1;
        assert_eq!(g.user_to_display(50.0, 100.0), (0.0, 500.0));
        assert_eq!(g.user_to_display(450.0, 600.0), (400.0, 0.0));
    }

    #[test]
    fn roundtrip_px_user_px() {
        for (name, g) in all_geoms() {
            let (pw, ph) = (1860, 2480);
            let fit = page_fit_for(&g, pw, ph);
            for &(x, y) in &[(300.0, 400.0), (1000.5, 1500.25), (fit.ox + 10.0, fit.oy + 10.0), (1500.0, 2000.0)] {
                let (ux, uy) = grimoire_to_user(&g, &fit, x, y).unwrap();
                let (ux, uy) = (round2(ux).unwrap(), round2(uy).unwrap());
                let (bx, by) = user_to_grimoire(&g, &fit, ux, uy);
                assert!((bx - x).abs() < 0.5 && (by - y).abs() < 0.5, "{name}: ({x},{y}) -> ({bx},{by})");
            }
        }
    }

    #[test]
    fn round2_edges() {
        assert_eq!(round2(-0.0).unwrap().to_bits(), 0.0f64.to_bits());
        assert_eq!(round2(-0.001).unwrap().to_bits(), 0.0f64.to_bits());
        assert_eq!(round2(f64::NAN), None);
        assert_eq!(round2(f64::INFINITY), None);
        assert_eq!(round2(1.005 + 0.2349), Some(1.24));
    }

    #[test]
    fn rdp_endpoints_and_reduction() {
        let line: Vec<(f64, f64)> = (0..=200).map(|i| (i as f64, 2.0 * i as f64)).collect();
        let s = rdp_simplify(&line, RDP_EPS_PT);
        assert_eq!(s, vec![(0.0, 0.0), (200.0, 400.0)]);
        let bent = vec![(0.0, 0.0), (10.0, 0.0), (10.0, 10.0)];
        let s = rdp_simplify(&bent, 0.4);
        assert_eq!(s.len(), 3);
        assert_eq!(s[0], bent[0]);
        assert_eq!(*s.last().unwrap(), bent[2]);
        assert_eq!(rdp_simplify(&[(1.0, 1.0)], 0.4).len(), 1);
    }

    #[test]
    fn width_clamp() {
        let fit = PageFit { scale: 3.0, ox: 0.0, oy: 0.0 };
        assert_eq!(width_px_to_pt(0.1, &fit), 0.5);
        assert_eq!(width_px_to_pt(1e6, &fit), 200.0);
        assert_eq!(width_px_to_pt(3.75, &fit), 1.25);
        let p = ToolParams { size_min_mm: 0.5, size_max_mm: 1.5, ..Default::default() };
        assert!((stroke_width_px(&p, 254.0) - 10.0).abs() < 1e-9);
    }

    fn stroke(tool: crate::prelude::ToolKind, pts: &[(f32, f32)]) -> Box<Stroke> {
        let p = ToolParams { size_min_mm: 0.4, size_max_mm: 0.4, ..Default::default() };
        let mut s = crate::prelude::stroke_new(tool, &p, 0);
        for &(x, y) in pts {
            s.pts.push(PenSample { x, y, pressure: 0.5, ..Default::default() });
        }
        s
    }

    fn a4() -> ZkPdfPageGeometry {
        all_geoms().remove(0).1
    }

    #[test]
    fn buckets_exclude_erase_and_floor() {
        let mut page = crate::prelude::page_new(1860, 2480, PaperKind(0));
        page.strokes.push(stroke(TOOL_PENCIL, &[(300.0, 300.0), (400.0, 300.0)]));
        page.strokes.push(stroke(TOOL_PENCIL, &[(300.0, 500.0), (400.0, 500.0)]));
        page.strokes.push(stroke(TOOL_ERASER, &[(310.0, 300.0), (320.0, 300.0)]));
        page.strokes.push(stroke(TOOL_ERASER, &[(-6000.0, -6000.0)]));
        page.strokes.push(stroke(TOOL_HIGHLIGHTER, &[(300.0, 700.0), (500.0, 700.0)]));
        let b = page_local_buckets(&page, 7, 3, &a4(), 300.0);
        assert_eq!(b.len(), 2);
        assert!(b.windows(2).all(|w| w[0].bucket < w[1].bucket));
        let black = b.iter().find(|x| x.color == "#000000").unwrap();
        assert_eq!(black.paths.as_ref().unwrap().len(), 2);
        assert_eq!(black.page_id, 7);
        assert_eq!(black.page_index, 3);
        assert_eq!(black.hash.len(), 16);
        assert!(black.bucket.starts_with("#000000|"));
        assert!(b.iter().any(|x| x.color == ZOTERO_HIGHLIGHTER_COLOR));
        // deterministic
        assert_eq!(b, page_local_buckets(&page, 7, 3, &a4(), 300.0));
    }

    #[test]
    fn single_point_stroke_is_two_points() {
        let mut page = crate::prelude::page_new(1860, 2480, PaperKind(0));
        page.strokes.push(stroke(TOOL_PENCIL, &[(500.0, 500.0)]));
        let b = page_local_buckets(&page, 1, 0, &a4(), 300.0);
        let p = &b[0].paths.as_ref().unwrap()[0];
        assert_eq!(p.len(), 4);
        assert_eq!(p[0], p[2]);
        assert_eq!(p[1], p[3]);
    }

    const PULL: &str = r##"{
  "schema_version": 1, "attachment_key": "EFGH5678", "synced": true,
  "ink": [
    {"key": "K9M2P4Q7", "version": 42, "pageIndex": 0, "paths": [[100.5, 700.25, 120.0, 695.5]], "width": 2.0,
     "color": "#000000", "comment": "", "tags": [], "sortIndex": "00000|000000|00091", "pageLabel": "1",
     "dirty": false, "conflict": false, "dateModified": "2026-11-24T05:00:00Z"},
    {"key": "bad", "version": 1, "pageIndex": 0, "paths": [[1,2]], "width": 1},
    {"key": "ODDPATH1", "version": 1, "pageIndex": 0, "paths": [[1,2,3]], "width": 1}
  ],
  "highlights": [
    {"key": "HL123456", "version": 12, "type": "highlight", "pageIndex": 0, "pageLabel": "1",
     "sortIndex": "x", "color": "#ffd400", "text": "Normal science...", "comment": "", "tags": ["epistemology"],
     "rects": [[72.0, 500.0, 300.0, 514.0]]},
    {"key": "NOTE1234", "version": 1, "type": "note", "pageIndex": 0, "rects": [[1,2,3,4]]}
  ],
  "other_counts": {"note": 0, "image": 2, "text": 0}
}"##;

    #[test]
    fn parse_pull_sample() {
        let p = parse_ink_pull(PULL.as_bytes()).unwrap();
        assert_eq!(p.attachment_key, "EFGH5678");
        assert!(p.synced);
        assert_eq!(p.ink.len(), 1, "malformed entries skipped");
        let a = &p.ink[0];
        assert_eq!((a.key.as_str(), a.version, a.page_index, a.width), ("K9M2P4Q7", 42, 0, 2.0));
        assert_eq!(a.paths, vec![vec![100.5, 700.25, 120.0, 695.5]]);
        assert_eq!(a.sort_index, "00000|000000|00091");
        assert_eq!(p.highlights.len(), 1);
        assert_eq!(p.highlights[0].rects, vec![[72.0, 500.0, 300.0, 514.0]]);
        assert_eq!(p.highlights[0].kind, "highlight");
        assert_eq!(p.other_counts, vec![("note".to_string(), 0), ("image".to_string(), 2), ("text".to_string(), 0)]);
        assert!(parse_ink_pull(br#"{"schema_version":2,"attachment_key":"EFGH5678"}"#).is_err());
        assert!(parse_ink_pull(b"nope").is_err());
    }

    #[test]
    fn upsert_payload_and_result() {
        let p = ZkUpsertPayload {
            annotations: vec![ZkUpsertAnnotation {
                key: String::new(),
                page_index: 2,
                width: 1.25,
                color: "#000000".into(),
                comment: "a \"q\"\n\\".into(),
                tags: vec!["t1".into()],
                page_label: String::new(),
                sort_index: String::new(),
                paths: vec![vec![1.0, 2.5, -0.0, 4.0]],
            }],
            delete_keys: vec!["ABCD1234".into()],
        };
        assert!(!p.is_empty());
        let s = serialize_upsert_payload(&p).unwrap();
        let (toks, _) = crate::zotero_bridge::parse_json(s.as_bytes()).expect("valid json");
        assert_eq!(toks[0].type_, JsonType::Object);
        assert!(s.contains("\"comment\":\"a \\\"q\\\"\\n\\\\\""));
        assert!(s.contains("\"paths\":[[1.0,2.5,0.0,4.0]]"));
        assert!(!s.contains("pageLabel") && !s.contains("sortIndex"));
        assert!(s.contains("\"delete_keys\":[\"ABCD1234\"]"));
        let mut bad = p.clone();
        bad.annotations[0].width = f64::NAN;
        assert!(serialize_upsert_payload(&bad).is_err());
        assert!(ZkUpsertPayload::default().is_empty());

        let r = parse_upsert_result(
            br#"{"schema_version":1,"attachment_key":"EFGH5678","created":1,"updated":0,"deleted":1,"unchanged":0,
            "results":[{"index":0,"key":"K9M2P4Q7","action":"created","version":0,"dirty":true},
                       {"key":"ABCD1234","action":"deleted","version":3,"dirty":true}]}"#,
        )
        .unwrap();
        assert_eq!((r.created, r.deleted), (1, 1));
        assert_eq!(r.results.len(), 2);
        assert_eq!(r.results[0], ZkUpsertResultRow { index: 0, key: "K9M2P4Q7".into(), action: "created".into(), version: 0, dirty: true });
        assert_eq!(r.results[1].index, u32::MAX);
    }

    fn lb(pid: u32, bucket: &str, paths: Option<Vec<Vec<f64>>>, hash: &str) -> LocalBucket {
        LocalBucket { page_id: pid, page_index: pid, bucket: bucket.into(), width: 1.25, color: "#000000".into(), paths, hash: hash.into() }
    }

    fn remote_for(key: &str, l: &LocalBucket, paths: Vec<Vec<f64>>, version: u64) -> ZkInkAnnotation {
        ZkInkAnnotation { key: key.into(), version, page_index: l.page_index, paths, width: l.width, color: l.color.clone(), ..Default::default() }
    }

    fn pthz(v: f64) -> Vec<Vec<f64>> {
        vec![vec![v, v, v + 10.0, v + 10.0]]
    }

    fn bucket_of(pid: u32, v: f64) -> LocalBucket {
        let paths = pthz(v);
        let mut l = lb(pid, "#000000|1.25", Some(paths.clone()), "");
        l.hash = canonical_hash(l.page_index, l.width, &l.color, &paths);
        l
    }

    fn bind_for(key: &str, l: &LocalBucket) -> InkBinding {
        InkBinding { key: key.into(), page_id: l.page_id, page_index: l.page_index, bucket: l.bucket.clone(), hash: l.hash.clone(), version: 1 }
    }

    fn st(bindings: Vec<InkBinding>) -> InkSyncState {
        InkSyncState { schema_version: 1, attachment_key: "EFGH5678".into(), local: bindings, ..Default::default() }
    }

    fn pull(ink: Vec<ZkInkAnnotation>) -> ZkInkPull {
        ZkInkPull { attachment_key: "EFGH5678".into(), synced: true, ink, ..Default::default() }
    }

    #[test]
    fn plan_case1_unchanged_and_update() {
        let l = bucket_of(1, 100.0);
        let b = bind_for("KEYAAAAA", &l);
        let r = remote_for("KEYAAAAA", &l, pthz(100.0), 5);
        let plan = plan_ink_sync(&st(vec![b.clone()]), &pull(vec![r.clone()]), &[l.clone()]);
        assert!(plan.payload.is_empty());
        assert_eq!(plan.keep.len(), 1);
        assert_eq!(plan.keep[0].version, 5);
        // local edited: update the SAME key, carrying remote comment/tags
        let l2 = bucket_of(1, 200.0);
        let mut r2 = r.clone();
        r2.comment = "keep me".into();
        r2.tags = vec!["x".into()];
        let plan = plan_ink_sync(&st(vec![b.clone()]), &pull(vec![r2]), &[l2.clone()]);
        assert_eq!(plan.payload.annotations.len(), 1);
        assert_eq!(plan.payload.annotations[0].key, "KEYAAAAA");
        assert_eq!(plan.payload.annotations[0].comment, "keep me");
        assert_eq!(plan.payload.annotations[0].tags, vec!["x".to_string()]);
        assert_eq!(plan.ann_targets[0], (1, l2.bucket.clone(), l2.hash.clone(), 1));
        assert!(plan.keep.is_empty());
        // needs paths but cached -> stale
        let mut cached = l2.clone();
        cached.paths = None;
        let plan = plan_ink_sync(&st(vec![b]), &pull(vec![r]), &[cached]);
        assert_eq!(plan.stale_pages, vec![1]);
        assert!(plan.payload.is_empty());
        assert_eq!(plan.keep.len(), 1);
    }

    #[test]
    fn plan_case2_remote_edit_and_conflict() {
        let l = bucket_of(1, 100.0);
        let b = bind_for("KEYAAAAA", &l);
        let edited = remote_for("KEYAAAAA", &l, pthz(150.0), 9);
        // local unchanged: remote wins silently, nothing pushed, no conflict
        let plan = plan_ink_sync(&st(vec![b.clone()]), &pull(vec![edited.clone()]), &[l.clone()]);
        assert!(plan.payload.is_empty());
        assert_eq!(plan.conflicts, 0);
        assert_eq!(plan.keep[0].version, 9);
        // both changed: conflict, remote preserved, fresh create
        let l2 = bucket_of(1, 300.0);
        let plan = plan_ink_sync(&st(vec![b]), &pull(vec![edited]), &[l2]);
        assert_eq!(plan.conflicts, 1);
        assert_eq!(plan.preserved_remote, vec!["KEYAAAAA".to_string()]);
        assert_eq!(plan.payload.annotations.len(), 1);
        assert_eq!(plan.payload.annotations[0].key, "");
        assert!(plan.keep.is_empty());
    }

    #[test]
    fn plan_case3_remote_deleted_and_suppressed() {
        let l = bucket_of(1, 100.0);
        let b = bind_for("KEYAAAAA", &l);
        let plan = plan_ink_sync(&st(vec![b.clone()]), &pull(vec![]), &[l.clone()]);
        assert!(plan.payload.is_empty());
        assert_eq!(plan.suppressed, vec![InkSuppressed { page_id: 1, bucket: l.bucket.clone(), hash: l.hash.clone() }]);
        assert!(plan.keep.is_empty());
        // local changed + remote gone -> fresh create
        let l2 = bucket_of(1, 200.0);
        let plan = plan_ink_sync(&st(vec![b]), &pull(vec![]), &[l2]);
        assert_eq!(plan.payload.annotations.len(), 1);
        assert_eq!(plan.payload.annotations[0].key, "");
        // a suppressed unchanged bucket is not resurrected; a changed one is created and the entry dropped
        let mut s = st(vec![]);
        s.suppressed = vec![InkSuppressed { page_id: 1, bucket: l.bucket.clone(), hash: l.hash.clone() }];
        let plan = plan_ink_sync(&s, &pull(vec![]), &[l.clone()]);
        assert!(plan.payload.is_empty());
        assert_eq!(plan.suppressed.len(), 1);
        let plan = plan_ink_sync(&s, &pull(vec![]), &[bucket_of(1, 400.0)]);
        assert_eq!(plan.payload.annotations.len(), 1);
        assert!(plan.suppressed.is_empty());
    }

    #[test]
    fn plan_case4_local_gone() {
        let l = bucket_of(1, 100.0);
        let b = bind_for("KEYAAAAA", &l);
        let r = remote_for("KEYAAAAA", &l, pthz(100.0), 5);
        let plan = plan_ink_sync(&st(vec![b.clone()]), &pull(vec![r]), &[]);
        assert_eq!(plan.deleting, vec!["KEYAAAAA".to_string()]);
        assert_eq!(plan.payload.delete_keys, vec!["KEYAAAAA".to_string()]);
        let edited = remote_for("KEYAAAAA", &l, pthz(150.0), 9);
        let plan = plan_ink_sync(&st(vec![b.clone()]), &pull(vec![edited]), &[]);
        assert!(plan.payload.is_empty());
        assert_eq!(plan.preserved_remote, vec!["KEYAAAAA".to_string()]);
        let plan = plan_ink_sync(&st(vec![b]), &pull(vec![]), &[]);
        assert!(plan.payload.is_empty() && plan.keep.is_empty() && plan.deleting.is_empty());
    }

    fn created_result(key: &str) -> ZkUpsertResult {
        ZkUpsertResult {
            attachment_key: "EFGH5678".into(),
            created: 1,
            results: vec![ZkUpsertResultRow { index: 0, key: key.into(), action: "created".into(), version: 0, dirty: true }],
            ..Default::default()
        }
    }

    #[test]
    fn apply_create_rebind_unchanged_delete() {
        let l = bucket_of(1, 100.0);
        // create
        let s0 = st(vec![]);
        let p0 = pull(vec![]);
        let plan = plan_ink_sync(&s0, &p0, &[l.clone()]);
        assert_eq!(plan.payload.annotations.len(), 1);
        let (s1, sum) = apply_ink_sync(&s0, &p0, &plan, Some(&created_result("KEYAAAAA")), &[l.clone()], 1000);
        assert_eq!(sum.up, 1);
        assert_eq!(s1.local, vec![InkBinding { key: "KEYAAAAA".into(), page_id: 1, page_index: 1, bucket: l.bucket.clone(), hash: l.hash.clone(), version: 0 }]);
        assert_eq!(s1.last_sync_ms, 1000);
        // rebind: the pull now returns our own annotation (same content, new version): nothing to push, not "down"
        let r = remote_for("KEYAAAAA", &l, l.paths.clone().unwrap(), 11);
        let p1 = pull(vec![r.clone()]);
        let plan = plan_ink_sync(&s1, &p1, &[l.clone()]);
        assert!(plan.payload.is_empty());
        let (s2, sum) = apply_ink_sync(&s1, &p1, &plan, None, &[l.clone()], 2000);
        assert_eq!((sum.up, sum.down, sum.conflicts), (0, 0, 0));
        assert_eq!(s2.local[0].version, 11);
        assert!(s2.remote_ink.is_empty(), "own annotation is not a remote overlay");
        // unchanged idempotency
        let plan = plan_ink_sync(&s2, &p1, &[l.clone()]);
        let (s3, sum) = apply_ink_sync(&s2, &p1, &plan, None, &[l.clone()], 3000);
        assert!(plan.payload.is_empty());
        assert_eq!(s3.local, s2.local);
        assert_eq!(sum, InkSyncSummary::default());
        assert_eq!(summary_toast(&sum), "Zotero: up to date");
        // delete: bucket erased locally
        let plan = plan_ink_sync(&s3, &p1, &[]);
        assert_eq!(plan.payload.delete_keys.len(), 1);
        let res = ZkUpsertResult {
            deleted: 1,
            results: vec![ZkUpsertResultRow { index: u32::MAX, key: "KEYAAAAA".into(), action: "deleted".into(), version: 12, dirty: true }],
            ..Default::default()
        };
        let (s4, sum) = apply_ink_sync(&s3, &p1, &plan, Some(&res), &[], 4000);
        assert!(s4.local.is_empty());
        assert!(s4.remote_ink.is_empty());
        assert_eq!(sum.up, 1);
        // failed upsert keeps bindings and remote
        let (s5, _) = apply_ink_sync(&s3, &p1, &plan, None, &[], 5000);
        assert_eq!(s5.local, s3.local);
    }

    #[test]
    fn apply_down_count_and_stamps() {
        let mut s0 = st(vec![]);
        s0.page_stamps = vec![PageStamp { page_id: 1, ..Default::default() }, PageStamp { page_id: 2, ..Default::default() }];
        let mut p = pull(vec![remote_for("REMOTE01", &bucket_of(0, 1.0), pthz(1.0), 3)]);
        p.highlights = vec![ZkHighlight { key: "HL123456".into(), version: 1, kind: "highlight".into(), page_index: 0, color: "#ffd400".into(), rects: vec![[0.0, 0.0, 1.0, 1.0]] }];
        let mut cached = bucket_of(1, 100.0);
        cached.paths = None;
        let plan = plan_ink_sync(&s0, &p, &[cached.clone()]);
        assert_eq!(plan.stale_pages, vec![1]);
        let (s1, sum) = apply_ink_sync(&s0, &p, &plan, None, &[cached], 10);
        assert_eq!(sum.down, 2);
        assert_eq!(s1.page_stamps.len(), 1);
        assert_eq!(s1.page_stamps[0].page_id, 2);
        assert_eq!(s1.remote_ink.len(), 1);
        let plan = plan_ink_sync(&s1, &p, &[]);
        let (_, sum) = apply_ink_sync(&s1, &p, &plan, None, &[], 20);
        assert_eq!(sum.down, 0);
    }

    #[test]
    fn toast_strings() {
        assert_eq!(summary_toast(&InkSyncSummary { offline: true, up: 3, ..Default::default() }), "Offline \u{2014} saved locally");
        assert_eq!(summary_toast(&InkSyncSummary { up: 1, down: 2, ..Default::default() }), "Zotero synced: \u{2191}1 \u{2193}2");
        assert!(summary_toast(&InkSyncSummary { up: 1, conflicts: 1, ..Default::default() }).ends_with("(1 conflict preserved)"));
        assert!(summary_toast(&InkSyncSummary { up: 1, conflicts: 2, ..Default::default() }).ends_with("(2 conflicts preserved)"));
    }

    #[test]
    fn sidecar_roundtrip() {
        let s = InkSyncState {
            schema_version: 1,
            attachment_key: "EFGH5678".into(),
            last_sync_ms: 1_732_350_000_123,
            local: vec![InkBinding { key: "KEYAAAAA".into(), page_id: 3, page_index: 2, bucket: "#000000|1.25".into(), hash: "00ff00ff00ff00ff".into(), version: 7 }],
            suppressed: vec![InkSuppressed { page_id: 4, bucket: "#ffd400|8.00".into(), hash: "abcdabcdabcdabcd".into() }],
            page_stamps: vec![PageStamp {
                page_id: 3,
                stamp: "12|34".into(),
                buckets: vec![("#000000|1.25".into(), "00ff00ff00ff00ff".into(), 2, 1.25, "#000000".into())],
            }],
            remote_ink: vec![ZkInkAnnotation {
                key: "REMOTE01".into(),
                version: 9,
                page_index: 1,
                paths: vec![vec![1.5, 2.5, 3.5, 4.5]],
                width: 2.0,
                color: "#112233".into(),
                comment: "c\"x\n".into(),
                tags: vec!["a".into(), "b".into()],
                sort_index: "00000|000000|00001".into(),
                page_label: "ii".into(),
                dirty: true,
                conflict: false,
            }],
            remote_highlights: vec![ZkHighlight { key: "HL123456".into(), version: 2, kind: "underline".into(), page_index: 0, color: "#ffd400".into(), rects: vec![[1.0, 2.0, 3.0, 4.0]] }],
        };
        let json = serialize_ink_sync_state(&s).unwrap();
        assert_eq!(parse_ink_sync_state(json.as_bytes()), Some(s.clone()));
        assert_eq!(local_buckets_from_stamp(&s.page_stamps[0]).len(), 1);
        assert!(local_buckets_from_stamp(&s.page_stamps[0])[0].paths.is_none());
        assert!(parse_ink_sync_state(b"{}").is_none());
        assert!(parse_ink_sync_state(b"garbage").is_none());
    }

    #[test]
    fn page_map_modes() {
        let doc = |i| paper_image(crate::prelude::PAPER_ROLE_DOCUMENT, i);
        let other = paper_image(0, 99);
        let plain = PaperKind(0);
        // document-role count == page_count: k-th doc page -> k, others None
        let papers = [plain, doc(1), other, doc(2), doc(3)];
        assert_eq!(pdf_page_index_map(&papers, 3), vec![None, Some(0), None, Some(1), Some(2)]);
        // mismatch: index mapping for i < page_count
        assert_eq!(pdf_page_index_map(&papers, 4), vec![Some(0), Some(1), Some(2), Some(3), None]);
        assert_eq!(pdf_page_index_map(&[], 2), Vec::<Option<u32>>::new());
    }

    fn src_with(pages: u32) -> ZoteroNotebookSource {
        let g = a4();
        ZoteroNotebookSource {
            page_count: pages,
            pages: (0..pages).map(|i| ZkPdfPageGeometry { page_index: i, ..g.clone() }).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn overlays_build_and_ambiguity() {
        let doc = |i| paper_image(crate::prelude::PAPER_ROLE_DOCUMENT, i);
        let mut s = st(vec![]);
        s.remote_highlights = vec![
            ZkHighlight { key: "HL000001".into(), version: 1, kind: "highlight".into(), page_index: 0, color: "#ffd400".into(), rects: vec![[72.0, 500.0, 300.0, 514.0]] },
            ZkHighlight { key: "HL000002".into(), version: 1, kind: "underline".into(), page_index: 1, color: "#ffd400".into(), rects: vec![[72.0, 500.0, 300.0, 514.0]] },
        ];
        s.remote_ink = vec![remote_for("REMOTE01", &bucket_of(0, 1.0), vec![vec![100.0, 700.0, 150.0, 650.0]], 1)];
        let src = src_with(2);
        let ov = build_page_overlays(&s, &src, &[doc(5), doc(6)], 1860, 2480);
        assert_eq!(ov.len(), 2);
        let p0 = &ov.iter().find(|(p, _)| *p == doc(5).0).unwrap().1;
        assert!(matches!(p0.prims[0], OverlayPrim::Wash { .. }));
        assert!(matches!(p0.prims[1], OverlayPrim::Ink { .. }), "highlights first, then ink");
        let p1 = &ov.iter().find(|(p, _)| *p == doc(6).0).unwrap().1;
        assert!(matches!(p1.prims[0], OverlayPrim::Underline { .. }));
        // the same paper on two pages mapping to different PDF pages is skipped
        let ov = build_page_overlays(&s, &src, &[doc(5), doc(5)], 1860, 2480);
        assert!(ov.is_empty());
        // pages without prims are omitted
        let ov = build_page_overlays(&InkSyncState::default(), &src, &[doc(5), doc(6)], 1860, 2480);
        assert!(ov.is_empty());
    }

    #[test]
    fn overlay_paint_region_and_noop() {
        // registry is global: keep every assertion about it inside this one test
        let mut c = Canvas::default();
        assert_eq!(canvas_init(&mut c, 100, 100), 0);
        let region = Rect { x: 0, y: 0, w: 100, h: 100 };
        overlay_clear();
        let g0 = overlay_generation();
        overlay_paint(&mut c, "nb1", PaperKind(42), region);
        assert!(c.px.iter().all(|&p| p == 255), "no-op when unregistered");
        let ov = PageOverlay {
            page_w: 100,
            page_h: 100,
            prims: vec![
                OverlayPrim::Wash { r: [20.0, 20.0, 60.0, 40.0], mul: 200 },
                OverlayPrim::Ink { width_px: 4.0, level: 0, pts: vec![(10.0, 80.0), (90.0, 80.0)] },
            ],
        };
        overlay_set("nb1", vec![(42, ov)]);
        assert!(overlay_generation() > g0);
        assert_eq!(overlay_prim_count("nb1", 42), 2);
        assert_eq!(overlay_prim_count("other", 42), 0);
        overlay_paint(&mut c, "other", PaperKind(42), region);
        assert!(c.px.iter().all(|&p| p == 255), "wrong notebook is a no-op");
        // only the washed rect (and the ink) change, restricted to the region
        overlay_paint(&mut c, "nb1", PaperKind(42), Rect { x: 0, y: 0, w: 100, h: 50 });
        fn at(c: &Canvas, x: usize, y: usize) -> u8 {
            c.px[y * 100 + x]
        }
        assert_eq!(at(&c, 30, 30), (255u32 * 200 / 255) as u8);
        assert_eq!(at(&c, 10, 30), 255);
        assert_eq!(at(&c, 60, 30), 255);
        assert_eq!(at(&c, 30, 45), 255);
        assert_eq!(at(&c, 50, 80), 255, "ink outside the region untouched");
        overlay_paint(&mut c, "nb1", PaperKind(42), region);
        assert!(at(&c, 50, 80) < 10, "ink darkens inside the region");
        assert_eq!(at(&c, 50, 90), 255);
        // Wash is multiplicative: painting twice darkens, never lightens
        let before = at(&c, 30, 30);
        overlay_paint(&mut c, "nb1", PaperKind(42), region);
        assert!(at(&c, 30, 30) <= before);
        overlay_clear();
        assert_eq!(overlay_prim_count("nb1", 42), 0);
    }
}

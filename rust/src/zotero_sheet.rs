//! The Zotero sheet: an editor-hosted sheet with two tabs, Inbox and Library. Inbox lists the citation
//! events Zotero sent while Grimoire was closed (and any that arrived since); Library searches and
//! filters the catalog snapshot `library_index.json` by query and collection with pagination.
//! A row can be cited onto the open page, opened directly as a PDF notebook in Grimoire, or handed off
//! to Zotero via exit-42. Like Find and the Links bar, the sheet draws itself, takes the taps and says
//! what was asked (`ZsResult`); the editor does it. Its functions never reach back into the editor's state.
//!
//! With no bridge directory the sheet is inert: it has no rows and offers no action. Nothing here
//! follows a symlink, reads a credential, or opens a socket; all catalog and inbox bytes come from
//! `zotero_bridge.rs` and scrubbed status from `zotero_sync.rs`.

use crate::prelude::*;
use std::collections::HashSet;

/// The most rows the sheet shows at once.
pub const ZS_ROWS: usize = 8;

// Hit ids.
const ZS_CLOSE: i32 = 1;
const ZS_INBOX: i32 = 2;
const ZS_LIBRARY: i32 = 3;
const ZS_ROW: i32 = 4;
const ZS_CITE: i32 = 5;
const ZS_ADD: i32 = 6;
const ZS_OPEN: i32 = 7;
const ZS_DISCARD: i32 = 8;
const ZS_REFRESH: i32 = 9;
const ZS_SYNC: i32 = 10;
const ZS_OPEN_PDF: i32 = 11;
const ZS_SEARCH: i32 = 12;
const ZS_CLEAR_SEARCH: i32 = 13;
const ZS_COLLECTION: i32 = 14;
const ZS_PREV: i32 = 15;
const ZS_NEXT: i32 = 16;
const ZS_OPEN_CURRENT: i32 = 17;

/// Which side of the sheet is up.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ZoteroTab {
    #[default]
    Inbox,
    Library,
}
pub type ZsTab = ZoteroTab;

/// Where a row came from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ZoteroRowKind {
    #[default]
    Pending,
    Catalog,
}
pub type ZsRowKind = ZoteroRowKind;

/// One row, with everything an editor action needs. Strings are already cleaned by the bridge.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ZoteroSheetRow {
    pub kind: ZoteroRowKind,
    /// the citation event's id, or the catalog item's key
    pub id: String,
    pub item_key: String,
    pub attachment_key: String,
    /// Zotero's page number (0 for a catalog item, which has no particular page)
    pub page: u32,
    pub locator: String,
    pub quote: String,
    /// what the row shows, and the smaller line under it
    pub label: String,
    pub sub: String,
    /// attachment & local cache metadata for Phase 2 unified reader
    pub has_pdf: bool,
    pub pdf_cached: bool,
    pub has_epub: bool,
    pub has_snapshot: bool,
    pub tag: String,
    pub item: Option<ZoteroLibraryItem>,
}
pub type ZsRow = ZoteroSheetRow;

/// What a tap on the sheet asks the editor to do. The row is carried with the action so the editor
/// does not have to read the sheet back (and so a direct caller can drive the action in a test).
#[derive(Clone, Debug, Default, PartialEq)]
pub enum ZsResult {
    #[default]
    None,
    Close,
    /// the selection, the filter or the tab changed: the sheet was drawn again
    Changed,
    /// add this reference to the open page, consuming a pending citation when it is one
    Cite(ZoteroSheetRow),
    /// add a catalog item's reference to the open page
    Add(ZoteroSheetRow),
    /// write a handoff for this row and leave for Zotero ("Open in Zotero")
    Open(ZoteroSheetRow),
    /// open a PDF item directly in Grimoire (saving current essay first)
    OpenPdf(ZoteroSheetRow),
    /// hand off the currently open Zotero-bound PDF page directly to Zotero (escape hatch)
    OpenCurrentInZotero,
    /// discard a pending citation (a catalog row has nothing to discard)
    Discard(ZoteroSheetRow),
}

/// The sheet's whole state.
#[derive(Clone, Debug, Default)]
pub struct ZoteroSheet {
    pub open: bool,
    pub tab: ZoteroTab,
    pub filter: String,
    pub filter_before_kb: String,
    pub collection: String,
    pub collection_label: String,
    pub collections: Vec<(String, String)>,
    pub page: i32,
    pub pages: usize,
    pub per_page: usize,
    pub total: usize,
    pub rows: Vec<ZoteroSheetRow>,
    /// the selected row, 0-based; -1 with no rows
    pub selected: i32,
    pub rect: Rect,
    /// the screen this card was laid out for, so a refresh can keep its place
    pub aw: i32,
    pub ah: i32,
    pub hits: HitList,
    pub press: Press,
    pub pressed: Hit,
    pub has_pressed: bool,
    /// a short line under the title: how many citations/catalog items are shown
    pub status: String,
    pub custom_status: String,
    pub kb: Keyboard,
    /// When opened from a Zotero-bound PDF notebook: `(item_key, attachment_key, page_one_based)`.
    pub current_pdf: Option<(String, String, u32)>,
}

fn u(v: f32) -> i32 {
    ui_px(v)
}

fn pair(a: &str, b: &str) -> String {
    match (a.trim(), b.trim()) {
        (x, "") => x.to_string(),
        ("", y) => y.to_string(),
        (x, y) => format!("{x} — {y}"),
    }
}

fn row_tag(has_pdf: bool, pdf_cached: bool, has_epub: bool, has_snapshot: bool) -> String {
    if has_pdf {
        if pdf_cached {
            "PDF · Local".to_string()
        } else {
            "PDF · Cloud".to_string()
        }
    } else if has_epub {
        "EPUB".to_string()
    } else if has_snapshot {
        "Snapshot".to_string()
    } else {
        String::new()
    }
}

pub(crate) fn from_citation(c: &ZoteroCitation) -> ZoteroSheetRow {
    let label = if !c.short_cite.is_empty() {
        c.short_cite.clone()
    } else if !c.title.is_empty() {
        c.title.clone()
    } else {
        c.item_key.clone()
    };
    let cat_item = library_index_load().and_then(|idx| {
        idx.items
            .iter()
            .find(|it| it.item_key == c.item_key || (!c.attachment_key.is_empty() && it.attachment_key == c.attachment_key))
            .cloned()
    });
    let (has_pdf, pdf_cached, has_epub, has_snapshot) = match &cat_item {
        Some(it) => (
            it.has_pdf,
            it.pdf_cached || crate::zotero_sync::zotero_pdf_on_disk(it),
            it.has_epub,
            it.has_snapshot,
        ),
        None => (false, false, false, false),
    };
    ZoteroSheetRow {
        kind: ZoteroRowKind::Pending,
        id: c.id.clone(),
        item_key: c.item_key.clone(),
        attachment_key: c.attachment_key.clone(),
        page: c.page,
        locator: clean_text(&c.locator, 512),
        quote: clean_text(&c.quote, 4096),
        sub: pair(&pair(&c.authors, &c.year), &pair(&c.locator, &c.quote)),
        label,
        has_pdf,
        pdf_cached,
        has_epub,
        has_snapshot,
        tag: row_tag(has_pdf, pdf_cached, has_epub, has_snapshot),
        item: cat_item,
    }
}

fn from_item(i: &ZoteroLibraryItem) -> ZoteroSheetRow {
    let label = if !i.title.is_empty() {
        i.title.clone()
    } else if !i.citation_key.is_empty() {
        i.citation_key.clone()
    } else {
        i.item_key.clone()
    };
    let has_pdf = i.has_pdf && !i.attachment_key.is_empty();
    let pdf_cached = has_pdf && (i.pdf_cached || crate::zotero_sync::zotero_pdf_on_disk(i));
    ZoteroSheetRow {
        kind: ZoteroRowKind::Catalog,
        id: i.item_key.clone(),
        item_key: i.item_key.clone(),
        attachment_key: i.attachment_key.clone(),
        page: 0,
        locator: String::new(),
        quote: String::new(),
        sub: pair(&pair(&i.authors, &i.year), &i.citation_key),
        label,
        has_pdf,
        pdf_cached,
        has_epub: i.has_epub,
        has_snapshot: i.has_snapshot,
        tag: row_tag(has_pdf, pdf_cached, i.has_epub, i.has_snapshot),
        item: Some(i.clone()),
    }
}

fn build_collections_list(idx: &ZoteroLibraryIndex) -> Vec<(String, String)> {
    let mut out = vec![(String::new(), "All".to_string())];
    let mut seen = HashSet::new();
    seen.insert("all".to_string());
    for c in &idx.collections {
        let key = c.key.trim();
        let name = c.name.trim();
        let label = if !name.is_empty() { name } else { key };
        if label.is_empty() {
            continue;
        }
        let val = if !key.is_empty() { key.to_string() } else { label.to_string() };
        if seen.insert(label.to_lowercase()) {
            out.push((val, label.to_string()));
        }
    }
    for item in &idx.items {
        for name in &item.collections {
            let trimmed = name.trim();
            if !trimmed.is_empty() && seen.insert(trimmed.to_lowercase()) {
                out.push((trimmed.to_string(), trimmed.to_string()));
            }
        }
    }
    out
}

/// Opens the sheet and fills it from the bridge. With no bridge it opens empty and says so.
pub fn zotero_sheet_open(z: &mut ZoteroSheet, a: &App) {
    *z = ZoteroSheet {
        open: true,
        collection_label: "All".to_string(),
        collections: vec![(String::new(), "All".to_string())],
        pages: 1,
        per_page: ZS_ROWS,
        selected: -1,
        ..Default::default()
    };
    zotero_sheet_layout(z, a.w, a.h);
    zotero_sheet_refresh(z);
}

/// Closes it, leaving its data for a caller that wants to inspect it.
pub fn zotero_sheet_close(z: &mut ZoteroSheet) {
    keyboard_close(&mut z.kb);
    z.open = false;
    z.has_pressed = false;
    z.current_pdf = None;
}

/// Set a scrubbed status line on the sheet.
pub fn zotero_sheet_set_status(z: &mut ZoteroSheet, status: &str) {
    let clean = crate::zotero_sync::scrub_error_message(status);
    z.custom_status = clean.clone();
    z.status = clean;
}

fn rows_top(z: &ZoteroSheet, card_y: i32) -> i32 {
    card_y + if z.tab == ZoteroTab::Library { u(314.0) } else { u(244.0) }
}

fn compute_per_page(z: &ZoteroSheet) -> usize {
    let ry = rows_top(z, z.rect.y);
    let by = z.rect.y + z.rect.h - u(142.0);
    let rh = u(112.0);
    let rg = u(14.0);
    let avail = (by - u(14.0) - ry).max(rh);
    ((avail + rg) / (rh + rg)).clamp(1, ZS_ROWS as i32) as usize
}

/// The sheet's card on an `aw x ah` screen: centred, shrinking above the search keyboard when open.
pub fn zotero_sheet_layout(z: &mut ZoteroSheet, aw: i32, ah: i32) {
    z.aw = aw;
    z.ah = ah;
    let pad = u(40.0);
    let w = (aw - 2 * pad).min(u(1500.0)).max(u(360.0));
    let (cy, h) = if z.kb.open {
        let screen = Rect { x: 0, y: 0, w: aw, h: ah };
        let kb_r = keyboard_area(&z.kb, screen);
        z.kb.area = kb_r;
        let top = u(20.0);
        let avail = (kb_r.y - top - u(16.0)).max(u(520.0));
        let ch = avail.min(u(1660.0));
        (top, ch)
    } else {
        let ch = (ah - 2 * pad).min(u(1660.0)).max(u(520.0));
        ((ah - ch) / 2, ch)
    };
    z.rect = Rect { x: (aw - w) / 2, y: cy, w, h };
    z.per_page = compute_per_page(z);
    hits_clear(&mut z.hits);
    if !z.open {
        return;
    }
    let inn = rect_inflate(z.rect, -u(24.0));
    let hy = z.rect.y + u(18.0);
    let hh = u(96.0);
    let close = Rect { x: inn.x + inn.w - u(120.0), y: hy, w: u(120.0), h: hh };
    hits_add(&mut z.hits, close, ZS_CLOSE, 0);

    let busy = crate::zotero_sync::zk_bg_busy();
    let mut h_right = close.x - u(12.0);
    if crate::zotero_sync::zk_sync_available() {
        let st = if busy { ButtonStyle::Disabled } else { ButtonStyle::Plain };
        let sw = ui_button_need_w(hh, IconKind::Update, "Sync", st).max(u(188.0));
        let sr = Rect { x: h_right - sw, y: hy, w: sw, h: hh };
        if !busy {
            hits_add(&mut z.hits, sr, ZS_SYNC, 0);
        }
        h_right = sr.x - u(12.0);
    }
    if zotero_sheet_selected(z).is_some_and(|r| r.has_pdf && !r.attachment_key.is_empty()) {
        let st = if busy { ButtonStyle::Disabled } else { ButtonStyle::Primary };
        let ow = ui_button_need_w(hh, IconKind::None, "Open PDF", st).max(u(236.0));
        let or = Rect { x: h_right - ow, y: hy, w: ow, h: hh };
        if !busy {
            hits_add(&mut z.hits, or, ZS_OPEN_PDF, 0);
        }
        h_right = or.x - u(12.0);
    }
    if z.current_pdf.is_some() {
        let title_min_w = font_measure(FontFace::Bold, ui_text_size(), "Zotero") + u(28.0);
        let cw = ui_button_need_w(hh, IconKind::None, "Open in Zotero", ButtonStyle::Plain).max(u(270.0));
        if h_right - cw >= inn.x + title_min_w {
            let cr = Rect { x: h_right - cw, y: hy, w: cw, h: hh };
            hits_add(&mut z.hits, cr, ZS_OPEN_CURRENT, 0);
        }
    }

    let ty = z.rect.y + u(128.0);
    let tw = (inn.w - u(16.0)) / 2;
    hits_add(&mut z.hits, Rect { x: inn.x, y: ty, w: tw, h: u(96.0) }, ZS_INBOX, 0);
    hits_add(&mut z.hits, Rect { x: inn.x + tw + u(16.0), y: ty, w: tw, h: u(96.0) }, ZS_LIBRARY, 0);

    if z.tab == ZoteroTab::Library {
        let fy = z.rect.y + u(234.0);
        let fh = u(68.0);
        let mut right_x = inn.x + inn.w;
        if z.pages > 1 {
            let nav_w = u(74.0);
            let next_r = Rect { x: right_x - nav_w, y: fy, w: nav_w, h: fh };
            if (z.page as usize + 1) < z.pages {
                hits_add(&mut z.hits, next_r, ZS_NEXT, 0);
            }
            let page_txt = format!("{}/{}", z.page + 1, z.pages);
            let ptw = (font_measure(FontFace::Bold, ui_small_size(), &page_txt) + u(20.0)).max(u(104.0));
            let prev_r = Rect { x: next_r.x - ptw - nav_w, y: fy, w: nav_w, h: fh };
            if z.page > 0 {
                hits_add(&mut z.hits, prev_r, ZS_PREV, 0);
            }
            right_x = prev_r.x - u(12.0);
        }
        let coll_w = (inn.w * 30 / 100).clamp(u(320.0), u(460.0));
        let coll_r = Rect { x: right_x - coll_w, y: fy, w: coll_w, h: fh };
        hits_add(&mut z.hits, coll_r, ZS_COLLECTION, 0);

        let search_r = Rect {
            x: inn.x,
            y: fy,
            w: (coll_r.x - inn.x - u(12.0)).max(u(200.0)),
            h: fh,
        };
        if !z.filter.is_empty() {
            let clr_r = Rect {
                x: search_r.x + search_r.w - u(68.0),
                y: search_r.y + u(4.0),
                w: u(64.0),
                h: search_r.h - u(8.0),
            };
            hits_add(&mut z.hits, clr_r, ZS_CLEAR_SEARCH, 0);
            hits_add(
                &mut z.hits,
                Rect {
                    x: search_r.x,
                    y: search_r.y,
                    w: (clr_r.x - search_r.x).max(u(80.0)),
                    h: search_r.h,
                },
                ZS_SEARCH,
                0,
            );
        } else {
            hits_add(&mut z.hits, search_r, ZS_SEARCH, 0);
        }
    }

    let ry = rows_top(z, z.rect.y);
    let rh = u(112.0);
    let rg = u(14.0);
    let max_vis = z.per_page.clamp(1, ZS_ROWS);
    for i in 0..z.rows.len().min(max_vis) {
        hits_add(&mut z.hits, Rect { x: inn.x, y: ry + i as i32 * (rh + rg), w: inn.w, h: rh }, ZS_ROW, i as i32);
    }
    let by = z.rect.y + z.rect.h - u(142.0);
    let gap = u(12.0);
    let bw = (inn.w - 4 * gap) / 5;
    for (i, id) in [ZS_CITE, ZS_ADD, ZS_OPEN, ZS_DISCARD, ZS_REFRESH].into_iter().enumerate() {
        let on = z.selected >= 0 || id == ZS_REFRESH;
        if on {
            hits_add(&mut z.hits, Rect { x: inn.x + i as i32 * (bw + gap), y: by, w: bw, h: u(112.0) }, id, 0);
        }
    }
}

/// Rebuilds the rows from the bridge and the filter, keeping the selected row when it is still there.
/// A pending event is listed once whether it came from startup's memory or a file still in inbox/.
pub fn zotero_sheet_refresh(z: &mut ZoteroSheet) -> bool {
    let before: Vec<String> = z.rows.iter().map(|r| r.id.clone()).collect();
    let selected_id = zotero_sheet_selected(z).map(|r| r.id.clone());
    let aw = z.aw.max(1860);
    let ah = z.ah.max(2480);
    zotero_sheet_layout(z, aw, ah);
    let per_page = z.per_page.clamp(1, ZS_ROWS);

    if !bridge_exists() {
        z.rows.clear();
        z.total = 0;
        z.page = 0;
        z.pages = 1;
        z.selected = -1;
        z.status = "Zotero bridge is not connected.".to_string();
        zotero_sheet_layout(z, aw, ah);
        return before.is_empty();
    }
    z.rows.clear();
    if z.tab == ZoteroTab::Inbox {
        let mut seen = HashSet::new();
        let mut pending: Vec<ZoteroCitation> = pending_citations();
        pending.extend(inbox_peek());
        pending.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
        let all_rows: Vec<ZoteroSheetRow> = pending
            .into_iter()
            .filter(|c| seen.insert(c.id.clone()))
            .map(|c| from_citation(&c))
            .collect();
        z.total = all_rows.len();
        z.pages = z.total.div_ceil(per_page).max(1);
        z.page = z.page.clamp(0, z.pages as i32 - 1);
        let start = (z.page as usize) * per_page;
        let end = (start + per_page).min(z.total);
        z.rows = all_rows[start..end].to_vec();
        z.status = if !z.custom_status.is_empty() {
            z.custom_status.clone()
        } else {
            format!("{} citation(s) waiting.", z.total)
        };
    } else if let Some(idx) = library_index_load() {
        z.collections = build_collections_list(&idx);
        if !z.collection.is_empty() {
            if let Some((_, label)) = z
                .collections
                .iter()
                .find(|(k, l)| k.eq_ignore_ascii_case(&z.collection) || l.eq_ignore_ascii_case(&z.collection))
            {
                z.collection_label = label.clone();
            } else {
                z.collection.clear();
                z.collection_label = "All".to_string();
            }
        } else {
            z.collection_label = "All".to_string();
        }
        let filtered = library_index_filter(&idx, &z.filter, &z.collection);
        z.total = filtered.len();
        z.pages = z.total.div_ceil(per_page).max(1);
        z.page = z.page.clamp(0, z.pages as i32 - 1);
        let start = (z.page as usize) * per_page;
        let end = (start + per_page).min(z.total);
        for item in &filtered[start..end] {
            z.rows.push(from_item(item));
        }
        z.status = if !z.custom_status.is_empty() {
            z.custom_status.clone()
        } else {
            format!("{} of {} item(s) shown.", z.rows.len(), idx.items.len())
        };
    } else {
        z.total = 0;
        z.page = 0;
        z.pages = 1;
        z.collections = vec![(String::new(), "All".to_string())];
        z.status = if !z.custom_status.is_empty() {
            z.custom_status.clone()
        } else {
            "No Zotero catalog found.".to_string()
        };
    }
    z.selected = selected_id
        .and_then(|id| z.rows.iter().position(|r| r.id == id))
        .or_else(|| (!z.rows.is_empty()).then_some(0))
        .map_or(-1, |i| i as i32);
    zotero_sheet_layout(z, aw, ah);
    before != z.rows.iter().map(|r| r.id.clone()).collect::<Vec<_>>()
}

/// The selected row, if any.
pub fn zotero_sheet_selected(z: &ZoteroSheet) -> Option<&ZoteroSheetRow> {
    if !z.open || z.selected < 0 {
        return None;
    }
    z.rows.get(z.selected as usize)
}

// A short-name alias for callers that spell the type Zs.
pub fn zs_selected(z: &ZoteroSheet) -> Option<&ZoteroSheetRow> {
    zotero_sheet_selected(z)
}

/// Draws the sheet where it meets `r`, and returns its card (plus keyboard when open) for a refresh.
pub fn zotero_sheet_draw(z: &mut ZoteroSheet, a: &mut App, r: Rect) -> Rect {
    let prev_per = z.per_page;
    zotero_sheet_layout(z, a.w, a.h);
    if z.open && z.per_page != prev_per {
        zotero_sheet_refresh(z);
    }
    if !z.open {
        return RECT_EMPTY;
    }
    let total_area = if z.kb.open { rect_union(z.rect, z.kb.area) } else { z.rect };
    if !rect_overlaps(r, total_area) {
        return total_area;
    }
    zotero_sheet_layout(z, a.w, a.h);
    let card = z.rect;
    let inn = rect_inflate(card, -u(24.0));
    let ts = ui_text_size();
    let ss = ui_small_size();
    let busy = crate::zotero_sync::zk_bg_busy() || a.zotero_import.is_some();

    let c = &mut a.screen;
    ui_frame(c, card, u(24.0), 255, true, 0);
    ui_frame(c, card, u(24.0), 0, false, u(6.0));

    let hy = card.y + u(18.0);
    let hh = u(96.0);
    let close_r = Rect { x: inn.x + inn.w - u(120.0), y: hy, w: u(120.0), h: hh };
    ui_button(c, close_r, IconKind::Close, "", ButtonStyle::Plain);

    let mut h_right = close_r.x - u(12.0);
    if crate::zotero_sync::zk_sync_available() {
        let st = if busy { ButtonStyle::Disabled } else { ButtonStyle::Plain };
        let sw = ui_button_need_w(hh, IconKind::Update, "Sync", st).max(u(188.0));
        let sr = Rect { x: h_right - sw, y: hy, w: sw, h: hh };
        ui_button(c, sr, IconKind::Update, "Sync", st);
        h_right = sr.x - u(12.0);
    }
    if zotero_sheet_selected(z).is_some_and(|row| row.has_pdf && !row.attachment_key.is_empty()) {
        let st = if busy { ButtonStyle::Disabled } else { ButtonStyle::Primary };
        let ow = ui_button_need_w(hh, IconKind::None, "Open PDF", st).max(u(236.0));
        let or = Rect { x: h_right - ow, y: hy, w: ow, h: hh };
        ui_button(c, or, IconKind::None, "Open PDF", st);
        h_right = or.x - u(12.0);
    }
    if z.current_pdf.is_some() {
        let title_min_w = font_measure(FontFace::Bold, ts, "Zotero") + u(28.0);
        let cw = ui_button_need_w(hh, IconKind::None, "Open in Zotero", ButtonStyle::Plain).max(u(270.0));
        if h_right - cw >= inn.x + title_min_w {
            let cr = Rect { x: h_right - cw, y: hy, w: cw, h: hh };
            ui_button(c, cr, IconKind::None, "Open in Zotero", ButtonStyle::Plain);
            h_right = cr.x - u(12.0);
        }
    }

    let title_w = (h_right - inn.x).max(u(200.0));
    font_draw_left_name(c, FontFace::Bold, ts, Rect { x: inn.x, y: card.y + u(18.0), w: title_w, h: u(66.0) }, "Zotero", 0);
    font_draw_left_name(c, FontFace::Regular, ss, Rect { x: inn.x, y: card.y + u(80.0), w: title_w, h: u(46.0) }, &z.status, 150);

    let ty = card.y + u(128.0);
    let tw = (inn.w - u(16.0)) / 2;
    ui_button(c, Rect { x: inn.x, y: ty, w: tw, h: u(96.0) }, IconKind::None, "Inbox", if z.tab == ZoteroTab::Inbox { ButtonStyle::Selected } else { ButtonStyle::Plain });
    ui_button(c, Rect { x: inn.x + tw + u(16.0), y: ty, w: tw, h: u(96.0) }, IconKind::None, "Library", if z.tab == ZoteroTab::Library { ButtonStyle::Selected } else { ButtonStyle::Plain });

    if z.tab == ZoteroTab::Library {
        let fy = card.y + u(234.0);
        let fh = u(68.0);
        let mut right_x = inn.x + inn.w;
        if z.pages > 1 {
            let nav_w = u(74.0);
            let can_prev = z.page > 0;
            let can_next = (z.page as usize + 1) < z.pages;
            let next_r = Rect { x: right_x - nav_w, y: fy, w: nav_w, h: fh };
            ui_button(c, next_r, IconKind::Next, "", if can_next { ButtonStyle::Plain } else { ButtonStyle::Disabled });
            let page_txt = format!("{}/{}", z.page + 1, z.pages);
            let ptw = (font_measure(FontFace::Bold, ss, &page_txt) + u(20.0)).max(u(104.0));
            let page_r = Rect { x: next_r.x - ptw, y: fy, w: ptw, h: fh };
            font_draw_centered(c, FontFace::Bold, ss, page_r, &page_txt, 0);
            let prev_r = Rect { x: page_r.x - nav_w, y: fy, w: nav_w, h: fh };
            ui_button(c, prev_r, IconKind::Prev, "", if can_prev { ButtonStyle::Plain } else { ButtonStyle::Disabled });
            right_x = prev_r.x - u(12.0);
        }
        let coll_w = (inn.w * 30 / 100).clamp(u(320.0), u(460.0));
        let coll_r = Rect { x: right_x - coll_w, y: fy, w: coll_w, h: fh };
        ui_frame(c, coll_r, u(14.0), 255, true, 0);
        ui_frame(c, coll_r, u(14.0), 0, false, if z.collection.is_empty() { u(3.0) } else { u(5.0) });
        let coll_label = if z.collection_label.is_empty() { "All" } else { z.collection_label.as_str() };
        font_draw_left_name(
            c,
            FontFace::Bold,
            ss,
            Rect { x: coll_r.x + u(16.0), y: coll_r.y, w: coll_r.w - u(32.0), h: coll_r.h },
            &format!("Collection: {coll_label}"),
            0,
        );

        let search_r = Rect {
            x: inn.x,
            y: fy,
            w: (coll_r.x - inn.x - u(12.0)).max(u(200.0)),
            h: fh,
        };
        ui_frame(c, search_r, u(14.0), 255, true, 0);
        ui_frame(c, search_r, u(14.0), 0, false, if z.kb.open { u(6.0) } else { u(3.0) });
        let mut st_right = search_r.x + search_r.w - u(14.0);
        if !z.filter.is_empty() {
            let clr_r = Rect {
                x: search_r.x + search_r.w - u(68.0),
                y: search_r.y + u(4.0),
                w: u(64.0),
                h: search_r.h - u(8.0),
            };
            ui_icon(c, IconKind::Close, rect_inflate(clr_r, -u(10.0)), 0);
            st_right = clr_r.x - u(6.0);
        }
        let search_txt = if z.filter.is_empty() {
            "Search: all items".to_string()
        } else {
            format!("Search: {}", z.filter)
        };
        font_draw_left_name(
            c,
            FontFace::Regular,
            ss,
            Rect {
                x: search_r.x + u(16.0),
                y: search_r.y,
                w: (st_right - (search_r.x + u(16.0))).max(u(60.0)),
                h: search_r.h,
            },
            &search_txt,
            if z.filter.is_empty() { 140 } else { 0 },
        );
    }

    let ry0 = rows_top(z, card.y);
    let rh = u(112.0);
    let rg = u(14.0);
    if z.rows.is_empty() {
        let msg = if !bridge_exists() {
            "Zotero bridge is not connected."
        } else if z.tab == ZoteroTab::Inbox {
            "No Zotero citations are waiting."
        } else {
            "No Zotero items match."
        };
        font_draw_left_name(c, FontFace::Regular, ts, Rect { x: inn.x + u(14.0), y: ry0, w: inn.w - u(28.0), h: rh }, msg, 150);
    }
    let max_vis = z.per_page.clamp(1, ZS_ROWS);
    for i in 0..z.rows.len().min(max_vis) {
        let r = Rect { x: inn.x, y: ry0 + i as i32 * (rh + rg), w: inn.w, h: rh };
        let selected = i as i32 == z.selected;
        ui_frame(c, r, u(16.0), 255, true, 0);
        ui_frame(c, r, u(16.0), 0, false, if selected { u(8.0) } else { u(3.0) });

        let mut text_w = r.w - u(36.0);
        if !z.rows[i].tag.is_empty() {
            let tag_tw = font_measure(FontFace::Bold, ss, &z.rows[i].tag);
            let tag_w = (tag_tw + u(30.0)).min(r.w / 3);
            let tag_h = u(46.0);
            let tag_r = Rect {
                x: r.x + r.w - tag_w - u(16.0),
                y: r.y + (r.h - tag_h) / 2,
                w: tag_w,
                h: tag_h,
            };
            if z.rows[i].pdf_cached {
                ui_frame(c, tag_r, tag_h / 2, 0, true, 0);
                font_draw_centered(c, FontFace::Bold, ss, tag_r, &z.rows[i].tag, 255);
            } else {
                ui_frame(c, tag_r, tag_h / 2, 255, true, 0);
                ui_frame(c, tag_r, tag_h / 2, 0, false, u(3.0));
                font_draw_centered(c, FontFace::Bold, ss, tag_r, &z.rows[i].tag, 0);
            }
            text_w = (tag_r.x - (r.x + u(18.0)) - u(14.0)).max(u(80.0));
        }

        let text = Rect { x: r.x + u(18.0), y: r.y, w: text_w, h: r.h / 2 };
        let sub = Rect { x: text.x, y: r.y + r.h / 2, w: text_w, h: r.h / 2 };
        font_draw_left_name(c, FontFace::Bold, ss, text, &z.rows[i].label, 0);
        font_draw_left_name(c, FontFace::Regular, ss, sub, &z.rows[i].sub, 150);
    }

    let by = card.y + card.h - u(142.0);
    let gap = u(12.0);
    let bw = (inn.w - 4 * gap) / 5;
    for (i, (id, label)) in [
        (ZS_CITE, "Cite"),
        (ZS_ADD, "Add"),
        (ZS_OPEN, "Open in Zotero"),
        (ZS_DISCARD, "Discard"),
        (ZS_REFRESH, "Refresh"),
    ]
    .into_iter()
    .enumerate()
    {
        let r = Rect { x: inn.x + i as i32 * (bw + gap), y: by, w: bw, h: u(112.0) };
        let on = z.selected >= 0 || id == ZS_REFRESH;
        ui_button(c, r, IconKind::None, label, if on { ButtonStyle::Plain } else { ButtonStyle::Disabled });
    }

    if z.kb.open {
        let screen = Rect { x: 0, y: 0, w: a.w, h: a.h };
        keyboard_draw(&mut z.kb, &mut a.screen, screen);
        a.toast_floor = z.kb.area.y;
        rect_union(card, z.kb.area)
    } else {
        a.toast_floor = card.y;
        card
    }
}

fn cycle_sheet_collection(z: &mut ZoteroSheet) {
    if z.collections.len() <= 1 {
        z.collection.clear();
        z.collection_label = "All".to_string();
        return;
    }
    let idx = z
        .collections
        .iter()
        .position(|(k, l)| {
            if z.collection.is_empty() {
                k.is_empty()
            } else {
                k.eq_ignore_ascii_case(&z.collection) || l.eq_ignore_ascii_case(&z.collection)
            }
        })
        .unwrap_or(0);
    let next = (idx + 1) % z.collections.len();
    z.collection = z.collections[next].0.clone();
    z.collection_label = z.collections[next].1.clone();
    z.page = 0;
    z.custom_status.clear();
    zotero_sheet_refresh(z);
}

fn trigger_sheet_sync(z: &mut ZoteroSheet, a: &mut App) {
    if crate::zotero_sync::zk_bg_busy() {
        return;
    }
    if crate::zotero_sync::zk_bg_start_refresh(true) {
        z.custom_status = "Syncing Zotero library...".to_string();
        z.status = z.custom_status.clone();
        app_notice(a, NoticeKey::Zotero, "Syncing Zotero library...");
    }
}

/// Tick the sheet's embedded search keyboard.
pub fn zotero_sheet_tick(z: &mut ZoteroSheet, a: &mut App, now: u32) -> bool {
    if !z.open || !z.kb.open {
        return false;
    }
    let mut kb = std::mem::take(&mut z.kb);
    let r = app_keyboard_tick(a, &mut kb, now);
    let new_text = kb.text.as_str().to_string();
    z.kb = kb;
    if r == KbResult::Changed && z.filter != new_text {
        z.filter = new_text;
        z.page = 0;
        z.custom_status.clear();
        zotero_sheet_refresh(z);
        return true;
    }
    false
}

/// A pen or finger on the sheet (phase 0 down, 1 move, 2 up).
pub fn zotero_sheet_pointer(z: &mut ZoteroSheet, a: &mut App, ph: i32, ev: &InEvent, dirty: &mut Rect) -> ZsResult {
    *dirty = RECT_EMPTY;
    if !z.open {
        return ZsResult::None;
    }

    if z.kb.open
        && !z.has_pressed
        && (z.kb.pressed_key >= 0
            || z.kb.field_drag
            || a.kb_inject.is_some()
            || rect_contains(z.kb.area, ev.x as i32, ev.y as i32))
    {
        let mut kb = std::mem::take(&mut z.kb);
        let (r, key_d) = app_keyboard_pointer_split(a, &mut kb, ph, ev);
        let new_text = kb.text.as_str().to_string();
        z.kb = kb;
        match r {
            KbResult::Changed => {
                if z.filter != new_text {
                    z.filter = new_text;
                    z.page = 0;
                    z.custom_status.clear();
                    zotero_sheet_refresh(z);
                    return ZsResult::Changed;
                }
                app_invalidate(a, key_d, RefreshMode::Fast);
                return ZsResult::None;
            }
            KbResult::Done => {
                z.filter = new_text;
                z.page = 0;
                z.custom_status.clear();
                keyboard_close(&mut z.kb);
                zotero_sheet_refresh(z);
                return ZsResult::Changed;
            }
            KbResult::Cancel => {
                z.filter = z.filter_before_kb.clone();
                z.page = 0;
                z.custom_status.clear();
                keyboard_close(&mut z.kb);
                zotero_sheet_refresh(z);
                return ZsResult::Changed;
            }
            _ => {
                app_invalidate(a, key_d, RefreshMode::Fast);
                return ZsResult::None;
            }
        }
    }

    if ph == 0 {
        press_begin(&mut z.press, ev.x, ev.y, ev.t_ms);
        let h = hits_find(&z.hits, ev.x, ev.y).copied();
        z.has_pressed = h.is_some();
        if let Some(h) = h {
            z.pressed = h;
            app_press_feedback(a, h.r);
        }
        return ZsResult::None;
    }
    if !z.has_pressed {
        return ZsResult::None;
    }
    press_move(&mut z.press, ev.x, ev.y);
    if ph == 1 {
        return ZsResult::None;
    }
    z.has_pressed = false;
    let h = z.pressed;
    canvas_invert(&mut a.screen, h.r);
    *dirty = h.r;
    if touch_cancelled(ev) || !press_is_tap(&z.press, h.r) {
        return ZsResult::None;
    }
    match h.id {
        ZS_CLOSE => ZsResult::Close,
        ZS_SYNC => {
            trigger_sheet_sync(z, a);
            ZsResult::Changed
        }
        ZS_OPEN_PDF => zotero_sheet_selected(z).cloned().map_or(ZsResult::None, ZsResult::OpenPdf),
        ZS_OPEN_CURRENT => {
            if z.current_pdf.is_some() {
                ZsResult::OpenCurrentInZotero
            } else {
                ZsResult::None
            }
        }
        ZS_INBOX | ZS_LIBRARY if h.id == ZS_INBOX && z.tab != ZoteroTab::Inbox => {
            keyboard_close(&mut z.kb);
            z.tab = ZoteroTab::Inbox;
            z.filter.clear();
            z.page = 0;
            z.custom_status.clear();
            zotero_sheet_refresh(z);
            ZsResult::Changed
        }
        ZS_LIBRARY => {
            z.tab = ZoteroTab::Library;
            z.page = 0;
            z.custom_status.clear();
            zotero_sheet_refresh(z);
            ZsResult::Changed
        }
        ZS_SEARCH => {
            if !z.kb.open {
                z.filter_before_kb = z.filter.clone();
                keyboard_open(&mut z.kb, "Search Zotero", z.filter.as_bytes());
                z.kb.area = keyboard_area(&z.kb, Rect { x: 0, y: 0, w: a.w, h: a.h });
                zotero_sheet_refresh(z);
                ZsResult::Changed
            } else {
                ZsResult::None
            }
        }
        ZS_CLEAR_SEARCH => {
            z.filter.clear();
            if z.kb.open {
                z.kb.text.clear();
                z.kb.caret = 0;
                z.kb.anchor = 0;
            }
            z.page = 0;
            z.custom_status.clear();
            zotero_sheet_refresh(z);
            ZsResult::Changed
        }
        ZS_COLLECTION => {
            cycle_sheet_collection(z);
            ZsResult::Changed
        }
        ZS_PREV if z.page > 0 => {
            z.page -= 1;
            zotero_sheet_refresh(z);
            ZsResult::Changed
        }
        ZS_NEXT if (z.page as usize + 1) < z.pages => {
            z.page += 1;
            zotero_sheet_refresh(z);
            ZsResult::Changed
        }
        ZS_ROW if h.arg >= 0 && (h.arg as usize) < z.rows.len() => {
            z.selected = h.arg;
            zotero_sheet_layout(z, a.w, a.h);
            ZsResult::Changed
        }
        ZS_CITE => zotero_sheet_selected(z).cloned().map_or(ZsResult::None, ZsResult::Cite),
        ZS_ADD => zotero_sheet_selected(z).cloned().map_or(ZsResult::None, ZsResult::Add),
        ZS_OPEN => zotero_sheet_selected(z).cloned().map_or(ZsResult::None, ZsResult::Open),
        ZS_DISCARD => zotero_sheet_selected(z).cloned().map_or(ZsResult::None, ZsResult::Discard),
        ZS_REFRESH => {
            z.custom_status.clear();
            if crate::zotero_sync::zk_sync_available() && !crate::zotero_sync::zk_bg_busy() {
                if crate::zotero_sync::zk_bg_start_refresh(false) {
                    z.custom_status = "Refreshing Zotero catalog...".to_string();
                    z.status = z.custom_status.clone();
                    app_notice(a, NoticeKey::Zotero, "Refreshing Zotero catalog...");
                    return ZsResult::Changed;
                }
            }
            let _ = library_index_reload();
            zotero_sheet_refresh(z);
            ZsResult::Changed
        }
        _ => ZsResult::None,
    }
}

/// A named control's centre, for the headless runner's `tap panel zotero.NAME`. Only controls that
/// are actually on the sheet resolve.
pub fn zotero_sheet_control_point(z: &ZoteroSheet, name: &str) -> Option<(f32, f32)> {
    if !z.open {
        return None;
    }
    let (id, arg) = if let Some(n) = name.strip_prefix("row.") {
        (ZS_ROW, n.parse::<i32>().ok()? - 1)
    } else {
        (
            match name {
                "close" => ZS_CLOSE,
                "inbox" => ZS_INBOX,
                "library" => ZS_LIBRARY,
                "cite" => ZS_CITE,
                "add" => ZS_ADD,
                "open" => ZS_OPEN,
                "open_pdf" => ZS_OPEN_PDF,
                "open_current" => ZS_OPEN_CURRENT,
                "discard" => ZS_DISCARD,
                "refresh" => ZS_REFRESH,
                "sync" => ZS_SYNC,
                "search" => ZS_SEARCH,
                "clear" | "clear_search" => ZS_CLEAR_SEARCH,
                "collection" => ZS_COLLECTION,
                "prev" => ZS_PREV,
                "next" => ZS_NEXT,
                _ => return None,
            },
            0,
        )
    };
    z.hits.h.iter().find(|h| h.id == id && h.arg == arg).map(|h| ((h.r.x + h.r.w / 2) as f32, (h.r.y + h.r.h / 2) as f32))
}

/// The name of what is at screen point (x, y), for the runner's `editor.ui_at`.
pub fn zotero_sheet_hit_name(z: &ZoteroSheet, x: i32, y: i32) -> String {
    if !z.open {
        return String::new();
    }
    match hits_find(&z.hits, x as f32, y as f32) {
        Some(h) => match h.id {
            ZS_CLOSE => "zotero.close".to_string(),
            ZS_INBOX => "zotero.inbox".to_string(),
            ZS_LIBRARY => "zotero.library".to_string(),
            ZS_ROW => format!("zotero.row.{}", h.arg + 1),
            ZS_CITE => "zotero.cite".to_string(),
            ZS_ADD => "zotero.add".to_string(),
            ZS_OPEN => "zotero.open".to_string(),
            ZS_OPEN_PDF => "zotero.open_pdf".to_string(),
            ZS_OPEN_CURRENT => "zotero.open_current".to_string(),
            ZS_DISCARD => "zotero.discard".to_string(),
            ZS_REFRESH => "zotero.refresh".to_string(),
            ZS_SYNC => "zotero.sync".to_string(),
            ZS_SEARCH => "zotero.search".to_string(),
            ZS_CLEAR_SEARCH => "zotero.clear".to_string(),
            ZS_COLLECTION => "zotero.collection".to_string(),
            ZS_PREV => "zotero.prev".to_string(),
            ZS_NEXT => "zotero.next".to_string(),
            _ => String::new(),
        },
        None => String::new(),
    }
}

/// Drive the sheet without a pointer: used by headless tests and by the runner's named controls.
pub fn zotero_sheet_control(z: &mut ZoteroSheet, a: &mut App, key: &str, value: &str) -> ZsResult {
    match key {
        "show" | "open_sheet" => {
            zotero_sheet_open(z, a);
            ZsResult::Changed
        }
        "close" => {
            zotero_sheet_close(z);
            ZsResult::Close
        }
        "tab" => {
            let tab = match value {
                "library" | "1" => ZoteroTab::Library,
                _ => ZoteroTab::Inbox,
            };
            if z.tab != tab {
                keyboard_close(&mut z.kb);
                z.tab = tab;
                z.page = 0;
                zotero_sheet_refresh(z);
            }
            ZsResult::Changed
        }
        "filter" => {
            z.filter = value.to_string();
            if z.kb.open {
                z.kb.text.set_bytes(z.filter.as_bytes());
                z.kb.caret = z.kb.text.len();
                z.kb.anchor = z.kb.caret;
            }
            z.page = 0;
            z.custom_status.clear();
            zotero_sheet_refresh(z);
            ZsResult::Changed
        }
        "collection" => {
            let v = value.trim();
            if v.is_empty() || v.eq_ignore_ascii_case("all") {
                z.collection.clear();
                z.collection_label = "All".to_string();
            } else if v == "next" {
                cycle_sheet_collection(z);
                return ZsResult::Changed;
            } else {
                z.collection = v.to_string();
            }
            z.page = 0;
            z.custom_status.clear();
            zotero_sheet_refresh(z);
            ZsResult::Changed
        }
        "page" => {
            if let Ok(p) = value.parse::<i32>() {
                z.page = (p - 1).max(0);
                zotero_sheet_refresh(z);
                ZsResult::Changed
            } else {
                ZsResult::None
            }
        }
        "select" | "row" => {
            let n = value.parse::<i32>().unwrap_or(0);
            if n >= 1 && (n as usize) <= z.rows.len().min(ZS_ROWS) {
                z.selected = n - 1;
                zotero_sheet_layout(z, a.w, a.h);
                ZsResult::Changed
            } else {
                ZsResult::None
            }
        }
        "sync" => {
            trigger_sheet_sync(z, a);
            ZsResult::Changed
        }
        "refresh" => {
            z.custom_status.clear();
            let _ = library_index_reload();
            zotero_sheet_refresh(z);
            ZsResult::Changed
        }
        "cite" => zotero_sheet_selected(z).cloned().map_or(ZsResult::None, ZsResult::Cite),
        "add" => zotero_sheet_selected(z).cloned().map_or(ZsResult::None, ZsResult::Add),
        "open" => zotero_sheet_selected(z).cloned().map_or(ZsResult::None, ZsResult::Open),
        "open_pdf" => zotero_sheet_selected(z).cloned().map_or(ZsResult::None, ZsResult::OpenPdf),
        "open_current" => {
            if z.current_pdf.is_some() {
                ZsResult::OpenCurrentInZotero
            } else {
                ZsResult::None
            }
        }
        "discard" => zotero_sheet_selected(z).cloned().map_or(ZsResult::None, ZsResult::Discard),
        _ => ZsResult::None,
    }
}

/// The sheet's probe keys (asked without an `editor.zotero.sheet.` prefix).
pub fn zotero_sheet_probe(z: &ZoteroSheet, key: &str, out: &mut String, cap: usize) -> bool {
    match key {
        "open" => { let _ = csnprintf!(out, cap, "%d", z.open as i32); }
        "tab" => { let _ = csnprintf!(out, cap, "%s", if z.tab == ZoteroTab::Inbox { "inbox" } else { "library" }); }
        "rows" => { let _ = csnprintf!(out, cap, "%d", if z.open { z.rows.len().min(ZS_ROWS) as i32 } else { 0 }); }
        "total" => { let _ = csnprintf!(out, cap, "%d", if z.open { z.total as i32 } else { 0 }); }
        "page" => { let _ = csnprintf!(out, cap, "%d", if z.open { z.page + 1 } else { 0 }); }
        "pages" => { let _ = csnprintf!(out, cap, "%d", if z.open { z.pages.max(1) as i32 } else { 0 }); }
        "selected" => { let _ = csnprintf!(out, cap, "%d", if z.open && z.selected >= 0 { z.selected + 1 } else { 0 }); }
        "filter" => { let _ = csnprintf!(out, cap, "%s", &z.filter); }
        "collection" => {
            let _ = csnprintf!(
                out,
                cap,
                "%s",
                if z.collection.is_empty() { "All" } else { z.collection_label.as_str() }
            );
        }
        "status" => { let _ = csnprintf!(out, cap, "%s", &z.status); }
        "rect" if z.open => { let _ = csnprintf!(out, cap, "%d %d %d %d", z.rect.x, z.rect.y, z.rect.w, z.rect.h); }
        "rect" => { let _ = csnprintf!(out, cap, "%s", ""); }
        _ => {
            let n = key.strip_prefix("row.").or_else(|| key.strip_prefix("label."));
            if let Some(n) = n {
                let r = n.parse::<usize>().ok().and_then(|n| n.checked_sub(1)).and_then(|n| z.rows.get(n)).filter(|_| z.open);
                let _ = csnprintf!(out, cap, "%s", r.map_or("", |r| r.label.as_str()));
            } else if let Some(n) = key.strip_prefix("id.") {
                let r = n.parse::<usize>().ok().and_then(|n| n.checked_sub(1)).and_then(|n| z.rows.get(n)).filter(|_| z.open);
                let _ = csnprintf!(out, cap, "%s", r.map_or("", |r| r.id.as_str()));
            } else if let Some(n) = key.strip_prefix("tag.").or_else(|| key.strip_prefix("badge.")) {
                let r = n.parse::<usize>().ok().and_then(|n| n.checked_sub(1)).and_then(|n| z.rows.get(n)).filter(|_| z.open);
                let _ = csnprintf!(out, cap, "%s", r.map_or("", |r| r.tag.as_str()));
            } else if key.bytes().all(|b| b.is_ascii_digit()) && !key.is_empty() {
                let r = key.parse::<usize>().ok().and_then(|n| n.checked_sub(1)).and_then(|n| z.rows.get(n)).filter(|_| z.open);
                let _ = csnprintf!(out, cap, "%s", r.map_or("", |r| r.label.as_str()));
            } else {
                return false;
            }
        }
    }
    true
}

//! The Zotero sheet: an editor-hosted sheet with two tabs, Inbox and Library. Inbox lists the citation
//! events Zotero sent while Grimoire was closed (and any that arrived since); Library searches the
//! catalog snapshot `library_index.json`. A row can be cited onto the open page, or opened in Zotero in
//! a handoff. Like Find and the Links bar, the sheet draws itself, takes the taps and says what was
//! asked (`ZsResult`); the editor does it. Its functions never reach back into the editor's state.
//!
//! With no bridge directory the sheet is inert: it has no rows and offers no action. Nothing here
//! follows a link, reads a credential, or opens a socket; all catalog and inbox bytes come from
//! `zotero_bridge.rs`.

use crate::prelude::*;

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
    /// write a handoff for this row and leave for Zotero
    Open(ZoteroSheetRow),
    /// discard a pending citation (a catalog row has nothing to discard)
    Discard(ZoteroSheetRow),
}

/// The sheet's whole state.
#[derive(Clone, Debug, Default)]
pub struct ZoteroSheet {
    pub open: bool,
    pub tab: ZoteroTab,
    pub filter: String,
    pub rows: Vec<ZoteroSheetRow>,
    /// the selected row, from 1; -1 with no rows
    pub selected: i32,
    pub rect: Rect,
    /// the screen this card was laid out for, so a refresh can keep its place
    pub aw: i32,
    pub ah: i32,
    pub hits: HitList,
    pub press: Press,
    pub pressed: Hit,
    pub has_pressed: bool,
    /// a short line under the title: how many citations,catalog items are shown
    pub status: String,
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

pub(crate) fn from_citation(c: &ZoteroCitation) -> ZoteroSheetRow {
    let label = if !c.short_cite.is_empty() {
        c.short_cite.clone()
    } else if !c.title.is_empty() {
        c.title.clone()
    } else {
        c.item_key.clone()
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
    }
}

/// Opens the sheet and fills it from the bridge. With no bridge it opens empty and says so.
pub fn zotero_sheet_open(z: &mut ZoteroSheet, a: &App) {
    *z = ZoteroSheet { open: true, ..Default::default() };
    zotero_sheet_layout(z, a.w, a.h);
    zotero_sheet_refresh(z);
}

/// Closes it, leaving its data for a caller that wants to inspect it.
pub fn zotero_sheet_close(z: &mut ZoteroSheet) {
    z.open = false;
    z.has_pressed = false;
}

fn rows_top(z: &ZoteroSheet, card_y: i32) -> i32 {
    card_y + if z.tab == ZoteroTab::Library { u(294.0) } else { u(244.0) }
}

/// The sheet's card on an `aw x ah` screen: centred, with at least 40 logical px around it.
pub fn zotero_sheet_layout(z: &mut ZoteroSheet, aw: i32, ah: i32) {
    z.aw = aw;
    z.ah = ah;
    let pad = u(40.0);
    let w = (aw - 2 * pad).min(u(1500.0)).max(u(360.0));
    let h = (ah - 2 * pad).min(u(1660.0)).max(u(520.0));
    z.rect = Rect { x: (aw - w) / 2, y: (ah - h) / 2, w, h };
    hits_clear(&mut z.hits);
    if !z.open {
        return;
    }
    let inn = rect_inflate(z.rect, -u(24.0));
    let close = Rect { x: inn.x + inn.w - u(120.0), y: z.rect.y + u(18.0), w: u(120.0), h: u(96.0) };
    hits_add(&mut z.hits, close, ZS_CLOSE, 0);
    let ty = z.rect.y + u(128.0);
    let tw = (inn.w - u(16.0)) / 2;
    hits_add(&mut z.hits, Rect { x: inn.x, y: ty, w: tw, h: u(96.0) }, ZS_INBOX, 0);
    hits_add(&mut z.hits, Rect { x: inn.x + tw + u(16.0), y: ty, w: tw, h: u(96.0) }, ZS_LIBRARY, 0);
    let ry = rows_top(z, z.rect.y);
    let rh = u(112.0);
    let rg = u(14.0);
    for i in 0..z.rows.len().min(ZS_ROWS) {
        hits_add(&mut z.hits, Rect { x: inn.x, y: ry + i as i32 * (rh + rg), w: inn.w, h: rh }, ZS_ROW, i as i32);
    }
    let by = z.rect.y + z.rect.h - u(150.0);
    let gap = u(12.0);
    let bw = (inn.w - 4 * gap) / 5;
    for (i, id) in [ZS_CITE, ZS_ADD, ZS_OPEN, ZS_DISCARD, ZS_REFRESH].into_iter().enumerate() {
        hits_add(&mut z.hits, Rect { x: inn.x + i as i32 * (bw + gap), y: by, w: bw, h: u(112.0) }, id, 0);
    }
}

/// Rebuilds the rows from the bridge and the filter, keeping the selected row when it is still there.
/// A pending event is listed once whether it came from startup's memory or a file still in inbox/.
pub fn zotero_sheet_refresh(z: &mut ZoteroSheet) -> bool {
    let before: Vec<String> = z.rows.iter().map(|r| r.id.clone()).collect();
    let selected_id = zotero_sheet_selected(z).map(|r| r.id.clone());
    if !bridge_exists() {
        z.rows.clear();
        z.selected = -1;
        z.status = "Zotero bridge is not connected.".to_string();
        zotero_sheet_layout(z, z.rect.w.max(1) + 2 * u(40.0), z.rect.h.max(1) + 2 * u(40.0));
        return before.is_empty();
    }
    z.rows.clear();
    if z.tab == ZoteroTab::Inbox {
        let mut seen = std::collections::HashSet::new();
        let mut pending: Vec<ZoteroCitation> = pending_citations();
        pending.extend(inbox_peek());
        pending.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
        for c in pending {
            if seen.insert(c.id.clone()) {
                z.rows.push(from_citation(&c));
            }
        }
        z.status = format!("{} citation(s) waiting.", z.rows.len());
    } else if let Some(idx) = library_index_load() {
        for item in library_index_search(&idx, &z.filter).into_iter().take(ZS_ROWS) {
            z.rows.push(from_item(&item));
        }
        z.status = format!("{} of {} item(s) shown.", z.rows.len(), idx.items.len());
    } else {
        z.status = "No Zotero catalog found.".to_string();
    }
    z.selected = selected_id
        .and_then(|id| z.rows.iter().position(|r| r.id == id))
        .or_else(|| (!z.rows.is_empty()).then_some(0))
        .map_or(-1, |i| i as i32);
    zotero_sheet_layout(z, z.aw.max(1), z.ah.max(1));
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

/// Draws the sheet where it meets `r`, and returns its card for a refresh.
pub fn zotero_sheet_draw(z: &mut ZoteroSheet, a: &mut App, r: Rect) -> Rect {
    zotero_sheet_layout(z, a.w, a.h);
    if !z.open || !rect_overlaps(r, z.rect) {
        return if z.open { z.rect } else { RECT_EMPTY };
    }
    hits_clear(&mut z.hits);
    zotero_sheet_layout(z, a.w, a.h);
    let card = z.rect;
    let inn = rect_inflate(card, -u(24.0));
    let c = &mut a.screen;
    ui_frame(c, card, u(24.0), 255, true, 0);
    ui_frame(c, card, u(24.0), 0, false, u(6.0));
    let ts = ui_text_size();
    font_draw_left(c, FontFace::Bold, ts, Rect { x: inn.x, y: card.y + u(18.0), w: inn.w - u(160.0), h: u(96.0) }, "Zotero", 0);
    font_draw_left_name(c, FontFace::Regular, ui_small_size(), Rect { x: inn.x, y: card.y + u(84.0), w: inn.w - u(160.0), h: u(46.0) }, &z.status, 150);
    ui_button(c, Rect { x: inn.x + inn.w - u(120.0), y: card.y + u(18.0), w: u(120.0), h: u(96.0) }, IconKind::Close, "", ButtonStyle::Plain);
    hits_add(&mut z.hits, Rect { x: inn.x + inn.w - u(120.0), y: card.y + u(18.0), w: u(120.0), h: u(96.0) }, ZS_CLOSE, 0);
    let ty = card.y + u(128.0);
    let tw = (inn.w - u(16.0)) / 2;
    ui_button(c, Rect { x: inn.x, y: ty, w: tw, h: u(96.0) }, IconKind::None, "Inbox", if z.tab == ZoteroTab::Inbox { ButtonStyle::Selected } else { ButtonStyle::Plain });
    ui_button(c, Rect { x: inn.x + tw + u(16.0), y: ty, w: tw, h: u(96.0) }, IconKind::None, "Library", if z.tab == ZoteroTab::Library { ButtonStyle::Selected } else { ButtonStyle::Plain });
    hits_add(&mut z.hits, Rect { x: inn.x, y: ty, w: tw, h: u(96.0) }, ZS_INBOX, 0);
    hits_add(&mut z.hits, Rect { x: inn.x + tw + u(16.0), y: ty, w: tw, h: u(96.0) }, ZS_LIBRARY, 0);
    if z.tab == ZoteroTab::Library {
        font_draw_left_name(c, FontFace::Regular, ui_small_size(), Rect { x: inn.x, y: card.y + u(232.0), w: inn.w, h: u(52.0) }, &format!("Search: {}", z.filter), 150);
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
    for i in 0..z.rows.len().min(ZS_ROWS) {
        let r = Rect { x: inn.x, y: ry0 + i as i32 * (rh + rg), w: inn.w, h: rh };
        let selected = i as i32 == z.selected;
        ui_frame(c, r, u(16.0), 255, true, 0);
        ui_frame(c, r, u(16.0), 0, false, if selected { u(8.0) } else { u(3.0) });
        let text = Rect { x: r.x + u(18.0), y: r.y, w: r.w - u(36.0), h: r.h / 2 };
        let sub = Rect { x: text.x, y: r.y + r.h / 2, w: text.w, h: r.h / 2 };
        font_draw_left_name(c, FontFace::Bold, ui_small_size(), text, &z.rows[i].label, 0);
        font_draw_left_name(c, FontFace::Regular, ui_small_size(), sub, &z.rows[i].sub, 150);
        hits_add(&mut z.hits, r, ZS_ROW, i as i32);
    }
    let by = card.y + card.h - u(150.0);
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
        if on {
            hits_add(&mut z.hits, r, id, 0);
        }
    }
    a.toast_floor = card.y;
    card
}

/// A pen or finger on the sheet (phase 0 down, 1 move, 2 up).
pub fn zotero_sheet_pointer(z: &mut ZoteroSheet, a: &mut App, ph: i32, ev: &InEvent, dirty: &mut Rect) -> ZsResult {
    *dirty = RECT_EMPTY;
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
        ZS_INBOX | ZS_LIBRARY if h.id == ZS_INBOX && z.tab != ZoteroTab::Inbox => {
            z.tab = ZoteroTab::Inbox;
            z.filter.clear();
            zotero_sheet_refresh(z);
            ZsResult::Changed
        }
        ZS_LIBRARY => {
            z.tab = ZoteroTab::Library;
            zotero_sheet_refresh(z);
            ZsResult::Changed
        }
        ZS_ROW if h.arg >= 0 && (h.arg as usize) < z.rows.len() => {
            z.selected = h.arg;
            ZsResult::Changed
        }
        ZS_CITE => zotero_sheet_selected(z).cloned().map_or(ZsResult::None, ZsResult::Cite),
        ZS_ADD => zotero_sheet_selected(z).cloned().map_or(ZsResult::None, ZsResult::Add),
        ZS_OPEN => zotero_sheet_selected(z).cloned().map_or(ZsResult::None, ZsResult::Open),
        ZS_DISCARD => zotero_sheet_selected(z).cloned().map_or(ZsResult::None, ZsResult::Discard),
        ZS_REFRESH => {
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
                "discard" => ZS_DISCARD,
                "refresh" => ZS_REFRESH,
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
            ZS_DISCARD => "zotero.discard".to_string(),
            ZS_REFRESH => "zotero.refresh".to_string(),
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
                z.tab = tab;
                zotero_sheet_refresh(z);
            }
            ZsResult::Changed
        }
        "filter" => {
            z.filter = value.to_string();
            zotero_sheet_refresh(z);
            ZsResult::Changed
        }
        "select" | "row" => {
            let n = value.parse::<i32>().unwrap_or(0);
            if n >= 1 && (n as usize) <= z.rows.len().min(ZS_ROWS) {
                z.selected = n - 1;
                ZsResult::Changed
            } else {
                ZsResult::None
            }
        }
        "refresh" => {
            zotero_sheet_refresh(z);
            ZsResult::Changed
        }
        "cite" => zotero_sheet_selected(z).cloned().map_or(ZsResult::None, ZsResult::Cite),
        "add" => zotero_sheet_selected(z).cloned().map_or(ZsResult::None, ZsResult::Add),
        "open" => zotero_sheet_selected(z).cloned().map_or(ZsResult::None, ZsResult::Open),
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
        "selected" => { let _ = csnprintf!(out, cap, "%d", if z.open { z.selected + 1 } else { 0 }); }
        "filter" => { let _ = csnprintf!(out, cap, "%s", &z.filter); }
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

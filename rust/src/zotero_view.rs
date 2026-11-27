//! Built-in Zotero Library Browser modal (`Modal::Zotero` on the Library screen).
//!
//! Renders the local `library_index.json` catalog snapshot with live search, collection filtering,
//! pagination, attachment/cache/notebook badges, background `zk-sync` refresh/pull controls, and
//! direct PDF import / Zotero reader handoff.
//!
//! Security & e-ink invariants:
//! - Zero-Credential Invariant: never reads credentials or raw env vars; any background status text
//!    comes from `zotero_sync` where secrets are scrubbed.
//! - Zero-Cut Typography: dynamic titles, authors, collections, and status lines use `*_name`
//!   ellipsizing helpers; every fixed button/badge is sized via `ui_button_need_w` / `font_measure`.

use crate::prelude::*;
use std::collections::HashSet;

// Hit IDs for ZoteroView controls.
const ZV_CLOSE: i32 = 1;
const ZV_SYNC: i32 = 2;
const ZV_SEARCH: i32 = 3;
const ZV_CLEAR_SEARCH: i32 = 4;
const ZV_COLLECTION: i32 = 5;
const ZV_ROW: i32 = 6;
const ZV_PREV: i32 = 7;
const ZV_NEXT: i32 = 8;
const ZV_OPEN: i32 = 9;
const ZV_OPEN_ZOTERO: i32 = 10;
const ZV_REFRESH: i32 = 11;

#[derive(Clone, Debug, Default)]
pub struct ZoteroViewRow {
    pub item: ZoteroLibraryItem,
    pub title: String,
    pub sub: String,
    pub badge: String,
    pub badge_ready: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum ZvResult {
    #[default]
    None,
    Close,
    Changed,
    Open(ZoteroLibraryItem),
    OpenInZotero(ZoteroLibraryItem),
}

#[derive(Clone, Debug, Default)]
pub struct ZoteroView {
    pub open: bool,
    pub filter: String,
    pub filter_before_kb: String,
    /// Empty string means "All"; otherwise either collection `key` or collection `name`.
    pub collection: String,
    /// Human-readable label for the active collection ("All" when `collection` is empty).
    pub collection_label: String,
    /// Available `(filter_value, display_label)` options, starting with `("", "All")`.
    pub collections: Vec<(String, String)>,
    /// 0-based page index.
    pub page: i32,
    /// Total page count (>= 1).
    pub pages: usize,
    /// Rows fitted per page in the current layout (>= 1).
    pub per_page: usize,
    /// Total filtered item count across all pages.
    pub total: usize,
    /// Visible rows on the current page.
    pub rows: Vec<ZoteroViewRow>,
    /// 0-based selected row index within `rows`, or `-1` when `rows` is empty.
    pub selected: i32,
    pub status: String,
    /// Custom status override (e.g. while syncing/fetching or displaying an error).
    pub custom_status: String,
    pub rect: Rect,
    pub aw: i32,
    pub ah: i32,
    pub hits: HitList,
    pub press: Press,
    pub pressed: Hit,
    pub has_pressed: bool,
    pub kb: Keyboard,
}

fn u(v: f32) -> i32 {
    ui_px(v)
}

fn collect_bound_attachment_keys() -> HashSet<String> {
    let mut set = HashSet::new();
    let mut notebooks = Vec::new();
    if store_list(&mut notebooks) == 0 {
        for nb in &notebooks {
            if let Some(src) = crate::zotero_sync::zotero_source_load(nb.id.as_str()) {
                if !src.attachment_key.is_empty() {
                    set.insert(src.attachment_key);
                }
            }
        }
    }
    set
}

fn build_collections_list(idx: &ZoteroLibraryIndex) -> Vec<(String, String)> {
    let mut out = vec![(String::new(), "All".to_string())];
    let mut seen_labels = HashSet::new();
    seen_labels.insert("all".to_string());

    for c in &idx.collections {
        let key = c.key.trim();
        let name = c.name.trim();
        let label = if !name.is_empty() { name } else { key };
        if label.is_empty() {
            continue;
        }
        let val = if !key.is_empty() { key.to_string() } else { label.to_string() };
        if seen_labels.insert(label.to_lowercase()) {
            out.push((val, label.to_string()));
        }
    }
    for item in &idx.items {
        for name in &item.collections {
            let trimmed = name.trim();
            if !trimmed.is_empty() && seen_labels.insert(trimmed.to_lowercase()) {
                out.push((trimmed.to_string(), trimmed.to_string()));
            }
        }
    }
    out
}

fn format_item_subtitle(item: &ZoteroLibraryItem) -> String {
    let mut parts: Vec<&str> = Vec::with_capacity(4);
    if !item.authors.trim().is_empty() {
        parts.push(item.authors.trim());
    }
    if !item.year.trim().is_empty() {
        parts.push(item.year.trim());
    }
    if let Some(col) = item.collections.iter().map(|s| s.trim()).find(|s| !s.is_empty()) {
        parts.push(col);
    } else if !item.citation_key.trim().is_empty() {
        parts.push(item.citation_key.trim());
    }
    if parts.is_empty() {
        if !item.item_type.trim().is_empty() {
            item.item_type.trim().to_string()
        } else {
            item.item_key.clone()
        }
    } else {
        parts.join(" · ")
    }
}

fn make_row(item: &ZoteroLibraryItem, bound_keys: &HashSet<String>) -> ZoteroViewRow {
    let title = if !item.title.trim().is_empty() {
        item.title.trim().to_string()
    } else if !item.citation_key.trim().is_empty() {
        item.citation_key.trim().to_string()
    } else {
        item.item_key.clone()
    };
    let bound = !item.attachment_key.is_empty() && bound_keys.contains(&item.attachment_key);
    let local_ok = item.pdf_cached || crate::zotero_sync::zotero_pdf_on_disk(item);
    let (badge, badge_ready) = if item.has_pdf && !item.attachment_key.is_empty() {
        if bound {
            ("PDF · Notes".to_string(), true)
        } else if local_ok {
            ("PDF · Ready".to_string(), true)
        } else {
            ("PDF · Cloud".to_string(), false)
        }
    } else if item.has_epub {
        ("EPUB".to_string(), false)
    } else if item.has_snapshot {
        ("Snapshot".to_string(), false)
    } else {
        ("Item".to_string(), false)
    };
    ZoteroViewRow {
        item: item.clone(),
        title,
        sub: format_item_subtitle(item),
        badge,
        badge_ready,
    }
}

fn compute_card_and_per_page(zv: &mut ZoteroView, aw: i32, ah: i32) -> (Rect, i32, i32, i32, i32) {
    zv.aw = aw;
    zv.ah = ah;
    let screen = Rect { x: 0, y: 0, w: aw, h: ah };
    let pad = u(40.0);
    let cw = (aw - 2 * pad).min(u(1720.0)).max(u(600.0));
    let cx = (aw - cw) / 2;

    let (cy, ch) = if zv.kb.open {
        let kb_r = keyboard_area(&zv.kb, screen);
        zv.kb.area = kb_r;
        let top_pad = u(20.0);
        let gap = u(16.0);
        let avail = (kb_r.y - top_pad - gap).max(u(500.0));
        (top_pad, avail.min(u(1460.0)))
    } else {
        let h = (ah - 2 * pad).min(u(2280.0)).max(u(640.0));
        ((ah - h) / 2, h)
    };

    let card = Rect { x: cx, y: cy, w: cw, h: ch };
    zv.rect = card;

    let fy = card.y + u(122.0);
    let fh = u(88.0);
    let by = card.y + card.h - u(124.0);
    let ry0 = fy + fh + u(16.0);
    let avail_rows_h = (by - u(16.0) - ry0).max(u(116.0));
    let rg = u(14.0);
    let min_rh = u(128.0);
    let per_page = ((avail_rows_h + rg) / (min_rh + rg)).clamp(1, 9) as usize;
    let rh = ((avail_rows_h - (per_page as i32 - 1) * rg) / per_page as i32).clamp(u(114.0), u(168.0));
    zv.per_page = per_page;

    (card, fy, ry0, rh, by)
}

/// Open the Zotero Library modal and populate its catalog rows.
/// Automatically triggers a background local catalog export when no `library_index.json` exists yet
/// and `zk-sync` is configured.
pub fn zotero_view_open(zv: &mut ZoteroView, a: &mut App) {
    *zv = ZoteroView {
        open: true,
        collection_label: "All".to_string(),
        collections: vec![(String::new(), "All".to_string())],
        pages: 1,
        per_page: 8,
        selected: -1,
        ..Default::default()
    };
    compute_card_and_per_page(zv, a.w, a.h);
    if library_index_load().is_none()
        && crate::zotero_sync::zk_sync_available()
        && !crate::zotero_sync::zk_bg_busy()
    {
        if crate::zotero_sync::zk_bg_start_refresh(false) {
            zv.custom_status = "Refreshing Zotero catalog...".to_string();
            app_notice(a, NoticeKey::Zotero, "Refreshing Zotero catalog...");
        }
    }
    zotero_view_refresh(zv);
}

/// Close the Zotero Library modal cleanly, dismissing its search keyboard if open.
pub fn zotero_view_close(zv: &mut ZoteroView) {
    keyboard_close(&mut zv.kb);
    zv.open = false;
    zv.has_pressed = false;
}

/// Selected row on the current page, if any.
pub fn zotero_view_selected(zv: &ZoteroView) -> Option<&ZoteroViewRow> {
    if !zv.open || zv.selected < 0 {
        return None;
    }
    zv.rows.get(zv.selected as usize)
}

/// Recompute filtered rows, pagination, and status from the cached/disk `library_index.json`.
pub fn zotero_view_refresh(zv: &mut ZoteroView) {
    let aw = zv.aw.max(1860);
    let ah = zv.ah.max(2480);
    compute_card_and_per_page(zv, aw, ah);
    let prev_key = zotero_view_selected(zv).map(|r| r.item.item_key.clone());

    if !bridge_exists() {
        zv.rows.clear();
        zv.total = 0;
        zv.page = 0;
        zv.pages = 1;
        zv.selected = -1;
        zv.collections = vec![(String::new(), "All".to_string())];
        zv.collection.clear();
        zv.collection_label = "All".to_string();
        zv.status = "Zotero bridge is not connected.".to_string();
        return;
    }

    let Some(idx) = library_index_load() else {
        zv.rows.clear();
        zv.total = 0;
        zv.page = 0;
        zv.pages = 1;
        zv.selected = -1;
        zv.collections = vec![(String::new(), "All".to_string())];
        if !zv.custom_status.is_empty() {
            zv.status = zv.custom_status.clone();
        } else if let Some(bg) = crate::zotero_sync::zk_bg_status_text() {
            zv.status = bg;
        } else if crate::zotero_sync::zk_sync_available() {
            zv.status = "No Zotero catalog yet — tap Sync or Refresh.".to_string();
        } else {
            zv.status = "No Zotero catalog found.".to_string();
        }
        return;
    };

    zv.collections = build_collections_list(&idx);
    if !zv.collection.is_empty() {
        if let Some((_, label)) = zv
            .collections
            .iter()
            .find(|(k, l)| k.eq_ignore_ascii_case(&zv.collection) || l.eq_ignore_ascii_case(&zv.collection))
        {
            zv.collection_label = label.clone();
        } else {
            zv.collection.clear();
            zv.collection_label = "All".to_string();
        }
    } else {
        zv.collection_label = "All".to_string();
    }

    let filtered = library_index_filter(&idx, &zv.filter, &zv.collection);
    zv.total = filtered.len();
    let per_page = zv.per_page.max(1);
    zv.pages = zv.total.div_ceil(per_page).max(1);
    zv.page = zv.page.clamp(0, zv.pages as i32 - 1);

    let start = (zv.page as usize) * per_page;
    let end = (start + per_page).min(zv.total);
    let bound_keys = collect_bound_attachment_keys();
    zv.rows = filtered[start..end]
        .iter()
        .map(|it| make_row(it, &bound_keys))
        .collect();

    zv.selected = prev_key
        .and_then(|k| zv.rows.iter().position(|r| r.item.item_key == k))
        .or_else(|| (!zv.rows.is_empty()).then_some(0))
        .map_or(-1, |i| i as i32);

    if !zv.custom_status.is_empty() {
        zv.status = zv.custom_status.clone();
    } else if let Some(bg) = crate::zotero_sync::zk_bg_status_text() {
        zv.status = bg;
    } else if !zv.filter.trim().is_empty() || !zv.collection.is_empty() {
        zv.status = format!(
            "{} of {} items match.",
            zv.total,
            idx.items.len()
        );
    } else {
        let n = idx.items.len();
        zv.status = format!("{n} item{} in Zotero catalog.", if n == 1 { "" } else { "s" });
    }
}

/// Set a transient status line (scrubbed by caller) and keep rows as they are.
pub fn zotero_view_set_status(zv: &mut ZoteroView, status: &str) {
    let clean = crate::zotero_sync::scrub_error_message(status);
    zv.custom_status = clean.clone();
    zv.status = clean;
}

/// Draw the Zotero Library modal (and its on-screen keyboard when open) into `a.screen`.
pub fn zotero_view_draw(zv: &mut ZoteroView, a: &mut App) -> Rect {
    if !zv.open {
        return RECT_EMPTY;
    }
    let old_per_page = zv.per_page;
    let (card, fy, ry0, rh, by) = compute_card_and_per_page(zv, a.w, a.h);
    if zv.per_page != old_per_page {
        zotero_view_refresh(zv);
    }
    hits_clear(&mut zv.hits);

    let inn = rect_inflate(card, -u(24.0));
    let ts = ui_text_size();
    let ss = ui_small_size();
    let busy = crate::zotero_sync::zk_bg_busy() || a.zotero_import.is_some();
    let has_zk = crate::zotero_sync::zk_sync_available();

    let c = &mut a.screen;
    ui_frame(c, card, u(24.0), 255, true, 0);
    ui_frame(c, card, u(24.0), 0, false, u(6.0));

    // ---- Header ----
    let hh = u(92.0);
    let hy = card.y + u(18.0);
    let close_w = u(104.0);
    let close_r = Rect {
        x: inn.x + inn.w - close_w,
        y: hy,
        w: close_w,
        h: hh,
    };
    ui_button(c, close_r, IconKind::Close, "", ButtonStyle::Plain);
    hits_add(&mut zv.hits, close_r, ZV_CLOSE, 0);

    let mut header_right_x = close_r.x - u(14.0);
    if has_zk {
        let sync_st = if busy { ButtonStyle::Disabled } else { ButtonStyle::Plain };
        let sync_w = ui_button_need_w(hh, IconKind::None, "Sync", sync_st).max(u(196.0));
        let sync_r = Rect {
            x: header_right_x - sync_w,
            y: hy,
            w: sync_w,
            h: hh,
        };
        ui_button(c, sync_r, IconKind::None, "Sync", sync_st);
        if !busy {
            hits_add(&mut zv.hits, sync_r, ZV_SYNC, 0);
        }
        header_right_x = sync_r.x - u(16.0);
    }

    let title_w = (header_right_x - inn.x).max(u(200.0));
    font_draw_left(
        c,
        FontFace::Bold,
        ts,
        Rect {
            x: inn.x,
            y: card.y + u(14.0),
            w: title_w,
            h: u(56.0),
        },
        "Zotero Library",
        0,
    );
    let sub_line = crate::zotero_sync::zk_bg_status_text()
        .filter(|_| zv.custom_status.is_empty())
        .unwrap_or_else(|| zv.status.clone());
    font_draw_left_name(
        c,
        FontFace::Regular,
        ss,
        Rect {
            x: inn.x,
            y: card.y + u(70.0),
            w: title_w,
            h: u(44.0),
        },
        &sub_line,
        130,
    );

    // ---- Search + Collection Filter Bar ----
    let fh = u(88.0);
    let coll_w = (inn.w * 36 / 100).clamp(u(400.0), u(580.0));
    let coll_r = Rect {
        x: inn.x + inn.w - coll_w,
        y: fy,
        w: coll_w,
        h: fh,
    };
    ui_frame(c, coll_r, u(16.0), 255, true, 0);
    ui_frame(
        c,
        coll_r,
        u(16.0),
        0,
        false,
        if zv.collection.is_empty() { u(3.0) } else { u(5.0) },
    );
    let coll_text = format!("Collection: {}", zv.collection_label);
    // "All" is fixed UI copy (a cut is a layout bug); a collection's own name may be shortened.
    let coll_draw = if zv.collection.is_empty() { font_draw_left } else { font_draw_left_name };
    coll_draw(
        c,
        FontFace::Bold,
        ss,
        Rect {
            x: coll_r.x + u(18.0),
            y: coll_r.y,
            w: coll_r.w - u(36.0),
            h: coll_r.h,
        },
        &coll_text,
        0,
    );
    hits_add(&mut zv.hits, coll_r, ZV_COLLECTION, 0);

    let search_r = Rect {
        x: inn.x,
        y: fy,
        w: (coll_r.x - inn.x - u(16.0)).max(u(240.0)),
        h: fh,
    };
    ui_frame(c, search_r, u(16.0), 255, true, 0);
    ui_frame(
        c,
        search_r,
        u(16.0),
        0,
        false,
        if zv.kb.open { u(6.0) } else { u(3.0) },
    );
    let icon_sz = u(46.0);
    ui_icon(
        c,
        IconKind::Search,
        Rect {
            x: search_r.x + u(16.0),
            y: search_r.y + (search_r.h - icon_sz) / 2,
            w: icon_sz,
            h: icon_sz,
        },
        if zv.filter.is_empty() { 130 } else { 0 },
    );

    let mut search_text_right = search_r.x + search_r.w - u(16.0);
    if !zv.filter.is_empty() {
        let clr_r = Rect {
            x: search_r.x + search_r.w - u(82.0),
            y: search_r.y + u(8.0),
            w: u(74.0),
            h: search_r.h - u(16.0),
        };
        ui_icon(c, IconKind::Close, rect_inflate(clr_r, -u(12.0)), 0);
        hits_add(&mut zv.hits, clr_r, ZV_CLEAR_SEARCH, 0);
        search_text_right = clr_r.x - u(8.0);
        // Main search hit excludes clear button so tapping × clears cleanly.
        hits_add(
            &mut zv.hits,
            Rect {
                x: search_r.x,
                y: search_r.y,
                w: (clr_r.x - search_r.x).max(u(80.0)),
                h: search_r.h,
            },
            ZV_SEARCH,
            0,
        );
    } else {
        hits_add(&mut zv.hits, search_r, ZV_SEARCH, 0);
    }
    let stx = search_r.x + u(16.0) + icon_sz + u(12.0);
    let stw = (search_text_right - stx).max(u(60.0));
    let (search_str, search_gray) = if zv.filter.is_empty() {
        ("Search title, author, year, tag...", 140)
    } else {
        (zv.filter.as_str(), 0)
    };
    let search_draw = if zv.filter.is_empty() { font_draw_left } else { font_draw_left_name };
    search_draw(
        c,
        FontFace::Regular,
        ss,
        Rect {
            x: stx,
            y: search_r.y,
            w: stw,
            h: search_r.h,
        },
        search_str,
        search_gray,
    );

    // ---- Paginated Item Rows ----
    let rg = u(14.0);
    if zv.rows.is_empty() {
        let empty_msg = if !bridge_exists() {
            "Zotero bridge is not connected."
        } else if library_index_load().is_none() {
            if busy {
                "Loading Zotero catalog..."
            } else {
                "No Zotero catalog loaded yet. Tap Sync or Refresh to load your library."
            }
        } else {
            "No Zotero items match your search or collection filter."
        };
        font_draw_left(
            c,
            FontFace::Regular,
            ts,
            Rect {
                x: inn.x + u(14.0),
                y: ry0 + u(16.0),
                w: inn.w - u(28.0),
                h: u(80.0),
            },
            empty_msg,
            130,
        );
    } else {
        let title_px = if rh >= u(136.0) { ts } else { ss };
        for (i, row) in zv.rows.iter().enumerate() {
            let r = Rect {
                x: inn.x,
                y: ry0 + (i as i32) * (rh + rg),
                w: inn.w,
                h: rh,
            };
            let selected = i as i32 == zv.selected;
            ui_frame(c, r, u(16.0), 255, true, 0);
            ui_frame(c, r, u(16.0), 0, false, if selected { u(8.0) } else { u(3.0) });

            let badge_tw = font_measure(FontFace::Bold, ss, &row.badge);
            let badge_w = (badge_tw + u(36.0)).min(r.w / 3);
            let badge_h = u(52.0).min(r.h - u(24.0));
            let badge_r = Rect {
                x: r.x + r.w - badge_w - u(18.0),
                y: r.y + (r.h - badge_h) / 2,
                w: badge_w,
                h: badge_h,
            };
            if row.badge_ready {
                ui_frame(c, badge_r, badge_h / 2, 0, true, 0);
                font_draw_centered(c, FontFace::Bold, ss, badge_r, &row.badge, 255);
            } else {
                ui_frame(c, badge_r, badge_h / 2, 255, true, 0);
                ui_frame(c, badge_r, badge_h / 2, 0, false, u(3.0));
                font_draw_centered(c, FontFace::Bold, ss, badge_r, &row.badge, 0);
            }

            let text_x = r.x + u(20.0);
            let text_w = (badge_r.x - text_x - u(18.0)).max(u(80.0));
            let top_h = r.h * 54 / 100;
            let bot_h = r.h - top_h;
            font_draw_left_name(
                c,
                FontFace::Bold,
                title_px,
                Rect {
                    x: text_x,
                    y: r.y + u(4.0),
                    w: text_w,
                    h: top_h,
                },
                &row.title,
                0,
            );
            font_draw_left_name(
                c,
                FontFace::Regular,
                ss,
                Rect {
                    x: text_x,
                    y: r.y + top_h - u(4.0),
                    w: text_w,
                    h: bot_h,
                },
                &row.sub,
                130,
            );
            hits_add(&mut zv.hits, r, ZV_ROW, i as i32);
        }
    }

    // ---- Footer Bar ----
    let bh = u(100.0);
    let gap = u(12.0);
    let can_prev = zv.page > 0;
    let can_next = (zv.page as usize + 1) < zv.pages;
    let prev_st = if can_prev { ButtonStyle::Plain } else { ButtonStyle::Disabled };
    let next_st = if can_next { ButtonStyle::Plain } else { ButtonStyle::Disabled };

    let prev_w = ui_button_need_w(bh, IconKind::None, "Prev", prev_st).max(u(144.0));
    let prev_r = Rect { x: inn.x, y: by, w: prev_w, h: bh };
    ui_button(c, prev_r, IconKind::None, "Prev", prev_st);
    if can_prev {
        hits_add(&mut zv.hits, prev_r, ZV_PREV, 0);
    }

    let page_txt = format!("{} / {}", zv.page + 1, zv.pages.max(1));
    let page_tw = font_measure(FontFace::Bold, ss, &page_txt);
    let page_w = (page_tw + u(32.0)).max(u(156.0));
    let page_r = Rect {
        x: prev_r.x + prev_r.w + u(8.0),
        y: by,
        w: page_w,
        h: bh,
    };
    font_draw_centered(c, FontFace::Bold, ss, page_r, &page_txt, 0);

    let next_w = ui_button_need_w(bh, IconKind::None, "Next", next_st).max(u(144.0));
    let next_r = Rect {
        x: page_r.x + page_r.w + u(8.0),
        y: by,
        w: next_w,
        h: bh,
    };
    ui_button(c, next_r, IconKind::None, "Next", next_st);
    if can_next {
        hits_add(&mut zv.hits, next_r, ZV_NEXT, 0);
    }

    // Right-aligned action buttons: [Open PDF / Open in Zotero] [In Zotero (when PDF)] [Refresh]
    let sel_row = zotero_view_selected(zv).cloned();
    let sel_has_pdf = sel_row
        .as_ref()
        .is_some_and(|r| r.item.has_pdf && !r.item.attachment_key.is_empty());
    let open_label = if sel_row.is_none() || sel_has_pdf {
        "Open PDF"
    } else {
        "Open in Zotero"
    };
    let open_on = sel_row.is_some() && !busy;
    let open_st = if open_on {
        ButtonStyle::Primary
    } else {
        ButtonStyle::Disabled
    };
    let ref_st = if busy {
        ButtonStyle::Disabled
    } else {
        ButtonStyle::Plain
    };

    let ref_w = ui_button_need_w(bh, IconKind::None, "Refresh", ref_st).max(u(196.0));
    let ref_r = Rect {
        x: inn.x + inn.w - ref_w,
        y: by,
        w: ref_w,
        h: bh,
    };
    ui_button(c, ref_r, IconKind::None, "Refresh", ref_st);
    if !busy {
        hits_add(&mut zv.hits, ref_r, ZV_REFRESH, 0);
    }

    let zot_on = sel_row.is_some() && !busy;
    let zot_st = if zot_on {
        ButtonStyle::Plain
    } else {
        ButtonStyle::Disabled
    };
    let zot_w = ui_button_need_w(bh, IconKind::None, "In Zotero", zot_st).max(u(216.0));
    let zot_r = Rect {
        x: ref_r.x - gap - zot_w,
        y: by,
        w: zot_w,
        h: bh,
    };
    ui_button(c, zot_r, IconKind::None, "In Zotero", zot_st);
    if zot_on {
        hits_add(&mut zv.hits, zot_r, ZV_OPEN_ZOTERO, 0);
    }
    let right_edge = zot_r.x - gap;

    let open_w = ui_button_need_w(bh, IconKind::None, open_label, open_st).max(u(256.0));
    let open_r = Rect {
        x: (right_edge - open_w).max(next_r.x + next_r.w + gap),
        y: by,
        w: open_w,
        h: bh,
    };
    ui_button(c, open_r, IconKind::None, open_label, open_st);
    if open_on {
        hits_add(&mut zv.hits, open_r, ZV_OPEN, 0);
    }

    let mut total_rect = card;
    if zv.kb.open {
        let screen = Rect { x: 0, y: 0, w: a.w, h: a.h };
        keyboard_draw(&mut zv.kb, &mut a.screen, screen);
        total_rect = rect_union(card, zv.kb.area);
        a.toast_floor = zv.kb.area.y;
    } else {
        a.toast_floor = card.y;
    }
    total_rect
}

fn cycle_collection(zv: &mut ZoteroView) {
    if zv.collections.len() <= 1 {
        zv.collection.clear();
        zv.collection_label = "All".to_string();
        return;
    }
    let cur_idx = zv
        .collections
        .iter()
        .position(|(k, l)| {
            if zv.collection.is_empty() {
                k.is_empty()
            } else {
                k.eq_ignore_ascii_case(&zv.collection) || l.eq_ignore_ascii_case(&zv.collection)
            }
        })
        .unwrap_or(0);
    let next_idx = (cur_idx + 1) % zv.collections.len();
    zv.collection = zv.collections[next_idx].0.clone();
    zv.collection_label = zv.collections[next_idx].1.clone();
    zv.page = 0;
    zv.custom_status.clear();
    zotero_view_refresh(zv);
}

fn trigger_sync(zv: &mut ZoteroView, a: &mut App) {
    if crate::zotero_sync::zk_bg_busy() {
        return;
    }
    if crate::zotero_sync::zk_bg_start_refresh(true) {
        zv.custom_status = "Syncing Zotero library...".to_string();
        zv.status = zv.custom_status.clone();
        app_notice(a, NoticeKey::Zotero, "Syncing Zotero library...");
    }
}

fn trigger_refresh(zv: &mut ZoteroView, a: &mut App) {
    if crate::zotero_sync::zk_bg_busy() {
        return;
    }
    zv.custom_status.clear();
    if crate::zotero_sync::zk_sync_available() {
        if crate::zotero_sync::zk_bg_start_refresh(false) {
            zv.custom_status = "Refreshing Zotero catalog...".to_string();
            zv.status = zv.custom_status.clone();
            app_notice(a, NoticeKey::Zotero, "Refreshing Zotero catalog...");
        }
    } else {
        let _ = library_index_reload();
        zotero_view_refresh(zv);
    }
}

/// Handle pointer events (card controls + embedded search keyboard).
pub fn zotero_view_pointer(
    zv: &mut ZoteroView,
    a: &mut App,
    ph: i32,
    ev: &InEvent,
    dirty: &mut Rect,
) -> ZvResult {
    *dirty = RECT_EMPTY;
    if !zv.open {
        return ZvResult::None;
    }

    // Route contacts on (or owned by) the search keyboard when open.
    if zv.kb.open && (zv.kb.pressed_key >= 0 || zv.kb.field_drag || a.kb_inject.is_some() || rect_contains(zv.kb.area, ev.x as i32, ev.y as i32)) && !zv.has_pressed {
        let mut kb = std::mem::take(&mut zv.kb);
        let (r, key_d) = app_keyboard_pointer_split(a, &mut kb, ph, ev);
        let new_text = kb.text.as_str().to_string();
        zv.kb = kb;
        match r {
            KbResult::Changed => {
                if zv.filter != new_text {
                    zv.filter = new_text;
                    zv.page = 0;
                    zv.custom_status.clear();
                    zotero_view_refresh(zv);
                    // Redraw only the card above the keyboard so the key's Fast refresh stays separate.
                    zv.kb.open = false;
                    let card_r = zotero_view_draw(zv, a);
                    zv.kb.open = true;
                    a.toast_floor = zv.kb.area.y;
                    app_invalidate(a, card_r, RefreshMode::Ui);
                    app_flush(a);
                }
                app_invalidate(a, key_d, RefreshMode::Fast);
                return ZvResult::None;
            }
            KbResult::Done => {
                zv.filter = new_text;
                zv.page = 0;
                zv.custom_status.clear();
                keyboard_close(&mut zv.kb);
                zotero_view_refresh(zv);
                return ZvResult::Changed;
            }
            KbResult::Cancel => {
                zv.filter = zv.filter_before_kb.clone();
                zv.page = 0;
                zv.custom_status.clear();
                keyboard_close(&mut zv.kb);
                zotero_view_refresh(zv);
                return ZvResult::Changed;
            }
            _ => {
                app_invalidate(a, key_d, RefreshMode::Fast);
                return ZvResult::None;
            }
        }
    }

    if ph == 0 {
        press_begin(&mut zv.press, ev.x, ev.y, ev.t_ms);
        let h = hits_find(&zv.hits, ev.x, ev.y).copied();
        zv.has_pressed = h.is_some();
        if let Some(h) = h {
            zv.pressed = h;
            app_press_feedback(a, h.r);
        } else if !rect_contains(zv.rect, ev.x as i32, ev.y as i32)
            && !(zv.kb.open && rect_contains(zv.kb.area, ev.x as i32, ev.y as i32))
        {
            // Tap outside dismisses keyboard first if open, else closes modal on release.
            zv.pressed = Hit { id: -1, arg: 0, r: RECT_EMPTY };
            zv.has_pressed = true;
        }
        return ZvResult::None;
    }

    if !zv.has_pressed {
        return ZvResult::None;
    }
    press_move(&mut zv.press, ev.x, ev.y);
    if ph == 1 {
        return ZvResult::None;
    }
    zv.has_pressed = false;
    let h = zv.pressed;
    if h.id == -1 {
        if !touch_cancelled(ev) && !zv.press.moved {
            if zv.kb.open {
                keyboard_close(&mut zv.kb);
                zotero_view_refresh(zv);
                return ZvResult::Changed;
            }
            return ZvResult::Close;
        }
        return ZvResult::None;
    }

    canvas_invert(&mut a.screen, h.r);
    *dirty = h.r;
    if touch_cancelled(ev) || !press_is_tap(&zv.press, h.r) {
        return ZvResult::None;
    }

    match h.id {
        ZV_CLOSE => ZvResult::Close,
        ZV_SYNC => {
            trigger_sync(zv, a);
            ZvResult::Changed
        }
        ZV_REFRESH => {
            trigger_refresh(zv, a);
            ZvResult::Changed
        }
        ZV_SEARCH => {
            if !zv.kb.open {
                zv.filter_before_kb = zv.filter.clone();
                keyboard_open(&mut zv.kb, "Search Zotero", zv.filter.as_bytes());
                zv.kb.area = keyboard_area(&zv.kb, Rect { x: 0, y: 0, w: a.w, h: a.h });
                zotero_view_refresh(zv);
                ZvResult::Changed
            } else {
                ZvResult::None
            }
        }
        ZV_CLEAR_SEARCH => {
            zv.filter.clear();
            if zv.kb.open {
                zv.kb.text.clear();
                zv.kb.caret = 0;
                zv.kb.anchor = 0;
            }
            zv.page = 0;
            zv.custom_status.clear();
            zotero_view_refresh(zv);
            ZvResult::Changed
        }
        ZV_COLLECTION => {
            cycle_collection(zv);
            ZvResult::Changed
        }
        ZV_PREV if zv.page > 0 => {
            zv.page -= 1;
            zotero_view_refresh(zv);
            ZvResult::Changed
        }
        ZV_NEXT if (zv.page as usize + 1) < zv.pages => {
            zv.page += 1;
            zotero_view_refresh(zv);
            ZvResult::Changed
        }
        ZV_ROW if h.arg >= 0 && (h.arg as usize) < zv.rows.len() => {
            if zv.selected == h.arg {
                let item = zv.rows[h.arg as usize].item.clone();
                ZvResult::Open(item)
            } else {
                zv.selected = h.arg;
                ZvResult::Changed
            }
        }
        ZV_OPEN => zotero_view_selected(zv)
            .map(|r| ZvResult::Open(r.item.clone()))
            .unwrap_or(ZvResult::None),
        ZV_OPEN_ZOTERO => zotero_view_selected(zv)
            .map(|r| ZvResult::OpenInZotero(r.item.clone()))
            .unwrap_or(ZvResult::None),
        _ => ZvResult::None,
    }
}

/// Tick the embedded search keyboard (held backspace repeats + pause grayscale settle).
pub fn zotero_view_tick(zv: &mut ZoteroView, a: &mut App, now: u32) -> bool {
    if !zv.open || !zv.kb.open {
        return false;
    }
    let mut kb = std::mem::take(&mut zv.kb);
    let r = app_keyboard_tick(a, &mut kb, now);
    let new_text = kb.text.as_str().to_string();
    zv.kb = kb;
    if r == KbResult::Changed && zv.filter != new_text {
        zv.filter = new_text;
        zv.page = 0;
        zv.custom_status.clear();
        zotero_view_refresh(zv);
        return true;
    }
    false
}

/// Resolve a named control's center point (`zotero.NAME`, passed without the `"zotero."` prefix).
pub fn zotero_view_control_point(zv: &ZoteroView, name: &str) -> Option<(f32, f32)> {
    if !zv.open {
        return None;
    }
    let (id, arg) = if let Some(n) = name.strip_prefix("row.") {
        (ZV_ROW, n.parse::<i32>().ok()? - 1)
    } else {
        (
            match name {
                "close" => ZV_CLOSE,
                "sync" => ZV_SYNC,
                "refresh" => ZV_REFRESH,
                "open" => ZV_OPEN,
                "open_zotero" => ZV_OPEN_ZOTERO,
                "prev" => ZV_PREV,
                "next" => ZV_NEXT,
                "search" => ZV_SEARCH,
                "clear" | "clear_search" => ZV_CLEAR_SEARCH,
                "collection" => ZV_COLLECTION,
                _ => return None,
            },
            0,
        )
    };
    zv.hits
        .h
        .iter()
        .find(|h| h.id == id && h.arg == arg)
        .map(|h| ((h.r.x + h.r.w / 2) as f32, (h.r.y + h.r.h / 2) as f32))
}

/// Hit name at `(x, y)` for `library.ui_at`.
pub fn zotero_view_hit_name(zv: &ZoteroView, x: i32, y: i32) -> String {
    if !zv.open {
        return String::new();
    }
    match hits_find(&zv.hits, x as f32, y as f32) {
        Some(h) => match h.id {
            ZV_CLOSE => "zotero.close".to_string(),
            ZV_SYNC => "zotero.sync".to_string(),
            ZV_REFRESH => "zotero.refresh".to_string(),
            ZV_OPEN => "zotero.open".to_string(),
            ZV_OPEN_ZOTERO => "zotero.open_zotero".to_string(),
            ZV_PREV => "zotero.prev".to_string(),
            ZV_NEXT => "zotero.next".to_string(),
            ZV_SEARCH => "zotero.search".to_string(),
            ZV_CLEAR_SEARCH => "zotero.clear".to_string(),
            ZV_COLLECTION => "zotero.collection".to_string(),
            ZV_ROW => format!("zotero.row.{}", h.arg + 1),
            _ => String::new(),
        },
        None => String::new(),
    }
}

/// Programmatic control entry point for tests / headless helpers.
pub fn zotero_view_control(
    zv: &mut ZoteroView,
    a: &mut App,
    key: &str,
    value: &str,
) -> ZvResult {
    match key {
        "show" => {
            zotero_view_open(zv, a);
            ZvResult::Changed
        }
        "close" => {
            zotero_view_close(zv);
            ZvResult::Close
        }
        "filter" => {
            zv.filter = value.to_string();
            if zv.kb.open {
                zv.kb.text.set_bytes(zv.filter.as_bytes());
                zv.kb.caret = zv.kb.text.len();
                zv.kb.anchor = zv.kb.caret;
            }
            zv.page = 0;
            zv.custom_status.clear();
            zotero_view_refresh(zv);
            ZvResult::Changed
        }
        "collection" => {
            let v = value.trim();
            if v.is_empty() || v.eq_ignore_ascii_case("all") {
                zv.collection.clear();
                zv.collection_label = "All".to_string();
            } else if v == "next" {
                cycle_collection(zv);
                return ZvResult::Changed;
            } else {
                zv.collection = v.to_string();
            }
            zv.page = 0;
            zv.custom_status.clear();
            zotero_view_refresh(zv);
            ZvResult::Changed
        }
        "page" => {
            if let Ok(p) = value.parse::<i32>() {
                zv.page = (p - 1).max(0);
                zotero_view_refresh(zv);
                ZvResult::Changed
            } else {
                ZvResult::None
            }
        }
        "select" | "row" => {
            let n = value.parse::<i32>().unwrap_or(0);
            if n >= 1 && (n as usize) <= zv.rows.len() {
                zv.selected = n - 1;
                ZvResult::Changed
            } else {
                ZvResult::None
            }
        }
        "sync" => {
            trigger_sync(zv, a);
            ZvResult::Changed
        }
        "refresh" => {
            trigger_refresh(zv, a);
            ZvResult::Changed
        }
        "open" => zotero_view_selected(zv)
            .map(|r| ZvResult::Open(r.item.clone()))
            .unwrap_or(ZvResult::None),
        "open_zotero" => zotero_view_selected(zv)
            .map(|r| ZvResult::OpenInZotero(r.item.clone()))
            .unwrap_or(ZvResult::None),
        _ => ZvResult::None,
    }
}

/// Probe `library.zotero.*` keys (passed without `"zotero."` prefix).
pub fn zotero_view_probe(zv: &ZoteroView, key: &str, out: &mut String, cap: usize) -> bool {
    match key {
        "open" => {
            let _ = csnprintf!(out, cap, "%d", zv.open as i32);
        }
        "rows" => {
            let _ = csnprintf!(out, cap, "%d", if zv.open { zv.rows.len() as i32 } else { 0 });
        }
        "total" => {
            let _ = csnprintf!(out, cap, "%d", if zv.open { zv.total as i32 } else { 0 });
        }
        "page" => {
            let _ = csnprintf!(out, cap, "%d", if zv.open { zv.page + 1 } else { 0 });
        }
        "pages" => {
            let _ = csnprintf!(out, cap, "%d", if zv.open { zv.pages.max(1) as i32 } else { 0 });
        }
        "selected" => {
            let _ = csnprintf!(
                out,
                cap,
                "%d",
                if zv.open && zv.selected >= 0 { zv.selected + 1 } else { 0 }
            );
        }
        "filter" => {
            let _ = csnprintf!(out, cap, "%s", &zv.filter);
        }
        "collection" => {
            let _ = csnprintf!(
                out,
                cap,
                "%s",
                if zv.collection.is_empty() {
                    "All"
                } else {
                    zv.collection_label.as_str()
                }
            );
        }
        "collection_key" => {
            let _ = csnprintf!(out, cap, "%s", &zv.collection);
        }
        "status" => {
            let _ = csnprintf!(out, cap, "%s", &zv.status);
        }
        "keyboard" | "kb" => {
            let _ = csnprintf!(out, cap, "%d", (zv.open && zv.kb.open) as i32);
        }
        "rect" if zv.open => {
            let _ = csnprintf!(out, cap, "%d %d %d %d", zv.rect.x, zv.rect.y, zv.rect.w, zv.rect.h);
        }
        "rect" => {
            let _ = csnprintf!(out, cap, "%s", "");
        }
        _ => {
            if let Some(n) = key.strip_prefix("row.") {
                let r = n
                    .parse::<usize>()
                    .ok()
                    .and_then(|i| i.checked_sub(1))
                    .and_then(|i| zv.rows.get(i))
                    .filter(|_| zv.open);
                let _ = csnprintf!(out, cap, "%s", r.map_or("", |r| r.title.as_str()));
            } else if let Some(n) = key.strip_prefix("sub.") {
                let r = n
                    .parse::<usize>()
                    .ok()
                    .and_then(|i| i.checked_sub(1))
                    .and_then(|i| zv.rows.get(i))
                    .filter(|_| zv.open);
                let _ = csnprintf!(out, cap, "%s", r.map_or("", |r| r.sub.as_str()));
            } else if let Some(n) = key.strip_prefix("badge.") {
                let r = n
                    .parse::<usize>()
                    .ok()
                    .and_then(|i| i.checked_sub(1))
                    .and_then(|i| zv.rows.get(i))
                    .filter(|_| zv.open);
                let _ = csnprintf!(out, cap, "%s", r.map_or("", |r| r.badge.as_str()));
            } else if let Some(n) = key.strip_prefix("key.") {
                let r = n
                    .parse::<usize>()
                    .ok()
                    .and_then(|i| i.checked_sub(1))
                    .and_then(|i| zv.rows.get(i))
                    .filter(|_| zv.open);
                let _ = csnprintf!(out, cap, "%s", r.map_or("", |r| r.item.item_key.as_str()));
            } else if let Some(n) = key.strip_prefix("att.") {
                let r = n
                    .parse::<usize>()
                    .ok()
                    .and_then(|i| i.checked_sub(1))
                    .and_then(|i| zv.rows.get(i))
                    .filter(|_| zv.open);
                let _ = csnprintf!(out, cap, "%s", r.map_or("", |r| r.item.attachment_key.as_str()));
            } else {
                return false;
            }
        }
    }
    true
}

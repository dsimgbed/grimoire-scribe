//! Track E: the editor's Zotero UI. One test function: the store root, GZ_BRIDGE_DIR, the pending
//! inbox and the font overflow hook are process-wide, like the rest of the app's file-backed state.

mod testlib;
use grimoire::prelude::*;
use std::path::PathBuf;
use std::sync::Mutex;
use testlib::*;

const W: i32 = 1860;
const H: i32 = 2480;
const DPI: f32 = 300.0;

static CUTS: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn fit_hook(text: &str, _need: i32, _have: i32) {
    CUTS.lock().unwrap().push(text.to_string());
}

fn base() -> PathBuf {
    let root = PathBuf::from(std::env::var("GRIMOIRE_TEST_TMP").unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().into_owned()))
        .join(format!("zotero-editor-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn book(name: &str, n: u32) -> NotebookInfo {
    let mut nb = NotebookInfo::default();
    assert_eq!(store_create(name, "", PAPER_BLANK, &mut nb), 0);
    nb.page_ids = (1..=n).collect();
    nb.next_page_id = n + 1;
    assert_eq!(store_save_info(&mut nb), 0);
    nb
}

fn app_with_editor() -> App {
    let mut a = App { w: W, h: H, dpi: DPI, ..Default::default() };
    assert_eq!(canvas_init(&mut a.screen, W, H), 0);
    editor_init(&mut a);
    library_init(&mut a);
    a.scr = ScreenId::Editor;
    a.running = true;
    a
}

fn citation(id: &str, at: i64, item: &str) -> ZoteroCitation {
    ZoteroCitation {
        schema_version: SCHEMA_VERSION,
        id: id.to_string(),
        created_at: at,
        item_key: item.to_string(),
        attachment_key: "EFGH5678".to_string(),
        short_cite: "Kuhn 1962".to_string(),
        title: "The Structure of Scientific Revolutions".to_string(),
        authors: "Kuhn".to_string(),
        year: "1962".to_string(),
        page: 42,
        locator: "p. 42".to_string(),
        quote: "Normal science...".to_string(),
        note: String::new(),
    }
}

fn library_index(bridge: &str) {
    let item = |key: &str, att: &str, cite: &str, title: &str, authors: &str, year: &str| {
        json_members(&[
            json_field("item_key", key),
            json_field("attachment_key", att),
            json_field("citation_key", cite),
            json_field("title", title),
            json_field("authors", authors),
            json_field("year", year),
            json_field("item_type", "book"),
            json_array_field("collections", &[]),
            json_array_field("tags", &[]),
            json_bool_field("has_pdf", true),
            json_bool_field("has_epub", false),
            json_bool_field("has_snapshot", false),
        ])
    };
    let text = json_members(&[
        json_int_field("schema_version", SCHEMA_VERSION as i64),
        json_int_field("exported_at", 1732350000),
        json_int_field("item_count", 2),
        format!("\"items\":[{}]", [
            item("ABCD1234", "EFGH5678", "Kuhn 1962", "The Structure of Scientific Revolutions", "Kuhn", "1962"),
            item("WXYZ0001", "", "Smith 2020", "A Study", "Smith", "2020"),
        ].join(",")),
    ]);
    let path = bridge_path_in(bridge, LIBRARY_INDEX_FILE).unwrap();
    assert_eq!(write_file_atomic(&path, text.as_bytes()), 0);
}

fn probe(a: &mut App, key: &str) -> String {
    let mut out = String::new();
    assert_eq!(editor_probe(a, key, &mut out, 512), 0, "editor probe {key}");
    out
}

fn tap(a: &mut App, name: &str, t: u32) -> bool {
    let Some((x, y)) = editor_panel_point(a, name) else { return false; };
    editor_event(a, &InEvent { type_: InType::TouchDown, x, y, t_ms: t, ..Default::default() });
    editor_event(a, &InEvent { type_: InType::TouchUp, x, y, t_ms: t + 80, ..Default::default() });
    true
}

fn tap_at(a: &mut App, x: f32, y: f32, t: u32) {
    editor_event(a, &InEvent { type_: InType::TouchDown, x, y, t_ms: t, ..Default::default() });
    editor_event(a, &InEvent { type_: InType::TouchUp, x, y, t_ms: t + 80, ..Default::default() });
}

fn no_bridge_is_inert(root: &PathBuf) {
    let absent = root.join("absent-bridge").to_string_lossy().into_owned();
    std::env::set_var(BRIDGE_DIR_ENV, &absent);
    check!(!bridge_exists());

    let mut a = app_with_editor();
    check!(probe(&mut a, "zotero.show") == "0");
    check!(editor_panel_point(&mut a, "bar.zotero").is_none());
    check!(tap(&mut a, "bar.zotero", 100) == false);
    check!(probe(&mut a, "zotero.sheet") == "0");
    check!(probe(&mut a, "zotero.links") == "0");
}

fn sheet_and_badges(root: &PathBuf, bridge: &str, store: &str, nb: &NotebookInfo) {
    std::env::set_var(BRIDGE_DIR_ENV, bridge);
    std::fs::create_dir_all(format!("{bridge}/inbox")).unwrap();
    check!(bridge_exists());
    library_index(bridge);
    assert_eq!(citation_write_in(bridge, &citation("1732350011-c1", 1732350011, "ABCD1234")), 0);
    assert_eq!(citation_write_in(bridge, &citation("1732350012-c2", 1732350012, "WXYZ0001")), 0);

    let mut a = app_with_editor();
    editor_open_notebook(&mut a, nb.id.as_str());
    check!(probe(&mut a, "notebook_id") == nb.id.as_str());
    assert_eq!(bridge_startup(&mut a), 0);
    check!(probe(&mut a, "zotero.pending") == "2");
    check!(probe(&mut a, "zotero.show") == "1");

    assert!(editor_shortcut(&mut a, SHORTCUT_LINKS));
    check!(probe(&mut a, "bar.zotero") == "1");
    check!(tap(&mut a, "bar.zotero", 1000));
    check!(probe(&mut a, "zotero.sheet") == "1");
    check!(probe(&mut a, "zotero.sheet.tab") == "inbox");
    check!(probe(&mut a, "zotero.sheet.rows") == "2");
    check!(probe(&mut a, "zotero.sheet.1") == "Kuhn 1962");
    check!(probe(&mut a, "zotero.sheet.selected") == "1");

    // Draw it under the fit hook: none of the new labels is cut short.
    CUTS.lock().unwrap().clear();
    check!(probe(&mut a, "screen_composed") == "1");
    let cuts = CUTS.lock().unwrap().clone();
    check!(!cuts.iter().any(|t| t.contains("Zotero") || t.contains("citation") || t.contains("Search:")));

    // Row selection and Library search are named controls too.
    check!(tap(&mut a, "zotero.library", 1200));
    check!(probe(&mut a, "zotero.sheet.tab") == "library");
    check!(probe(&mut a, "zotero.sheet.rows") == "2");
    let (x, y) = editor_panel_point(&mut a, "zotero.library").unwrap();
    check!(probe(&mut a, &format!("ui_at.{}.{}", x as i32, y as i32)) == "zotero.library");
    check!(tap(&mut a, "zotero.row.1", 1300));
    check!(probe(&mut a, "zotero.sheet.selected") == "1");

    // Open in Zotero writes a handoff with a return_to block, then leaves for Zotero.
    check!(tap(&mut a, "zotero.open", 1400));
    check!(!a.running && a.exit_code == EXIT_HANDOFF_TO_ZOTERO);
    let h = handoff_peek().unwrap();
    check!(h.target_enum() == Some(HandoffTarget::Zotero));
    check!(h.zotero.item_key == "ABCD1234" && h.zotero.attachment_key == "EFGH5678");
    check!(h.zotero.page == 0 && h.return_to.is_some());
    handoff_clear();
    a.running = true;
    a.exit_code = EXIT_CLEAN;

    // Add a catalog row: no pending citation is consumed, and a badge appears on the open page.
    check!(tap(&mut a, "zotero.add", 1500));
    check!(probe(&mut a, "zotero.links") == "1");
    check!(probe(&mut a, "zotero.badges") == "1");
    check!(probe(&mut a, "zotero.badge.1.item") == "ABCD1234");
    check!(probe(&mut a, "zotero.sheet") == "0");

    // Cite a pending event from the Inbox: it becomes a link and leaves the inbox.
    check!(probe(&mut a, "bar") == "0");
    assert!(editor_shortcut(&mut a, SHORTCUT_LINKS));
    check!(tap(&mut a, "bar.zotero", 1550));
    check!(probe(&mut a, "bar") == "0");
    check!(tap(&mut a, "zotero.inbox", 1600));
    check!(probe(&mut a, "zotero.sheet.tab") == "inbox");
    check!(probe(&mut a, "zotero.sheet.rows") == "2");
    check!(tap(&mut a, "zotero.row.1", 1700));
    check!(tap(&mut a, "zotero.cite", 1800));
    // Same item and page as the catalog badge: the citation replaces it, keeping its locator.
    check!(probe(&mut a, "zotero.links") == "1");
    check!(probe(&mut a, "zotero.pending") == "1");
    check!(probe(&mut a, "zotero.badge.1.locator") == "p. 42");

    // Discard the other pending event: its file and memory copy both go.
    assert!(editor_shortcut(&mut a, SHORTCUT_LINKS));
    check!(tap(&mut a, "bar.zotero", 1900));
    check!(tap(&mut a, "zotero.row.1", 2000));
    check!(tap(&mut a, "zotero.discard", 2100));
    check!(probe(&mut a, "zotero.pending") == "0");
    check!(inbox_count() == 0);

    // The sidecar on disk carries the two citations added to this page.
    let sidecar = root.join("store").join("notebooks").join(nb.id.as_str()).join(ZOTEROLINKS_FILE);
    let text = std::fs::read_to_string(&sidecar).unwrap();
    check!(text.starts_with(ZOTEROLINKS_HEADER));
    check!(text.contains("ABCD1234") && text.contains("EFGH5678"));
    check!(text.contains(&format!("zref.{}=", nb.page_ids[0])));

    // Close the picker so the badge on the page can be tapped (the Links bar is already closed).
    assert!(editor_shortcut(&mut a, SHORTCUT_BACK));
    check!(probe(&mut a, "zotero.sheet") == "0");
    check!(probe(&mut a, "bar") == "0");

    // A tap on a citation badge hands its item back to Zotero, with the essay's return place.
    let area = probe(&mut a, "zotero.badge.1.area");
    let nums: Vec<f32> = area.split_whitespace().filter_map(|s| s.parse().ok()).collect();
    check!(nums.len() == 4);
    if nums.len() == 4 {
        let (px, py) = grimoire::editor::editor_replay_point(&mut a, nums[0] + nums[2] / 2.0, nums[1] + nums[3] / 2.0);
        check!(editor_control_at(&a, px, py));
        let ui = probe(&mut a, &format!("ui_at.{}.{}", px as i32, py as i32));
        check!(ui == "zotero.badge");
        tap_at(&mut a, px, py, 2200);
        check!(!a.running && a.exit_code == EXIT_HANDOFF_TO_ZOTERO);
        let h = handoff_peek().unwrap();
        check!(h.zotero.item_key == "ABCD1234" && h.zotero.locator == "p. 42");
        check!(h.return_to.is_some_and(|r| r.notebook_id == nb.id.as_str() && r.page_id == nb.page_ids[0]));
        handoff_clear();
    }

    let _ = &store;
}

fn pen_event(a: &mut App, type_: InType, side: bool, x: f32, y: f32, t_ms: u32) {
    if matches!(type_, InType::PenUp | InType::PenLeave) {
        app_pen_feedback_end(a);
    }
    editor_event(a, &InEvent { type_, side_button: side, x, y, t_ms, ..Default::default() });
}

fn pen_lasso_card(a: &mut App, bx: i32, by: i32, bw: i32, bh: i32, t_start: u32) {
    let (cx, cy) = (bx as f32 + bw as f32 / 2.0, by as f32 + bh as f32 / 2.0);
    let (sx, sy) = grimoire::editor::editor_replay_point(a, cx, cy);
    let r = (bw.max(bh) as f32 / 2.0) + 60.0;
    pen_event(a, InType::PenDown, false, sx - r, sy - r, t_start);
    let ring = [
        (sx, sy - r - 5.0),
        (sx + r, sy - r),
        (sx + r + 5.0, sy),
        (sx + r, sy + r),
        (sx, sy + r + 5.0),
        (sx - r, sy + r),
        (sx - r - 5.0, sy),
        (sx - r, sy - r + 10.0),
    ];
    for (k, &(px, py)) in ring.iter().enumerate() {
        pen_event(a, InType::PenMove, false, px, py, t_start + 10 * (k as u32 + 1));
    }
    pen_event(a, InType::PenUp, false, sx - r, sy - r + 10.0, t_start + 100);
}

fn quote_card_sizing_and_overflow(root: &PathBuf, bridge: &str, store: &str, _base_nb: &NotebookInfo) {
    let qnb = book("Quote cards", 3);
    let mut a = app_with_editor();
    editor_open_notebook(&mut a, qnb.id.as_str());
    check!(probe(&mut a, "notebook_id") == qnb.id.as_str());
    check!(probe(&mut a, "zotero.badges") == "0");

    let mut c1 = citation("1732350020-q1", 1732350020, "ABCD1234");
    c1.quote = "Normal science, the activity in which most scientists inevitably spend almost all their time, is predicated on the assumption that the scientific community knows what the world is like. Much of the success of the enterprise derives from the community's willingness to defend that assumption, if necessary at considerable cost.".to_string();
    assert_eq!(citation_write_in(bridge, &c1), 0);
    assert_eq!(bridge_startup(&mut a), 0);
    check!(probe(&mut a, "zotero.pending") == "1");

    assert!(editor_shortcut(&mut a, SHORTCUT_LINKS));
    check!(tap(&mut a, "bar.zotero", 3000));
    check!(tap(&mut a, "zotero.inbox", 3050));
    check!(tap(&mut a, "zotero.row.1", 3100));
    check!(tap(&mut a, "zotero.cite", 3150));
    check!(probe(&mut a, "zotero.badges") == "1");
    check!(probe(&mut a, "zotero.badge.1.quote") == c1.quote);

    // Verify zero font overflow
    CUTS.lock().unwrap().clear();
    check!(probe(&mut a, "screen_composed") == "1");
    let cuts = CUTS.lock().unwrap().clone();
    check!(cuts.is_empty());

    // Check dimensions
    let area1 = probe(&mut a, "zotero.badge.1.area");
    let nums1: Vec<i32> = area1.split_whitespace().filter_map(|s| s.parse().ok()).collect();
    check!(nums1.len() == 4);
    let (_x1, _y1, w1, h1) = (nums1[0], nums1[1], nums1[2], nums1[3]);
    let expected_w = W * 55 / 100;
    check!((w1 - expected_w).abs() <= 5);
    let max_h = H * 38 / 100;
    check!(h1 <= max_h);

    // Add a second quote card to test vertical stacking and spacing
    let mut c2 = citation("1732350021-q2", 1732350021, "WXYZ0001");
    c2.quote = "A paradigm is what members of a scientific community share, and, conversely, a scientific community consists of men who share a paradigm.".to_string();
    assert_eq!(citation_write_in(bridge, &c2), 0);
    assert_eq!(bridge_startup(&mut a), 0);
    assert!(editor_shortcut(&mut a, SHORTCUT_LINKS));
    check!(tap(&mut a, "bar.zotero", 3200));
    check!(tap(&mut a, "zotero.inbox", 3250));
    check!(tap(&mut a, "zotero.row.1", 3300));
    check!(tap(&mut a, "zotero.cite", 3350));
    check!(probe(&mut a, "zotero.badges") == "2");

    CUTS.lock().unwrap().clear();
    check!(probe(&mut a, "screen_composed") == "1");
    let cuts2 = CUTS.lock().unwrap().clone();
    check!(cuts2.is_empty());

    let (top_badge, bottom_badge) = if probe(&mut a, "zotero.badge.1.item") == "ABCD1234" {
        ("zotero.badge.1", "zotero.badge.2")
    } else {
        ("zotero.badge.2", "zotero.badge.1")
    };
    let area_top = probe(&mut a, &format!("{top_badge}.area"));
    let nums_top: Vec<i32> = area_top.split_whitespace().filter_map(|s| s.parse().ok()).collect();
    let area_bot = probe(&mut a, &format!("{bottom_badge}.area"));
    let nums_bot: Vec<i32> = area_bot.split_whitespace().filter_map(|s| s.parse().ok()).collect();
    check!(nums_top.len() == 4 && nums_bot.len() == 4);
    let (_x1, y1, _w1, h1) = (nums_top[0], nums_top[1], nums_top[2], nums_top[3]);
    let (_x2, y2, _w2, h2) = (nums_bot[0], nums_bot[1], nums_bot[2], nums_bot[3]);
    check!(y2 >= y1 + h1);
    check!(y2 + h2 <= H);

    let _ = (root, store);
}

fn lasso_move_delete_undo(root: &PathBuf, bridge: &str, store: &str) {
    let lnb = book("Lasso move undo", 2);
    let mut a = app_with_editor();
    editor_open_notebook(&mut a, lnb.id.as_str());
    check!(probe(&mut a, "notebook_id") == lnb.id.as_str());

    let mut c = citation("1732350030-l1", 1732350030, "ABCD1234");
    c.quote = "Testing lasso move and undo on zotero card.".to_string();
    assert_eq!(citation_write_in(bridge, &c), 0);
    assert_eq!(bridge_startup(&mut a), 0);

    assert!(editor_shortcut(&mut a, SHORTCUT_LINKS));
    check!(tap(&mut a, "bar.zotero", 4000));
    check!(tap(&mut a, "zotero.inbox", 4050));
    check!(tap(&mut a, "zotero.row.1", 4100));
    check!(tap(&mut a, "zotero.cite", 4150));
    check!(probe(&mut a, "zotero.badges") == "1");

    let area_str = probe(&mut a, "zotero.badge.1.area");
    let nums: Vec<i32> = area_str.split_whitespace().filter_map(|s| s.parse().ok()).collect();
    let (bx, by, bw, bh) = (nums[0], nums[1], nums[2], nums[3]);

    // Enter select mode
    assert!(editor_shortcut(&mut a, SHORTCUT_SELECT));
    check!(probe(&mut a, "select") == "armed");

    // Lasso select the card with NO ink
    pen_lasso_card(&mut a, bx, by, bw, bh, 4200);
    check!(probe(&mut a, "select") == "selected");
    check!(probe(&mut a, "sel_zlinks") == "1");

    // Drag move by dx=100, dy=150
    let (cx, cy) = (bx as f32 + bw as f32 / 2.0, by as f32 + bh as f32 / 2.0);
    let (sx, sy) = grimoire::editor::editor_replay_point(&mut a, cx, cy);
    let (dx, dy) = (100.0, 150.0);
    pen_event(&mut a, InType::PenDown, false, sx, sy, 4400);
    pen_event(&mut a, InType::PenMove, false, sx + dx, sy + dy, 4450);
    pen_event(&mut a, InType::PenUp, false, sx + dx, sy + dy, 4500);

    // Verify moved area
    let new_area_str = probe(&mut a, "zotero.badge.1.area");
    let new_nums: Vec<i32> = new_area_str.split_whitespace().filter_map(|s| s.parse().ok()).collect();
    let (nbx, nby, nbw, nbh) = (new_nums[0], new_nums[1], new_nums[2], new_nums[3]);
    check!((nbx - (bx + 100)).abs() <= 2);
    check!((nby - (by + 150)).abs() <= 2);
    check!((nbw - bw).abs() <= 2);
    check!((nbh - bh).abs() <= 2);

    // Verify written to zoterolinks.ini on disk
    let sidecar = root.join("store").join("notebooks").join(lnb.id.as_str()).join(ZOTEROLINKS_FILE);
    let sidecar_text = std::fs::read_to_string(&sidecar).unwrap();
    check!(sidecar_text.contains(&format!("{},{},{},{}", nbx, nby, nbw, nbh)));

    // Finish select mode
    check!(tap(&mut a, "select.done", 4550));

    // Tap hit-test at new location
    let (hit_sx, hit_sy) = grimoire::editor::editor_replay_point(&mut a, (nbx + nbw / 2) as f32, (nby + nbh / 2) as f32);
    check!(probe(&mut a, &format!("ui_at.{}.{}", hit_sx as i32, hit_sy as i32)) == "zotero.badge");

    // Undo move
    assert!(editor_shortcut(&mut a, SHORTCUT_UNDO));
    let undo_area_str = probe(&mut a, "zotero.badge.1.area");
    let undo_nums: Vec<i32> = undo_area_str.split_whitespace().filter_map(|s| s.parse().ok()).collect();
    check!((undo_nums[0] - bx).abs() <= 2);
    check!((undo_nums[1] - by).abs() <= 2);
    let sidecar_undo = std::fs::read_to_string(&sidecar).unwrap();
    check!(sidecar_undo.contains(&format!("{},{},{},{}", bx, by, bw, bh)));

    // Redo move
    assert!(editor_shortcut(&mut a, SHORTCUT_REDO));
    let redo_area_str = probe(&mut a, "zotero.badge.1.area");
    let redo_nums: Vec<i32> = redo_area_str.split_whitespace().filter_map(|s| s.parse().ok()).collect();
    check!((redo_nums[0] - nbx).abs() <= 2);
    check!((redo_nums[1] - nby).abs() <= 2);

    // Lasso select card at new position and delete it
    assert!(editor_shortcut(&mut a, SHORTCUT_SELECT));
    pen_lasso_card(&mut a, nbx, nby, nbw, nbh, 4600);
    check!(probe(&mut a, "select") == "selected");
    check!(tap(&mut a, "select.delete", 4700));
    check!(probe(&mut a, "zotero.badges") == "0");
    let sidecar_del = std::fs::read_to_string(&sidecar).unwrap();
    check!(!sidecar_del.contains("ABCD1234"));

    // Undo delete
    assert!(editor_shortcut(&mut a, SHORTCUT_UNDO));
    check!(probe(&mut a, "zotero.badges") == "1");
    check!(probe(&mut a, "zotero.badge.1.item") == "ABCD1234");
    check!(probe(&mut a, "zotero.badge.1.quote") == c.quote);
    let sidecar_restored = std::fs::read_to_string(&sidecar).unwrap();
    check!(sidecar_restored.contains("ABCD1234"));

    let _ = store;
}

fn lasso_copy_paste(root: &PathBuf, bridge: &str, store: &str) {
    let cnb = book("Lasso copy paste", 2);
    let mut a = app_with_editor();
    editor_open_notebook(&mut a, cnb.id.as_str());

    let mut c = citation("1732350050-cp1", 1732350050, "ABCD1234");
    c.quote = "Copy paste quote test.".to_string();
    assert_eq!(citation_write_in(bridge, &c), 0);
    assert_eq!(bridge_startup(&mut a), 0);

    assert!(editor_shortcut(&mut a, SHORTCUT_LINKS));
    check!(tap(&mut a, "bar.zotero", 5000));
    check!(tap(&mut a, "zotero.inbox", 5050));
    check!(tap(&mut a, "zotero.row.1", 5100));
    check!(tap(&mut a, "zotero.cite", 5150));
    check!(probe(&mut a, "zotero.badges") == "1");

    let area_str = probe(&mut a, "zotero.badge.1.area");
    let nums: Vec<i32> = area_str.split_whitespace().filter_map(|s| s.parse().ok()).collect();

    // Select with lasso
    assert!(editor_shortcut(&mut a, SHORTCUT_SELECT));
    pen_lasso_card(&mut a, nums[0], nums[1], nums[2], nums[3], 5200);
    check!(probe(&mut a, "select") == "selected");

    // Copy and Paste
    check!(tap(&mut a, "select.copy", 5300));
    check!(tap(&mut a, "select.paste", 5400));
    check!(probe(&mut a, "zotero.badges") == "2");

    let seed1 = probe(&mut a, "zotero.badge.1.seed");
    let seed2 = probe(&mut a, "zotero.badge.2.seed");
    check!(seed1 != seed2);
    check!(probe(&mut a, "zotero.badge.1.item") == probe(&mut a, "zotero.badge.2.item"));
    check!(probe(&mut a, "zotero.badge.1.quote") == probe(&mut a, "zotero.badge.2.quote"));

    let sidecar = root.join("store").join("notebooks").join(cnb.id.as_str()).join(ZOTEROLINKS_FILE);
    let sidecar_paste = std::fs::read_to_string(&sidecar).unwrap();
    check!(sidecar_paste.lines().filter(|l| l.contains("ABCD1234")).count() == 2);

    // Undo paste
    assert!(editor_shortcut(&mut a, SHORTCUT_UNDO));
    check!(probe(&mut a, "zotero.badges") == "1");
    let sidecar_after_undo = std::fs::read_to_string(&sidecar).unwrap();
    check!(sidecar_after_undo.lines().filter(|l| l.contains("ABCD1234")).count() == 1);

    let _ = store;
}

fn return_essay_handoff(_root: &PathBuf, bridge: &str, _store: &str) {
    let hnb = book("Return handoff", 2);
    let mut a = app_with_editor();
    editor_open_notebook(&mut a, hnb.id.as_str());
    check!(probe(&mut a, "notebook_id") == hnb.id.as_str());

    // 1. Single pending citation auto-placed on return_essay
    let c1 = citation("1732350070-rh1", 1732350070, "ABCD1234");
    assert_eq!(citation_write_in(bridge, &c1), 0);
    check!(inbox_count() == 1);

    // Write return_essay handoff from Zotero
    assert_eq!(handoff_write_to_grimoire(bridge, hnb.id.as_str(), hnb.page_ids[0], 0), 0);

    // Startup consumes handoff and executes return_essay
    assert_eq!(bridge_startup(&mut a), 0);
    check!(probe(&mut a, "zotero.badges") == "1");
    check!(probe(&mut a, "zotero.badge.1.item") == "ABCD1234");
    check!(probe(&mut a, "zotero.sheet") == "0");
    check!(inbox_count() == 0);

    // 2. Multiple pending citations opens inbox sheet
    let c2 = citation("1732350071-rh2", 1732350071, "ABCD1234");
    let c3 = citation("1732350072-rh3", 1732350072, "WXYZ0001");
    assert_eq!(citation_write_in(bridge, &c2), 0);
    assert_eq!(citation_write_in(bridge, &c3), 0);
    check!(inbox_count() == 2);

    assert_eq!(handoff_write_to_grimoire(bridge, hnb.id.as_str(), hnb.page_ids[0], 0), 0);
    assert_eq!(bridge_startup(&mut a), 0);
    check!(probe(&mut a, "zotero.sheet") == "1");
    check!(probe(&mut a, "zotero.sheet.tab") == "inbox");
    check!(probe(&mut a, "zotero.sheet.rows") == "2");

    assert!(editor_shortcut(&mut a, SHORTCUT_BACK));
    check!(probe(&mut a, "zotero.sheet") == "0");
}

fn sidecar_lifecycle(nb: &NotebookInfo) {
    let path = format!("{}/notebooks/{}/{ZOTEROLINKS_FILE}", "store", nb.id.as_str());
    let _ = path; // the real path is checked in sheet_and_badges
    let mut links = ZoteroLinks { writable: true, ..Default::default() };
    let mut z = ZoteroRef {
        pid: nb.page_ids[0],
        seed: zotero_seed(nb.page_ids[0], "ABCD1234"),
        area: Rect::new(10, 20, 60, 60),
        item_key: "ABCD1234".to_string(),
        attachment_key: "EFGH5678".to_string(),
        page: 42,
        locator: "p. 42".to_string(),
        short_cite: "Kuhn 1962".to_string(),
        quote: String::new(),
    };
    check!(links.push(z.clone()));
    z.pid = nb.page_ids[1];
    z.seed = zotero_seed(nb.page_ids[1], "ABCD1234");
    check!(links.push(z.clone()));
    let mut shorter = nb.clone();
    shorter.page_ids.retain(|&p| p != nb.page_ids[1]);
    check!(zoterolinks_prune(&mut links, &shorter) == 1);
    check!(links.links.len() == 1);
}

#[test]
fn zotero_editor_ui() {
    let root = base();
    let bridge = root.join("bridge").to_string_lossy().into_owned();
    let store = root.join("store").to_string_lossy().into_owned();
    std::env::set_var("GRIMOIRE_SCRIPT", "/dev/null");
    let (mut dw, mut dh, mut ddpi) = (0, 0, 0.0);
    let _ = display_open(&mut dw, &mut dh, &mut ddpi);
    font_set_overflow_hook(Some(fit_hook));
    ui_init(DPI);
    assert_eq!(font_init(), 0);
    assert_eq!(store_init(&store), 0);
    store_set_device(DPI, W, H);

    let nb = book("Zotero editor", 3);

    eprintln!("- no_bridge_is_inert");
    no_bridge_is_inert(&root);
    eprintln!("- sheet_and_badges");
    sheet_and_badges(&root, &bridge, &store, &nb);
    eprintln!("- quote_card_sizing_and_overflow");
    quote_card_sizing_and_overflow(&root, &bridge, &store, &nb);
    eprintln!("- lasso_move_delete_undo");
    lasso_move_delete_undo(&root, &bridge, &store);
    eprintln!("- lasso_copy_paste");
    lasso_copy_paste(&root, &bridge, &store);
    eprintln!("- return_essay_handoff");
    return_essay_handoff(&root, &bridge, &store);
    eprintln!("- sidecar_lifecycle");
    sidecar_lifecycle(&nb);

    std::env::remove_var(BRIDGE_DIR_ENV);
    std::env::remove_var("GRIMOIRE_SCRIPT");
    display_close();
    font_set_overflow_hook(None);
    let _ = std::fs::remove_dir_all(&root);
    done();
}

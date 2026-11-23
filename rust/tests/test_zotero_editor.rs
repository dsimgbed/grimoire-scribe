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
    assert_eq!(display_open(&mut dw, &mut dh, &mut ddpi), 0);
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
    eprintln!("- sidecar_lifecycle");
    sidecar_lifecycle(&nb);

    std::env::remove_var(BRIDGE_DIR_ENV);
    std::env::remove_var("GRIMOIRE_SCRIPT");
    display_close();
    font_set_overflow_hook(None);
    let _ = std::fs::remove_dir_all(&root);
    done();
}

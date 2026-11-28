//! Phase 3 integration test suite: instant in-Grimoire Essay <-> Zotero PDF round-trip,
//! 1-tap `Cite p. N` + `<- Back` floating PDF bar, multi-hop safety, wet-ink exclusion,
//! zero-overlap / zero-overflow layout invariants across Gothic & Classic themes, and
//! optional 300-DPI headless render screenshots when `GRIMOIRE_PHASE3_SHOT_DIR` is set.

mod testlib;

use grimoire::editor::{
    editor_close_zotero_sheet, editor_page_number, editor_replay_point, editor_zotero_bar_refresh,
    editor_zotero_control,
};
use grimoire::prelude::*;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use testlib::*;

const W: i32 = 1860;
const H: i32 = 2480;
const DPI: f32 = 300.0;

static CUTS: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn overflow_hook(text: &str, need: i32, have: i32) {
    CUTS.lock().unwrap().push(format!("{text:?} needs {need} has {have}"));
}

fn scratch_root() -> PathBuf {
    let base = std::env::var("GRIMOIRE_TEST_TMP")
        .unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().into_owned());
    let root = PathBuf::from(base).join(format!("zotero-phase3-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

/// Write a sentinel `zk-sync` executable that logs any invocation to `$ZK_HOME/zk_sync_invoked.log`
/// and fails. Bound-PDF jumps and 1-tap citations must NEVER execute it.
fn write_sentinel_zk_sync(bin_path: &Path) {
    if let Some(parent) = bin_path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let script = "#!/bin/sh\n\
mkdir -p \"$ZK_HOME\"\n\
printf '%s\\n' \"$*\" >> \"$ZK_HOME/zk_sync_invoked.log\"\n\
exit 1\n";
    std::fs::write(bin_path, script.as_bytes()).unwrap();
    let mut perms = std::fs::metadata(bin_path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(bin_path, perms).unwrap();
}

fn assert_no_zk_sync(zk_home: &Path) {
    let log = zk_home.join("zk_sync_invoked.log");
    if log.exists() {
        let body = std::fs::read_to_string(&log).unwrap_or_default();
        eprintln!("  unexpected zk-sync invocation(s): {body}");
    }
    check!(!log.exists());
}

fn make_notebook(name: &str, paper: PaperKind, pages: u32) -> NotebookInfo {
    let mut nb = NotebookInfo::default();
    assert_eq!(store_create(name, "", paper, &mut nb), 0);
    nb.page_ids = (1..=pages).collect();
    nb.next_page_id = pages + 1;
    assert_eq!(store_save_info(&mut nb), 0);
    nb
}

fn make_pdf_page_geo(page_index: u32, page_label: &str) -> ZkPdfPageGeometry {
    ZkPdfPageGeometry {
        page_index,
        width_pt: 612.0,
        height_pt: 792.0,
        rotation: 0,
        page_label: page_label.to_string(),
        crop_box: [0.0, 0.0, 612.0, 792.0],
        user_to_page: [1.0, 0.0, 0.0, -1.0, 0.0, 792.0],
    }
}

fn write_library_index(bridge: &str) {
    let raw = r#"{
        "schema_version": 1,
        "exported_at": 1732401000,
        "item_count": 4,
        "collections": [
            {"key": "COLL0001", "name": "Philosophy of Science", "parent_key": ""}
        ],
        "items": [
            {
                "item_key": "ITEM_A11",
                "attachment_key": "ATT_A111",
                "citation_key": "Kuhn 1962",
                "title": "The Structure of Scientific Revolutions",
                "authors": "Thomas S. Kuhn",
                "year": "1962",
                "item_type": "book",
                "collections": ["Philosophy of Science"],
                "collection_keys": ["COLL0001"],
                "tags": ["paradigm"],
                "has_pdf": true,
                "has_epub": false,
                "has_snapshot": false,
                "pdf_cached": true,
                "pdf_local_path": "/mnt/us/zotero-bridge/storage/ATT_A111/kuhn1962.pdf",
                "last_opened": 1732400100
            },
            {
                "item_key": "ITEM_B22",
                "attachment_key": "ATT_B222",
                "citation_key": "Popper 1959",
                "title": "The Logic of Scientific Discovery",
                "authors": "Karl Popper",
                "year": "1959",
                "item_type": "book",
                "collections": ["Philosophy of Science"],
                "collection_keys": ["COLL0001"],
                "tags": ["falsification"],
                "has_pdf": true,
                "has_epub": false,
                "has_snapshot": false,
                "pdf_cached": true,
                "pdf_local_path": "/mnt/us/zotero-bridge/storage/ATT_B222/popper1959.pdf",
                "last_opened": 1732400200
            },
            {
                "item_key": "ITEM_EP1",
                "attachment_key": "ATT_EP11",
                "citation_key": "Hofstadter 1979",
                "title": "Godel, Escher, Bach",
                "authors": "Douglas R. Hofstadter",
                "year": "1979",
                "item_type": "book",
                "collections": ["Philosophy of Science"],
                "collection_keys": ["COLL0001"],
                "tags": [],
                "has_pdf": false,
                "has_epub": true,
                "has_snapshot": false,
                "pdf_cached": false,
                "pdf_local_path": "",
                "last_opened": 0
            },
            {
                "item_key": "ITEM_SN1",
                "attachment_key": "ATT_SN11",
                "citation_key": "Woodward 2021",
                "title": "Scientific Explanation",
                "authors": "James Woodward",
                "year": "2021",
                "item_type": "webpage",
                "collections": ["Philosophy of Science"],
                "collection_keys": ["COLL0001"],
                "tags": [],
                "has_pdf": false,
                "has_epub": false,
                "has_snapshot": true,
                "pdf_cached": false,
                "pdf_local_path": "",
                "last_opened": 0
            }
        ]
    }"#;
    let path = bridge_path_in(bridge, LIBRARY_INDEX_FILE).unwrap();
    assert_eq!(bridge_write_atomic(&path, raw.as_bytes()), 0);
    let loaded = library_index_reload().expect("library_index_reload must load Phase 3 fixture");
    check!(loaded.items.len() == 4);
}

fn probe(a: &mut App, key: &str) -> String {
    let mut out = String::new();
    assert_eq!(app_probe(a, key, &mut out, 512), 0, "probe {key}");
    out
}

fn contact(a: &mut App, type_: InType, x: f32, y: f32, t_ms: u32) {
    app_event(
        a,
        &InEvent {
            type_,
            x,
            y,
            t_ms,
            k_ms: t_ms,
            pressure: 0.5,
            ..Default::default()
        },
    );
}

fn tap_screen(a: &mut App, x: f32, y: f32) {
    let t = now_ms();
    contact(a, InType::TouchDown, x, y, t);
    contact(a, InType::TouchUp, x, y, t.wrapping_add(60));
    app_flush(a);
}

fn tap_page_point(a: &mut App, px: f32, py: f32) {
    let (sx, sy) = editor_replay_point(a, px, py);
    tap_screen(a, sx, sy);
}

fn tap_badge(a: &mut App, idx_1based: usize) {
    let area = probe(a, &format!("editor.zotero.badge.{idx_1based}.area"));
    let nums: Vec<f32> = area.split_whitespace().filter_map(|s| s.parse().ok()).collect();
    assert_eq!(nums.len(), 4, "badge {idx_1based} area: {area:?}");
    tap_page_point(a, nums[0] + nums[2] / 2.0, nums[1] + nums[3] / 2.0);
}

fn tap_bar(a: &mut App, control: &str) {
    let (x, y) = editor_zotero_bar_point(a, control)
        .unwrap_or_else(|| panic!("floating zotero bar control {control:?} missing"));
    tap_screen(a, x, y);
}

fn parse_rect_csv(s: &str) -> Rect {
    let nums: Vec<i32> = s.split(',').filter_map(|p| p.trim().parse().ok()).collect();
    assert_eq!(nums.len(), 4, "expected csv rect, got {s:?}");
    Rect::new(nums[0], nums[1], nums[2], nums[3])
}

fn parse_rect_ws(s: &str) -> Rect {
    let nums: Vec<i32> = s.split_whitespace().filter_map(|p| p.parse().ok()).collect();
    assert_eq!(nums.len(), 4, "expected whitespace rect, got {s:?}");
    Rect::new(nums[0], nums[1], nums[2], nums[3])
}

fn draw_sample_ink(a: &mut App, y_start: f32, lines: u32) {
    for line in 0..lines {
        let y = y_start + line as f32 * 90.0;
        let x0 = 260.0;
        let t = now_ms();
        contact(a, InType::PenDown, x0, y, t);
        for k in 1..=36u32 {
            let wave = if (k / 3) % 2 == 0 { 16.0 } else { -16.0 };
            contact(
                a,
                InType::PenMove,
                x0 + 28.0 * k as f32,
                y + wave,
                t.wrapping_add(8 * k),
            );
        }
        contact(a, InType::PenUp, x0 + 28.0 * 36.0, y, t.wrapping_add(320));
    }
    app_flush(a);
}

fn assert_no_overflow(theme: UiTheme, where_: &str) {
    let cuts = std::mem::take(&mut *CUTS.lock().unwrap());
    if !cuts.is_empty() {
        eprintln!("  overflow ({theme:?}, {where_}): {cuts:?}");
    }
    check!(cuts.is_empty());
}

fn save_png(a: &App, path: &Path) {
    let c = &a.screen;
    let name = std::ffi::CString::new(path.to_string_lossy().into_owned()).unwrap();
    // SAFETY: `name` is NUL-terminated; `c.px` holds `c.h` rows of `c.w` grayscale bytes spaced by `c.stride`.
    let rc = unsafe {
        grimoire::ffi_stb::stbi_write_png(
            name.as_ptr(),
            c.w,
            c.h,
            1,
            c.px.as_ptr() as *const _,
            c.stride,
        )
    };
    check!(rc != 0);
    let bytes = std::fs::read(path).unwrap_or_default();
    check!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
}

fn zref(
    pid: u32,
    seed: u32,
    area: Rect,
    item_key: &str,
    attachment_key: &str,
    page: u32,
    locator: &str,
    short_cite: &str,
) -> ZoteroRef {
    ZoteroRef {
        pid,
        seed,
        area,
        item_key: item_key.to_string(),
        attachment_key: attachment_key.to_string(),
        page,
        locator: locator.to_string(),
        short_cite: short_cite.to_string(),
        quote: String::new(),
    }
}

struct Fixtures {
    essay: NotebookInfo,
    pdf_a: NotebookInfo,
    pdf_b: NotebookInfo,
}

fn setup_notebooks() -> Fixtures {
    let essay = make_notebook("Philosophy Essay", PAPER_WIDE, 2);

    let mut pdf_a = make_notebook("The Structure of Scientific Revolutions", PAPER_BLANK, 5);
    pdf_a.last_page = 1; // 0-based page index 1 -> 1-based page 2
    assert_eq!(store_save_info(&mut pdf_a), 0);

    let src_a = ZoteroNotebookSource {
        schema_version: 1,
        item_key: "ITEM_A11".to_string(),
        attachment_key: "ATT_A111".to_string(),
        citation_key: "Kuhn 1962".to_string(),
        title: "The Structure of Scientific Revolutions".to_string(),
        authors: "Thomas S. Kuhn".to_string(),
        year: "1962".to_string(),
        pdf_local_path: "/mnt/us/zotero-bridge/storage/ATT_A111/kuhn1962.pdf".to_string(),
        page_count: 5,
        pages: vec![
            make_pdf_page_geo(0, "1"),
            make_pdf_page_geo(1, "2"),
            make_pdf_page_geo(2, "3"),
            make_pdf_page_geo(3, "42"), // custom non-index page_label on physical page 4
            make_pdf_page_geo(4, "5"),  // standard numeric page_label on physical page 5
        ],
    };
    assert!(zotero_source_save(pdf_a.id.as_str(), &src_a).is_ok());

    let pdf_b = make_notebook("The Logic of Scientific Discovery", PAPER_BLANK, 3);
    let src_b = ZoteroNotebookSource {
        schema_version: 1,
        item_key: "ITEM_B22".to_string(),
        attachment_key: "ATT_B222".to_string(),
        citation_key: "Popper 1959".to_string(),
        title: "The Logic of Scientific Discovery".to_string(),
        authors: "Karl Popper".to_string(),
        year: "1959".to_string(),
        pdf_local_path: "/mnt/us/zotero-bridge/storage/ATT_B222/popper1959.pdf".to_string(),
        page_count: 3,
        pages: vec![
            make_pdf_page_geo(0, "1"),
            make_pdf_page_geo(1, "2"),
            make_pdf_page_geo(2, "3"),
        ],
    };
    assert!(zotero_source_save(pdf_b.id.as_str(), &src_b).is_ok());

    Fixtures { essay, pdf_a, pdf_b }
}

/// Test 1: Essay -> Paper Instant In-Grimoire Jump (Zero-Bounce, 100% Offline for Bound PDFs)
fn test_1_essay_to_paper_instant_jump(a: &mut App, fx: &Fixtures, zk_home: &Path) {
    let essay_pid = fx.essay.page_ids[1]; // page 2
    check!(essay_pid == 2);

    // Populate Essay page 2 with badges covering 1a, 1b, 1c, and 1d.
    let mut zl = ZoteroLinks { writable: true, ..Default::default() };
    check!(zl.push(zref(
        essay_pid,
        0x8000_0101,
        Rect::new(240, 200, 460, 84),
        "ITEM_A11",
        "ATT_A111",
        3,
        "p. 3",
        "Kuhn 1962, p. 3",
    )));
    check!(zl.push(zref(
        essay_pid,
        0x8000_0102,
        Rect::new(240, 310, 480, 84),
        "ITEM_B22",
        "", // empty attachment_key -> item_key fallback
        2,
        "p. 2",
        "Popper 1959, p. 2",
    )));
    check!(zl.push(zref(
        essay_pid,
        0x8000_0103,
        Rect::new(240, 420, 480, 84),
        "ITEM_A11",
        "ATT_A111",
        999, // clamps cleanly to page 5
        "p. 999",
        "Kuhn 1962, p. 999",
    )));
    check!(zl.push(zref(
        essay_pid,
        0x8000_0104,
        Rect::new(240, 530, 400, 84),
        "ITEM_A11",
        "ATT_A111",
        0, // opens at PDF A's last_page
        "",
        "Kuhn 1962",
    )));
    check!(zl.push(zref(
        essay_pid,
        0x8000_0105,
        Rect::new(240, 640, 480, 84),
        "ITEM_EP1",
        "ATT_EP11",
        12,
        "ch. 2",
        "Hofstadter 1979",
    )));
    check!(zl.push(zref(
        essay_pid,
        0x8000_0106,
        Rect::new(240, 750, 480, 84),
        "ITEM_SN1",
        "ATT_SN11",
        1,
        "",
        "Woodward 2021",
    )));
    assert_eq!(zoterolinks_save(fx.essay.id.as_str(), &mut zl, &fx.essay), 0);

    // Also place a `z:` nav-link on Essay page 2 pointing to PDF B by item_key at page 2.
    let mut nav = Nav::default();
    nav_init(&mut nav);
    let z_target = zotero_target_from_parts("ITEM_B22", "", 2, "").expect("valid z: target");
    check!(z_target == "z:ITEM_B22@2");
    let z_nav_area = Rect::new(240, 900, 360, 120);
    let nav_link = NavLink {
        pid: essay_pid,
        seed: 0x0000_BEEF,
        target: FixedStr::from_str(&z_target),
        has_area: true,
        area: z_nav_area,
        outline: LinkOutline::default(),
    };
    assert_eq!(nav_link_put(&mut nav, &nav_link), 0);
    assert_eq!(nav_save_links(fx.essay.id.as_str(), &mut nav, &fx.essay), 0);

    // Open Essay at page 2.
    editor_open_notebook_at(a, fx.essay.id.as_str(), essay_pid, false);
    app_flush(a);
    check!(probe(a, "editor.notebook_id") == fx.essay.id.as_str());
    check!(editor_page_number(a) == Some((2, 2)));
    check!(probe(a, "editor.zotero.pid") == "2");
    check!(probe(a, "editor.zotero.badges") == "6");
    check!(probe(a, "editor.zotero.bar") == "0");

    // --- Test 1a: Badge tap by attachment_key ---
    tap_badge(a, 1);
    check!(a.running && a.exit_code == 0);
    assert_no_zk_sync(zk_home);
    check!(probe(a, "editor.notebook_id") == fx.pdf_a.id.as_str());
    check!(editor_page_number(a) == Some((3, 5)));
    check!(a.zotero_return.as_ref().is_some_and(|r| {
        r.notebook_id == fx.essay.id.as_str()
            && r.page_id == essay_pid
            && r.page_index == 1
            && r.notebook_title == "Philosophy Essay"
    }));
    check!(probe(a, "editor.zotero.bar") == "1");
    check!(!probe(a, "editor.zotero.bar.back").is_empty());
    check!(!probe(a, "editor.zotero.bar.cite").is_empty());
    check!(probe(a, "editor.zotero.source") == "ITEM_A11:ATT_A111");

    // --- Test 1b: Badge tap by item_key fallback AND `z:` nav-link activation ---
    editor_open_notebook_at(a, fx.essay.id.as_str(), essay_pid, false);
    app_flush(a);
    tap_badge(a, 2);
    check!(a.running && a.exit_code == 0);
    assert_no_zk_sync(zk_home);
    check!(probe(a, "editor.notebook_id") == fx.pdf_b.id.as_str());
    check!(editor_page_number(a) == Some((2, 3)));
    check!(a.zotero_return.as_ref().is_some_and(|r| {
        r.notebook_id == fx.essay.id.as_str() && r.page_id == essay_pid
    }));
    check!(probe(a, "editor.zotero.bar") == "1");

    // Now test `z:` nav-link activation from Essay page 2.
    editor_open_notebook_at(a, fx.essay.id.as_str(), essay_pid, false);
    app_flush(a);
    tap_page_point(
        a,
        (z_nav_area.x + z_nav_area.w / 2) as f32,
        (z_nav_area.y + z_nav_area.h / 2) as f32,
    );
    check!(a.running && a.exit_code == 0);
    assert_no_zk_sync(zk_home);
    check!(probe(a, "editor.notebook_id") == fx.pdf_b.id.as_str());
    check!(editor_page_number(a) == Some((2, 3)));
    check!(a.zotero_return.as_ref().is_some_and(|r| {
        r.notebook_id == fx.essay.id.as_str() && r.page_id == essay_pid
    }));

    // --- Test 1c: Page clamping (`page: 999`) & `page == 0` last_page fallback ---
    editor_open_notebook_at(a, fx.essay.id.as_str(), essay_pid, false);
    app_flush(a);
    tap_badge(a, 3); // page: 999 on 5-page PDF A
    check!(a.running && a.exit_code == 0);
    assert_no_zk_sync(zk_home);
    check!(probe(a, "editor.notebook_id") == fx.pdf_a.id.as_str());
    check!(editor_page_number(a) == Some((5, 5)));

    // Navigate to page 2 inside PDF A so its saved `last_page` is index 1 (page 2), then return to Essay.
    for _ in 0..3 {
        check!(editor_shortcut(a, SHORTCUT_PREV_PAGE));
    }
    app_flush(a);
    check!(editor_page_number(a) == Some((2, 5)));

    editor_open_notebook_at(a, fx.essay.id.as_str(), essay_pid, false);
    app_flush(a);
    tap_badge(a, 4); // page: 0 -> opens at PDF A's last_page (page 2)
    check!(a.running && a.exit_code == 0);
    assert_no_zk_sync(zk_home);
    check!(probe(a, "editor.notebook_id") == fx.pdf_a.id.as_str());
    check!(editor_page_number(a) == Some((2, 5)));

    // --- Test 1d: Non-PDF EPUB & Snapshot badges -> exit-42 handoff, plus Open in Zotero escape hatch ---
    handoff_clear();
    editor_open_notebook_at(a, fx.essay.id.as_str(), essay_pid, false);
    app_flush(a);

    // EPUB badge tap
    tap_badge(a, 5);
    check!(!a.running && a.exit_code == EXIT_HANDOFF_TO_ZOTERO);
    assert_no_zk_sync(zk_home);
    let h_ep = handoff_peek().expect("EPUB badge must write handoff.json");
    check!(h_ep.target_enum() == Some(HandoffTarget::Zotero));
    check!(h_ep.zotero.item_key == "ITEM_EP1" && h_ep.zotero.attachment_key == "ATT_EP11");
    check!(h_ep.zotero.page == 12);
    check!(h_ep.return_to.as_ref().is_some_and(|r| {
        r.notebook_id == fx.essay.id.as_str() && r.page_id == essay_pid
    }));

    // Reset and tap Snapshot badge
    a.running = true;
    a.exit_code = EXIT_CLEAN;
    handoff_clear();
    tap_badge(a, 6);
    check!(!a.running && a.exit_code == EXIT_HANDOFF_TO_ZOTERO);
    assert_no_zk_sync(zk_home);
    let h_sn = handoff_peek().expect("Snapshot badge must write handoff.json");
    check!(h_sn.target_enum() == Some(HandoffTarget::Zotero));
    check!(h_sn.zotero.item_key == "ITEM_SN1" && h_sn.zotero.attachment_key == "ATT_SN11");
    check!(h_sn.return_to.as_ref().is_some_and(|r| {
        r.notebook_id == fx.essay.id.as_str() && r.page_id == essay_pid
    }));

    // Reset and verify explicit "Open in Zotero" escape hatch from Zotero sheet while on bound PDF A page 4.
    a.running = true;
    a.exit_code = EXIT_CLEAN;
    handoff_clear();
    editor_open_notebook_at(a, fx.pdf_a.id.as_str(), fx.pdf_a.page_ids[3], false);
    app_flush(a);
    check!(editor_page_number(a) == Some((4, 5)));
    check!(editor_zotero_control(a, "show", ""));
    app_flush(a);
    check!(probe(a, "editor.zotero.sheet") == "1");
    check!(probe(a, "editor.zotero.bar") == "0");
    let (ox, oy) = editor_panel_point(a, "zotero.open_current")
        .expect("Open in Zotero button must be present on bound PDF");
    tap_screen(a, ox, oy);
    check!(!a.running && a.exit_code == EXIT_HANDOFF_TO_ZOTERO);
    assert_no_zk_sync(zk_home);
    let h_cur = handoff_peek().expect("open_current must write handoff.json");
    check!(h_cur.target_enum() == Some(HandoffTarget::Zotero));
    check!(h_cur.zotero.item_key == "ITEM_A11" && h_cur.zotero.attachment_key == "ATT_A111");
    check!(h_cur.zotero.page == 4);

    // Clean reset for Test 2
    a.running = true;
    a.exit_code = EXIT_CLEAN;
    handoff_clear();
    editor_close_zotero_sheet(a);
    app_flush(a);
}

/// Test 2: Paper -> Essay 1-Tap `Cite p. N` + `<- Back` + Multi-Hop Safety + Standalone + Wet-Ink
fn test_2_cite_back_multihop_and_wet_ink(a: &mut App, fx: &Fixtures, zk_home: &Path) {
    let essay_pid = fx.essay.page_ids[1]; // page 2

    // Reset Essay page 2 to two clean starting badges (both page > 0) and remove the temporary z: nav link.
    let mut clean_nav = Nav::default();
    nav_init(&mut clean_nav);
    assert_eq!(nav_save_links(fx.essay.id.as_str(), &mut clean_nav, &fx.essay), 0);

    let mut zl = ZoteroLinks { writable: true, ..Default::default() };
    check!(zl.push(zref(
        essay_pid,
        0x8000_0201,
        Rect::new(240, 160, 460, 84),
        "ITEM_A11",
        "ATT_A111",
        3,
        "p. 3",
        "Kuhn 1962, p. 3",
    )));
    check!(zl.push(zref(
        essay_pid,
        0x8000_0202,
        Rect::new(240, 264, 480, 84),
        "ITEM_B22",
        "ATT_B222",
        2,
        "p. 2",
        "Popper 1959, p. 2",
    )));
    assert_eq!(zoterolinks_save(fx.essay.id.as_str(), &mut zl, &fx.essay), 0);

    // Open Essay page 2 and add neat handwritten lines in the lower canvas so screenshots show live essay ink.
    editor_open_notebook_at(a, fx.essay.id.as_str(), essay_pid, false);
    app_flush(a);
    draw_sample_ink(a, 1180.0, 4);
    check!(probe(a, "editor.zotero.badges") == "2");

    // Jump Essay page 2 -> PDF A page 3
    tap_badge(a, 1);
    check!(probe(a, "editor.notebook_id") == fx.pdf_a.id.as_str());
    check!(editor_page_number(a) == Some((3, 5)));
    check!(probe(a, "editor.zotero.bar") == "1");

    // Navigate to page 4 (custom page_label "42") and add a subtle margin annotation on PDF A page 4.
    check!(editor_shortcut(a, SHORTCUT_NEXT_PAGE));
    app_flush(a);
    check!(editor_page_number(a) == Some((4, 5)));
    draw_sample_ink(a, 620.0, 2);

    // 1-tap Cite on page 4 (custom page_label "42")
    tap_bar(a, "cite");
    let toast4 = probe(a, "toast");
    check!(toast4.contains("Cited p. 42 in Philosophy Essay"));

    // Navigate to page 5 (standard numeric page_label "5") and 1-tap Cite again in the SAME visit!
    check!(editor_shortcut(a, SHORTCUT_NEXT_PAGE));
    app_flush(a);
    check!(editor_page_number(a) == Some((5, 5)));
    tap_bar(a, "cite");
    let toast5 = probe(a, "toast");
    check!(toast5.contains("Cited p. 5 in Philosophy Essay"));

    // Inspect zoterolinks.ini on Essay page 2 directly on disk.
    let saved = zoterolinks_load(fx.essay.id.as_str());
    let p2_refs = zoterolinks_of(&saved, essay_pid);
    check!(p2_refs.len() == 4);

    // All seeds distinct, all areas pairwise non-overlapping.
    for i in 0..p2_refs.len() {
        for j in (i + 1)..p2_refs.len() {
            check!(p2_refs[i].seed != p2_refs[j].seed);
            check!(!rect_overlaps(p2_refs[i].area, p2_refs[j].area));
        }
    }
    let cite4 = p2_refs.iter().find(|r| r.item_key == "ITEM_A11" && r.page == 4);
    let cite5 = p2_refs.iter().find(|r| r.item_key == "ITEM_A11" && r.page == 5);
    check!(cite4.is_some_and(|r| {
        r.attachment_key == "ATT_A111" && r.short_cite == "Kuhn 1962, p. 42" && r.locator == "p. 42"
    }));
    check!(cite5.is_some_and(|r| {
        r.attachment_key == "ATT_A111" && r.short_cite == "Kuhn 1962, p. 5" && r.locator == "p. 5"
    }));

    // --- Wet-ink exclusion check while floating PDF bar (`back` + `cite`) is visible ---
    let (bx, by) = editor_zotero_bar_point(a, "back").expect("back button point");
    let (cx, cy) = editor_zotero_bar_point(a, "cite").expect("cite button point");
    let pen_at = |x: f32, y: f32| InEvent {
        type_: InType::PenDown,
        x,
        y,
        pressure: 0.5,
        ..Default::default()
    };
    let mut style_back = LiveStyle::default();
    editor_live_style(a, &pen_at(bx, by), &mut style_back);
    check!(style_back.enabled == 0);

    let mut style_cite = LiveStyle::default();
    editor_live_style(a, &pen_at(cx, cy), &mut style_cite);
    check!(style_cite.enabled == 0);

    let mut style_canvas = LiveStyle::default();
    editor_live_style(a, &pen_at(W as f32 * 0.45, H as f32 * 0.50), &mut style_canvas);
    check!(style_canvas.enabled == 1);

    // --- Tap `<- Back` -> instant return to Philosophy Essay page 2 ---
    tap_bar(a, "back");
    check!(a.running && a.exit_code == 0);
    assert_no_zk_sync(zk_home);
    check!(probe(a, "editor.notebook_id") == fx.essay.id.as_str());
    check!(editor_page_number(a) == Some((2, 2)));
    check!(probe(a, "editor.zotero.pid") == "2");
    check!(a.zotero_return.is_none());
    check!(probe(a, "editor.zotero.bar") == "0");
    check!(probe(a, "editor.zotero.badges") == "4");

    // Verify all 4 displayed badges on Essay page 2 are non-overlapping and find the newly inserted p. 5 badge.
    let mut badge_rects = Vec::new();
    let mut p5_badge_idx = None;
    let mut pdf_b_badge_idx = None;
    for i in 1..=4usize {
        let r = parse_rect_ws(&probe(a, &format!("editor.zotero.badge.{i}.area")));
        badge_rects.push(r);
        let item = probe(a, &format!("editor.zotero.badge.{i}.item"));
        let pg = probe(a, &format!("editor.zotero.badge.{i}.page"));
        if item == "ITEM_A11" && pg == "5" {
            p5_badge_idx = Some(i);
        }
        if item == "ITEM_B22" && pg == "2" {
            pdf_b_badge_idx = Some(i);
        }
    }
    for i in 0..badge_rects.len() {
        for j in (i + 1)..badge_rects.len() {
            check!(!rect_overlaps(badge_rects[i], badge_rects[j]));
        }
    }
    let p5_idx = p5_badge_idx.expect("newly inserted p. 5 badge must be visible");
    let pdf_b_idx = pdf_b_badge_idx.expect("Popper 1959 badge must be visible");

    // Tap newly inserted p. 5 badge -> jumps straight back to PDF A page 5 inside Grimoire!
    tap_badge(a, p5_idx);
    check!(a.running && a.exit_code == 0);
    assert_no_zk_sync(zk_home);
    check!(probe(a, "editor.notebook_id") == fx.pdf_a.id.as_str());
    check!(editor_page_number(a) == Some((5, 5)));
    check!(a.zotero_return.as_ref().is_some_and(|r| {
        r.notebook_id == fx.essay.id.as_str() && r.page_id == essay_pid
    }));

    // --- Multi-hop safety: Essay -> PDF A -> Back -> Essay -> PDF B -> PDF A -> Back -> Essay ---
    tap_bar(a, "back");
    check!(probe(a, "editor.notebook_id") == fx.essay.id.as_str());
    check!(editor_page_number(a) == Some((2, 2)));
    check!(a.zotero_return.is_none());

    tap_badge(a, pdf_b_idx);
    check!(probe(a, "editor.notebook_id") == fx.pdf_b.id.as_str());
    check!(editor_page_number(a) == Some((2, 3)));
    check!(a.zotero_return.as_ref().is_some_and(|r| {
        r.notebook_id == fx.essay.id.as_str()
            && r.page_id == essay_pid
            && r.notebook_id != fx.pdf_a.id.as_str()
            && r.notebook_id != fx.pdf_b.id.as_str()
    }));

    // PDF B -> PDF A direct hop must preserve original Essay return context (never PDF B!)
    zotero_open_bound_notebook(
        a,
        fx.pdf_a.id.as_str(),
        1,
        Some(ReturnPlace {
            notebook_id: fx.pdf_b.id.to_string(),
            page_id: fx.pdf_b.page_ids[1],
            page_index: 1,
            ..Default::default()
        }),
    );
    app_flush(a);
    check!(probe(a, "editor.notebook_id") == fx.pdf_a.id.as_str());
    check!(editor_page_number(a) == Some((1, 5)));
    check!(a.zotero_return.as_ref().is_some_and(|r| {
        r.notebook_id == fx.essay.id.as_str()
            && r.page_id == essay_pid
            && r.notebook_id != fx.pdf_b.id.as_str()
    }));

    tap_bar(a, "back");
    check!(probe(a, "editor.notebook_id") == fx.essay.id.as_str());
    check!(editor_page_number(a) == Some((2, 2)));
    check!(a.zotero_return.is_none());

    // Returning to Library (`SHORTCUT_LIBRARY` / `P_LIBRARY`) clears `a.zotero_return`.
    tap_badge(a, p5_idx);
    check!(a.zotero_return.is_some());
    check!(editor_shortcut(a, SHORTCUT_LIBRARY));
    app_flush(a);
    check!(probe(a, "screen") == "library");
    check!(a.zotero_return.is_none());

    // Opening PDF A standalone clears `a.zotero_return`: `bar.back` empty, `bar.cite` still shown!
    editor_open_notebook_at(a, fx.pdf_a.id.as_str(), fx.pdf_a.page_ids[1], false);
    app_flush(a);
    check!(editor_page_number(a) == Some((2, 5)));
    check!(a.zotero_return.is_none());
    check!(probe(a, "editor.zotero.bar") == "1");
    check!(probe(a, "editor.zotero.bar.back").is_empty());
    check!(!probe(a, "editor.zotero.bar.cite").is_empty());

    // --- Standalone Cite check 1: `a.last_essay` set -> cites directly into last_essay ---
    check!(a.last_essay.as_ref().is_some_and(|r| {
        r.notebook_id == fx.essay.id.as_str() && r.page_id == essay_pid
    }));
    tap_bar(a, "cite");
    check!(probe(a, "toast").contains("Cited p. 2 in Philosophy Essay"));
    let after_last_essay = zoterolinks_load(fx.essay.id.as_str());
    let p2_after = zoterolinks_of(&after_last_essay, essay_pid);
    check!(p2_after.len() == 5);
    check!(p2_after.iter().any(|r| {
        r.item_key == "ITEM_A11" && r.page == 2 && r.short_cite == "Kuhn 1962, p. 2"
    }));

    // Remove the standalone p. 2 citation so Essay page 2 keeps the clean 4-badge state for screenshots.
    let mut keep_four = ZoteroLinks { writable: true, ..Default::default() };
    for r in p2_after {
        if !(r.item_key == "ITEM_A11" && r.page == 2) {
            check!(keep_four.push(r.clone()));
        }
    }
    assert_eq!(zoterolinks_save(fx.essay.id.as_str(), &mut keep_four, &fx.essay), 0);

    // --- Standalone Cite check 2: `a.last_essay == None && a.zotero_return == None` -> queues in inbox ---
    a.last_essay = None;
    a.zotero_return = None;
    check!(editor_shortcut(a, SHORTCUT_NEXT_PAGE));
    app_flush(a);
    check!(editor_page_number(a) == Some((3, 5)));
    check!(inbox_count() == 0);

    tap_bar(a, "cite");
    check!(probe(a, "toast").contains("Queued p. 3 in Zotero inbox"));
    check!(inbox_count() == 1);
    let queued = inbox_peek();
    check!(queued.len() == 1);
    if let Some(cit) = queued.first() {
        check!(cit.item_key == "ITEM_A11");
        check!(cit.attachment_key == "ATT_A111");
        check!(cit.page == 3);
        check!(cit.locator == "p. 3");
        check!(cit.short_cite == "Kuhn 1962, p. 3");
        check!(inbox_remove(&cit.id) == 0);
    }
    check!(inbox_count() == 0);
    assert_no_zk_sync(zk_home);
}

/// Test 3: 300-DPI Headless Render Screenshots (`Gothic` & `Classic`) + Zero-Overlap & Zero-Overflow
fn test_3_themes_layout_and_screenshots(a: &mut App, fx: &Fixtures, zk_home: &Path) {
    let essay_pid = fx.essay.page_ids[1];
    let shot_dir = std::env::var_os("GRIMOIRE_PHASE3_SHOT_DIR")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from);
    if let Some(ref d) = shot_dir {
        std::fs::create_dir_all(d).unwrap();
    }

    let orig_theme = ui_theme();
    let orig_status_on = a.settings.status_on;
    let orig_status_corner = a.settings.status_corner;
    a.settings.status_on = true;

    for (theme, suffix) in [(UiTheme::Gothic, "gothic"), (UiTheme::Classic, "classic")] {
        a.settings.theme = theme as i32;
        ui_set_theme(theme);
        a.settings.status_corner = 0; // default top-left

        // Open Essay page 2 and jump to PDF A page 4 so `<- Philosophy Essay` + `Cite p. 4` are both shown.
        editor_open_notebook_at(a, fx.essay.id.as_str(), essay_pid, false);
        app_flush(a);
        zotero_open_bound_notebook(
            a,
            fx.pdf_a.id.as_str(),
            4,
            Some(ReturnPlace {
                notebook_id: fx.essay.id.to_string(),
                page_id: essay_pid,
                page_index: 1,
                ..Default::default()
            }),
        );
        app_toast_dismiss(a);
        editor_zotero_bar_refresh(a);
        status_tick(a, now_ms());
        app_flush(a);

        // Full compose under overflow hook with status_corner = 0 (top-left).
        CUTS.lock().unwrap().clear();
        check!(probe(a, "editor.screen_composed") == "1");
        assert_no_overflow(theme, "PDF bar (status_corner=0)");

        check!(probe(a, "editor.zotero.bar") == "1");
        let back_r = parse_rect_csv(&probe(a, "editor.zotero.bar.back"));
        let cite_r = parse_rect_csv(&probe(a, "editor.zotero.bar.cite"));
        let bar_r = rect_union(back_r, cite_r);
        let panel_r = parse_rect_ws(&probe(a, "editor.panel_rect"));
        let status_z0 = status_zone(a);

        check!(!rect_is_empty(back_r) && !rect_is_empty(cite_r));
        check!(!rect_overlaps(back_r, cite_r));
        check!(!rect_overlaps(bar_r, status_z0));
        check!(!rect_overlaps(bar_r, panel_r));

        if let Some(ref dir) = shot_dir {
            save_png(a, &dir.join(format!("phase3_pdf_bar_{suffix}.png")));
        }

        // Move status_corner = 1 (top-right): bar must shift cleanly with ZERO overlap with status_zone or panel!
        a.settings.status_corner = 1;
        editor_zotero_bar_refresh(a);
        status_tick(a, now_ms());
        app_flush(a);

        CUTS.lock().unwrap().clear();
        check!(probe(a, "editor.screen_composed") == "1");
        assert_no_overflow(theme, "PDF bar (status_corner=1)");

        let back_r1 = parse_rect_csv(&probe(a, "editor.zotero.bar.back"));
        let cite_r1 = parse_rect_csv(&probe(a, "editor.zotero.bar.cite"));
        let bar_r1 = rect_union(back_r1, cite_r1);
        let panel_r1 = parse_rect_ws(&probe(a, "editor.panel_rect"));
        let status_z1 = status_zone(a);

        check!(!rect_overlaps(back_r1, cite_r1));
        check!(!rect_overlaps(bar_r1, status_z1));
        check!(!rect_overlaps(bar_r1, panel_r1));
        check!(bar_r1.x < bar_r.x);

        // Restore status_corner = 0
        a.settings.status_corner = 0;
        editor_zotero_bar_refresh(a);
        status_tick(a, now_ms());
        app_flush(a);

        // Links bar opens -> floating Zotero bar hides (`zotero.bar == "0"`)
        check!(editor_shortcut(a, SHORTCUT_LINKS));
        app_flush(a);
        check!(probe(a, "editor.bar") == "open");
        check!(probe(a, "editor.zotero.bar") == "0");
        check!(editor_shortcut(a, SHORTCUT_BACK));
        app_flush(a);
        check!(probe(a, "editor.bar") == "0");
        check!(probe(a, "editor.zotero.bar") == "1");

        // Zotero sheet opens -> floating Zotero bar hides (`zotero.bar == "0"`), zero overflow on sheet too
        check!(editor_zotero_control(a, "show", ""));
        app_flush(a);
        check!(probe(a, "editor.zotero.sheet") == "1");
        check!(probe(a, "editor.zotero.bar") == "0");
        CUTS.lock().unwrap().clear();
        check!(probe(a, "editor.screen_composed") == "1");
        assert_no_overflow(theme, "Zotero sheet over bound PDF");
        editor_close_zotero_sheet(a);
        app_flush(a);
        check!(probe(a, "editor.zotero.bar") == "1");

        // Tap `<- Back` -> Essay page 2 showing the newly inserted 1-tap citation badges
        tap_bar(a, "back");
        app_toast_dismiss(a);
        editor_zotero_bar_refresh(a);
        status_tick(a, now_ms());
        app_flush(a);

        check!(probe(a, "editor.notebook_id") == fx.essay.id.as_str());
        check!(editor_page_number(a) == Some((2, 2)));
        check!(probe(a, "editor.zotero.bar") == "0");
        check!(probe(a, "editor.zotero.badges") == "4");

        CUTS.lock().unwrap().clear();
        check!(probe(a, "editor.screen_composed") == "1");
        assert_no_overflow(theme, "Essay page with 1-tap citations");

        if let Some(ref dir) = shot_dir {
            save_png(a, &dir.join(format!("phase3_essay_cited_{suffix}.png")));
        }
    }

    a.settings.theme = orig_theme as i32;
    ui_set_theme(orig_theme);
    a.settings.status_on = orig_status_on;
    a.settings.status_corner = orig_status_corner;
    assert_no_zk_sync(zk_home);
}

fn main() {
    if std::env::args().any(|a| a == "--list") {
        println!("zotero_phase3: test");
        return;
    }

    let root = scratch_root();
    let bridge = root.join("bridge");
    let store = root.join("store");
    let zk_home = root.join("zk-home");
    let zk_cfg = root.join("zk-config");
    let bin_path = root.join("bin").join("zk-sync");

    for d in [&bridge, &bridge.join("inbox"), &store, &zk_home, &zk_cfg] {
        std::fs::create_dir_all(d).unwrap();
    }
    write_sentinel_zk_sync(&bin_path);

    let bridge_str = bridge.to_string_lossy().into_owned();
    let store_str = store.to_string_lossy().into_owned();

    std::env::set_var("GRIMOIRE_SCRIPT", "/dev/null");
    bridge_set_dir(Some(&bridge_str));
    zk_sync_set_test_config(Some(ZkSyncConfig {
        bin: bin_path.to_string_lossy().into_owned(),
        zk_home: zk_home.to_string_lossy().into_owned(),
        zk_config_dir: zk_cfg.to_string_lossy().into_owned(),
        local_timeout_ms: 5_000,
        network_timeout_ms: 5_000,
    }));
    zk_bg_reset();
    check!(zk_sync_available());

    let (mut dw, mut dh, mut ddpi) = (0, 0, 0.0);
    let _ = display_open(&mut dw, &mut dh, &mut ddpi);
    assert_eq!(store_init(&store_str), 0);
    let mut a = App::default();
    assert_eq!(app_init(&mut a, W, H, DPI), 0);
    app_toast_dismiss(&mut a);
    app_flush(&mut a);
    font_set_overflow_hook(Some(overflow_hook));

    write_library_index(&bridge_str);
    let fx = setup_notebooks();

    eprintln!("- test_1_essay_to_paper_instant_jump");
    test_1_essay_to_paper_instant_jump(&mut a, &fx, &zk_home);

    eprintln!("- test_2_cite_back_multihop_and_wet_ink");
    test_2_cite_back_multihop_and_wet_ink(&mut a, &fx, &zk_home);

    eprintln!("- test_3_themes_layout_and_screenshots");
    test_3_themes_layout_and_screenshots(&mut a, &fx, &zk_home);

    font_set_overflow_hook(None);
    let wait_deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while bg_busy() && std::time::Instant::now() < wait_deadline {
        app_tick(&mut a, now_ms());
        app_flush(&mut a);
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    app_shutdown(&mut a);
    display_close();
    zk_sync_set_test_config(None);
    bridge_set_dir(None);
    std::env::remove_var("GRIMOIRE_SCRIPT");
    let _ = std::fs::remove_dir_all(&root);
    done();
}

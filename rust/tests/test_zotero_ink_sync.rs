//! Phase 4 integration tests: two-way Zotero ink sync (coordinates, simplification, create/update/delete,
//! remote highlights + ink overlays, conflicts, offline) against a stateful mock `zk-sync`, plus layout
//! invariants of the `<- Essay | Cite p. N | Sync` PDF bar and optional screenshots when
//! `GRIMOIRE_PHASE4_SHOT_DIR` is set.

mod testlib;

use grimoire::editor::{editor_replay_point, editor_zotero_bar_refresh, editor_zotero_control};
use grimoire::prelude::*;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
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
    let root = PathBuf::from(base).join(format!("zotero-ink-sync-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

// ---------------------------------------------------------------------------------------------
// Mock zk-sync: a tiny stateful stand-in for the Zotero SQLite (python3), state in $ZK_HOME/mock
// ---------------------------------------------------------------------------------------------

const MOCK_ZK_SYNC: &str = r#"#!/usr/bin/env python3
import os, sys, json, shutil

H = os.path.join(os.environ["ZK_HOME"], "mock")
os.makedirs(H, exist_ok=True)
ALPHA = "23456789ABCDEFGHIJKLMNPQRSTUVWXYZ"
STATE = os.path.join(H, "state.json")

with open(os.path.join(H, "calls.log"), "a") as f:
    f.write(" ".join(sys.argv[1:]) + "\n")

def load():
    try:
        with open(STATE) as f:
            return json.load(f)
    except Exception:
        return {"ink": [], "highlights": [], "seq": 0, "pushes": 0, "upserts": 0}

def save(st):
    with open(STATE, "w") as f:
        json.dump(st, f)

def fail(msg, code=1):
    sys.stderr.write(msg + "\n")
    sys.exit(code)

def rnd(v):
    r = round(float(v) * 100) / 100.0
    return 0.0 if r == 0 else r

def canon(pi, width, paths, color, comment, tags):
    return [int(pi), rnd(width), [[rnd(v) for v in p] for p in paths], color, comment, list(tags)]

def rec_canon(r):
    return canon(r["pageIndex"], r["width"], r["paths"], r["color"], r["comment"], r["tags"])

def sort_index(pi):
    return "%05d|000000|%05d" % (pi, 0)

def mint(st):
    k = "GRMA222" + ALPHA[st["seq"] % len(ALPHA)]
    st["seq"] += 1
    return k

args = sys.argv[1:]
offline = os.path.exists(os.path.join(H, "offline"))
if not args:
    fail("usage", 2)
cmd = args[0]

if cmd == "ink-pull":
    local_only = "--local-only" in args
    if offline and not local_only:
        fail("network error: connection refused apiKey=SECRET123 (https://api.zotero.org/users/1/items?key=SECRET123)")
    st = load()
    ink = []
    for r in st["ink"]:
        if r.get("deleted"):
            continue
        o = dict(r)
        o.pop("deleted", None)
        o["conflict"] = False
        ink.append(o)
    out = {"schema_version": 1, "attachment_key": args[1], "synced": not local_only, "ink": ink,
           "highlights": st["highlights"], "other_counts": {"note": 0, "image": 0, "text": 0}}
    sys.stdout.write(json.dumps(out))
elif cmd == "ink-upsert":
    st = load()
    with open(args[2]) as f:
        payload = json.load(f)
    st["upserts"] += 1
    n = st["upserts"]
    os.makedirs(os.path.join(H, "upserts"), exist_ok=True)
    os.makedirs(os.path.join(H, "results"), exist_ok=True)
    shutil.copy(args[2], os.path.join(H, "upserts", "%d.json" % n))
    by_key = {r["key"]: r for r in st["ink"]}
    res = []
    counts = {"created": 0, "updated": 0, "deleted": 0, "unchanged": 0}
    matched = set()

    def delete(r):
        if r["version"] == 0:
            st["ink"] = [x for x in st["ink"] if x["key"] != r["key"]]
        else:
            r["deleted"] = True
            r["dirty"] = True

    for i, a in enumerate(payload.get("annotations", [])):
        key = a.get("key", "")
        paths = a.get("paths", [])
        if key:
            r = by_key.get(key)
            if r is not None:
                if paths == []:
                    if r.get("deleted"):
                        act = "unchanged"
                    else:
                        delete(r); act = "deleted"
                else:
                    if r.get("deleted"):
                        fail("annotation is deleted")
                    new = canon(a["pageIndex"], a["width"], paths, a["color"], a.get("comment", ""), a.get("tags", []))
                    if new == rec_canon(r):
                        act = "unchanged"
                    else:
                        r.update({"pageIndex": a["pageIndex"], "width": rnd(a["width"]),
                                  "paths": [[rnd(v) for v in p] for p in paths], "color": a["color"],
                                  "comment": a.get("comment", ""), "tags": a.get("tags", []), "dirty": True})
                        act = "updated"
                res.append({"index": i, "key": key, "action": act, "version": r["version"], "dirty": r["dirty"]})
                counts[act] += 1
            else:
                if paths == []:
                    res.append({"index": i, "key": key, "action": "unchanged", "version": 0, "dirty": False})
                    counts["unchanged"] += 1
                else:
                    r = {"key": key, "version": 0, "pageIndex": a["pageIndex"], "width": rnd(a["width"]),
                         "paths": [[rnd(v) for v in p] for p in paths], "color": a["color"],
                         "comment": a.get("comment", ""), "tags": a.get("tags", []),
                         "sortIndex": a.get("sortIndex") or sort_index(a["pageIndex"]),
                         "pageLabel": a.get("pageLabel") or str(a["pageIndex"] + 1), "dirty": True}
                    st["ink"].append(r); by_key[key] = r
                    res.append({"index": i, "key": key, "action": "created", "version": 0, "dirty": True})
                    counts["created"] += 1
        else:
            if paths == []:
                res.append({"index": i, "key": "", "action": "unchanged", "version": 0, "dirty": False})
                counts["unchanged"] += 1
                continue
            new = canon(a["pageIndex"], a["width"], paths, a["color"], a.get("comment", ""), a.get("tags", []))
            hit = None
            for r in st["ink"]:
                if not r.get("deleted") and r["key"] not in matched and rec_canon(r) == new:
                    hit = r; break
            if hit is not None:
                matched.add(hit["key"])
                res.append({"index": i, "key": hit["key"], "action": "unchanged", "version": hit["version"], "dirty": hit["dirty"]})
                counts["unchanged"] += 1
            else:
                k = mint(st)
                r = {"key": k, "version": 0, "pageIndex": a["pageIndex"], "width": rnd(a["width"]),
                     "paths": [[rnd(v) for v in p] for p in paths], "color": a["color"],
                     "comment": a.get("comment", ""), "tags": a.get("tags", []),
                     "sortIndex": a.get("sortIndex") or sort_index(a["pageIndex"]),
                     "pageLabel": a.get("pageLabel") or str(a["pageIndex"] + 1), "dirty": True}
                st["ink"].append(r); by_key[k] = r
                matched.add(k)
                res.append({"index": i, "key": k, "action": "created", "version": 0, "dirty": True})
                counts["created"] += 1
    for k in payload.get("delete_keys", []):
        r = by_key.get(k)
        if r is None or r.get("deleted"):
            res.append({"key": k, "action": "unchanged", "version": 0, "dirty": False})
            counts["unchanged"] += 1
        else:
            delete(r)
            res.append({"key": k, "action": "deleted", "version": r["version"], "dirty": True})
            counts["deleted"] += 1
    save(st)
    out = {"schema_version": 1, "attachment_key": args[1], "created": counts["created"], "updated": counts["updated"],
           "deleted": counts["deleted"], "unchanged": counts["unchanged"], "results": res}
    with open(os.path.join(H, "results", "%d.json" % n), "w") as f:
        json.dump(out, f)
    sys.stdout.write(json.dumps(out))
elif cmd == "push":
    if offline:
        fail("push failed: cannot reach server apiKey=SECRET123")
    st = load()
    st["pushes"] += 1
    mx = max([r["version"] for r in st["ink"]] + [h["version"] for h in st["highlights"]] + [0])
    v = mx + 1
    keep = []
    for r in st["ink"]:
        if r.get("deleted"):
            continue
        if r.get("dirty"):
            r["version"] = v
            r["dirty"] = False
        keep.append(r)
    st["ink"] = keep
    save(st)
    sys.stdout.write("pushed\n")
elif cmd == "_edit":
    # a remote edit (by Zotero) of ink KEY: different paths, library version bump, nothing dirty
    st = load()
    mx = max([r["version"] for r in st["ink"]] + [0])
    for r in st["ink"]:
        if r["key"] == args[1]:
            r["paths"] = [[rnd(v) + 10.0 for v in p] for p in r["paths"]]
            r["version"] = mx + 1
            r["dirty"] = False
    save(st)
else:
    fail("unknown command", 2)
"#;

fn write_mock_zk_sync(bin_path: &Path) {
    if let Some(parent) = bin_path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(bin_path, MOCK_ZK_SYNC.as_bytes()).unwrap();
    let mut perms = std::fs::metadata(bin_path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(bin_path, perms).unwrap();
}

struct Mock {
    bin: PathBuf,
    home: PathBuf,
}

impl Mock {
    fn dir(&self) -> PathBuf {
        self.home.join("mock")
    }
    /// Empty the mock Zotero and seed its state JSON.
    fn reset(&self, seed: &str) {
        let _ = std::fs::remove_dir_all(self.dir());
        std::fs::create_dir_all(self.dir()).unwrap();
        std::fs::write(self.dir().join("state.json"), seed).unwrap();
    }
    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir().join("calls.log"))
            .unwrap_or_default()
            .lines()
            .map(|l| l.to_string())
            .collect()
    }
    fn count(&self, prefix: &str) -> usize {
        self.calls().iter().filter(|l| l.starts_with(prefix)).count()
    }
    fn upsert_files(&self) -> Vec<String> {
        let mut out = Vec::new();
        for n in 1..200 {
            match std::fs::read_to_string(self.dir().join("upserts").join(format!("{n}.json"))) {
                Ok(s) => out.push(s),
                Err(_) => break,
            }
        }
        out
    }
    fn result(&self, n: usize) -> String {
        std::fs::read_to_string(self.dir().join("results").join(format!("{n}.json"))).unwrap_or_default()
    }
    fn set_offline(&self, on: bool) {
        let f = self.dir().join("offline");
        if on {
            std::fs::write(&f, b"1").unwrap();
        } else {
            let _ = std::fs::remove_file(&f);
        }
    }
    fn remote_edit(&self, key: &str) {
        let st = std::process::Command::new(&self.bin)
            .args(["_edit", key])
            .env("ZK_HOME", &self.home)
            .status()
            .unwrap();
        assert!(st.success());
    }
}

const SEED_EMPTY: &str = r#"{"ink":[],"highlights":[],"seq":0,"pushes":0,"upserts":0}"#;

const SEED_REMOTE: &str = r##"{"ink":[{"key":"RMTA2222","version":7,"pageIndex":0,"paths":[[100,300,200,320,400,300]],"width":2.0,"color":"#222222","comment":"","tags":[],"sortIndex":"00000|000000|00300","pageLabel":"1","dirty":false,"deleted":false}],"highlights":[{"key":"HLTA2222","version":5,"type":"highlight","pageIndex":0,"pageLabel":"1","sortIndex":"00000|000120|00210","color":"#ffd400","text":"Normal science","comment":"","tags":[],"rects":[[72,500,300,514]]},{"key":"HLTA2223","version":5,"type":"underline","pageIndex":0,"pageLabel":"1","sortIndex":"00000|000220|00250","color":"#2ea8e5","text":"paradigm","comment":"","tags":[],"rects":[[72,450,300,464]]}],"seq":0,"pushes":0,"upserts":0}"##;

// ---------------------------------------------------------------------------------------------
// harness helpers
// ---------------------------------------------------------------------------------------------

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

/// A Zotero-bound PDF notebook: `pages` document-image pages (distinct papers) + zotero_source.json.
fn make_pdf_nb(name: &str, att_key: &str, pages: u32, base_id: u32) -> NotebookInfo {
    let nb = make_notebook(name, PAPER_BLANK, pages);
    for i in 0..pages {
        let pg = page_new(W, H, paper_image(PAPER_ROLE_DOCUMENT, base_id + i));
        assert_eq!(store_save_page(&nb, i as i32, &pg), 0);
    }
    let src = ZoteroNotebookSource {
        schema_version: 1,
        item_key: format!("ITM{att_key}"),
        attachment_key: att_key.to_string(),
        citation_key: "Test 2026".to_string(),
        title: name.to_string(),
        authors: "A. Tester".to_string(),
        year: "2026".to_string(),
        pdf_local_path: format!("/mnt/us/zotero-bridge/storage/{att_key}/t.pdf"),
        page_count: pages,
        pages: (0..pages).map(|i| make_pdf_page_geo(i, &(i + 1).to_string())).collect(),
    };
    assert!(zotero_source_save(nb.id.as_str(), &src).is_ok());
    nb
}

fn probe(a: &mut App, key: &str) -> String {
    let mut out = String::new();
    assert_eq!(app_probe(a, key, &mut out, 512), 0, "probe {key}");
    out
}

fn probe_n(a: &mut App, key: &str) -> i64 {
    probe(a, key).trim().parse().unwrap_or(-1)
}

fn contact(a: &mut App, type_: InType, x: f32, y: f32, t_ms: u32) {
    app_event(
        a,
        &InEvent { type_, x, y, t_ms, k_ms: t_ms, pressure: 0.5, ..Default::default() },
    );
}

fn draw_sample_ink(a: &mut App, y_start: f32, lines: u32) {
    for line in 0..lines {
        let y = y_start + line as f32 * 90.0;
        let x0 = 260.0;
        let t = now_ms();
        contact(a, InType::PenDown, x0, y, t);
        for k in 1..=36u32 {
            let wave = if (k / 3) % 2 == 0 { 16.0 } else { -16.0 };
            contact(a, InType::PenMove, x0 + 28.0 * k as f32, y + wave, t.wrapping_add(8 * k));
        }
        contact(a, InType::PenUp, x0 + 28.0 * 36.0, y, t.wrapping_add(320));
    }
    app_flush(a);
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
        grimoire::ffi_stb::stbi_write_png(name.as_ptr(), c.w, c.h, 1, c.px.as_ptr() as *const _, c.stride)
    };
    check!(rc != 0);
    let bytes = std::fs::read(path).unwrap_or_default();
    check!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
}

/// Open a notebook at its first page with the editor composed.
fn open_nb(a: &mut App, nb: &NotebookInfo) {
    editor_open_notebook_at(a, nb.id.as_str(), nb.page_ids[0], false);
    app_toast_dismiss(a);
    app_flush(a);
    check!(probe(a, "editor.notebook_id") == nb.id.as_str());
}

/// Sync ink, run to the end on this thread; the toast it left.
fn sync(a: &mut App) -> String {
    app_toast_dismiss(a);
    check!(editor_zotero_control(a, "ink.sync_blocking", ""));
    app_flush(a);
    probe(a, "toast")
}

fn screen_px(a: &App, sx: f32, sy: f32) -> u8 {
    let c = &a.screen;
    let (x, y) = ((sx as i32).clamp(0, c.w - 1), (sy as i32).clamp(0, c.h - 1));
    c.px[(y * c.stride + x) as usize]
}

fn page_screen_px(a: &mut App, px: f64, py: f64) -> u8 {
    let (sx, sy) = editor_replay_point(a, px as f32, py as f32);
    screen_px(a, sx, sy)
}

/// A test body that prints one PASS line when none of its checks failed.
fn run_test(name: &str, f: impl FnOnce()) {
    eprintln!("- {name}");
    let before = FAILURES.load(Ordering::Relaxed);
    f();
    if FAILURES.load(Ordering::Relaxed) == before {
        println!("PASS {name}");
    } else {
        println!("FAIL {name}");
    }
}

// ---------------------------------------------------------------------------------------------
// Test 1: coordinate round trips
// ---------------------------------------------------------------------------------------------

/// A geometry for a page with crop box `crop` (user space) and /Rotate `rot`; user_to_page as PDF viewers build it.
fn geom_for(rot: i32, crop: [f64; 4]) -> ZkPdfPageGeometry {
    let (cx, cy) = (crop[0], crop[1]);
    let (w, h) = (crop[2] - crop[0], crop[3] - crop[1]);
    let (dw, dh, m) = match rot {
        0 => (w, h, [1.0, 0.0, 0.0, -1.0, -cx, h + cy]),
        90 => (h, w, [0.0, 1.0, 1.0, 0.0, -cy, -cx]),
        180 => (w, h, [-1.0, 0.0, 0.0, 1.0, w + cx, -cy]),
        _ => (h, w, [0.0, -1.0, -1.0, 0.0, h + cy, w + cx]),
    };
    ZkPdfPageGeometry {
        page_index: 0,
        width_pt: dw,
        height_pt: dh,
        rotation: rot,
        page_label: "1".to_string(),
        crop_box: crop,
        user_to_page: m,
    }
}

fn test_1_coordinate_round_trip() {
    let a4 = [0.0, 0.0, 595.28, 841.89];
    let letter_l = [0.0, 0.0, 792.0, 612.0];
    let letter = [0.0, 0.0, 612.0, 792.0];
    let cropped = [36.0, 36.0, 576.0, 756.0];
    let cases: Vec<(&str, ZkPdfPageGeometry)> = vec![
        ("A4 portrait", geom_for(0, a4)),
        ("letter landscape", geom_for(0, letter_l)),
        ("rotate 0", geom_for(0, letter)),
        ("rotate 90", geom_for(90, letter)),
        ("rotate 180", geom_for(180, letter)),
        ("rotate 270", geom_for(270, letter)),
        ("crop origin 36,36", geom_for(0, cropped)),
        ("crop origin + rotate 90", geom_for(90, cropped)),
        ("crop origin + rotate 270", geom_for(270, cropped)),
    ];
    for (name, g) in &cases {
        let fit = page_fit_for(g, W, H);
        check!(fit.scale > 0.0);
        let mut worst = 0.0f64;
        for iy in 0..=8 {
            for ix in 0..=8 {
                let q = (g.width_pt * ix as f64 / 8.0, g.height_pt * iy as f64 / 8.0);
                let (px, py) = (fit.ox + q.0 * fit.scale, fit.oy + q.1 * fit.scale);
                let Some((ux, uy)) = grimoire_to_user(g, &fit, px, py) else {
                    check!(false);
                    continue;
                };
                let (bx, by) = user_to_grimoire(g, &fit, ux, uy);
                worst = worst.max((bx - px).abs()).max((by - py).abs());
            }
        }
        if worst >= 0.5 {
            eprintln!("  {name}: worst round-trip error {worst}");
        }
        check!(worst < 0.5);
        // user-space origin of the crop box lands on the expected display corner
        let (cx, cy) = (g.crop_box[0], g.crop_box[1]);
        let (dw, dh) = (g.width_pt, g.height_pt);
        let want = match g.rotation {
            0 => (0.0, dh),
            90 => (0.0, 0.0),
            180 => (dw, 0.0),
            _ => (dw, dh),
        };
        let (gx, gy) = user_to_grimoire(g, &fit, cx, cy);
        let (ex, ey) = (fit.ox + want.0 * fit.scale, fit.oy + want.1 * fit.scale);
        if (gx - ex).abs() >= 0.5 || (gy - ey).abs() >= 0.5 {
            eprintln!("  {name}: origin maps to ({gx},{gy}), want ({ex},{ey})");
        }
        check!((gx - ex).abs() < 0.5 && (gy - ey).abs() < 0.5);
        // the whole page lies inside the Scribe page
        for (qx, qy) in [(0.0, 0.0), (dw, dh)] {
            let (px, py) = (fit.ox + qx * fit.scale, fit.oy + qy * fit.scale);
            check!(px >= -0.5 && py >= -0.5 && px <= W as f64 + 0.5 && py <= H as f64 + 0.5);
        }
        // a rect: bbox of the four mapped corners, never degenerate
        let r = [cx + 40.0, cy + 60.0, cx + 140.0, cy + 110.0];
        let b = rect_user_to_grimoire(g, &fit, r);
        check!(b[2] - b[0] > 1.0 && b[3] - b[1] > 1.0);
        for (ux, uy) in [(r[0], r[1]), (r[2], r[1]), (r[2], r[3]), (r[0], r[3])] {
            let (x, y) = user_to_grimoire(g, &fit, ux, uy);
            check!(x >= b[0] - 1e-6 && x <= b[2] + 1e-6 && y >= b[1] - 1e-6 && y <= b[3] + 1e-6);
        }
        // a 90/270 rotation swaps the rect's sides relative to an unrotated page at the same scale
        if g.rotation == 90 || g.rotation == 270 {
            let (bw, bh) = (b[2] - b[0], b[3] - b[1]);
            check_near!(bw, 50.0 * fit.scale, 0.01);
            check_near!(bh, 100.0 * fit.scale, 0.01);
        } else {
            check_near!(b[2] - b[0], 100.0 * fit.scale, 0.01);
            check_near!(b[3] - b[1], 50.0 * fit.scale, 0.01);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Test 2: simplification, width conversion, exclusion
// ---------------------------------------------------------------------------------------------

fn test_2_simplify_width_exclusion() {
    let g = make_pdf_page_geo(3, "4");
    let fit = page_fit_for(&g, W, H);
    let mut page = page_new(W, H, PAPER_BLANK);

    let mut pen = ToolParams::default();
    tool_params_default(TOOL_FINELINER, &mut pen);
    let mut s = stroke_new(TOOL_FINELINER, &pen, 1);
    for i in 0..500 {
        stroke_append(&mut s, &PenSample { x: 300.0 + i as f32 * 2.0, y: 1000.0, pressure: 0.5, ..Default::default() }, DPI);
    }
    page_add(&mut page, s);

    let mut hl = ToolParams::default();
    tool_params_default(TOOL_HIGHLIGHTER, &mut hl);
    let mut h = stroke_new(TOOL_HIGHLIGHTER, &hl, 2);
    for i in 0..60 {
        stroke_append(&mut h, &PenSample { x: 300.0 + i as f32 * 10.0, y: 1400.0, pressure: 0.5, ..Default::default() }, DPI);
    }
    page_add(&mut page, h);

    let mut er = ToolParams::default();
    tool_params_default(TOOL_ERASER, &mut er);
    let mut e = stroke_new(TOOL_ERASER, &er, 3);
    for i in 0..40 {
        stroke_append(&mut e, &PenSample { x: 300.0 + i as f32 * 10.0, y: 1200.0, pressure: 0.5, ..Default::default() }, DPI);
    }
    page_add(&mut page, e);
    page_add(&mut page, floor_marker_new(1));
    check!(page.strokes.len() == 4);

    let buckets = page_local_buckets(&page, 7, 3, &g, DPI);
    check!(buckets.len() == 2); // eraser + floor marker excluded
    check!(buckets.iter().all(|b| b.page_id == 7 && b.page_index == 3));

    let pen_b = buckets.iter().find(|b| b.color == "#000000");
    let hl_b = buckets.iter().find(|b| b.color == ZOTERO_HIGHLIGHTER_COLOR);
    check!(pen_b.is_some() && hl_b.is_some());
    if let Some(b) = pen_b {
        let paths = b.paths.as_ref().expect("paths loaded");
        check!(paths.len() == 1);
        check!(paths[0].len() == 4); // a dense straight stroke is a 2-point path
        // endpoints are the first/last sample in PDF user space
        let (u0x, u0y) = grimoire_to_user(&g, &fit, 300.0, 1000.0).unwrap();
        let (u1x, u1y) = grimoire_to_user(&g, &fit, 300.0 + 499.0 * 2.0, 1000.0).unwrap();
        check_near!(paths[0][0], u0x, 0.01);
        check_near!(paths[0][1], u0y, 0.01);
        check_near!(paths[0][2], u1x, 0.01);
        check_near!(paths[0][3], u1y, 0.01);
        // width: mm -> px -> pt, rounded to 2 decimals and clamped
        let px = stroke_width_px(&pen, DPI);
        let want = (((px / fit.scale).clamp(0.5, 200.0)) * 100.0).round() / 100.0;
        check_near!(width_px_to_pt(px, &fit), want, 1e-9);
        let quant = ((want / 0.25).round() * 0.25 * 100.0).round() / 100.0;
        check_near!(b.width, quant, 1e-9);
        check!(b.bucket == format!("#000000|{:.2}", b.width));
        check!(b.hash.len() == 16 && b.hash.bytes().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }
    if let Some(b) = hl_b {
        check!(b.bucket.starts_with("#ffd400|"));
        check!(b.paths.as_ref().is_some_and(|p| p.len() == 1 && p[0].len() == 4));
        check!(b.width > 0.5);
    }
    // the width clamp
    check_near!(width_px_to_pt(0.0001, &fit), 0.5, 1e-9);
    check_near!(width_px_to_pt(1.0e9, &fit), 200.0, 1e-9);
}

// ---------------------------------------------------------------------------------------------
// Test 3: end-to-end create / unchanged / update / delete
// ---------------------------------------------------------------------------------------------

fn test_3_e2e_round_trip(a: &mut App, mock: &Mock) {
    mock.reset(SEED_EMPTY);
    let nb = make_pdf_nb("Ink E2E", "ATTINK03", 3, 0x301);
    open_nb(a, &nb);
    check!(probe(a, "editor.zotero.bar.sync").len() > 0);

    // create
    draw_sample_ink(a, 600.0, 2);
    check!(probe_n(a, "editor.strokes") == 2);
    let t = sync(a);
    check!(t.contains("synced") && t.contains('\u{2191}'));
    check!(mock.count("ink-upsert") == 1);
    check!(mock.count("push") == 1);
    let r1 = mock.result(1);
    check!(r1.contains("\"created\": 1"));
    let st = ink_sync_load(nb.id.as_str()).expect("sidecar written");
    check!(st.local.len() == 1);
    let key = st.local.first().map(|b| b.key.clone()).unwrap_or_default();
    check!(key == "GRMA2222");
    check!(st.local[0].page_index == 0 && st.local[0].page_id == nb.page_ids[0]);
    check!(st.attachment_key == "ATTINK03");
    check!(probe_n(a, "editor.zotero.ink.local") == 1);
    check!(probe_n(a, "editor.zotero.ink.remote") == 0);
    check!(mock.upsert_files()[0].contains("\"key\":\"\""));

    // nothing changed: no upsert, no push
    let t = sync(a);
    check!(t == "Zotero: up to date");
    check!(mock.count("ink-upsert") == 1);
    check!(mock.count("push") == 1);

    // more ink with the same pen: the SAME key is updated
    draw_sample_ink(a, 900.0, 1);
    check!(probe_n(a, "editor.strokes") == 3);
    let t = sync(a);
    check!(t.contains("synced"));
    check!(mock.count("ink-upsert") == 2);
    check!(mock.count("push") == 2);
    let ups = mock.upsert_files();
    check!(ups[1].contains(&format!("\"key\":\"{key}\"")));
    let r2 = mock.result(2);
    check!(r2.contains("\"updated\": 1") && r2.contains("\"created\": 0"));
    let st = ink_sync_load(nb.id.as_str()).unwrap();
    check!(st.local.len() == 1 && st.local[0].key == key);

    // erase everything on the page: the key is deleted, the binding goes
    check!(editor_shortcut(a, SHORTCUT_CLEAR_PAGE));
    app_flush(a);
    check!(probe_n(a, "editor.strokes") == 0);
    let t = sync(a);
    check!(t.contains("synced"));
    let ups = mock.upsert_files();
    check!(ups.len() == 3);
    check!(ups[2].contains("\"delete_keys\":[") && ups[2].contains(&key));
    check!(mock.result(3).contains("\"deleted\": 1"));
    check!(mock.count("push") == 3);
    let st = ink_sync_load(nb.id.as_str()).unwrap();
    check!(st.local.is_empty());
    check!(probe_n(a, "editor.zotero.ink.local") == 0);
    // and then it is quiet again
    check!(sync(a) == "Zotero: up to date");
    check!(mock.count("ink-upsert") == 3);
}

// ---------------------------------------------------------------------------------------------
// Test 4: remote highlights + remote ink as non-destructive overlays
// ---------------------------------------------------------------------------------------------

fn test_4_remote_overlays(a: &mut App, mock: &Mock) {
    mock.reset(SEED_REMOTE);
    let nb = make_pdf_nb("Ink Overlays", "ATTINK04", 3, 0x401);
    open_nb(a, &nb);
    draw_sample_ink(a, 1700.0, 1);
    let strokes0 = probe_n(a, "editor.strokes");
    check!(strokes0 == 1);

    let t = sync(a);
    check!(t.contains("synced"));
    check!(probe_n(a, "editor.zotero.ink.remote") == 1);
    check!(probe_n(a, "editor.zotero.ink.highlights") == 2);
    check!(probe_n(a, "editor.zotero.ink.overlay") > 0);
    check!(probe_n(a, "editor.strokes") == strokes0); // overlays never enter Page.strokes
    let st = ink_sync_load(nb.id.as_str()).unwrap();
    check!(st.remote_ink.len() == 1 && st.remote_ink[0].key == "RMTA2222");
    check!(st.remote_highlights.len() == 2);
    check!(st.local.len() == 1);

    // pixels: the highlight washes the page, remote ink is dark
    check!(probe(a, "editor.screen_composed") == "1");
    let g = make_pdf_page_geo(0, "1");
    let fit = page_fit_for(&g, W, H);
    let hr = rect_user_to_grimoire(&g, &fit, [72.0, 500.0, 300.0, 514.0]);
    let (cx, cy) = ((hr[0] + hr[2]) / 2.0, (hr[1] + hr[3]) / 2.0);
    let inside = page_screen_px(a, cx, cy);
    let outside = page_screen_px(a, hr[2] + 400.0, cy);
    if !(inside < outside) {
        eprintln!("  highlight pixel {inside} vs paper {outside}");
    }
    check!(inside < outside && inside < 250);
    check!(outside >= 240);
    let (ix, iy) = user_to_grimoire(&g, &fit, 200.0, 320.0);
    let mut darkest = 255u8;
    for dy in -3..=3 {
        for dx in -3..=3 {
            darkest = darkest.min(page_screen_px(a, ix + dx as f64, iy + dy as f64));
        }
    }
    if darkest >= 100 {
        eprintln!("  remote ink darkest pixel {darkest}");
    }
    check!(darkest < 100);
    check!(darkest < inside);

    // later syncs never send the remote key back
    draw_sample_ink(a, 1800.0, 1);
    let t = sync(a);
    check!(t.contains("synced"));
    let ups = mock.upsert_files();
    check!(ups.len() == 2);
    for u in &ups {
        check!(!u.contains("RMTA2222") && !u.contains("HLTA2222") && !u.contains("HLTA2223"));
    }
    check!(probe_n(a, "editor.zotero.ink.remote") == 1);
    check!(probe_n(a, "editor.strokes") == 2);

    // reopening reads the cached overlays without any network
    let pulls = mock.count("ink-pull");
    open_nb(a, &nb);
    check!(probe_n(a, "editor.zotero.ink.highlights") == 2);
    check!(probe_n(a, "editor.zotero.ink.overlay") > 0);
    check!(mock.count("ink-pull") == pulls);
}

// ---------------------------------------------------------------------------------------------
// Test 5: conflict -> remote version preserved, local ink gets a fresh key
// ---------------------------------------------------------------------------------------------

fn test_5_conflict(a: &mut App, mock: &Mock) {
    mock.reset(SEED_EMPTY);
    let nb = make_pdf_nb("Ink Conflict", "ATTINK05", 3, 0x501);
    open_nb(a, &nb);
    draw_sample_ink(a, 700.0, 1);
    check!(sync(a).contains("synced"));
    let st = ink_sync_load(nb.id.as_str()).unwrap();
    check!(st.local.len() == 1);
    let k1 = st.local[0].key.clone();
    check!(k1 == "GRMA2222");

    mock.remote_edit(&k1); // edited in Zotero meanwhile
    draw_sample_ink(a, 1000.0, 1); // and edited here
    let t = sync(a);
    if !t.contains("conflict preserved") {
        eprintln!("  conflict toast: {t:?}");
    }
    check!(t.contains("conflict preserved"));
    check!(probe_n(a, "editor.zotero.ink.remote") == 1);
    let st = ink_sync_load(nb.id.as_str()).unwrap();
    check!(st.remote_ink.iter().any(|r| r.key == k1));
    check!(st.local.len() == 1);
    check!(st.local[0].key != k1 && st.local[0].key == "GRMA2223");
    check!(probe_n(a, "editor.zotero.ink.overlay") > 0);
    // the preserved remote key is never uploaded as ours
    let ups = mock.upsert_files();
    check!(ups.len() == 2);
    check!(ups[1].contains("\"key\":\"\"") && !ups[1].contains(&format!("\"key\":\"{k1}\"")));
    // Zotero now holds both
    let state = std::fs::read_to_string(mock.dir().join("state.json")).unwrap();
    check!(state.contains("GRMA2222") && state.contains("GRMA2223"));
    check!(probe_n(a, "editor.strokes") == 2);
    check!(sync(a) == "Zotero: up to date");
}

// ---------------------------------------------------------------------------------------------
// Test 6: offline -> saved locally, no secret leaks
// ---------------------------------------------------------------------------------------------

fn test_6_offline(a: &mut App, mock: &Mock) {
    mock.reset(SEED_EMPTY);
    mock.set_offline(true);
    let nb = make_pdf_nb("Ink Offline", "ATTINK06", 3, 0x601);
    open_nb(a, &nb);
    draw_sample_ink(a, 700.0, 2);
    let t = sync(a);
    check!(t == "Offline \u{2014} saved locally");
    check!(!t.contains("SECRET123") && !t.contains("apiKey"));
    check!(probe_n(a, "editor.strokes") == 2);
    check!(mock.count("push") == 0);
    check!(mock.count("ink-pull") >= 2 && mock.calls().iter().any(|l| l.contains("--local-only")));
    check!(mock.count("ink-upsert") == 1);
    let st = ink_sync_load(nb.id.as_str()).expect("sidecar written offline");
    check!(st.local.len() == 1);
    let sidecar = std::fs::read_to_string(
        Path::new(&std::env::var("GRIMOIRE_TEST_STORE").unwrap_or_default())
            .join("notebooks")
            .join(nb.id.as_str())
            .join("zotero_ink_sync.json"),
    )
    .unwrap_or_default();
    check!(sidecar.contains("\"attachment_key\""));
    check!(!sidecar.contains("SECRET123"));
    check!(!probe(a, "toast").contains("SECRET123"));

    // back online: the dirty local annotation goes up
    mock.set_offline(false);
    let t = sync(a);
    check!(!t.contains("Offline"));
    check!(mock.count("push") == 1);
    check!(mock.count("ink-upsert") == 1); // nothing more to write
    check!(probe_n(a, "editor.strokes") == 2);
}

// ---------------------------------------------------------------------------------------------
// Test 7: layout invariants + screenshots (Gothic / Classic)
// ---------------------------------------------------------------------------------------------

fn test_7_bar_layout_and_screenshots(a: &mut App, mock: &Mock) {
    let shot_dir = std::env::var_os("GRIMOIRE_PHASE4_SHOT_DIR").filter(|d| !d.is_empty()).map(PathBuf::from);
    if let Some(ref d) = shot_dir {
        std::fs::create_dir_all(d).unwrap();
    }
    mock.reset(SEED_REMOTE);
    let essay = make_notebook("Philosophy Essay", PAPER_WIDE, 2);
    let nb = make_pdf_nb("Structure of Revolutions", "ATTINK07", 3, 0x701);
    open_nb(a, &nb);
    draw_sample_ink(a, 1450.0, 3);
    check!(sync(a).contains("synced"));

    let orig_theme = ui_theme();
    let orig_status_on = a.settings.status_on;
    let orig_status_corner = a.settings.status_corner;
    a.settings.status_on = true;
    for (theme, suffix) in [(UiTheme::Gothic, "gothic"), (UiTheme::Classic, "classic")] {
        a.settings.theme = theme as i32;
        ui_set_theme(theme);
        for corner in [0, 1] {
            a.settings.status_corner = corner;
            editor_open_notebook_at(a, essay.id.as_str(), essay.page_ids[0], false);
            app_flush(a);
            zotero_open_bound_notebook(
                a,
                nb.id.as_str(),
                1,
                Some(ReturnPlace {
                    notebook_id: essay.id.to_string(),
                    page_id: essay.page_ids[0],
                    page_index: 0,
                    ..Default::default()
                }),
            );
            app_toast_dismiss(a);
            editor_zotero_bar_refresh(a);
            status_tick(a, now_ms());
            app_flush(a);

            CUTS.lock().unwrap().clear();
            check!(probe(a, "editor.screen_composed") == "1");
            assert_no_overflow(theme, "PDF bar with Sync");
            check!(probe(a, "editor.zotero.bar") == "1");
            check!(probe_n(a, "editor.zotero.ink.highlights") == 2);
            check!(probe_n(a, "editor.zotero.ink.remote") == 1);
            check!(probe_n(a, "editor.zotero.ink.overlay") > 0);
            check!(probe_n(a, "editor.strokes") == 3);

            let back = parse_rect_csv(&probe(a, "editor.zotero.bar.back"));
            let cite = parse_rect_csv(&probe(a, "editor.zotero.bar.cite"));
            let sync_r = parse_rect_csv(&probe(a, "editor.zotero.bar.sync"));
            let bar = rect_union(rect_union(back, cite), sync_r);
            let panel = parse_rect_ws(&probe(a, "editor.panel_rect"));
            let status = status_zone(a);
            check!(!rect_is_empty(back) && !rect_is_empty(cite) && !rect_is_empty(sync_r));
            check!(!rect_overlaps(back, cite));
            check!(!rect_overlaps(back, sync_r));
            check!(!rect_overlaps(cite, sync_r));
            check!(bar.x >= 0 && bar.y >= 0 && bar.x + bar.w <= W && bar.y + bar.h <= H);
            check!(!rect_overlaps(bar, status));
            check!(!rect_overlaps(bar, panel));
            check!(editor_zotero_bar_point(a, "sync").is_some());
            check!(editor_zotero_bar_point(a, "back").is_some() && editor_zotero_bar_point(a, "cite").is_some());

            if corner == 0 {
                if let Some(ref dir) = shot_dir {
                    save_png(a, &dir.join(format!("phase4_zotero_ink_{suffix}.png")));
                }
            }
        }
    }
    a.settings.theme = orig_theme as i32;
    ui_set_theme(orig_theme);
    a.settings.status_on = orig_status_on;
    a.settings.status_corner = orig_status_corner;
}

fn main() {
    if std::env::args().any(|a| a == "--list") {
        println!("zotero_ink_sync: test");
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
    write_mock_zk_sync(&bin_path);
    std::env::set_var("GRIMOIRE_TEST_STORE", &store);
    let mock = Mock { bin: bin_path.clone(), home: zk_home.clone() };

    let bridge_str = bridge.to_string_lossy().into_owned();
    let store_str = store.to_string_lossy().into_owned();
    std::env::set_var("GRIMOIRE_SCRIPT", "/dev/null");
    bridge_set_dir(Some(&bridge_str));
    zk_sync_set_test_config(Some(ZkSyncConfig {
        bin: bin_path.to_string_lossy().into_owned(),
        zk_home: zk_home.to_string_lossy().into_owned(),
        zk_config_dir: zk_cfg.to_string_lossy().into_owned(),
        local_timeout_ms: 10_000,
        network_timeout_ms: 10_000,
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

    run_test("test_1_coordinate_round_trip", test_1_coordinate_round_trip);
    run_test("test_2_simplify_width_exclusion", test_2_simplify_width_exclusion);
    run_test("test_3_e2e_round_trip", || test_3_e2e_round_trip(&mut a, &mock));
    run_test("test_4_remote_overlays", || test_4_remote_overlays(&mut a, &mock));
    run_test("test_5_conflict", || test_5_conflict(&mut a, &mock));
    run_test("test_6_offline", || test_6_offline(&mut a, &mock));
    run_test("test_7_bar_layout_and_screenshots", || test_7_bar_layout_and_screenshots(&mut a, &mock));

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
    std::env::remove_var("GRIMOIRE_TEST_STORE");
    let _ = std::fs::remove_dir_all(&root);
    done();
}

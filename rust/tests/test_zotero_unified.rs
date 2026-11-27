//! Checkpoint 1 (`docs/unified-ink-sync-contract.md`): enriched `library_index.json` parsing and
//! collection/query filtering, headless `zk-sync` subprocess wrapper & Zero-Credential scrubbing,
//! `pdf-info` geometry inspection/fetch, and `zotero_source.json` notebook sidecar + deduplication.
//!
//! Phase 2 (`mod phase2`, `--features sim`): the unified reader end to end on a 1860 x 2480 panel at
//! 300 DPI, with real PDF imports through `grimoire-headless --work` and a mock `zk-sync`.

#[cfg(feature = "sim")]
mod pdffix;
mod testlib;

use grimoire::prelude::*;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use testlib::*;

const W: i32 = 1860;
const H: i32 = 2480;
const DPI: f32 = 300.0;

fn scratch_root() -> PathBuf {
    let base = std::env::var("GRIMOIRE_TEST_TMP")
        .unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().into_owned());
    let root = PathBuf::from(base).join(format!("zotero-unified-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

/// `store_save_info` stamps `modified` with the wall clock (1 s resolution), so notebooks made in one
/// second tie. Write the wanted time into `notebook.ini` itself, where `store_list` reads it.
fn set_modified(store: &str, nb: &mut NotebookInfo, modified: i64) {
    let path = Path::new(store).join("notebooks").join(nb.id.as_str()).join("notebook.ini");
    let ini = std::fs::read_to_string(&path).unwrap();
    let out: String = ini
        .lines()
        .map(|l| if l.starts_with("modified=") { format!("modified={modified}\n") } else { format!("{l}\n") })
        .collect();
    std::fs::write(&path, out).unwrap();
    let id = nb.id.as_str().to_string();
    assert_eq!(store_load_info(&id, nb), 0);
    assert_eq!(nb.modified, modified);
}

fn make_notebook(store: &str, name: &str, pages: u32, modified: i64) -> NotebookInfo {
    let mut nb = NotebookInfo::default();
    assert_eq!(store_create(name, "", PAPER_BLANK, &mut nb), 0);
    nb.page_ids = (1..=pages).collect();
    nb.next_page_id = pages + 1;
    assert_eq!(store_save_info(&mut nb), 0);
    set_modified(store, &mut nb, modified);
    nb
}

fn write_mock_zk_sync(bin_path: &Path) {
    let script = r#"#!/usr/bin/env python3
import json
import os
import sys

args = sys.argv[1:]
zk_home = os.environ.get("ZK_HOME", "")
zk_cfg = os.environ.get("ZK_CONFIG_DIR", "")
no_upd = os.environ.get("ZK_NO_UPDATES", "")

os.makedirs(zk_home, exist_ok=True)
log_path = os.path.join(zk_home, "invocations.jsonl")
with open(log_path, "a", encoding="utf-8") as lf:
    lf.write(json.dumps({
        "args": args,
        "zk_home": zk_home,
        "zk_config_dir": zk_cfg,
        "no_updates": no_upd,
    }) + "\n")

mode_path = os.path.join(zk_home, "mode.txt")
mode = "ok"
if os.path.exists(mode_path):
    with open(mode_path, "r", encoding="utf-8") as mf:
        mode = mf.read().strip()

if not args:
    sys.stderr.write("missing subcommand\n")
    sys.exit(2)

cmd = args[0]

if cmd == "pull":
    if mode == "pull_fallback_sync":
        sys.stderr.write("unknown command pull\n")
        sys.exit(2)
    if mode == "offline_leak":
        sys.stderr.write(
            "WebDAV PROPFIND https://alice:s3cr3tPass@dav.example.com/zotero/?token=tok999 "
            "failed; apiKey=ZOT_SECRET_KEY_111 ZK_WEBDAV_PASS=\"dav_pw_222\"\n"
        )
        sys.stdout.write("Bearer eyJ_BEARER_SECRET_333 ZK_ZOTERO_API_KEY: ZOT_KEY_444\n")
        sys.exit(1)
    with open(os.path.join(zk_home, "remote_pulled.flag"), "w", encoding="utf-8") as f:
        f.write("pulled\n")
    sys.stdout.write('{"status":"ok"}\n')
    sys.exit(0)

if cmd == "sync":
    if mode == "offline_leak":
        sys.stderr.write("sync failed https://alice:s3cr3tPass@dav.example.com/zotero apiKey=ZOT_SECRET_KEY_111\n")
        sys.exit(1)
    with open(os.path.join(zk_home, "remote_pulled.flag"), "w", encoding="utf-8") as f:
        f.write("synced\n")
    sys.stdout.write('{"status":"ok"}\n')
    sys.exit(0)

if cmd == "export-index":
    if len(args) < 2:
        sys.stderr.write("missing export path\n")
        sys.exit(2)
    dest = args[1]
    if mode == "export_fail_leak":
        sys.stderr.write("export error: password=MY_DB_PASS_555 https://bob:topsecret@dav.example.com/dav\n")
        sys.exit(3)
    cached_pdf = os.path.join(zk_home, "storage", "ATTCACH1", "kuhn1962.pdf")
    items = [
        {
            "item_key": "ABCD1234",
            "attachment_key": "ATTCACH1",
            "citation_key": "kuhn1962",
            "title": "The Structure of Scientific Revolutions",
            "authors": "Thomas S. Kuhn",
            "year": "1962",
            "item_type": "book",
            "collections": ["Philosophy of Science"],
            "collection_keys": ["COLL0001"],
            "tags": ["epistemology", "paradigm"],
            "has_pdf": True,
            "has_epub": False,
            "has_snapshot": False,
            "pdf_cached": os.path.isfile(cached_pdf),
            "pdf_local_path": cached_pdf if os.path.isfile(cached_pdf) else "",
            "last_opened": 1732400100,
        }
    ]
    if os.path.exists(os.path.join(zk_home, "remote_pulled.flag")):
        items.append({
            "item_key": "PULL9999",
            "attachment_key": "ATTUNC01",
            "citation_key": "smith2024",
            "title": "Freshly Pulled Quantum Notes",
            "authors": "J. Smith",
            "year": "2024",
            "item_type": "journalArticle",
            "collections": ["Physics"],
            "collection_keys": ["COLL0002"],
            "tags": ["quantum"],
            "has_pdf": True,
            "has_epub": False,
            "has_snapshot": False,
            "pdf_cached": False,
            "pdf_local_path": "",
            "last_opened": 0,
        })
    doc = {
        "schema_version": 1,
        "exported_at": 1732400500,
        "item_count": len(items),
        "collections": [
            {"key": "COLL0001", "name": "Philosophy of Science", "parent_key": ""},
            {"key": "COLL0002", "name": "Physics", "parent_key": ""},
        ],
        "items": items,
    }
    tmp = dest + ".tmp." + str(os.getpid())
    os.makedirs(os.path.dirname(dest), exist_ok=True)
    with open(tmp, "w", encoding="utf-8") as out:
        json.dump(doc, out)
    os.replace(tmp, dest)
    sys.stdout.write(json.dumps({
        "schema_version": 1,
        "exported_at": 1732400500,
        "item_count": len(items),
    }) + "\n")
    sys.exit(0)

if cmd == "pdf-info":
    if len(args) < 2:
        sys.stderr.write("missing attachment key\n")
        sys.exit(2)
    att_key = args[1]
    do_fetch = "--fetch" in args[2:]

    if do_fetch and mode == "offline_leak":
        sys.stderr.write(
            "fetch https://user:secret@dav.example.com/zotero/" + att_key + ".zip?sig=sig777 "
            "failed: network unreachable (apiKey=FETCH_KEY_888)\n"
        )
        sys.exit(1)

    sample_pages = [
        {
            "page_index": 0,
            "width_pt": 612.0,
            "height_pt": 792.0,
            "rotation": 0,
            "page_label": "i",
            "crop_box": [0.0, 0.0, 612.0, 792.0],
            "user_to_page": [1.0, 0.0, 0.0, -1.0, 0.0, 792.0],
        },
        {
            "page_index": 1,
            "width_pt": 792.0,
            "height_pt": 612.0,
            "rotation": 90,
            "page_label": "1",
            "crop_box": [18.0, 24.0, 594.0, 768.0],
            "user_to_page": [0.0, 1.0, 1.0, 0.0, -24.0, -18.0],
        },
    ]

    if att_key == "ATTCACH1":
        pdf_path = os.path.join(zk_home, "storage", "ATTCACH1", "kuhn1962.pdf")
        out = {
            "schema_version": 1,
            "attachment_key": "ATTCACH1",
            "parent_item_key": "ABCD1234",
            "filename": "kuhn1962.pdf",
            "cached": True,
            "fetched": False,
            "local_path": pdf_path,
            "page_count": 2,
            "pages": sample_pages,
        }
        sys.stdout.write(json.dumps(out) + "\n")
        sys.exit(0)

    if att_key == "ATTUNC01":
        pdf_path = os.path.join(zk_home, "storage", "ATTUNC01", "smith2024.pdf")
        fetched_now = False
        if do_fetch and not os.path.exists(pdf_path):
            os.makedirs(os.path.dirname(pdf_path), exist_ok=True)
            with open(pdf_path, "wb") as pf:
                pf.write(b"%PDF-1.4\n1 0 obj<<>>endobj\ntrailer<<>>\n%%EOF\n")
            fetched_now = True
        is_cached = os.path.isfile(pdf_path)
        out = {
            "schema_version": 1,
            "attachment_key": "ATTUNC01",
            "parent_item_key": "PULL9999",
            "filename": "smith2024.pdf",
            "cached": is_cached,
            "fetched": fetched_now,
            "local_path": pdf_path if is_cached else "",
            "page_count": 2 if is_cached else 0,
            "pages": sample_pages if is_cached else [],
        }
        sys.stdout.write(json.dumps(out) + "\n")
        sys.exit(0)

    if att_key == "ATTSYML1":
        sym_path = os.path.join(zk_home, "storage", "ATTSYML1", "linked.pdf")
        out = {
            "schema_version": 1,
            "attachment_key": "ATTSYML1",
            "parent_item_key": "ABCD1234",
            "filename": "linked.pdf",
            "cached": True,
            "fetched": False,
            "local_path": sym_path,
            "page_count": 1,
            "pages": [sample_pages[0]],
        }
        sys.stdout.write(json.dumps(out) + "\n")
        sys.exit(0)

    sys.stderr.write("unknown attachment " + att_key + "\n")
    sys.exit(1)

sys.stderr.write("unknown command " + cmd + "\n")
sys.exit(2)
"#;
    if let Some(parent) = bin_path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(bin_path, script.as_bytes()).unwrap();
    let mut perms = std::fs::metadata(bin_path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(bin_path, perms).unwrap();
}

fn set_mock_mode(zk_home: &Path, mode: &str) {
    std::fs::create_dir_all(zk_home).unwrap();
    std::fs::write(zk_home.join("mode.txt"), mode.as_bytes()).unwrap();
}

fn take_invocations(zk_home: &Path) -> Vec<String> {
    let p = zk_home.join("invocations.jsonl");
    let Ok(s) = std::fs::read_to_string(&p) else {
        return Vec::new();
    };
    let _ = std::fs::remove_file(&p);
    s.lines().map(|l| l.to_string()).collect()
}

fn test_enriched_index_and_filter(bridge: &str) {
    let raw = r#"{
        "schema_version": 1,
        "exported_at": 1732400000,
        "item_count": 4,
        "collections": [
            {"key": "COLL0001", "name": "Philosophy of Science", "parent_key": null},
            {"key": "COLL0002", "name": "Epistemology", "parent_key": "COLL0001"},
            {"key": "bad/key", "name": "Ignored Bad Key", "parent_key": ""}
        ],
        "items": [
            {
                "item_key": "ABCD1234",
                "attachment_key": "ATTCACH1",
                "citation_key": "kuhn1962",
                "title": "The Structure of Scientific Revolutions",
                "authors": "Thomas S. Kuhn",
                "year": "1962",
                "item_type": "book",
                "collections": ["Philosophy of Science", "Epistemology"],
                "collection_keys": ["COLL0001", "COLL0002", "bad/coll"],
                "tags": ["paradigm", "history"],
                "has_pdf": true,
                "has_epub": false,
                "has_snapshot": false,
                "pdf_cached": true,
                "pdf_local_path": "/mnt/us/zotero-bridge/storage/ATTCACH1/kuhn1962.pdf",
                "last_opened": 1732399900
            },
            {
                "item_key": "MINV1001",
                "title": "Minimal V1 Legacy Entry",
                "authors": "Karl Popper",
                "year": "1959",
                "collections": ["Philosophy of Science"],
                "has_pdf": false
            },
            {
                "item_key": "BADPATH1",
                "attachment_key": "ATTBAD01",
                "title": "Corrupt Local Path Entry",
                "authors": "Imre Lakatos",
                "year": "1970",
                "collection_keys": ["COLL0002"],
                "collections": ["Epistemology"],
                "has_pdf": true,
                "pdf_cached": true,
                "pdf_local_path": "/bad/path\ninjected.pdf",
                "last_opened": -5
            }
        ]
    }"#;

    let idx = library_index_parse(raw.as_bytes()).expect("enriched library_index.json must parse");
    check!(idx.schema_version == SCHEMA_VERSION);
    check!(idx.exported_at == 1732400000);
    check!(idx.item_count == 4);

    // Collections: valid entries kept, null parent_key normalized to "", invalid key skipped.
    check!(idx.collections.len() == 2);
    check!(idx.collections[0].key == "COLL0001");
    check!(idx.collections[0].name == "Philosophy of Science");
    check!(idx.collections[0].parent_key.is_empty());
    check!(idx.collections[1].key == "COLL0002");
    check!(idx.collections[1].parent_key == "COLL0001");

    // Item 0: enriched fields preserved, invalid collection_key entry filtered out.
    check!(idx.items.len() == 3);
    let kuhn = &idx.items[0];
    check!(kuhn.item_key == "ABCD1234");
    check!(kuhn.attachment_key == "ATTCACH1");
    check!(kuhn.collection_keys == vec!["COLL0001".to_string(), "COLL0002".to_string()]);
    check!(kuhn.pdf_cached);
    check!(kuhn.pdf_local_path == "/mnt/us/zotero-bridge/storage/ATTCACH1/kuhn1962.pdf");
    check!(kuhn.last_opened == 1732399900);

    // Item 1: backwards compatibility with minimal v1 schema items.
    let popper = &idx.items[1];
    check!(popper.item_key == "MINV1001");
    check!(popper.attachment_key.is_empty());
    check!(popper.collection_keys.is_empty());
    check!(!popper.pdf_cached);
    check!(popper.pdf_local_path.is_empty());
    check!(popper.last_opened == 0);

    // Item 2: control byte in pdf_local_path clears path & resets pdf_cached; negative last_opened -> 0.
    let lakatos = &idx.items[2];
    check!(lakatos.item_key == "BADPATH1");
    check!(!lakatos.pdf_cached);
    check!(lakatos.pdf_local_path.is_empty());
    check!(lakatos.last_opened == 0);

    // Filtering by collection key, collection name, and multi-term queries.
    check!(library_index_filter(&idx, "", "").len() == 3);
    let by_key_c1 = library_index_filter(&idx, "", "COLL0001");
    check!(by_key_c1.len() == 1 && by_key_c1[0].item_key == "ABCD1234");
    check!(library_index_filter(&idx, "", "coll0001").is_empty()); // collection_keys are case-sensitive

    let by_name = library_index_filter(&idx, "", "philosophy of science");
    check!(by_name.len() == 2); // matches both Kuhn and minimal v1 Popper by collection name!

    let by_key_and_query = library_index_filter(&idx, "lakatos 1970", "COLL0002");
    check!(by_key_and_query.len() == 1 && by_key_and_query[0].item_key == "BADPATH1");

    let by_multi_terms = library_index_filter(&idx, "kuhn paradigm 1962", "Epistemology");
    check!(by_multi_terms.len() == 1 && by_multi_terms[0].item_key == "ABCD1234");
    check!(library_index_filter(&idx, "kuhn nonexistent", "COLL0001").is_empty());

    // Arc cache hit & invalidation via library_index_load / library_index_reload.
    bridge_set_dir(Some(bridge));
    let index_path = bridge_path_in(bridge, LIBRARY_INDEX_FILE).unwrap();
    assert_eq!(bridge_write_atomic(&index_path, raw.as_bytes()), 0);

    let gen_before = zotero_index_generation();
    let loaded1 = library_index_load().expect("library_index_load returns Some(Arc)");
    let gen_after_load = zotero_index_generation();
    check!(gen_after_load > gen_before);
    let loaded2 = library_index_load().expect("cached load returns Some(Arc)");
    check!(Arc::ptr_eq(&loaded1, &loaded2));
    check!(zotero_index_generation() == gen_after_load);

    let reloaded = library_index_reload().expect("reload returns fresh Arc");
    check!(!Arc::ptr_eq(&loaded1, &reloaded));
    check!(zotero_index_generation() > gen_after_load);
    check!(*reloaded == idx);
}

fn test_refresh_and_credential_scrubbing(root: &Path, bridge: &str, zk_home: &Path, bin_path: &Path) {
    let zk_cfg_dir = root.join("zk-config");
    std::fs::create_dir_all(&zk_cfg_dir).unwrap();

    // Seed cached PDF file for ATTCACH1 so export-index marks pdf_cached = true.
    let cached_pdf = zk_home.join("storage").join("ATTCACH1").join("kuhn1962.pdf");
    std::fs::create_dir_all(cached_pdf.parent().unwrap()).unwrap();
    std::fs::write(&cached_pdf, b"%PDF-1.4\n%%EOF\n").unwrap();

    bridge_set_dir(Some(bridge));
    zk_sync_set_test_config(Some(ZkSyncConfig {
        bin: bin_path.to_string_lossy().into_owned(),
        zk_home: zk_home.to_string_lossy().into_owned(),
        zk_config_dir: zk_cfg_dir.to_string_lossy().into_owned(),
        local_timeout_ms: 5_000,
        network_timeout_ms: 5_000,
    }));
    check!(zk_sync_available());

    // 1. zk_sync_refresh_index(false): runs export-index ONLY (no pull/sync).
    let _ = take_invocations(zk_home);
    let _ = std::fs::remove_file(zk_home.join("remote_pulled.flag"));
    set_mock_mode(zk_home, "ok");

    let idx_local = zk_sync_refresh_index(false).expect("local refresh succeeds");
    check!(idx_local.items.len() == 1);
    check!(idx_local.items[0].item_key == "ABCD1234");
    check!(idx_local.items[0].pdf_cached);
    let calls_local = take_invocations(zk_home);
    check!(calls_local.len() == 1);
    check!(calls_local[0].contains("\"export-index\""));
    check!(!calls_local[0].contains("\"pull\""));
    check!(calls_local[0].contains("\"no_updates\": \"1\""));

    // 2. zk_sync_refresh_index(true): runs pull + export-index.
    let idx_remote = zk_sync_refresh_index(true).expect("remote refresh succeeds");
    check!(idx_remote.items.len() == 2);
    check!(library_index_find(&idx_remote, "PULL9999").is_some());
    let calls_remote = take_invocations(zk_home);
    check!(calls_remote.len() == 2);
    check!(calls_remote[0].contains("\"pull\""));
    check!(calls_remote[1].contains("\"export-index\""));

    // 3. Fallback when pull exits 2 -> invokes sync + export-index.
    let _ = std::fs::remove_file(zk_home.join("remote_pulled.flag"));
    set_mock_mode(zk_home, "pull_fallback_sync");
    let idx_fallback = zk_sync_refresh_index(true).expect("fallback sync succeeds");
    check!(idx_fallback.items.len() == 2);
    let calls_fb = take_invocations(zk_home);
    check!(calls_fb.len() == 3);
    check!(calls_fb[0].contains("\"pull\""));
    check!(calls_fb[1].contains("\"sync\""));
    check!(calls_fb[2].contains("\"export-index\""));

    // 4. Offline failure & Zero-Credential Invariant scrubbing on stderr + stdout.
    set_mock_mode(zk_home, "offline_leak");
    let err = zk_sync_refresh_index(true).expect_err("offline pull must fail");
    let msg = err.message();
    check!(matches!(err, ZkSyncError::SyncFailed(_)));
    check!(msg.contains("[REDACTED]"));
    check!(msg.contains("[REDACTED]@dav.example.com/zotero/?[REDACTED]"));
    check!(!msg.contains("alice"));
    check!(!msg.contains("s3cr3tPass"));
    check!(!msg.contains("tok999"));
    check!(!msg.contains("ZOT_SECRET_KEY_111"));
    check!(!msg.contains("dav_pw_222"));
    check!(!msg.contains("eyJ_BEARER_SECRET_333"));
    check!(!msg.contains("ZOT_KEY_444"));
    check!(msg.len() <= ZK_ERROR_MAX_BYTES);

    // Also check export-index failure scrubbing.
    set_mock_mode(zk_home, "export_fail_leak");
    let exp_err = zk_sync_refresh_index(false).expect_err("export failure must return CommandFailed");
    let exp_msg = exp_err.message();
    check!(matches!(exp_err, ZkSyncError::CommandFailed { code: 3, .. }));
    check!(exp_msg.contains("[REDACTED]"));
    check!(!exp_msg.contains("MY_DB_PASS_555"));
    check!(!exp_msg.contains("bob"));
    check!(!exp_msg.contains("topsecret"));

    // Direct unit checks on scrub_error_message edge cases.
    let direct = scrub_error_message(
        "Error: apiKey=RAW_KEY_999 https://user:secret@dav.example.com/path/file.pdf?token=abc",
    );
    check!(direct.contains("apiKey=[REDACTED]"));
    check!(direct.contains("https://[REDACTED]@dav.example.com/path/file.pdf?[REDACTED]"));
    check!(!direct.contains("RAW_KEY_999") && !direct.contains("user") && !direct.contains("secret") && !direct.contains("abc"));

    set_mock_mode(zk_home, "ok");
    let _ = take_invocations(zk_home);
}

fn test_pdf_info_and_bg_worker(root: &Path, zk_home: &Path) {
    set_mock_mode(zk_home, "ok");
    let _ = take_invocations(zk_home);

    // Reject option-injection or invalid attachment keys before spawning.
    check!(matches!(
        zk_sync_pdf_info("-inject", false),
        Err(ZkSyncError::InvalidArgument(_))
    ));
    check!(matches!(
        zk_sync_pdf_info("bad/key", true),
        Err(ZkSyncError::InvalidArgument(_))
    ));
    check!(take_invocations(zk_home).is_empty());

    // 1. Cached PDF without --fetch -> full pages geometry including rotated page user_to_page.
    let cached_info = zk_sync_pdf_info("ATTCACH1", false).expect("cached pdf-info succeeds");
    check!(cached_info.cached && !cached_info.fetched);
    check!(cached_info.attachment_key == "ATTCACH1");
    check!(cached_info.parent_item_key == "ABCD1234");
    check!(cached_info.filename == "kuhn1962.pdf");
    check!(cached_info.page_count == 2 && cached_info.pages.len() == 2);

    let p0 = &cached_info.pages[0];
    check!(p0.page_index == 0 && p0.rotation == 0 && p0.page_label == "i");
    check_near!(p0.width_pt, 612.0, 1e-6);
    check_near!(p0.height_pt, 792.0, 1e-6);
    check!(p0.crop_box == [0.0, 0.0, 612.0, 792.0]);
    check!(p0.user_to_page == [1.0, 0.0, 0.0, -1.0, 0.0, 792.0]);
    let (qx0, qy0) = p0.user_to_display(100.0, 700.0);
    check_near!(qx0, 100.0, 1e-6);
    check_near!(qy0, 92.0, 1e-6);
    let (ux0, uy0) = p0.display_to_user(qx0, qy0).expect("invertible p0");
    check_near!(ux0, 100.0, 1e-6);
    check_near!(uy0, 700.0, 1e-6);

    let p1 = &cached_info.pages[1];
    check!(p1.page_index == 1 && p1.rotation == 90 && p1.page_label == "1");
    check_near!(p1.width_pt, 792.0, 1e-6);
    check_near!(p1.height_pt, 612.0, 1e-6);
    check!(p1.crop_box == [18.0, 24.0, 594.0, 768.0]);
    check!(p1.user_to_page == [0.0, 1.0, 1.0, 0.0, -24.0, -18.0]);
    let (qx1, qy1) = p1.user_to_display(50.0, 120.0);
    check_near!(qx1, 96.0, 1e-6);
    check_near!(qy1, 32.0, 1e-6);
    let (ux1, uy1) = p1.display_to_user(qx1, qy1).expect("invertible rotated p1");
    check_near!(ux1, 50.0, 1e-6);
    check_near!(uy1, 120.0, 1e-6);

    // 2. Uncached PDF without --fetch -> cached: false, empty pages.
    let uncached_pdf = zk_home.join("storage").join("ATTUNC01").join("smith2024.pdf");
    let _ = std::fs::remove_file(&uncached_pdf);
    let _ = take_invocations(zk_home);
    let uncached_info = zk_sync_pdf_info("ATTUNC01", false).expect("uncached pdf-info succeeds");
    check!(!uncached_info.cached && !uncached_info.fetched);
    check!(uncached_info.local_path.is_empty());
    check!(uncached_info.page_count == 0 && uncached_info.pages.is_empty());
    let calls_no_fetch = take_invocations(zk_home);
    check!(calls_no_fetch.len() == 1 && !calls_no_fetch[0].contains("--fetch"));

    // 3. Offline --fetch failure -> clean scrubbed ZkSyncError.
    set_mock_mode(zk_home, "offline_leak");
    let fetch_err = zk_sync_pdf_info("ATTUNC01", true).expect_err("offline --fetch must fail");
    let fetch_msg = fetch_err.message();
    check!(matches!(fetch_err, ZkSyncError::CommandFailed { code: 1, .. }));
    check!(fetch_msg.contains("[REDACTED]"));
    check!(!fetch_msg.contains("secret") && !fetch_msg.contains("sig777") && !fetch_msg.contains("FETCH_KEY_888"));
    check!(!uncached_pdf.exists());

    // 4. Online --fetch success -> creates PDF on disk and returns geometry.
    set_mock_mode(zk_home, "ok");
    let _ = take_invocations(zk_home);
    let fetched_info = zk_sync_pdf_info("ATTUNC01", true).expect("online --fetch succeeds");
    check!(fetched_info.cached && fetched_info.fetched);
    check!(Path::new(&fetched_info.local_path).is_file());
    check!(fetched_info.page_count == 2 && fetched_info.pages.len() == 2);
    check!(fetched_info.pages[1].rotation == 90);
    let calls_fetch = take_invocations(zk_home);
    check!(calls_fetch.len() == 1 && calls_fetch[0].contains("\"--fetch\""));

    // 5. Symlink at cached local_path must be rejected.
    let outside_pdf = root.join("outside.pdf");
    std::fs::write(&outside_pdf, b"%PDF-1.4\n%%EOF\n").unwrap();
    let sym_dir = zk_home.join("storage").join("ATTSYML1");
    std::fs::create_dir_all(&sym_dir).unwrap();
    let sym_pdf = sym_dir.join("linked.pdf");
    let _ = std::fs::remove_file(&sym_pdf);
    std::os::unix::fs::symlink(&outside_pdf, &sym_pdf).unwrap();
    let sym_err = zk_sync_pdf_info("ATTSYML1", false).expect_err("symlinked local_path must fail");
    check!(matches!(sym_err, ZkSyncError::InvalidOutput(_)));

    // 6. Single-flight background worker poll roundtrip.
    let item = ZoteroLibraryItem {
        item_key: "ABCD1234".to_string(),
        attachment_key: "ATTCACH1".to_string(),
        title: "Kuhn".to_string(),
        ..Default::default()
    };
    check!(zk_bg_start_pdf_info(item, false));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut bg_res = None;
    while std::time::Instant::now() < deadline {
        if let Some(done) = zk_bg_poll() {
            bg_res = Some(done);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    match bg_res {
        Some(ZkBgDone::PdfInfo { item: it, result: Ok(info) }) => {
            check!(it.attachment_key == "ATTCACH1");
            check!(info.cached && info.page_count == 2);
        }
        other => check!(false && other.is_some()),
    }
    check!(!zk_bg_busy());
}

fn test_zotero_source_sidecar_and_dedup(root: &Path, store: &str) {
    let nb1 = make_notebook(store, "Kuhn Older Notes", 2, 1732400100);
    let nb2 = make_notebook(store, "Kuhn Newer Notes", 2, 1732400900);
    let nb_other = make_notebook(store, "Smith Notes", 1, 1732400500);

    let src1 = ZoteroNotebookSource {
        schema_version: 1,
        item_key: "ABCD1234".to_string(),
        attachment_key: "ATTCACH1".to_string(),
        citation_key: "kuhn1962".to_string(),
        title: "The Structure of Scientific Revolutions".to_string(),
        authors: "Thomas S. Kuhn".to_string(),
        year: "1962".to_string(),
        pdf_local_path: "/mnt/us/zotero-bridge/storage/ATTCACH1/kuhn1962.pdf".to_string(),
        page_count: 2,
        pages: vec![
            ZkPdfPageGeometry {
                page_index: 0,
                width_pt: 612.25,
                height_pt: 792.5,
                rotation: 0,
                page_label: "i".to_string(),
                crop_box: [0.0, 0.0, 612.25, 792.5],
                user_to_page: [1.0, 0.0, 0.0, -1.0, 0.0, 792.5],
            },
            ZkPdfPageGeometry {
                page_index: 1,
                width_pt: 792.0,
                height_pt: 612.0,
                rotation: 270,
                page_label: "1".to_string(),
                crop_box: [18.5, 24.25, 593.5, 767.75],
                user_to_page: [0.0, -1.0, -1.0, 0.0, 767.75, 593.5],
            },
        ],
    };

    check!(zotero_source_load(nb1.id.as_str()).is_none());
    check!(find_notebook_by_attachment_key("ATTCACH1").is_none());
    check!(find_notebook_by_attachment_key("-bad").is_none());

    // Save & load exact roundtrip on nb1.
    check!(zotero_source_save(nb1.id.as_str(), &src1).is_ok());
    let loaded1 = zotero_source_load(nb1.id.as_str()).expect("zotero_source_load must read saved sidecar");
    check!(loaded1 == src1);
    check!(loaded1.pages[0].width_pt == 612.25 && loaded1.pages[0].height_pt == 792.5);
    check!(loaded1.pages[1].rotation == 270);
    check!(loaded1.pages[1].crop_box == [18.5, 24.25, 593.5, 767.75]);
    check!(loaded1.pages[1].user_to_page == [0.0, -1.0, -1.0, 0.0, 767.75, 593.5]);

    // Single bound notebook found by attachment_key.
    check!(find_notebook_by_attachment_key("ATTCACH1").as_deref() == Some(nb1.id.as_str()));

    // Bind a second, newer notebook (nb2) to ATTCACH1 and nb_other to ATTUNC01:
    // find_notebook_by_attachment_key returns the newest valid notebook (nb2).
    check!(zotero_source_save(nb2.id.as_str(), &src1).is_ok());
    let mut src_other = src1.clone();
    src_other.item_key = "PULL9999".to_string();
    src_other.attachment_key = "ATTUNC01".to_string();
    src_other.page_count = 1;
    src_other.pages.truncate(1);
    check!(zotero_source_save(nb_other.id.as_str(), &src_other).is_ok());

    check!(find_notebook_by_attachment_key("ATTCACH1").as_deref() == Some(nb2.id.as_str()));
    check!(find_notebook_by_attachment_key("ATTUNC01").as_deref() == Some(nb_other.id.as_str()));

    // Incomplete `.import-*` directory in notebooks/ must be ignored even if it has zotero_source.json.
    let dot_import = Path::new(store).join("notebooks").join(".import-temp");
    std::fs::create_dir_all(&dot_import).unwrap();
    let mut src_ghost = src_other.clone();
    src_ghost.attachment_key = "ATTGHOST".to_string();
    let raw_sidecar = std::fs::read_to_string(
        Path::new(store).join("notebooks").join(nb1.id.as_str()).join(ZOTERO_SOURCE_FILE),
    )
    .unwrap()
    .replace("ATTCACH1", "ATTGHOST");
    std::fs::write(dot_import.join(ZOTERO_SOURCE_FILE), raw_sidecar.as_bytes()).unwrap();
    check!(find_notebook_by_attachment_key("ATTGHOST").is_none());

    // Trashing nb2 falls back cleanly to nb1!
    assert_eq!(store_delete(nb2.id.as_str()), 0);
    check!(find_notebook_by_attachment_key("ATTCACH1").as_deref() == Some(nb1.id.as_str()));

    // Symlink at zotero_source.json is rejected on both load and save.
    let sidecar_path = Path::new(store)
        .join("notebooks")
        .join(nb1.id.as_str())
        .join(ZOTERO_SOURCE_FILE);
    let outside_json = root.join("outside_source.json");
    std::fs::copy(&sidecar_path, &outside_json).unwrap();
    std::fs::remove_file(&sidecar_path).unwrap();
    std::os::unix::fs::symlink(&outside_json, &sidecar_path).unwrap();

    check!(zotero_source_load(nb1.id.as_str()).is_none());
    check!(zotero_source_save(nb1.id.as_str(), &src1).is_err());
    check!(find_notebook_by_attachment_key("ATTCACH1").is_none());
}

fn main() {
    if std::env::args().any(|a| a == "--list") {
        println!("zotero_unified: test");
        return;
    }

    let root = scratch_root();
    let bridge = root.join("bridge");
    let store = root.join("store");
    let zk_home = root.join("zk-home");
    let bin_path = root.join("bin").join("zk-sync");

    std::fs::create_dir_all(&bridge).unwrap();
    std::fs::create_dir_all(&store).unwrap();
    write_mock_zk_sync(&bin_path);

    let bridge_str = bridge.to_string_lossy().into_owned();
    let store_str = store.to_string_lossy().into_owned();

    ui_init(DPI);
    assert_eq!(font_init(), 0);
    assert_eq!(store_init(&store_str), 0);
    store_set_device(DPI, W, H);

    eprintln!("- test_enriched_index_and_filter");
    test_enriched_index_and_filter(&bridge_str);

    eprintln!("- test_refresh_and_credential_scrubbing");
    test_refresh_and_credential_scrubbing(&root, &bridge_str, &zk_home, &bin_path);

    eprintln!("- test_pdf_info_and_bg_worker");
    test_pdf_info_and_bg_worker(&root, &zk_home);

    eprintln!("- test_zotero_source_sidecar_and_dedup");
    test_zotero_source_sidecar_and_dedup(&root, &store_str);

    #[cfg(feature = "sim")]
    phase2::run(&root);

    zk_sync_set_test_config(None);
    bridge_set_dir(None);
    let _ = std::fs::remove_dir_all(&root);
    done();
}

/// Phase 2 end to end: one `App` driven by contacts and ticks as the device's main loop drives it.
#[cfg(feature = "sim")]
mod phase2 {
    use super::*;
    use super::pdffix::{self, PageSpec};
    use grimoire::library::{library_back, library_control_point, library_shortcut, library_zotero_control};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    static CUTS: Mutex<Vec<String>> = Mutex::new(Vec::new());

    fn overflow_hook(text: &str, need: i32, have: i32) {
        CUTS.lock().unwrap().push(format!("{text:?} needs {need} has {have}"));
    }

    const KUHN_TITLE: &str = "The Structure of Scientific Revolutions";
    const CLOUD_TITLE: &str = "Attention Is All You Need";

    const MOCK: &str = r#"#!/usr/bin/env python3
import json
import os
import shutil
import sys

args = sys.argv[1:]
home = os.environ["ZK_HOME"]

def read(name, default=""):
    p = os.path.join(home, name)
    if not os.path.exists(p):
        return default
    with open(p, "r", encoding="utf-8") as f:
        return f.read().strip()

with open(os.path.join(home, "invocations.jsonl"), "a", encoding="utf-8") as lf:
    lf.write(json.dumps({"args": args}) + "\n")

mode = read("mode.txt", "ok")
files = read("files_dir.txt")
cat = json.loads(read("catalog.json"))
cmd = args[0] if args else ""

def pdf_path(item):
    return os.path.join(files, item["attachment_key"], item["_file"])

if cmd == "pull":
    if mode == "offline":
        sys.stderr.write("pull failed apiKey=SECRET999\n")
        sys.exit(1)
    open(os.path.join(home, "pulled.flag"), "w").close()
    print('{"status":"ok"}')
    sys.exit(0)

if cmd == "export-index":
    items = list(cat["items"])
    if os.path.exists(os.path.join(home, "pulled.flag")):
        items.append(cat["after_pull"])
    out = []
    for it in items:
        o = {k: v for k, v in it.items() if k != "_file"}
        cached = it["has_pdf"] and os.path.isfile(pdf_path(it))
        o["pdf_cached"] = cached
        o["pdf_local_path"] = pdf_path(it) if cached else ""
        out.append(o)
    doc = {"schema_version": 1, "exported_at": 1732400500, "item_count": len(out),
           "collections": cat["collections"], "items": out}
    tmp = args[1] + ".tmp"
    with open(tmp, "w", encoding="utf-8") as f:
        json.dump(doc, f)
    os.replace(tmp, args[1])
    print(json.dumps({"schema_version": 1, "exported_at": 1732400500, "item_count": len(out)}))
    sys.exit(0)

if cmd == "pdf-info":
    att = args[1]
    fetch = "--fetch" in args[2:]
    item = next(i for i in cat["items"] if i["attachment_key"] == att)
    path = pdf_path(item)
    if fetch and mode == "offline":
        sys.stderr.write("fetch failed: network unreachable apiKey=SECRET999\n")
        sys.exit(1)
    fetched = False
    if fetch and not os.path.exists(path):
        os.makedirs(os.path.dirname(path), exist_ok=True)
        shutil.copyfile(os.path.join(home, "cloud_source.pdf"), path)
        fetched = True
    cached = os.path.isfile(path)
    pages = json.loads(read("pages.json")) if cached else []
    print(json.dumps({"schema_version": 1, "attachment_key": att, "parent_item_key": item["item_key"],
                      "filename": item["_file"], "cached": cached, "fetched": fetched,
                      "local_path": path if cached else "", "page_count": len(pages), "pages": pages}))
    sys.exit(0)

sys.stderr.write("unknown command\n")
sys.exit(2)
"#;

    /// (item_key, attachment_key, title, authors, year, collection 0..3, kind)
    const ITEMS: [(&str, &str, &str, &str, &str, usize, &str); 11] = [
        ("ABCD1234", "ATTCACH1", KUHN_TITLE, "Thomas S. Kuhn", "1962", 0, "pdf"),
        ("CLOUD001", "ATTCLOUD1", CLOUD_TITLE, "Ashish Vaswani et al.", "2017", 2, "pdf"),
        ("EPUB0001", "ATTEPUB1", "Gödel, Escher, Bach", "Douglas R. Hofstadter", "1979", 2, "epub"),
        ("SNAP0001", "ATTSNAP1", "Stanford Encyclopedia: Scientific Explanation", "James Woodward", "2021", 0, "snapshot"),
        ("POPP0001", "ATTPOPP1", "The Logic of Scientific Discovery", "Karl Popper", "1959", 0, "pdf"),
        ("LAKA0001", "ATTLAKA1", "The Methodology of Scientific Research Programmes", "Imre Lakatos", "1978", 0, "pdf"),
        ("FEYE0001", "ATTFEYE1", "Against Method", "Paul Feyerabend", "1975", 0, "pdf"),
        ("EINS0001", "ATTEINS1", "On the Electrodynamics of Moving Bodies", "Albert Einstein", "1905", 1, "pdf"),
        ("FEYN0001", "ATTFEYN1", "Space-Time Approach to Quantum Electrodynamics", "Richard P. Feynman", "1949", 1, "pdf"),
        ("SHAN0001", "ATTSHAN1", "A Mathematical Theory of Communication", "Claude E. Shannon", "1948", 2, "pdf"),
        ("TURI0001", "ATTTURI1", "On Computable Numbers, with an Application to the Entscheidungsproblem", "Alan M. Turing", "1936", 2, "pdf"),
    ];

    const COLLECTIONS: [(&str, &str); 3] =
        [("COLL0001", "Philosophy of Science"), ("COLL0002", "Physics"), ("COLL0003", "Computing")];

    fn quoted(s: &str) -> String {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    }

    fn item_json(key: &str, att: &str, title: &str, authors: &str, year: &str, col: usize, kind: &str) -> String {
        format!(
            "{{\"item_key\":{},\"attachment_key\":{},\"citation_key\":{},\"title\":{},\"authors\":{},\"year\":{},\
             \"item_type\":\"book\",\"collections\":[{}],\"collection_keys\":[{}],\"tags\":[],\
             \"has_pdf\":{},\"has_epub\":{},\"has_snapshot\":{},\"last_opened\":0,\"_file\":{}}}",
            quoted(key),
            quoted(att),
            quoted(&format!("{}{year}", key.to_lowercase())),
            quoted(title),
            quoted(authors),
            quoted(year),
            quoted(COLLECTIONS[col].1),
            quoted(COLLECTIONS[col].0),
            kind == "pdf",
            kind == "epub",
            kind == "snapshot",
            quoted(&format!("{}.pdf", key.to_lowercase())),
        )
    }

    fn catalog_json() -> String {
        let cols: Vec<String> = COLLECTIONS
            .iter()
            .map(|(k, n)| format!("{{\"key\":{},\"name\":{},\"parent_key\":\"\"}}", quoted(k), quoted(n)))
            .collect();
        let items: Vec<String> = ITEMS
            .iter()
            .map(|&(k, att, t, au, y, c, kind)| item_json(k, att, t, au, y, c, kind))
            // (the cached PDF keeps its real file name)
            .map(|j| j.replace("\"abcd1234.pdf\"", "\"kuhn1962.pdf\""))
            .collect();
        let extra = item_json("NOET0001", "ATTNOET1", "Invariant Variation Problems", "Emmy Noether", "1918", 1, "pdf");
        format!(
            "{{\"collections\":[{}],\"items\":[{}],\"after_pull\":{extra}}}",
            cols.join(","),
            items.join(",")
        )
    }

    /// The geometry `zk-sync pdf-info` reports for `paper()`: page 2 is turned a quarter.
    const PAGES_JSON: &str = r#"[
        {"page_index":0,"width_pt":612.0,"height_pt":792.0,"rotation":0,"page_label":"1",
         "crop_box":[0.0,0.0,612.0,792.0],"user_to_page":[1.0,0.0,0.0,-1.0,0.0,792.0]},
        {"page_index":1,"width_pt":792.0,"height_pt":612.0,"rotation":90,"page_label":"2",
         "crop_box":[0.0,0.0,612.0,792.0],"user_to_page":[0.0,1.0,1.0,0.0,0.0,0.0]}
    ]"#;

    /// A real 2-page PDF that looks enough like a paper to read in the screenshot.
    fn paper(title: &str, authors: &str) -> Vec<u8> {
        let text = |size: u32, x: u32, y: u32, s: &str| format!("0 g BT /F1 {size} Tf {x} {y} Td ({s}) Tj ET\n");
        let page = |head: &str, sub: &str| {
            let mut c = text(22, 64, 712, head);
            c.push_str(&text(13, 64, 688, sub));
            c.push_str("0 G 1 w 64 674 m 548 674 l S\n");
            for i in 0..26u32 {
                c.push_str(&text(11, 64, 646 - i * 20, "Normal science means research firmly based upon past scientific achievements."));
            }
            c
        };
        let first = PageSpec::new(612.0, 792.0, &page(title, authors));
        let mut second = PageSpec::new(612.0, 792.0, &page("The Route to Normal Science", "Page 2"));
        second.rotate = 90;
        pdffix::make(&[first, second])
    }

    fn probe(a: &mut App, key: &str) -> String {
        let mut out = String::new();
        assert_eq!(app_probe(a, key, &mut out, 512), 0, "probe {key}");
        out
    }

    fn zv(a: &mut App, key: &str) -> String {
        probe(a, &format!("library.zotero.{key}"))
    }

    fn contact(a: &mut App, type_: InType, x: f32, y: f32, t_ms: u32) {
        app_event(a, &InEvent { type_, x, y, t_ms, k_ms: t_ms, pressure: 0.5, ..Default::default() });
    }

    /// A finger tap on a named Library or Zotero view control.
    fn tap(a: &mut App, name: &str) {
        let (x, y) = library_control_point(a, name).unwrap_or_else(|| panic!("no control {name}"));
        let t = now_ms();
        contact(a, InType::TouchDown, x, y, t);
        contact(a, InType::TouchUp, x, y, t.wrapping_add(60));
        app_flush(a);
    }

    fn pen_stroke(a: &mut App) {
        let (x, y, t) = (W as f32 * 0.25, H as f32 * 0.5, now_ms());
        contact(a, InType::PenDown, x, y, t);
        for k in 1..=40u32 {
            let wave = if (k / 4) % 2 == 0 { 28.0 } else { -28.0 };
            contact(a, InType::PenMove, x + 22.0 * k as f32, y + wave, t.wrapping_add(8 * k));
        }
        contact(a, InType::PenUp, x + 880.0, y, t.wrapping_add(340));
        app_flush(a);
    }

    /// The main loop's ticks until `done` holds.
    fn tick_until(a: &mut App, what: &str, mut done: impl FnMut(&mut App) -> bool) {
        let t0 = Instant::now();
        loop {
            app_tick(a, now_ms());
            app_flush(a);
            if done(a) {
                return;
            }
            if t0.elapsed() > Duration::from_secs(90) {
                let screen = probe(a, "screen");
                let toast = probe(a, "toast");
                panic!("timed out waiting for {what}: screen {screen}, toast {toast:?}");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn open_view(a: &mut App) {
        assert_eq!(probe(a, "screen"), "library");
        tap(a, "zotero");
        assert_eq!(zv(a, "open"), "1", "the Zotero view opens from the top bar");
    }

    fn search(a: &mut App, query: &str) {
        check!(library_zotero_control(a, "filter", query));
        app_flush(a);
    }

    fn open_pdf(a: &mut App, query: &str) {
        open_view(a);
        search(a, query);
        check!(zv(a, "total") == "1");
        tap(a, "zotero.open");
        tick_until(a, "the Zotero PDF to open in the editor", |a| {
            a.scr == ScreenId::Editor && a.zotero_import.is_none()
        });
    }

    fn to_library(a: &mut App) {
        check!(editor_shortcut(a, SHORTCUT_LIBRARY));
        app_flush(a);
        check!(probe(a, "screen") == "library");
    }

    fn notebooks() -> Vec<NotebookInfo> {
        let mut v = Vec::new();
        assert_eq!(store_list(&mut v), 0);
        v
    }

    fn page_keys(a: &mut App) -> Vec<String> {
        let rows: usize = zv(a, "rows").parse().unwrap();
        (1..=rows).map(|i| zv(a, &format!("key.{i}"))).collect()
    }

    fn library_view(a: &mut App, bridge: &Path, zk_home: &Path) {
        // Without the bridge there is no top-bar button; with it there is.
        bridge_set_dir(Some(&bridge.with_extension("absent").to_string_lossy()));
        app_switch(a, ScreenId::Library);
        check!(library_control_point(a, "zotero").is_none());
        bridge_set_dir(Some(&bridge.to_string_lossy()));
        app_switch(a, ScreenId::Library);
        check!(library_control_point(a, "zotero").is_some());

        // No library_index.json yet: opening the view exports one in the background.
        check!(!bridge.join(LIBRARY_INDEX_FILE).exists());
        let _ = take_invocations(zk_home);
        open_view(a);
        tick_until(a, "the first catalog export", |a| zv(a, "total") == "11");
        let calls = take_invocations(zk_home);
        check!(calls.len() == 1 && calls[0].contains("export-index"));

        check!(probe(a, "library.sheet") == "zotero");
        check!(zv(a, "pages") == "2" && zv(a, "page") == "1" && zv(a, "rows") == "9");
        let mut keys = page_keys(a);
        tap(a, "zotero.next");
        check!(zv(a, "page") == "2" && zv(a, "rows") == "2");
        keys.extend(page_keys(a));
        check!(library_control_point(a, "zotero.next").is_none());
        tap(a, "zotero.prev");
        check!(zv(a, "page") == "1" && zv(a, "rows") == "9");
        let mut want: Vec<String> = ITEMS.iter().map(|i| i.0.to_string()).collect();
        keys.sort();
        want.sort();
        check!(keys == want);

        // Hardware page turns move through the same pages.
        check!(library_shortcut(a, SHORTCUT_NEXT_PAGE));
        check!(zv(a, "page") == "2");
        check!(library_shortcut(a, SHORTCUT_PREV_PAGE));
        check!(zv(a, "page") == "1");

        // The collection control cycles All -> each collection -> All.
        for (name, total) in [("Philosophy of Science", "5"), ("Physics", "2"), ("Computing", "4"), ("All", "11")] {
            tap(a, "zotero.collection");
            check!(zv(a, "collection") == name);
            check!(zv(a, "total") == total);
        }

        // Search: by author, no match, cleared by its x; the keyboard opens and Back closes it.
        search(a, "turing");
        check!(zv(a, "total") == "1" && zv(a, "key.1") == "TURI0001");
        search(a, "no such paper");
        check!(zv(a, "total") == "0" && zv(a, "rows") == "0" && zv(a, "selected") == "0");
        tap(a, "zotero.clear");
        check!(zv(a, "total") == "11" && zv(a, "filter").is_empty());
        tap(a, "zotero.search");
        check!(zv(a, "keyboard") == "1");
        check!(library_back(a));
        check!(zv(a, "keyboard") == "0" && zv(a, "open") == "1");

        for (query, badge) in [("kuhn", "PDF · Ready"), ("vaswani", "PDF · Cloud"), ("escher", "EPUB"), ("woodward", "Snapshot")] {
            search(a, query);
            check!(zv(a, "badge.1") == badge);
        }
        search(a, "");

        // Sync: pull then export-index on the background thread, picked up by app_tick.
        let _ = take_invocations(zk_home);
        tap(a, "zotero.sync");
        tick_until(a, "Sync to finish", |a| zv(a, "total") == "12");
        let calls = take_invocations(zk_home);
        check!(calls.len() == 2 && calls[0].contains("\"pull\"") && calls[1].contains("export-index"));
        check!(zv(a, "status") == "Synced 12 items in Zotero catalog.");

        // Back to the 11-item catalog for the rest.
        std::fs::remove_file(zk_home.join("pulled.flag")).unwrap();
        tap(a, "zotero.refresh");
        tick_until(a, "Refresh to finish", |a| zv(a, "total") == "11");
        tap(a, "zotero.close");
        check!(zv(a, "open") == "0");
    }

    fn cached_import_and_dedup(a: &mut App, bridge: &Path, zk_home: &Path) {
        let cached = bridge.join("files").join("ATTCACH1").join("kuhn1962.pdf");
        check!(notebooks().is_empty());
        let _ = take_invocations(zk_home);
        open_pdf(a, "kuhn");

        let calls = take_invocations(zk_home);
        check!(calls.iter().any(|c| c.contains("pdf-info") && c.contains("ATTCACH1")));
        check!(!calls.iter().any(|c| c.contains("--fetch")));

        let books = notebooks();
        assert_eq!(books.len(), 1, "one notebook per attachment");
        let id = books[0].id.as_str().to_string();
        check!(books[0].name.as_str() == KUHN_TITLE);
        check!(books[0].npages() == 2);
        check!(probe(a, "editor.notebook_id") == id);
        check!(probe(a, "editor.page") == "1" && probe(a, "editor.pages") == "2");
        check!(cached.is_file()); // (read in place, never moved away from the bridge)

        let src = zotero_source_load(&id).expect("zotero_source.json is bound");
        check!(src.item_key == "ABCD1234" && src.attachment_key == "ATTCACH1");
        check!(src.citation_key == "abcd12341962");
        check!(src.title == KUHN_TITLE && src.authors == "Thomas S. Kuhn" && src.year == "1962");
        check!(Path::new(&src.pdf_local_path) == cached);
        assert_eq!(src.pages.len(), 2);
        check!(src.page_count == 2);
        let (p0, p1) = (&src.pages[0], &src.pages[1]);
        check!(p0.width_pt == 612.0 && p0.height_pt == 792.0 && p0.rotation == 0);
        check!(p0.user_to_page == [1.0, 0.0, 0.0, -1.0, 0.0, 792.0]);
        check!(p1.width_pt == 792.0 && p1.height_pt == 612.0 && p1.rotation == 90);
        check!(p1.user_to_page == [0.0, 1.0, 1.0, 0.0, 0.0, 0.0]);

        check!(probe(a, "editor.strokes") == "0");
        pen_stroke(a);
        check!(probe(a, "editor.strokes") == "1");
        check!(editor_shortcut(a, SHORTCUT_NEXT_PAGE));
        app_flush(a);
        check!(probe(a, "editor.page") == "2" && probe(a, "editor.strokes") == "0");
        to_library(a);

        // The same item again: the bound notebook reopens, on the page left, with no new import.
        let _ = take_invocations(zk_home);
        open_view(a);
        search(a, "kuhn");
        check!(zv(a, "badge.1") == "PDF · Notes");
        tap(a, "zotero.open");
        check!(probe(a, "screen") == "editor");
        check!(a.zotero_import.is_none());
        check!(take_invocations(zk_home).is_empty());
        check!(notebooks().len() == 1);
        check!(probe(a, "editor.notebook_id") == id);
        check!(probe(a, "editor.page") == "2");
        check!(editor_shortcut(a, SHORTCUT_PREV_PAGE));
        app_flush(a);
        check!(probe(a, "editor.page") == "1" && probe(a, "editor.strokes") == "1");
        to_library(a);
    }

    fn cloud_fetch(a: &mut App, bridge: &Path, zk_home: &Path, store: &Path) {
        let dest = bridge.join("files").join("ATTCLOUD1").join("cloud001.pdf");
        std::fs::write(zk_home.join("cloud_source.pdf"), paper(CLOUD_TITLE, "Ashish Vaswani et al., 2017")).unwrap();

        // Offline: the Library stays, the error is said without the secret.
        set_mock_mode(zk_home, "offline");
        let _ = take_invocations(zk_home);
        open_view(a);
        search(a, "vaswani");
        tap(a, "zotero.open");
        tick_until(a, "the offline fetch to fail", |a| a.zotero_import.is_none());
        check!(take_invocations(zk_home).iter().any(|c| c.contains("--fetch")));
        check!(probe(a, "screen") == "library" && zv(a, "open") == "1");
        let toast = probe(a, "toast");
        let status = zv(a, "status");
        check!(toast.contains("network unreachable") && toast.contains("[REDACTED]"));
        check!(status.contains("[REDACTED]"));
        check!(!toast.contains("SECRET999") && !status.contains("SECRET999"));
        check!(!probe(a, "notice").contains("SECRET999"));
        let log = std::fs::read(store.join("grimoire.log")).unwrap_or_default();
        check!(!String::from_utf8_lossy(&log).contains("SECRET999"));
        check!(!dest.exists());
        check!(notebooks().len() == 1);

        // Online: the same tap downloads, imports and opens.
        set_mock_mode(zk_home, "ok");
        let _ = take_invocations(zk_home);
        tap(a, "zotero.open");
        tick_until(a, "the fetched PDF to open in the editor", |a| {
            a.scr == ScreenId::Editor && a.zotero_import.is_none()
        });
        check!(take_invocations(zk_home).iter().any(|c| c.contains("--fetch")));
        check!(dest.is_file());
        check!(notebooks().len() == 2);
        let id = probe(a, "editor.notebook_id");
        check!(probe(a, "editor.pages") == "2");
        let src = zotero_source_load(&id).expect("fetched PDF is bound");
        check!(src.item_key == "CLOUD001" && src.attachment_key == "ATTCLOUD1");
        check!(src.title == CLOUD_TITLE && src.pages.len() == 2 && src.pages[1].rotation == 90);
        to_library(a);
    }

    fn fallback_to_zotero(a: &mut App, zk_home: &Path) {
        // The Open button for the EPUB; a tap on the already selected row for the snapshot.
        for (query, control, item, att) in [
            ("escher", "zotero.open", "EPUB0001", "ATTEPUB1"),
            ("woodward", "zotero.row.1", "SNAP0001", "ATTSNAP1"),
        ] {
            handoff_clear();
            app_switch(a, ScreenId::Library);
            open_view(a);
            search(a, query);
            let _ = take_invocations(zk_home);
            tap(a, control);
            check!(!a.running && a.exit_code == 42);
            let h = handoff_peek().expect("handoff.json is written");
            check!(h.target_enum() == Some(HandoffTarget::Zotero));
            check!(h.zotero.item_key == item && h.zotero.attachment_key == att);
            check!(a.zotero_import.is_none() && take_invocations(zk_home).is_empty());
            check!(notebooks().len() == 2);
            handoff_clear();
            a.running = true;
            a.exit_code = 0;
        }
        app_switch(a, ScreenId::Library);
    }

    /// Nothing was cut since the last call.
    fn fits(theme: UiTheme, place: &str) {
        let cuts = std::mem::take(&mut *CUTS.lock().unwrap());
        if !cuts.is_empty() {
            eprintln!("  cut ({theme:?}, {place}): {cuts:?}");
        }
        check!(cuts.is_empty());
    }

    fn save_png(a: &App, path: &Path) {
        let c = &a.screen;
        let name = std::ffi::CString::new(path.to_string_lossy().into_owned()).unwrap();
        // SAFETY: name is NUL-terminated; c.px holds c.h rows of c.w gray bytes, c.stride apart.
        let rc = unsafe {
            grimoire::ffi_stb::stbi_write_png(name.as_ptr(), c.w, c.h, 1, c.px.as_ptr() as *const _, c.stride)
        };
        check!(rc != 0);
        let png = std::fs::read(path).unwrap_or_default();
        check!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
    }

    fn layout_and_screenshots(a: &mut App, root: &Path) {
        let default_theme = ui_theme();
        let use_theme = |a: &mut App, t: UiTheme| {
            a.settings.theme = t as i32;
            ui_set_theme(t);
        };
        for theme in [UiTheme::Gothic, UiTheme::Classic] {
            use_theme(a, theme);
            app_toast_dismiss(a);
            CUTS.lock().unwrap().clear();
            app_switch(a, ScreenId::Library);
            app_flush(a);
            check!(library_control_point(a, "zotero").is_some());
            fits(theme, "Library home");

            open_view(a);
            fits(theme, "Zotero view, page 1");
            tap(a, "zotero.next");
            fits(theme, "Zotero view, page 2");
            for _ in 0..4 {
                tap(a, "zotero.collection");
                fits(theme, "Zotero view, a collection");
            }
            for query in ["escher", "kuhn", "no such paper"] {
                search(a, query);
                fits(theme, query);
            }
            search(a, "");
            fits(theme, "Zotero view, cleared");

            search(a, "kuhn");
            CUTS.lock().unwrap().clear();
            tap(a, "zotero.open");
            check!(probe(a, "screen") == "editor");
            app_toast_dismiss(a);
            let _ = probe(a, "editor.screen_composed"); // (draws the whole editor under the hook)
            fits(theme, "Zotero PDF in the editor");
            to_library(a);
        }
        use_theme(a, default_theme);

        let mut dirs = vec![root.join("shots")];
        if let Some(d) = std::env::var_os("GRIMOIRE_ZOTERO_SAVE_SHOTS").filter(|d| !d.is_empty()) {
            dirs.push(PathBuf::from(d));
        }
        for d in &dirs {
            std::fs::create_dir_all(d).unwrap();
        }

        app_toast_dismiss(a);
        app_switch(a, ScreenId::Library);
        open_view(a);
        app_flush(a);
        for d in &dirs {
            save_png(a, &d.join("desktop_zotero_library_view.png"));
        }

        search(a, "kuhn");
        tap(a, "zotero.open");
        check!(probe(a, "screen") == "editor");
        if probe(a, "editor.page") != "1" {
            check!(editor_shortcut(a, SHORTCUT_PREV_PAGE));
        }
        app_toast_dismiss(a);
        app_flush(a);
        check!(probe(a, "editor.strokes") == "1");
        check!(probe(a, "editor.screen_composed") == "1");
        for d in &dirs {
            save_png(a, &d.join("desktop_zotero_opened_pdf.png"));
        }
        to_library(a);
    }

    pub fn run(base: &Path) {
        let root = base.join("phase2");
        let storage = root.join("us");
        let bridge = storage.join("zotero-bridge");
        let store = storage.join("grimoire");
        let zk_home = root.join("zk-home");
        let bin = root.join("bin").join("zk-sync");
        for d in [&bridge, &store, &zk_home, &storage.join("documents"), &bridge.join("files").join("ATTCACH1")] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(
            bridge.join("files").join("ATTCACH1").join("kuhn1962.pdf"),
            paper(KUHN_TITLE, "Thomas S. Kuhn, 1962"),
        )
        .unwrap();
        write_mock_zk_sync(&bin);
        std::fs::write(&bin, MOCK).unwrap(); // (keeps the executable bit)
        std::fs::write(zk_home.join("files_dir.txt"), bridge.join("files").to_string_lossy().as_bytes()).unwrap();
        std::fs::write(zk_home.join("catalog.json"), catalog_json()).unwrap();
        std::fs::write(zk_home.join("pages.json"), PAGES_JSON).unwrap();

        std::env::set_var("GRIMOIRE_STORAGE", &storage);
        std::env::set_var("GRIMOIRE_WORK_EXE", env!("CARGO_BIN_EXE_grimoire-headless"));
        std::env::set_var("GRIMOIRE_SCRIPT", "/dev/null");
        bridge_set_dir(Some(&bridge.to_string_lossy()));
        zk_sync_set_test_config(Some(ZkSyncConfig {
            bin: bin.to_string_lossy().into_owned(),
            zk_home: zk_home.to_string_lossy().into_owned(),
            zk_config_dir: root.join("zk-config").to_string_lossy().into_owned(),
            local_timeout_ms: 20_000,
            network_timeout_ms: 20_000,
        }));
        zk_bg_reset();

        let (mut dw, mut dh, mut ddpi) = (0, 0, 0.0);
        let _ = display_open(&mut dw, &mut dh, &mut ddpi);
        assert_eq!(store_init(&store.to_string_lossy()), 0);
        let mut a = App::default();
        assert_eq!(app_init(&mut a, W, H, DPI), 0);
        app_toast_dismiss(&mut a);
        app_flush(&mut a);
        font_set_overflow_hook(Some(overflow_hook));

        eprintln!("- phase2_library_view");
        library_view(&mut a, &bridge, &zk_home);
        eprintln!("- phase2_cached_import_and_dedup");
        cached_import_and_dedup(&mut a, &bridge, &zk_home);
        eprintln!("- phase2_cloud_fetch");
        cloud_fetch(&mut a, &bridge, &zk_home, &store);
        eprintln!("- phase2_fallback_to_zotero");
        fallback_to_zotero(&mut a, &zk_home);
        eprintln!("- phase2_layout_and_screenshots");
        layout_and_screenshots(&mut a, &root);

        font_set_overflow_hook(None);
        tick_until(&mut a, "background jobs to finish", |_| !bg_busy());
        app_shutdown(&mut a);
        display_close();
        for v in ["GRIMOIRE_STORAGE", "GRIMOIRE_WORK_EXE", "GRIMOIRE_SCRIPT"] {
            std::env::remove_var(v);
        }
    }
}

//! Checkpoint 1 (`docs/unified-ink-sync-contract.md`): enriched `library_index.json` parsing and
//! collection/query filtering, headless `zk-sync` subprocess wrapper & Zero-Credential scrubbing,
//! `pdf-info` geometry inspection/fetch, and `zotero_source.json` notebook sidecar + deduplication.

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

fn make_notebook(name: &str, pages: u32, modified: i64) -> NotebookInfo {
    let mut nb = NotebookInfo::default();
    assert_eq!(store_create(name, "", PAPER_BLANK, &mut nb), 0);
    nb.page_ids = (1..=pages).collect();
    nb.next_page_id = pages + 1;
    nb.modified = modified;
    assert_eq!(store_save_info(&mut nb), 0);
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
    let nb1 = make_notebook("Kuhn Older Notes", 2, 1732400100);
    let nb2 = make_notebook("Kuhn Newer Notes", 2, 1732400900);
    let nb_other = make_notebook("Smith Notes", 1, 1732400500);

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
        println!("zotero_unified_checkpoint1: test");
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

    zk_sync_set_test_config(None);
    bridge_set_dir(None);
    let _ = std::fs::remove_dir_all(&root);
    done();
}

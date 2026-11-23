//! docs/bridge-contract.md: the Rust side of the Grimoire x Zotero bridge. One test function: the
//! store root and GZ_BRIDGE_DIR are process-wide, like the rest of the app's file-backed state.

use grimoire::prelude::*;
use std::path::PathBuf;

const W: i32 = 1860;
const H: i32 = 2480;
const DPI: f32 = 300.0;

fn base() -> PathBuf {
    let root = PathBuf::from(std::env::var("GRIMOIRE_TEST_TMP").unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().into_owned()))
        .join(format!("zotero-bridge-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn now_secs() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
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
    a.running = true;
    a
}

fn validators_and_io(bridge: &str) {
    // The field rules, from the contract, including the ones a native caller can get wrong.
    assert!(item_key_valid("ABCD1234") && item_key_valid("a_b-C9"));
    assert!(!item_key_valid("") && !item_key_valid("bad/key") && !item_key_valid(&"x".repeat(33)));
    assert!(attachment_key_valid("EFGH5678") && !attachment_key_valid("a b"));
    assert!(notebook_id_valid("20261123-101500-ab12") && !notebook_id_valid(""));
    assert!(nonce_valid("1732350000123-a1b2c3d4") && event_id_valid("1732350012345-f0e1d2"));
    assert!(!event_id_valid("a.b"));
    assert!(valid_page_id(0) && valid_page_index(u32::MAX));
    assert!(valid_zotero_page(0) && valid_zotero_page(1) && valid_zotero_page(ZOTERO_PAGE_MAX));
    assert!(!valid_zotero_page(ZOTERO_PAGE_MAX + 1));
    assert!(locator_valid("p. 42") && !locator_valid("p.\n42") && !locator_valid(&"x".repeat(513)));
    assert!(bridge_text_valid("Kuhn 1962", 256) && !bridge_text_valid("Kuhn\n1962", 256));
    assert!(title_valid("A title") && !title_valid("a\0b"));
    assert!(quote_valid("a quote", 4096) && !quote_valid("a\0quote", 4096));
    assert_eq!(clean_text("  a\tb\n c\0d ", 32), "a b cd");
    assert_eq!(clean_text("abcdef", 3), "abc");
    let enc = pct_encode("a,b% \r\n");
    assert_eq!(enc, "a%2Cb%25%20%0D%0A");
    assert_eq!(pct_decode(&enc).as_deref(), Some("a,b% \r\n"));
    assert_eq!(pct_decode("a%2cb").as_deref(), Some("a,b")); // lowercase hex is valid on read
    assert_eq!(pct_decode("a%2"), None);
    assert_eq!(HandoffTarget::parse("zotero"), Some(HandoffTarget::Zotero));
    assert_eq!(HandoffTarget::Zotero.as_str(), TARGET_ZOTERO);
    assert_eq!(HandoffAction::parse("return_essay"), Some(HandoffAction::ReturnEssay));
    assert_eq!(HandoffAction::OpenItem.as_str(), ACTION_OPEN_ITEM);

    // Path names are fixed: no component of a caller's name can leave the bridge.
    assert_eq!(bridge_dir_in("/tmp/x/"), "/tmp/x");
    assert!(bridge_path_in(bridge, "../handoff.json").is_none());
    assert!(bridge_path_in(bridge, "sub/handoff.json").is_none());
    assert!(bridge_path_in(bridge, HANDOFF_FILE).is_some());
    assert!(inbox_name_ok("1732350012345-a1b2.json"));
    assert!(!inbox_name_ok("../1732350012345-a1b2.json"));
    assert!(!inbox_name_ok("abc-a1b2.json"));
    assert!(!inbox_name_ok("123-a.b.json"));
    assert!(inbox_path_in(bridge, "../x.json").is_none());
    assert!(!bridge_exists(), "no bridge directory: everything is a no-op");

    // Atomic write, strict read, and no temp file left behind.
    let path = bridge_path_in(bridge, HANDOFF_FILE).unwrap();
    assert_eq!(bridge_read(&path, BRIDGE_JSON_MAX), Err(BridgeReadError::Absent));
    let doc = b"{\"a\":1}";
    assert_eq!(bridge_write_atomic(&path, doc), 0);
    assert_eq!(bridge_read(&path, BRIDGE_JSON_MAX), Ok(Some(doc.to_vec())));
    let tmp_prefix = format!("{}.tmp.", HANDOFF_FILE);
    assert!(std::fs::read_dir(bridge).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().starts_with(&tmp_prefix)));
    assert_eq!(bridge_remove(HANDOFF_FILE), 0);
    assert_eq!(bridge_remove(HANDOFF_FILE), 0); // removing an absent file is fine

    // Oversized, binary and linked documents are never returned as partial answers.
    let big = bridge_path_in(bridge, LIBRARY_INDEX_FILE).unwrap();
    std::fs::write(&big, vec![b'x'; BRIDGE_JSON_MAX + 1]).unwrap();
    assert_eq!(bridge_read(&big, BRIDGE_JSON_MAX), Err(BridgeReadError::TooLarge));
    std::fs::write(&big, b"a\0b").unwrap();
    assert_eq!(bridge_read(&big, BRIDGE_JSON_MAX), Err(BridgeReadError::Invalid));
    let _ = std::fs::remove_file(&big);

    let outside = PathBuf::from(bridge).parent().unwrap().join("outside.json");
    std::fs::write(&outside, b"outside").unwrap();
    let link = bridge_path_in(bridge, RETURN_CONTEXT_FILE).unwrap();
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    assert_eq!(bridge_read(&link, BRIDGE_JSON_MAX), Err(BridgeReadError::NotFile));
    assert_eq!(bridge_write_atomic(&link, b"{}"), -1);
    assert_eq!(std::fs::read(&outside).unwrap(), b"outside");
    assert_eq!(bridge_remove(RETURN_CONTEXT_FILE), 0); // the link goes, the target stays
    assert_eq!(std::fs::read(&outside).unwrap(), b"outside");

    // A link at the bridge's own name is not the bridge.
    let dirlink = format!("{bridge}-link");
    let _ = std::fs::remove_file(&dirlink);
    std::os::unix::fs::symlink(bridge, &dirlink).unwrap();
    assert!(bridge_exists_in(bridge));
    assert!(!bridge_exists_in(&dirlink));
    std::fs::remove_file(&dirlink).unwrap();
}

fn handoffs_and_context(bridge: &str) {
    let rt = Handoff::return_to("nb-1", 7, 3);
    let h = Handoff::new_to_zotero("ABCD1234", "EFGH5678", 42, "p. 42", Some(rt.clone()));
    assert!(handoff_validate(&h));
    assert_eq!(h.target_enum(), Some(HandoffTarget::Zotero));
    assert_eq!(h.action_enum(), Some(HandoffAction::OpenItem));
    assert!(handoff_fresh(&h, now_secs()));
    assert!(!handoff_fresh(&h, h.created_at + HANDOFF_MAX_AGE_SECS + 1));
    let json = handoff_json(&h);
    assert!(json.starts_with("{\"schema_version\":1,") && json.contains("\"target\":\"zotero\""));
    assert!(json.contains("\"return_to\":{") && !json.contains("api_key"));
    assert_eq!(handoff_parse(json.as_bytes()), Some(h.clone()));

    assert_eq!(handoff_write_in(bridge, &h), 0);
    assert!(handoff_exists());
    assert_eq!(handoff_peek_in(bridge), Some(h.clone())); // peek leaves it
    assert_eq!(handoff_take_in(bridge), Some(h.clone())); // take consumes it
    assert!(handoff_peek_in(bridge).is_none());
    assert_eq!(handoff_take_in(bridge), None);

    let mut bad = h.clone();
    bad.target = "elsewhere".to_string();
    assert!(!handoff_validate(&bad));
    assert_eq!(handoff_write_in(bridge, &bad), -1);
    assert!(handoff_parse(b"{\"schema_version\":1}").is_none());
    assert!(handoff_parse(b"\0").is_none());

    let mut stale = h.clone();
    stale.created_at = now_secs() - HANDOFF_MAX_AGE_SECS - 5;
    assert!(handoff_validate(&stale), "age is checked at read time, not in the writer");
    assert_eq!(handoff_write_in(bridge, &stale), 0);
    assert!(handoff_peek_in(bridge).is_none(), "a stale handoff is discarded");
    let handoff_path = bridge_path_in(bridge, HANDOFF_FILE).unwrap();
    assert!(!PathBuf::from(&handoff_path).exists());

    assert_eq!(write_file_atomic(&handoff_path, b"{\"schema_version\":1,\"nonce\":\"n\0\"}"), 0);
    assert!(handoff_peek_in(bridge).is_none(), "a malformed handoff is discarded");
    assert!(!PathBuf::from(&handoff_path).exists());
    let log = std::fs::read_to_string(bridge_path_in(bridge, BRIDGE_LOG_FILE).unwrap()).unwrap();
    assert!(log.contains("removed invalid handoff.json") && log.contains("removed stale handoff.json"));
    assert!(!log.contains("ABCD1234"), "no field of a document reaches bridge.log");

    let hg = Handoff::new_to_grimoire("", 9, 0);
    assert!(handoff_validate(&hg));
    assert_eq!(hg.target_enum(), Some(HandoffTarget::Grimoire));
    assert_eq!(hg.action_enum(), Some(HandoffAction::ReturnEssay));
    assert_eq!(handoff_write_in(bridge, &hg), 0);
    let home = handoff_parse(&std::fs::read(&handoff_path).unwrap()).unwrap();
    assert_eq!(home.target, TARGET_GRIMOIRE);
    assert_eq!(home.grimoire.notebook_id, "", "an empty id means the normal start");
    assert_eq!((home.grimoire.page_id, home.grimoire.page_index), (0, 0), "page fields are meaningless without an id");
    let hg_named = Handoff::new_to_grimoire("nb-1", 9, 2);
    assert_eq!(handoff_write_in(bridge, &hg_named), 0);
    assert_eq!(handoff_parse(&std::fs::read(&handoff_path).unwrap()), Some(hg_named));
    assert_eq!(handoff_clear(), 0);
    assert!(!handoff_exists());

    let rc = ZoteroReturnContext::new("nb-1", 7, 3);
    assert!(return_context_valid(&rc));
    assert_eq!(return_context_write_in(bridge, &rc), 0);
    assert_eq!(return_context_load_in(bridge), Some(rc.clone()));
    let mut bad_rc = rc.clone();
    bad_rc.notebook_id.clear();
    assert!(!return_context_valid(&bad_rc));
    assert_eq!(return_context_write_in(bridge, &bad_rc), -1);
    assert!(return_context_parse(b"{}").is_none());
    assert_eq!(return_context_save_in(bridge, &h), 0);
    let saved = return_context_load_in(bridge).unwrap();
    assert_eq!((saved.notebook_id.as_str(), saved.page_id, saved.page_index), ("nb-1", 7, 3));
    let _ = std::fs::remove_file(bridge_path_in(bridge, RETURN_CONTEXT_FILE).unwrap());
    let no_rt = Handoff::new_to_zotero("ABCD1234", "", 0, "", None);
    assert_eq!(return_context_save_in(bridge, &no_rt), 0);
    assert!(return_context_load_in(bridge).is_none());
}

fn library(bridge: &str) {
    let item = |key: &str, att: &str, cite: &str, title: &str, authors: &str, year: &str, kind: &str, collections: &[&str], tags: &[&str], pdf: bool| {
        json_members(&[
            json_field("item_key", key),
            json_field("attachment_key", att),
            json_field("citation_key", cite),
            json_field("title", title),
            json_field("authors", authors),
            json_field("year", year),
            json_field("item_type", kind),
            json_array_field("collections", &collections.iter().map(|s| s.to_string()).collect::<Vec<_>>()),
            json_array_field("tags", &tags.iter().map(|s| s.to_string()).collect::<Vec<_>>()),
            json_bool_field("has_pdf", pdf),
            json_bool_field("has_epub", false),
            json_bool_field("has_snapshot", false),
        ])
    };
    let text = json_members(&[
        json_int_field("schema_version", SCHEMA_VERSION as i64),
        json_int_field("exported_at", 1732350000),
        json_int_field("item_count", 3),
        format!("\"items\":[{}]", [
            item("ABCD1234", "EFGH5678", "Kuhn 1962", "The Structure of Scientific Revolutions", "Kuhn", "1962", "book", &["Philosophy of Science"], &["epistemology"], true),
            item("WXYZ0001", "", "Smith 2020", "A Study", "Smith", "2020", "article", &["History"], &[], false),
            item("bad/key", "", "Broken", "Broken", "", "", "", &[], &[], false),
        ].join(",")),
    ]);
    let idx = library_index_parse(text.as_bytes()).unwrap();
    assert_eq!(idx.schema_version, SCHEMA_VERSION);
    assert_eq!(idx.item_count, 3, "the catalog's own count is kept");
    assert_eq!(idx.items.len(), 2, "an item with a bad key is skipped");
    assert!(idx.items[0].has_pdf && !idx.items[0].has_epub);
    assert_eq!(idx.items[0].collections, vec!["Philosophy of Science".to_string()]);
    assert_eq!(library_index_find(&idx, "ABCD1234").map(|i| i.authors.as_str()), Some("Kuhn"));
    assert!(library_index_find(&idx, "abcd1234").is_none(), "Zotero keys are case sensitive");
    assert_eq!(library_index_search(&idx, "kuhn").len(), 1);
    assert_eq!(library_index_search(&idx, "PHILOSOPHY").len(), 1);
    assert_eq!(library_index_search(&idx, "HISTORY").len(), 1);
    assert_eq!(library_index_search(&idx, "").len(), 2);
    assert!(library_index_search(&idx, "nothing like this").is_empty());
    assert!(library_index_parse(b"{\"schema_version\":2}").is_none());
    let path = bridge_path_in(bridge, LIBRARY_INDEX_FILE).unwrap();
    assert_eq!(write_file_atomic(&path, text.as_bytes()), 0);
    assert_eq!(library_index_load_in(bridge), Some(idx));
    let _ = std::fs::remove_file(&path);
}

fn citation(id: &str, created_at: i64, item_key: &str) -> ZoteroCitation {
    ZoteroCitation {
        schema_version: SCHEMA_VERSION,
        id: id.to_string(),
        created_at,
        item_key: item_key.to_string(),
        attachment_key: "EFGH5678".to_string(),
        short_cite: "Kuhn 1962, p. 42".to_string(),
        title: "The Structure of Scientific Revolutions".to_string(),
        authors: "Kuhn".to_string(),
        year: "1962".to_string(),
        page: 42,
        locator: "p. 42".to_string(),
        quote: "Normal science...".to_string(),
        note: String::new(),
    }
}

fn inbox(bridge: &str) {
    let c1 = citation("a1", 1732350012, "ABCD1234");
    let c2 = citation("b2", 1732350012, "WXYZ0001");
    assert!(citation_valid(&c1));
    assert_eq!(citation_parse(citation_json(&c1).as_bytes()), Some(c1.clone()));
    let mut bad = c1.clone();
    bad.page = ZOTERO_PAGE_MAX + 1;
    assert!(!citation_valid(&bad));
    let mut bad = c1.clone();
    bad.id = "a/b".to_string();
    assert!(!citation_valid(&bad));
    assert!(citation_parse(b"{\"schema_version\":1}").is_none());

    assert_eq!(citation_write_in(bridge, &c1), 0);
    assert_eq!(citation_write_in(bridge, &c2), 0);
    let name = format!("{}-{}.json", c1.created_at, c1.id);
    assert!(inbox_name_ok(&name));
    assert!(PathBuf::from(inbox_path_in(bridge, &name).unwrap()).exists());
    let names = inbox_list_in(bridge);
    assert_eq!(names.len(), 2, "a link in inbox/ is not listed");
    assert_eq!(inbox_peek_in(bridge), vec![c1.clone(), c2.clone()]); // peek keeps the files
    assert_eq!(inbox_count(), 2);

    // A link, and a malformed file, are never consumed as citations.
    let outside = PathBuf::from(bridge).parent().unwrap().join("inbox-outside.json");
    std::fs::write(&outside, citation_json(&c1)).unwrap();
    let link = inbox_path_in(bridge, "1732350012-link.json").unwrap();
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    let malformed = inbox_path_in(bridge, "1732350012-bad.json").unwrap();
    assert_eq!(write_file_atomic(&malformed, b"{\"schema_version\":1,\"id\":\"bad\0\"}"), 0);
    let listed = inbox_list_in(bridge);
    assert_eq!(listed.len(), 3);
    assert!(!listed.iter().any(|n| n.ends_with("-link.json")), "a link is never queued");
    assert_eq!(inbox_peek_in(bridge), vec![c1.clone(), c2.clone()]);
    assert!(!PathBuf::from(&malformed).exists(), "a bad event is removed so it cannot poison the queue");
    assert!(PathBuf::from(&link).is_symlink() && PathBuf::from(&outside).exists());

    assert_eq!(inbox_remove_in(bridge, &c1.id), 0);
    assert_eq!(inbox_remove_in(bridge, "never-there"), -1);
    assert_eq!(inbox_take_one(), Some(c2.clone()));
    assert_eq!(inbox_count(), 0);

    // The optional older producer's single file drains too, and is unlinked.
    let c3 = citation("legacy-1", 1732350020, "ABCD1234");
    let legacy = json_members(&[
        json_int_field("schema_version", SCHEMA_VERSION as i64),
        format!("\"citations\":[{}]", citation_json(&c3)),
    ]);
    let legacy_path = bridge_path_in(bridge, LEGACY_INBOX_FILE).unwrap();
    assert_eq!(write_file_atomic(&legacy_path, legacy.as_bytes()), 0);
    assert_eq!(inbox_drain_in(bridge), vec![c3.clone()]);
    assert!(!PathBuf::from(&legacy_path).exists());
    assert_eq!(inbox_count(), 0);
    std::fs::remove_file(&link).unwrap();
    std::fs::remove_file(&outside).unwrap();

    pending_citations_push(&[c3.clone()]);
    assert_eq!(pending_citations(), vec![c3.clone()]);
    assert_eq!(pending_citations_take(), vec![c3]);
    assert!(pending_citations().is_empty());
}

fn zref(pid: u32, seed: u32, item_key: &str, attachment_key: &str, page: u32, locator: &str, short_cite: &str, area: Rect) -> ZoteroRef {
    ZoteroRef { pid, seed, area, item_key: item_key.to_string(), attachment_key: attachment_key.to_string(), page, locator: locator.to_string(), short_cite: short_cite.to_string() }
}

fn sidecar(bridge: &str, store: &str, nb: &NotebookInfo) {
    assert_eq!(bridge, bridge_dir().as_str(), "the test's bridge is the active bridge");
    let path = format!("{store}/notebooks/{}/{ZOTEROLINKS_FILE}", nb.id.as_str());
    let _ = std::fs::remove_file(&path);
    let mut zl = zoterolinks_load(nb.id.as_str());
    assert!(zl.writable && !zl.file && zl.is_empty());

    let seed = zotero_seed(nb.page_ids[0], "ABCD1234");
    assert_ne!(seed & 0x8000_0000, 0);
    let r = zref(nb.page_ids[0], seed, "ABCD1234", "EFGH5678", 42, "p. 42, note", "Kuhn, 1962", Rect::new(10, 20, 100, 40));
    assert!(zl.push(r.clone()));
    assert!(zl.push(r.clone()), "a later line for one seed replaces the earlier one");
    assert_eq!(zl.links.len(), 1);
    let r2 = zref(nb.page_ids[1], zotero_seed(nb.page_ids[1], "WXYZ0001"), "WXYZ0001", "", 7, "", "Smith 2020", Rect::new(10, 20, 100, 40));
    assert!(zl.push(r2.clone()));
    assert_eq!(zoterolinks_of(&zl, nb.page_ids[0]), &[r.clone()]);
    assert_eq!(zoterolinks_save(nb.id.as_str(), &mut zl, nb), 0);
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.starts_with(ZOTEROLINKS_HEADER));
    assert!(text.contains(&format!("zref.{}={:08x},", nb.page_ids[0], seed)));
    assert!(text.contains("ABCD1234") && text.contains("p.%2042%2C%20note"));
    assert!(text.contains("Kuhn%2C%201962"), "uppercase percent encoding keeps the INI to one line");
    let back = zoterolinks_load(nb.id.as_str());
    assert!(back.file && back.writable);
    assert_eq!(back.links, zl.links);
    assert!(zoterolinks_generation(nb.id.as_str()) > 0);

    // Unknown lines are carried verbatim; unreadable files and links are never written over.
    let mut carried = back.clone();
    carried.unknown.extend_from_slice(b"future.key=opaque value\n");
    assert_eq!(zoterolinks_save(nb.id.as_str(), &mut carried, nb), 0);
    assert!(std::fs::read_to_string(&path).unwrap().ends_with("future.key=opaque value\n"));
    assert_eq!(zoterolinks_load(nb.id.as_str()).unknown, b"future.key=opaque value\n");

    assert_eq!(write_file_atomic(&path, b"\0"), 0);
    let mut unreadable = zoterolinks_load(nb.id.as_str());
    assert!(!unreadable.writable && unreadable.file && unreadable.is_empty());
    assert_eq!(zoterolinks_save(nb.id.as_str(), &mut unreadable, nb), -1);
    assert_eq!(std::fs::read(&path).unwrap(), b"\0");
    std::fs::remove_file(&path).unwrap();
    let outside = PathBuf::from(store).join("elsewhere.ini");
    std::fs::write(&outside, b"zref.1=00000001,0,0,5,5,ABCD1234,,,0,,\n").unwrap();
    std::os::unix::fs::symlink(&outside, &path).unwrap();
    let mut linked = zoterolinks_load(nb.id.as_str());
    assert!(!linked.writable && linked.file);
    assert_eq!(zoterolinks_save(nb.id.as_str(), &mut linked, nb), -1);
    assert_eq!(std::fs::read(&outside).unwrap(), b"zref.1=00000001,0,0,5,5,ABCD1234,,,0,,\n");
    std::fs::remove_file(&path).unwrap();

    // A page gone from the notebook drops its links at the next save.
    let mut gone = r.clone();
    gone.pid = nb.page_ids[0];
    let mut live = ZoteroLinks { writable: true, ..Default::default() };
    assert!(live.push(gone));
    assert!(live.push(r2.clone()));
    let mut shorter = nb.clone();
    shorter.page_ids.retain(|&p| p != nb.page_ids[0]);
    assert_eq!(zoterolinks_prune(&mut live, &shorter), 1);
    assert_eq!(zoterolinks_save(nb.id.as_str(), &mut live, &shorter), 0);
    assert!(!std::fs::read_to_string(&path).unwrap().contains(&format!("zref.{}=", nb.page_ids[0])));

    // A carried line past the cap makes the file read-only for this version.
    let mut over = Vec::new();
    for _ in 0..40 {
        over.extend_from_slice("x".repeat(511).as_bytes());
        over.push(b'\n');
    }
    let parsed = zoterolinks_parse(&over).unwrap();
    assert!(!parsed.writable && parsed.skipped > 0);
    let mut parsed = parsed;
    assert_eq!(zoterolinks_save(nb.id.as_str(), &mut parsed, nb), -1);

    let too_many = (1..=ZOTEROLINKS_PER_PAGE + 1).map(|seed| format!("zref.{}={:08x},0,0,5,5,ABCD1234,,0,,\n", nb.page_ids[0], seed)).collect::<String>();
    assert!(zoterolinks_parse(too_many.as_bytes()).is_none(), "a full page cannot grow past its cap");
    assert!(zoterolinks_parse(b"zref.1=zz,0,0,5,5,ABCD1234,,,0,,\n").unwrap().is_empty());

    // The nodes a Graph builds from the sidecar, and the target a tap on one carries.
    let mut final_links = ZoteroLinks { writable: true, ..Default::default() };
    assert!(final_links.push(zref(nb.page_ids[0], 0x8000_0001, "ABCD1234", "EFGH5678", 42, "p. 42, note", "Kuhn 1962", Rect::new(10, 20, 100, 40))));
    assert!(final_links.push(zref(nb.page_ids[1], 0x8000_0003, "ABCD1234", "EFGH5678", 42, "", "", Rect::new(10, 20, 100, 40))));
    assert!(final_links.push(zref(nb.page_ids[0], 0x8000_0002, "WXYZ0001", "", 7, "", "Smith 2020", Rect::new(200, 20, 100, 40))));
    assert_eq!(zoterolinks_save(nb.id.as_str(), &mut final_links, nb), 0);
    let refs = zotero_references(&final_links);
    assert_eq!(refs.len(), 2);
    assert_eq!(refs[0].item_key, "ABCD1234");
    assert_eq!(refs[0].pages, vec![nb.page_ids[0], nb.page_ids[1]]);
    assert_eq!((refs[0].links, refs[0].first_seed), (2, 0x8000_0001));
    assert_eq!(refs[1].item_key, "WXYZ0001");
    assert_eq!(zotero_reference_nodes(nb.id.as_str()), refs);

    let target = zotero_target(&final_links.links[0]).unwrap();
    assert_eq!(target, "z:ABCD1234@42/EFGH5678#p.%2042%2C%20note");
    assert_eq!(zotero_target_encode(&final_links.links[0]), target);
    let decoded = zotero_target_parse(&target).unwrap();
    assert_eq!((decoded.item_key.as_str(), decoded.attachment_key.as_str(), decoded.page, decoded.locator.as_str()), ("ABCD1234", "EFGH5678", 42, "p. 42, note"));
    assert_eq!(zotero_target_decode(&target), Some(decoded));
    assert_eq!(zotero_target_from_parts("ABCD1234", "", 0, "").as_deref(), Some("z:ABCD1234"));
    let draft = zotero_target_parse("z:ABCD1234:EFGH5678:42").unwrap();
    assert_eq!((draft.item_key.as_str(), draft.attachment_key.as_str(), draft.page), ("ABCD1234", "EFGH5678", 42));
    assert!(zotero_target_parse("z:bad!key").is_none());
    assert!(zotero_target_parse("z:ABCD1234@1000001").is_none());
    assert!(zotero_target_parse("z:ABCD1234#%ZZ").is_none());
    assert!(is_zotero_target(&target) && !is_zotero_target("p1"));
    let long = zotero_target_from_parts("ABCD1234", "EFGH5678", 42, &"x".repeat(400)).unwrap();
    assert!(long.len() < NAV_TARGET_LEN && !long.contains('#'), "an overlarge locator is dropped, not truncated mid-escape");
}

fn app_and_startup(bridge: &str, base: &PathBuf, nb: &NotebookInfo) {
    assert_eq!((EXIT_CLEAN, EXIT_HANDOFF_TO_ZOTERO, EXIT_HANDOFF_TO_GRIMOIRE, EXIT_ZOTERO_RESTART), (0, 42, 43, 75));

    let h = Handoff::new_to_zotero("ABCD1234", "EFGH5678", 42, "", Some(Handoff::return_to(nb.id.as_str(), nb.page_ids[0], 0)));
    let mut a = app_with_editor();
    assert_eq!(app_request_handoff(&mut a, &h), 0);
    assert!(!a.running);
    assert_eq!(a.exit_code, EXIT_HANDOFF_TO_ZOTERO);
    assert_eq!(handoff_peek_in(bridge), Some(h.clone()));
    handoff_clear();

    let mut invalid = h.clone();
    invalid.zotero.item_key = "bad/key".to_string();
    let mut refused = app_with_editor();
    assert_eq!(app_request_handoff(&mut refused, &invalid), -1);
    assert!(refused.running && refused.exit_code == EXIT_CLEAN, "a failed write does not stop the loop");
    assert_eq!(app_request_zotero_handoff(&mut refused, "ABCD1234", "", 0, "", None), 0);
    assert!(!refused.running && refused.exit_code == EXIT_HANDOFF_TO_ZOTERO);
    handoff_clear();

    let mut back = app_with_editor();
    assert_eq!(app_request_return_to_grimoire(&mut back, "", nb.page_ids[0], 0), 0);
    assert!(!back.running && back.exit_code == EXIT_CLEAN);
    let written = handoff_take_in(bridge).unwrap();
    assert_eq!(written.target_enum(), Some(HandoffTarget::Grimoire));
    assert_eq!(written.action_enum(), Some(HandoffAction::ReturnEssay));

    let mut wrapper = app_with_editor();
    assert_eq!(app_request_handoff_exit(&mut wrapper, &h), 0);
    assert_eq!(wrapper.exit_code, EXIT_HANDOFF_TO_ZOTERO);
    handoff_clear();

    // No bridge directory: startup is a complete no-op.
    let mut a = app_with_editor();
    let absent = base.join("absent-bridge").to_string_lossy().into_owned();
    std::env::set_var(BRIDGE_DIR_ENV, &absent);
    assert!(!bridge_exists());
    assert_eq!(bridge_startup(&mut a), 0);
    assert!(pending_citations().is_empty());
    std::env::set_var(BRIDGE_DIR_ENV, bridge);

    // A handoff home and a queued citation are both handled at startup, once.
    let home = Handoff::new_to_grimoire(nb.id.as_str(), nb.page_ids[1], 1);
    assert_eq!(handoff_write_in(bridge, &home), 0);
    let event = citation("startup-1", 1732350030, "ABCD1234");
    assert_eq!(citation_write_in(bridge, &event), 0);
    assert_eq!(bridge_startup(&mut a), 0);
    assert!(handoff_peek_in(bridge).is_none(), "a handoff is single-shot");
    assert_eq!(inbox_count(), 0);
    assert_eq!(pending_citations(), vec![event]);
    let mut probe = String::new();
    assert_eq!(editor_probe(&mut a, "notebook_id", &mut probe, 64), 0);
    assert_eq!(probe, nb.id.as_str());
    assert_eq!(editor_probe(&mut a, "page", &mut probe, 32), 0);
    assert_eq!(probe, "2");
    assert!(a.running && a.exit_code == EXIT_CLEAN, "coming home is not a handoff exit");
    pending_citations_take();
}

fn graph_anchors(nb: &NotebookInfo, store: &str) {
    let path = format!("{store}/notebooks/{}/{ZOTEROLINKS_FILE}", nb.id.as_str());
    let _ = std::fs::remove_file(&path);
    let mut app = App { w: W, h: H, dpi: DPI, ..Default::default() };
    assert_eq!(canvas_init(&mut app.screen, W, H), 0);
    let mut nav = Nav::default();
    nav_init(&mut nav);
    let mut v = MapView::default();
    assert!(mapview_open(&mut v, &mut app, nb, &nav, nb.page_ids[0], false));
    let mut out = String::new();
    assert!(mapview_probe(&v, "anchors", &mut out));
    assert_eq!(out, "0", "no sidecar: no anchors, no change to the map");
    let mut visible = String::new();
    assert!(mapview_probe(&v, "visible", &mut visible));
    mapview_close(&mut v);

    let mut links = ZoteroLinks { writable: true, ..Default::default() };
    assert!(links.push(zref(nb.page_ids[0], 0x8000_0001, "ABCD1234", "EFGH5678", 42, "p. 42, note", "Kuhn 1962", Rect::new(10, 20, 100, 40))));
    assert_eq!(zoterolinks_save(nb.id.as_str(), &mut links, nb), 0);
    assert!(mapview_open(&mut v, &mut app, nb, &nav, nb.page_ids[0], false));
    let mut still_visible = String::new();
    assert!(mapview_probe(&v, "visible", &mut still_visible));
    assert_eq!(still_visible, visible, "a reference node never moves a dot");
    assert!(mapview_probe(&v, "anchors", &mut out));
    assert_eq!(out, "1");
    assert!(mapview_probe(&v, "anchor.1", &mut out));
    assert!(out.starts_with("out ") && out.contains("z:ABCD1234"), "{out}");
    assert!(!mapview_anchors_refresh(&mut v, nb, &nav), "an unchanged sidecar changes nothing");

    assert!(links.push(zref(nb.page_ids[1], 0x8000_0002, "WXYZ0001", "", 7, "", "Smith", Rect::new(10, 20, 100, 40))));
    assert_eq!(zoterolinks_save(nb.id.as_str(), &mut links, nb), 0);
    assert!(mapview_anchors_refresh(&mut v, nb, &nav));
    assert!(mapview_probe(&v, "anchors", &mut out));
    assert_eq!(out, "2");
    let mut refreshed = String::new();
    assert!(mapview_probe(&v, "visible", &mut refreshed));
    assert_eq!(refreshed, visible);
    mapview_close(&mut v);
    let _ = std::fs::remove_file(&path);
}

fn adversarial_regressions(bridge: &str, root: &std::path::Path) {
    // 1. Empty return_to block must not poison a valid Zotero -> Grimoire handoff.
    let empty_ret = br#"{"schema_version":1,"nonce":"n1","created_at":1732350000,"source":"zotero","target":"grimoire","action":"return_essay","grimoire":{"notebook_id":"","page_id":0,"page_index":0},"return_to":{"target":"grimoire","notebook_id":"","page_id":0,"page_index":0}}"#;
    let parsed = handoff_parse(empty_ret).expect("empty return_to should be ignored");
    assert_eq!(parsed.target, TARGET_GRIMOIRE);
    assert!(parsed.grimoire.notebook_id.is_empty());
    assert!(parsed.return_to.is_none());

    // 2. Zotero handoff with return_to and NO grimoire block must validate (C++ wire format).
    let cpp_wire = br#"{"schema_version":1,"nonce":"n2","created_at":1732350000,"source":"grimoire","target":"zotero","action":"open_item","zotero":{"item_key":"ABCD1234","attachment_key":"","page":12,"locator":""},"return_to":{"target":"grimoire","notebook_id":"nb_1","page_id":3000000000,"page_index":2147483648}}"#;
    let h_u32 = handoff_parse(cpp_wire).expect("C++ wire format with u32 > i32::MAX must parse");
    let rt = h_u32.return_to.as_ref().expect("return_to present");
    assert_eq!(rt.page_id, 3_000_000_000u32);
    assert_eq!(rt.page_index, 2_147_483_648u32);

    // 3. Full uint32 in return_context and rejection of negative / overflow numbers.
    let rc_u32 = br#"{"schema_version":1,"updated_at":1732350000,"notebook_id":"nb_1","page_id":4294967295,"page_index":2500000000}"#;
    let rc = return_context_parse(rc_u32).expect("u32::MAX page_id must parse");
    assert_eq!(rc.page_id, u32::MAX);
    assert_eq!(rc.page_index, 2_500_000_000u32);
    let rc_neg = br#"{"schema_version":1,"updated_at":1732350000,"notebook_id":"nb_1","page_id":-1,"page_index":0}"#;
    assert!(return_context_parse(rc_neg).is_none(), "negative page_id must fail");
    let rc_over = br#"{"schema_version":1,"updated_at":1732350000,"notebook_id":"nb_1","page_id":4294967296,"page_index":0}"#;
    assert!(return_context_parse(rc_over).is_none(), "u32 overflow page_id must fail");

    // 4. clean_text exact max-length and no trailing space on truncation.
    assert_eq!(clean_text("abcdef", 6), "abcdef");
    assert_eq!(clean_text("abc def", 4), "abc");

    // 5. library_index filters out empty/whitespace-only tags & collections.
    let idx_raw = br#"{"schema_version":1,"exported_at":1732350000,"item_count":1,"items":[{"item_key":"ITEM0001","title":"T","collections":["","  ","RealCol"],"tags":["\t","RealTag"]}]}"#;
    let idx = library_index_parse(idx_raw).expect("library index parses");
    assert_eq!(idx.items[0].collections, vec!["RealCol"]);
    assert_eq!(idx.items[0].tags, vec!["RealTag"]);

    // 6. Parent symlink component rejected by bridge_write_atomic.
    let real_parent = root.join("real_parent");
    let sym_parent = root.join("sym_parent");
    let _ = std::fs::create_dir_all(&real_parent);
    let _ = std::os::unix::fs::symlink(&real_parent, &sym_parent);
    let via_sym = sym_parent.join("nested").join("handoff.json");
    assert_ne!(bridge_write_atomic(&via_sym.to_string_lossy(), b"{}"), 0);
    let _ = bridge;
}

#[test]
fn zotero_bridge_contract() {
    let root = base();
    let bridge = root.join("bridge").to_string_lossy().into_owned();
    let store = root.join("store").to_string_lossy().into_owned();
    std::env::set_var(BRIDGE_DIR_ENV, &bridge);
    ui_init(DPI);
    assert_eq!(font_init(), 0);
    assert_eq!(store_init(&store), 0);
    store_set_device(DPI, W, H);

    let nb = book("Zotero links", 3);
    let graph_nb = book("Zotero graph", 3);

    validators_and_io(&bridge);
    handoffs_and_context(&bridge);
    library(&bridge);
    inbox(&bridge);
    sidecar(&bridge, &store, &nb);
    app_and_startup(&bridge, &root, &nb);
    graph_anchors(&graph_nb, &store);
    adversarial_regressions(&bridge, &root);

    std::env::remove_var(BRIDGE_DIR_ENV);
    let _ = std::fs::remove_dir_all(&root);
}

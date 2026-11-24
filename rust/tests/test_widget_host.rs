// test_widget_host.rs: what the widget host (widgetwasm.rs) lets an installed module do, with modules written
// for the purpose (tests/widgetfix): each reaches and passes one rule of the host.
mod widgetfix;

use grimoire::prelude::*;
use widgetfix::*;

/// A fresh data root for one test (the Makefile's test rules set GRIMOIRE_TEST_TMP), the UI at 300 dpi.
fn fresh_root(name: &str) -> std::path::PathBuf {
    let base = std::env::var("GRIMOIRE_TEST_TMP").map(std::path::PathBuf::from).unwrap_or_else(|_| std::env::temp_dir());
    let root = base.join(format!("widget-host-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    assert_eq!(store_init(root.to_str().unwrap()), 0);
    ui_init(300.0);
    assert_eq!(font_init(), 0);
    root
}

/// Installs a module as a package, as an install from a repository does, and loads it.
fn installed(id: &str, capabilities: &[&str], module: &[u8]) -> WidgetRuntime {
    let m = manifest(id, "Test", capabilities, FIXED_400_300, module);
    widget_package_install(&m, module, "", 1860, 2480).unwrap();
    WidgetRuntime::new(id).unwrap()
}

/// A package as an earlier install left it, from before the manifest check asked for `editor:overlay`: installed,
/// and its manifest then declaring these capabilities only. Loaded as it is.
fn installed_earlier(id: &str, capabilities: &[&str], module: &[u8]) -> WidgetRuntime {
    let m = manifest(id, "Test", &BOTH, FIXED_400_300, module);
    widget_package_install(&m, module, "", 1860, 2480).unwrap();
    let yaml = std::path::PathBuf::from(format!("{}/package.yaml", widget_package_dir(id)));
    rewrite(&yaml, "capabilities: [editor:overlay, storage:widget-local]", &format!("capabilities: [{}]", capabilities.join(", ")));
    WidgetRuntime::new(id).unwrap()
}

/// A canvas with no two neighbours alike, so that any pixel a draw changes shows.
fn patterned(w: i32, h: i32) -> Canvas {
    let mut c = Canvas::default();
    assert_eq!(canvas_init(&mut c, w, h), 0);
    for (i, p) in c.px.iter_mut().enumerate() {
        *p = (i * 7 % 251) as u8;
    }
    c
}

/// How many pixels outside `keep` differ between the two canvases.
fn changed_outside(before: &Canvas, after: &Canvas, keep: Rect) -> usize {
    let mut n = 0;
    for y in 0..before.h {
        for x in 0..before.w {
            let at = (y * before.stride + x) as usize;
            if !rect_contains(keep, x, y) && before.px[at] != after.px[at] {
                n += 1;
            }
        }
    }
    n
}

/// How many pixels of `r` are not white: what a pass drew there (the host fills the body white first).
fn inked(c: &Canvas, r: Rect) -> usize {
    (r.y..r.y + r.h).map(|y| (r.x..r.x + r.w).filter(|&x| c.px[(y * c.stride + x) as usize] != 255).count()).sum()
}

const BODY: Rect = Rect { x: 200, y: 150, w: 240, h: 180 };
const MAX: i32 = i32::MAX;
const MIN: i32 = i32::MIN;

/// Drawing passes that reach outside the body every way a rectangle, a text and a button can: far out, across
/// each edge, with sizes that are negative, zero, huge or overflow when added to their place.
fn passes() -> Vec<(&'static str, Vec<Op>)> {
    // (the text's bytes are the data segment below: 200 letters at address 16)
    let (t, n) = (16, 200);
    let mut far = Vec::new();
    far.extend(button(1, -5000, -5000, 20_000, 20_000, 0, 0, 1));
    far.extend(rect(-100_000, -100_000, 300_000, 300_000, 1));
    let mut overflow = Vec::new();
    for (x, y, w, h) in [(MIN, MIN, MAX, MAX), (MAX, MAX, MAX, MAX), (MIN, MIN, MIN, MIN), (-1, -1, MAX, MAX), (MAX, 0, 10, 10), (0, MAX, 10, 10), (MIN, 0, MAX, 40), (0, 0, -50, -50), (0, 0, 0, 0)] {
        overflow.extend(rect(x, y, w, h, 0));
        overflow.extend(button(2, x, y, w, h, t, 8, 0));
        overflow.extend(text(x, y, w, h, t, n, 40, 1));
        overflow.extend(hit(3, x, y, w, h));
    }
    let mut edges = Vec::new();
    for (x, y) in [(-30, -30), (BODY.w - 30, -30), (-30, BODY.h - 30), (BODY.w - 30, BODY.h - 30), (-200, 60), (BODY.w - 40, 60), (60, -200), (60, BODY.h - 20)] {
        edges.extend(rect(x, y, 240, 90, 1));
        edges.extend(button(4, x, y, 240, 90, t, 12, 1));
        edges.extend(text(x, y, 240, 90, t, 12, 60, 0));
    }
    let mut texts = Vec::new();
    for px in [MAX, 100_000, 513, 512, 300, 0, -5, MIN] {
        texts.extend(text(-300, -200, 5000, 400, t, n, px, 0));
        texts.extend(text(-4000, 20, 9000, 60, t, 3, px, 1));
    }
    let mut small = Vec::new();
    for s in 0..40 {
        small.extend(button(5, s * 6 - 20, s * 4 - 10, s, s, t, 6, s % 3));
        small.extend(button(5, s * 6 - 20, 200 - s * 4, 300, s, t, 6, 0));
        small.extend(text(s * 6 - 20, s * 3, s, s, t, 6, s, 1));
    }
    vec![("far", far), ("overflow", overflow), ("edges", edges), ("texts", texts), ("small", small)]
}

#[test]
fn whatever_a_module_draws_lands_in_its_body() {
    let root = fresh_root("body");
    let letters = [b'W'; 200];
    for (i, (name, ops)) in passes().into_iter().enumerate() {
        let mut rt = installed(&format!("test.body.{i}"), &BOTH, &module(&ops, &[], &[(16, &letters)]));
        for theme in [UiTheme::Gothic, UiTheme::Classic] {
            ui_set_theme(theme);
            let before = patterned(700, 600);
            let mut c = before.clone();
            rt.draw(&mut c, BODY);
            assert_eq!(rt.error(), None, "{name}");
            assert_eq!(changed_outside(&before, &c, BODY), 0, "{name} in {theme:?}: pixels outside the body changed");
            assert_eq!(c.clip, before.clip, "{name}: the canvas keeps its clip");
            assert_ne!(inked(&c, BODY), 0, "{name} in {theme:?}: the pass drew in the body");
            if name == "far" {
                // The selected button covers the body, and the rectangle over it is the lighter fill.
                let inside = (BODY.y..BODY.y + BODY.h).all(|y| (BODY.x..BODY.x + BODY.w).all(|x| c.px[(y * c.stride + x) as usize] == 210));
                assert!(inside, "the body is filled to its edges");
            }
        }
    }
    ui_set_theme(UiTheme::Gothic);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn drawing_keeps_to_the_clip_it_is_given_too() {
    let root = fresh_root("clip");
    let mut rt = installed("test.clip", &BOTH, &wide());
    // The caller composes a part of the screen that holds a corner of the body: only that corner is drawn.
    let part = Rect { x: 100, y: 100, w: 180, h: 120 };
    let before = patterned(700, 600);
    let mut c = before.clone();
    canvas_set_clip(&mut c, part);
    rt.draw(&mut c, BODY);
    let corner = rect_intersect(part, BODY);
    assert_eq!(changed_outside(&before, &c, corner), 0);
    assert!((corner.y..corner.y + corner.h).all(|y| (corner.x..corner.x + corner.w).all(|x| c.px[(y * c.stride + x) as usize] == 0)));
    assert_eq!(c.clip, part);
    // A part that holds none of the body: nothing is drawn.
    let mut c = before.clone();
    canvas_set_clip(&mut c, Rect { x: 0, y: 0, w: 150, h: 600 });
    rt.draw(&mut c, BODY);
    assert_eq!(changed_outside(&before, &c, RECT_EMPTY), 0);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_hit_region_is_the_part_of_it_inside_the_body() {
    let root = fresh_root("hits");
    let mut draw = Vec::new();
    draw.extend(hit(7, -100_000, -100_000, 300_000, 300_000));
    draw.extend(hit(8, 100, 50, 1000, 1000));
    draw.extend(hit(9, BODY.w, 0, 50, 50));
    draw.extend(hit(10, MAX, MAX, MAX, MAX));
    draw.extend(hit(11, -50, -50, 50, 50));
    draw.extend(hit(12, MIN, MIN, MAX, MAX));
    draw.extend(hit(13, 10, 10, -5, 20));
    draw.extend(status_is_state());
    let mut rt = installed("test.hits", &BOTH, &module(&draw, &tap_saves_id(), &[]));
    let mut c = patterned(700, 600);
    rt.draw(&mut c, BODY);
    assert_eq!(rt.hit(7), Some(Rect { x: 0, y: 0, w: BODY.w, h: BODY.h }));
    assert_eq!(rt.hit(8), Some(Rect { x: 100, y: 50, w: BODY.w - 100, h: BODY.h - 50 }));
    for outside in 9..=13 {
        assert_eq!(rt.hit(outside), None, "region {outside} has no part in the body");
    }
    assert_eq!(rt.hit_count(), 2);
    // A tap in the body reaches the region there; one beside the body (the frame's margin) reaches none.
    for ((x, y), want) in [((5, 5), "7"), ((BODY.w - 1, BODY.h - 1), "7"), ((-3, 5), "/"), ((5, -1), "/"), ((BODY.w, 5), "/"), ((5, BODY.h), "/"), ((MIN, MAX), "/")] {
        rt.tap(x, y);
        rt.draw(&mut c, BODY);
        assert_eq!(rt.status(), want, "a tap at {x},{y}");
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_package_without_the_storage_capability_has_no_saved_state() {
    let root = fresh_root("storage");
    let state_file = |id: &str| root.join(format!("packages/widgets/{id}/state/runtime.bin"));
    let mut c = patterned(700, 600);
    // Declared: the first draw finds nothing saved and saves a word, the next finds it, and so does a new load.
    let mut rt = installed("test.keeps", &BOTH, &keeps());
    rt.draw(&mut c, BODY);
    assert_eq!(rt.status(), "");
    assert_eq!(std::fs::read(state_file("test.keeps")).unwrap(), b"hello");
    rt.draw(&mut c, BODY);
    assert_eq!(rt.status(), "hello");
    let mut rt = WidgetRuntime::new("test.keeps").unwrap();
    rt.draw(&mut c, BODY);
    assert_eq!(rt.status(), "hello");
    // Not declared: state_get gives nothing, state_set keeps nothing, in a draw and in a tap, and no file is made.
    let mut rt = installed("test.forgets", &["editor:overlay"], &keeps());
    for _ in 0..3 {
        rt.draw(&mut c, BODY);
        assert_eq!(rt.error(), None);
        assert_eq!(rt.status(), "");
        assert_eq!(rt.hit_count(), 1);
        rt.tap(20, 20);
    }
    assert!(!state_file("test.forgets").exists());
    assert!(!state_file("test.forgets").parent().unwrap().exists(), "no state folder is made either");
    // A state file from a version of the package that declared the capability is not read, and stays as it is.
    std::fs::create_dir_all(state_file("test.forgets").parent().unwrap()).unwrap();
    std::fs::write(state_file("test.forgets"), b"kept").unwrap();
    let mut rt = WidgetRuntime::new("test.forgets").unwrap();
    rt.draw(&mut c, BODY);
    rt.tap(20, 20);
    rt.draw(&mut c, BODY);
    assert_eq!(rt.status(), "");
    assert_eq!(std::fs::read(state_file("test.forgets")).unwrap(), b"kept");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_package_without_the_overlay_capability_draws_nothing() {
    let root = fresh_root("overlay");
    // (no install gives such a package now: these are as an install before that rule left them)
    for (id, capabilities) in [("test.unseen", &["storage:widget-local"][..]), ("test.bare", &[][..])] {
        let mut rt = installed_earlier(id, capabilities, &keeps());
        let before = patterned(700, 600);
        let mut c = before.clone();
        for pass in 0..2 {
            rt.draw(&mut c, BODY);
            assert_eq!(rt.error(), None, "{id}");
            // The host clears the body; the module's button, its label and its hit region are not there.
            assert_eq!(changed_outside(&before, &c, BODY), 0, "{id}");
            assert_eq!(inked(&c, BODY), 0, "{id}");
            assert_eq!(rt.hit_count(), 0, "{id}");
            assert_eq!(rt.text(), "", "{id}");
            // (the calls that need no capability, and the storage when it is declared, are as ever)
            let saved = if pass == 1 && !capabilities.is_empty() { "hello" } else { "" };
            assert_eq!(rt.status(), saved, "{id}");
        }
    }
    // The same module with the capability draws its button.
    let mut rt = installed("test.seen", &BOTH, &keeps());
    let mut c = patterned(700, 600);
    rt.draw(&mut c, BODY);
    assert_ne!(inked(&c, BODY), 0);
    assert_eq!((rt.hit_count(), rt.text().as_str()), (1, "hello"));
    let _ = std::fs::remove_dir_all(&root);
}

/// What a frame says once a drawing pass of its module used up its instructions: the host's words, then the
/// interpreter's for its out-of-fuel trap.
const STOPPED: &str = "The widget stopped: all fuel consumed by WebAssembly";

#[test]
fn a_module_that_loops_for_ever_stops_when_its_instructions_are_used_up() {
    let root = fresh_root("loops");
    let before = patterned(700, 600);
    // A drawing pass that never ends: the call is ended when its instructions are used up, the frame says so,
    // nothing is drawn and the module is not called again.
    let mut rt = installed("test.loop.draw", &BOTH, &Wasm::new(forever(&[])).bytes());
    let mut c = before.clone();
    rt.draw(&mut c, BODY);
    assert_eq!(rt.error(), Some(STOPPED));
    assert_eq!(changed_outside(&before, &c, RECT_EMPTY), 0);
    rt.draw(&mut c, BODY);
    rt.tap(4, 4);
    assert_eq!(rt.error(), Some(STOPPED));
    // The same for a loop that draws on every turn (it ends by a limit of the pass or by its instructions,
    // whichever comes first, with the same words), and for a tap that never ends.
    let mut rt = installed("test.loop.rects", &BOTH, &Wasm::new(forever(&code(&rect(0, 0, 8, 8, 0)))).bytes());
    rt.draw(&mut c, BODY);
    assert_eq!(rt.error(), Some(STOPPED));
    let mut tapped = Wasm::new(code(&rect(0, 0, 8, 8, 0)));
    tapped.tap = forever(&[]);
    let mut rt = installed("test.loop.tap", &BOTH, &tapped.bytes());
    rt.draw(&mut c, BODY);
    assert_eq!(rt.error(), None);
    rt.tap(4, 4);
    assert_eq!(rt.error(), Some("The widget stopped while handling a tap."));
    // A start function that never ends: the module does not start, and the frame says why.
    let mut starts = Wasm::new(Vec::new());
    starts.start = Some(forever(&[]));
    let m = manifest("test.loop.start", "Test", &BOTH, FIXED_400_300, &starts.bytes());
    widget_package_install(&m, &starts.bytes(), "", 1860, 2480).unwrap();
    assert_eq!(WidgetRuntime::new("test.loop.start").err().as_deref(), Some("The widget could not start: all fuel consumed by WebAssembly"));
    // A function that calls itself for ever ends too: the interpreter's own call stack has a depth.
    let mut deep = Wasm::new(vec![0x10, FIRST_EXTRA as u8]);
    deep.extra = vec![vec![0x10, FIRST_EXTRA as u8]];
    let mut rt = installed("test.loop.deep", &BOTH, &deep.bytes());
    rt.draw(&mut c, BODY);
    assert!(rt.error().is_some_and(|e| e.starts_with("The widget stopped: ")), "{:?}", rt.error());
    // A long call that does end runs to its end: 20,000 calls of an import in a row, and a loop of 200,000 turns.
    let line: Vec<Op> = (0..20_000).flat_map(|_| [Op::Call("now"), Op::Drop]).collect();
    let mut rt = installed("test.loop.line", &BOTH, &draws(&line));
    rt.draw(&mut c, BODY);
    assert_eq!(rt.error(), None);
    let mut rt = installed("test.loop.turns", &BOTH, &Wasm::new([repeat(200_000, &[]), code(&rect(0, 0, 8, 8, 0))].concat()).bytes());
    rt.draw(&mut c, BODY);
    assert_eq!(rt.error(), None);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_pass_at_a_limit_runs_and_one_past_it_stops_as_out_of_instructions() {
    let root = fresh_root("limits");
    let stopped = STOPPED.to_string();
    let (commands, hits) = (WIDGET_PASS_COMMANDS_MAX, WIDGET_PASS_HITS_MAX);
    let (one, all, line) = (WIDGET_TEXT_MAX as i32, WIDGET_PASS_TEXT_MAX / WIDGET_TEXT_MAX, WIDGET_STATUS_MAX as i32);
    let state = WIDGET_STATE_MAX as i32;
    let moves = WIDGET_CALL_STATE_MAX / WIDGET_STATE_MAX;
    let small = rect(0, 0, 8, 8, 0);
    let region = hit(1, 0, 0, 8, 8);
    let label = |len: i32| text(0, 0, 200, 40, 0, len, 20, 0);
    let key = |len: i32| button(1, 0, 0, 200, 40, 0, len, 0);
    let join = |a: Vec<Op>, b: Vec<Op>| [a, b].concat();
    // (name, a pass at the limit, a pass one past it)
    let cases: Vec<(&str, Vec<Op>, Vec<Op>)> = vec![
        ("rectangles", times(commands, small.clone()), times(commands + 1, small.clone())),
        ("a text as the last command", join(times(commands - 1, small.clone()), label(4)), join(times(commands, small.clone()), label(4))),
        ("a button as the last command", join(times(commands - 1, small.clone()), key(4)), join(times(commands, small.clone()), key(4))),
        ("hit regions", times(hits, region.clone()), times(hits + 1, region.clone())),
        ("a button as the last region", join(times(hits - 1, region.clone()), key(4)), join(times(hits, region.clone()), key(4))),
        ("one text", label(one), label(one + 1)),
        ("one label", key(one), key(one + 1)),
        ("a text of every length an i32 can say", label(-1), label(i32::MAX)),
        ("all the texts of a pass", times(all, label(one)), join(times(all, label(one)), label(1))),
        ("all the texts, the last a label", join(times(all - 1, label(one)), key(one)), join(times(all, label(one)), key(1))),
        ("the status line", status(0, line), status(0, line + 1)),
        ("a status line of every length", status(0, -1), status(0, i32::MAX)),
        ("state written", times(moves, call("state_set", &[0, state])), times(moves + 1, call("state_set", &[0, state]))),
        (
            "state read",
            join(call("state_set", &[0, state]), times(moves - 1, join(call("state_get", &[0, state]), vec![Op::Drop]))),
            join(call("state_set", &[0, state]), times(moves, join(call("state_get", &[0, state]), vec![Op::Drop]))),
        ),
    ];
    for (i, (name, at, past)) in cases.into_iter().enumerate() {
        let before = patterned(700, 600);
        let mut c = before.clone();
        let mut rt = installed(&format!("test.at.{i}"), &BOTH, &draws(&at));
        for _ in 0..2 {
            // (twice: the counts start again with each pass)
            rt.draw(&mut c, BODY);
            assert_eq!(rt.error(), None, "{name}: a pass at the limit runs");
        }
        assert_eq!(changed_outside(&before, &c, BODY), 0, "{name}");
        let mut c = before.clone();
        let mut rt = installed(&format!("test.past.{i}"), &BOTH, &draws(&past));
        rt.draw(&mut c, BODY);
        assert_eq!(rt.error(), Some(stopped.as_str()), "{name}: a pass one past the limit stops");
        // Nothing of the stopped pass is drawn or kept, and the module is not called again.
        assert_eq!(changed_outside(&before, &c, RECT_EMPTY), 0, "{name}");
        assert_eq!((rt.hit_count(), rt.text().as_str(), rt.status()), (0, "", ""), "{name}");
        rt.draw(&mut c, BODY);
        rt.tap(4, 4);
        assert_eq!(rt.error(), Some(stopped.as_str()), "{name}");
        assert_eq!(changed_outside(&before, &c, RECT_EMPTY), 0, "{name}");
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// A module's load, by its memories (pages at the start) and its tables (entries at the start): None when it
/// loads, else what the frame would say.
fn load_of(id: &str, memories: &[u32], tables: &[u32]) -> Option<String> {
    let mut w = Wasm::new(Vec::new());
    w.memories = memories.iter().map(|&pages| (pages, None)).collect();
    w.tables = tables.iter().map(|&entries| (entries, None)).collect();
    let m = manifest(id, "Test", &BOTH, FIXED_400_300, &w.bytes());
    widget_package_install(&m, &w.bytes(), "", 1860, 2480).unwrap();
    WidgetRuntime::new(id).err()
}

#[test]
fn a_module_has_one_memory_and_one_table_of_a_bounded_length() {
    let root = fresh_root("store");
    let not_started = |e: Option<String>| e.is_some_and(|e| e.starts_with("The widget could not start: "));
    let (pages, entries) = (32 * 1024 * 1024 / 65536, WIDGET_TABLE_MAX as u32);
    // One memory of 32 MiB and one table of 65,536 entries are the most a module has.
    assert_eq!(load_of("test.store.most", &[pages], &[entries]), None);
    assert_eq!(load_of("test.store.none", &[1], &[]), None);
    // A second memory, however small; many memories; one memory a page larger than the limit.
    assert!(not_started(load_of("test.store.two", &[1, 1], &[])));
    assert!(not_started(load_of("test.store.many", &[512; 24], &[])));
    assert!(not_started(load_of("test.store.large", &[pages + 1], &[])));
    // A second table; one table an entry longer than the limit; a table of twenty million entries.
    assert!(not_started(load_of("test.store.tables", &[1], &[1, 1])));
    assert!(not_started(load_of("test.store.long", &[1], &[entries + 1])));
    assert!(not_started(load_of("test.store.longer", &[1], &[20_000_000])));
    // In a call, growing past a limit fails as WebAssembly defines it: the result is -1, nothing is given,
    // and the module runs on. The status line is one character for each try: '1' where the result was -1.
    let tried = |grow: &[u8], at: i32| [code(&[Op::I(at)]), grow.to_vec(), code(&[Op::I(-1)]), vec![0x46], code(&[Op::I(48), Op::Add, Op::Store8])].concat();
    let table_grow = |by: i32| [vec![0xd0, 0x70], code(&[Op::I(by)]), vec![0xfc, 0x0f, 0x00]].concat();
    let memory_grow = |by: i32| [code(&[Op::I(by)]), vec![0x40, 0x00]].concat();
    let mut w = Wasm::new(
        [
            tried(&table_grow(entries as i32), 0),     // 1 + 65,536 entries: past the limit
            tried(&table_grow(30_000_000), 1),         // far past it
            tried(&table_grow(entries as i32 - 1), 2), // to the limit exactly: given
            tried(&table_grow(1), 3),                  // and one more: not given
            tried(&memory_grow(pages as i32), 4),      // 1 + 512 pages: past the limit
            tried(&memory_grow(pages as i32 - 1), 5),  // to the limit exactly: given
            tried(&memory_grow(1), 6),                 // and one more: not given
            code(&status(0, 7)),
        ]
        .concat(),
    );
    w.tables = vec![(1, None)];
    let mut rt = installed("test.store.grow", &BOTH, &w.bytes());
    let mut c = patterned(700, 600);
    rt.draw(&mut c, BODY);
    assert_eq!(rt.error(), None);
    assert_eq!(rt.status(), "1101101");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_limits_hold_in_a_tap_and_a_tap_leaves_nothing_for_the_next_pass() {
    let root = fresh_root("tap-limits");
    let before = patterned(700, 600);
    // A tap that draws: what it recorded is dropped when the next call starts, so the pass that follows holds its
    // own commands only and may be as long as any other.
    let black = button(1, -50, -50, 2000, 2000, 0, 0, 1);
    let full = times(WIDGET_PASS_COMMANDS_MAX - 1, black.clone());
    let mut rt = installed("test.tap.draws", &BOTH, &module(&times(WIDGET_PASS_COMMANDS_MAX, rect(10, 10, 20, 20, 0)), &full, &[]));
    let mut c = before.clone();
    rt.draw(&mut c, BODY);
    rt.tap(15, 15);
    rt.draw(&mut c, BODY);
    assert_eq!(rt.error(), None);
    assert_eq!(rt.hit_count(), 0, "the tap's buttons are no regions of the pass");
    assert!(c.px[((BODY.y + 100) * c.stride + BODY.x + 100) as usize] == 255, "the tap's buttons are not drawn");
    // A tap past a limit stops the widget as any tap that traps does.
    let mut rt = installed("test.tap.past", &BOTH, &module(&[], &times(WIDGET_PASS_COMMANDS_MAX + 1, rect(0, 0, 8, 8, 0)), &[]));
    rt.draw(&mut c, BODY);
    assert_eq!(rt.error(), None);
    rt.tap(1, 1);
    assert_eq!(rt.error(), Some("The widget stopped while handling a tap."));
    rt.draw(&mut c, BODY);
    assert_eq!(rt.error(), Some("The widget stopped while handling a tap."));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn lengths_are_held_before_memory_is_read() {
    let root = fresh_root("lengths");
    let mut c = patterned(700, 600);
    // state_set keeps the first 64 KiB of what it is given, whatever length it is told; a setting's key longer
    // than any key is no setting's; state_get copies no more than there is.
    let mut draw = status_is_state();
    draw.extend(call("state_set", &[WORD_AT, i32::MAX]));
    draw.extend(call("setting", &[0, i32::MAX, 0, 16]));
    draw.push(Op::Drop);
    draw.extend(call("setting", &[0, -1, 0, i32::MAX]));
    draw.push(Op::Drop);
    draw.extend(call("state_get", &[100, i32::MAX]));
    draw.push(Op::Drop);
    let mut rt = installed("test.lengths", &BOTH, &module(&draw, &[], &[(WORD_AT as u32, b"hello")]));
    rt.draw(&mut c, BODY);
    assert_eq!(rt.error(), None);
    let saved = std::fs::read(root.join("packages/widgets/test.lengths/state/runtime.bin")).unwrap();
    // (the module's one page of memory ends before 64 KiB from the word are read: the state is what was there)
    assert_eq!(saved.len(), 65536 - WORD_AT as usize);
    assert!(saved.len() <= WIDGET_STATE_MAX && saved.starts_with(b"hello"));
    rt.draw(&mut c, BODY);
    assert_eq!(rt.error(), None);
    assert!(rt.status().starts_with("hello"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn request_redraw_has_a_shortest_and_a_longest_delay() {
    let root = fresh_root("redraw");
    let mut c = patterned(700, 600);
    // (times are the app's clock, `now_ms`: milliseconds that wrap)
    let after = |t: u32, ms: i32| t.wrapping_add(ms as u32);
    for (i, (asked, given)) in [(i32::MIN, WIDGET_REDRAW_MIN_MS), (-5, WIDGET_REDRAW_MIN_MS), (0, WIDGET_REDRAW_MIN_MS), (1, WIDGET_REDRAW_MIN_MS), (16, WIDGET_REDRAW_MIN_MS), (WIDGET_REDRAW_MIN_MS, WIDGET_REDRAW_MIN_MS), (WIDGET_REDRAW_MIN_MS + 1, WIDGET_REDRAW_MIN_MS + 1), (5000, 5000), (i32::MAX, WIDGET_REDRAW_MAX_MS)].into_iter().enumerate() {
        let mut rt = installed(&format!("test.redraw.{i}"), &BOTH, &draws(&call("request_redraw", &[asked])));
        let t0 = now_ms();
        assert!(!rt.redraw_due(after(t0, WIDGET_REDRAW_MAX_MS)), "nothing is asked for before the first pass");
        rt.draw(&mut c, BODY);
        let t1 = now_ms();
        assert!(!rt.redraw_due(after(t0, given - 1)), "request_redraw({asked}) is not due before {given} ms");
        assert!(rt.redraw_due(after(t1, given)), "request_redraw({asked}) is due after {given} ms");
        // The request is taken once, when it is due, and not before; the next pass's request is another.
        assert!(!rt.redraw_taken(after(t0, given - 1)));
        assert!(rt.redraw_taken(after(t1, given)));
        assert!(!rt.redraw_taken(after(t1, given)) && !rt.redraw_due(after(t1, WIDGET_REDRAW_MAX_MS)), "taken once");
        rt.draw(&mut c, BODY);
        assert!(rt.redraw_due(after(now_ms(), given)), "the next pass asks again");
    }
    assert_eq!(WIDGET_REDRAW_MIN_MS, 100);
    // A module that asked and then stopped is not drawn again: nothing stays due.
    let mut rt = installed("test.redraw.stops", &BOTH, &draws(&[call("request_redraw", &[100]), times(WIDGET_PASS_COMMANDS_MAX + 1, rect(0, 0, 8, 8, 0))].concat()));
    rt.draw(&mut c, BODY);
    assert_eq!(rt.error(), Some(STOPPED));
    assert!(!rt.redraw_due(after(now_ms(), WIDGET_REDRAW_MAX_MS)));
    let _ = std::fs::remove_dir_all(&root);
}

/// The calculator package of tests/fixtures/widgetcalc: its manifest and its module.
fn calculator() -> (String, Vec<u8>) {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests/fixtures/widgetcalc/packages/widgets/com.grimoire.calculator");
    (std::fs::read_to_string(dir.join("package.yaml")).unwrap(), std::fs::read(dir.join("calculator.bin")).unwrap())
}

#[test]
fn a_widget_manifest_declares_the_overlay_capability() {
    let root = fresh_root("declares");
    let refused = "`capabilities` must declare `editor:overlay`; without it nothing a widget draws is shown";
    let (fixture, module) = calculator();
    let both = "capabilities: [editor:overlay, storage:widget-local]\n";
    assert!(fixture.contains(both));
    // The calculator's manifest as its repository had it at version 1.2.0: the fixture's with its repository's
    // address and an empty list of capabilities. The manifest check refuses it, which is before anything is
    // asked of the reader or written.
    let with_address = fixture.replace("license: MIT\n", "license: MIT\nrepository: https://github.com/nebuff/gri-calc\n");
    let published = with_address.replace(both, "capabilities: []\n");
    let folder = root.join("packages/widgets/com.grimoire.calculator");
    for (what, m) in [("an empty list", published.clone()), ("no list", with_address.replace(both, "")), ("the storage alone", with_address.replace(both, "capabilities: [storage:widget-local]\n"))] {
        let p = widget_package_parse(&m).unwrap();
        assert_eq!(widget_package_validate(&p, 1860, 2480), Err(refused.to_string()), "{what}");
        assert_eq!(widget_package_install(&m, &module, "", 1860, 2480).err().as_deref(), Some(refused), "{what}");
        assert!(!folder.exists() && widget_package_installed("com.grimoire.calculator").is_none(), "{what}: nothing is installed");
    }
    // The same manifest with both capabilities installs, and its frame draws: the calculator's display and keys.
    widget_package_install(&with_address, &module, "", 1860, 2480).unwrap();
    let mut h = WidgetHost::new();
    let area = Rect { x: 0, y: 0, w: 1860, h: 2480 };
    assert!(h.spawn(widget_def("com.grimoire.calculator").unwrap(), area, Rect { x: 1700, y: 220, w: 128, h: 1700 }));
    let body = h.frames[0].body_rect();
    let mut rt = WidgetRuntime::new("com.grimoire.calculator").unwrap();
    let before = patterned(1860, 2480);
    let mut c = before.clone();
    rt.draw(&mut c, body);
    assert_eq!(rt.error(), None);
    assert!(inked(&c, body) > 10_000 && rt.hit_count() > 10 && rt.text().contains('7'), "{} dark pixels, {} hit regions, {:?}", inked(&c, body), rt.hit_count(), rt.text());
    assert_eq!(changed_outside(&before, &c, body), 0);
    // (and it keeps its state: a tap on a key, a pass, and the file is there)
    let key = rt.hit(rt.hit_count() as i32 / 2).or_else(|| rt.hit(0)).or_else(|| rt.hit(1)).unwrap();
    rt.tap(key.x + key.w / 2, key.y + key.h / 2);
    rt.draw(&mut c, body);
    assert_eq!(rt.error(), None);
    assert!(folder.join("state/runtime.bin").exists());
    // With the capability to draw alone it installs and draws too, and keeps no state.
    widget_package_remove("com.grimoire.calculator", false).unwrap();
    widget_package_install(&with_address.replace(both, "capabilities: [editor:overlay]\n"), &module, "", 1860, 2480).unwrap();
    let mut rt = WidgetRuntime::new("com.grimoire.calculator").unwrap();
    let mut c = before.clone();
    rt.draw(&mut c, body);
    assert!(rt.error().is_none() && inked(&c, body) > 10_000 && rt.hit_count() > 10);
    assert!(!folder.join("state").exists());
    widget_catalog_invalidate();
    let _ = std::fs::remove_dir_all(&root);
}

/// What a frame says when its module's file fails the check before a load.
const NOT_READ: &str = "The widget's artifact could not be read.";

/// The error of a load of the package, None when it loads.
fn load(id: &str) -> Option<String> {
    WidgetRuntime::new(id).err()
}

/// A file of the data root with `from` replaced by `to`, as a hand with an editor would.
fn rewrite(path: &std::path::Path, from: &str, to: &str) {
    let text = std::fs::read_to_string(path).unwrap();
    assert!(text.contains(from), "{from} in {path:?}");
    std::fs::write(path, text.replace(from, to)).unwrap();
}

#[test]
fn a_module_is_checked_again_before_each_load() {
    use std::os::unix::fs::symlink;
    let root = fresh_root("load");
    let (module, other) = (keeps(), wide());
    drop(installed("test.load", &BOTH, &module));
    let dir = root.join("packages/widgets/test.load");
    let (file, yaml, index) = (dir.join("module.bin"), dir.join("package.yaml"), root.join("packages/index.json"));
    assert_eq!(load("test.load"), None);

    // The file's bytes are not the ones installed: one bit changed, another module, nothing, no file.
    let mut bit = module.clone();
    *bit.last_mut().unwrap() ^= 1;
    for bytes in [&bit[..], &other[..], &[][..]] {
        std::fs::write(&file, bytes).unwrap();
        assert_eq!(load("test.load").as_deref(), Some(NOT_READ));
    }
    std::fs::remove_file(&file).unwrap();
    assert_eq!(load("test.load").as_deref(), Some(NOT_READ));

    // A link in the file's place is not followed, whether it leads to the same bytes inside the folder or outside.
    let (inside, outside) = (dir.join("copy.bin"), root.join("packages/widgets/elsewhere.bin"));
    std::fs::write(&inside, &module).unwrap();
    std::fs::write(&outside, &module).unwrap();
    for target in [std::path::Path::new("copy.bin"), &inside, &outside] {
        symlink(target, &file).unwrap();
        assert_eq!(load("test.load").as_deref(), Some(NOT_READ), "a link to {target:?}");
        std::fs::remove_file(&file).unwrap();
    }
    std::fs::write(&file, &module).unwrap();
    assert_eq!(load("test.load"), None);

    // The installed manifest names another file: outside the folder, by an absolute path, the manifest, the state,
    // a name the walk does not take, or a file of the folder that the index does not name.
    std::fs::create_dir_all(dir.join("state")).unwrap();
    std::fs::write(dir.join("state/runtime.bin"), &module).unwrap();
    std::fs::write(dir.join(".module.bin"), &module).unwrap();
    let absolute = outside.to_str().unwrap().to_string();
    for path in ["../elsewhere.bin", "./module.bin", "x/../module.bin", absolute.as_str(), "/module.bin", "package.yaml", "state/runtime.bin", ".module.bin", "copy.bin", ""] {
        rewrite(&yaml, "path: module.bin", &format!("path: {path}"));
        assert_eq!(load("test.load").as_deref(), Some(NOT_READ), "artifact.path: {path}");
        rewrite(&yaml, &format!("path: {path}"), "path: module.bin");
    }
    // (the index and the manifest naming that file together, as an install writes them, load it)
    rewrite(&yaml, "path: module.bin", "path: copy.bin");
    rewrite(&index, "test.load/module.bin", "test.load/copy.bin");
    assert_eq!(load("test.load"), None);
    rewrite(&yaml, "path: copy.bin", "path: module.bin");
    assert_eq!(load("test.load").as_deref(), Some(NOT_READ), "the index names another file than the manifest");
    rewrite(&index, "test.load/copy.bin", "test.load/module.bin");
    assert_eq!(load("test.load"), None);

    // The index's record of the file: another digest, none, and the same one in capitals.
    let sha = grimoire::sha256::sha256_of(&module);
    for (recorded, loads) in [(grimoire::sha256::sha256_of(&other), false), (String::new(), false), (sha[..63].to_string(), false), (sha.to_uppercase(), true)] {
        rewrite(&index, &format!("\"sha\":\"{sha}\""), &format!("\"sha\":\"{recorded}\""));
        assert_eq!(load("test.load").is_none(), loads, "sha {recorded}");
        rewrite(&index, &format!("\"sha\":\"{recorded}\""), &format!("\"sha\":\"{sha}\""));
    }

    // A folder on the way to the file, and the package's own folder, replaced by a link to one with the same files.
    let m = manifest("test.deep", "Test", &BOTH, FIXED_400_300, &module).replace("path: module.bin", "path: dist/module.bin");
    widget_package_install(&m, &module, "", 1860, 2480).unwrap();
    assert_eq!(load("test.deep"), None);
    let deep = root.join("packages/widgets/test.deep");
    std::fs::rename(deep.join("dist"), deep.join("real")).unwrap();
    symlink("real", deep.join("dist")).unwrap();
    assert_eq!(load("test.deep").as_deref(), Some(NOT_READ), "a folder of the path is a link");
    std::fs::remove_file(deep.join("dist")).unwrap();
    std::fs::rename(deep.join("real"), deep.join("dist")).unwrap();
    assert_eq!(load("test.deep"), None);
    std::fs::rename(&deep, root.join("packages/widgets/moved")).unwrap();
    symlink("moved", &deep).unwrap();
    assert!(load("test.deep").is_some(), "the package's folder is a link");
    std::fs::remove_file(&deep).unwrap();
    std::fs::rename(root.join("packages/widgets/moved"), &deep).unwrap();
    assert_eq!(load("test.deep"), None);

    // The check is the load's: a frame that has loaded its module draws without reading the file again.
    let mut rt = WidgetRuntime::new("test.load").unwrap();
    std::fs::write(&file, &other).unwrap();
    let mut c = patterned(700, 600);
    rt.draw(&mut c, BODY);
    rt.draw(&mut c, BODY);
    assert_eq!(rt.error(), None);
    assert_eq!(load("test.load").as_deref(), Some(NOT_READ), "the next load checks it");
    let _ = std::fs::remove_dir_all(&root);
}

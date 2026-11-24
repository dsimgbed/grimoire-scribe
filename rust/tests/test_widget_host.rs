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

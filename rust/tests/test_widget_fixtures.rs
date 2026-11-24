// test_widget_fixtures.rs: the installed widget packages the scenarios copy (tests/fixtures/widgethost/) are what
// tests/widgetfix makes now (as test_font_fixtures.rs checks the fonts). `make widget-fixtures` writes them
// (GRIMOIRE_WRITE_WIDGET_FIXTURES=<dir>).
mod widgetfix;

fn dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests/fixtures/widgethost")
}

#[test]
fn committed_widget_fixtures_are_what_the_generator_makes() {
    if let Ok(out) = std::env::var("GRIMOIRE_WRITE_WIDGET_FIXTURES") {
        let _ = std::fs::remove_dir_all(&out);
        std::fs::create_dir_all(&out).unwrap();
        for (name, bytes) in widgetfix::scenario_files() {
            let p = std::path::Path::new(&out).join(name);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, bytes).unwrap();
        }
        return;
    }
    // (and nothing else lies there: a stale fixture would be copied into a scenario)
    let mut have = Vec::new();
    fn walk(d: &std::path::Path, base: &std::path::Path, out: &mut Vec<String>) {
        for e in std::fs::read_dir(d).unwrap().flatten() {
            if e.path().is_dir() {
                walk(&e.path(), base, out);
            } else {
                out.push(e.path().strip_prefix(base).unwrap().to_string_lossy().into_owned());
            }
        }
    }
    walk(&dir(), &dir(), &mut have);
    have.sort();
    let mut want: Vec<String> = widgetfix::scenario_files().iter().map(|f| f.0.to_string()).collect();
    want.sort();
    assert_eq!(have, want, "tests/fixtures/widgethost holds what the generator makes, no more: make widget-fixtures");
    for (name, bytes) in widgetfix::scenario_files() {
        let on_disk = std::fs::read(dir().join(&name)).unwrap_or_else(|_| panic!("tests/fixtures/widgethost/{name} is missing: make widget-fixtures"));
        assert!(on_disk == bytes, "tests/fixtures/widgethost/{name} differs from what tests/widgetfix makes: make widget-fixtures");
    }
}

#[test]
fn the_generator_is_deterministic_and_its_modules_are_webassembly() {
    assert_eq!(widgetfix::wide(), widgetfix::wide());
    assert_eq!(widgetfix::longest_id().len(), grimoire::widgetpkg::WIDGET_ID_MAX);
    assert!(grimoire::widgetpkg::widget_id_ok(&widgetfix::longest_id()));
    for p in widgetfix::packages() {
        assert!(p.module.starts_with(b"\0asm\x01\0\0\0"), "{}", p.id);
        assert!(p.manifest.contains(&format!("id: {}\n", p.id)));
    }
}

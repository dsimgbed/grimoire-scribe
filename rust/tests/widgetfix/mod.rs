// widgetfix: the WebAssembly modules the widget host's tests and scenarios run, written here byte by byte (no
// compiler, no file from anywhere else), and the installed packages the scenarios copy
// (tests/fixtures/widgethost/: `make widget-fixtures` writes them; test_widget_fixtures.rs checks the
// committed bytes are what this makes now).
//
// Every module here imports the ten functions of the `grimoire` ABI (docs/WIDGETS.md), exports one page of
// memory, `widget_draw` and `widget_tap`, and each of the two is a straight line of the steps it is given
// (`Op`): constants pushed, imports called. That is enough to pass the host any numbers.
#![allow(dead_code)]

use grimoire::sha256::sha256_of;

/// One step of a function's body.
#[derive(Clone, Debug)]
pub enum Op {
    /// i32.const
    I(i32),
    /// a call of the import with this name
    Call(&'static str),
    /// local.get: a parameter (widget_draw's width, height and scale; widget_tap's hit id)
    Arg(u32),
    /// drop: a result that is not used
    Drop,
    /// i32.add
    Add,
    /// i32.store8: the value on top to the address under it
    Store8,
    /// a loop that never ends
    Forever,
}

/// The ABI's imports in the order a module here imports them (a function's index), each with its type's index.
const IMPORTS: [(&str, u8); 10] = [
    ("rect", 0),
    ("text", 1),
    ("button", 1),
    ("hit", 0),
    ("status", 2),
    ("request_redraw", 3),
    ("setting", 4),
    ("state_get", 5),
    ("state_set", 2),
    ("now", 6),
];
/// The function types: how many i32 parameters, and the result (0x7f an i32, 0x7e an i64, 0 none).
const TYPES: [(usize, u8); 8] = [(5, 0), (8, 0), (2, 0), (1, 0), (4, 0x7f), (2, 0x7f), (0, 0x7e), (3, 0)];
const TYPE_DRAW: u8 = 7;
const TYPE_TAP: u8 = 3;

fn uleb(out: &mut Vec<u8>, mut v: u32) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

fn sleb(out: &mut Vec<u8>, mut v: i32) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if (v == 0 && b & 0x40 == 0) || (v == -1 && b & 0x40 != 0) {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

fn name(out: &mut Vec<u8>, s: &str) {
    uleb(out, s.len() as u32);
    out.extend_from_slice(s.as_bytes());
}

fn section(out: &mut Vec<u8>, id: u8, body: &[u8]) {
    out.push(id);
    uleb(out, body.len() as u32);
    out.extend_from_slice(body);
}

fn body(ops: &[Op]) -> Vec<u8> {
    let mut code = vec![0u8]; // no locals
    for op in ops {
        match op {
            Op::I(v) => {
                code.push(0x41);
                sleb(&mut code, *v);
            }
            Op::Call(import) => {
                let at = IMPORTS.iter().position(|(n, _)| n == import).unwrap_or_else(|| panic!("no import {import}"));
                code.push(0x10);
                uleb(&mut code, at as u32);
            }
            Op::Arg(n) => {
                code.push(0x20);
                uleb(&mut code, *n);
            }
            Op::Drop => code.push(0x1a),
            Op::Add => code.push(0x6a),
            Op::Store8 => code.extend_from_slice(&[0x3a, 0x00, 0x00]),
            Op::Forever => code.extend_from_slice(&[0x03, 0x40, 0x0c, 0x00, 0x0b]),
        }
    }
    code.push(0x0b);
    let mut out = Vec::new();
    uleb(&mut out, code.len() as u32);
    out.extend_from_slice(&code);
    out
}

/// A module whose `widget_draw` and `widget_tap` run these steps, with `data` in its memory (address, bytes).
pub fn module(draw: &[Op], tap: &[Op], data: &[(u32, &[u8])]) -> Vec<u8> {
    let mut out = b"\0asm\x01\0\0\0".to_vec();
    let mut s = Vec::new();
    uleb(&mut s, TYPES.len() as u32);
    for (params, result) in TYPES {
        s.push(0x60);
        uleb(&mut s, params as u32);
        s.extend(std::iter::repeat_n(0x7f, params));
        if result == 0 {
            s.push(0);
        } else {
            s.extend_from_slice(&[1, result]);
        }
    }
    section(&mut out, 1, &s);
    s.clear();
    uleb(&mut s, IMPORTS.len() as u32);
    for (import, ty) in IMPORTS {
        name(&mut s, "grimoire");
        name(&mut s, import);
        s.extend_from_slice(&[0x00, ty]);
    }
    section(&mut out, 2, &s);
    section(&mut out, 3, &[2, TYPE_DRAW, TYPE_TAP]);
    section(&mut out, 5, &[1, 0x00, 1]); // one memory, one page, no maximum
    s.clear();
    s.push(3);
    name(&mut s, "memory");
    s.extend_from_slice(&[0x02, 0]);
    name(&mut s, "widget_draw");
    s.extend_from_slice(&[0x00, IMPORTS.len() as u8]);
    name(&mut s, "widget_tap");
    s.extend_from_slice(&[0x00, IMPORTS.len() as u8 + 1]);
    section(&mut out, 7, &s);
    s.clear();
    s.push(2);
    s.extend_from_slice(&body(draw));
    s.extend_from_slice(&body(tap));
    section(&mut out, 10, &s);
    if !data.is_empty() {
        s.clear();
        uleb(&mut s, data.len() as u32);
        for (at, bytes) in data {
            s.extend_from_slice(&[0x00, 0x41]);
            sleb(&mut s, *at as i32);
            s.push(0x0b);
            uleb(&mut s, bytes.len() as u32);
            s.extend_from_slice(bytes);
        }
        section(&mut out, 11, &s);
    }
    out
}

fn call(import: &'static str, args: &[i32]) -> Vec<Op> {
    let mut v: Vec<Op> = args.iter().map(|&a| Op::I(a)).collect();
    v.push(Op::Call(import));
    v
}

pub fn rect(x: i32, y: i32, w: i32, h: i32, style: i32) -> Vec<Op> {
    call("rect", &[x, y, w, h, style])
}
#[allow(clippy::too_many_arguments)]
pub fn text(x: i32, y: i32, w: i32, h: i32, ptr: i32, len: i32, px: i32, center: i32) -> Vec<Op> {
    call("text", &[x, y, w, h, ptr, len, px, center])
}
#[allow(clippy::too_many_arguments)]
pub fn button(id: i32, x: i32, y: i32, w: i32, h: i32, ptr: i32, len: i32, style: i32) -> Vec<Op> {
    call("button", &[id, x, y, w, h, ptr, len, style])
}
pub fn hit(id: i32, x: i32, y: i32, w: i32, h: i32) -> Vec<Op> {
    call("hit", &[id, x, y, w, h])
}
pub fn status(ptr: i32, len: i32) -> Vec<Op> {
    call("status", &[ptr, len])
}

/// Where the modules here keep the saved state they read back, and the byte a tap writes.
pub const STATE_AT: i32 = 60000;
pub const TAP_AT: i32 = 61000;

/// `widget_draw`'s last steps in the modules that say which hit region was tapped: the saved state read into
/// memory and passed on as the status line.
pub fn status_is_state() -> Vec<Op> {
    vec![Op::I(STATE_AT), Op::I(STATE_AT), Op::I(64), Op::Call("state_get"), Op::Call("status")]
}

/// `widget_tap` for those modules: the hit id as one character ('0' + id, so '/' for -1, a tap on no region)
/// saved as the state.
pub fn tap_saves_id() -> Vec<Op> {
    vec![Op::I(TAP_AT), Op::Arg(0), Op::I(48), Op::Add, Op::Store8, Op::I(TAP_AT), Op::I(1), Op::Call("state_set")]
}

/// A package's manifest for `artifact`, its file `module.bin`. `size` is the lines under `spec.size`.
pub fn manifest(id: &str, name: &str, capabilities: &[&str], size: &str, artifact: &[u8]) -> String {
    format!(
        "name: {name}\nid: {id}\ntype: widget\nversion: 1.0.0\nauthor: Grimoire tests\ndescription: A module of the widget host's tests.\n\
         platforms: [all]\nartifact:\n  path: module.bin\n  sha256: {}\n  size: {}\ncapabilities: [{}]\n\
         spec:\n  runtime: wasm\n  size:\n{size}",
        sha256_of(artifact),
        artifact.len(),
        capabilities.join(", "),
    )
}

pub const FIXED_400_300: &str = "    mode: fixed\n    default: [400, 300]\n";
pub const BOTH: [&str; 2] = ["editor:overlay", "storage:widget-local"];

/// A package of the scenarios' data root: its id, name, manifest and module.
pub struct Package {
    pub id: &'static str,
    pub name: &'static str,
    pub manifest: String,
    pub module: Vec<u8>,
}

fn package(id: &'static str, name: &'static str, capabilities: &[&str], size: &str, module: Vec<u8>) -> Package {
    Package { id, name, manifest: manifest(id, name, capabilities, size, &module), module }
}

/// A module that draws far outside its body: a selected (black) button from far above and left of the body to
/// far below and right of it, under a hit region as large. A tap saves which region it hit, and the next draw
/// says so in the status line.
pub fn wide() -> Vec<u8> {
    let mut draw = hit(7, -100_000, -100_000, 300_000, 300_000);
    draw.extend(button(1, -5000, -5000, 20_000, 20_000, 0, 0, 1));
    draw.extend(status_is_state());
    module(&draw, &tap_saves_id(), &[])
}

/// The packages of tests/fixtures/widgethost, in the order of its index.
pub fn packages() -> Vec<Package> {
    vec![package("test.host.wide", "Wide", &BOTH, FIXED_400_300, wide())]
}

/// tests/fixtures/widgethost: an installed data root's `packages` folder, each file's path and bytes.
pub fn scenario_files() -> Vec<(String, Vec<u8>)> {
    let mut files = Vec::new();
    let mut index = String::from("{\n  \"version\": 1,\n  \"packages\": [");
    for (i, p) in packages().iter().enumerate() {
        let dir = format!("packages/widgets/{}", p.id);
        index.push_str(if i > 0 { ",\n    " } else { "\n    " });
        index.push_str(&format!(
            "{{\"id\":\"{}\",\"name\":\"{}\",\"version\":\"1.0.0\",\"author\":\"Grimoire tests\",\"description\":\"A module of the widget host's tests.\",\"icon\":\"\",\"path\":\"{dir}/module.bin\",\"sha\":\"{}\",\"installed_at\":1,\"source_url\":\"\"}}",
            p.id,
            p.name,
            sha256_of(&p.module),
        ));
        files.push((format!("{dir}/package.yaml"), p.manifest.clone().into_bytes()));
        files.push((format!("{dir}/module.bin"), p.module.clone()));
    }
    index.push_str("\n  ]\n}\n");
    files.push(("packages/index.json".to_string(), index.into_bytes()));
    files
}

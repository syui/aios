// aish-pkg: aios のパッケージを最新にする (人は M-p、Claude は MCP のツール)
//   pkg_check  配布元の最新の版といまの pkgver をくらべる (新しいものだけ。all で全部)
//   pkg_edit   PKGBUILD (pkgver、pkgrel、sha256sums) と .aios.json を、配布元の最新か ver に書きかえる
//   M-p        pkg_check を画面に出す (キーを押すと消える)
// コマンドとしても動く (大きな tarball の edit は時間がかかるので、run の bg で):
//   aish-pkg check [NAME...] [--all] [--refresh]   aish-pkg edit NAME [VER]
// リポジトリは dir か、いまのディレクトリから上にたどって pkg/ があるところ、なければ $AIOS_SRC か /usr/src/aios
mod up;

use aish_plugin::{Spec, Tool, Tty, Value, error, json, s};
use std::path::{Path, PathBuf};

const CHECK: &str = r#"{"type":"object","properties":{"names":{"type":"array","items":{"type":"string"},"description":"見るパッケージ (なければ全部)"},"all":{"type":"boolean","description":"最新のものも出す (既定: 新しい版があるものとしくじったものだけ)"},"refresh":{"type":"boolean","description":"どこを見るか (pkg/upstream.json) を PKGBUILD と Arch の .nvchecker.toml から作りなおす"},"dir":{"type":"string","description":"aios のリポジトリ (既定: いまのディレクトリから上へ探す)"}}}"#;
const EDIT: &str = r#"{"type":"object","properties":{"name":{"type":"string","description":"パッケージ"},"ver":{"type":"string","description":"版 (なければ配布元の最新)"},"dir":{"type":"string"}},"required":["name"]}"#;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if matches!(args.first().map(String::as_str), Some("check" | "edit")) {
        std::process::exit(cli(&args));
    }
    let spec = Spec {
        name: "pkg",
        hooks: &["key"],
        keys: &[("M-p", "check")],
        tools: &[
            Tool { name: "pkg_check", desc: "aios のパッケージ (pkg/*/NAME/PKGBUILD) の配布元の最新の版を見て、いまの pkgver とくらべる。1 行に 1 つ: NAME いま → 最新", input: CHECK },
            Tool { name: "pkg_edit", desc: "PKGBUILD の pkgver を配布元の最新 (か ver) にし、pkgrel を 1 に、sha256sums を取ってきたもので書きかえ、.aios.json の版と updated も変える。大きな tarball は時間がかかるので run の bg で aish-pkg edit NAME", input: EDIT },
        ],
    };
    aish_plugin::run(spec, |ev, v| match ev {
        "tool" => {
            let a = &v["args"];
            let root = root(a["dir"].as_str(), s(v, "pwd"));
            match s(v, "name") {
                "pkg_check" => {
                    let names: Vec<String> = a["names"].as_array().map(|x| x.iter().filter_map(|n| n.as_str().map(String::from)).collect()).unwrap_or_default();
                    check(&root, &names, a["all"] == true, a["refresh"] == true)
                }
                "pkg_edit" => match up::edit(&root, s(a, "name"), a["ver"].as_str()) {
                    Ok(r) => r,
                    Err(e) => error(e),
                },
                n => error(format!("{}: no such tool", n)),
            }
        }
        "key" => key(&root(None, s(v, "pwd"))),
        _ => json!({}),
    });
}

/// aios のリポジトリ
fn root(dir: Option<&str>, pwd: &str) -> PathBuf {
    if let Some(d) = dir {
        return PathBuf::from(d);
    }
    let start = if pwd.is_empty() { std::env::current_dir().unwrap_or_default() } else { PathBuf::from(pwd) };
    for d in start.ancestors() {
        if d.join("pkg").is_dir() && !up::pkgbuilds(&d.join("pkg")).is_empty() {
            return d.to_path_buf();
        }
    }
    PathBuf::from(std::env::var("AIOS_SRC").unwrap_or_else(|_| "/usr/src/aios".into()))
}

/// check の答え: text は新しいもの (all なら全部) としくじったもの
fn check(root: &Path, names: &[String], all: bool, refresh: bool) -> Value {
    let rs = match up::check(&root.join("pkg"), names, refresh) {
        Ok(r) => r,
        Err(e) => return error(e),
    };
    let w = rs.iter().filter_map(|r| r["name"].as_str()).map(str::len).max().unwrap_or(0);
    let (mut new, mut same, mut bad) = (0, 0, 0);
    let mut text = String::new();
    for r in &rs {
        let (n, cur) = (r["name"].as_str().unwrap_or(""), r["pkgver"].as_str().unwrap_or(""));
        if let Some(e) = r["error"].as_str() {
            bad += 1;
            text.push_str(&format!("{:w$}  {}  ? {}\n", n, cur, e));
        } else if r["new"] == true {
            new += 1;
            text.push_str(&format!("{:w$}  {} → {}\n", n, cur, r["latest"].as_str().unwrap_or("")));
        } else {
            same += 1;
            if all {
                text.push_str(&format!("{:w$}  {}\n", n, cur));
            }
        }
    }
    let news: Vec<&Value> = rs.iter().filter(|r| r["new"] == true).map(|r| &r["name"]).collect();
    json!({ "text": text, "new": news, "latest": same, "errors": bad, "count": new + same + bad })
}

fn cli(args: &[String]) -> i32 {
    let root = root(None, "");
    let rest: Vec<String> = args[1..].iter().filter(|a| !a.starts_with("--")).cloned().collect();
    let r = match args[0].as_str() {
        "check" => check(&root, &rest, args.iter().any(|a| a == "--all"), args.iter().any(|a| a == "--refresh")),
        _ => match rest.first() {
            Some(n) => up::edit(&root, n, rest.get(1).map(String::as_str)).unwrap_or_else(error),
            None => error("aish-pkg edit NAME [VER]"),
        },
    };
    if let Some(t) = r["text"].as_str() {
        print!("{}", t);
        eprintln!("{} new, {} latest, {} errors", r["new"].as_array().map_or(0, |a| a.len()), r["latest"], r["errors"]);
    } else {
        println!("{}", r);
    }
    if r.get("error").is_some() { 1 } else { 0 }
}

/// M-p: check を画面に出す
fn key(root: &Path) -> Value {
    let Ok(mut tty) = Tty::open() else { return json!({}) };
    tty.write("\x1b[2m(見ています…)\x1b[0m");
    let r = check(root, &[], false, false);
    let t = match r["text"].as_str() {
        Some("") => "pkg: みんな最新".to_string(),
        Some(t) => t.to_string(),
        None => format!("pkg: {}", r["error"].as_str().unwrap_or("")),
    };
    let lines: Vec<&str> = t.lines().collect();
    tty.write("\r\x1b[K");
    for l in &lines {
        tty.write(&format!("\x1b[2m{}\x1b[0m\r\n", aish_plugin::clip(l, tty.cols().saturating_sub(1))));
    }
    tty.write("\x1b[2m(何かキーで消す)\x1b[0m");
    let _ = tty.key();
    tty.write(&format!("\r\x1b[{}A\x1b[J", lines.len()));
    json!({})
}

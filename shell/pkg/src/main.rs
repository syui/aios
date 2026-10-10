// aish-pkg: aios のパッケージを最新にする (人は M-p、Claude は MCP のツール)
//   pkg_check  配布元の最新の版といまの pkgver をくらべる (新しいものだけ。all で全部)。
//              一覧は pkg/pkg.json に作る: 1 つのパッケージに 1 行で name type src (取ってくる URL) now latest
//   pkg_edit   PKGBUILD (pkgver、pkgrel、sha256sums) と .aios.json を、配布元の最新か ver に書きかえる
//   pkg_build  bin/mkpkg.sh で作って repo/aarch64/KIND/ に置く (古い版を外し、aios.db を作りなおす)
//   pkg_test   作ったものを確かめる (ELF が aarch64 か、版、bin/ を qemu か aarch64 で --version)
//   pkg_push   ai/repo とくらべて、変わるものを bin/gitea.sh repo で送る (署名つきの 1 コミット)
//   M-p        pkg_check を画面に出す (キーを押すと消える)
// build と push は長いので、うしろで動かす (自分を aish-pkg build / push で起こす)。もういちど呼ぶと様子か結果
// コマンドとしても動く (大きな tarball の edit は時間がかかるので、run の bg で):
//   aish-pkg check [NAME...] [--all] [--refresh]   aish-pkg edit NAME [VER]   aish-pkg build NAME   aish-pkg test NAME   aish-pkg push [--force]
// リポジトリは dir か、いまのディレクトリから上にたどって pkg/ があるところ、なければ $AIOS_SRC か /usr/src/aios
mod repo;
mod test;
mod up;

use aish_plugin::{Spec, Tool, Tty, Value, error, json, s};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const CHECK: &str = r#"{"type":"object","properties":{"names":{"type":"array","items":{"type":"string"},"description":"見るパッケージ (なければ全部)"},"all":{"type":"boolean","description":"全部の一覧 (pkg/pkg.json: name type src now latest) を返す (既定: 新しい版があるものとしくじったものだけ)"},"refresh":{"type":"boolean","description":"どこを見るか (pkg/upstream.json) を PKGBUILD と Arch の .nvchecker.toml から作りなおす"},"dir":{"type":"string","description":"aios のリポジトリ (既定: いまのディレクトリから上へ探す)"}}}"#;
const BUILD: &str = r#"{"type":"object","properties":{"name":{"type":"string","description":"パッケージ"},"wait_ms":{"type":"integer","description":"終わるまで待つ長さ (既定 30000、50000 まで)"},"dir":{"type":"string"}},"required":["name"]}"#;
const PUSH: &str = r#"{"type":"object","properties":{"force":{"type":"boolean","description":"ai/repo のほうが新しいもの (ほかで送ったもの) を消す・下げることになっても送る"},"wait_ms":{"type":"integer","description":"終わるまで待つ長さ (既定 30000、50000 まで)"},"dir":{"type":"string"}}}"#;
const EDIT: &str = r#"{"type":"object","properties":{"name":{"type":"string","description":"パッケージ"},"ver":{"type":"string","description":"版 (なければ配布元の最新)"},"dir":{"type":"string"}},"required":["name"]}"#;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if matches!(args.first().map(String::as_str), Some("check" | "edit" | "build" | "fetch" | "test" | "push")) {
        std::process::exit(cli(&args));
    }
    let spec = Spec {
        name: "pkg",
        hooks: &["key"],
        keys: &[("M-p", "check")],
        tools: &[
            Tool { name: "pkg_check", desc: "aios のパッケージ (pkg/*/NAME/PKGBUILD) の配布元の最新の版を見て、いまの pkgver とくらべる。1 行に 1 つ: NAME いま → 最新。all で全部の一覧 (name type src now latest。pkg/pkg.json にも)", input: CHECK },
            Tool { name: "pkg_build", desc: "パッケージを作る (bin/mkpkg.sh)。できたものは repo/aarch64/KIND/ に置き、古い版を外して aios.db を作りなおす。repo/aarch64 がなければ ai/repo からそろえる。うしろで動くので、終わっていなければ running とログの終わり。もういちど呼ぶと続きを待つ", input: BUILD },
            Tool { name: "pkg_fetch", desc: "重いパッケージ (pkg/ci.json の firefox、llvm、uv) は GitHub の CI (.github/workflows/pkg.yml) が作ってリリース NAME-VER-REL に置く。そのいちばん新しいものを取ってきて (SHA256SUMS を確かめる)、pkg_build と同じく repo/aarch64/KIND/ に置き、PKGBUILD もリリースのもの (CI が版を上げたもの) にする。あとは pkg_test → pkg_push。うしろで動く", input: BUILD },
            Tool { name: "pkg_test", desc: "pkg_build で作ったものを確かめる: 版が PKGBUILD と同じか、中の ELF がみな aarch64 か、bin/ のプログラムが --version で動くか (aarch64 でなければ qemu-aarch64 で。使うパッケージと musl も広げる)。通ったものだけ pkg_push で送れる。うしろで動く", input: BUILD },
            Tool { name: "pkg_push", desc: "repo/aarch64 を ai/repo (git.syui.ai) に送る (bin/gitea.sh repo。署名つきの 1 コミット)。先に ai/repo とくらべて変わるもの (new / 上がる) を出す。pkg_test を通っていないものや、ai/repo のほうが新しいもの (消える・下がる) があれば止まる (force で送る)。うしろで動く", input: PUSH },
            Tool { name: "pkg_edit", desc: "PKGBUILD の pkgver を配布元の最新 (か ver) にし、pkgrel を 1 に、sha256sums を取ってきたもので書きかえ、.aios.json の版と updated も変える。大きな tarball は時間がかかるので run の bg で aish-pkg edit NAME", input: EDIT },
        ],
    };
    let mut jobs: HashMap<String, Job> = HashMap::new();
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
                "pkg_build" => {
                    let n = s(a, "name");
                    if n.is_empty() || n.contains('/') {
                        return error("give name");
                    }
                    job(&mut jobs, &root, &format!("build-{}", n), &["build", n], a)
                }
                "pkg_fetch" => {
                    let n = s(a, "name");
                    if n.is_empty() || n.contains('/') {
                        return error("give name");
                    }
                    job(&mut jobs, &root, &format!("fetch-{}", n), &["fetch", n], a)
                }
                "pkg_test" => {
                    let n = s(a, "name");
                    if n.is_empty() || n.contains('/') {
                        return error("give name");
                    }
                    job(&mut jobs, &root, &format!("test-{}", n), &["test", n], a)
                }
                "pkg_push" => {
                    let argv: &[&str] = if a["force"] == true { &["push", "--force"] } else { &["push"] };
                    job(&mut jobs, &root, "push", argv, a)
                }
                n => error(format!("{}: no such tool", n)),
            }
        }
        "key" => key(&root(None, s(v, "pwd"))),
        _ => json!({}),
    });
}

/// うしろで動いているもの (自分を aish-pkg build / push で起こしたもの)。出力は build/aish-pkg/KEY.log
struct Job {
    child: std::process::Child,
    log: PathBuf,
    t0: Instant,
}

/// key のものがなければ起こし、wait_ms まで待つ。終わったら最後の行の JSON (と log の終わり)
fn job(jobs: &mut HashMap<String, Job>, root: &Path, key: &str, argv: &[&str], a: &Value) -> Value {
    if !jobs.contains_key(key) {
        let dir = root.join("build/aish-pkg");
        let _ = std::fs::create_dir_all(&dir);
        let log = dir.join(format!("{}.log", key));
        let (Ok(out), Ok(exe)) = (std::fs::File::create(&log), std::env::current_exe()) else { return error(format!("{}: cannot start", log.display())) };
        let err = match out.try_clone() {
            Ok(e) => e,
            Err(e) => return error(e),
        };
        let child = std::process::Command::new(exe).args(argv).current_dir(root).env("AIOS_SRC", root).stdin(std::process::Stdio::null()).stdout(out).stderr(err).spawn();
        match child {
            Ok(c) => jobs.insert(key.to_string(), Job { child: c, log, t0: Instant::now() }),
            Err(e) => return error(e),
        };
    }
    let wait = Duration::from_millis(a["wait_ms"].as_u64().unwrap_or(30_000).min(50_000));
    let end = Instant::now() + wait;
    let j = jobs.get_mut(key).unwrap();
    let status = loop {
        match j.child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) if Instant::now() < end => std::thread::sleep(Duration::from_millis(200)),
            _ => break None,
        }
    };
    let text = std::fs::read_to_string(&j.log).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    let tail = lines[lines.len().saturating_sub(15)..].join("\n");
    let ms = j.t0.elapsed().as_millis() as u64;
    let Some(st) = status else {
        return json!({ "running": true, "ms": ms, "log": tail, "hint": "call again to wait more" });
    };
    let log = j.log.display().to_string();
    jobs.remove(key);
    // 最後の JSON の行が答え
    let mut r = lines.iter().rev().find_map(|l| aish_plugin::parse(l)).unwrap_or_else(|| json!({}));
    if !st.success() && r.get("error").is_none() {
        r["error"] = json!(format!("exit {}", st));
    }
    r["ms"] = json!(ms);
    r["log"] = json!(log);
    if r.get("error").is_some() {
        r["tail"] = json!(tail);
    }
    r
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
    // all なら一覧 (pkg/pkg.json: name type src now latest) をそのまま (names があればその行だけ)
    if all {
        let list = std::fs::read_to_string(root.join("pkg").join(up::OVERVIEW)).unwrap_or_default();
        text = list.lines().filter(|l| names.is_empty() || names.iter().any(|n| l.contains(&format!("\"name\": \"{}\",", n)))).map(|l| format!("{}\n", l)).collect();
    }
    let news: Vec<&Value> = rs.iter().filter(|r| r["new"] == true).map(|r| &r["name"]).collect();
    json!({ "text": text, "new": news, "latest": same, "errors": bad, "count": new + same + bad })
}

fn cli(args: &[String]) -> i32 {
    let root = root(None, "");
    let rest: Vec<String> = args[1..].iter().filter(|a| !a.starts_with("--")).cloned().collect();
    let r = match args[0].as_str() {
        "check" if args.iter().any(|a| a == "--json") => {
            // CI (.github/workflows/pkg.yml) が読む: 1 行の JSON {versions: [{name, now, latest, new}]}
            let r = match up::check(&root.join("pkg"), &rest, false) {
                Ok(rs) => json!({ "versions": rs.iter().map(|r| json!({ "name": r["name"], "now": r["pkgver"], "latest": r["latest"], "new": r["new"] == true, "error": r["error"] })).collect::<Vec<_>>() }),
                Err(e) => error(e),
            };
            println!("{}", r);
            return if r.get("error").is_some() { 1 } else { 0 };
        }
        "check" => check(&root, &rest, args.iter().any(|a| a == "--all"), args.iter().any(|a| a == "--refresh")),
        "build" => match rest.first() {
            Some(n) => repo::build(&root, n).unwrap_or_else(error),
            None => error("aish-pkg build NAME"),
        },
        "fetch" => match rest.first() {
            Some(n) => repo::fetch(&root, n).unwrap_or_else(error),
            None => error("aish-pkg fetch NAME"),
        },
        "test" => match rest.first() {
            Some(n) => test::test(&root, n).unwrap_or_else(error),
            None => error("aish-pkg test NAME"),
        },
        "push" => repo::push(&root, args.iter().any(|a| a == "--force")).unwrap_or_else(error),
        _ => match rest.first() {
            Some(n) => up::edit(&root, n, rest.get(1).map(String::as_str)).unwrap_or_else(error),
            None => error("aish-pkg edit NAME [VER]"),
        },
    };
    if matches!(args[0].as_str(), "build" | "fetch" | "test" | "push") {
        // うしろで動かしたとき (pkg_build / pkg_push)、最後の行を答えにする
        println!("{}", r);
    } else if let Some(t) = r["text"].as_str() {
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

// aish-pkg: aios のパッケージを最新にする (人は M-p、Claude は MCP のツール)
//   pkg_check  配布元の最新の版といまの pkgver をくらべる (新しいものだけ。all で全部)。
//              一覧は pkg/pkg.json に作る: 1 つのパッケージに 1 行で name type src (取ってくる URL) now latest
//              中に古いライブラリ (静的リンク) が入ったままのものと、ちがう Zig で作ったものも出す: rebuild (embed.rs)
//              直っていない CVE (Alpine の secdb と、firefox は Mozilla の advisories とくらべる) も出す: vulns (vuln.rs)。
//              Rust の crate (aios と pkg/rust の Cargo.lock を RustSec と) と npm (.aios.json の版を npm の advisories と) も
//   pkg_edit   PKGBUILD (pkgver、pkgrel、sha256sums) と .aios.json を、配布元の最新か ver に書きかえる。
//              rebuild なら pkgrel を 1 つ上げるだけ (pkg_check の rebuild を作りなおすとき)
//   pkg_build  bin/mkpkg.sh で作って repo/aarch64/KIND/ に置く (古い版を外し、aios.db を作りなおす)
//   pkg_test   作ったものを確かめる (ELF が aarch64 か、COPY のデータの重なり、NEEDED、版、bin/ を qemu か aarch64 で --version)
//   pkg_scan   repo/aarch64 のパッケージみなを、動かさずに pkg_test の copies と needs で
//   pkg_rebuild いくつものパッケージを順に edit rebuild → build → test (と push)。えらび方と順は batch.rs
//   pkg_push   ai/repo とくらべて、変わるものを bin/gitea.sh repo で送る (署名つきの 1 コミット)
//   M-p        pkg_check を画面に出す (キーを押すと消える)
// edit と build と push は長いので、うしろで動かす (自分を aish-pkg edit / build / push で起こす)。もういちど呼ぶと様子か結果
// コマンドとしても動く (大きな tarball の edit は時間がかかるので、run の bg で):
//   aish-pkg check [NAME...] [--all] [--refresh]   aish-pkg edit NAME [VER] [--rebuild]   aish-pkg build NAME   aish-pkg test NAME   aish-pkg push [--force]
//   aish-pkg scan [NAME...]   repo/aarch64 のパッケージみなを、動かさずに pkg_test の copies で (古い Zig で作ったものを探す)
//   aish-pkg rebuild [NAME...] [--unrecorded] [--force] [--fail-fast] [--push] [--vm] [--dry]   aish-pkg test NAME [--vm]
// リポジトリは dir か、いまのディレクトリから上にたどって pkg/ があるところ、なければ $AIOS_SRC か /usr/src/aios
mod batch;
mod elf;
mod embed;
mod repo;
mod test;
mod up;
mod vmtest;
mod vuln;

use aish_plugin::{Spec, Tool, Tty, Value, error, json, s};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const CHECK: &str = r#"{"type":"object","properties":{"names":{"type":"array","items":{"type":"string"},"description":"見るパッケージ (なければ全部)"},"all":{"type":"boolean","description":"全部の一覧 (pkg/pkg.json: name type src now latest) を返す (既定: 新しい版があるものとしくじったものだけ)"},"refresh":{"type":"boolean","description":"どこを見るか (pkg/upstream.json) を PKGBUILD と Arch の .nvchecker.toml から作りなおす"},"wait_ms":{"type":"integer","description":"終わるまで待つ長さ (既定 30000、50000 まで。みなを見ると 1 分ほど)"},"stop":{"type":"boolean","description":"動いているものを止める"},"dir":{"type":"string","description":"aios のリポジトリ (既定: いまのディレクトリから上へ探す)"}}}"#;
const BUILD: &str = r#"{"type":"object","properties":{"name":{"type":"string","description":"パッケージ"},"wait_ms":{"type":"integer","description":"終わるまで待つ長さ (既定 30000、50000 まで)"},"stop":{"type":"boolean","description":"動いているものを止める (孫の make や zig まで)"},"dir":{"type":"string"}},"required":["name"]}"#;
const TEST: &str = r#"{"type":"object","properties":{"name":{"type":"string","description":"パッケージ"},"vm":{"type":"boolean","description":"aios の VM (-snapshot で repo/ を出したもの。動いていなければ起こし、終わっても止めない) にも ap -S で入れて、bin/ のものを aios のカーネルの上で動かす (このマシンの qemu-user で動いたものが aios で動かなければしくじり)"},"wait_ms":{"type":"integer","description":"終わるまで待つ長さ (既定 30000、50000 まで)"},"stop":{"type":"boolean","description":"動いているものを止める"},"dir":{"type":"string"}},"required":["name"]}"#;
const PUSH: &str = r#"{"type":"object","properties":{"force":{"type":"boolean","description":"ai/repo のほうが新しいもの (ほかで送ったもの) を消す・下げることになっても送る"},"wait_ms":{"type":"integer","description":"終わるまで待つ長さ (既定 30000、50000 まで)"},"stop":{"type":"boolean","description":"動いているものを止める"},"dir":{"type":"string"}}}"#;
const SCAN: &str = r#"{"type":"object","properties":{"names":{"type":"array","items":{"type":"string"},"description":"見るパッケージ (なければ repo/aarch64 のみな)"},"wait_ms":{"type":"integer","description":"終わるまで待つ長さ (既定 30000、50000 まで)"},"stop":{"type":"boolean","description":"動いているものを止める"},"dir":{"type":"string"}}}"#;
const REBUILD: &str = r#"{"type":"object","properties":{"names":{"type":"array","items":{"type":"string"},"description":"作りなおすパッケージ (なければ pkg_check の rebuild のもの)"},"unrecorded":{"type":"boolean","description":"Zig を書きとめる前に作ったもの (pkg_check の unrecorded) も"},"force":{"type":"boolean","description":"もう作りなおしたもの (いまの PKGBUILD の版で、古いライブラリもちがう Zig もない) も作る"},"fail_fast":{"type":"boolean","description":"しくじったら止める (既定: 飛ばして続け、さいごにまとめて出す)"},"push":{"type":"boolean","description":"みな通ったら pkg_push で ai/repo に送る"},"vm":{"type":"boolean","description":"pkg_test を aios の VM の中でも (pkg_test の vm)"},"dry":{"type":"boolean","description":"作らずに、何をどの順に作るかだけ (すぐ答える)"},"wait_ms":{"type":"integer","description":"終わるまで待つ長さ (既定 30000、50000 まで)"},"stop":{"type":"boolean","description":"動いているものを止める (いま作っているものの make や zig まで)"},"dir":{"type":"string"}}}"#;
const EDIT: &str = r#"{"type":"object","properties":{"name":{"type":"string","description":"パッケージ"},"ver":{"type":"string","description":"版 (なければ配布元の最新)"},"rebuild":{"type":"boolean","description":"版はそのまま pkgrel を 1 つ上げるだけ (pkg_check の rebuild: 中に入れたライブラリが新しくなったとき。同じ版では pkg_push も ap -Syu も変わったと見ない)"},"wait_ms":{"type":"integer","description":"終わるまで待つ長さ (既定 30000、50000 まで)"},"stop":{"type":"boolean","description":"動いているものを止める"},"dir":{"type":"string"}},"required":["name"]}"#;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if matches!(args.first().map(String::as_str), Some("check" | "edit" | "build" | "fetch" | "test" | "push" | "scan" | "rebuild")) {
        std::process::exit(cli(&args));
    }
    let spec = Spec {
        name: "pkg",
        hooks: &["key"],
        keys: &[("M-p", "check")],
        tools: &[
            Tool { name: "pkg_check", desc: "aios のパッケージ (pkg/*/NAME/PKGBUILD) の配布元の最新の版を見て、いまの pkgver とくらべる。1 行に 1 つ: NAME いま → 最新 (ライブラリなら、それを中に入れているパッケージも)。all で全部の一覧 (name type src now latest。pkg/pkg.json にも)。中に古いライブラリが入ったまま (静的リンク。c_deps のもの) か、いまとちがう Zig (zig.sh の ZIG_VERSION-ZIG_REL。makedepend = zig=...) で作ったパッケージは rebuild に: pkg_edit (rebuild) → pkg_build → pkg_test → pkg_push。Zig を書きとめる前に作ったものは unrecorded に (作った日の Zig の版ごと)。直っていない CVE は vulns に (Alpine の secdb の「この版で直した」とくらべる。pkg/c のものなど。firefox は Mozilla の advisories と): fixed in VER なら pkg_edit でそれから上に、patched by Alpine (同じ版にパッチ) なら aports のパッチを足して PKGBUILD に CVE の番号を書く (あてはまらないものも番号を書けば出なくなる)。Rust の crate も (aios の Cargo.lock と pkg/rust の配布元の Cargo.lock を cargo audit と同じ RustSec の advisories と。PKGBUILD の cargo_update で上げたものは上げた版で): crate の fixed in VER なら pkg/rust は build() に cargo_update CRATE@VER (cross.sh) を足して pkg_edit rebuild、aios の Cargo.lock は cargo update -p CRATE。npm も (.aios.json の pkg.linux.npm の版を npm audit と同じ advisories と)。うしろで動くので、終わっていなければ running (配布元をみな見ると 1 分ほど。もういちど呼ぶと続きを待つ)", input: CHECK },
            Tool { name: "pkg_build", desc: "パッケージを作る (bin/mkpkg.sh)。できたものは repo/aarch64/KIND/ に置き、古い版を外して aios.db を作りなおす。repo/aarch64 がなければ ai/repo からそろえる。うしろで動くので、終わっていなければ running とログの終わり。もういちど呼ぶと続きを待つ (stop で止める。aish が入れかわっても引きつぐ)。このライブラリを中に入れているパッケージは rebuild に。前の版にあったライブラリ (SONAME) がなくなれば、それを使う (NEEDED) パッケージは soname.rebuild に", input: BUILD },
            Tool { name: "pkg_fetch", desc: "重いパッケージ (pkg/ci.json の firefox、llvm、uv) は GitHub の CI (.github/workflows/pkg.yml) が作ってリリース NAME-VER-REL に置く。そのいちばん新しいものを取ってきて (SHA256SUMS を確かめる)、pkg_build と同じく repo/aarch64/KIND/ に置き、PKGBUILD もリリースのもの (CI が版を上げたもの) にする。あとは pkg_test → pkg_push。うしろで動く", input: BUILD },
            Tool { name: "pkg_test", desc: "pkg_build で作ったものを確かめる: 版が PKGBUILD と同じか、中の ELF がみな aarch64 か、COPY で写したライブラリのデータが 1 つの場所に重なっていないか (copies。古い Zig の libc.so でリンクすると stdin stdout stderr が 1 つ)、NEEDED が depends の中にあるか (needs)、bin/ のプログラムが --version で動くか (aarch64 でなければ qemu-aarch64 で。使うパッケージと musl も広げる)。vm なら aios の VM にも ap -S で入れて、aios のカーネルの上で動かす。通ったものだけ pkg_push で送れる。うしろで動く", input: TEST },
            Tool { name: "pkg_scan", desc: "repo/aarch64 のパッケージみな (names があればそれだけ) を、動かさずに pkg_test の copies と needs で確かめる: COPY で写したライブラリのデータが 1 つの場所に重なったプログラム (古い Zig の libc.so でリンクしたもの。affected を rebuild に) と、depends にない NEEDED (unresolved。それのあるパッケージも)。Zig を上げたときなどに。うしろで動く (みなで 1 分ほど)", input: SCAN },
            Tool { name: "pkg_rebuild", desc: "いくつものパッケージを順に作りなおす: pkgrel を上げ (pkg_edit rebuild)、pkg_build、pkg_test。names がなければ pkg_check の rebuild のもの (unrecorded なら Zig を書きとめる前に作ったものも)。c_deps で使うものを先に。pkgrel はいまの版がもう ai/repo にあるときだけ上げ、もう作りなおしたもの (pkg_test を通っているか送ってあり、古いライブラリもちがう Zig もない) は飛ばす (もういちど動かしても 2 つ上がらない。pkg_test でしくじったものは同じ版で作りなおす)。CI が作るもの (pkg/ci.json) は飛ばす。しくじったものは飛ばして続け (fail_fast で止める)、push ならみな通ったら pkg_push。dry で順だけ。うしろで動く (進みぐあいは log、くわしくは build-NAME.log / test-NAME.log)。あとで PKGBUILD をコミットして unix に", input: REBUILD },
            Tool { name: "pkg_push", desc: "repo/aarch64 を ai/repo (git.syui.ai) に送る (bin/gitea.sh repo。署名つきの 1 コミット)。先に ai/repo とくらべて変わるもの (new / 上がる) を出す。pkg_test を通っていないものや、ai/repo のほうが新しいもの (消える・下がる) があれば止まる (force で送る)。うしろで動く", input: PUSH },
            Tool { name: "pkg_edit", desc: "PKGBUILD の pkgver を配布元の最新 (か ver) にし、pkgrel を 1 に、sha256sums を取ってきたもので書きかえ、.aios.json の版と updated も変える。rebuild なら pkgrel を 1 つ上げるだけ (pkg_check の rebuild)。うしろで動くので、終わっていなければ running (大きな tarball や遅い配布元でも。もういちど呼ぶと続きを待つ)", input: EDIT },
        ],
    };
    let mut jobs: HashMap<String, Job> = HashMap::new();
    aish_plugin::run(spec, |ev, v| match ev {
        "tool" => {
            let a = &v["args"];
            let root = root(a["dir"].as_str(), s(v, "pwd"));
            match s(v, "name") {
                "pkg_check" => {
                    // 配布元をみな見ると 1 分ほどかかり、ツールが 60 秒で切れるので、build と同じくうしろで
                    let names: Vec<&str> = a["names"].as_array().map(|x| x.iter().filter_map(|n| n.as_str()).filter(|n| !n.is_empty() && !n.starts_with('-')).collect()).unwrap_or_default();
                    let mut argv = vec!["check", "--answer"];
                    for (k, f) in [("all", "--all"), ("refresh", "--refresh")] {
                        if a[k] == true {
                            argv.push(f);
                        }
                    }
                    argv.extend(names);
                    job(&mut jobs, &root, "check", &argv, a)
                }
                "pkg_edit" => {
                    let n = s(a, "name");
                    if n.is_empty() || n.contains('/') {
                        return error("give name");
                    }
                    if a["rebuild"] == true {
                        return up::bump_rel(&root, n).unwrap_or_else(error);
                    }
                    // tarball を取ってきて sha256 を計るので、build と同じくうしろで (大きなものや遅い配布元でも
                    // ツールが 60 秒で切れないように)
                    let mut argv = vec!["edit", n];
                    if let Some(v) = a["ver"].as_str() {
                        argv.push(v);
                    }
                    job(&mut jobs, &root, &format!("edit-{}", n), &argv, a)
                }
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
                    let argv: &[&str] = if a["vm"] == true { &["test", n, "--vm"] } else { &["test", n] };
                    job(&mut jobs, &root, &format!("test-{}", n), argv, a)
                }
                "pkg_scan" => {
                    let names: Vec<&str> = a["names"].as_array().map(|x| x.iter().filter_map(|n| n.as_str()).filter(|n| !n.is_empty() && !n.contains('/') && !n.starts_with('-')).collect()).unwrap_or_default();
                    let argv: Vec<&str> = std::iter::once("scan").chain(names).collect();
                    job(&mut jobs, &root, "scan", &argv, a)
                }
                "pkg_rebuild" => {
                    let names: Vec<&str> = a["names"].as_array().map(|x| x.iter().filter_map(|n| n.as_str()).filter(|n| !n.is_empty() && !n.contains('/') && !n.starts_with('-')).collect()).unwrap_or_default();
                    let flag = |k: &str| a[k] == true;
                    if flag("dry") {
                        let o = batch::Opts { names: names.iter().map(|s| s.to_string()).collect(), unrecorded: flag("unrecorded"), force: flag("force"), ..Default::default() };
                        return batch::dry(&root, &o);
                    }
                    let mut argv: Vec<&str> = std::iter::once("rebuild").chain(names).collect();
                    for (k, f) in [("unrecorded", "--unrecorded"), ("force", "--force"), ("fail_fast", "--fail-fast"), ("push", "--push"), ("vm", "--vm")] {
                        if flag(k) {
                            argv.push(f);
                        }
                    }
                    job(&mut jobs, &root, "rebuild", &argv, a)
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

/// うしろで動いているもの (自分を aish-pkg build / push で起こしたもの)。出力は build/aish-pkg/KEY.log、pid は KEY.pid。
/// aish やこのプラグインが入れかわっても (aish-mcp.sh --build のたびに入れかわる)、次に呼ばれたときに KEY.pid から
/// 引きついで続きを待つ (前は忘れてしまい、もういちど呼ぶと同じビルドをもう 1 つ起こした)
struct Job {
    /// 自分で起こした子。入れかわる前に起こしたもの (pid だけわかる) は None
    child: Option<std::process::Child>,
    pid: i32,
    log: PathBuf,
    t0: Instant,
}

impl Job {
    /// 終わったら Some(終わりの番号。引きついだものは None (わからない))
    fn done(&mut self) -> Option<Option<std::process::ExitStatus>> {
        match &mut self.child {
            Some(c) => match c.try_wait() {
                Ok(None) => None,
                Ok(st) => Some(st),
                Err(_) => Some(None),
            },
            None => (!alive(self.pid)).then_some(None),
        }
    }
}

/// pid が動いているか (終わって親に拾われていないもの (ゾンビ) は、終わったとみる)
fn alive(pid: i32) -> bool {
    std::fs::read_to_string(format!("/proc/{}/stat", pid)).is_ok_and(|s| s.rsplit_once(')').is_some_and(|(_, rest)| !rest.trim_start().starts_with('Z')))
}

/// 前のプラグインが起こした key のもの。動いていれば、同じ引数の aish-pkg のときだけ (pid が別のものに使われていないか)。
/// 終わっていても KEY.pid が残っていれば、答えをまだ渡していないので引きつぐ (答えは log の終わりの JSON)
fn adopt(dir: &Path, key: &str, argv: &[&str]) -> Option<Job> {
    let pidf = dir.join(format!("{}.pid", key));
    let pid: i32 = std::fs::read_to_string(&pidf).ok()?.trim().parse().ok().filter(|p| *p > 1)?;
    if alive(pid) {
        let cmd = std::fs::read(format!("/proc/{}/cmdline", pid)).unwrap_or_default();
        let mut words = cmd.split(|&b| b == 0).map(|w| String::from_utf8_lossy(w).into_owned());
        let same = words.next().is_some_and(|exe| exe.ends_with("aish-pkg")) && words.take(argv.len()).eq(argv.iter().map(|s| s.to_string()));
        if !same {
            let _ = std::fs::remove_file(&pidf);
            return None;
        }
    }
    let started = std::fs::metadata(&pidf).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok());
    let t0 = started.and_then(|e| Instant::now().checked_sub(e)).unwrap_or_else(Instant::now);
    Some(Job { child: None, pid, log: dir.join(format!("{}.log", key)), t0 })
}

/// key のものがなければ起こし、wait_ms まで待つ。終わったら最後の行の JSON (と log の終わり)。
/// stop なら、動いているものをグループごと (mkpkg.sh や make や zig まで) 止める
fn job(jobs: &mut HashMap<String, Job>, root: &Path, key: &str, argv: &[&str], a: &Value) -> Value {
    let dir = root.join("build/aish-pkg");
    let pidf = dir.join(format!("{}.pid", key));
    if !jobs.contains_key(key)
        && let Some(j) = adopt(&dir, key, argv)
    {
        jobs.insert(key.to_string(), j);
    }
    if a["stop"] == true {
        let Some(mut j) = jobs.remove(key) else { return error(format!("{}: not running", key)) };
        let _ = std::fs::remove_file(&pidf);
        // グループ (spawn の process_group(0) で、番号は子の pid) に送る。kill のコマンドは使わない:
        // procps の kill は -PID を -- なしではシグナルと読み、何も止めずに 0 で終わる (wait がいつまでも返らなかった)。
        // 0 や 1 (自分のグループやみんな) にはぜったい送らない。終わったもの (pid がもう別のものかもしれない) にも送らない
        let pid = j.pid;
        if pid > 1 && j.done().is_none() {
            unsafe { libc::kill(-pid, libc::SIGTERM) };
            let t = Instant::now();
            while j.done().is_none() && t.elapsed() < Duration::from_secs(5) {
                std::thread::sleep(Duration::from_millis(100));
            }
            if j.done().is_none() {
                unsafe { libc::kill(-pid, libc::SIGKILL) };
            }
        }
        // グループがなくても (前の aish-pkg が起こしたものなど)、子は止めて wait が返るように
        if let Some(c) = &mut j.child {
            if matches!(c.try_wait(), Ok(None)) {
                let _ = c.kill();
            }
            let _ = c.wait();
        }
        return json!({ "stopped": key, "ms": j.t0.elapsed().as_millis() as u64, "log": j.log.display().to_string() });
    }
    if !jobs.contains_key(key) {
        let _ = std::fs::create_dir_all(&dir);
        let log = dir.join(format!("{}.log", key));
        let (Ok(out), Ok(exe)) = (std::fs::File::create(&log), exe()) else { return error(format!("{}: cannot start", log.display())) };
        let err = match out.try_clone() {
            Ok(e) => e,
            Err(e) => return error(e),
        };
        // 自分のプロセスのグループで (stop で孫まで止められるように)
        use std::os::unix::process::CommandExt;
        let child = std::process::Command::new(exe).args(argv).current_dir(root).env("AIOS_SRC", root).stdin(std::process::Stdio::null()).stdout(out).stderr(err).process_group(0).spawn();
        match child {
            Ok(c) => {
                let pid = c.id() as i32;
                let _ = std::fs::write(&pidf, format!("{}\n", pid));
                jobs.insert(key.to_string(), Job { child: Some(c), pid, log, t0: Instant::now() })
            }
            Err(e) => return error(e),
        };
    }
    let wait = Duration::from_millis(a["wait_ms"].as_u64().unwrap_or(30_000).min(50_000));
    let end = Instant::now() + wait;
    let j = jobs.get_mut(key).unwrap();
    let status = loop {
        match j.done() {
            Some(st) => break Some(st),
            None if Instant::now() < end => std::thread::sleep(Duration::from_millis(200)),
            None => break None,
        }
    };
    let text = std::fs::read_to_string(&j.log).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    let tail = lines[lines.len().saturating_sub(15)..].iter().map(|l| short_line(l)).collect::<Vec<_>>().join("\n");
    let ms = j.t0.elapsed().as_millis() as u64;
    let adopted = j.child.is_none();
    let Some(st) = status else {
        let mut r = json!({ "running": true, "ms": ms, "log": tail, "hint": "call again to wait more" });
        if adopted {
            r["adopted"] = json!(format!("started before aish-pkg was restarted (pid {})", j.pid));
        }
        return r;
    };
    let log = j.log.display().to_string();
    jobs.remove(key);
    let _ = std::fs::remove_file(&pidf);
    // 最後の JSON の行が答え
    let mut r = lines.iter().rev().find_map(|l| aish_plugin::parse(l)).unwrap_or_else(|| json!({}));
    match st {
        Some(st) if !st.success() && r.get("error").is_none() => r["error"] = json!(format!("exit {}", st)),
        // 引きついだもの: 終わりの番号はわからないので、答えの行がなければしくじったとみる
        None if r.as_object().is_none_or(|o| o.is_empty()) => r["error"] = json!("ended without an answer (stopped, or the machine restarted)"),
        _ => {}
    }
    r["ms"] = json!(ms);
    r["log"] = json!(log);
    if r.get("error").is_some() {
        r["tail"] = json!(tail);
    }
    r
}

/// ログの 1 行を見せる形に: \r で描きなおした進みぐあい (curl など) は最後のものだけ、長いもの (gcc のコマンドの行は
/// 1 行で千字ほどあり、15 行で 15 KB になっていた) は頭の 200 字と … だけ
fn short_line(l: &str) -> String {
    let l = l.rsplit('\r').find(|s| !s.trim().is_empty()).unwrap_or("");
    match l.char_indices().nth(200) {
        Some((i, _)) => format!("{}…", &l[..i]),
        None => l.to_string(),
    }
}

/// このプログラム (子の aish-pkg build などを起こす)。ビルドしなおして置きかわると /proc/self/exe は "... (deleted)" になるので、
/// そのときは同じところの新しいものを (pkg_rebuild のように長く動くあいだに aish をビルドしなおしても)
fn exe() -> std::io::Result<PathBuf> {
    let p = std::env::current_exe()?;
    Ok(match p.to_str().and_then(|s| s.strip_suffix(" (deleted)")) {
        Some(s) => PathBuf::from(s),
        None => p,
    })
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
    // 中に古いライブラリが入ったままのもの (ネットは見ない)
    let stale = embed::stale(root);
    // 直っていない CVE (secdb は 1 日に 1 回取ってくる。取れなければ前のもので、そう書いた 1 行も)
    let (vulns, vnote) = vuln::check(root, names).unwrap_or_else(|e| (Vec::new(), e));
    let w = rs.iter().filter_map(|r| r["name"].as_str()).chain(stale.keys().map(String::as_str)).chain(vulns.iter().map(|v| v.name.as_str())).map(str::len).max().unwrap_or(0);
    let (mut new, mut same, mut bad) = (0, 0, 0);
    let mut text = String::new();
    for r in &rs {
        let (n, cur) = (r["name"].as_str().unwrap_or(""), r["pkgver"].as_str().unwrap_or(""));
        if let Some(e) = r["error"].as_str() {
            bad += 1;
            text.push_str(&format!("{:w$}  {}  ? {}\n", n, cur, e));
        } else if r["new"] == true {
            new += 1;
            // ライブラリなら、上げたあとに作りなおすもの
            let users = embed::embedders(root, n);
            let users = if users.is_empty() { String::new() } else { format!("  (embedded in: {})", users.join(" ")) };
            text.push_str(&format!("{:w$}  {} → {}{}\n", n, cur, r["latest"].as_str().unwrap_or(""), users));
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
    } else {
        for (n, libs) in &stale {
            text.push_str(&format!("{:w$}  rebuild: {}\n", n, embed::describe(libs)));
        }
        for v in &vulns {
            text.push_str(&format!("{:w$}  vulns {}\n", v.name, vuln::describe(v)));
        }
        for h in vuln::hints(&vulns) {
            text.push_str(&format!("{}\n", h));
        }
        if !vnote.is_empty() {
            text.push_str(&format!("vulns: {}\n", vnote));
        }
    }
    // 作った Zig を書きとめる前に作ったもの (作った日の zig.sh の版ごとに数だけ。名前は JSON の unrecorded)
    let unrec = embed::unrecorded(root);
    if !all && names.is_empty() && !unrec.is_empty() {
        let n: usize = unrec.values().map(Vec::len).sum();
        let by: Vec<String> = unrec.iter().map(|(v, ns)| format!("{} with zig {}", ns.len(), if v.is_empty() { "?" } else { v })).collect();
        text.push_str(&format!("{} packages were built before the zig record ({}); pkg_edit rebuild to build them with zig {}\n", n, by.join(", "), embed::zig_now(root).unwrap_or_default()));
    }
    let news: Vec<&Value> = rs.iter().filter(|r| r["new"] == true).map(|r| &r["name"]).collect();
    let mut r = json!({ "text": text, "new": news, "latest": same, "errors": bad, "count": new + same + bad });
    if !stale.is_empty() {
        r["rebuild"] = embed::to_json(&stale);
    }
    if !unrec.is_empty() {
        r["unrecorded"] = json!(unrec);
    }
    if !vulns.is_empty() {
        r["vulns"] = vuln::to_json(&vulns);
    }
    if !vnote.is_empty() {
        r["vulns_note"] = json!(vnote);
    }
    r
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
        "check" if args.iter().any(|a| a == "--answer") => {
            // うしろで動かしたとき (pkg_check): 答えをそのまま 1 行の JSON で
            let r = check(&root, &rest, args.iter().any(|a| a == "--all"), args.iter().any(|a| a == "--refresh"));
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
            Some(n) => test::test(&root, n, args.iter().any(|a| a == "--vm")).unwrap_or_else(error),
            None => error("aish-pkg test NAME [--vm]"),
        },
        "push" => repo::push(&root, args.iter().any(|a| a == "--force")).unwrap_or_else(error),
        "rebuild" => {
            let has = |f: &str| args.iter().any(|a| a == f);
            let o = batch::Opts { names: rest.clone(), unrecorded: has("--unrecorded"), force: has("--force"), fail_fast: has("--fail-fast"), push: has("--push"), vm: has("--vm") };
            if has("--dry") {
                let r = batch::dry(&root, &o);
                print!("{}", r["text"].as_str().unwrap_or(""));
                return if r.get("error").is_some() { 1 } else { 0 };
            }
            // 進みぐあいは動きながら出しているので、さいごに答えの JSON を (しくじったものは failed に。終わりの番号は 0)
            let r = batch::rebuild(&root, &o);
            eprint!("{}", r["text"].as_str().unwrap_or(""));
            println!("{}", r);
            return if r.get("error").is_some() { 1 } else { 0 };
        }
        "scan" => {
            // 人には文を (標準エラーに)、pkg_scan には最後の行の JSON を。見つけたものは JSON の affected と unresolved に
            // (終わりの番号を 0 でなくすると、pkg_scan がしくじったと読む)
            let r = test::scan(&root, &rest);
            eprint!("{}", r["text"].as_str().unwrap_or(""));
            println!("{}", r);
            return 0;
        }
        _ => match rest.first() {
            Some(n) if args.iter().any(|a| a == "--rebuild") => up::bump_rel(&root, n).unwrap_or_else(error),
            Some(n) => up::edit(&root, n, rest.get(1).map(String::as_str)).unwrap_or_else(error),
            None => error("aish-pkg edit NAME [VER] [--rebuild]"),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_lines() {
        assert_eq!(short_line("[6/215] Compiling C object"), "[6/215] Compiling C object");
        // curl の進みぐあいは最後のものだけ
        assert_eq!(short_line("  0  100k\r 50  100k\r100  100k done\r"), "100  100k done");
        let gcc = format!("aarch64-linux-gnu-gcc {}", "-Wall ".repeat(300));
        assert_eq!(short_line(&gcc).chars().count(), 201);
        assert!(short_line(&"字".repeat(300)).ends_with("字…"));
    }

    #[test]
    fn adopt_only_our_jobs() {
        let dir = std::env::temp_dir().join(format!("aish-pkg-adopt-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        // 動いているが aish-pkg build x ではないもの (このテスト): 引きつがず、KEY.pid を消す
        std::fs::write(dir.join("build-x.pid"), format!("{}\n", std::process::id())).unwrap();
        assert!(adopt(&dir, "build-x", &["build", "x"]).is_none());
        assert!(!dir.join("build-x.pid").exists());
        // 終わったもの: 答え (log の終わりの JSON) を渡すために引きつぐ
        let mut c = std::process::Command::new("true").spawn().unwrap();
        let pid = c.id() as i32;
        c.wait().unwrap();
        std::fs::write(dir.join("build-y.pid"), format!("{}\n", pid)).unwrap();
        let mut j = adopt(&dir, "build-y", &["build", "y"]).unwrap();
        assert!(j.child.is_none() && j.pid == pid);
        assert_eq!(j.done(), Some(None));
        // pid のないもの
        assert!(adopt(&dir, "build-z", &["build", "z"]).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn alive_or_not() {
        assert!(alive(std::process::id() as i32));
        let mut c = std::process::Command::new("true").spawn().unwrap();
        let pid = c.id() as i32;
        c.wait().unwrap();
        assert!(!alive(pid));
    }
}

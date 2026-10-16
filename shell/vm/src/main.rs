// aish-vm: 開発のマシン (Linux) で aios の VM を相手にする道具 (Claude は MCP のツール)
//   vm      VM を動かす: start (カーネルを選んで起こす) stop status run (シリアルの端末でコマンド。bg でうしろで) job put get shot
//           (画面を png に) keys (画面のキーボード: @alt-b など) log (シリアルの終わり、grep) mon (QEMU のモニタ)。
//           test/vm.py を呼ぶ
//   try     C (か C++) のコードを aarch64 の静的なプログラムにして (zig cc)、VM の中で動かして結果を返す
//   ab      2 つのカーネルで同じコマンドを n 回ずつ動かし、時間と大きなロックの使われ方をくらべる
//   bisect  test が通る (good) コミットと通らない (bad) コミットのあいだを git bisect で探す。
//           コミットごとにカーネルを作って VM で test を動かす (変わるのはカーネルだけ。ディスクはいまのもの)
//   box     カーネルの砂場: kernel.json の extra や quirk を切って (起動の引数 aios.off=。kernel/src/parts.rs) 起こし、
//           テスト (既定は CI と同じ vm-check と vm-ctests) を動かして、何も切らないときとくらべる。each で 1 つずつ全部
//   bench   ファイルまわりの速さ (test/fsbench.c) を VM で n 回測り、op ごとのまんなかと、/proc/ai/prof の part
//           (kernel.json の名前) ごとの時間の割合、lockbench (ロックなどの 1 回の時間) のまんなか。against で 2 つの
//           カーネルをくらべる。結果は build/aish-vm/bench/ にも
//   sysdiff Linux と答えあわせ (test/sysdiff.c): 種から決まるシステムコールの並びを、このマシンの Linux と VM の
//           aios で動かしてくらべ、ちがいをまとめて小さくする (sysdiff.rs)
// カーネルの選び方 (kernel、a、b): ファイルのパス / "." (作業中のソースから作る) / git のコミット (別の作業場所
//   build/aish-vm/wt で作る)。どれも AIOS_INITRD=none (ルートはディスク) で build/aish-vm/target (wt は target-wt) に作り、
//   build/aish-vm/kernels/ にとっておく (同じコミットは作りなおさない)
// start、ab、bisect は長いので、うしろで動かす (自分を aish-vm start / ab / bisect で起こす)。もういちど呼ぶと様子か結果。
// ab と bisect は、動いている VM を止めてから、-snapshot (ディスクに書き残さない) で起こしなおす。2 つの VM が同じ
// disk.img を使うと壊れるので、同時には動かさない
// リポジトリは dir か、いまのディレクトリから上にたどって test/vm.py があるところ、なければ $AIOS_SRC
mod sysdiff;

use aish_plugin::{Spec, Tool, Value, error, json, s};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const VM: &str = r#"{"type":"object","properties":{"op":{"type":"string","description":"start / stop / status / run / job / put / get / shot / keys / log / mon / reboot"},"bg":{"type":"boolean","description":"run: 中でうしろで動かしてすぐ答える ({job: ID})。50 秒より長いもの (ap -S firefox など) に。そのあいだも run が使える"},"id":{"type":"integer","description":"job: どれ (なければ一覧)。様子と、前に見せたところから先の出力"},"kill":{"type":"boolean","description":"job: 止める (孫まで)"},"reset_after":{"type":"number","description":"reboot: この秒でプロンプトが来なければ QEMU をリセット (止まった新しいカーネルの電源を入れなおす)"},"kernel":{"type":"string","description":"start: カーネル (ファイルのパス / . で作業中のソースから作る / git のコミット)。なければ disk.img のもの"},"www":{"type":"string","description":"start: このディレクトリを http://10.0.2.2:8000/ に出す (中から fetch で取れる)"},"snapshot":{"type":"boolean","description":"start: ディスクに書き残さない (QEMU の -snapshot)"},"off":{"type":"array","items":{"type":"string"},"description":"start: 切って起こす extra や quirk (kernel.json の off のあるもの。起動の引数 aios.off=。カーネルを直に起こすので kernel といっしょに)"},"cmd":{"type":"string","description":"run: シリアルの端末で動かすコマンド。mon: QEMU のモニタのコマンド"},"timeout":{"type":"number","description":"run: 秒 (既定 50、50 まで)"},"local":{"type":"string","description":"put: 送るファイル。get: 外の置き場 (既定 build/aish-vm/get/ファイル名)"},"remote":{"type":"string","description":"put: 中の置き場 (既定 /tmp/ファイル名)。get: 取り出す中のファイル"},"file":{"type":"string","description":"shot: png の置き場 (既定 build/aish-vm/shot.png)"},"keys":{"type":"array","items":{"type":"string"},"description":"keys: @ で始まるものは QEMU のキーの名前 (@alt-b @ctrl-l @ret @f5)、ほかは文字として打つ"},"lines":{"type":"integer","description":"log: 終わりから何行 (既定 40)"},"grep":{"type":"string","description":"log: この文字をふくむ行だけ"},"wait_ms":{"type":"integer","description":"start: 待つ長さ (既定 30000、50000 まで。起動しきっていなければ running。もういちど呼ぶと続きを待つ)。job: 終わるまで待つ長さ (既定 0、40000 まで)"},"dir":{"type":"string"}},"required":["op"]}"#;
const TRY: &str = r#"{"type":"object","properties":{"code":{"type":"string","description":"C のソース (main のあるもの)"},"file":{"type":"string","description":"code のかわりにソースのファイル"},"lang":{"type":"string","description":"c (既定) か c++"},"cflags":{"type":"array","items":{"type":"string"},"description":"zig cc に足すもの (-lpthread、-DX=1 など)"},"args":{"type":"string","description":"中で動かすときの引数 (シェルの書き方で)"},"timeout":{"type":"number","description":"秒 (既定 30、50 まで)"},"dir":{"type":"string"}}}"#;
const AB: &str = r#"{"type":"object","properties":{"a":{"type":"string","description":"くらべる 1 つめのカーネル (パス / . / git のコミット。既定 HEAD)"},"b":{"type":"string","description":"2 つめ (既定 . = 作業中のソース)"},"cmd":{"type":"string","description":"中で動かして時間を測るコマンド"},"setup":{"type":"string","description":"起こしたあとに 1 度だけ動かすもの (測らない)"},"www":{"type":"string","description":"このディレクトリを http://10.0.2.2:8000/ に出す (setup で fetch して使う)"},"n":{"type":"integer","description":"カーネルごとに何回 (既定 5)"},"timeout":{"type":"number","description":"1 回の上限 (秒、既定 300)"},"wait_ms":{"type":"integer","description":"待つ長さ (既定 30000、50000 まで)"},"dir":{"type":"string"}},"required":["cmd"]}"#;
const BOX: &str = r#"{"type":"object","properties":{"off":{"type":"array","items":{"type":"string"},"description":"切るもの (kernel.json の extra や quirk のうち off のあるもの。いっしょに切る)"},"each":{"type":"boolean","description":"off のかわりに、切れるものを 1 つずつ全部 (どのテストがどれを要るかの表。長い)"},"test":{"type":"string","description":"中で動かすもの (既定: CI と同じ vm-check.sh と vm-ctests.sh。test/ を http で出して中から取る)。FAIL で始まる行をくらべる"},"www":{"type":"string","description":"test のために http://10.0.2.2:8000/ に出すディレクトリ (既定の test なら test)"},"kernel":{"type":"string","description":"カーネル (パス / . / git のコミット。既定 .)"},"baseline":{"type":"boolean","description":"何も切らないものも動かしてくらべる (既定 true)"},"timeout":{"type":"number","description":"1 回の test の上限 (秒、既定 1200)"},"wait_ms":{"type":"integer","description":"待つ長さ (既定 30000、50000 まで)"},"dir":{"type":"string"}}}"#;
const BENCH: &str = r#"{"type":"object","properties":{"kernel":{"type":"string","description":"測るカーネル (パス / . / git のコミット。既定 .)"},"against":{"type":"string","description":"くらべるカーネル (あれば。たとえば HEAD)。op ごとに、kernel の時間が against の何倍か"},"n":{"type":"integer","description":"カーネルごとに何回 (既定 3。まんなかを使う)"},"scale":{"type":"integer","description":"fsbench の量 (既定 1)"},"off":{"type":"array","items":{"type":"string"},"description":"切って測る extra や quirk (起動の引数 aios.off=)"},"wait_ms":{"type":"integer","description":"待つ長さ (既定 30000、50000 まで)"},"dir":{"type":"string"}}}"#;
const SYSDIFF: &str = r#"{"type":"object","properties":{"seeds":{"type":"integer","description":"いくつの種 (既定 300)"},"from":{"type":"integer","description":"はじめの種 (既定 1)"},"ops":{"type":"integer","description":"種ごとの op の数 (既定 30、200 まで)"},"kernel":{"type":"string","description":"aios のカーネル (パス / . / git のコミット。既定 .)"},"fs":{"type":"string","description":"動かすところ: home (ホームの ext4。既定) か tmp (tmpfs)。Linux も同じ種類のところで"},"linux":{"type":"string","description":"Linux の答えのファイル (ほかの Linux、たとえば aios(main) で cc -O2 -o sysdiff test/sysdiff.c して sysdiff -n OPS DIR FROM-TO > FILE としたもの)。なければこのマシンの Linux で動かす (縮めるのはいつもこのマシンの Linux で)"},"shrink":{"type":"integer","description":"縮めるちがいの数 (多いものから。既定 12、0 で縮めない)"},"wait_ms":{"type":"integer","description":"待つ長さ (既定 30000、50000 まで)"},"dir":{"type":"string"}}}"#;
const BISECT: &str = r#"{"type":"object","properties":{"good":{"type":"string","description":"test が通るコミット"},"bad":{"type":"string","description":"test が通らないコミット (既定 HEAD)"},"test":{"type":"string","description":"中で動かすコマンド。終了コード 0 で good、125 で skip、ほかは bad (起動しない、時間切れも bad)"},"timeout":{"type":"number","description":"1 回の test の上限 (秒、既定 300)"},"wait_ms":{"type":"integer","description":"待つ長さ (既定 30000、50000 まで)"},"dir":{"type":"string"}},"required":["good","test"]}"#;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let (Some(cmd), Some(j)) = (args.first(), args.get(1)) {
        let a = aish_plugin::parse(j).unwrap_or(json!({}));
        let root = PathBuf::from(std::env::var("AIOS_SRC").unwrap_or_else(|_| ".".into()));
        let r = match cmd.as_str() {
            "start" => job_start(&root, &a),
            "ab" => job_ab(&root, &a),
            "bisect" => job_bisect(&root, &a),
            "box" => job_box(&root, &a),
            "bench" => job_bench(&root, &a),
            "sysdiff" => sysdiff::job(&root, &a),
            _ => Err(format!("{}: unknown", cmd)),
        };
        match r {
            Ok(v) => println!("{}", v),
            Err(e) => {
                println!("{}", json!({ "error": e }));
                std::process::exit(1);
            }
        }
        return;
    }
    let spec = Spec {
        name: "vm",
        hooks: &[],
        keys: &[],
        tools: &[
            Tool { name: "vm", desc: "開発のマシンで aios の VM を動かす (test/vm.py): start (kernel: パス / . / git のコミット) stop status run (シリアルの端末でコマンド。bg でうしろで動かして job で様子と出力) put (ファイルを中へ) get (中のファイルを外へ。中で作った png やログを Read で見るなど。16 MB まで) shot (画面を png に。Read で見る) keys (画面のキーボード: @alt-b @ctrl-l @ret、ほかは文字) log (シリアルの終わり、grep) mon (QEMU のモニタ) reboot (中で sudo reboot してプロンプトまで待つ。reset_after 秒で来なければリセット)", input: VM },
            Tool { name: "try", desc: "C (か C++) のコードを aarch64-linux-musl の静的なプログラムにして (zig cc -O2 -static -s。zig がなければ aarch64-linux-gnu-gcc で glibc)、動いている VM の中で動かし、出力と終了コードを返す。カーネルのふるまいをすぐ確かめるため (VM は vm の start で先に)", input: TRY },
            Tool { name: "ab", desc: "2 つのカーネル (a、b: パス / . / git のコミット) で同じ cmd を n 回ずつ動かし、時間 (ms のまんなか、いちばん短い、長い) と大きなロックを持っていた割合をくらべる。動いている VM を止めて -snapshot で起こしなおす。うしろで動くので、終わっていなければ running。もういちど呼ぶと続きを待つ", input: AB },
            Tool { name: "bisect", desc: "test が通る good と通らない bad のあいだのコミットを git bisect で探す (別の作業場所 build/aish-vm/wt で)。コミットごとにカーネルを作り、VM (-snapshot) で test を動かす: 0 で good、125 で skip、ほか (起動しない、時間切れも) bad。変わるのはカーネルだけ。うしろで動くので、もういちど呼ぶと様子か結果 (first_bad)", input: BISECT },
            Tool { name: "bench", desc: "ファイルまわりの速さを測る (test/fsbench.c: create、stat、ないものの stat (2 度目も)、open、4 KiB の書き読み、readdir、深いパス、rename、unlink、mkdir、16 MiB の書き読み、fsync)。VM (-snapshot) で n 回動かして op ごとの µs (大きなファイルは MB/s) のまんなかと、prof をつけた 1 回で part (kernel.json の名前: sysfile、vfs、extfs、vm、block、jbd2) ごとの時間の割合 (どこがボトルネックか) と、時間の多い区間。lock は lockbench (/proc/ai/ctl: スピンロック、ヒープ、大きなロック、時計の 1 回の ns) の 5 回のまんなか。against で 2 つのカーネルをくらべる。結果は build/aish-vm/bench/ にも残る。うしろで動くので、終わっていなければ running。もういちど呼ぶと続きを待つ", input: BENCH },
            Tool { name: "sysdiff", desc: "Linux と答えあわせ (test/sysdiff.c): 種から決まるシステムコールの並び (open、read、write、stat、mkdir、rename、link、symlink、chmod、access、getdents、dup、fcntl、pipe など。fd は前の op が開けたもの) を、このマシンの Linux (root なら nobody で) と VM (-snapshot) の aios で動かし、case ごとに最初にちがう行をくらべる。ちがいは (op、Linux の答え、aios の答え) でまとめ、多いものから、op を減らしても同じちがいが同じ op で出るいちばん小さな並び (SEED:I,J,...) に縮める。aios がえらんだちがい (test/sysdiff.c の chosen、doc/aios.md) は Linux でもそのふるまいをまねて答えにし、chosen に何 case で使ったかを分けて出す。答えはアーキテクチャによらない形 (フラグは名前、stat は種類と許可と数)。結果は build/aish-vm/sysdiff/ にも残る。うしろで動くので、終わっていなければ running。もういちど呼ぶと続きを待つ", input: SYSDIFF },
            Tool { name: "box", desc: "カーネルの砂場: kernel.json の extra や quirk (off のあるもの) を切って (起動の引数 aios.off=) VM (-snapshot) を起こし、test (既定は CI と同じ vm-check と vm-ctests) を動かして、何も切らないときと FAIL の行をくらべる。それがなくても動くか、quirk がまだ要るかを確かめる。each で 1 つずつ全部 (どのテストがどれを要るか)。うしろで動くので、終わっていなければ running。もういちど呼ぶと続きを待つ", input: BOX },
        ],
    };
    let mut jobs: HashMap<String, Job> = HashMap::new();
    aish_plugin::run(spec, |ev, v| match ev {
        "tool" => {
            let a = &v["args"];
            let Some(root) = root(a["dir"].as_str(), s(v, "pwd")) else { return error("no aios repository (test/vm.py) here or above; give dir or set AIOS_SRC") };
            match s(v, "name") {
                "vm" => vm(&mut jobs, &root, a),
                "try" => try_(&root, a),
                "ab" => job(&mut jobs, &root, "ab", a),
                "bisect" => job(&mut jobs, &root, "bisect", a),
                "box" => job(&mut jobs, &root, "box", a),
                "bench" => job(&mut jobs, &root, "bench", a),
                "sysdiff" => job(&mut jobs, &root, "sysdiff", a),
                n => error(format!("{}: no such tool", n)),
            }
        }
        _ => json!({}),
    });
}

/// aios のリポジトリ (test/vm.py のあるところ)
fn root(dir: Option<&str>, pwd: &str) -> Option<PathBuf> {
    let has = |p: &Path| p.join("test/vm.py").is_file();
    if let Some(d) = dir.filter(|d| !d.is_empty()) {
        return has(Path::new(d)).then(|| PathBuf::from(d));
    }
    let mut p = PathBuf::from(if pwd.is_empty() { "." } else { pwd });
    loop {
        if has(&p) {
            return Some(p);
        }
        if !p.pop() {
            break;
        }
    }
    std::env::var("AIOS_SRC").ok().map(PathBuf::from).filter(|p| has(p))
}

/// コマンドを動かす (時間切れなら止める)。(終了コード, 出力, エラー出力, ms, 時間切れ)
fn sh(root: &Path, prog: &str, args: &[&str], env: &[(&str, String)], timeout: Duration) -> (i32, String, String, u64, bool) {
    let t0 = Instant::now();
    let mut c = Command::new(prog);
    c.args(args).current_dir(root).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    for (k, v) in env {
        c.env(k, v);
    }
    let Ok(mut child) = c.spawn() else { return (-1, String::new(), format!("{}: cannot run", prog), 0, false) };
    // 出力はうしろで読む (パイプがいっぱいになって止まらないように)
    let (mut so, mut se) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
    let to = std::thread::spawn(move || {
        let mut b = String::new();
        let _ = std::io::Read::read_to_string(&mut so, &mut b);
        b
    });
    let te = std::thread::spawn(move || {
        let mut b = String::new();
        let _ = std::io::Read::read_to_string(&mut se, &mut b);
        b
    });
    let mut timed_out = false;
    let st = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st.code().unwrap_or(-1),
            Ok(None) if t0.elapsed() < timeout => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                timed_out = true;
                break child.wait().ok().and_then(|s| s.code()).unwrap_or(-1);
            }
        }
    };
    (st, to.join().unwrap_or_default(), te.join().unwrap_or_default(), t0.elapsed().as_millis() as u64, timed_out)
}

/// run の答えに入れる出力の大きさ (aish --mcp の run と同じ)
const MAX_OUT: usize = 12_000;

/// 長ければ頭 (1/3) と終わり (2/3) だけにし、全部は build/aish-vm/out.txt に (read や grep で読めるように)。
/// aish の run と同じ形で、行の切れ目で切る
fn cut(root: &Path, s: String) -> String {
    if s.len() <= MAX_OUT {
        return s;
    }
    let file = root.join("build/aish-vm/out.txt");
    let saved = std::fs::write(&file, &s).is_ok();
    let mut head = MAX_OUT / 3;
    while !s.is_char_boundary(head) {
        head -= 1;
    }
    if let Some(k) = s[..head].rfind('\n') {
        head = k + 1;
    }
    let mut tail = s.len() - MAX_OUT * 2 / 3;
    while !s.is_char_boundary(tail) {
        tail += 1;
    }
    if let Some(k) = s[tail..].find('\n') {
        tail += k + 1;
    }
    let lines = s[head..tail].matches('\n').count();
    let how = if saved { format!(". all of it is in {} (read / grep it)", file.display()) } else { String::new() };
    format!("{}... ({} lines, {} bytes cut{}) ...\n{}", &s[..head], lines, tail - head, how, &s[tail..])
}

/// test/vm.py を動かす
fn vmpy(root: &Path, args: &[&str], env: &[(&str, String)], timeout: Duration) -> (i32, String, String, u64, bool) {
    let mut a = vec!["test/vm.py"];
    a.extend_from_slice(args);
    sh(root, "python3", &a, env, timeout)
}

fn ready(root: &Path) -> bool {
    let (_, out, _, _, _) = vmpy(root, &["status"], &[], Duration::from_secs(15));
    aish_plugin::parse(out.trim()).is_some_and(|v| v["ready"] == true)
}

fn secs(a: &Value, k: &str, default: f64, max: f64) -> f64 {
    a[k].as_f64().unwrap_or(default).clamp(1.0, max)
}

fn vm(jobs: &mut HashMap<String, Job>, root: &Path, a: &Value) -> Value {
    let op = s(a, "op");
    let short = Duration::from_secs(55);
    let res = |(st, out, err, ms, to): (i32, String, String, u64, bool)| {
        let mut r = json!({ "status": st, "out": out, "ms": ms });
        if !err.trim().is_empty() {
            r["err"] = json!(err.trim());
        }
        if to {
            r["timeout"] = json!(true);
        }
        r
    };
    match op {
        "start" => job(jobs, root, "start", a),
        "stop" | "status" => res(vmpy(root, &[op], &[], short)),
        "run" if a["bg"] == true => {
            // 中でうしろで (vm.py bg)。様子と出力は op job で
            let (st, out, err, ms, to) = vmpy(root, &["bg", s(a, "cmd")], &[], short);
            match aish_plugin::parse(out.trim()) {
                Some(mut r) if st == 0 => {
                    r["hint"] = json!("vm op job with this id shows its output and whether it ended (wait_ms to wait, kill to stop it)");
                    r
                }
                _ => res((st, out, err, ms, to)),
            }
        }
        "job" => {
            // うしろのものの様子と、前に見せたところから先の出力。Claude Code は 60 秒しか待たないので 40 秒まで待つ
            // (中で様子を見るのにも少しかかる)
            let w = a["wait_ms"].as_u64().unwrap_or(0).min(40_000) as f64 / 1000.0;
            let mut args = vec!["job".to_string()];
            if let Some(id) = a["id"].as_u64() {
                args.push(id.to_string());
            }
            args.extend(["-w".to_string(), format!("{}", w)]);
            if a["kill"] == true {
                args.push("--kill".into());
            }
            let argv: Vec<&str> = args.iter().map(String::as_str).collect();
            let (st, out, err, ms, to) = vmpy(root, &argv, &[], Duration::from_secs_f64(w + 15.0));
            match aish_plugin::parse(out.trim()) {
                Some(mut r) if st == 0 => {
                    if let Some(o) = r["out"].as_str().map(String::from) {
                        r["out"] = json!(cut(root, symbolize(root, o)));
                    }
                    r
                }
                _ => res((st, out, err, ms, to)),
            }
        }
        "run" => {
            let t = secs(a, "timeout", 50.0, 50.0);
            let (st, out, err, ms, to) = vmpy(root, &["run", s(a, "cmd"), "-t", &format!("{}", t)], &[], Duration::from_secs_f64(t + 5.0));
            res((st, cut(root, symbolize(root, out)), err, ms, to))
        }
        "put" => {
            let local = s(a, "local");
            let name = Path::new(local).file_name().and_then(|n| n.to_str()).unwrap_or("file");
            let remote = a["remote"].as_str().filter(|r| !r.is_empty()).map(String::from).unwrap_or(format!("/tmp/{}", name));
            let mut r = res(vmpy(root, &["put", local, &remote], &[], short));
            r["remote"] = json!(remote);
            r
        }
        "get" => {
            // 中のファイルを外へ (vm.py get: gzip と base64 でシリアルから。sha256 を確かめる)。置き場の既定は build/aish-vm/get/名前
            let remote = s(a, "remote");
            if remote.is_empty() {
                return error("get: give remote (the file inside)");
            }
            let name = Path::new(remote).file_name().and_then(|n| n.to_str()).unwrap_or("file");
            let local = a["local"].as_str().filter(|l| !l.is_empty()).map(PathBuf::from).unwrap_or_else(|| root.join("build/aish-vm/get").join(name));
            let (st, out, err, ms, to) = vmpy(root, &["get", remote, &local.display().to_string()], &[], short);
            match aish_plugin::parse(out.trim()) {
                Some(mut r) if st == 0 => {
                    r["ms"] = json!(ms);
                    r
                }
                _ => res((st, out, err, ms, to)),
            }
        }
        "shot" => {
            let f = a["file"].as_str().filter(|f| !f.is_empty()).map(PathBuf::from).unwrap_or_else(|| root.join("build/aish-vm/shot.png"));
            let _ = std::fs::create_dir_all(f.parent().unwrap_or(Path::new(".")));
            let (st, out, err, ms, _) = vmpy(root, &["shot", &f.display().to_string()], &[], short);
            json!({ "status": st, "file": out.trim(), "err": err.trim(), "ms": ms })
        }
        "keys" => {
            let keys: Vec<String> = a["keys"].as_array().map(|k| k.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
            let mut args = vec!["keys"];
            args.extend(keys.iter().map(String::as_str));
            res(vmpy(root, &args, &[], short))
        }
        "log" => {
            let n = a["lines"].as_u64().unwrap_or(40);
            let g = s(a, "grep");
            // grep するなら多めに読んでから絞る
            let (st, out, err, ms, to) = vmpy(root, &["log", &format!("{}", if g.is_empty() { n } else { 20000 })], &[], short);
            let text = if g.is_empty() { out } else { out.lines().filter(|l| l.contains(g)).collect::<Vec<_>>().iter().rev().take(n as usize).rev().cloned().collect::<Vec<_>>().join("\n") };
            res((st, symbolize(root, text), err, ms, to))
        }
        "mon" => res(vmpy(root, &["mon", s(a, "cmd")], &[], short)),
        "reboot" => {
            // 中で sudo reboot してプロンプトまで待つ (Claude Code は 60 秒しか待たないので 50 秒まで)。
            // reset_after: その秒で来なければリセット (止まった新しいカーネルから、aiboot が前のもので起こしなおす)
            let t = secs(a, "timeout", 50.0, 50.0);
            let mut args = vec!["reboot".to_string(), "-t".into(), format!("{}", t)];
            if let Some(r) = a["reset_after"].as_f64() {
                args.extend(["--reset-after".into(), format!("{}", r)]);
            }
            let argv: Vec<&str> = args.iter().map(String::as_str).collect();
            res(vmpy(root, &argv, &[], Duration::from_secs_f64(t + 5.0)))
        }
        _ => error("op: start stop status run (bg) job put get shot keys log mon reboot"),
    }
}

// ---- C を中で試す ----

/// zig (PATH か $ZIG か build/zig/*/zig)
fn zig(root: &Path) -> Option<String> {
    if let Ok(z) = std::env::var("ZIG") {
        return Some(z);
    }
    if sh(root, "zig", &["version"], &[], Duration::from_secs(10)).0 == 0 {
        return Some("zig".into());
    }
    let d = root.join("build/zig");
    std::fs::read_dir(&d).ok()?.filter_map(|e| e.ok()).map(|e| e.path().join("zig")).find(|p| p.is_file()).map(|p| p.display().to_string())
}

/// C を aarch64 の静的なプログラムにするもの: zig cc (musl) か、なければ aarch64-linux-gnu-gcc (glibc。クラウドの
/// セッションはこちらだけを入れる)。(プログラム, 頭につける引数)
fn compiler(root: &Path, cxx: bool) -> Option<(String, Vec<String>)> {
    if let Some(z) = zig(root) {
        return Some((z, vec![if cxx { "c++" } else { "cc" }.into(), "-target".into(), "aarch64-linux-musl".into()]));
    }
    let gcc = if cxx { "aarch64-linux-gnu-g++" } else { "aarch64-linux-gnu-gcc" };
    (sh(root, gcc, &["--version"], &[], Duration::from_secs(10)).0 == 0).then(|| (gcc.to_string(), vec!["-pthread".into()]))
}

fn try_(root: &Path, a: &Value) -> Value {
    let cxx = s(a, "lang") == "c++";
    let Some((cc, head)) = compiler(root, cxx) else { return error("no compiler: zig (PATH, $ZIG or build/zig/*/zig) or aarch64-linux-gnu-gcc") };
    let dir = root.join("build/aish-vm/try");
    let _ = std::fs::create_dir_all(&dir);
    let src = match a["file"].as_str().filter(|f| !f.is_empty()) {
        Some(f) => PathBuf::from(f),
        None => {
            let p = dir.join(if cxx { "try.cc" } else { "try.c" });
            if std::fs::write(&p, s(a, "code")).is_err() {
                return error("cannot write the source");
            }
            p
        }
    };
    let bin = dir.join("try");
    let (srcs, bins) = (src.display().to_string(), bin.display().to_string());
    let mut args: Vec<String> = head;
    args.extend(["-O2".into(), "-static".into(), "-s".into(), "-o".into(), bins.clone(), srcs]);
    if let Some(f) = a["cflags"].as_array() {
        args.extend(f.iter().filter_map(|x| x.as_str().map(String::from)));
    }
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let (st, _, cerr, cms, _) = sh(root, &cc, &argv, &[], Duration::from_secs(120));
    if st != 0 {
        return json!({ "error": "compile failed", "compile": cerr.trim(), "compile_ms": cms });
    }
    if !ready(root) {
        return error("the VM is not running (vm start first)");
    }
    let (pst, _, perr, _, _) = vmpy(root, &["put", &bins, "/tmp/.try"], &[], Duration::from_secs(55));
    if pst != 0 {
        return json!({ "error": "put failed", "err": perr.trim() });
    }
    let t = secs(a, "timeout", 30.0, 50.0);
    let cmd = format!("chmod +x /tmp/.try && /tmp/.try {}", s(a, "args"));
    let (st, out, err, ms, to) = vmpy(root, &["run", &cmd, "-t", &format!("{}", t)], &[], Duration::from_secs_f64(t + 5.0));
    let mut r = json!({ "status": st, "out": out, "ms": ms, "compile_ms": cms });
    if !err.trim().is_empty() {
        r["err"] = json!(err.trim());
    }
    if to {
        r["timeout"] = json!(true);
    }
    // 警告
    if !cerr.trim().is_empty() {
        r["compile"] = json!(cerr.trim());
    }
    r
}

// ---- うしろで動かすもの ----

/// うしろで動いているもの (自分を aish-vm start / ab / bisect で起こしたもの)。出力は build/aish-vm/KEY.log
struct Job {
    child: std::process::Child,
    log: PathBuf,
    t0: Instant,
    /// 最後に様子を見たとき
    polled: Instant,
}

/// key のものがなければ起こし、wait_ms まで待つ。終わったら最後の行の JSON (と途中の行)
fn job(jobs: &mut HashMap<String, Job>, root: &Path, key: &str, a: &Value) -> Value {
    // running と答えたまま見に来なかったもの (5 分より前) が終わっていたら、続きではなく新しく始める
    // (前の start の結果を、あとの start に返してしまわないように)
    if jobs.get_mut(key).is_some_and(|j| j.polled.elapsed() > Duration::from_secs(300) && matches!(j.child.try_wait(), Ok(Some(_)))) {
        jobs.remove(key);
    }
    if !jobs.contains_key(key) {
        let dir = root.join("build/aish-vm");
        let _ = std::fs::create_dir_all(&dir);
        let log = dir.join(format!("{}.log", key));
        let (Ok(out), Ok(exe)) = (std::fs::File::create(&log), std::env::current_exe()) else { return error(format!("{}: cannot start", log.display())) };
        let Ok(err) = out.try_clone() else { return error("cannot start") };
        match Command::new(exe).args([key, &a.to_string()]).current_dir(root).env("AIOS_SRC", root).stdin(Stdio::null()).stdout(out).stderr(err).spawn() {
            Ok(c) => jobs.insert(key.to_string(), Job { child: c, log, t0: Instant::now(), polled: Instant::now() }),
            Err(e) => return error(e),
        };
    }
    let wait = Duration::from_millis(a["wait_ms"].as_u64().unwrap_or(30_000).min(50_000));
    let end = Instant::now() + wait;
    let j = jobs.get_mut(key).unwrap();
    j.polled = Instant::now();
    let status = loop {
        match j.child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) if Instant::now() < end => std::thread::sleep(Duration::from_millis(200)),
            _ => break None,
        }
    };
    let text = std::fs::read_to_string(&j.log).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    let tail = lines[lines.len().saturating_sub(12)..].join("\n");
    let ms = j.t0.elapsed().as_millis() as u64;
    let Some(st) = status else {
        return json!({ "running": true, "ms": ms, "log": tail, "hint": "call again to wait more" });
    };
    let log = j.log.display().to_string();
    jobs.remove(key);
    let mut r = lines.iter().rev().find_map(|l| aish_plugin::parse(l)).unwrap_or_else(|| json!({}));
    if !st.success() && r.get("error").is_none() {
        r["error"] = json!(format!("exit {}", st));
    }
    r["ms"] = json!(ms);
    r["log"] = json!(log);
    r
}

/// 途中の様子 (ログに 1 行。最後の行だけが答え)
fn note(v: Value) {
    println!("{}", v);
}

/// テストや測るコマンドはサブシェルで (exit 125 などで、中のシリアルの端末のシェルごと終わらないように。
/// 終わると vm.py は終わりの印を見られず、時間切れまで待つ)
fn sub(cmd: &str) -> String {
    format!("(\n{}\n)", cmd)
}

// ---- カーネルを選ぶ ----

/// dir (ワークスペース) のカーネルを作る。できたもののパス
fn build_kernel(root: &Path, dir: &Path) -> Result<PathBuf, String> {
    // 作業中のソースと、コミットの作業場所 (wt) は置き場を分ける。同じ置き場だと、wt で作ったあとに作業中の
    // ソースを作ると、cargo はソースが変わっていないので作りなおさず、置き場の aios (wt のもの) がそのまま残る
    // (ab で a と b が同じカーネルになっていた)
    let target = root.join(if dir == root { "build/aish-vm/target" } else { "build/aish-vm/target-wt" });
    let env = [("AIOS_INITRD", "none".to_string()), ("CARGO_TARGET_DIR", target.display().to_string())];
    let (st, _, err, _, _) = sh(dir, "cargo", &["build", "--release", "-p", "aios"], &env, Duration::from_secs(1800));
    if st != 0 {
        let tail: Vec<&str> = err.lines().filter(|l| l.starts_with("error")).take(5).collect();
        return Err(format!("kernel build failed: {}", tail.join(" / ")));
    }
    Ok(target.join("aarch64-unknown-none-softfloat/release/aios"))
}

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let (st, out, err, _, _) = sh(dir, "git", args, &[], Duration::from_secs(300));
    if st == 0 { Ok(out.trim().to_string()) } else { Err(format!("git {}: {}", args.join(" "), err.trim())) }
}

/// 別の作業場所 (build/aish-vm/wt。なければ作る)
fn worktree(root: &Path) -> Result<PathBuf, String> {
    let wt = root.join("build/aish-vm/wt");
    if !wt.join(".git").exists() {
        let _ = std::fs::create_dir_all(root.join("build/aish-vm"));
        git(root, &["worktree", "add", "--detach", &wt.display().to_string(), "HEAD"])?;
    }
    Ok(wt)
}

/// カーネルの選び方 (パス / . / git のコミット) から、とっておいたカーネルのパス
fn kernel_for(root: &Path, spec: &str) -> Result<PathBuf, String> {
    let kdir = root.join("build/aish-vm/kernels");
    let _ = std::fs::create_dir_all(&kdir);
    if spec.is_empty() {
        return Err("no kernel".into());
    }
    let p = if Path::new(spec).is_absolute() { PathBuf::from(spec) } else { root.join(spec) };
    if spec != "." && p.is_file() {
        return Ok(p);
    }
    if spec == "." {
        let k = build_kernel(root, root)?;
        let dst = kdir.join("work");
        std::fs::copy(&k, &dst).map_err(|e| e.to_string())?;
        return Ok(dst);
    }
    let sha = git(root, &["rev-parse", "--verify", &format!("{}^{{commit}}", spec)])?;
    let dst = kdir.join(&sha);
    if dst.is_file() {
        return Ok(dst);
    }
    let wt = worktree(root)?;
    git(&wt, &["checkout", "--detach", "-f", &sha])?;
    let k = build_kernel(root, &wt)?;
    std::fs::copy(&k, &dst).map_err(|e| e.to_string())?;
    Ok(dst)
}

/// いま動いている VM のカーネルの ELF (boot で書く)
fn booted(root: &Path) -> PathBuf {
    root.join("build/aish-vm/booted-kernel")
}

/// カーネルの panic のバックトレース (backtrace: 0x... の行) があれば、addr2line で関数の名前にして足す
fn symbolize(root: &Path, text: String) -> String {
    let Some(line) = text.lines().find_map(|l| l.trim_start().strip_prefix("backtrace: 0x").map(|_| l.trim_start())) else { return text };
    let Ok(k) = std::fs::read_to_string(booted(root)) else { return text };
    let addrs: Vec<&str> = line["backtrace:".len()..].split_whitespace().collect();
    let Ok(o) = Command::new("addr2line").arg("-e").arg(k.trim()).args(["-f", "-C", "-p"]).args(&addrs).output() else { return text };
    let names: Vec<String> = String::from_utf8_lossy(&o.stdout).lines().map(|l| format!("  {}", if l.ends_with(":?") { l.rsplit_once(" at ").map_or(l, |x| x.0) } else { l })).collect();
    if names.is_empty() {
        return text;
    }
    format!("{}\nbacktrace (addr2line):\n{}", text, names.join("\n"))
}

/// VM を止めて、kernel で起こす (snapshot ならディスクに書き残さない、off は切って)。起動に失敗したら Err
fn boot(root: &Path, kernel: Option<&Path>, snapshot: bool, www: &str, off: &[String]) -> Result<Value, String> {
    vmpy(root, &["stop"], &[], Duration::from_secs(90));
    let mut env: Vec<(&str, String)> = Vec::new();
    if let Some(k) = kernel {
        env.push(("AIOS_KERNEL", k.display().to_string()));
    }
    // カーネルのコマンドライン (bin/run.sh の AIOS_CMDLINE は、カーネルを直に起こすときの -append)
    if !off.is_empty() {
        env.push(("AIOS_CMDLINE", format!("aios.off={}", off.join(","))));
    }
    // panic のバックトレースを関数の名前にするため (symbolize)。disk.img のカーネルなら無い
    let _ = match kernel {
        Some(k) => std::fs::write(booted(root), k.display().to_string()),
        None => std::fs::remove_file(booted(root)),
    };
    if snapshot {
        env.push(("AIOS_QEMU_ARGS", "-snapshot".into()));
    }
    let mut args = vec!["start"];
    if !www.is_empty() {
        args.extend(["--www", www]);
    }
    let (st, out, err, ms, _) = vmpy(root, &args, &env, Duration::from_secs(1000));
    let r = aish_plugin::parse(out.trim()).unwrap_or(json!({}));
    if st != 0 || r["ready"] != true {
        return Err(format!("the VM did not come up ({} ms): {}", ms, err.lines().rev().take(3).collect::<Vec<_>>().join(" / ")));
    }
    Ok(r)
}

fn job_start(root: &Path, a: &Value) -> Result<Value, String> {
    let k = match a["kernel"].as_str().filter(|k| !k.is_empty()) {
        Some(spec) => {
            note(json!({ "step": "kernel", "spec": spec }));
            Some(kernel_for(root, spec)?)
        }
        None => None,
    };
    let www = a["www"].as_str().unwrap_or("");
    let off = strings(&a["off"]);
    if !off.is_empty() {
        let names = switchable(root)?;
        if let Some(n) = off.iter().find(|n| !names.contains(n)) {
            return Err(format!("off: {} cannot be turned off (kernel.json: {})", n, names.join(" ")));
        }
    }
    let r = boot(root, k.as_deref(), a["snapshot"] == true, www, &off)?;
    let mut v = json!({ "ready": true, "boot_s": r["boot_s"], "kernel": k.map(|p| p.display().to_string()) });
    if !off.is_empty() {
        v["off"] = json!(off);
    }
    Ok(v)
}

// ---- くらべる ----

fn median(v: &[u64]) -> u64 {
    let mut s = v.to_vec();
    s.sort();
    s.get(s.len() / 2).copied().unwrap_or(0)
}

/// 大きなロックを持っていた割合 (/proc/ai/bkl、なければ /proc/bkl の all の行)
fn bkl_hold(root: &Path) -> Option<f64> {
    let (_, out, _, _, _) = vmpy(root, &["run", "sudo cat /proc/ai/bkl 2>/dev/null || grep '^all' /proc/bkl", "-t", "20"], &[], Duration::from_secs(30));
    if let Some(v) = out.lines().find_map(aish_plugin::parse) {
        return v["hold_pct"].as_f64();
    }
    // all  待った ms 待った%  持っていた ms 持っていた%
    let l = out.lines().find(|l| l.starts_with("all"))?;
    let pcts: Vec<f64> = l.split_whitespace().filter_map(|w| w.strip_suffix('%')?.parse().ok()).collect();
    pcts.get(1).copied()
}

fn job_ab(root: &Path, a: &Value) -> Result<Value, String> {
    let n = a["n"].as_u64().unwrap_or(5).clamp(1, 100) as usize;
    let cmd = s(a, "cmd");
    let t = Duration::from_secs_f64(secs(a, "timeout", 300.0, 3600.0));
    let specs = [a["a"].as_str().filter(|x| !x.is_empty()).unwrap_or("HEAD"), a["b"].as_str().filter(|x| !x.is_empty()).unwrap_or(".")];
    let mut res = Vec::new();
    for spec in specs {
        note(json!({ "step": "kernel", "spec": spec }));
        let k = kernel_for(root, spec)?;
        note(json!({ "step": "boot", "spec": spec }));
        boot(root, Some(&k), true, a["www"].as_str().unwrap_or(""), &[])?;
        let setup = s(a, "setup");
        if !setup.is_empty() {
            // setup は cd などを残すためのものなので、そのままのシェルで
            vmpy(root, &["run", setup, "-t", &format!("{}", t.as_secs())], &[], t + Duration::from_secs(10));
        }
        vmpy(root, &["run", "sudo sh -c 'echo > /proc/bkl'", "-t", "20"], &[], Duration::from_secs(30));
        let (mut ms, mut fails) = (Vec::new(), 0);
        for i in 0..n {
            let (st, _, _, m, to) = vmpy(root, &["run", &sub(cmd), "-t", &format!("{}", t.as_secs())], &[], t + Duration::from_secs(10));
            if st != 0 || to {
                fails += 1;
            }
            ms.push(m);
            note(json!({ "step": "run", "spec": spec, "i": i, "ms": m, "status": st }));
        }
        let hold = bkl_hold(root);
        res.push(json!({ "kernel": spec, "path": k.display().to_string(), "ms": ms, "median_ms": median(&ms), "min_ms": ms.iter().min(), "max_ms": ms.iter().max(), "failures": fails, "bkl_hold_pct": hold }));
    }
    vmpy(root, &["stop"], &[], Duration::from_secs(90));
    let (m0, m1) = (res[0]["median_ms"].as_u64().unwrap_or(0) as f64, res[1]["median_ms"].as_u64().unwrap_or(0) as f64);
    let change = if m0 > 0.0 { (m1 - m0) / m0 * 100.0 } else { 0.0 };
    // はっきりした差か: いちばん短い〜長いの範囲が重ならない (QEMU の上の時間は 10% ほどぶれる)
    let range = |r: &Value| (r["min_ms"].as_u64().unwrap_or(0), r["max_ms"].as_u64().unwrap_or(0));
    let ((a0, a1), (b0, b1)) = (range(&res[0]), range(&res[1]));
    let clear = b1 < a0 || a1 < b0;
    let verdict = match (clear, m1 < m0) {
        (false, _) => "no clear difference (the ranges overlap; try a larger n or a longer cmd)",
        (true, true) => "b is faster",
        (true, false) => "b is slower",
    };
    Ok(json!({ "a": res[0], "b": res[1], "b_vs_a_pct": (change * 10.0).round() / 10.0, "clear": clear, "verdict": verdict, "note": "ms includes the serial round trip (about the same for both); the VM ran with -snapshot" }))
}

// ---- さがす ----

fn job_bisect(root: &Path, a: &Value) -> Result<Value, String> {
    let good = s(a, "good");
    let bad = a["bad"].as_str().filter(|x| !x.is_empty()).unwrap_or("HEAD");
    let test = s(a, "test");
    let t = Duration::from_secs_f64(secs(a, "timeout", 300.0, 3600.0));
    let (good, bad) = (git(root, &["rev-parse", "--verify", &format!("{}^{{commit}}", good)])?, git(root, &["rev-parse", "--verify", &format!("{}^{{commit}}", bad)])?);
    let wt = worktree(root)?;
    let _ = git(&wt, &["bisect", "reset"]);
    git(&wt, &["checkout", "--detach", "-f", &bad])?;
    let mut out = git(&wt, &["bisect", "start", &bad, &good])?;
    let mut steps = Vec::new();
    let result = loop {
        if let Some(first) = out.lines().find(|l| l.contains("is the first bad commit")) {
            let sha = first.split_whitespace().next().unwrap_or("").to_string();
            let subject = git(root, &["log", "-1", "--format=%s", &sha]).unwrap_or_default();
            break Ok(json!({ "first_bad": sha, "subject": subject }));
        }
        if steps.len() >= 40 {
            break Err("too many steps".to_string());
        }
        let sha = git(&wt, &["rev-parse", "HEAD"])?;
        let t0 = Instant::now();
        // カーネルを作る (作れなければ skip)、起こす (起きなければ bad)、test (0 good、125 skip、ほか bad)
        let verdict = match kernel_for(root, &sha) {
            Err(e) => ("skip", e),
            Ok(k) => match boot(root, Some(&k), true, "", &[]) {
                Err(e) => ("bad", e),
                Ok(_) => {
                    let (st, o, _, _, to) = vmpy(root, &["run", &sub(test), "-t", &format!("{}", t.as_secs())], &[], t + Duration::from_secs(10));
                    let last = o.lines().rev().take(3).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join(" / ");
                    match (st, to) {
                        (_, true) => ("bad", format!("timeout: {}", last)),
                        (0, _) => ("good", last),
                        (125, _) => ("skip", last),
                        _ => ("bad", format!("exit {}: {}", st, last)),
                    }
                }
            },
        };
        let subject = git(root, &["log", "-1", "--format=%s", &sha]).unwrap_or_default();
        let step = json!({ "sha": &sha[..12.min(sha.len())], "subject": subject, "result": verdict.0, "why": verdict.1, "ms": t0.elapsed().as_millis() as u64 });
        note(step.clone());
        steps.push(step);
        out = match git(&wt, &["bisect", verdict.0]) {
            Ok(o) => o,
            Err(e) => break Err(e),
        };
    };
    let _ = git(&wt, &["bisect", "reset"]);
    vmpy(root, &["stop"], &[], Duration::from_secs(90));
    let mut r = result?;
    r["steps"] = json!(steps);
    r["note"] = json!("only the kernel changes between steps; the disk (userland) is the current disk.img");
    Ok(r)
}

// ---- カーネルの砂場 ----

/// 文字列の並び (JSON の配列から)
fn strings(v: &Value) -> Vec<String> {
    v.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).filter(|x| !x.is_empty()).collect()).unwrap_or_default()
}

/// 切れるもの: kernel/kernel.json の off のある名前 (kernel/src/parts.rs と同じ。test/kernel-json.py が確かめる)
fn switchable(root: &Path) -> Result<Vec<String>, String> {
    let t = std::fs::read_to_string(root.join("kernel/kernel.json")).map_err(|e| format!("kernel/kernel.json: {}", e))?;
    let v = aish_plugin::parse(&t).ok_or("kernel/kernel.json: not JSON")?;
    Ok(v.as_array().map(|a| a.iter().filter(|e| e.get("off").is_some()).filter_map(|e| e["name"].as_str().map(String::from)).collect()).unwrap_or_default())
}

/// CI と同じテスト (test/vm-check.sh と test/vm-ctests.sh)。C のテストは中で動かす静的なプログラムに作っておく
const DEFAULT_TEST: &str = "fetch http://10.0.2.2:8000/vm-check.sh > /tmp/vm-check.sh && sh /tmp/vm-check.sh; a=$?; \
fetch http://10.0.2.2:8000/vm-ctests.sh > /tmp/vm-ctests.sh && sh /tmp/vm-ctests.sh run; b=$?; exit $((a + b))";

fn build_ctests(root: &Path) -> Result<(), String> {
    let cc = ["aarch64-linux-gnu-gcc", "gcc"].into_iter().find(|c| {
        let (st, out, _, _, _) = sh(root, c, &["-dumpmachine"], &[], Duration::from_secs(10));
        st == 0 && out.starts_with("aarch64")
    });
    let Some(cc) = cc else { return Err("no aarch64 C compiler for the C tests (aarch64-linux-gnu-gcc)".into()) };
    let (st, _, err, _, _) = sh(root, "sh", &["test/vm-ctests.sh", "build", cc], &[], Duration::from_secs(600));
    if st != 0 {
        return Err(format!("test/vm-ctests.sh build: {}", err.lines().last().unwrap_or("")));
    }
    Ok(())
}

fn job_box(root: &Path, a: &Value) -> Result<Value, String> {
    let names = switchable(root)?;
    let sets: Vec<Vec<String>> = if a["each"] == true { names.iter().map(|n| vec![n.clone()]).collect() } else { vec![strings(&a["off"])] };
    if sets.iter().all(|s| s.is_empty()) {
        return Err(format!("give off (or each): {}", names.join(" ")));
    }
    if let Some(n) = sets.iter().flatten().find(|n| !names.contains(n)) {
        return Err(format!("{} cannot be turned off (kernel.json: {})", n, names.join(" ")));
    }
    let custom = s(a, "test");
    let test = if custom.is_empty() { DEFAULT_TEST } else { custom };
    let www = match a["www"].as_str().filter(|w| !w.is_empty()) {
        Some(w) => w.to_string(),
        None if custom.is_empty() => "test".to_string(),
        None => String::new(),
    };
    if custom.is_empty() {
        note(json!({ "step": "ctests" }));
        build_ctests(root)?;
    }
    let spec = a["kernel"].as_str().filter(|x| !x.is_empty()).unwrap_or(".");
    note(json!({ "step": "kernel", "spec": spec }));
    let k = kernel_for(root, spec)?;
    let t = Duration::from_secs_f64(secs(a, "timeout", 1200.0, 3600.0));
    // 1 つの組: 起こして (起きなければそれが答え)、切れているかを確かめ、test を動かして FAIL の行を集める
    let run = |off: &[String]| -> Value {
        note(json!({ "step": "boot", "off": off }));
        let t0 = Instant::now();
        if let Err(e) = boot(root, Some(&k), true, &www, off) {
            return json!({ "off": off, "boot": false, "error": e });
        }
        let (_, kmsg, _, _, _) = vmpy(root, &["run", "grep -a 'parts:' /proc/kmsg", "-t", "20"], &[], Duration::from_secs(30));
        let parts = kmsg.lines().find(|l| l.contains("parts: off")).map(|l| l.trim().to_string());
        if !off.is_empty() && parts.is_none() {
            return json!({ "off": off, "boot": true, "error": "the kernel did not say parts: off (aios.off= did not reach it)" });
        }
        let (st, out, _, ms, to) = vmpy(root, &["run", &sub(test), "-t", &format!("{}", t.as_secs())], &[], t + Duration::from_secs(10));
        // FAIL の行 (字下げしたものは、その中の様子なので数えない)
        let fails: Vec<String> = out.lines().filter(|l| l.starts_with("FAIL")).map(|l| l.trim_end().trim_end_matches(':').to_string()).collect();
        let last: Vec<&str> = out.lines().rev().take(3).collect::<Vec<_>>().into_iter().rev().collect();
        note(json!({ "step": "test", "off": off, "status": st, "fails": fails.len(), "ms": ms }));
        json!({ "off": off, "boot": true, "parts": parts, "status": st, "timeout": to, "fails": fails, "last": last.join(" / "), "ms": t0.elapsed().as_millis() as u64 })
    };
    let base = (a["baseline"] != false).then(|| run(&[]));
    let base_fails: Vec<String> = base.as_ref().map(|b| strings(&b["fails"])).unwrap_or_default();
    let base_status = base.as_ref().and_then(|b| b["status"].as_i64()).unwrap_or(0);
    let mut runs = Vec::new();
    let mut table = Vec::new();
    for set in &sets {
        let mut r = run(set);
        let fails = strings(&r["fails"]);
        let new: Vec<&String> = fails.iter().filter(|f| !base_fails.contains(f)).collect();
        let gone: Vec<&String> = base_fails.iter().filter(|f| !fails.contains(f)).collect();
        let line = if r["boot"] == false || r.get("error").is_some() {
            format!("{}: {}", set.join(","), r["error"].as_str().unwrap_or("did not come up"))
        } else if r["timeout"] == true {
            format!("{}: the test timed out", set.join(","))
        } else if new.is_empty() && r["status"].as_i64().unwrap_or(0) != base_status {
            // FAIL の行はないが、終わりの番号がちがう (テストが始められなかったなど)
            format!("{}: the test exited {} (with nothing off {}) without a new FAIL line: {}", set.join(","), r["status"], base_status, r["last"].as_str().unwrap_or(""))
        } else if new.is_empty() && (base.is_some() || fails.is_empty()) {
            format!("{}: nothing new fails", set.join(","))
        } else {
            format!("{}: {} new: {}", set.join(","), new.len(), new.iter().map(|f| f.as_str()).collect::<Vec<_>>().join("; "))
        };
        r["new_fails"] = json!(new);
        if !gone.is_empty() {
            r["passes_only_off"] = json!(gone);
        }
        table.push(line);
        runs.push(r);
    }
    vmpy(root, &["stop"], &[], Duration::from_secs(90));
    let mut out = json!({ "kernel": spec, "path": k.display().to_string(), "results": table, "runs": runs, "note": "each ran on a fresh -snapshot VM; new means a FAIL line the run with nothing off does not have" });
    if let Some(b) = base {
        out["baseline"] = b;
    }
    if custom.is_empty() && sets.iter().any(|s| s.iter().any(|n| n == "net")) {
        out["hint"] = json!("the default test fetches its scripts over the network, so with net off it cannot start; give a test that needs no network");
    }
    Ok(out)
}

// ---- 速さを測る ----

/// test/fsbench.c を中で動かす静的なプログラムに (build/aish-vm/bench/fsbench。ソースが新しいときだけ作りなおす)
fn build_fsbench(root: &Path) -> Result<PathBuf, String> {
    let dir = root.join("build/aish-vm/bench");
    let _ = std::fs::create_dir_all(&dir);
    let (src, out) = (root.join("test/fsbench.c"), dir.join("fsbench"));
    let mtime = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    if let (Some(o), Some(s)) = (mtime(&out), mtime(&src)) && o >= s {
        return Ok(out);
    }
    let Some((cc, head)) = compiler(root, false) else { return Err("no compiler: zig or aarch64-linux-gnu-gcc".into()) };
    let mut args = head;
    args.extend(["-O2".into(), "-static".into(), "-s".into(), "-o".into(), out.display().to_string(), src.display().to_string()]);
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let (st, _, err, _, _) = sh(root, &cc, &argv, &[], Duration::from_secs(120));
    if st != 0 {
        return Err(format!("fsbench: {}", err.trim()));
    }
    Ok(out)
}

/// lockbench の答え (kmsg の「lockbench: NAME 49.2 ns, NAME 260.7 ns, ...」に LB をつけた行) を、NAME ごとの
/// まんなか、いちばん小さい、大きい (ns) に。lockbench のないカーネルなら空
fn lockbench(out: &str) -> Value {
    let mut vals: Vec<(String, Vec<f64>)> = Vec::new();
    for l in out.lines().filter_map(|l| l.trim_start().strip_prefix("LB ")) {
        let Some((_, rest)) = l.split_once("lockbench: ") else { continue };
        for piece in rest.split(", ") {
            let Some((name, x)) = piece.trim().strip_suffix(" ns").and_then(|p| p.rsplit_once(' ')) else { continue };
            let Ok(x) = x.parse::<f64>() else { continue };
            match vals.iter_mut().find(|e| e.0 == name) {
                Some(e) => e.1.push(x),
                None => vals.push((name.to_string(), vec![x])),
            }
        }
    }
    let mut r = json!({});
    for (name, v) in &vals {
        let (med, lo, hi) = spread(v);
        r[name.as_str()] = json!({ "ns": med, "min": lo, "max": hi });
    }
    r
}

/// まんなか、いちばん小さい、大きい
fn spread(v: &[f64]) -> (f64, f64, f64) {
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    (s.get(s.len() / 2).copied().unwrap_or(0.0), s.first().copied().unwrap_or(0.0), s.last().copied().unwrap_or(0.0))
}

fn job_bench(root: &Path, a: &Value) -> Result<Value, String> {
    let n = a["n"].as_u64().unwrap_or(3).clamp(1, 20) as usize;
    let scale = a["scale"].as_u64().unwrap_or(1).clamp(1, 100);
    let off = strings(&a["off"]);
    if !off.is_empty() {
        let names = switchable(root)?;
        if let Some(x) = off.iter().find(|x| !names.contains(x)) {
            return Err(format!("off: {} cannot be turned off (kernel.json: {})", x, names.join(" ")));
        }
    }
    note(json!({ "step": "fsbench" }));
    let bin = build_fsbench(root)?;
    let www = bin.parent().unwrap_or(root).display().to_string();
    let t = Duration::from_secs(600);
    // ext4 の上 (ホーム) で動かす。/tmp は tmpfs のこともある
    let run_cmd = format!("cd ~ && /tmp/fsbench . {}", scale);
    let measure = |spec: &str| -> Result<Value, String> {
        note(json!({ "step": "kernel", "spec": spec }));
        let k = kernel_for(root, spec)?;
        note(json!({ "step": "boot", "spec": spec }));
        boot(root, Some(&k), true, &www, &off)?;
        let (st, out, _, _, _) = vmpy(root, &["run", "fetch http://10.0.2.2:8000/fsbench -o /tmp/fsbench >/dev/null && chmod +x /tmp/fsbench && echo fetched", "-t", "60"], &[], Duration::from_secs(70));
        if st != 0 || !out.contains("fetched") {
            return Err(format!("{}: cannot fetch fsbench into the VM: {}", spec, out.trim()));
        }
        // 1 回目は捨てる (起動したばかりのキャッシュ)
        vmpy(root, &["run", &sub(&format!("{} > /dev/null", run_cmd)), "-t", &format!("{}", t.as_secs())], &[], t + Duration::from_secs(10));
        // op → 回ごとの値 (µs か MB/s)
        let mut vals: Vec<(String, &'static str, Vec<f64>)> = Vec::new();
        let mut totals = Vec::new();
        for i in 0..n {
            let (st, out, _, ms, to) = vmpy(root, &["run", &sub(&run_cmd), "-t", &format!("{}", t.as_secs())], &[], t + Duration::from_secs(10));
            if st != 0 || to {
                return Err(format!("{}: fsbench failed (status {}): {}", spec, st, out.lines().rev().take(3).collect::<Vec<_>>().join(" / ")));
            }
            for v in out.lines().filter_map(aish_plugin::parse) {
                if let Some(t) = v["total_ms"].as_f64() {
                    totals.push(t);
                }
                let Some(op) = v["op"].as_str() else { continue };
                let (unit, x) = match (v["us"].as_f64(), v["mb_s"].as_f64()) {
                    (Some(us), _) => ("us", us),
                    (None, Some(mb)) => ("mb_s", mb),
                    _ => continue,
                };
                match vals.iter_mut().find(|e| e.0 == op) {
                    Some(e) => e.2.push(x),
                    None => vals.push((op.to_string(), unit, vec![x])),
                }
            }
            note(json!({ "step": "run", "spec": spec, "i": i, "ms": ms }));
        }
        // ロックなどの 1 回の時間 (lockbench を 5 回。kmsg の答えに LB をつけて、シリアルに出たカーネルの行と分ける)
        let lb = "for i in 1 2 3 4 5; do echo lockbench > /proc/ai/ctl; done 2>/dev/null; grep 'lockbench:' /proc/kmsg | tail -5 | sed 's/^/LB /'";
        let (_, lout, _, _, _) = vmpy(root, &["run", &sub(lb), "-t", "60"], &[], Duration::from_secs(70));
        let lock = lockbench(&lout);
        // prof をつけて 1 回 (数えるぶん少し遅くなるので、時間は上の回のものを使う)
        let prof_cmd = format!("echo prof on > /proc/ai/ctl && ({} > /dev/null); echo prof off > /proc/ai/ctl; cat /proc/ai/prof", run_cmd);
        let (_, pout, _, _, _) = vmpy(root, &["run", &sub(&prof_cmd), "-t", &format!("{}", t.as_secs())], &[], t + Duration::from_secs(10));
        let prof: Vec<Value> = pout.lines().filter_map(aish_plugin::parse).collect();
        let parts: Vec<Value> = prof.iter().filter(|v| v.get("part").is_some()).cloned().collect();
        let spans: Vec<Value> = prof.iter().filter(|v| v.get("name").is_some()).take(20).cloned().collect();
        let mut ops = json!({});
        for (op, unit, v) in &vals {
            let (med, lo, hi) = spread(v);
            ops[op.as_str()] = json!({ unit.to_string(): (med * 100.0).round() / 100.0, "min": lo, "max": hi });
        }
        let (tmed, _, _) = spread(&totals);
        let mut r = json!({ "kernel": spec, "path": k.display().to_string(), "total_ms": tmed, "ops": ops, "parts": parts, "spans": spans });
        // lockbench のないカーネル (古いもの) なら出さない
        if lock.as_object().is_some_and(|o| !o.is_empty()) {
            r["lock"] = lock;
        }
        if parts.is_empty() {
            // 数えられなかった (/proc/ai/ctl に書けない、aiproc を切った、など)
            r["prof_error"] = json!(pout.lines().filter(|l| !l.trim().is_empty()).take(3).collect::<Vec<_>>().join(" / "));
        }
        Ok(r)
    };
    let spec = a["kernel"].as_str().filter(|x| !x.is_empty()).unwrap_or(".");
    let against = a["against"].as_str().filter(|x| !x.is_empty());
    let base = against.map(&measure).transpose()?;
    let mine = measure(spec)?;
    vmpy(root, &["stop"], &[], Duration::from_secs(90));
    let mut out = json!({ "bench": "fs", "n": n, "scale": scale, "result": mine.clone(), "note": "us: microseconds per operation (smaller is faster); mb_s: megabytes per second (bigger is faster). Each kernel ran on a fresh -snapshot VM; the first run is thrown away; parts come from one more run with prof on; lock: nanoseconds per lockbench step (median of 5)" });
    if !off.is_empty() {
        out["off"] = json!(off);
    }
    if let Some(b) = base {
        // op ごとに、kernel の時間が against の何倍か (1 より小さければ速い)。範囲が重ならなければ clear
        let mut cmp = Vec::new();
        let verdict = |m: &Value, o: &Value, faster: bool| {
            let (mlo, mhi, olo, ohi) = (m["min"].as_f64().unwrap_or(0.0), m["max"].as_f64().unwrap_or(0.0), o["min"].as_f64().unwrap_or(0.0), o["max"].as_f64().unwrap_or(0.0));
            match (mhi < olo || ohi < mlo, faster) {
                (false, _) => "no clear difference",
                (true, true) => "faster",
                (true, false) => "slower",
            }
        };
        if let Some(ops) = mine["ops"].as_object() {
            for (op, m) in ops {
                let o = &b["ops"][op];
                let (unit, mv, ov) = if let Some(us) = m["us"].as_f64() { ("us", us, o["us"].as_f64().unwrap_or(0.0)) } else { ("mb_s", m["mb_s"].as_f64().unwrap_or(0.0), o["mb_s"].as_f64().unwrap_or(0.0)) };
                if mv <= 0.0 || ov <= 0.0 {
                    continue;
                }
                let time_ratio = if unit == "us" { mv / ov } else { ov / mv };
                cmp.push(format!("{}: {:.2}x the time of {} ({})", op, time_ratio, against.unwrap_or(""), verdict(m, o, time_ratio < 1.0)));
            }
        }
        // lockbench (どちらのカーネルにもあるもの)
        if let Some(lk) = mine["lock"].as_object() {
            for (name, m) in lk {
                let o = &b["lock"][name];
                let (Some(mv), Some(ov)) = (m["ns"].as_f64(), o["ns"].as_f64()) else { continue };
                if mv > 0.0 && ov > 0.0 {
                    cmp.push(format!("lockbench {}: {:.2}x the time of {} ({})", name, mv / ov, against.unwrap_or(""), verdict(m, o, mv < ov)));
                }
            }
        }
        out["against"] = b;
        out["compare"] = json!(cmp);
    }
    // 残す: build/aish-vm/bench/UNIX秒.json
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let file = root.join("build/aish-vm/bench").join(format!("{}.json", secs));
    if std::fs::write(&file, format!("{}\n", out)).is_ok() {
        out["saved"] = json!(file.display().to_string());
    }
    Ok(out)
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lockbench_lines() {
        // シリアルに出たカーネルの行 (LB がない) は数えない
        let out = "lockbench: SpinLock lock+unlock 999.0 ns\n\
                   LB [    9.754366] lockbench: SpinLock lock+unlock 49.2 ns, heap alloc+free 260.7 ns, BKL unlock+lock 661.0 ns, uptime_ns 175.4 ns\n\
                   LB [   10.009001] lockbench: SpinLock lock+unlock 53.1 ns, heap alloc+free 507.1 ns, BKL unlock+lock 442.7 ns, uptime_ns 204.4 ns\n\
                   LB [   10.349281] lockbench: SpinLock lock+unlock 46.1 ns, heap alloc+free 251.7 ns, BKL unlock+lock 438.7 ns, uptime_ns 163.7 ns\n";
        let r = lockbench(out);
        assert_eq!(r.as_object().map(|o| o.len()), Some(4));
        assert_eq!(r["SpinLock lock+unlock"]["ns"].as_f64(), Some(49.2));
        assert_eq!(r["SpinLock lock+unlock"]["max"].as_f64(), Some(53.1));
        assert_eq!(r["BKL unlock+lock"]["min"].as_f64(), Some(438.7));
        assert_eq!(r["uptime_ns"]["ns"].as_f64(), Some(175.4));
        // lockbench のないカーネル
        assert!(lockbench("sh: write error: Invalid argument\n").as_object().is_some_and(|o| o.is_empty()));
    }
}

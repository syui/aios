// aish-sys: aios の様子を見る (aios を作るための、aish の基本のプラグイン。doc/aios.md の Claude の入口)
//   get     状態の木 (aios get --json。host kernel mem disk proc service pkg net user boot log)
//   do      aiosd に頼んで変える (サービス、パッケージ、再起動、apply / rollback。aios do と同じ)
//   diff    /etc/aios.json といまのちがい (aios diff)
//   sys     まとめ: カーネル、起きてからの時間、CPU、メモリ、スワップ、ディスク、重いプロセス、BKL、カーネルのメッセージ
//   procs   プロセスの一覧 (CPU の時間かメモリの順)
//   kmsg    カーネルのメッセージ (/proc/kmsg。シリアルの画面にしか出なかった println! のもの)
//   log     サービスのログ (/var/log/UNIT.log。journalctl -u と同じもの)
//   bkl     コマンドを 1 つ動かして、そのあいだの大きなロック (/proc/bkl) を測る
//   strace  コマンドを 1 つ動かして、そのシステムコールを kmsg から取る (/proc/strace)
//   threads スレッドの一覧 (/proc/ai/threads): 眠っていれば何を (futex / 見張っている fd とその中身) どれだけ待っているか
//   hang    プロセスがなぜ止まっているか: 長く眠っているスレッドと待っているもの、読まれずに残っているデータ、
//           futex をみな起こしても同じところで眠りなおすか (起こしが消えたのか、だれも起こしていないのか)
//   stack   スレッドの呼び出しの並び (/proc/ai/stack と、写しているファイルの .eh_frame。unwind.rs)
//   watch   aiwatch (カーネルの見張り) がいま見つけているものと、最近の記録 (/run/aiwatch.json、/var/log/aiwatch.service.log)
//   tune    sysctl の値を順に変えてコマンドの時間をくらべる (aios tune。--apply でいちばん速い値を /etc/aios.json に)
//   mem     メモリの中 (/proc/ai/mem): RAM、カーネルのヒープ、ページキャッシュ (ファイルごと)、tmpfs、スワップ、
//           メモリを多く使っているプロセス
//   fs      ファイルシステムの中 (/proc/ai/fs): マウントごと (ext4 のブロックキャッシュ、ジャーナル、inode の覚え)、
//           パスの覚え、開いているファイル、ディスクの読み書き
//   fail    わざとしくじらせる (/proc/ai/ctl の fail): cmd のプロセスのシステムコールを -ERRNO に、カーネルのヒープや
//           ページを取れないことにして、どう耐えるかを見る
//   delta   cmd の前と後のカーネルの中の数のちがい (/proc/ai/mem、/proc/ai/fs、/proc/bkl): ページキャッシュ、ヒープ、
//           ディスク、パスの覚え、大きなロック
//   part    カーネルの切れるところ (/proc/ai/parts): いま切ってあるか。off / on で、動かしたまま切る・入れる (ctl の part)
//   trace   カーネルの中の出来事 (/proc/ai/trace): cmd を動かしているあいだのシステムコールの入りと出、切りかえ、眠りと
//           起こし、割り込み、ディスク、フォルトを記録して、スレッドごと・システムコールごと・遅かったものにまとめる
//   M-s     まとめを画面に出す (キーを押すと消える)
// /proc/bkl、/proc/strace を 0 からにするのは root だけなので、root でなければ sudo -n tee で書く。
// 開発の Linux でも動く (aios にしかないものは「ない」と答える)
mod unwind;

use aish_plugin::{Spec, Tool, Tty, Value, error, json, s};
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const GET: &str = r#"{"type":"object","properties":{"path":{"type":"string","description":"点でつなぐ: host kernel mem disk proc service pkg net user boot log の下 (kernel.cpus、service.sshd、pkg.installed.cargo、proc.123)。なければぜんぶ"}}}"#;
const DO: &str = r#"{"type":"object","properties":{"op":{"type":"string","description":"service / pkg / power / apply (/etc/aios.json のとおりにそろえる) / rollback (ひとつ前の apply に戻す) / ping"},"action":{"type":"string","description":"service: start stop restart enable disable。pkg: install remove upgrade refresh。power: reboot poweroff"},"name":{"type":"string","description":"service の名前 (sshd など)"},"names":{"type":"array","items":{"type":"string"},"description":"pkg install / remove のパッケージ"}},"required":["op"]}"#;
const DIFF: &str = r#"{"type":"object","properties":{}}"#;
const TUNE: &str = r#"{"type":"object","properties":{"name":{"type":"string","description":"sysctl の名前 (kernel.sched_timeslice_ms など。aios get kernel.sysctl で一覧)"},"values":{"type":"array","items":{"type":"string"},"description":"くらべる値"},"cmd":{"type":"string","description":"測る仕事 (sh -c)。値ごとに n 回動かす"},"n":{"type":"integer","description":"値ごとに動かす回数 (既定 3)"},"apply":{"type":"boolean","description":"いちばん速かった値を /etc/aios.json の sysctl に書いて aios apply する"}},"required":["name","values","cmd"]}"#;
const NONE: &str = r#"{"type":"object","properties":{}}"#;
const PROCS: &str = r#"{"type":"object","properties":{"sort":{"type":"string","description":"cpu (既定。使った CPU の時間) か mem (メモリ)"},"limit":{"type":"integer","description":"いくつまで (既定 20)"},"name":{"type":"string","description":"名前にこれをふくむものだけ"}}}"#;
const KMSG: &str = r#"{"type":"object","properties":{"grep":{"type":"string","description":"この文字をふくむ行だけ"},"lines":{"type":"integer","description":"終わりから何行 (既定 50)"}}}"#;
const LOG: &str = r#"{"type":"object","properties":{"unit":{"type":"string","description":"サービスの名前 (sshd、aiwm.service など)。なければログのある一覧"},"grep":{"type":"string"},"lines":{"type":"integer","description":"終わりから何行 (既定 50)"}}}"#;
const RUN: &str = r#"{"type":"object","properties":{"cmd":{"type":"string","description":"動かすコマンド (sh -c)。なければ今の値を読むだけ (bkl)"},"timeout_ms":{"type":"integer","description":"これを過ぎたら止める (既定 50000)"}}}"#;
const THREADS: &str = r#"{"type":"object","properties":{"pid":{"type":"integer","description":"このプロセスのスレッドだけ"},"name":{"type":"string","description":"名前 (スレッドかプロセスの) にこれをふくむものだけ"},"state":{"type":"string","description":"sleep / run / ready / zombie"},"min_slept_s":{"type":"number","description":"これだけ眠りつづけているものだけ"},"limit":{"type":"integer","description":"いくつまで (既定 50)"}}}"#;
const HANG: &str = r#"{"type":"object","properties":{"pid":{"type":"integer","description":"見るプロセス (pid も name もなければ、みなからあやしいものを探す)"},"name":{"type":"string","description":"pid のかわりに名前 (プロセスの代表スレッドの名前にふくむもの。いくつもあればみな)"},"kick":{"type":"boolean","description":"futex で眠っているスレッドをみな起こして、同じところで眠りなおすかを見る (既定 true。わけもなく起こしても壊れない)"},"stack":{"type":"boolean","description":"長く (1 秒より) 眠っているスレッドの呼び出しの並び (stack ツールと同じ。長い順に 4 つまで)"}}}"#;
const STACK: &str = r#"{"type":"object","properties":{"tid":{"type":"integer","description":"スレッドの番号"},"file":{"type":"string","description":"tid のかわりに、残しておいた /proc/ai/stack の中身 (aiwatch の stack_file)"},"pid":{"type":"integer","description":"tid のかわりに: そのプロセスのスレッドみな (眠っているものの、長く眠っている順)"},"max":{"type":"integer","description":"何段まで (既定 40)"},"limit":{"type":"integer","description":"pid のときいくつのスレッドまで (既定 8)"}}}"#;
const WATCH: &str = r#"{"type":"object","properties":{"lines":{"type":"integer","description":"記録の終わりから何行 (既定 20)"}}}"#;
const FAIL: &str = r#"{"type":"object","properties":{"cmd":{"type":"string","description":"動かすコマンド (sh -c)。決まりをかけて動かし、終わったらやめる。なければ決まりをかけるだけ (off: true でやめる)"},"name":{"type":"string","description":"相手: この名前で始まるプロセス (既定: cmd の最初の語)"},"pid":{"type":"integer","description":"name のかわりに、このプロセス"},"what":{"type":"string","description":"sys (システムコールを -errno に。既定) / page (物理ページを取れないことに: プロセスが殺される) / heap (カーネルのヒープを取れないことに: いまはカーネルが panic する。-snapshot の VM で)"},"syscall":{"type":"string","description":"sys のとき: 名前 (openat、read ...) か番号か all (既定 all)"},"errno":{"type":"string","description":"sys のとき: ENOMEM、EIO、EAGAIN ... か番号 (既定 ENOMEM)"},"times":{"type":"integer","description":"何回しくじらせるか (既定 1)"},"off":{"type":"boolean","description":"決まりをやめる"},"timeout_ms":{"type":"integer"}}}"#;
const DELTA: &str = r#"{"type":"object","properties":{"cmd":{"type":"string","description":"動かすコマンド (sh -c)"},"timeout_ms":{"type":"integer"}},"required":["cmd"]}"#;
const PART: &str = r#"{"type":"object","properties":{"name":{"type":"string","description":"切れるところの名前 (kernel.json の off のあるもの: epoll inotify landlock module ns seccomp swap timerfd vdso tty-root-fdlink sound-min-period qemu-uart-burst。装置 (net、virtio_rng、bcm2835_rng) と aiproc は起動のときだけ: box で)"},"off":{"type":"boolean","description":"true で切る、false で入れる。なければ見るだけ"}}}"#;
const TRACE: &str = r#"{"type":"object","properties":{"cmd":{"type":"string","description":"動かすコマンド (sh -c)。記録を空にして始め、終わったら止めて読む。なければ、いま記録にあるものを読むだけ"},"name":{"type":"string","description":"この名前で始まるプロセスのシステムコールとフォルトだけ (既定: cmd の最初の語。切りかえ・眠り・起こし・割り込み・ディスクはみな)"},"all":{"type":"boolean","description":"名前で絞らない (みなのシステムコールも)"},"limit":{"type":"integer","description":"events に返す出来事の数 (終わりから。既定 200)"},"timeout_ms":{"type":"integer","description":"cmd をこれで止める (既定 50000)"}}}"#;
const MEM: &str = r#"{"type":"object","properties":{"files":{"type":"integer","description":"ページキャッシュを多く使っているファイルをいくつまで (既定 10)"},"procs":{"type":"integer","description":"メモリを多く使っているプロセスをいくつまで (既定 8)"}}}"#;
const STRACE: &str = r#"{"type":"object","properties":{"cmd":{"type":"string","description":"動かすコマンド (sh -c)"},"name":{"type":"string","description":"見るプロセスの名前 (既定: cmd の最初の語。子のプロセスを見るときに)"},"all":{"type":"boolean","description":"うまくいったものも (既定 true。false ならしくじったものだけ)"},"limit":{"type":"integer","description":"何行まで (既定 300)"},"timeout_ms":{"type":"integer"}},"required":["cmd"]}"#;

fn main() {
    let spec = Spec {
        name: "sys",
        hooks: &["key"],
        keys: &[("M-s", "sys")],
        tools: &[
            Tool { name: "get", desc: "aios の状態の木 (aios get --json と同じ)。host kernel mem disk proc service pkg net user boot log。path で一部だけ", input: GET },
            Tool { name: "do", desc: "aiosd (root) に頼んで aios を変える: サービスの start/stop/restart/enable/disable、パッケージの install/remove/upgrade/refresh、reboot/poweroff、apply / rollback (/etc/aios.json)。root と wheel の人だけ。したことは /var/log/aiosd.log に残る", input: DO },
            Tool { name: "diff", desc: "/etc/aios.json (望む状態) といまのちがいと、そろえる手順 (aios diff。動かさない)。そろえるのは do の apply", input: DIFF },
            Tool { name: "tune", desc: "sysctl の値を順に変えて cmd を n 回ずつ動かし、かかった時間のまんなか (ms) でくらべる (aios tune。root で。終わったら元の値に戻す)。apply でいちばん速かった値を /etc/aios.json に書いて apply。50 秒をこえそうなら run の bg で sudo aios tune NAME=V1,V2 -- CMD", input: TUNE },
            Tool { name: "sys", desc: "aios のまとめ: カーネル、起きてからの時間、CPU、メモリ、スワップ、ディスク、CPU を使っているプロセス、BKL、カーネルの新しいメッセージ", input: NONE },
            Tool { name: "procs", desc: "プロセスの一覧 (pid ppid 状態 スレッド CPU 秒 メモリ 名前)。sort: cpu / mem", input: PROCS },
            Tool { name: "kmsg", desc: "カーネルのメッセージ (dmesg。[起動からの秒] つき)。grep で絞れる", input: KMSG },
            Tool { name: "log", desc: "サービスのログ (/var/log/UNIT.log)。unit がなければログのある一覧", input: LOG },
            Tool { name: "bkl", desc: "cmd を動かして、そのあいだの大きなロック (BKL) の統計 (CPU ごとの待ち・持ち、長く持ったシステムコール)。cmd がなければ今の値", input: RUN },
            Tool { name: "threads", desc: "スレッドの一覧 (/proc/ai/threads): tid pid 名前 状態 最後のシステムコール 走った回数。眠っていれば slept_s (眠りつづけている秒) と wait: futex (アドレス、待つ値、いまの値) / poll (見張っている fd と、それが何か・読めるか・残っているバイト) / chan", input: THREADS },
            Tool { name: "hang", desc: "プロセスがなぜ止まっているかを調べる: 長く眠っているスレッドと待っているもの、読まれずに残っているデータのある fd、futex をみな起こしても同じところで眠りなおすか (起こしが消えたのか、だれも起こしていないのか)、見立て (hints)。pid も name もなければ、みなを起こさずに見てあやしいものだけ ({checked, suspects: [{pid, name, hints}]})", input: HANG },
            Tool { name: "stack", desc: "スレッドの呼び出しの並び (いちばん上から)。ファイルの .eh_frame でたどり、CFI のないところ (musl の libc) はスタックから戻り先を探す (how: pc / cfi / lr / scan)。名前は .symtab か .dynsym、名前のないもの (strip した libxul など) は、その関数のあたりで使っている文字列 (strings_near) を手がかりに", input: STACK },
            Tool { name: "watch", desc: "aiwatch (カーネルの見張りのサービス) がいま見つけているもの (active) と最近の記録 (log)。kind: lost_wakeup_poll / lost_wakeup_futex (起こしが消えた: カーネルのまちがいかも) / wake_storm (起こしすぎ) / bkl_busy (大きなロックの混雑) / resolved。stack_file はそのときのスタック (stack ツールの file で)", input: WATCH },
            Tool { name: "strace", desc: "cmd を動かして、そのプロセス (name) のシステムコールを返す (名前(引数 3 つ) = 答え)", input: STRACE },
            Tool { name: "mem", desc: "メモリの中 (/proc/ai/mem): RAM の空き、カーネルのヒープ (使っている、いちばん多かった、空きのかたまり、取れなかった回数)、ページキャッシュ (ページの数、ファイルごと、読みそこねた数、捨てた数)、MAP_SHARED のページ、tmpfs、スワップ、ページフォルト、メモリを多く使っているプロセス。text はまとめ、ほかはそのままの数 (ページは 4 KiB)。root か wheel の人だけ", input: MEM },
            Tool { name: "fail", desc: "わざとしくじらせる (フォルト注入。/proc/ai/ctl の fail): cmd のプロセス (name か pid) のシステムコール (syscall か all) を、動かさずに -errno (ENOMEM など) にする、またはカーネルの物理ページ (page: そのプロセスが out of memory で殺される) やヒープ (heap: いまのカーネルはそこで panic するので、-snapshot の VM の中だけで) を取れないことにして、プログラムとカーネルがどう耐えるかを見る。times 回しくじらせたら効かなくなる。hits は効いた数。root か wheel の人だけ", input: FAIL },
            Tool { name: "delta", desc: "cmd を動かした前と後の、カーネルの中の数のちがい (/proc/ai/mem、/proc/ai/fs、/proc/bkl): ページキャッシュ (ページ、読みそこね、捨てた)、ヒープ (使っている、いちばん多かった、取れなかった)、tmpfs、スワップ、ページフォルト、ext4 のブロックキャッシュ (当たり、外れ、まとめ読み、捨てた、書き出し)、パスの覚え (当たり、外れ)、開いているファイル、区画ごとのディスクの読み書き、大きなロック。そのコマンドがカーネルに何をさせたかがわかる。root か wheel の人だけ", input: DELTA },
            Tool { name: "part", desc: "カーネルの切れるところ (kernel.json の extra と quirk で off のあるもの) のいまの様子 (/proc/ai/parts: 切ってあるか、動かしたまま変えられるか)。name と off で、動かしたまま切る (システムコールは ENOSYS、quirk は手当てをしない) か入れる: 入れずに作った Linux のようにしてプログラムを動かし、戻せる。装置と aiproc は起動のときだけ (vm start の off か box)。root か wheel の人だけ", input: PART },
            Tool { name: "trace", desc: "カーネルの中の出来事 (/proc/ai/trace): cmd を動かしているあいだの、システムコールの入りと出 (かかった時間)、スレッドの切りかえ、眠りと起こし (待った時間、起こされてから走るまで)、割り込み、ディスクの読み書き、ページフォルトを記録して、スレッドごと、システムコールごと (回数、合計、最大)、遅かった順、ディスク、割り込みにまとめる (text)。events は終わりの limit こ (t は起動からの ns、c は CPU、p は tid、k は sys/ret/switch/sleep/wake/irq/blk/blkdone/fault/exec/exit)。cmd がなければ、いま記録にあるもの (ctl の trace on で始めたもの)。root か wheel の人だけ", input: TRACE },
            Tool { name: "fs", desc: "ファイルシステムの中 (/proc/ai/fs): マウントごとの様子 (ext4 はブロックキャッシュの数と当たり・外れ・捨てた数、まだ書いていないブロック、書き出した回数、ジャーナル、inode と名前の覚え、空き。tmpfs はページ)、パスの覚え (数、当たり・外れ、捨てた回数)、開いているファイルの数、区画ごとのディスクの読み書き。text はまとめ、ほかはそのままの数。root か wheel の人だけ", input: NONE },
        ],
    };
    aish_plugin::run(spec, |ev, v| match ev {
        "tool" => {
            let a = &v["args"];
            let pwd = s(v, "pwd");
            match s(v, "name") {
                "get" => get(a),
                "do" => do_(a),
                "diff" => match Command::new("aios").arg("diff").output() {
                    Ok(o) => json!({ "status": o.status.code().unwrap_or(-1), "text": String::from_utf8_lossy(&o.stdout), "err": String::from_utf8_lossy(&o.stderr) }),
                    Err(e) => error(format!("aios: {}", e)),
                },
                "tune" => tune(a),
                "sys" => summary(),
                "procs" => procs(a),
                "kmsg" => kmsg(a),
                "log" => log(a),
                "bkl" => bkl(a, pwd),
                "strace" => strace(a, pwd),
                "mem" => mem(a),
                "fs" => fs(),
                "trace" => trace(a, pwd),
                "part" => part(a),
                "fail" => fail(a, pwd),
                "delta" => delta(a, pwd),
                "threads" => threads(a),
                "hang" => hang(a),
                "stack" => stack(a),
                "watch" => watch(a),
                n => error(format!("{}: no such tool", n)),
            }
        }
        "key" => key(),
        _ => json!({}),
    });
}

fn read(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// aios のカーネルか (/proc/bkl は aios だけ)。Linux の /proc/kmsg は読むと待ちつづけるので、aios でなければ読まない
fn aios() -> bool {
    Path::new("/proc/bkl").exists()
}

/// カーネルのメッセージ (aios の /proc/kmsg)
fn kmsg_text() -> Option<String> {
    if aios() { read("/proc/kmsg") } else { None }
}

/// 終わりから n 行 (pat があればそれをふくむ行だけ)
fn tail(text: &str, pat: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().filter(|l| pat.is_empty() || l.contains(pat)).collect();
    let from = lines.len().saturating_sub(n);
    let mut s = lines[from..].join("\n");
    s.push('\n');
    s
}

fn kib(kb: u64) -> String {
    if kb >= 1024 * 1024 {
        format!("{:.1} GiB", kb as f64 / 1024.0 / 1024.0)
    } else {
        format!("{} MiB", kb / 1024)
    }
}

// ---- 状態の木 (aios get) ----

/// tune: aios tune NAME=V1,V2 [-n N] [--apply] --json -- CMD。root でなければ sudo -n で
fn tune(a: &Value) -> Value {
    let vals: Vec<&str> = a["values"].as_array().map(|v| v.iter().filter_map(|x| x.as_str()).collect()).unwrap_or_default();
    let (name, cmd) = (s(a, "name"), s(a, "cmd"));
    if name.is_empty() || vals.is_empty() || cmd.is_empty() {
        return error("give name, values and cmd");
    }
    let mut args = vec!["aios".to_string(), "tune".into(), format!("{}={}", name, vals.join(",")), "-n".into(), a["n"].as_u64().unwrap_or(3).to_string(), "--json".into()];
    if a["apply"] == true {
        args.push("--apply".into());
    }
    args.push("--".into());
    args.push(cmd.to_string());
    let root = unsafe { libc::geteuid() } == 0;
    let mut c = if root { Command::new(&args[0]) } else { Command::new("sudo") };
    if root {
        c.args(&args[1..]);
    } else {
        c.arg("-n").args(&args);
    }
    match c.stdin(Stdio::null()).output() {
        Ok(o) => {
            let out = String::from_utf8_lossy(&o.stdout);
            let mut r = out.lines().find_map(|l| serde_json::from_str::<Value>(l).ok().filter(|v| v.is_object())).unwrap_or_else(|| json!({}));
            if !o.status.success() {
                r["error"] = json!(String::from_utf8_lossy(&o.stderr).trim().to_string());
            }
            // --apply のときは apply の結果の行も
            let rest: Vec<&str> = out.lines().filter(|l| !l.starts_with('{')).collect();
            if !rest.is_empty() {
                r["text"] = json!(rest.join("\n"));
            }
            r
        }
        Err(e) => error(format!("aios: {}", e)),
    }
}

fn get(a: &Value) -> Value {
    let path = a["path"].as_str().unwrap_or("");
    let mut c = Command::new("aios");
    c.arg("get");
    if !path.is_empty() {
        c.arg(path);
    }
    match c.arg("--json").output() {
        Ok(o) if o.status.success() => match serde_json::from_slice::<Value>(&o.stdout) {
            Ok(v) if v.is_object() => v,
            Ok(v) => json!({ "value": v }),
            Err(e) => error(format!("aios get: {}", e)),
        },
        Ok(o) => error(String::from_utf8_lossy(&o.stderr).trim().to_string()),
        Err(e) => error(format!("aios: {} (aios の base パッケージのコマンド)", e)),
    }
}

/// aiosd に 1 つ頼む (/run/aiosd.sock に 1 行の JSON)
fn do_(a: &Value) -> Value {
    use std::io::BufRead;
    let mut c = match std::os::unix::net::UnixStream::connect("/run/aiosd.sock") {
        Ok(c) => c,
        Err(e) => return error(format!("/run/aiosd.sock: {} (aiosd is not running: sudo systemctl enable --now aiosd)", e)),
    };
    let mut line = String::new();
    if writeln!(c, "{}", a).is_err() || std::io::BufReader::new(&c).read_line(&mut line).is_err() {
        return error("aiosd did not answer");
    }
    serde_json::from_str(&line).unwrap_or_else(|_| error("aiosd: bad answer"))
}

// ---- まとめ ----

fn meminfo() -> std::collections::HashMap<String, u64> {
    read("/proc/meminfo")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            Some((k.to_string(), v.split_whitespace().next()?.parse().ok()?))
        })
        .collect()
}

fn disk(path: &str) -> Option<(u64, u64)> {
    let c = std::ffi::CString::new(path).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let bs = st.f_frsize as u64;
    Some(((st.f_blocks - st.f_bfree) as u64 * bs / 1024, st.f_blocks as u64 * bs / 1024))
}

fn uname_r() -> String {
    let mut u: libc::utsname = unsafe { std::mem::zeroed() };
    if unsafe { libc::uname(&mut u) } != 0 {
        return "?".into();
    }
    let f = |a: &[libc::c_char]| unsafe { std::ffi::CStr::from_ptr(a.as_ptr()) }.to_string_lossy().into_owned();
    format!("{} {} ({})", f(&u.sysname), f(&u.release), f(&u.machine))
}

fn summary() -> Value {
    let mut t = String::new();
    t.push_str(&format!("kernel   {}\n", uname_r()));
    if let Some(up) = read("/proc/uptime").and_then(|u| u.split_whitespace().next()?.parse::<f64>().ok()) {
        let s = up as u64;
        t.push_str(&format!("uptime   {}h {}m {}s\n", s / 3600, s / 60 % 60, s % 60));
    }
    let cpus = read("/proc/cpuinfo").map_or(0, |c| c.lines().filter(|l| l.starts_with("processor")).count());
    t.push_str(&format!("cpu      {}\n", cpus));
    let m = meminfo();
    let g = |k: &str| m.get(k).copied().unwrap_or(0);
    let avail = if m.contains_key("MemAvailable") { g("MemAvailable") } else { g("MemFree") };
    t.push_str(&format!("memory   {} / {} used\n", kib(g("MemTotal").saturating_sub(avail)), kib(g("MemTotal"))));
    t.push_str(&format!("swap     {} / {} used\n", kib(g("SwapTotal").saturating_sub(g("SwapFree"))), kib(g("SwapTotal"))));
    if let Some((used, total)) = disk("/") {
        t.push_str(&format!("disk /   {} / {} used ({}%)\n", kib(used), kib(total), used * 100 / total.max(1)));
    }
    let ps = list_procs();
    t.push_str(&format!("procs    {}\n", ps.len()));
    let mut top: Vec<&P> = ps.iter().collect();
    top.sort_by_key(|p| std::cmp::Reverse(p.ticks));
    for p in top.iter().take(5) {
        t.push_str(&format!("  {:>6} {:>8.1}s {:>8} {}\n", p.pid, p.ticks as f64 / 100.0, kib(p.rss_kb), p.name));
    }
    if let Some(b) = read("/proc/bkl")
        && let Some(all) = b.lines().find(|l| l.starts_with("all"))
    {
        t.push_str(&format!("bkl      {}\n", all.trim_start_matches("all").trim()));
    }
    // カーネルの中のメモリ (root か wheel なら): ページキャッシュ、ヒープ、パスの覚え (くわしくは mem と fs)
    if let Ok(m) = ai_json("/proc/ai/mem") {
        let pg = m["page"].as_u64().unwrap_or(4096);
        let h = &m["kernel"]["heap"];
        t.push_str(&format!(
            "cache    page cache {} ({} pages), tmpfs {}, shared {}; heap {} / {} (peak {}, fails {})\n",
            mib(m["page_cache"]["pages"].as_u64().unwrap_or(0) * pg),
            m["page_cache"]["pages"].as_u64().unwrap_or(0),
            mib(m["tmpfs"]["pages"].as_u64().unwrap_or(0) * pg),
            mib(m["shared"]["pages"].as_u64().unwrap_or(0) * pg),
            mib(h["used"].as_u64().unwrap_or(0)),
            mib(h["size"].as_u64().unwrap_or(0)),
            mib(h["peak"].as_u64().unwrap_or(0)),
            h["fails"].as_u64().unwrap_or(0)
        ));
    }
    if let Ok(f) = ai_json("/proc/ai/fs") {
        let p = &f["paths"];
        let (hit, miss) = (p["hits"].as_u64().unwrap_or(0), p["misses"].as_u64().unwrap_or(0));
        t.push_str(&format!("paths    {} cached ({} negative), hit {}%, {} open files\n", p["entries"], p["negative"], hit * 100 / (hit + miss).max(1), f["files"]["open"]));
    }
    if let Some(k) = kmsg_text() {
        t.push_str("kmsg (終わりの 5 行)\n");
        for l in tail(&k, "", 5).lines() {
            t.push_str(&format!("  {}\n", l));
        }
    }
    json!({ "text": t })
}

/// バイトを MiB (小数 1 けた) か KiB で
fn mib(b: u64) -> String {
    if b >= 1 << 20 { format!("{:.1} MiB", b as f64 / (1u64 << 20) as f64) } else { format!("{} KiB", b / 1024) }
}

/// /proc/ai の 1 行の JSON (root か wheel で読む)
fn ai_json(path: &str) -> Result<Value, String> {
    if !Path::new(path).exists() {
        return Err(format!("no {} (not aios, or an older kernel)", path));
    }
    let text = read_root(path)?;
    serde_json::from_str(text.trim()).map_err(|e| format!("{}: {}", path, e))
}

fn pct(a: u64, b: u64) -> u64 {
    a * 100 / (a + b).max(1)
}

/// mem: /proc/ai/mem のまとめ
fn mem(a: &Value) -> Value {
    let mut m = match ai_json("/proc/ai/mem") {
        Ok(m) => m,
        Err(e) => return error(e),
    };
    let nfiles = a["files"].as_u64().unwrap_or(10) as usize;
    let nprocs = a["procs"].as_u64().unwrap_or(8) as usize;
    let t = mem_text(&m, nfiles, nprocs);
    if let Some(files) = m["page_cache"]["files"].as_array_mut() {
        files.truncate(nfiles);
    }
    if let Some(ps) = m["procs"].as_array_mut() {
        ps.truncate(nprocs);
    }
    m["text"] = json!(t);
    m
}

/// /proc/ai/mem の JSON を、人 (と AI) が読む形に
fn mem_text(m: &Value, nfiles: usize, nprocs: usize) -> String {
    let pg = m["page"].as_u64().unwrap_or(4096);
    let u = |v: &Value| v.as_u64().unwrap_or(0);
    let mut t = String::new();
    let r = &m["ram"];
    t.push_str(&format!("ram        {} total, {} free (reclaim below {})\n", mib(u(&r["total"]) * pg), mib(u(&r["free"]) * pg), mib(u(&r["min_free"]) * pg)));
    let h = &m["kernel"]["heap"];
    t.push_str(&format!(
        "kernel     heap {} / {} used (peak {}, free {} in {} blocks, largest {}, fails {}), stacks {}\n",
        mib(u(&h["used"])), mib(u(&h["size"])), mib(u(&h["peak"])), mib(u(&h["free"])), u(&h["free_blocks"]), mib(u(&h["largest_free"])), u(&h["fails"]), mib(u(&m["kernel"]["stacks"]))
    ));
    let c = &m["page_cache"];
    t.push_str(&format!(
        "page cache {} ({} pages, max {}), misses {} ({} ms), shrunk {}, changed {}\n",
        mib(u(&c["pages"]) * pg), u(&c["pages"]), u(&c["max"]), u(&c["misses"]), u(&c["miss_ns"]) / 1_000_000, u(&c["shrunk"]), u(&c["changes"])
    ));
    if let Some(files) = c["files"].as_array() {
        for f in files.iter().take(nfiles) {
            let name = f["name"].as_str().map_or_else(|| format!("fs{}:ino{}", u(&f["fs"]), u(&f["ino"])), |n| n.to_string());
            t.push_str(&format!("  {:>9} {:>10}  {}\n", mib(u(&f["pages"]) * pg), mib(u(&f["size"])), name));
        }
    }
    t.push_str(&format!("shared     {} ({} dirty)\n", mib(u(&m["shared"]["pages"]) * pg), u(&m["shared"]["dirty"])));
    t.push_str(&format!("tmpfs      {} / {} limit\n", mib(u(&m["tmpfs"]["pages"]) * pg), mib(u(&m["tmpfs"]["limit"]) * pg)));
    let sw = &m["swap"];
    t.push_str(&format!("swap       {} / {} used (in {} out {} pages)\n", mib(u(&sw["used"]) * pg), mib(u(&sw["total"]) * pg), u(&sw["in"]), u(&sw["out"])));
    t.push_str(&format!("faults     {} ({} without the big lock)\n", u(&m["faults"]["all"]), u(&m["faults"]["fast"])));
    if let Some(ps) = m["procs"].as_array() {
        t.push_str("procs      rss / vsize\n");
        for p in ps.iter().take(nprocs) {
            t.push_str(&format!("  {:>6} {:>10} {:>10}  {}\n", u(&p["pid"]), mib(u(&p["rss"]) * pg), mib(u(&p["vsize"])), s(p, "name")));
        }
    }
    t
}

/// fs: /proc/ai/fs のまとめ
fn fs() -> Value {
    let mut f = match ai_json("/proc/ai/fs") {
        Ok(f) => f,
        Err(e) => return error(e),
    };
    f["text"] = json!(fs_text(&f));
    f
}

/// /proc/ai/fs の JSON を、人 (と AI) が読む形に
fn fs_text(f: &Value) -> String {
    let u = |v: &Value| v.as_u64().unwrap_or(0);
    let mut t = String::from("mounts\n");
    if let Some(ms) = f["mounts"].as_array() {
        for m in ms {
            t.push_str(&format!("  {:<12} {:<6} {:<12} {}", s(m, "target"), s(m, "type"), s(m, "source"), if m["ro"].as_bool().unwrap_or(false) { "ro" } else { "rw" }));
            let st = &m["stats"];
            if st.is_object() {
                if s(m, "type") == "tmpfs" {
                    t.push_str(&format!("  {} pages / {} limit", u(&st["pages"]), u(&st["limit"])));
                } else {
                    let c = &st["cache"];
                    let bs = u(&st["block_size"]).max(1);
                    t.push_str(&format!(
                        "\n    cache {} / {} blocks, dirty meta {} data {}, hit {}% ({} / {}), readahead {}, evicted {}, flushed {} (meta {} data {} blocks, last {} s ago), commits {}",
                        u(&c["blocks"]), u(&c["limit"]), u(&c["dirty_meta"]), u(&c["dirty_data"]), pct(u(&c["hits"]), u(&c["misses"])), u(&c["hits"]), u(&c["misses"]),
                        u(&c["readahead"]), u(&c["evictions"]), u(&c["flushes"]), u(&c["written_meta"]), u(&c["written_data"]), u(&c["last_flush_s"]), u(&c["commits"])
                    ));
                    if st["journal"].is_object() {
                        t.push_str(&format!("\n    journal {} blocks, next transaction {}", u(&st["journal"]["capacity"]), u(&st["journal"]["seq"])));
                    }
                    t.push_str(&format!(
                        "\n    inode cache {} / {}, name cache {} / {}, inodes in use {}, orphans {}; free {} of {} ({} inodes free)",
                        u(&st["metas"]["entries"]), u(&st["metas"]["max"]), u(&st["names"]["entries"]), u(&st["names"]["max"]), u(&st["inodes_in_use"]), u(&st["orphans"]),
                        mib(u(&st["blocks"]["free"]) * bs), mib(u(&st["blocks"]["total"]) * bs), u(&st["inodes"]["free"])
                    ));
                }
            }
            t.push('\n');
        }
    }
    let p = &f["paths"];
    t.push_str(&format!(
        "paths      {} cached ({} negative, max {}), hit {}% ({} / {}), swept {} times, invalidated {}\n",
        u(&p["entries"]), u(&p["negative"]), u(&p["max"]), pct(u(&p["hits"]), u(&p["misses"])), u(&p["hits"]), u(&p["misses"]), u(&p["sweeps"]), u(&p["invalidations"])
    ));
    t.push_str(&format!("files      {} open in {} processes\n", u(&f["files"]["open"]), u(&f["files"]["procs"])));
    if let Some(ds) = f["disk"].as_array() {
        for d in ds {
            t.push_str(&format!(
                "disk       {:<12} {} reads ({}, {} ms), {} writes ({}, {} ms)\n",
                s(d, "part"), u(&d["reads"]), mib(u(&d["read_sectors"]) * 512), u(&d["read_ms"]), u(&d["writes"]), mib(u(&d["write_sectors"]) * 512), u(&d["write_ms"])
            ));
        }
    }
    t
}

// ---- fail: わざとしくじらせる (/proc/ai/fail、ctl の fail) ----

/// errno の名前 → 番号 (Linux の asm-generic)
fn errno_of(s: &str) -> Option<i64> {
    if let Ok(n) = s.parse::<i64>() {
        return Some(n);
    }
    let t = [
        ("EPERM", 1), ("ENOENT", 2), ("EIO", 5), ("ENXIO", 6), ("EBADF", 9), ("EAGAIN", 11), ("ENOMEM", 12), ("EACCES", 13), ("EFAULT", 14),
        ("EBUSY", 16), ("EEXIST", 17), ("ENODEV", 19), ("ENOTDIR", 20), ("EISDIR", 21), ("EINVAL", 22), ("ENFILE", 23), ("EMFILE", 24),
        ("EFBIG", 27), ("ENOSPC", 28), ("ESPIPE", 29), ("EROFS", 30), ("EPIPE", 32), ("ENOSYS", 38), ("ELOOP", 40), ("EOVERFLOW", 75),
        ("EOPNOTSUPP", 95), ("EADDRINUSE", 98), ("ENETUNREACH", 101), ("ECONNRESET", 104), ("ETIMEDOUT", 110), ("ECONNREFUSED", 111),
        ("EHOSTUNREACH", 113), ("EINTR", 4),
    ];
    t.iter().find(|(n, _)| n.eq_ignore_ascii_case(s)).map(|(_, v)| *v)
}

fn fail(a: &Value, pwd: &str) -> Value {
    if !Path::new("/proc/ai/fail").exists() {
        return error("no /proc/ai/fail (not aios, or an older kernel)");
    }
    if a["off"].as_bool().unwrap_or(false) {
        return match write_root("/proc/ai/ctl", "fail off") {
            Ok(()) => json!({ "off": true }),
            Err(e) => error(e),
        };
    }
    let cmd = s(a, "cmd");
    let target = match (a["pid"].as_u64(), s(a, "name")) {
        (Some(p), _) => format!("pid {}", p),
        (None, n) if !n.is_empty() => format!("name {}", n),
        (None, _) if !cmd.is_empty() => format!("name {}", Path::new(cmd.split_whitespace().next().unwrap_or("")).file_name().map_or(String::new(), |n| n.to_string_lossy().into_owned())),
        _ => return error("name, pid or cmd is needed"),
    };
    let what = if s(a, "what").is_empty() { "sys" } else { s(a, "what") };
    let times = a["times"].as_u64().unwrap_or(1);
    let rule = match what {
        "sys" => {
            let sc = if s(a, "syscall").is_empty() { "all" } else { s(a, "syscall") };
            let Some(e) = errno_of(if s(a, "errno").is_empty() { "ENOMEM" } else { s(a, "errno") }) else { return error("errno: a name (ENOMEM, EIO ...) or a number") };
            format!("fail {} sys {} {} {}", target, sc, e, times)
        }
        "heap" | "page" => format!("fail {} {} {}", target, what, times),
        w => return error(format!("what: sys / heap / page, not {}", w)),
    };
    if let Err(e) = write_root("/proc/ai/ctl", &rule) {
        return error(format!("{} ({})", e, rule));
    }
    if cmd.is_empty() {
        return json!({ "rule": rule, "state": ai_json("/proc/ai/fail").unwrap_or(Value::Null) });
    }
    let r = run(cmd, pwd, timeout_of(a));
    let state = ai_json("/proc/ai/fail").unwrap_or(Value::Null);
    let _ = write_root("/proc/ai/ctl", "fail off");
    match r {
        Ok((st, out, err, ms, timeout)) => json!({ "rule": rule, "hits": state["rule"]["hits"], "status": st, "out": tail(&out, "", 40), "err": tail(&err, "", 40), "ms": ms, "timeout": timeout }),
        Err(e) => error(e),
    }
}

// ---- delta: cmd の前と後の、カーネルの中の数のちがい ----

fn delta(a: &Value, pwd: &str) -> Value {
    let snap = || -> Result<(Value, Value, String), String> {
        let m = ai_json("/proc/ai/mem")?;
        let f = ai_json("/proc/ai/fs")?;
        let b = read("/proc/bkl").and_then(|b| b.lines().find(|l| l.starts_with("all")).map(|l| l.trim_start_matches("all").trim().to_string())).unwrap_or_default();
        Ok((m, f, b))
    };
    let before = match snap() {
        Ok(x) => x,
        Err(e) => return error(e),
    };
    let r = match run(s(a, "cmd"), pwd, timeout_of(a)) {
        Ok(r) => r,
        Err(e) => return error(e),
    };
    let after = match snap() {
        Ok(x) => x,
        Err(e) => return error(e),
    };
    let text = delta_text(&before.0, &before.1, &after.0, &after.1);
    json!({ "text": text, "bkl": { "before": before.2, "after": after.2 }, "ran": { "status": r.0, "ms": r.3, "timeout": r.4, "out": tail(&r.1, "", 10), "err": tail(&r.2, "", 10) } })
}

/// 2 つの様子 (mem と fs) の、変わった数だけを「名前 before → after (+差)」で
fn delta_text(m0: &Value, f0: &Value, m1: &Value, f1: &Value) -> String {
    let mut t = String::new();
    let mut row = |name: &str, a: &Value, b: &Value| {
        if let (Some(x), Some(y)) = (a.as_u64(), b.as_u64())
            && x != y
        {
            t.push_str(&format!("{:<28} {:>12} → {:<12} ({:+})\n", name, x, y, y as i64 - x as i64));
        }
    };
    for (name, path) in [
        ("page cache pages", vec!["page_cache", "pages"]),
        ("page cache misses", vec!["page_cache", "misses"]),
        ("page cache shrunk", vec!["page_cache", "shrunk"]),
        ("page cache changed", vec!["page_cache", "changes"]),
        ("heap used", vec!["kernel", "heap", "used"]),
        ("heap peak", vec!["kernel", "heap", "peak"]),
        ("heap fails", vec!["kernel", "heap", "fails"]),
        ("tmpfs pages", vec!["tmpfs", "pages"]),
        ("shared dirty", vec!["shared", "dirty"]),
        ("swap used", vec!["swap", "used"]),
        ("faults", vec!["faults", "all"]),
        ("ram free", vec!["ram", "free"]),
    ] {
        let (mut a, mut b) = (m0, m1);
        for k in &path {
            a = &a[*k];
            b = &b[*k];
        }
        row(name, a, b);
    }
    for (name, path) in [("paths hits", "hits"), ("paths misses", "misses"), ("paths sweeps", "sweeps"), ("paths invalidations", "invalidations"), ("paths entries", "entries")] {
        row(name, &f0["paths"][path], &f1["paths"][path]);
    }
    row("open files", &f0["files"]["open"], &f1["files"]["open"]);
    if let (Some(a), Some(b)) = (f0["mounts"].as_array(), f1["mounts"].as_array()) {
        for (x, y) in a.iter().zip(b) {
            if x["stats"]["cache"].is_object() {
                for k in ["blocks", "dirty_meta", "dirty_data", "hits", "misses", "readahead", "evictions", "flushes", "written_meta", "written_data", "commits"] {
                    row(&format!("{} cache {}", s(x, "target"), k), &x["stats"]["cache"][k], &y["stats"]["cache"][k]);
                }
            }
        }
    }
    if let (Some(a), Some(b)) = (f0["disk"].as_array(), f1["disk"].as_array()) {
        for (x, y) in a.iter().zip(b) {
            for k in ["reads", "read_sectors", "read_ms", "writes", "write_sectors", "write_ms"] {
                row(&format!("{} {}", s(x, "part"), k), &x[k], &y[k]);
            }
        }
    }
    if t.is_empty() {
        t.push_str("(nothing changed)\n");
    }
    t
}

// ---- part: 切れるところを動かしたまま切る・入れる (/proc/ai/parts、ctl の part) ----

fn part(a: &Value) -> Value {
    if !Path::new("/proc/ai/parts").exists() {
        return error("no /proc/ai/parts (not aios, or an older kernel)");
    }
    let name = s(a, "name");
    let mut changed = Value::Null;
    if let Some(off) = a["off"].as_bool() {
        if name.is_empty() {
            return error("name is needed with off");
        }
        if let Err(e) = write_root("/proc/ai/ctl", &format!("part {} {}", name, if off { "off" } else { "on" })) {
            return error(format!("{} (a device or aiproc cannot be changed while running; see /proc/kmsg)", e));
        }
        changed = json!({ "name": name, "off": off });
    }
    let p = match read("/proc/ai/parts").ok_or_else(|| "cannot read /proc/ai/parts".to_string()).and_then(|t| serde_json::from_str::<Value>(t.trim()).map_err(|e| e.to_string())) {
        Ok(p) => p,
        Err(e) => return error(e),
    };
    let list: Vec<&Value> = p["parts"].as_array().map(|v| v.iter().filter(|x| name.is_empty() || s(x, "name") == name).collect()).unwrap_or_default();
    let mut t = String::new();
    for x in &list {
        t.push_str(&format!("{:<18} {}{}\n", s(x, "name"), if x["off"].as_bool().unwrap_or(false) { "off" } else { "on " }, if x["runtime"].as_bool().unwrap_or(false) { "" } else { "  (boot only)" }));
    }
    json!({ "text": t, "parts": list, "changed": changed })
}

// ---- trace: カーネルの中の出来事 (/proc/ai/trace) ----

/// trace: cmd があれば、記録を空にして (trace clear)、name (既定は cmd の最初の語。all なら絞らない) で記録を始め
/// (trace on)、cmd を動かし、止めて (trace off) 読む。なければ、いま記録にあるものを読むだけ
fn trace(a: &Value, pwd: &str) -> Value {
    if !Path::new("/proc/ai/trace").exists() {
        return error("no /proc/ai/trace (not aios, or an older kernel)");
    }
    let cmd = s(a, "cmd");
    let mut ran = Value::Null;
    if !cmd.is_empty() {
        let name = if a["all"].as_bool().unwrap_or(false) {
            String::new()
        } else if !s(a, "name").is_empty() {
            s(a, "name").to_string()
        } else {
            Path::new(cmd.split_whitespace().next().unwrap_or("")).file_name().map_or(String::new(), |n| n.to_string_lossy().into_owned())
        };
        for c in ["trace clear", &format!("trace on {}", name)] {
            if let Err(e) = write_root("/proc/ai/ctl", c) {
                return error(e);
            }
        }
        let r = run(cmd, pwd, timeout_of(a));
        let _ = write_root("/proc/ai/ctl", "trace off");
        match r {
            Ok((st, out, err, ms, timeout)) => ran = json!({ "status": st, "out": tail(&out, "", 20), "err": tail(&err, "", 20), "ms": ms, "timeout": timeout }),
            Err(e) => return error(e),
        }
    }
    let text = match read_root("/proc/ai/trace") {
        Ok(t) => t,
        Err(e) => return error(e),
    };
    let limit = a["limit"].as_u64().unwrap_or(200) as usize;
    let (summary, head, events) = trace_summary(&text, limit);
    json!({ "text": summary, "ran": ran, "trace": head, "events": events })
}

/// /proc/ai/trace の中身から、まとめ (text) と、はじめの行と、終わりの limit この出来事
fn trace_summary(text: &str, limit: usize) -> (String, Value, Vec<Value>) {
    use std::collections::BTreeMap;
    let mut lines = text.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok());
    let head = lines.next().unwrap_or(Value::Null);
    let evs: Vec<Value> = lines.collect();
    let u = |v: &Value| v.as_u64().unwrap_or(0);
    let name_of = |tid: u64| head["names"][tid.to_string()].as_str().map_or(format!("?{}", tid), |n| n.to_string());
    // スレッドごと: システムコールの数と時間 (入りから出まで)、眠った数と待った時間、起こされてから走るまで、フォルト
    #[derive(Default)]
    struct T {
        sys: u64,
        sys_ns: u64,
        sleeps: u64,
        waited_ns: u64,
        woken: u64,
        delay_ns: u64,
        faults: u64,
        runs: u64,
    }
    let mut per: BTreeMap<u64, T> = BTreeMap::new();
    let mut pending: BTreeMap<u64, (String, u64)> = BTreeMap::new();
    let mut asleep: BTreeMap<u64, u64> = BTreeMap::new();
    let mut woken_at: BTreeMap<u64, u64> = BTreeMap::new();
    let mut by_sys: BTreeMap<String, (u64, u64, u64)> = BTreeMap::new();
    let mut slow: Vec<(u64, u64, String, u64)> = Vec::new();
    let mut irqs: BTreeMap<u64, u64> = BTreeMap::new();
    let (mut blk_r, mut blk_w, mut blk_sectors, mut blk_ns, mut blk_pending) = (0u64, 0u64, 0u64, 0u64, 0u64);
    let (mut switches, mut t_first, mut t_last) = (0u64, u64::MAX, 0u64);
    for e in &evs {
        let (t, p) = (u(&e["t"]), u(&e["p"]));
        t_first = t_first.min(t);
        t_last = t_last.max(t);
        let k = s(e, "k");
        match k {
            "sys" => {
                pending.insert(p, (s(e, "n").to_string(), t));
            }
            "ret" => {
                let th = per.entry(p).or_default();
                th.sys += 1;
                if let Some((n, t0)) = pending.remove(&p) {
                    let d = t.saturating_sub(t0);
                    th.sys_ns += d;
                    let b = by_sys.entry(n.clone()).or_insert((0, 0, 0));
                    b.0 += 1;
                    b.1 += d;
                    b.2 = b.2.max(d);
                    slow.push((d, p, n, t0));
                }
            }
            "sleep" => {
                per.entry(p).or_default().sleeps += 1;
                asleep.insert(p, t);
            }
            "wake" => {
                if let Some(t0) = asleep.remove(&p) {
                    let th = per.entry(p).or_default();
                    th.woken += 1;
                    th.waited_ns += t.saturating_sub(t0);
                }
                woken_at.insert(p, t);
            }
            "switch" => {
                switches += 1;
                let th = per.entry(p).or_default();
                th.runs += 1;
                if let Some(t0) = woken_at.remove(&p) {
                    th.delay_ns += t.saturating_sub(t0);
                }
            }
            "irq" => *irqs.entry(u(&e["irq"])).or_insert(0) += 1,
            "blk" => {
                if s(e, "op") == "write" { blk_w += 1 } else { blk_r += 1 }
                blk_sectors += u(&e["n"]);
                blk_pending += 1;
            }
            "blkdone" => {
                blk_ns += u(&e["ns"]);
                blk_pending = blk_pending.saturating_sub(1);
            }
            "fault" => per.entry(p).or_default().faults += 1,
            _ => {}
        }
    }
    let ms = |ns: u64| format!("{:.2} ms", ns as f64 / 1e6);
    let mut t = String::new();
    t.push_str(&format!(
        "trace      {} events ({} dropped), {} over {} ms, filter {:?}, {} switches\n",
        evs.len(),
        u(&head["dropped"]),
        if head["on"].as_bool().unwrap_or(false) { "still on" } else { "stopped" },
        if t_last > t_first { (t_last - t_first) / 1_000_000 } else { 0 },
        s(&head, "filter"),
        switches
    ));
    let mut ths: Vec<(&u64, &T)> = per.iter().collect();
    ths.sort_by_key(|(_, th)| std::cmp::Reverse(th.sys_ns + th.waited_ns));
    t.push_str("threads    tid name: syscalls (time), slept (waited, run delay), faults, runs\n");
    for (tid, th) in ths.iter().take(12) {
        t.push_str(&format!(
            "  {:>6} {:<16} {} ({}), {} ({}, {}), {} faults, {} runs\n",
            tid, name_of(**tid), th.sys, ms(th.sys_ns), th.sleeps, ms(th.waited_ns), ms(th.delay_ns), th.faults, th.runs
        ));
    }
    let mut bs: Vec<(&String, &(u64, u64, u64))> = by_sys.iter().collect();
    bs.sort_by_key(|(_, (_, total, _))| std::cmp::Reverse(*total));
    if !bs.is_empty() {
        t.push_str("syscalls   name: count, total, max\n");
        for (n, (c, total, max)) in bs.iter().take(10) {
            t.push_str(&format!("  {:<16} {:>6}  {:>12}  {:>12}\n", n, c, ms(*total), ms(*max)));
        }
    }
    slow.sort_by_key(|x| std::cmp::Reverse(x.0));
    if !slow.is_empty() {
        t.push_str("slowest    time, tid name, syscall, at (ms from the first event)\n");
        for (d, p, n, t0) in slow.iter().take(8) {
            t.push_str(&format!("  {:>12}  {:>6} {:<16} {:<16} {}\n", ms(*d), p, name_of(*p), n, ms(t0.saturating_sub(t_first))));
        }
    }
    if blk_r + blk_w > 0 {
        t.push_str(&format!("disk       {} reads, {} writes, {} ({} in the device, {} not finished)\n", blk_r, blk_w, mib(blk_sectors * 512), ms(blk_ns), blk_pending));
    }
    if !irqs.is_empty() {
        let v: Vec<String> = irqs.iter().map(|(n, c)| format!("{}: {}", n, c)).collect();
        t.push_str(&format!("irqs       {}\n", v.join(", ")));
    }
    let start = evs.len().saturating_sub(limit);
    (t, head, evs[start..].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delta_rows() {
        let m0: Value = serde_json::from_str(include_str!("../testdata/mem.json")).unwrap();
        let f0: Value = serde_json::from_str(include_str!("../testdata/fs.json")).unwrap();
        let mut m1 = m0.clone();
        let mut f1 = f0.clone();
        m1["page_cache"]["pages"] = json!(2000);
        f1["mounts"][0]["stats"]["cache"]["hits"] = json!(6000);
        f1["disk"][1]["reads"] = json!(400);
        let t = delta_text(&m0, &f0, &m1, &f1);
        let hits = f0["mounts"][0]["stats"]["cache"]["hits"].as_u64().unwrap();
        let reads = f0["disk"][1]["reads"].as_u64().unwrap();
        assert!(t.contains(&format!("page cache pages             {:>12} → {:<12} ({:+})", m0["page_cache"]["pages"].as_u64().unwrap(), 2000, 2000 - m0["page_cache"]["pages"].as_i64().unwrap())), "{}", t);
        assert!(t.contains(&format!("/ cache hits                 {:>12} → 6000         ({:+})", hits, 6000 - hits as i64)), "{}", t);
        assert!(t.contains(&format!("/dev/vda reads               {:>12} → 400          ({:+})", reads, 400 - reads as i64)), "{}", t);
        assert_eq!(t.lines().count(), 3, "{}", t);
        assert_eq!(delta_text(&m0, &f0, &m0, &f0), "(nothing changed)\n");
        assert_eq!(errno_of("enomem"), Some(12));
        assert_eq!(errno_of("5"), Some(5));
    }

    #[test]
    fn trace_text() {
        let t = concat!(
            "{\"on\":false,\"filter\":\"ls\",\"events\":8,\"dropped\":0,\"names\":{\"7\":\"ls\",\"3\":\"init\"}}\n",
            "{\"t\":1000000,\"c\":0,\"p\":7,\"k\":\"switch\",\"from\":3}\n",
            "{\"t\":1100000,\"c\":0,\"p\":7,\"k\":\"sys\",\"n\":\"openat\",\"a\":\"0x1\"}\n",
            "{\"t\":1300000,\"c\":0,\"p\":7,\"k\":\"blk\",\"op\":\"read\",\"sector\":8,\"n\":8}\n",
            "{\"t\":1400000,\"c\":0,\"p\":7,\"k\":\"sleep\",\"chan\":\"0x10\"}\n",
            "{\"t\":2400000,\"c\":1,\"p\":7,\"k\":\"wake\",\"chan\":\"0x10\",\"by\":0}\n",
            "{\"t\":2500000,\"c\":0,\"p\":7,\"k\":\"switch\",\"from\":0}\n",
            "{\"t\":2500000,\"c\":0,\"p\":7,\"k\":\"blkdone\",\"ns\":1200000}\n",
            "{\"t\":2600000,\"c\":0,\"p\":7,\"k\":\"ret\",\"n\":\"openat\",\"r\":3}\n",
        );
        let (text, head, events) = trace_summary(t, 3);
        assert_eq!(head["filter"], "ls");
        assert_eq!(events.len(), 3);
        assert!(text.contains("trace      8 events (0 dropped), stopped over 1 ms, filter \"ls\", 2 switches"), "{}", text);
        assert!(text.contains("       7 ls               1 (1.50 ms), 1 (1.00 ms, 0.10 ms), 0 faults, 2 runs"), "{}", text);
        assert!(text.contains("  openat                1       1.50 ms       1.50 ms"), "{}", text);
        assert!(text.contains("disk       1 reads, 0 writes, 4 KiB (1.20 ms in the device, 0 not finished)"), "{}", text);
    }

    // testdata/*.json は aios の VM で読んだ /proc/ai/mem と /proc/ai/fs そのもの
    #[test]
    fn mem_summary() {
        let m: Value = serde_json::from_str(include_str!("../testdata/mem.json")).unwrap();
        let t = mem_text(&m, 3, 2);
        assert!(t.contains("ram        2048.0 MiB total"), "{}", t);
        assert!(t.contains("kernel     heap 3.5 MiB / 64.0 MiB used"), "{}", t);
        assert!(t.contains("page cache 7.2 MiB (1844 pages, max 131072)"), "{}", t);
        assert!(t.contains("/usr/bin/aish"), "{}", t);
        assert_eq!(t.matches("/usr/").count(), 3, "{}", t);
        assert!(t.contains("procs      rss / vsize\n       7"), "{}", t);
        assert_eq!(t.lines().count(), 13, "{}", t);
    }

    #[test]
    fn fs_summary() {
        let f: Value = serde_json::from_str(include_str!("../testdata/fs.json")).unwrap();
        let t = fs_text(&f);
        assert!(t.contains("  /            ext4   /dev/vda2    rw\n    cache 4774 / 16384 blocks"), "{}", t);
        assert!(t.contains("journal 16383 blocks"), "{}", t);
        assert!(t.contains("  /tmp         tmpfs  tmpfs        rw  49 pages / 262144 limit"), "{}", t);
        assert!(t.contains("paths      447 cached (349 negative, max 4096), hit 45%"), "{}", t);
        assert!(t.contains("files      62 open in 17 processes"), "{}", t);
        assert!(t.contains("disk       /dev/vda2"), "{}", t);
    }
}

// ---- プロセス ----

struct P {
    pid: u32,
    ppid: u32,
    state: String,
    threads: u32,
    /// 使った CPU の時間 (1/100 秒)
    ticks: u64,
    rss_kb: u64,
    name: String,
    cmdline: String,
}

fn list_procs() -> Vec<P> {
    let mut out = Vec::new();
    let Ok(dir) = std::fs::read_dir("/proc") else { return out };
    for e in dir.flatten() {
        let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() else { continue };
        let Some(stat) = read(&format!("/proc/{}/stat", pid)) else { continue };
        // pid (comm) state ppid ... utime(14) stime(15) ... threads(20)
        let (Some(l), Some(r)) = (stat.find('('), stat.rfind(')')) else { continue };
        let name = stat[l + 1..r].to_string();
        let f: Vec<&str> = stat[r + 1..].split_whitespace().collect();
        let num = |i: usize| f.get(i - 3).and_then(|x| x.parse::<u64>().ok()).unwrap_or(0);
        let rss_kb = read(&format!("/proc/{}/status", pid))
            .and_then(|st| st.lines().find(|l| l.starts_with("VmRSS:"))?.split_whitespace().nth(1)?.parse().ok())
            .unwrap_or(0);
        let cmdline = std::fs::read(format!("/proc/{}/cmdline", pid))
            .map(|b| String::from_utf8_lossy(&b).replace('\0', " ").trim().to_string())
            .unwrap_or_default();
        out.push(P {
            pid,
            ppid: num(4) as u32,
            state: f.first().unwrap_or(&"?").to_string(),
            threads: num(20) as u32,
            ticks: num(14) + num(15),
            rss_kb,
            name,
            cmdline,
        });
    }
    out
}

fn procs(a: &Value) -> Value {
    let mut ps = list_procs();
    if let Some(n) = a["name"].as_str().filter(|n| !n.is_empty()) {
        ps.retain(|p| p.name.contains(n) || p.cmdline.contains(n));
    }
    match a["sort"].as_str().unwrap_or("cpu") {
        "mem" => ps.sort_by_key(|p| std::cmp::Reverse(p.rss_kb)),
        _ => ps.sort_by_key(|p| std::cmp::Reverse(p.ticks)),
    }
    let limit = a["limit"].as_u64().unwrap_or(20) as usize;
    let mut t = String::from("   pid   ppid s thr    cpu s      rss  cmd\n");
    for p in ps.iter().take(limit) {
        let cmd = if p.cmdline.is_empty() { format!("[{}]", p.name) } else { p.cmdline.chars().take(100).collect() };
        t.push_str(&format!("{:>6} {:>6} {} {:>3} {:>8.1} {:>8}  {}\n", p.pid, p.ppid, p.state, p.threads, p.ticks as f64 / 100.0, kib(p.rss_kb), cmd));
    }
    json!({ "text": t, "count": ps.len() })
}

// ---- ログ ----

fn kmsg(a: &Value) -> Value {
    let Some(k) = kmsg_text() else { return error("no /proc/kmsg (not aios, or an older kernel)") };
    json!({ "text": tail(&k, a["grep"].as_str().unwrap_or(""), a["lines"].as_u64().unwrap_or(50) as usize) })
}

fn log(a: &Value) -> Value {
    let unit = a["unit"].as_str().unwrap_or("");
    if unit.is_empty() {
        let mut names: Vec<String> = std::fs::read_dir("/var/log")
            .map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.ends_with(".log")).collect())
            .unwrap_or_default();
        names.sort();
        return json!({ "text": names.join("\n") + "\n" });
    }
    let full = if unit.contains('.') { unit.to_string() } else { format!("{}.service", unit) };
    let path = format!("/var/log/{}.log", full);
    match read(&path) {
        Some(t) => json!({ "path": path, "text": tail(&t, a["grep"].as_str().unwrap_or(""), a["lines"].as_u64().unwrap_or(50) as usize) }),
        None => error(format!("{}: no log", path)),
    }
}

// ---- コマンドを動かして測る ----

/// root だけが書ける /proc のファイルに書く (root でなければ sudo -n tee)
fn write_root(path: &str, data: &str) -> Result<(), String> {
    if unsafe { libc::geteuid() } == 0 {
        return std::fs::write(path, data).map_err(|e| format!("{}: {}", path, e));
    }
    let mut c = Command::new("sudo")
        .args(["-n", "tee", path])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("sudo: {}", e))?;
    let _ = c.stdin.take().unwrap().write_all(data.as_bytes());
    let o = c.wait_with_output().map_err(|e| e.to_string())?;
    if o.status.success() { Ok(()) } else { Err(format!("sudo -n tee {}: {}", path, String::from_utf8_lossy(&o.stderr).trim())) }
}

/// sh -c cmd を pwd で動かす。時間切れなら止める。(status, out, err, ms, timeout)
fn run(cmd: &str, pwd: &str, timeout: Duration) -> Result<(i32, String, String, u64, bool), String> {
    use std::os::unix::process::CommandExt;
    let start = Instant::now();
    let mut c = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(if pwd.is_empty() { "." } else { pwd })
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut o = c.stdout.take().unwrap();
    let mut e = c.stderr.take().unwrap();
    let ro = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = o.read_to_end(&mut b);
        b
    });
    let re = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = e.read_to_end(&mut b);
        b
    });
    let mut timed_out = false;
    let status = loop {
        match c.try_wait() {
            Ok(Some(st)) => break st.code().unwrap_or(-1),
            Ok(None) if start.elapsed() < timeout => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                timed_out = true;
                unsafe { libc::kill(-(c.id() as i32), libc::SIGKILL) };
                let _ = c.wait();
                break 124;
            }
        }
    };
    let cut = |b: Vec<u8>| {
        let s = String::from_utf8_lossy(&b).into_owned();
        if s.len() > 4000 { format!("...{}", &s[s.floor_char_boundary(s.len() - 4000)..]) } else { s }
    };
    let (out, err) = (cut(ro.join().unwrap_or_default()), cut(re.join().unwrap_or_default()));
    Ok((status, out, err, start.elapsed().as_millis() as u64, timed_out))
}

fn timeout_of(a: &Value) -> Duration {
    // Claude Code はツールの答えを 60 秒しか待たないので、長くても 50 秒
    Duration::from_millis(a["timeout_ms"].as_u64().unwrap_or(50_000).min(50_000))
}

fn bkl(a: &Value, pwd: &str) -> Value {
    if !Path::new("/proc/bkl").exists() {
        return error("no /proc/bkl (not aios, or an older kernel)");
    }
    let cmd = a["cmd"].as_str().unwrap_or("");
    if cmd.is_empty() {
        return json!({ "text": read("/proc/bkl").unwrap_or_default() });
    }
    if let Err(e) = write_root("/proc/bkl", "\n") {
        return error(e);
    }
    match run(cmd, pwd, timeout_of(a)) {
        Ok((status, out, err, ms, to)) => {
            let mut r = json!({ "status": status, "ms": ms, "text": read("/proc/bkl").unwrap_or_default() });
            if to {
                r["timeout"] = json!(true);
            }
            if status != 0 {
                r["out"] = json!(out);
                r["err"] = json!(err);
            }
            r
        }
        Err(e) => error(e),
    }
}

fn strace(a: &Value, pwd: &str) -> Value {
    if !aios() || !Path::new("/proc/strace").exists() {
        return error("no /proc/strace or /proc/kmsg (not aios, or an older kernel)");
    }
    let cmd = s(a, "cmd");
    let name: String = match a["name"].as_str().filter(|n| !n.is_empty()) {
        Some(n) => n.to_string(),
        None => cmd.split_whitespace().next().unwrap_or("").rsplit('/').next().unwrap_or("").to_string(),
    };
    if name.is_empty() {
        return error("no command");
    }
    // カーネルの comm は 15 文字まで
    let name: String = name.chars().take(15).collect();
    let all = a["all"].as_bool().unwrap_or(true);
    let before = kmsg_text().map_or(0, |k| k.lines().count());
    if let Err(e) = write_root("/proc/strace", &format!("{}{}", if all { "+" } else { "" }, name)) {
        return error(e);
    }
    let res = run(cmd, pwd, timeout_of(a));
    let _ = write_root("/proc/strace", "");
    let k = kmsg_text().unwrap_or_default();
    let limit = a["limit"].as_u64().unwrap_or(300) as usize;
    // 前に読んだ行より後ろの strace の行 (kmsg があふれていれば、あるものぜんぶ)
    let lines: Vec<&str> = k.lines().skip(before.min(k.lines().count())).filter(|l| l.contains("strace [")).collect();
    let n = lines.len();
    let mut text = lines.iter().take(limit).map(|l| l.split_once("strace ").map_or(*l, |(_, r)| r)).collect::<Vec<_>>().join("\n");
    text.push('\n');
    match res {
        Ok((status, out, err, ms, to)) => {
            let mut r = json!({ "status": status, "ms": ms, "calls": n, "text": text });
            if n > limit {
                r["more"] = json!(n - limit);
            }
            if to {
                r["timeout"] = json!(true);
            }
            r["out"] = json!(out);
            r["err"] = json!(err);
            r
        }
        Err(e) => error(e),
    }
}

// ---- M-s ----

fn key() -> Value {
    let Ok(mut tty) = Tty::open() else { return json!({}) };
    let t = summary()["text"].as_str().unwrap_or("").to_string();
    let lines: Vec<&str> = t.lines().collect();
    for l in &lines {
        tty.write(&format!("\x1b[2m{}\x1b[0m\r\n", aish_plugin::clip(l, tty.cols().saturating_sub(1))));
    }
    tty.write("\x1b[2m(何かキーで消す)\x1b[0m");
    let _ = tty.key();
    // 出したものを消して、始めの場所 (打ちかけの行の下の行の頭) へ戻る
    tty.write(&format!("\r\x1b[{}A\x1b[J", lines.len()));
    json!({})
}

// ---- /proc/ai: threads と hang ----

/// root (と /proc/ai は wheel の人) だけが読めるもの。読めなければ sudo -n cat
fn read_root(path: &str) -> Result<String, String> {
    if let Ok(t) = std::fs::read_to_string(path) {
        return Ok(t);
    }
    let o = Command::new("sudo").args(["-n", "cat", path]).output().map_err(|e| format!("sudo: {}", e))?;
    if o.status.success() { Ok(String::from_utf8_lossy(&o.stdout).into_owned()) } else { Err(format!("sudo -n cat {}: {}", path, String::from_utf8_lossy(&o.stderr).trim())) }
}

/// 1 行 1 つの JSON を読む
fn json_lines(path: &str) -> Result<Vec<Value>, String> {
    Ok(read_root(path)?.lines().filter_map(|l| serde_json::from_str(l).ok()).collect())
}

fn ai_threads() -> Result<Vec<Value>, String> {
    if !Path::new("/proc/ai/threads").exists() {
        return Err("no /proc/ai (not aios, or an older kernel)".into());
    }
    json_lines("/proc/ai/threads")
}

/// プロセス pid の fd (/proc/ai/fd/PID): fd 番号 → その行
fn ai_fds(pid: u64) -> std::collections::BTreeMap<u64, Value> {
    json_lines(&format!("/proc/ai/fd/{}", pid)).unwrap_or_default().into_iter().filter_map(|v| Some((v["fd"].as_u64()?, v))).collect()
}

/// wait の poll の fd 番号を、その fd の様子に置きかえる
fn annotate(t: &mut Value, fds: &std::collections::BTreeMap<u64, Value>) {
    if let Some(list) = t["wait"]["poll"].as_array() {
        let v: Vec<Value> = list.iter().map(|n| n.as_u64().and_then(|n| fds.get(&n).cloned()).unwrap_or(n.clone())).collect();
        t["wait"]["poll"] = json!(v);
    }
}

fn threads(a: &Value) -> Value {
    let ts = match ai_threads() {
        Ok(t) => t,
        Err(e) => return error(e),
    };
    // プロセスの名前 (代表スレッドの名前) も名前で絞るときに見る
    let leaders: std::collections::BTreeMap<u64, String> = ts.iter().filter(|t| t["tid"] == t["pid"]).map(|t| (t["pid"].as_u64().unwrap_or(0), s(t, "name").to_string())).collect();
    let name = s(a, "name");
    let state = s(a, "state");
    let min = a["min_slept_s"].as_f64().unwrap_or(0.0);
    let limit = a["limit"].as_u64().unwrap_or(50) as usize;
    let mut sel: Vec<Value> = ts
        .into_iter()
        .filter(|t| a["pid"].as_u64().is_none_or(|p| t["pid"].as_u64() == Some(p)))
        .filter(|t| name.is_empty() || s(t, "name").contains(name) || leaders.get(&t["pid"].as_u64().unwrap_or(0)).is_some_and(|l| l.contains(name)))
        .filter(|t| state.is_empty() || s(t, "state") == state)
        .filter(|t| min <= 0.0 || t["slept_s"].as_f64().is_some_and(|x| x >= min))
        .collect();
    let total = sel.len();
    sel.truncate(limit);
    let mut cache = std::collections::BTreeMap::new();
    for t in sel.iter_mut() {
        if t["wait"]["poll"].is_array() {
            let pid = t["pid"].as_u64().unwrap_or(0);
            let fds = cache.entry(pid).or_insert_with(|| ai_fds(pid));
            annotate(t, fds);
        }
    }
    json!({ "count": total, "threads": sel })
}

fn hang(a: &Value) -> Value {
    let ts = match ai_threads() {
        Ok(t) => t,
        Err(e) => return error(e),
    };
    let pids: Vec<u64> = match (a["pid"].as_u64(), s(a, "name")) {
        (Some(p), _) => vec![p],
        (None, n) if !n.is_empty() => ts.iter().filter(|t| t["tid"] == t["pid"] && s(t, "name").contains(n)).filter_map(|t| t["pid"].as_u64()).collect(),
        // どれとも言われなければ、みなを起こさずに見て、あやしいもの (hints のあるもの) だけを短く
        _ => {
            let me = std::process::id() as u64;
            let all: Vec<u64> = ts.iter().filter(|t| t["tid"] == t["pid"]).filter_map(|t| t["pid"].as_u64()).filter(|&p| p != me).collect();
            let found: Vec<Value> = all
                .iter()
                .map(|&p| hang_one(p, &ts, &[], false, false))
                .filter(|r| r["hints"].as_array().is_some_and(|h| !h.is_empty()))
                .map(|r| json!({ "pid": r["pid"], "name": r["name"], "hints": r["hints"] }))
                .collect();
            return json!({ "checked": all.len(), "suspects": found, "hint": "pid か name をつけると、そのプロセスのスレッドと待っているもの、kick (起こしてみる) と stack まで見る" });
        }
    };
    if pids.is_empty() {
        return error("no such process");
    }
    // 起こしてみる (futex で眠っているもの)。少し待って読みなおし、同じ futex で眠りなおしたかを見る
    let kick = a["kick"].as_bool().unwrap_or(true);
    let mut after: Vec<Value> = Vec::new();
    if kick {
        for p in &pids {
            if ts.iter().any(|t| t["pid"].as_u64() == Some(*p) && t["wait"]["futex"].is_u64()) {
                let cmd = format!("kick {}", p);
                if std::fs::write("/proc/ai/ctl", &cmd).is_err() {
                    let _ = write_root("/proc/ai/ctl", &cmd);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(500));
        after = ai_threads().unwrap_or_default();
    }
    let procs: Vec<Value> = pids.iter().map(|&pid| hang_one(pid, &ts, &after, kick, a["stack"].as_bool().unwrap_or(false))).collect();
    json!({ "procs": procs })
}

fn hang_one(pid: u64, ts: &[Value], after: &[Value], kick: bool, stack: bool) -> Value {
    let fds = ai_fds(pid);
    let mut mine: Vec<Value> = ts.iter().filter(|t| t["pid"].as_u64() == Some(pid)).cloned().collect();
    let name = mine.iter().find(|t| t["tid"].as_u64() == Some(pid)).map_or(String::new(), |t| s(t, "name").to_string());
    let mut hints: Vec<String> = Vec::new();
    // 読まれずに残っているもの (眠っている人がそれを見張っていないなら、起こしが消えたかもしれない)
    let pending: Vec<Value> = fds.values().filter(|f| f["pending"].as_u64().is_some_and(|n| n > 0)).cloned().collect();
    let watched: std::collections::BTreeSet<u64> = mine.iter().filter_map(|t| t["wait"]["poll"].as_array()).flatten().filter_map(|n| n.as_u64()).collect();
    for f in &pending {
        let n = f["fd"].as_u64().unwrap_or(0);
        if watched.contains(&n) {
            hints.push(format!("fd {} ({}) に読まれていないデータがあるのに、それを見張って眠っているスレッドがいる: 起こしが消えたかも", n, s(f, "what")));
        }
    }
    // 長く眠っているものから
    mine.sort_by(|x, y| y["slept_s"].as_f64().unwrap_or(-1.0).partial_cmp(&x["slept_s"].as_f64().unwrap_or(-1.0)).unwrap_or(std::cmp::Ordering::Equal));
    for t in mine.iter_mut() {
        annotate(t, &fds);
        let tid = t["tid"].as_u64().unwrap_or(0);
        if let Some(addr) = t["wait"]["futex"].as_u64() {
            let unchanged = t["wait"]["val"] == t["wait"]["now"];
            if kick {
                let again = after.iter().find(|x| x["tid"].as_u64() == Some(tid));
                let same = again.is_some_and(|x| x["wait"]["futex"].as_u64() == Some(addr) && x["state"] == "sleep");
                t["kick"] = json!(if same { "眠りなおした (同じ futex)" } else { "起きて進んだ" });
                if !same && t["slept_s"].as_f64().is_some_and(|x| x > 1.0) {
                    hints.push(format!("tid {} ({}) は起こすと進んだ: futex の起こしが消えていたかも", tid, s(t, "name")));
                }
            }
            if unchanged && t["slept_s"].as_f64().is_some_and(|x| x > 5.0) && tid == pid {
                hints.push(format!("代表スレッド {} が futex {:#x} で {:.0} 秒眠っていて、値は変わっていない: だれも知らせていない (ほかのスレッドかプロセスの返事を待っている。stack: true でどこで待っているか)", tid, addr, t["slept_s"].as_f64().unwrap_or(0.0)));
            }
        }
    }
    let busy: Vec<String> = mine.iter().filter(|t| t["state"] == "run" || t["state"] == "ready").map(|t| format!("{} {}", t["tid"], s(t, "name"))).collect();
    if !busy.is_empty() && mine.iter().any(|t| t["slept_s"].as_f64().is_some_and(|x| x > 5.0)) {
        hints.push(format!("走っているもの: {}。CPU が足りないか、大きなロックが混んでいるかもしれない (bkl で)", busy.join(", ")));
    }
    let mut r = json!({ "pid": pid, "name": name, "threads": mine, "pending_fds": pending, "hints": hints });
    if stack {
        // 長く眠っているものから 4 つ (mine はもう長い順)
        let tids: Vec<u64> = mine.iter().filter(|t| t["slept_s"].as_f64().is_some_and(|x| x > 1.0)).filter_map(|t| t["tid"].as_u64()).take(4).collect();
        r["stacks"] = json!(tids.iter().map(|&t| json!({ "tid": t, "frames": thread_frames(t, 30) })).collect::<Vec<_>>());
    }
    r
}

/// tid の呼び出しの並び (/proc/ai/stack/TID から)
fn thread_frames(tid: u64, max: usize) -> Value {
    match read_root(&format!("/proc/ai/stack/{}", tid)).map(|t| serde_json::from_str::<Value>(t.trim())) {
        Ok(Ok(st)) => json!(unwind::frames(&st, max)),
        Ok(Err(e)) => json!(format!("/proc/ai/stack/{}: {}", tid, e)),
        Err(e) => json!(e),
    }
}

fn stack(a: &Value) -> Value {
    let max = a["max"].as_u64().unwrap_or(40) as usize;
    if let Some(f) = a["file"].as_str().filter(|f| !f.is_empty()) {
        // 残しておいたもの: 写していたファイルが同じなら、プロセスが終わっていてもたどれる
        return match std::fs::read_to_string(f).map(|t| serde_json::from_str::<Value>(t.trim())) {
            Ok(Ok(st)) => json!({ "tid": st["tid"], "name": st["name"], "frames": unwind::frames(&st, max) }),
            Ok(Err(e)) => error(format!("{}: {}", f, e)),
            Err(e) => error(format!("{}: {}", f, e)),
        };
    }
    if let Some(t) = a["tid"].as_u64() {
        return json!({ "tid": t, "frames": thread_frames(t, max) });
    }
    let Some(pid) = a["pid"].as_u64() else { return error("tid か pid を") };
    let ts = match ai_threads() {
        Ok(t) => t,
        Err(e) => return error(e),
    };
    let mut mine: Vec<&Value> = ts.iter().filter(|t| t["pid"].as_u64() == Some(pid)).collect();
    mine.sort_by(|x, y| y["slept_s"].as_f64().unwrap_or(-1.0).partial_cmp(&x["slept_s"].as_f64().unwrap_or(-1.0)).unwrap_or(std::cmp::Ordering::Equal));
    let limit = a["limit"].as_u64().unwrap_or(8) as usize;
    let out: Vec<Value> = mine.iter().take(limit).map(|t| json!({ "tid": t["tid"], "name": t["name"], "state": t["state"], "wait": t["wait"], "frames": thread_frames(t["tid"].as_u64().unwrap_or(0), max) })).collect();
    json!({ "pid": pid, "threads": out })
}

fn watch(a: &Value) -> Value {
    let n = a["lines"].as_u64().unwrap_or(20) as usize;
    let state: Value = read("/run/aiwatch.json").and_then(|t| serde_json::from_str(t.trim()).ok()).unwrap_or(Value::Null);
    if state.is_null() {
        return error("no /run/aiwatch.json (aiwatch is not running: systemctl start aiwatch)");
    }
    let log = read("/var/log/aiwatch.service.log").unwrap_or_default();
    let all: Vec<Value> = log.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    let recent: Vec<Value> = all.iter().skip(all.len().saturating_sub(n)).cloned().collect();
    json!({ "active": state["active"], "checked_at": state["t"], "interval_s": state["interval_s"], "log": recent })
}

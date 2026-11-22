// aish-vm: 開発のマシン (Linux) で aios の VM を相手にする道具 (Claude は MCP のツール)
//   vm      VM を動かす: start (カーネルを選んで起こす) stop status run (シリアルの端末でコマンド。bg でうしろで) job put get shot
//           (画面を png に) keys (画面のキーボード: @alt-b など) log (シリアルの終わり、grep) mon (QEMU のモニタ)。
//           test/vm.py を呼ぶ
//   try     C (か C++) のコードを aarch64 の静的なプログラムにして (zig cc)、VM の中で動かして結果を返す
//   ab      2 つのカーネルで同じコマンドを n 回ずつ動かし、時間と大きなロックの使われ方をくらべる
//   bisect  test が通る (good) コミットと通らない (bad) コミットのあいだを git bisect で探す。
//           コミットごとにカーネルを作って VM で test を動かす (変わるのはカーネルだけ。ディスクはいまのもの)
//   box     カーネルの砂場: kernel.json の off のあるもの (extra、quirk と core の ns・vdso) を切って (起動の引数 aios.off=。
//           kernel/src/parts.rs) 起こし、
//           テスト (既定は CI と同じ vm-check と vm-ctests) を動かして、何も切らないときとくらべる。each で 1 つずつ全部
//   bench   速さ (kind fs: test/fsbench.c、net: test/netbench.c のループバック、smp: test/smpbench.c のいくつもの CPU) を VM で n 回測り、op ごとのまんなかと、
//           /proc/ai/prof の part (kernel.json の名前) ごとの CPU の時間の割合、lockbench のまんなか。against で 2 つの
//           カーネルを、linux で Linux の VM とくらべ、record で bench/KIND.jsonl に記録する (bench.rs)
//   sysdiff Linux と答えあわせ (test/sysdiff.c): 種から決まるシステムコールの並びを、手本の Linux (既定は最新の
//           安定版を QEMU で) と VM の aios で動かしてくらべ、ちがいをまとめて小さくする (sysdiff.rs)
//   ldiff   出すだけのテスト (test/NAME.c) を手本の Linux と VM の aios で動かしてくらべる (test/ldiff.sh) を、うしろで
//           いくつでも。答えだけ (ldiff.rs)
//   ctests  カーネルの C のテスト (test/vm-ctests.sh) を名前を選んで VM で。ok の数と FAIL したものだけ (ctests.rs)
// ファイル: main.rs (ツールの表と振り分け、みなで使う小さな道具)、vmop.rs (vm と panic の見つけかた)、kernel.rs (カーネルを
//   選ぶ・作る・起こす、start)、jobs.rs (うしろで動かすもの)、cc.rs (try)、ab.rs、bisect.rs、sandbox.rs (box)、bench.rs、
//   sysdiff.rs、ldiff.rs、ctests.rs
// カーネルの選び方 (kernel、a、b): ファイルのパス / "." (作業中のソースから作る) / git のコミット (別の作業場所
//   build/aish-vm/wt で作る)。どれも AIOS_INITRD=none (ルートはディスク) で build/aish-vm/target (wt は target-wt) に作り、
//   build/aish-vm/kernels/ にとっておく (同じコミットは作りなおさない)
// start、ab、bisect は長いので、うしろで動かす (自分を aish-vm start / ab / bisect で起こす)。もういちど呼ぶと様子か結果。
// ab と bisect は、動いている VM を止めてから、-snapshot (ディスクに書き残さない) で起こしなおす。2 つの VM が同じ
// aios.img を使うと壊れるので、同時には動かさない
// リポジトリは dir か、いまのディレクトリから上にたどって test/vm.py があるところ、なければ $AIOS_SRC
mod ab;
mod bench;
mod bisect;
mod cc;
mod ctests;
mod jobs;
mod kernel;
mod ldiff;
mod sandbox;
mod sysdiff;
mod vmop;

use aish_plugin::Ty::*;
use aish_plugin::{Arg, Spec, Tool, Value, arg, error, json, s};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use ab::job_ab;
use bisect::job_bisect;
use cc::try_;
use jobs::{Job, job};
use kernel::job_start;
use sandbox::job_box;
use vmop::vm;

const VM: &[Arg] = &[
    arg("op", Str, "start / stop / status / run / job / put / get / shot / keys / log / mon / reboot").req(),
    arg("bg", Bool, "run: 中でうしろで動かしてすぐ答える ({job: ID})。50 秒より長いもの (ap -S firefox など) に。そのあいだも run が使える"),
    arg("id", Int, "job: どれ (なければ一覧)。様子と、前に見せたところから先の出力"),
    arg("kill", Bool, "job: 止める (孫まで)"),
    arg("reset_after", Num, "reboot: この秒でプロンプトが来なければ QEMU をリセット (止まった新しいカーネルの電源を入れなおす)"),
    arg("kernel", Str, "start: カーネル (ファイルのパス / . で作業中のソースから作る / git のコミット)。なければ aios.img のもの"),
    arg("www", Str, "start: このディレクトリを http://10.0.2.2:8000/ に出す (中から fetch で取れる)"),
    arg("snapshot", Bool, "start: ディスクに書き残さない (QEMU の -snapshot)"),
    arg("off", Strs, "start: 切って起こす extra や quirk (kernel.json の off のあるもの。起動の引数 aios.off=。カーネルを直に起こすので kernel といっしょに)"),
    arg("cmd", Str, "run: シリアルの端末で動かすコマンド。mon: QEMU のモニタのコマンド"),
    arg("timeout", Num, "run: 秒 (既定 50、50 まで)"),
    arg("local", Str, "put: 送るファイル。get: 外の置き場 (既定 build/aish-vm/get/ファイル名)"),
    arg("remote", Str, "put: 中の置き場 (既定 /tmp/ファイル名)。get: 取り出す中のファイル"),
    arg("file", Str, "shot: png の置き場 (既定 build/aish-vm/shot.png)"),
    arg("keys", Strs, "keys: @ で始まるものは QEMU のキーの名前 (@alt-b @ctrl-l @ret @f5)、ほかは文字として打つ"),
    arg("lines", Int, "log: 終わりから何行 (既定 40)"),
    arg("grep", Str, "log: この文字をふくむ行だけ"),
    arg("wait_ms", Int, "start: 待つ長さ (既定 30000、50000 まで。起動しきっていなければ running。もういちど呼ぶと続きを待つ)。job: 終わるまで待つ長さ (既定 0、40000 まで)"),
    arg("dir", Str, ""),
];
const TRY: &[Arg] = &[
    arg("code", Str, "C のソース (main のあるもの)"),
    arg("file", Str, "code のかわりにソースのファイル"),
    arg("lang", Str, "c (既定) か c++"),
    arg("cflags", Strs, "zig cc に足すもの (-lpthread、-DX=1 など)"),
    arg("args", Str, "中で動かすときの引数 (シェルの書き方で)"),
    arg("timeout", Num, "秒 (既定 30、50 まで)"),
    arg("dir", Str, ""),
];
const AB: &[Arg] = &[
    arg("a", Str, "くらべる 1 つめのカーネル (パス / . / git のコミット。既定 HEAD)"),
    arg("b", Str, "2 つめ (既定 . = 作業中のソース)"),
    arg("cmd", Str, "中で動かして時間を測るコマンド").req(),
    arg("setup", Str, "起こしたあとに 1 度だけ動かすもの (測らない)"),
    arg("www", Str, "このディレクトリを http://10.0.2.2:8000/ に出す (setup で fetch して使う)"),
    arg("n", Int, "カーネルごとに何回 (既定 5)"),
    arg("timeout", Num, "1 回の上限 (秒、既定 300)"),
    arg("wait_ms", Int, "待つ長さ (既定 30000、50000 まで)"),
    arg("dir", Str, ""),
];
const BOX: &[Arg] = &[
    arg("off", Strs, "切るもの (kernel.json の off のあるもの: extra、quirk と core の ns・vdso。いっしょに切る)"),
    arg("each", Bool, "off のかわりに、切れるものを 1 つずつ全部 (どのテストがどれを要るかの表。長い)"),
    arg("test", Str, "中で動かすもの (既定: CI と同じ vm-check.sh と vm-ctests.sh。test/ を http で出して中から取る)。FAIL で始まる行をくらべる"),
    arg("www", Str, "test のために http://10.0.2.2:8000/ に出すディレクトリ (既定の test なら test)"),
    arg("kernel", Str, "カーネル (パス / . / git のコミット。既定 .)"),
    arg("baseline", Bool, "何も切らないものも動かしてくらべる (既定 true)"),
    arg("timeout", Num, "1 回の test の上限 (秒、既定 1200)"),
    arg("wait_ms", Int, "待つ長さ (既定 30000、50000 まで)"),
    arg("dir", Str, ""),
];
const BENCH: &[Arg] = &[
    arg("kind", Str, "fs (既定: ファイルまわり、test/fsbench.c)、net (ループバックの TCP / UDP / AF_UNIX とパイプ、test/netbench.c)、smp (いくつもの CPU でいっしょに: op ごとに 1 つのプロセスと CPU の数だけのプロセスで、伸びと /proc/ai/bkl。test/smpbench.c)"),
    arg("kernel", Str, "測るカーネル (パス / . / git のコミット。既定 .)"),
    arg("against", Str, "くらべる aios のカーネル (あれば。たとえば HEAD)。op ごとに、kernel の時間が against の何倍か"),
    arg("linux", Bool, "同じプログラムを Linux (test/linux-vm.sh の latest) の VM でも動かし、op ごとに aios の時間が Linux の何倍か (VM はどちらも cortex-a72、smp 個の CPU、2 GiB)"),
    arg("record", Bool, "結果を bench/KIND.jsonl (1 行 1 回の記録。リポジトリに入れる) に足し、bench/KIND.md (表) を作りなおす。ops、off、against なしで"),
    arg("show", Bool, "測らずに、記録のいちばん新しいものと、その前からの変わりかた、Linux との比"),
    arg("smp", Int, "VM の CPU の数 (既定 1、kind smp は 4。Linux も同じ)"),
    arg("ops", Strs, "net: 測る op だけ (pipe_rr futex_rr unix_rr tcp_rr udp_rr tcp_rr_epoll unix_local tcp_local udp_local unix_stream tcp_stream tcp_stream_4k tcp_connect)"),
    arg("n", Int, "カーネルごとに何回 (既定 3。まんなかを使う)"),
    arg("scale", Int, "量 (既定 1)"),
    arg("off", Strs, "切って測る extra や quirk (起動の引数 aios.off=)"),
    arg("full", Bool, "答えをみな (両方のカーネルの数と prof、lockbench、note)。既定は短く: op ごとの数 1 つ、ちがいがはっきりしたくらべだけ、CPU の時間の多い part と区間の上のほう (ぜんぶは saved のファイル)"),
    arg("wait_ms", Int, "待つ長さ (既定 30000、50000 まで)"),
    arg("dir", Str, ""),
];
const SYSDIFF: &[Arg] = &[
    arg("seeds", Int, "いくつの種 (既定 300)"),
    arg("from", Int, "はじめの種 (既定 1)"),
    arg("ops", Int, "種ごとの op の数 (既定 30、200 まで)"),
    arg("kernel", Str, "aios のカーネル (パス / . / git のコミット。既定 .)"),
    arg("fs", Str, "動かすところ: home (ホームの ext4。既定) か tmp (tmpfs)。Linux も同じ種類のところで"),
    arg("base", Str, "手本の Linux: latest (既定。kernel.org の最新の安定版を test/linux-vm.sh が arm64 で作り (版ごとに一度、数分)、QEMU で起こした中で nobody で動かす)、版 (7.2.11 など)、host (このマシンのカーネル)"),
    arg("linux", Str, "Linux の答えのファイル (ほかの Linux、たとえば aios(main) で cc -O2 -o sysdiff test/sysdiff.c して sysdiff -n OPS DIR FROM-TO > FILE としたもの)。なければ base の Linux で動かす (縮めるのはいつも base で)"),
    arg("shrink", Int, "縮めるちがいの数 (多いものから。既定 12、0 で縮めない)"),
    arg("wait_ms", Int, "待つ長さ (既定 30000、50000 まで)"),
    arg("dir", Str, ""),
];
const LDIFF: &[Arg] = &[
    arg("tests", Strs, r#"くらべるテスト: "NAME ARG..." (test/NAME.c。引数の @home は ext4 のところ: Linux は /work、aios はホーム)。続けて動かす"#),
    arg("name", Str, "tests のかわりに 1 つ"),
    arg("args", Strs, "name の引数"),
    arg("linux_only", Bool, "aios では動かさず、手本の Linux の出力と終わりの番号だけ (VM は要らない)"),
    arg("aios_only", Bool, "Linux では動かさず、aios の出力と終わりの番号だけ (aios だけのテスト。Linux が何も出さなかったときも、aios の出力を見せる)"),
    arg("root", Bool, "Linux は root、aios は sudo -n で"),
    arg("net", Bool, "手本の Linux に virtio-net をつなぐ"),
    arg("linux", Str, "手本の Linux の版 (既定 mainline。latest や 7.3.4 も)"),
    arg("fresh", Bool, "とっておいた Linux の答えを使わずに動かしなおす"),
    arg("kernel", Str, "はじめに、このカーネル (パス / . / git のコミット) で VM を起こしなおす (test/ を出して、-snapshot で)"),
    arg("wait_ms", Int, "待つ長さ (既定 30000、50000 まで)"),
    arg("dir", Str, ""),
];
const CTESTS: &[Arg] = &[
    arg("tests", Strs, "動かすテストの名前 (test/NAME.c。TESTS にないものも)。なければ CI と同じ全部 (5 分ほど)"),
    arg("name", Str, "tests のかわりに 1 つ"),
    arg("kernel", Str, "このカーネル (パス / . / git のコミット) で VM を起こしなおしてから。なければ、test/ を出している VM があればそれで、なければ作業中のソース (.) で起こす"),
    arg("timeout", Num, "上限 (秒、既定 1200)"),
    arg("wait_ms", Int, "待つ長さ (既定 30000、50000 まで)"),
    arg("dir", Str, ""),
];
const BISECT: &[Arg] = &[
    arg("good", Str, "test が通るコミット").req(),
    arg("bad", Str, "test が通らないコミット (既定 HEAD)"),
    arg("test", Str, "中で動かすコマンド。終了コード 0 で good、125 で skip、ほかは bad (起動しない、時間切れも bad)").req(),
    arg("timeout", Num, "1 回の test の上限 (秒、既定 300)"),
    arg("wait_ms", Int, "待つ長さ (既定 30000、50000 まで)"),
    arg("dir", Str, ""),
];

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
            "bench" => bench::job(&root, &a),
            "sysdiff" => sysdiff::job(&root, &a),
            "ldiff" => ldiff::job(&root, &a),
            "ctests" => ctests::job(&root, &a),
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
            Tool { name: "vm", desc: "開発のマシンで aios の VM を動かす (test/vm.py): start (kernel: パス / . / git のコミット) stop status run (シリアルの端末でコマンド。bg でうしろで動かして job で様子と出力) put (ファイルを中へ) get (中のファイルを外へ。中で作った png やログを Read で見るなど。16 MB まで) shot (画面を png に。Read で見る) keys (画面のキーボード: @alt-b @ctrl-l @ret、ほかは文字) log (シリアルの終わり、grep) mon (QEMU のモニタ) reboot (中で sudo reboot してプロンプトまで待つ。reset_after 秒で来なければリセット)。中のカーネルが panic していれば、run と job の時間切れと status の答えに panic (わけと、関数の名前にした backtrace)", args: VM },
            Tool { name: "try", desc: "C (か C++) のコードを aarch64-linux-musl の静的なプログラムにして (zig cc -O2 -static -s。zig がなければ aarch64-linux-gnu-gcc で glibc)、動いている VM の中で動かし、出力と終了コードを返す。カーネルのふるまいをすぐ確かめるため (VM は vm の start で先に)", args: TRY },
            Tool { name: "ab", desc: "2 つのカーネル (a、b: パス / . / git のコミット) で同じ cmd を n 回ずつ動かし、時間 (ms のまんなか、いちばん短い、長い) と大きなロックを持っていた割合をくらべる。動いている VM を止めて -snapshot で起こしなおす。うしろで動くので、終わっていなければ running。もういちど呼ぶと続きを待つ", args: AB },
            Tool { name: "bisect", desc: "test が通る good と通らない bad のあいだのコミットを git bisect で探す (別の作業場所 build/aish-vm/wt で)。コミットごとにカーネルを作り、VM (-snapshot) で test を動かす: 0 で good、125 で skip、ほか (起動しない、時間切れも) bad。変わるのはカーネルだけ。うしろで動くので、もういちど呼ぶと様子か結果 (first_bad)", args: BISECT },
            Tool { name: "bench", desc: "速さを測って記録し、くらべる。kind fs (既定): ファイルまわり (test/fsbench.c: create、stat、open、4 KiB の書き読み、readdir、rename、unlink、mkdir、16 MiB の書き読み、fsync など)。kind net: ループバック (test/netbench.c: パイプ、AF_UNIX、TCP、UDP の 1 バイトの往復を 2 つのプロセスと 1 つのプロセスで、futex で起こしあう 2 つのスレッドの往復、epoll で待つ往復、64 KiB と 4 KiB の流しこみ、connect)。kind smp: いくつもの CPU でいっしょに (test/smpbench.c: getppid、umask、futex、stat、open、4 KiB の読み書き、作って消す、パイプ、AF_UNIX、mmap、fork を、1 つのプロセスと CPU の数だけのプロセス (それぞれ自分のファイルなど) で。伸び (1 つのときの何倍まわせたか) を Linux とくらべ、op ごとに /proc/ai/bkl の大きなロックを待った割合と長く持ったシステムコール)。VM (-snapshot、smp 個の CPU) で n 回動かして op ごとの µs (流しこみは MB/s) のまんなかと、prof をつけた回で part (kernel.json の名前) ごとの CPU の時間の割合 (どこがボトルネックか。眠って待った時間は wait_us) と、時間の多い区間 (net は op ごと)。lock は lockbench の 5 回のまんなか。against で 2 つの aios のカーネルを、linux で同じプログラムを Linux の VM (cortex-a72) でくらべる。record で bench/KIND.jsonl に記録を足して bench/KIND.md の表を作りなおす (いつも、前の記録からの変わりかたも出す)。show で測らずに記録を見る。答えは短く (はっきりしたちがいだけ。full でみな)、ぜんぶは build/aish-vm/bench/ に残る。うしろで動くので、終わっていなければ running。もういちど呼ぶと続きを待つ", args: BENCH },
            Tool { name: "sysdiff", desc: "Linux と答えあわせ (test/sysdiff.c): 種から決まるシステムコールの並び (open、read、write、stat、mkdir、rename、link、symlink、chmod、access、getdents、dup、fcntl、pipe など。fd は前の op が開けたもの) を、手本の Linux (既定は kernel.org の最新の安定版を作って QEMU で。base: host でこのマシンのもの。どれも nobody で) と VM (-snapshot) の aios で動かし、case ごとに最初にちがう行をくらべる。ちがいは (op、Linux の答え、aios の答え) でまとめ、多いものから、op を減らしても同じちがいが同じ op で出るいちばん小さな並び (SEED:I,J,...) に縮める。aios がえらんだちがい (test/sysdiff.c の chosen、doc/aios.md) は Linux でもそのふるまいをまねて答えにし、chosen に何 case で使ったかを分けて出す (手本の Linux のまちがい (linux_bugs) も、Linux の側でただして同じように)。答えはアーキテクチャによらない形 (フラグは名前、stat は種類と許可と数)。結果は build/aish-vm/sysdiff/ にも残る。うしろで動くので、終わっていなければ running。もういちど呼ぶと続きを待つ", args: SYSDIFF },
            Tool { name: "ldiff", desc: "出すだけのテスト (test/NAME.c) を、手本の Linux (既定 mainline を QEMU で。答えはとっておく) と動いている aios の VM (test/ を出しているもの。なければ kernel で起こす) で動かし、出力をくらべる (test/ldiff.sh)。tests にいくつでも。テストごとに same、differ (diff -u の頭: --- Linux、+++ aios)、error だけを返す。手本の Linux に足りない機能は note に。linux_only で Linux の出力だけ (作るテストの答えを先に見る)、aios_only で aios の出力だけ (aios だけのテスト。Linux が何も出さなかったものも aios の出力を aios_only に)。うしろで動くので、終わっていなければ running。もういちど呼ぶと続きを待つ", args: LDIFF },
            Tool { name: "ctests", desc: "カーネルの C のテスト (test/vm-ctests.sh: CI の VM test と同じ、テストごとの引数や sudo も) を名前を選んで VM で動かす: 外で test/bin/NAME を作り (古いものだけ)、test/ を出している VM (なければ kernel か作業中のソースで起こす) の中で動かして、ok の数と、FAIL したもの (終わりの番号とそのテストの出力の終わり) だけを返す。時間切れなら、どこまで進んだか (last_ok) と中のカーネルの panic。うしろで動くので、終わっていなければ running。もういちど呼ぶと続きを待つ", args: CTESTS },
            Tool { name: "box", desc: "カーネルの砂場: kernel.json の off のあるもの (extra、quirk と core の ns・vdso) を切って (起動の引数 aios.off=) VM (-snapshot) を起こし、test (既定は CI と同じ vm-check と vm-ctests) を動かして、何も切らないときと FAIL の行をくらべる。それがなくても動くか、quirk がまだ要るかを確かめる。each で 1 つずつ全部 (どのテストがどれを要るか)。うしろで動くので、終わっていなければ running。もういちど呼ぶと続きを待つ", args: BOX },
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
                "ldiff" => job(&mut jobs, &root, "ldiff", a),
                "ctests" => job(&mut jobs, &root, "ctests", a),
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

/// 途中の様子 (ログに 1 行。最後の行だけが答え)
fn note(v: Value) {
    println!("{}", v);
}

/// テストや測るコマンドはサブシェルで (exit 125 などで、中のシリアルの端末のシェルごと終わらないように。
/// 終わると vm.py は終わりの印を見られず、時間切れまで待つ)
fn sub(cmd: &str) -> String {
    format!("(\n{}\n)", cmd)
}

/// 文字列の並び (JSON の配列から)
fn strings(v: &Value) -> Vec<String> {
    v.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).filter(|x| !x.is_empty()).collect()).unwrap_or_default()
}

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
//   fw      ファイアウォール (/proc/ai/fw): nftables の基の鎖と決まり (当たった数、さいごに当たってから、短い式)、
//           落としたもの (どのパケットを、どのフックで、どの決まりが)、conntrack のつながり、trace (パケットを流さずに
//           決まりの道を試す: ctl の fw trace)。nft があれば nft -a list ruleset も
//   M-s     まとめを画面に出す (キーを押すと消える)
// /proc/bkl、/proc/strace を 0 からにするのは root だけなので、root でなければ sudo -n tee で書く。
// 開発の Linux でも動く (aios にしかないものは「ない」と答える)
// ファイル: main.rs (ツールの表と振り分け、みなで使う小さな道具: read、tail、run など)、state.rs (get、do、tune)、
//   summary.rs (sys と M-s)、memfs.rs (mem、fs)、procs.rs、logs.rs (kmsg、log)、measure.rs (bkl、strace)、
//   threads.rs (threads、hang、stack、watch)、fail.rs、delta.rs、part.rs、trace.rs、fw.rs、unwind.rs (stack の .eh_frame)
mod delta;
mod fail;
mod fw;
mod logs;
mod measure;
mod memfs;
mod part;
mod procs;
mod state;
mod summary;
mod threads;
mod trace;
mod unwind;

use aish_plugin::{Spec, Tool, Value, error, json, s};
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use delta::delta;
use fail::fail;
use fw::fw;
use logs::{kmsg, log};
use measure::{bkl, strace};
use memfs::{fs, mem};
use part::part;
use procs::procs;
use state::{do_, get, tune};
use summary::{key, summary};
use threads::{hang, stack, threads, watch};
use trace::trace;

const GET: &str = r#"{"type":"object","properties":{"path":{"type":"string","description":"点でつなぐ: host kernel mem disk proc service pkg net user boot log の下 (kernel.cpus、service.sshd、pkg.installed.cargo、proc.123)。なければぜんぶ"}}}"#;
const DO: &str = r#"{"type":"object","properties":{"op":{"type":"string","description":"service / pkg / power / apply (/etc/aios.json のとおりにそろえる) / rollback (ひとつ前の apply に戻す) / confirm (confirm つきで入れたファイアウォールでよいとする: しなければ前の決まりにもどる) / ping"},"confirm":{"type":"integer","description":"apply のとき: ファイアウォール (aios.json の firewall) を変えるなら、この秒 (10 から 3600) のうちに op confirm がなければ前の決まりにもどす。ssh ごしに決まりを変えるときは、つけて、つながりを確かめてから confirm"},"action":{"type":"string","description":"service: start stop restart enable disable。pkg: install remove upgrade refresh。power: reboot poweroff"},"name":{"type":"string","description":"service の名前 (sshd など)"},"names":{"type":"array","items":{"type":"string"},"description":"pkg install / remove のパッケージ"}},"required":["op"]}"#;
const DIFF: &str = r#"{"type":"object","properties":{}}"#;
const TUNE: &str = r#"{"type":"object","properties":{"name":{"type":"string","description":"sysctl の名前 (kernel.sched_timeslice_ms など。aios get kernel.sysctl で一覧)"},"values":{"type":"array","items":{"type":"string"},"description":"くらべる値"},"cmd":{"type":"string","description":"測る仕事 (sh -c)。値ごとに n 回動かす"},"n":{"type":"integer","description":"値ごとに動かす回数 (既定 3)"},"apply":{"type":"boolean","description":"いちばん速かった値を /etc/aios.json の sysctl に書いて aios apply する"}},"required":["name","values","cmd"]}"#;
const NONE: &str = r#"{"type":"object","properties":{}}"#;
const PROCS: &str = r#"{"type":"object","properties":{"sort":{"type":"string","description":"cpu (既定。使った CPU の時間) か mem (メモリ)"},"limit":{"type":"integer","description":"いくつまで (既定 20)"},"name":{"type":"string","description":"名前にこれをふくむものだけ"}}}"#;
const KMSG: &str = r#"{"type":"object","properties":{"grep":{"type":"string","description":"この文字をふくむ行だけ"},"lines":{"type":"integer","description":"終わりから何行 (既定 50)"}}}"#;
const LOG: &str = r#"{"type":"object","properties":{"unit":{"type":"string","description":"サービスの名前 (sshd、aiwm.service など)。なければログのある一覧"},"grep":{"type":"string","description":"この文字をふくむ行だけ"},"lines":{"type":"integer","description":"終わりから何行 (既定 50)"}}}"#;
const RUN: &str = r#"{"type":"object","properties":{"cmd":{"type":"string","description":"動かすコマンド (sh -c)。なければ今の値を読むだけ (bkl)"},"timeout_ms":{"type":"integer","description":"これを過ぎたら止める (既定 50000)"}}}"#;
const THREADS: &str = r#"{"type":"object","properties":{"pid":{"type":"integer","description":"このプロセスのスレッドだけ"},"name":{"type":"string","description":"名前 (スレッドかプロセスの) にこれをふくむものだけ"},"state":{"type":"string","description":"sleep / run / ready / zombie"},"min_slept_s":{"type":"number","description":"これだけ眠りつづけているものだけ"},"limit":{"type":"integer","description":"いくつまで (既定 50)"}}}"#;
const HANG: &str = r#"{"type":"object","properties":{"pid":{"type":"integer","description":"見るプロセス (pid も name もなければ、みなからあやしいものを探す)"},"name":{"type":"string","description":"pid のかわりに名前 (プロセスの代表スレッドの名前にふくむもの。いくつもあればみな)"},"kick":{"type":"boolean","description":"futex で眠っているスレッドをみな起こして、同じところで眠りなおすかを見る (既定 true。わけもなく起こしても壊れない)"},"stack":{"type":"boolean","description":"長く (1 秒より) 眠っているスレッドの呼び出しの並び (stack ツールと同じ。長い順に 4 つまで)"}}}"#;
const STACK: &str = r#"{"type":"object","properties":{"tid":{"type":"integer","description":"スレッドの番号"},"file":{"type":"string","description":"tid のかわりに、残しておいた /proc/ai/stack の中身 (aiwatch の stack_file)"},"pid":{"type":"integer","description":"tid のかわりに: そのプロセスのスレッドみな (眠っているものの、長く眠っている順)"},"max":{"type":"integer","description":"何段まで (既定 40)"},"limit":{"type":"integer","description":"pid のときいくつのスレッドまで (既定 8)"}}}"#;
const WATCH: &str = r#"{"type":"object","properties":{"lines":{"type":"integer","description":"記録の終わりから何行 (既定 20)"}}}"#;
const FAIL: &str = r#"{"type":"object","properties":{"cmd":{"type":"string","description":"動かすコマンド (sh -c)。決まりをかけて動かし、終わったらやめる。なければ決まりをかけるだけ (off: true でやめる)"},"name":{"type":"string","description":"相手: この名前で始まるプロセス (既定: cmd の最初の語)"},"pid":{"type":"integer","description":"name のかわりに、このプロセス"},"what":{"type":"string","description":"sys (システムコールを -errno に。既定) / page (物理ページを取れないことに: プロセスが out of memory で殺される) / heap (カーネルのヒープを、耐えるところで取れないことに: そのシステムコールが ENOMEM) / oom (耐えないところで: 予備から渡り、次のシステムコールで oom がいちばん大きなプロセスを殺す。-snapshot の VM で)"},"syscall":{"type":"string","description":"sys のとき: 名前 (openat、read ...) か番号か all (既定 all)"},"errno":{"type":"string","description":"sys のとき: ENOMEM、EIO、EAGAIN ... か番号 (既定 ENOMEM)"},"times":{"type":"integer","description":"何回しくじらせるか (既定 1)"},"off":{"type":"boolean","description":"決まりをやめる"},"timeout_ms":{"type":"integer","description":"待つ長さ (ms、既定 50000、50000 まで)"}}}"#;
const DELTA: &str = r#"{"type":"object","properties":{"cmd":{"type":"string","description":"動かすコマンド (sh -c)"},"timeout_ms":{"type":"integer","description":"待つ長さ (ms、既定 50000、50000 まで)"}},"required":["cmd"]}"#;
const PART: &str = r#"{"type":"object","properties":{"name":{"type":"string","description":"切れるところの名前 (kernel.json の off のあるもの: epoll inotify landlock module ns seccomp swap timerfd vdso tty-root-fdlink sound-min-period qemu-uart-burst。装置 (net、virtio_rng、bcm2835_rng) と aiproc は起動のときだけ: box で)"},"off":{"type":"boolean","description":"true で切る、false で入れる。なければ見るだけ"}}}"#;
const TRACE: &str = r#"{"type":"object","properties":{"cmd":{"type":"string","description":"動かすコマンド (sh -c)。記録を空にして始め、終わったら止めて読む。なければ、いま記録にあるものを読むだけ"},"name":{"type":"string","description":"この名前で始まるプロセスのシステムコールとフォルトだけ (既定: cmd の最初の語。切りかえ・眠り・起こし・割り込み・ディスクはみな)"},"all":{"type":"boolean","description":"名前で絞らない (みなのシステムコールも)"},"limit":{"type":"integer","description":"events に返す出来事の数 (終わりから。既定 200)"},"timeout_ms":{"type":"integer","description":"cmd をこれで止める (既定 50000)"}}}"#;
const MEM: &str = r#"{"type":"object","properties":{"files":{"type":"integer","description":"ページキャッシュを多く使っているファイルをいくつまで (既定 10)"},"procs":{"type":"integer","description":"メモリを多く使っているプロセスをいくつまで (既定 8)"}}}"#;
const FW: &str = r#"{"type":"object","properties":{"trace":{"type":"string","description":"パケットを流さずに決まりの道を試す: 'in|out eth0|lo tcp|udp|icmp SRC[:PORT] DST[:PORT] [syn|ack|synack|fin|rst]' (in eth0: 外から来るもの、out eth0: 外へ出すもの、lo: 中どうし)。counter も conntrack も変えない"},"drops":{"type":"integer","description":"落としたものをいくつまで (新しい順。既定 20)"},"ct":{"type":"boolean","description":"conntrack のつながりも 1 つずつ (既定 false: 数だけ)"},"grep":{"type":"string","description":"この文字をふくむ決まり (表、鎖、式) だけ"},"nft":{"type":"boolean","description":"nft -a list ruleset の出力も (nft があれば。root でなければ sudo -n で)"}}}"#;
const STRACE: &str = r#"{"type":"object","properties":{"cmd":{"type":"string","description":"動かすコマンド (sh -c)"},"name":{"type":"string","description":"見るプロセスの名前 (既定: cmd の最初の語。子のプロセスを見るときに)"},"all":{"type":"boolean","description":"うまくいったものも (既定 true。false ならしくじったものだけ)"},"limit":{"type":"integer","description":"何行まで (既定 300)"},"timeout_ms":{"type":"integer","description":"待つ長さ (ms、既定 50000、50000 まで)"}},"required":["cmd"]}"#;

fn main() {
    let spec = Spec {
        name: "sys",
        hooks: &["key"],
        keys: &[("M-s", "sys")],
        tools: &[
            Tool { name: "health", desc: "いまの見たて (aios health --json): サービスのしくじり、ディスクとメモリの足りなさ、BKL の取りわすれ、カーネルのヒープと oom、ディスクを待って固まったスレッド、試している新しいカーネル、ファイアウォールの確かめ待ち、apply のしくじり、新しい版のあるパッケージ。1 つずつ status (ok / info / warn / crit)、what、hint (次に使う道具)。まずここを見る", input: NONE },
            Tool { name: "get", desc: "aios の状態の木 (aios get --json と同じ)。health host kernel mem disk proc service pkg net user boot log。path で一部だけ", input: GET },
            Tool { name: "do", desc: "aiosd (root) に頼んで aios を変える: サービスの start/stop/restart/enable/disable、パッケージの install/remove/upgrade/refresh、reboot/poweroff、apply / rollback (/etc/aios.json)、confirm (confirm つきの apply で入れたファイアウォールを残す)。root と wheel の人だけ。したことは /var/log/aiosd.log に残る", input: DO },
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
            Tool { name: "fail", desc: "わざとしくじらせる (フォルト注入。/proc/ai/ctl の fail): cmd のプロセス (name か pid) のシステムコール (syscall か all) を、動かさずに -errno (ENOMEM など) にする、またはカーネルの物理ページ (page: そのプロセスが out of memory で殺される) やヒープ (heap: 耐えるところで、そのシステムコールが ENOMEM。oom: 耐えないところで、予備から渡って次のシステムコールで oom がいちばん大きなプロセスを殺す。-snapshot の VM で) を取れないことにして、プログラムとカーネルがどう耐えるかを見る。times 回しくじらせたら効かなくなる。hits は効いた数。root か wheel の人だけ", input: FAIL },
            Tool { name: "delta", desc: "cmd を動かした前と後の、カーネルの中の数のちがい (/proc/ai/mem、/proc/ai/fs、/proc/bkl): ページキャッシュ (ページ、読みそこね、捨てた)、ヒープ (使っている、いちばん多かった、取れなかった)、tmpfs、スワップ、ページフォルト、ext4 のブロックキャッシュ (当たり、外れ、まとめ読み、捨てた、書き出し)、パスの覚え (当たり、外れ)、開いているファイル、区画ごとのディスクの読み書き、大きなロック。そのコマンドがカーネルに何をさせたかがわかる。root か wheel の人だけ", input: DELTA },
            Tool { name: "part", desc: "カーネルの切れるところ (kernel.json の off のあるもの: extra、quirk と core の ns・vdso) のいまの様子 (/proc/ai/parts: 切ってあるか、動かしたまま変えられるか)。name と off で、動かしたまま切る (システムコールは ENOSYS、quirk は手当てをしない) か入れる: 入れずに作った Linux のようにしてプログラムを動かし、戻せる。装置と aiproc は起動のときだけ (vm start の off か box)。root か wheel の人だけ", input: PART },
            Tool { name: "trace", desc: "カーネルの中の出来事 (/proc/ai/trace): cmd を動かしているあいだの、システムコールの入りと出 (かかった時間)、スレッドの切りかえ、眠りと起こし (待った時間、起こされてから走るまで)、割り込み、ディスクの読み書き、ページフォルトを記録して、スレッドごと、システムコールごと (回数、合計、最大)、遅かった順、ディスク、割り込みにまとめる (text)。events は終わりの limit こ (t は起動からの ns、c は CPU、p は tid、k は sys/ret/switch/sleep/wake/irq/blk/blkdone/fault/exec/exit)。cmd がなければ、いま記録にあるもの (ctl の trace on で始めたもの)。root か wheel の人だけ", input: TRACE },
            Tool { name: "fw", desc: "ファイアウォール (nftables) のいまの様子 (/proc/ai/fw): 基の鎖 (フック、priority、policy、通った数、policy まで来た数) と決まり (handle、当たった数 (counter がなくても)、さいごに当たってからの ms、短い式)、集まりの要素の数、落としたもの (新しい順: いつ、フック、口、proto、送り元とあて先、TCP のフラグ、どの決まりか (handle か policy)、reject、出したソケットの uid)、conntrack (数、状態ごと、ct: true でつながり)。trace: パケットを流さずに、いまの決まりでどこを通ってどうなるかを決まりごとに (つながらない・届かないわけを探すとき)。root か wheel の人だけ", input: FW },
            Tool { name: "fs", desc: "ファイルシステムの中 (/proc/ai/fs): マウントごとの様子 (ext4 はブロックキャッシュの数と当たり・外れ・捨てた数、まだ書いていないブロック、書き出した回数、ジャーナル、inode と名前の覚え、空き。tmpfs はページ)、パスの覚え (数、当たり・外れ、捨てた回数)、開いているファイルの数、区画ごとのディスクの読み書き。text はまとめ、ほかはそのままの数。root か wheel の人だけ", input: NONE },
        ],
    };
    aish_plugin::run(spec, |ev, v| match ev {
        "tool" => {
            let a = &v["args"];
            let pwd = s(v, "pwd");
            match s(v, "name") {
                "health" => match Command::new("aios").args(["health", "--json"]).output() {
                    // 終わりの番号は見たて (warn 1、crit 2) なので、しくじりにしない
                    Ok(o) => {
                        let v: Value = serde_json::from_slice(&o.stdout).unwrap_or_else(|_| json!({ "error": String::from_utf8_lossy(&o.stderr) }));
                        let text: String = v["checks"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .map(|c| {
                                let hint = c["hint"].as_str().map_or(String::new(), |h| format!("  → {}", h));
                                format!("{:<5} {:<9} {}{}\n", c["status"].as_str().unwrap_or("?"), c["name"].as_str().unwrap_or("?"), c["what"].as_str().unwrap_or(""), hint)
                            })
                            .collect();
                        json!({ "text": text, "status": v["status"], "summary": v["summary"], "checks": v["checks"] })
                    }
                    Err(e) => error(format!("aios: {}", e)),
                },
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
                "fw" => fw(a),
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

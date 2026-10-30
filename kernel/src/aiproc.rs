// /proc/ai: AI (aish の MCP のツール) が読むための、カーネルの中の様子。どれも 1 行 1 つの JSON。
// 読み書きできるのは root と wheel の人だけ (allowed)。
//   threads   すべてのスレッド: 状態、最後のシステムコール、眠っていれば何を (futex / 見張っている fd / channel)、
//             どれだけ (slept_s) 待っているか (proc::ai_threads)
//   prof      区間 (part:what) ごとの回数と時間と、part (kernel.json の名前) ごとの割合 (prof.rs。ctl に prof on で数える)
//   kernel    kernel/kernel.json: ファイルごとの core (起動して /init が動くのに要るもの) と extra、quirk (ある機械や
//             プログラムのつごうで入れた手当て。cause と until つき) の一覧。だれでも読める
//   heap      カーネルのヒープ: 使っている、いちばん多かった、空きのいちばん大きなかたまり、取れなかった回数 (heap::stats_json)
//   bkl       大きなロックの統計 (smp::stats_json)。ctl に bkl reset で 0 から
//   mem       メモリの中: RAM の空き、カーネルのヒープとスタック、ページキャッシュ (ファイルごとも)、MAP_SHARED の
//             ページ、tmpfs、スワップ、ページフォルト、メモリを多く使っているプロセス (mem_json)
//   fs        ファイルシステムの中: マウントごとの様子 (ext4 のブロックキャッシュ、ジャーナル、inode の覚え。tmpfs の
//             ページ)、パスの覚え、開いているファイルの数、区画ごとのディスクの読み書き (fs_json)
//   fail      わざとしくじらせる決まり (fail.rs) のいまの様子: 相手、何を、あと何回、効いた数。ctl の fail で決める
//   parts     切れるところ (parts.rs) のいまの様子: 切ってあるか、動かしたまま変えられるか。だれでも読める
//   trace     カーネルの中の出来事の記録 (trace.rs): システムコールの入りと出、スレッドの切りかえ、眠りと起こし、
//             割り込み、ディスクの読み書き、ページフォルト、exec と exit。ctl の trace on で始める。aish の trace
//   fd/PID    そのプロセスの fd: 何か、読める・書ける・閉じた、読まれずに残っているもの
//   stack/TID スレッドのレジスタ、スタック (sp から 64 KiB まで、16 進)、ファイルを写している地図 (proc::ai_stack)。
//             aish-sys がファイルの .eh_frame でたどる
//   ctl       調べもののスイッチ (書く。読むと書けるものの一覧):
//               kick PID      その PID の futex で眠っているスレッドをみな起こす (起こしが消えたのかを確かめる。
//                             futex はわけもなく起きてよいので、こわれない)
//               raw TID       そのスレッドのレジスタとスタック (32 KiB) を kmsg に (外で .eh_frame でたどる)
//               pcmiss on|off ページキャッシュに無かったものを kmsg に
//               strace NAME   /proc/strace と同じ (+NAME でうまくいったものも、空で止める)
//               bkl reset     /proc/bkl を 0 から
//               bkl selftest  BKL が守るもの (cell::Bkl) の見はりが効いているか (BKL を放して触る。kmsg に bkl: selftest ok)
//               heap trace on|off  /proc/ai/heap に、ヒープをいま多く持っている場所 (呼び出しの番地) も出す
//               heap check    ヒープの空きの管理 (tlsf) の決まりを確かめて kmsg に (heap: check ok / FAILED)
//               heap hog N    N MiB をわざと取って持っておく (足りなくなったときの道: 足す、捨てる、ENOMEM。0 で返す)
//               page cycle    空いている物理ページを (少し残して) みな取ってから返す (RAM をひととおり使ったあとの様子)
//               page check    物理ページの空き (buddy: kalloc.rs) の決まりを確かめて kmsg に (page: check ok / FAILED)
//               prof on|off   カーネルの中の区間 (prof::span) ごとの回数と時間を 0 から数える。/proc/ai/prof で読む
//               lockbench     スピンロック、ヒープ、大きなロック、時計、物理ページの 1 回の時間を kmsg に (spinlock::bench)
//               fail pid N|name NAME sys NR|all ERRNO [TIMES]  そのプロセスのシステムコール (番号か名前か、みな) を、
//                             動かさずに -ERRNO に (TIMES 回。既定 1)。fail ... heap [TIMES] はカーネルのヒープの alloc を
//                             耐えるところで (ENOMEM)、fail ... oom [TIMES] は耐えないところで (予備と oom.rs)、
//                             fail ... page [TIMES] は物理ページを、取れないことに。fail off でやめる。/proc/ai/fail で見る
//               part NAME off|on  切れるところ (kernel.json の off) を、動かしたまま切る・入れる (使うたびに見るもの
//                             だけ: システムコールと quirk。装置と aiproc はできない)。/proc/ai/parts で見る
//               trace on [NAME]|off|clear  出来事の記録を始める (NAME があれば、その名前で始まるプロセスのシステム
//                             コールとフォルトだけ)、止める、空にする (/proc/ai/trace)
//               panic [TEXT]  カーネルを panic させる (root だけ。panic の出力とバックトレースを確かめる。Linux の
//                             sysrq-trigger の c と同じ)
use alloc::format;
use alloc::string::String;

/// wheel (sudo できる人。aiosd と同じ)。/proc/ai/ctl は root:wheel の 0664
pub const WHEEL: u32 = 10;

/// /proc/ai を読み書きしてよいか: root か wheel の人で、user の namespace の中 (aibox --root) でないこと。
/// すべてのプロセスの中身 (待っているアドレス、fd、レジスタ) が見えるので。wheel の人はもともと sudo で見られる。
/// Claude の aish は aibox の中 (landlock で sudo が効かず、PID の namespace も別) で動くので、
/// wheel なら PID の namespace の中でも読める。番号はいつも本当のもの (はじめの namespace の)。
/// 書く (ctl) のは、aibox なら -w /proc/ai/ctl があるときだけ (landlock)
pub fn allowed() -> bool {
    let c = crate::proc::current_cred_ref();
    (c.euid == 0 || c.in_group(WHEEL)) && c.ns.user.is_none()
}

/// JSON の文字列の中身にする (" と \ と制御文字)
pub fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o
}

/// /proc/ai/fd/PID
pub fn fds_json(tgid: u32) -> Result<String, i64> {
    let l = crate::proc::find_leader(tgid).ok_or(-2)?;
    let files = l.files.as_ref().ok_or(-2)?;
    let mut out = String::new();
    for (i, f) in files.get().fds.iter().enumerate() {
        let Some(f) = f else { continue };
        let Ok(of) = f.file.try_borrow() else {
            out.push_str(&format!("{{\"fd\":{},\"busy\":true}}\n", i));
            continue;
        };
        let (r, w, h) = of.readiness();
        out.push_str(&format!("{{\"fd\":{},\"what\":\"{}\",\"r\":{},\"w\":{},\"hup\":{}", i, esc(&of.describe()), r, w, h));
        if let Some(n) = of.pending() {
            out.push_str(&format!(",\"pending\":{}", n));
        }
        out.push_str("}\n");
    }
    Ok(out)
}

/// /proc/ai/mem: 1 行の JSON。数はページ (page = 4096 バイト) か、バイト (ヒープ、vsize、ファイルの大きさ)
pub fn mem_json() -> String {
    use core::sync::atomic::Ordering::Relaxed;
    let pg = crate::memlayout::PGSIZE;
    let (hsize, hused, hpeak, hblocks, hbig, hfree, hfails) = crate::heap::stats();
    let (hgrown, hreclaims, hreserve, hemerg) = crate::heap::stats_more();
    let (tino, tino_limit) = crate::tmpfs::inodes();
    let (swt, swf) = crate::swap::totals();
    let (tmp, tmp_limit) = crate::tmpfs::stats();
    let (shp, shd) = crate::vm::shared_stats();
    // メモリを多く使っているプロセス (rss の多い順に 10。アドレス空間を分けているスレッドは 1 つ)
    let mut procs: alloc::vec::Vec<(u32, String, usize, usize)> = alloc::vec::Vec::new();
    let mut seen: alloc::vec::Vec<usize> = alloc::vec::Vec::new();
    for p in crate::proc::all_leader_procs() {
        let Some(mm) = p.mm.as_ref() else { continue };
        if seen.contains(&mm.id()) {
            continue;
        }
        seen.push(mm.id());
        let m = mm.get();
        let name = core::str::from_utf8(&p.comm).unwrap_or("").trim_end_matches('\0').into();
        let _h = m.pt.hold();
        procs.push((p.tgid, name, m.pt.resident(), m.pt.vsize()));
    }
    procs.sort_by_key(|x| core::cmp::Reverse(x.2));
    procs.truncate(10);
    // 空きページの、大きさ (2^k ページ) ごとのかたまりの数 (buddy: kalloc.rs) と、まだまとめていない 1 枚ずつのもの
    let (blocks, hot) = crate::kalloc::orders();
    let procs: alloc::vec::Vec<String> = procs.iter().map(|(pid, name, rss, vs)| format!("{{\"pid\":{},\"name\":\"{}\",\"rss\":{},\"vsize\":{}}}", pid, esc(name), rss, vs)).collect();
    format!(
        "{{\"page\":{},\"ram\":{{\"total\":{},\"free\":{},\"min_free\":{},\"blocks\":[{}],\"hot\":{}}},\"kernel\":{{\"heap\":{{\"size\":{},\"used\":{},\"peak\":{},\"free\":{},\"free_blocks\":{},\"largest_free\":{},\"fails\":{},\"grown\":{},\"reclaims\":{},\"reserve_used\":{},\"emergencies\":{},\"oom_kills\":{}}},\"stacks\":{}}},\"page_cache\":{},\"shared\":{{\"pages\":{},\"dirty\":{}}},\"tmpfs\":{{\"pages\":{},\"limit\":{},\"inodes\":{},\"inodes_limit\":{}}},\"swap\":{{\"total\":{},\"used\":{},\"in\":{},\"out\":{}}},\"faults\":{{\"all\":{},\"fast\":{}}},\"procs\":[{}]}}\n",
        pg,
        crate::memlayout::ram_size() / pg,
        crate::kalloc::nfree(),
        crate::sysctl::MIN_FREE_PAGES.load(Relaxed),
        blocks.iter().map(|n| format!("{}", n)).collect::<alloc::vec::Vec<_>>().join(","),
        hot,
        hsize,
        hused,
        hpeak,
        hfree,
        hblocks,
        hbig,
        hfails,
        hgrown,
        hreclaims,
        hreserve,
        hemerg,
        crate::oom::kills(),
        crate::proc::kstack_bytes(),
        crate::vm::page_cache_json(),
        shp,
        shd,
        tmp,
        tmp_limit,
        tino,
        tino_limit,
        swt,
        swt - swf,
        crate::swap::PSWPIN.load(Relaxed),
        crate::swap::PSWPOUT.load(Relaxed),
        crate::smp::FAULTS.load(Relaxed),
        crate::smp::FAST_FAULTS.load(Relaxed),
        procs.join(",")
    )
}

/// /proc/ai/fs: 1 行の JSON
pub fn fs_json() -> String {
    // 開いているファイル (fork や dup で分けているものは 1 つ) と、fd の表を持つプロセスの数
    let mut seen: alloc::collections::BTreeSet<usize> = alloc::collections::BTreeSet::new();
    let mut nprocs = 0;
    for p in crate::proc::all_leader_procs() {
        let Some(files) = p.files.as_ref() else { continue };
        nprocs += 1;
        for f in files.get().fds.iter().flatten() {
            seen.insert(alloc::rc::Rc::as_ptr(&f.file) as *const u8 as usize);
        }
    }
    // 区画ごとの読み書き (0 はディスク全体。何もしていない区画は出さない)
    let mut disk = alloc::vec::Vec::new();
    let mut parts = crate::block::parts();
    if !parts.iter().any(|p| p.num == 0) {
        parts.insert(0, crate::block::Part { start: 0, len: 0, num: 0 });
    }
    for p in parts {
        let io = crate::block::io_stats(p.num);
        if p.num != 0 && io.iter().all(|&x| x == 0) {
            continue;
        }
        disk.push(format!(
            "{{\"part\":\"{}\",\"reads\":{},\"read_sectors\":{},\"read_ms\":{},\"writes\":{},\"write_sectors\":{},\"write_ms\":{}}}",
            esc(&crate::block::part_name(&p)),
            io[0],
            io[1],
            io[2],
            io[3],
            io[4],
            io[5]
        ));
    }
    format!(
        "{{\"mounts\":{},\"paths\":{},\"files\":{{\"open\":{},\"procs\":{}}},\"locks\":{{\"held\":{},\"waiting\":{}}},\"disk\":[{}]}}\n",
        crate::vfs::mounts_json(),
        crate::vfs::paths_json(),
        seen.len(),
        nprocs,
        crate::locks::stats().0,
        crate::locks::stats().1,
        disk.join(",")
    )
}

pub const CTL_HELP: &str = "kick PID\nraw TID\npcmiss on|off\nstrace NAME\nbkl reset\nbkl selftest\nheap trace on|off\nheap check\nheap hog N\npage cycle\npage check\nprof on|off\nlockbench\ntrace on [NAME]|off|clear\npart NAME off|on\nfail pid N|name NAME sys NR|all ERRNO [TIMES] | heap [TIMES] | oom [TIMES] | page [TIMES]\nfail off\npanic [TEXT]\n";

/// /proc/ai/ctl に書いたもの (root と wheel だけ。呼ぶ側で allowed を確かめる)
pub fn ctl(b: &[u8]) -> Result<(), i64> {
    let s = core::str::from_utf8(b).map_err(|_| crate::why::inval())?.trim();
    let (cmd, arg) = s.split_once(' ').map_or((s, ""), |(c, a)| (c, a.trim()));
    let num = || arg.parse::<u32>().map_err(|_| crate::why::inval());
    match cmd {
        "kick" => {
            let n = crate::proc::futex_kick(num()?);
            crate::println!("futex kick {}: woke {}", arg, n);
        }
        "raw" => crate::proc::raw_dump(num()?),
        "pcmiss" => crate::vm::MISS_LOG.store(arg != "off", core::sync::atomic::Ordering::Relaxed),
        "strace" => crate::syscall::strace_set(arg.as_bytes()),
        "bkl" if arg == "reset" => crate::smp::stats_reset(),
        "bkl" if arg == "selftest" => crate::cell::selftest(),
        "heap" if arg == "trace on" || arg == "trace off" => crate::heap::trace(arg == "trace on"),
        "heap" if arg.starts_with("hog ") => {
            crate::heap::hog(arg[4..].trim().parse::<usize>().map_err(|_| crate::why::inval())?);
        }
        "heap" if arg == "check" => crate::heap::check(),
        "page" if arg == "cycle" => {
            let n = crate::kalloc::cycle();
            crate::println!("page cycle: {} pages taken and given back ({} free)", n, crate::kalloc::nfree());
        }
        "page" if arg == "check" => crate::kalloc::check(),
        "prof" if arg == "on" || arg == "off" => crate::prof::set(arg == "on"),
        "lockbench" => crate::spinlock::bench(),
        "trace" => crate::trace::ctl(arg)?,
        "fail" => crate::fail::ctl(arg)?,
        "part" => {
            let (name, how) = arg.split_once(' ').ok_or_else(|| crate::why::inval())?;
            let on = match how.trim() {
                "on" => true,
                "off" => false,
                _ => return Err(crate::why::inval()),
            };
            crate::parts::set(name.trim(), on)?;
        }
        "panic" => {
            const EPERM: i64 = 1;
            if crate::proc::current().cred.euid != 0 {
                return Err(-EPERM);
            }
            panic!("/proc/ai/ctl: {}", if arg.is_empty() { "panic" } else { arg });
        }
        _ => return Err(crate::why::inval()),
    }
    Ok(())
}

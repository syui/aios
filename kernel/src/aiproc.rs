// /proc/ai: AI (aish の MCP のツール) が読むための、カーネルの中の様子。どれも 1 行 1 つの JSON。
// 読み書きできるのは root と wheel の人だけ (allowed)。
//   threads   すべてのスレッド: 状態、最後のシステムコール、眠っていれば何を (futex / 見張っている fd / channel)、
//             どれだけ (slept_s) 待っているか (proc::ai_threads)
//   bkl       大きなロックの統計 (smp::stats_json)。ctl に bkl reset で 0 から
//   fd/PID    そのプロセスの fd: 何か、読める・書ける・閉じた、読まれずに残っているもの
//   ctl       調べもののスイッチ (書く。読むと書けるものの一覧):
//               kick PID      その PID の futex で眠っているスレッドをみな起こす (起こしが消えたのかを確かめる。
//                             futex はわけもなく起きてよいので、こわれない)
//               raw TID       そのスレッドのレジスタとスタック (32 KiB) を kmsg に (外で .eh_frame でたどる)
//               pcmiss on|off ページキャッシュに無かったものを kmsg に
//               strace NAME   /proc/strace と同じ (+NAME でうまくいったものも、空で止める)
//               bkl reset     /proc/bkl を 0 から
use alloc::format;
use alloc::string::String;

/// wheel (sudo できる人。aiosd と同じ)
const WHEEL: u32 = 10;

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

pub const CTL_HELP: &str = "kick PID\nraw TID\npcmiss on|off\nstrace NAME\nbkl reset\n";

/// /proc/ai/ctl に書いたもの (root と wheel だけ。呼ぶ側で allowed を確かめる)
pub fn ctl(b: &[u8]) -> Result<(), i64> {
    const EINVAL: i64 = 22;
    let s = core::str::from_utf8(b).map_err(|_| -EINVAL)?.trim();
    let (cmd, arg) = s.split_once(' ').map_or((s, ""), |(c, a)| (c, a.trim()));
    let num = || arg.parse::<u32>().map_err(|_| -EINVAL);
    match cmd {
        "kick" => {
            let n = crate::proc::futex_kick(num()?);
            crate::println!("futex kick {}: woke {}", arg, n);
        }
        "raw" => crate::proc::raw_dump(num()?),
        "pcmiss" => crate::vm::MISS_LOG.store(arg != "off", core::sync::atomic::Ordering::Relaxed),
        "strace" => crate::syscall::strace_set(arg.as_bytes()),
        "bkl" if arg == "reset" => crate::smp::stats_reset(),
        _ => return Err(-EINVAL),
    }
    Ok(())
}

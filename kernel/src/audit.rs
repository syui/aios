// 監査 (audit: Linux の kernel/audit.c)。NETLINK_AUDIT のソケットで、auditd や auditctl が監査の設定を読み書きし
// (AUDIT_GET / AUDIT_SET など)、login や sudo がユーザーの記録 (AUDIT_USER_*) を送る。記録 (AppArmor の type=1400 も
// ここを通る) は、auditd (AUDIT_SET の pid で名乗ったプロセスのソケット) がいればそこへ、いなければ kmsg へ
// "audit: type=N audit(秒.ミリ秒:番号): ..." (dmesg で見える)。番号は記録みなで 1 つの数え
//
// - 権限 (audit_netlink_ok): 設定のもの (GET、SET、FEATURE、ルール、SIGNAL_INFO、TTY) は CAP_AUDIT_CONTROL、ユーザーの
//   記録は CAP_AUDIT_WRITE (なければ EPERM)。はじめの user namespace の外からは ECONNREFUSED (PAM がログインを止め
//   ないように)、pid namespace の中から設定するのは EPERM。古い AUDIT_LIST / ADD / DEL は EOPNOTSUPP、知らないものは EINVAL
// - NLM_F_REQUEST がなくても受ける。しくじりか NLM_F_ACK なら ACK。答え (GET など) は ACK のあと (Linux は答えを別の
//   スレッドから送る)
// - AUDIT_SET: enabled (0、1、2 はもう変えられない)、failure (0 だまる、1 printk、2 panic)、pid (auditd になる・やめる)、
//   rate_limit、backlog_limit、backlog_wait_time の順。変えたものは AUDIT_CONFIG_CHANGE (1305) に残す。残すのは audit が
//   on のときだけ (on にしたときは前が off なので残らない。off にしたときは残る)。mask が LOST だけなら、なくした数を 0 に
//   して、その数を ACK のエラーの欄で返す (Linux の古い形)
// - auditd: 名乗れるのは自分の pid だけ (ほかは EINVAL)。いる auditd を別のものにはできない (EEXIST)、やめられるのは
//   その auditd だけ (EACCES)。名乗るたびに、いまの auditd に AUDIT_REPLACE (名乗ったものの pid) を送ってつながりを
//   たしかめる (送れなければいないことに)。auditd への記録は nlmsg_len が中身だけの長さ (Linux の古い形: 頭の 16 バイトを
//   数えない)。組 AUDIT_NLGRP_READLOG (1。入るのは CAP_AUDIT_READ) には、ふつうの長さで。組に入る・出るのも記録に
//   (AUDIT_EVENT_LISTENER: nl-mcgrp=N op=connect / disconnect)
// - 受け口の中で出た記録は、その答えと ACK のあとに出す (Linux は kauditd のスレッドが行列から送る)
// - kmsg へは 5 秒に 10 こまで (printk_ratelimit: こえたものは捨てて、次の 5 秒のはじめに
//   "kauditd_printk_skb: N callbacks suppressed")
// - ユーザーの記録は audit が off なら捨てる (AUDIT_USER_AVC だけは off でも)。1 バイトより短いものは EINVAL、終わりの
//   1 バイトは NUL にする (msg='...' に出るのはそのまえまで)
// - AUDIT_SIGNAL_INFO: auditd に SIGTERM / SIGHUP / SIGUSR1 / SIGUSR2 を送った最後のもの (uid、pid、ラベル: unconfined は空)
// - loginuid と sessionid (/proc/PID/loginuid と sessionid、記録の auid= ses=): 書けるのは自分だけ (set_loginuid)
// - システムコールの入れもの (Ctx: audit を一度 on にしたあとに作ったスレッド): そのスレッドのことの記録 (log_task) を
//   出したシステムコールは、出口で SYSCALL、PROCTITLE、EOE も (Linux 7.4 は記録が出ると dummy をやめる)
// - のこり: ルール (auditctl -a、システムコールのフィルタ。AUDIT_ADD_RULE / DEL_RULE / MAKE_EQUIV は EOPNOTSUPP)、
//   PATH と CWD の記録、TTY の入力の記録 (TTY_SET は覚えるだけ)、backlog (記録はすぐ出すので行列がない)
use crate::cell::Bkl;
use crate::netlink::{self, NetlinkRef};
use crate::proc;
use crate::spinlock::SpinLock;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};

pub const NETLINK_AUDIT: u32 = 9;
/// 記録の写しを受ける組 (AUDIT_NLGRP_READLOG。ほかの組 (32 まで) にも入れるが、なにも来ない)
const NLGRP_READLOG: u32 = 1;

const EPERM: i64 = 1;
const EACCES: i64 = 13;
const EEXIST: i64 = 17;
const EINVAL: i64 = 22;
const EOPNOTSUPP: i64 = 95;
const ECONNREFUSED: i64 = 111;

const NLMSG_HDRLEN: usize = 16;
const NLMSG_DONE: u16 = 3;
const NLM_F_MULTI: u16 = 2;
const NLM_F_ACK: u16 = 4;

// メッセージの種類 (include/uapi/linux/audit.h)
const AUDIT_GET: u16 = 1000;
const AUDIT_SET: u16 = 1001;
const AUDIT_LIST: u16 = 1002;
const AUDIT_ADD: u16 = 1003;
const AUDIT_DEL: u16 = 1004;
const AUDIT_USER: u16 = 1005;
const AUDIT_LOGIN: u16 = 1006;
const AUDIT_SIGNAL_INFO: u16 = 1010;
const AUDIT_ADD_RULE: u16 = 1011;
const AUDIT_DEL_RULE: u16 = 1012;
const AUDIT_LIST_RULES: u16 = 1013;
const AUDIT_TRIM: u16 = 1014;
const AUDIT_MAKE_EQUIV: u16 = 1015;
const AUDIT_TTY_GET: u16 = 1016;
const AUDIT_TTY_SET: u16 = 1017;
const AUDIT_SET_FEATURE: u16 = 1018;
const AUDIT_GET_FEATURE: u16 = 1019;
const AUDIT_USER_AVC: u16 = 1107;
const AUDIT_USER_TTY: u16 = 1124;
const AUDIT_SYSCALL: u16 = 1300;
const AUDIT_EOE: u16 = 1320;
const AUDIT_PROCTITLE: u16 = 1327;
const AUDIT_CONFIG_CHANGE: u16 = 1305;
const AUDIT_FEATURE_CHANGE: u16 = 1328;
const AUDIT_REPLACE: u16 = 1329;
const AUDIT_EVENT_LISTENER: u16 = 1335;
const AUDIT_KERNEL: u16 = 2000;
/// AppArmor などの LSM の記録
pub const AUDIT_AVC: u16 = 1400;

// AUDIT_SET の mask
const STATUS_ENABLED: u32 = 0x1;
const STATUS_FAILURE: u32 = 0x2;
const STATUS_PID: u32 = 0x4;
const STATUS_RATE_LIMIT: u32 = 0x8;
const STATUS_BACKLOG_LIMIT: u32 = 0x10;
const STATUS_BACKLOG_WAIT_TIME: u32 = 0x20;
const STATUS_LOST: u32 = 0x40;
const STATUS_BACKLOG_WAIT_TIME_ACTUAL: u32 = 0x80;

const OFF: u32 = 0;
const LOCKED: u32 = 2;
const FAIL_SILENT: u32 = 0;
const FAIL_PRINTK: u32 = 1;
const FAIL_PANIC: u32 = 2;
/// AUDIT_FEATURE_BITMAP_ALL (backlog_limit、backlog_wait_time、executable_path、exclude_extend、sessionid_filter、
/// lost_reset、filter_fs)
const FEATURE_BITMAP_ALL: u32 = 0x7f;
/// AUDIT_BACKLOG_WAIT_TIME (60 * HZ、HZ は 250)
const BACKLOG_WAIT_TIME: u32 = 15000;
/// AUDIT_FEATURE_VERSION と名前 (AUDIT_FEATURE_ONLY_UNSET_LOGINUID、AUDIT_FEATURE_LOGINUID_IMMUTABLE)
const FEATURE_VERSION: u32 = 1;
const FEATURE_NAMES: [&str; 2] = ["only_unset_loginuid", "loginuid_immutable"];
const TTY_ENABLE: u32 = 1;
const TTY_LOG_PASSWD: u32 = 2;
/// AUDIT_MESSAGE_TEXT_MAX
const TEXT_MAX: usize = 8560;
/// まだ決まっていない loginuid と session (AUDIT_UID_UNSET、AUDIT_SID_UNSET)
const UNSET: u32 = u32::MAX;

struct State {
    enabled: u32,
    failure: u32,
    /// auditd の tgid とソケットのポート (0 はいない)
    pid: u32,
    portid: u32,
    rate_limit: u32,
    backlog_limit: u32,
    backlog_wait_time: u32,
    lost: u32,
    /// AUDIT_SET_FEATURE の機能と、もう変えられないもの (ビット)
    features: u32,
    lock: u32,
    /// auditd にシグナルを送った最後のもの (uid、tgid、ラベル)
    sig_uid: u32,
    sig_pid: i32,
    sig_ctx: String,
    /// rate_limit の数え (いまの秒と、その秒に出した数) と、なくしたことを最後に知らせた秒
    rate_sec: u64,
    rate_n: u32,
    lost_sec: u64,
}

static ST: Bkl<State> = Bkl::new(State {
    enabled: OFF,
    failure: FAIL_PRINTK,
    pid: 0,
    portid: 0,
    rate_limit: 0,
    backlog_limit: 64,
    backlog_wait_time: BACKLOG_WAIT_TIME,
    lost: 0,
    features: 0,
    lock: 0,
    sig_uid: UNSET,
    sig_pid: -1,
    sig_ctx: String::new(),
    rate_sec: 0,
    rate_n: 0,
    lost_sec: 0,
});

/// 記録の番号 (audit_serial: 1 から)
static SERIAL: AtomicU32 = AtomicU32::new(1);

/// audit を一度でも on にしたか (audit_ever_enabled: このあとに作るスレッドには Ctx をつける)
static EVER: AtomicBool = AtomicBool::new(false);

/// 受け口の中で出た記録 (種類と文字。答えと ACK のあとに出す)。None は受け口の外
static DEFER: Bkl<Option<Vec<(u16, String)>>> = Bkl::new(None);

/// printk_ratelimit (5 秒に 10 こ): 窓のはじめ (起動からの ns)、窓の中で出した数、捨てた数
struct Ratelimit {
    begin: u64,
    printed: u32,
    missed: u32,
}
static RATELIMIT: SpinLock<Ratelimit> = SpinLock::new(Ratelimit { begin: 0, printed: 0, missed: 0 });
const RATELIMIT_NS: u64 = 5_000_000_000;
const RATELIMIT_BURST: u32 = 10;

fn st() -> &'static mut State {
    ST.get()
}

/// 起動のとき (audit_init): kmsg に 2 行
pub fn init() {
    crate::println!("audit: initializing netlink subsys (disabled)");
    log(AUDIT_KERNEL, "state=initialized audit_enabled=0 res=1");
}

// ---- 記録 ----

/// 記録を出す (audit_log_start(NULL, ...) から audit_log_end): body は "audit(...): " のうしろ。受け口の中ならあとで
pub fn log(typ: u16, body: &str) {
    let ns = crate::timer::epoch_ns();
    put(typ, ns, SERIAL.fetch_add(1, Relaxed), body);
}

/// いまのスレッドのことの記録 (audit_log_start(audit_context(), ...)): システムコールの中なら、その時刻と番号で、
/// そのシステムコールの終わりに SYSCALL の記録も出す (auditsc_get_stamp と「dummy をやめる」)
pub fn log_task(typ: u16, body: &str) {
    let locked = crate::smp::holding() || crate::smp::online() <= 1;
    let stamp = match proc::try_current().and_then(|p| p.audit.as_mut()).filter(|_| locked) {
        Some(c) => {
            c.dummy = false;
            if c.in_syscall {
                if c.serial == 0 {
                    c.serial = SERIAL.fetch_add(1, Relaxed);
                }
                if !c.prio {
                    (c.prio, c.record) = (true, true);
                }
                Some((c.ctime, c.serial))
            } else {
                None
            }
        }
        None => None,
    };
    match stamp {
        Some((ns, serial)) => put(typ, ns, serial, body),
        None => log(typ, body),
    }
}

/// 時刻と番号をつけて出す
fn put(typ: u16, ns: u64, serial: u32, body: &str) {
    let text = format!("audit({}.{:03}:{}): {}", ns / 1_000_000_000, ns / 1_000_000 % 1000, serial, body);
    // 大きなロックの外 (ほかの CPU が動いているとき) からは、状態もソケットも触らずに kmsg へ
    if !crate::smp::holding() && crate::smp::online() > 1 {
        printk(typ, &text);
        return;
    }
    if let Some(d) = DEFER.get() {
        d.push((typ, text));
        return;
    }
    emit(typ, text);
}

/// 記録を送る (kauditd_send_queue): 組 READLOG に入っているソケットへ、それから auditd がいればそこへ、いなければ kmsg へ
fn emit(typ: u16, text: String) {
    if !rate_ok() {
        log_lost("rate limit exceeded");
        return;
    }
    if netlink::has_listeners(NETLINK_AUDIT, NLGRP_READLOG) {
        netlink::notify_proto(NETLINK_AUDIT, NLGRP_READLOG, frame(typ, &text, true));
    }
    let portid = st().portid;
    if portid != 0 {
        if netlink::unicast(NETLINK_AUDIT, portid, frame(typ, &text, false)) {
            return;
        }
        // auditd のソケットはもうない (auditd_reset)
        reset();
    }
    printk(typ, &text);
}

/// kmsg へ (kauditd_printk_skb: 5 秒に 10 こまで。AUDIT_EOE は出さない)
fn printk(typ: u16, text: &str) {
    if typ != AUDIT_EOE && ratelimit("kauditd_printk_skb") {
        crate::println!("audit: type={} {}", typ, text);
    }
}

/// printk_ratelimit (___ratelimit): 窓が過ぎていれば、捨てた数を知らせて新しい窓に
fn ratelimit(func: &str) -> bool {
    let now = crate::timer::uptime_ns();
    let (ok, missed) = {
        let mut r = RATELIMIT.lock();
        if r.begin == 0 {
            r.begin = now;
        }
        let mut missed = 0;
        if now > r.begin + RATELIMIT_NS {
            missed = core::mem::take(&mut r.missed);
            (r.begin, r.printed) = (now, 0);
        }
        let ok = r.printed < RATELIMIT_BURST;
        if ok {
            r.printed += 1;
        } else {
            r.missed += 1;
        }
        (ok, missed)
    };
    if missed > 0 {
        crate::println!("{}: {} callbacks suppressed", func, missed);
    }
    ok
}

/// 記録のデータグラム。full でなければ nlmsg_len は中身だけ (auditd へのもの)
fn frame(typ: u16, text: &str, full: bool) -> Vec<u8> {
    let len = text.len() + if full { NLMSG_HDRLEN } else { 0 };
    let mut m = Vec::with_capacity(NLMSG_HDRLEN + text.len());
    m.extend_from_slice(&(len as u32).to_le_bytes());
    m.extend_from_slice(&typ.to_le_bytes());
    m.extend_from_slice(&[0; 10]);
    m.extend_from_slice(text.as_bytes());
    m
}

/// rate_limit (1 秒にいくつまで。0 はかぎりなし)
fn rate_ok() -> bool {
    let s = st();
    if s.rate_limit == 0 {
        return true;
    }
    let sec = crate::timer::uptime_ns() / 1_000_000_000;
    if sec != s.rate_sec {
        (s.rate_sec, s.rate_n) = (sec, 0);
    }
    s.rate_n += 1;
    s.rate_n <= s.rate_limit
}

/// 記録をなくした (audit_log_lost): 数えて、failure にしたがって知らせる (rate_limit があれば 1 秒に 1 度)
fn log_lost(why: &str) {
    let s = st();
    s.lost += 1;
    let sec = crate::timer::uptime_ns() / 1_000_000_000;
    if s.failure != FAIL_PANIC && s.rate_limit != 0 {
        if s.lost_sec == sec {
            return;
        }
        s.lost_sec = sec;
    }
    if ratelimit("audit_log_lost") {
        crate::println!("audit: audit_lost={} audit_rate_limit={} audit_backlog_limit={}", s.lost, s.rate_limit, s.backlog_limit);
    }
    match s.failure {
        FAIL_PRINTK if ratelimit("audit_panic") => crate::println!("audit: {}", why),
        FAIL_PANIC => panic!("audit: {}", why),
        _ => {}
    }
}

/// auditd をいないことに
fn reset() {
    let s = st();
    (s.pid, s.portid) = (0, 0);
}

/// "auid=... ses=..." (audit_log_session_info: いまのプロセスの loginuid と sessionid)
fn session() -> String {
    let c = &proc::current().cred;
    format!("auid={} ses={}", c.loginuid, c.sessionid)
}

/// " subj=ラベル" (audit_log_task_context: AppArmor がなければなし)
fn subj() -> String {
    crate::apparmor::secctx().map_or(String::new(), |c| format!(" subj={}", c))
}

/// ユーザーからのもの・設定の記録の頭 (audit_log_common_recv_msg): "pid=… uid=… auid=… ses=… subj=…"
fn recv_head() -> String {
    let p = proc::current();
    format!("pid={} uid={} {}{}", p.tgid, p.cred.uid, session(), subj())
}

/// いまのプロセスの comm と exe (audit_log_untrustedstring と audit_log_d_path_exe: mm がなければ (null))
fn comm_exe() -> (String, String) {
    let p = proc::current();
    let n = p.comm.iter().position(|&x| x == 0).unwrap_or(p.comm.len());
    let exe = if p.mm.is_some() { untrusted(format!("/{}", p.mm().exe).as_bytes()) } else { "(null)".into() };
    (untrusted(&p.comm[..n]), exe)
}

/// 組に入った・出た (audit_log_multicast: AUDIT_EVENT_LISTENER。audit が on のときだけ)。netlink.rs の bind と
/// NETLINK_ADD_MEMBERSHIP / DROP_MEMBERSHIP、閉じるときから
pub fn multicast(group: u32, op: &str, ok: bool) {
    if (!crate::smp::holding() && crate::smp::online() > 1) || st().enabled == OFF {
        return;
    }
    let p = proc::current();
    let (comm, exe) = comm_exe();
    let body = format!(
        "pid={} uid={} auid={} tty={} ses={}{} comm={} exe={} nl-mcgrp={} op={} res={}",
        p.tgid,
        p.cred.uid,
        p.cred.loginuid,
        crate::tty::audit_name(),
        p.cred.sessionid,
        subj(),
        comm,
        exe,
        group,
        op,
        ok as u32
    );
    log_task(AUDIT_EVENT_LISTENER, &body);
}

/// audit_log_task_info: " ppid=… pid=… auid=… uid=… … tty=… ses=… comm=… exe=… subj=…"
fn task_info() -> String {
    let (comm, exe) = comm_exe();
    let p = proc::current();
    let c = &p.cred;
    format!(
        " ppid={} pid={} auid={} uid={} gid={} euid={} suid={} fsuid={} egid={} sgid={} fsgid={} tty={} ses={} comm={} exe={}{}",
        p.ppid,
        p.tgid,
        c.loginuid,
        c.uid,
        c.gid,
        c.euid,
        c.suid,
        c.euid,
        c.egid,
        c.sgid,
        c.egid,
        crate::tty::audit_name(),
        c.sessionid,
        comm,
        exe,
        subj()
    )
}

/// audit_log_untrustedstring: 引用符でかこむ。空白や " や 7bit の外の文字があれば 16 進で
fn untrusted(s: &[u8]) -> String {
    if s.iter().any(|&c| c == b'"' || !(0x21..=0x7e).contains(&c)) {
        s.iter().map(|c| format!("{:02X}", c)).collect()
    } else {
        format!("\"{}\"", String::from_utf8_lossy(s))
    }
}

/// 設定を変えた記録 (audit_log_config_change)
fn config_change(name: &str, new: u32, old: u32, res: bool) {
    log_task(AUDIT_CONFIG_CHANGE, &format!("op=set {}={} old={} {}{} res={}", name, new, old, session(), subj(), res as u32));
}

/// audit_do_config_change: on なら残し、固定 (enabled 2) でなければ変える (固定なら EPERM)
fn do_config_change(name: &str, field: fn(&mut State) -> &mut u32, new: u32) -> Result<(), i64> {
    let (enabled, old) = (st().enabled, *field(st()));
    let allow = enabled != LOCKED;
    if enabled != OFF {
        config_change(name, new, old, allow);
    }
    if !allow {
        return Err(-EPERM);
    }
    *field(st()) = new;
    Ok(())
}

// ---- システムコールの入れもの (auditsc.c の audit_context) ----

/// スレッドごとの入れもの (Linux の struct audit_context のうち使うもの)。システムコールの入口で番号と引数と時刻を
/// 覚え、そのあいだに出たこのスレッドの記録 (log_task) があれば、出口で SYSCALL と PROCTITLE と EOE の記録を出す
pub struct Ctx {
    /// システムコールの中 (AUDIT_CTX_SYSCALL)
    in_syscall: bool,
    /// 出口で出すものがない (ルールがなく、まだ記録もない)
    dummy: bool,
    /// 出口で出す (AUDIT_STATE_RECORD) と、それを決めたか (prio)
    record: bool,
    prio: bool,
    major: u64,
    argv: [u64; 4],
    /// 入口の時刻と、記録の番号 (0 はまだ)
    ctime: u64,
    serial: u32,
    /// PROCTITLE の中身 (はじめて出すときに作り、このスレッドのあいだ使う: exec しても変わらない。Linux と同じ)
    proctitle: Option<Vec<u8>>,
}

/// fork で作るスレッドの入れもの (audit_alloc: audit を一度も on にしていなければ作らない)
pub fn alloc() -> Option<alloc::boxed::Box<Ctx>> {
    if !EVER.load(Relaxed) {
        return None;
    }
    Some(alloc::boxed::Box::new(Ctx { in_syscall: false, dummy: true, record: false, prio: false, major: 0, argv: [0; 4], ctime: 0, serial: 0, proctitle: None }))
}

/// システムコールの入口 (__audit_syscall_entry): audit が on なら、番号と引数と時刻を覚える
pub fn syscall_entry(nr: u64, a: &[u64; 6]) {
    if st().enabled == OFF {
        return;
    }
    let Some(c) = proc::current().audit.as_mut() else { return };
    // ルールはまだないので、いつも記録が出るまで dummy (audit_n_rules が 0)
    c.dummy = true;
    c.major = nr;
    c.argv = [a[0], a[1], a[2], a[3]];
    c.in_syscall = true;
    c.record = false;
    c.ctime = crate::timer::epoch_ns() / 1_000_000 * 1_000_000;
}

/// システムコールの出口 (__audit_syscall_exit): 記録を出すことになっていれば出し、入れものを空にする
pub fn syscall_exit(ret: i64) {
    let Some(c) = proc::current().audit.as_mut() else { return };
    let emit = c.in_syscall && !c.dummy && c.record;
    c.in_syscall = false;
    if emit {
        log_exit(ret);
    }
    let Some(c) = proc::current().audit.as_mut() else { return };
    if !c.dummy {
        (c.record, c.prio, c.serial, c.ctime, c.major, c.argv) = (false, false, 0, 0, 0, [0; 4]);
    }
}

/// audit_log_exit: SYSCALL、PROCTITLE、EOE (同じ時刻と番号で)
fn log_exit(ret: i64) {
    const ERESTARTSYS: i64 = 512;
    const ERESTART_RESTARTBLOCK: i64 = 516;
    const ENOIOCTLCMD: i64 = 515;
    const EINTR: i64 = 4;
    let code = if (-ERESTART_RESTARTBLOCK..=-ERESTARTSYS).contains(&ret) && ret != -ENOIOCTLCMD { -EINTR } else { ret };
    let success = !(-4095..0).contains(&ret);
    let (major, argv, ns, serial) = {
        let c = proc::current().audit.as_mut().unwrap();
        if c.serial == 0 {
            c.serial = SERIAL.fetch_add(1, Relaxed);
        }
        (c.major, c.argv, c.ctime, c.serial)
    };
    let body = format!(
        "arch={:x} syscall={} success={} exit={} a0={:x} a1={:x} a2={:x} a3={:x} items=0{} key=(null)",
        crate::seccomp::AUDIT_ARCH_AARCH64,
        major,
        if success { "yes" } else { "no" },
        code,
        argv[0],
        argv[1],
        argv[2],
        argv[3],
        task_info()
    );
    put(AUDIT_SYSCALL, ns, serial, &body);
    put(AUDIT_PROCTITLE, ns, serial, &format!("proctitle={}", proctitle()));
    put(AUDIT_EOE, ns, serial, "");
}

/// PROCTITLE の中身: 引数の場所の 128 バイトまで、終わりの表示できない文字を落として (audit_proctitle_rtrim)。
/// 読めなければ (null)
fn proctitle() -> String {
    const MAX: usize = 128;
    let p = proc::current();
    if p.audit.as_ref().is_some_and(|c| c.proctitle.is_none()) {
        let b = p.mm.as_ref().and_then(|_| {
            let (a, e, _) = p.mm().args;
            if a == 0 || e <= a {
                return None;
            }
            let mut b = alloc::vec![0u8; (e - a).min(MAX)];
            let pt = &mut p.mm().pt;
            let _h = pt.hold();
            pt.copy_in(&mut b, a)?;
            Some(b)
        });
        if let Some(mut b) = b {
            let printable = |c: u8| (0x20..0x7f).contains(&c);
            while b.len() > 1 && !printable(b[b.len() - 1]) {
                b.pop();
            }
            if b.len() == 1 && !printable(b[0]) {
                b.clear();
            }
            if !b.is_empty() {
                p.audit.as_mut().unwrap().proctitle = Some(b);
            }
        }
    }
    match p.audit.as_ref().and_then(|c| c.proctitle.as_ref()) {
        Some(b) => untrusted(b),
        None => untrusted(b"(null)"),
    }
}

// ---- loginuid と sessionid (auditsc.c の audit_set_loginuid、/proc/PID/loginuid) ----

/// セッションの番号 (session_id: 1 から。u32::MAX はとばす)
static SESSION: AtomicU32 = AtomicU32::new(0);

/// /proc/self/loginuid に書いた (proc_loginuid_write のあと): 決めるか (UNSET なら) 外す。決まっていなければだれでも、
/// 決まっていれば CAP_AUDIT_CONTROL (loginuid_immutable ならだれも、only_unset_loginuid なら外すだけ)。決めると
/// 新しいセッションの番号。audit が on なら AUDIT_LOGIN の記録
pub fn set_loginuid(new: u32) -> Result<(), i64> {
    let (old, old_ses) = {
        let c = &proc::current().cred;
        (c.loginuid, c.sessionid)
    };
    let features = st().features;
    let rc = if old == UNSET {
        Ok(())
    } else if features & 2 != 0 || !proc::current().cred.capable(crate::cred::CAP_AUDIT_CONTROL) || (features & 1 != 0 && new != UNSET) {
        Err(-EPERM)
    } else {
        Ok(())
    };
    let mut ses = UNSET;
    if rc.is_ok() {
        if new != UNSET {
            ses = SESSION.fetch_add(1, Relaxed).wrapping_add(1);
            if ses == UNSET {
                ses = SESSION.fetch_add(1, Relaxed).wrapping_add(1);
            }
        }
        let c = &mut proc::current().cred;
        (c.loginuid, c.sessionid) = (new, ses);
    }
    if st().enabled != OFF {
        let p = proc::current();
        let body = format!(
            "pid={} uid={}{} old-auid={} auid={} tty={} old-ses={} ses={} res={}",
            p.tgid,
            p.cred.uid,
            subj(),
            old,
            new,
            crate::tty::audit_name(),
            old_ses,
            ses,
            rc.is_ok() as u32
        );
        log_task(AUDIT_LOGIN, &body);
    }
    rc
}

// ---- シグナル (audit_signal_info) ----

/// tgid に sig が送られる: auditd への止める・読みなおすシグナルなら、送ったものを覚える
pub fn signal_info(tgid: u32, sig: i32) {
    const SIGHUP: i32 = 1;
    const SIGUSR1: i32 = 10;
    const SIGUSR2: i32 = 12;
    const SIGTERM: i32 = 15;
    let s = st();
    if s.pid == 0 || s.pid != tgid || !matches!(sig, SIGTERM | SIGHUP | SIGUSR1 | SIGUSR2) {
        return;
    }
    let p = proc::current();
    // loginuid があればそれ、なければ uid
    (s.sig_pid, s.sig_uid) = (p.tgid as i32, if p.cred.loginuid != UNSET { p.cred.loginuid } else { p.cred.uid });
    // ラベル (Linux 7.4 では unconfined のものは空になる)
    s.sig_ctx = crate::apparmor::secctx().filter(|c| c != "unconfined").unwrap_or_default();
}

// ---- NETLINK_AUDIT の受け口 (audit_receive) ----

/// 送られたメッセージをみな受ける。答えと ACK は n に (auditd への記録はそのソケットへ)。me は送ったソケットのポート
pub fn rcv(n: &NetlinkRef, data: &[u8], me: u32, cap_ack: bool) {
    let mut at = 0;
    while data.len() - at >= NLMSG_HDRLEN {
        let h = &data[at..];
        let len = u32::from_le_bytes(h[0..4].try_into().unwrap()) as usize;
        if len < NLMSG_HDRLEN || h.len() < len {
            break;
        }
        let msg = &h[..len];
        let flags = u16::from_le_bytes([msg[6], msg[7]]);
        let mut ack = flags & NLM_F_ACK != 0;
        let mut out = Vec::new();
        *DEFER.get() = Some(Vec::new());
        let err = receive(n, msg, me, cap_ack, &mut ack, &mut out);
        let records = DEFER.get().take().unwrap_or_default();
        if ack || err != 0 {
            netlink::deliver(n, 0, 0, netlink::ack(msg, err, cap_ack, false, None, None, me));
        }
        for d in out {
            netlink::deliver(n, 0, 0, d);
        }
        for (typ, text) in records {
            emit(typ, text);
        }
        at += ((len + 3) & !3).min(data.len() - at);
    }
}

/// 答えのデータグラム (audit_make_reply: ポート 0、seq は頼んだもの)
fn reply(out: &mut Vec<Vec<u8>>, typ: u16, flags: u16, seq: u32, payload: &[u8]) {
    let mut m = Vec::with_capacity(NLMSG_HDRLEN + payload.len());
    m.extend_from_slice(&((NLMSG_HDRLEN + payload.len()) as u32).to_le_bytes());
    m.extend_from_slice(&typ.to_le_bytes());
    m.extend_from_slice(&flags.to_le_bytes());
    m.extend_from_slice(&seq.to_le_bytes());
    m.extend_from_slice(&0u32.to_le_bytes());
    m.extend_from_slice(payload);
    m.resize((m.len() + 3) & !3, 0);
    out.push(m);
}

fn u32s(v: &[u32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

/// audit_netlink_ok: だれが送ってよいか
fn netlink_ok(typ: u16) -> Result<(), i64> {
    let p = proc::current();
    if p.cred.ns.user.is_some() {
        return Err(-ECONNREFUSED);
    }
    match typ {
        AUDIT_LIST | AUDIT_ADD | AUDIT_DEL => Err(-EOPNOTSUPP),
        AUDIT_GET | AUDIT_SET | AUDIT_GET_FEATURE | AUDIT_SET_FEATURE | AUDIT_LIST_RULES | AUDIT_ADD_RULE | AUDIT_DEL_RULE | AUDIT_SIGNAL_INFO
        | AUDIT_TTY_GET | AUDIT_TTY_SET | AUDIT_TRIM | AUDIT_MAKE_EQUIV => {
            if p.pid_ns.is_some() || !p.cred.capable(crate::cred::CAP_AUDIT_CONTROL) {
                return Err(-EPERM);
            }
            Ok(())
        }
        t if is_user(t) => {
            if !p.cred.capable(crate::cred::CAP_AUDIT_WRITE) {
                return Err(-EPERM);
            }
            Ok(())
        }
        _ => Err(-EINVAL),
    }
}

/// ユーザーの記録の種類 (AUDIT_USER、AUDIT_FIRST_USER_MSG から LAST、MSG2 の範囲)
fn is_user(t: u16) -> bool {
    t == AUDIT_USER || (1100..=1199).contains(&t) || (2100..=2999).contains(&t)
}

/// audit_receive_msg: 負の errno か 0 (LOST はなくした数)
fn receive(n: &NetlinkRef, msg: &[u8], me: u32, cap_ack: bool, ack: &mut bool, out: &mut Vec<Vec<u8>>) -> i64 {
    let typ = u16::from_le_bytes([msg[4], msg[5]]);
    let seq = u32::from_le_bytes(msg[8..12].try_into().unwrap());
    if let Err(e) = netlink_ok(typ) {
        return e;
    }
    let data = &msg[NLMSG_HDRLEN..];
    match typ {
        AUDIT_GET => {
            let s = st();
            let v = [0, s.enabled, s.failure, s.pid, s.rate_limit, s.backlog_limit, s.lost, 0, FEATURE_BITMAP_ALL, s.backlog_wait_time, 0];
            reply(out, AUDIT_GET, 0, seq, &u32s(&v));
            0
        }
        AUDIT_SET => match set(n, msg, me, cap_ack, ack) {
            Ok(v) => v,
            Err(e) => e,
        },
        AUDIT_GET_FEATURE => {
            let s = st();
            reply(out, AUDIT_GET_FEATURE, 0, seq, &u32s(&[FEATURE_VERSION, u32::MAX, s.features, s.lock]));
            0
        }
        AUDIT_SET_FEATURE => {
            if data.len() < 16 {
                return -EINVAL;
            }
            let w = |i: usize| u32::from_le_bytes(data[i * 4..i * 4 + 4].try_into().unwrap());
            set_feature(w(1), w(2), w(3)).err().unwrap_or(0)
        }
        t if is_user(t) => {
            user_msg(t, data);
            if st().enabled == OFF && t != AUDIT_USER_AVC {
                return 0;
            }
            if data.len() < 2 { -EINVAL } else { 0 }
        }
        AUDIT_ADD_RULE | AUDIT_DEL_RULE => {
            // struct audit_rule_data (1040 バイト) より短いものは EINVAL。固定なら EPERM (残す)
            if data.len() < 1040 {
                return -EINVAL;
            }
            if st().enabled == LOCKED {
                let op = if typ == AUDIT_ADD_RULE { "add_rule" } else { "remove_rule" };
                log_task(AUDIT_CONFIG_CHANGE, &format!("{} op={} audit_enabled={} res=0", recv_head(), op, LOCKED));
                return -EPERM;
            }
            -EOPNOTSUPP
        }
        AUDIT_LIST_RULES => {
            // ルールはないので NLMSG_DONE だけ
            reply(out, NLMSG_DONE, NLM_F_MULTI, seq, &[]);
            0
        }
        AUDIT_TRIM => {
            if st().enabled != OFF {
                log_task(AUDIT_CONFIG_CHANGE, &format!("{} op=trim res=1", recv_head()));
            }
            0
        }
        AUDIT_MAKE_EQUIV => -EOPNOTSUPP,
        AUDIT_SIGNAL_INFO => {
            let s = st();
            let mut v = u32s(&[s.sig_uid, s.sig_pid as u32]);
            v.extend_from_slice(s.sig_ctx.as_bytes());
            reply(out, AUDIT_SIGNAL_INFO, 0, seq, &v);
            0
        }
        AUDIT_TTY_GET => {
            let t = proc::current().cred.audit_tty;
            reply(out, AUDIT_TTY_GET, 0, seq, &u32s(&[t & TTY_ENABLE, (t & TTY_LOG_PASSWD != 0) as u32]));
            0
        }
        AUDIT_TTY_SET => {
            let w = |i: usize| data.get(i * 4..i * 4 + 4).map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()));
            let (en, pw) = (w(0), w(1));
            let ok = en <= 1 && pw <= 1;
            let cred = &mut proc::current().cred;
            let old = cred.audit_tty;
            if ok {
                cred.audit_tty = en | if pw != 0 { TTY_LOG_PASSWD } else { 0 };
            }
            if st().enabled != OFF {
                let body = format!(
                    "{} op=tty_set old-enabled={} new-enabled={} old-log_passwd={} new-log_passwd={} res={}",
                    recv_head(),
                    old & TTY_ENABLE,
                    en,
                    (old & TTY_LOG_PASSWD != 0) as u32,
                    pw,
                    ok as u32
                );
                log(AUDIT_CONFIG_CHANGE, &body);
            }
            if ok { 0 } else { -EINVAL }
        }
        _ => -EINVAL,
    }
}

/// ユーザーの記録 (audit が off なら AUDIT_USER_AVC だけ)
fn user_msg(typ: u16, data: &[u8]) {
    if (st().enabled == OFF && typ != AUDIT_USER_AVC) || data.len() < 2 {
        return;
    }
    let body = if typ != AUDIT_USER_TTY {
        // 終わりの 1 バイトを NUL に (そのまえの NUL までを出す)
        let d = &data[..data.len() - 1];
        let d = &d[..d.iter().position(|&c| c == 0).unwrap_or(d.len()).min(TEXT_MAX)];
        format!("{} msg='{}'", recv_head(), String::from_utf8_lossy(d))
    } else {
        let d = if data[data.len() - 1] == 0 { &data[..data.len() - 1] } else { data };
        format!("{} data={}", recv_head(), untrusted(d))
    };
    log(typ, &body);
}

/// AUDIT_SET (struct audit_status: 短ければ 0 で埋める)
fn set(n: &NetlinkRef, msg: &[u8], me: u32, cap_ack: bool, ack: &mut bool) -> Result<i64, i64> {
    let data = &msg[NLMSG_HDRLEN..];
    let w = |i: usize| data.get(i * 4..i * 4 + 4).map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()));
    let mask = w(0);
    if mask & STATUS_ENABLED != 0 {
        let v = w(1);
        if v > LOCKED {
            return Err(-EINVAL);
        }
        do_config_change("audit_enabled", |s| &mut s.enabled, v)?;
        if v != OFF {
            EVER.store(true, Relaxed);
        }
    }
    if mask & STATUS_FAILURE != 0 {
        let v = w(2);
        if !matches!(v, FAIL_SILENT | FAIL_PRINTK | FAIL_PANIC) {
            return Err(-EINVAL);
        }
        do_config_change("audit_failure", |s| &mut s.failure, v)?;
    }
    if mask & STATUS_PID != 0 {
        set_pid(n, msg, w(3), me, cap_ack, ack)?;
    }
    if mask & STATUS_RATE_LIMIT != 0 {
        do_config_change("audit_rate_limit", |s| &mut s.rate_limit, w(4))?;
    }
    if mask & STATUS_BACKLOG_LIMIT != 0 {
        do_config_change("audit_backlog_limit", |s| &mut s.backlog_limit, w(5))?;
    }
    if mask & STATUS_BACKLOG_WAIT_TIME != 0 {
        // 古い (短い) audit_status にはない欄
        if msg.len() < 44 {
            return Err(-EINVAL);
        }
        let v = w(9);
        if v > 10 * BACKLOG_WAIT_TIME {
            return Err(-EINVAL);
        }
        do_config_change("audit_backlog_wait_time", |s| &mut s.backlog_wait_time, v)?;
    }
    if mask == STATUS_LOST {
        let lost = core::mem::take(&mut st().lost);
        config_change("lost", 0, lost, true);
        return Ok(lost as i64);
    }
    if mask == STATUS_BACKLOG_WAIT_TIME_ACTUAL {
        config_change("backlog_wait_time_actual", 0, 0, true);
    }
    Ok(0)
}

/// auditd になる (new_pid が自分) か、やめる (0)
fn set_pid(n: &NetlinkRef, msg: &[u8], new_pid: u32, me: u32, cap_ack: bool, ack: &mut bool) -> Result<(), i64> {
    let tgid = proc::current().tgid;
    if new_pid != 0 && new_pid != tgid {
        return Err(-EINVAL);
    }
    // いまの auditd につながりをたしかめる (audit_replace: 名乗ったものの pid を送る)
    let portid = st().portid;
    if portid != 0 {
        let mut m = Vec::new();
        reply(&mut m, AUDIT_REPLACE, 0, 0, &tgid.to_le_bytes());
        if !netlink::unicast(NETLINK_AUDIT, portid, m.pop().unwrap()) {
            reset();
        }
    }
    let cur = st().pid;
    if cur != 0 {
        if new_pid != 0 {
            config_change("audit_pid", new_pid, cur, false);
            return Err(-EEXIST);
        }
        if tgid != cur {
            config_change("audit_pid", new_pid, cur, false);
            return Err(-EACCES);
        }
    }
    if new_pid != 0 {
        (st().pid, st().portid) = (new_pid, me);
        // ACK を先に (auditd_set: 記録より前に)
        if *ack {
            netlink::deliver(n, 0, 0, netlink::ack(msg, 0, cap_ack, false, None, None, me));
            *ack = false;
        }
        if st().enabled != OFF {
            config_change("audit_pid", new_pid, cur, true);
        }
    } else {
        // 記録は ACK のあとに出すので、そのときにはもう auditd はいない (kmsg に)
        if st().enabled != OFF {
            config_change("audit_pid", 0, cur, true);
        }
        reset();
    }
    Ok(())
}

/// AUDIT_SET_FEATURE (audit_set_feature): 固定のものを変えようとすれば EPERM、変えたものは AUDIT_FEATURE_CHANGE に
fn set_feature(mask: u32, features: u32, lock: u32) -> Result<(), i64> {
    let s = st();
    for (i, name) in FEATURE_NAMES.iter().enumerate() {
        let f = 1 << i;
        if mask & f == 0 {
            continue;
        }
        let (old, new) = (s.features & f, features & f);
        let (old_lock, new_lock) = (s.lock & f, (lock | s.lock) & f);
        if old_lock != 0 && new != old {
            feature_change(name, old, new, old_lock, new_lock, false);
            return Err(-EPERM);
        }
    }
    for (i, name) in FEATURE_NAMES.iter().enumerate() {
        let f = 1 << i;
        if mask & f == 0 {
            continue;
        }
        let s = st();
        let (old, new) = (s.features & f, features & f);
        let (old_lock, new_lock) = (s.lock & f, (lock | s.lock) & f);
        if new != old {
            feature_change(name, old, new, old_lock, new_lock, true);
        }
        let s = st();
        if new != 0 {
            s.features |= f;
        } else {
            s.features &= !f;
        }
        s.lock |= new_lock;
    }
    Ok(())
}

fn feature_change(name: &str, old: u32, new: u32, old_lock: u32, new_lock: u32, res: bool) {
    if st().enabled == OFF {
        return;
    }
    let b = |v: u32| (v != 0) as u32;
    let body = format!(
        "{} feature={} old={} new={} old_lock={} new_lock={} res={}",
        task_info(),
        name,
        b(old),
        b(new),
        b(old_lock),
        b(new_lock),
        res as u32
    );
    log_task(AUDIT_FEATURE_CHANGE, &body);
}

// /proc/sys: カーネルの値を読み書きする (Linux の sysctl と同じ場所と形)
//
//   /proc/sys/kernel/hostname                uname の nodename (sethostname と同じ)
//   /proc/sys/kernel/ostype, osrelease       読むだけ
//   /proc/sys/kernel/sched_timeslice_ms      ほかに順番をゆずるまで走る長さ (tick の倍数に切り上げ)
//   /proc/sys/kernel/sched_wakeup_preempt    1 なら、起こされたものが CPU を使い続けているものに割りこむ
//   /proc/sys/kernel/random/entropy_avail    乱数のもとのビット (256 なら ready。rand.rs)。poolsize は 256
//   /proc/sys/kernel/random/boot_id, uuid    この起動の UUID (はじめて読んだときに決める) と、読むたびに新しい UUID
//   /proc/sys/fs/inotify/max_queued_events   inotify にためておくできごとの数
//   /proc/sys/fs/inotify/max_user_watches    1 つの inotify の watch の数
//   /proc/sys/fs/nr_open                     読むだけ (開けるファイルの数の上限)
//   /proc/sys/fs/suid_dumpable               uid や gid を変えたプロセスの dumpable (0、1、2: Linux と同じ)
//   /proc/sys/vm/min_free_kbytes             空きがこれを割ったら swap へ回収する (その 4 倍まで)
//   /proc/sys/net/netfilter/nf_conntrack_*   conntrack の数、上限、時間切れ、tcp_loose など (conntrack.rs)
//   /proc/sys/net/ipv6/conf/*/                口ごとの IPv6 の値 (addrconf::CONF)、net/ipv4/ping_group_range
//
// 書けるのは root だけ。値は 1 行 (終わりの改行や空白はなくてよい)。起動のときに init が
// /etc/sysctl.d/*.conf を読んでここに書く (aios apply の sysctl)
use alloc::format;
use alloc::string::{String, ToString};
use core::sync::atomic::{AtomicUsize, Ordering};
use crate::spinlock::SpinLock;


/// inotify にためておくできごとの数。こえたら IN_Q_OVERFLOW
pub static INOTIFY_MAX_QUEUED: AtomicUsize = AtomicUsize::new(16384);
/// 1 つの inotify の watch の数
pub static INOTIFY_MAX_WATCHES: AtomicUsize = AtomicUsize::new(8192);
/// euid や egid を変えた・setuid のものを exec した・読めないものを exec したプロセスの dumpable (0 作らない、1 作る、
/// 2 root の権利で。Linux の fs.suid_dumpable: 既定 0)
pub static SUID_DUMPABLE: AtomicUsize = AtomicUsize::new(0);
/// タイムスライス (tick)。走りはじめてからタイマの割り込みがこれだけ来たら、ほかに順番をゆずる
pub static SCHED_TIMESLICE_TICKS: AtomicUsize = AtomicUsize::new(1);
/// 起こされたもの (あまり CPU を使っていない) が、走っているもの (使っている) に割りこむか
pub static SCHED_WAKEUP_PREEMPT: AtomicUsize = AtomicUsize::new(1);
/// 空きがこれ (ページ) を割ったら回収する (4 MiB)。回収はこの 4 倍になるまで
pub static MIN_FREE_PAGES: AtomicUsize = AtomicUsize::new(1024);

pub struct Entry {
    /// /proc/sys の下のパス
    pub path: &'static str,
    get: fn() -> String,
    /// None なら読むだけ
    set: Option<fn(&str) -> Result<(), i64>>,
}

impl Entry {
    pub fn writable(&self) -> bool {
        self.set.is_some()
    }

    pub fn read(&self) -> String {
        let mut s = (self.get)();
        s.push('\n');
        s
    }

    pub fn write(&self, b: &[u8]) -> Result<(), i64> {
        let set = self.set.ok_or_else(|| crate::why::inval())?;
        let s = core::str::from_utf8(b).map_err(|_| crate::why::inval())?;
        set(s.trim_end_matches(['\n', ' ', '\t']))
    }
}

/// 乱数の UUID (版 4。Linux の /proc/sys/kernel/random/uuid と同じ形)
fn uuid(mut b: [u8; 16]) -> String {
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{:02x}", x)).collect();
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

fn random_uuid() -> String {
    let mut b = [0u8; 16];
    crate::rand::fill(&mut b);
    uuid(b)
}

/// この起動の boot_id (はじめて読んだときに決める)
static BOOT_ID: SpinLock<Option<[u8; 16]>> = SpinLock::new(None);

fn boot_id() -> String {
    let mut g = BOOT_ID.lock();
    uuid(*g.get_or_insert_with(|| {
        let mut b = [0u8; 16];
        crate::rand::fill(&mut b);
        b
    }))
}

/// /proc/sys/net/ipv6/conf の値 (addrconf::CONF の i 番の口)
fn conf_get(i: usize, f: fn(&crate::addrconf::DevConf) -> i32) -> String {
    format!("{}", f(&crate::addrconf::CONF.get()[i]))
}

/// 書く (Linux の proc_dointvec: 符号つきの数。hop_limit は 1〜255、mtu は 1280〜口の MTU: addrconf_sysctl_mtu)
fn conf_set(i: usize, key: &str, s: &str) -> Result<(), i64> {
    let v = s.trim().parse::<i32>().map_err(|_| crate::why::inval())?;
    let ok = match key {
        "hop_limit" => (1..=255).contains(&v),
        "mtu" => (1280..=crate::addrconf::dev_mtu(i)).contains(&v),
        _ => true,
    };
    if !ok {
        return Err(crate::why::inval());
    }
    let c = &mut crate::addrconf::CONF.get()[i];
    match key {
        "accept_dad" => c.accept_dad = v,
        "accept_ra" => c.accept_ra = v,
        "autoconf" => c.autoconf = v,
        "dad_transmits" => c.dad_transmits = v,
        "hop_limit" => c.hop_limit = v,
        "mtu" => c.mtu = v,
        "temp_valid_lft" => c.temp_valid_lft = v,
        "temp_prefered_lft" => c.temp_prefered_lft = v,
        "regen_max_retry" => c.regen_max_retry = v,
        "max_desync_factor" => c.max_desync_factor = v,
        "max_addresses" => c.max_addresses = v,
        "regen_min_advance" => c.regen_min_advance = v,
        _ => c.use_tempaddr = v,
    }
    Ok(())
}

/// 数を lo..=hi の中で受ける
fn num(s: &str, lo: usize, hi: usize) -> Result<usize, i64> {
    s.trim().parse::<usize>().ok().filter(|n| (lo..=hi).contains(n)).ok_or_else(|| crate::why::inval())
}

pub static TABLE: &[Entry] = &[
    Entry { path: "kernel/hostname", get: crate::syscall::hostname, set: Some(|s| crate::syscall::set_hostname(s.as_bytes())) },
    Entry { path: "kernel/ostype", get: || "aios".to_string(), set: None },
    Entry { path: "kernel/osrelease", get: || env!("AIOS_RELEASE").to_string(), set: None },
    Entry {
        path: "kernel/sched_timeslice_ms",
        get: || format!("{}", SCHED_TIMESLICE_TICKS.load(Ordering::Relaxed) as u64 * 1000 / crate::timer::HZ),
        // 1 tick (10 ms) から 1 秒。tick の倍数に切り上げる
        set: Some(|s| {
            let ms = num(s, 1, 1000)? as u64;
            let tick = 1000 / crate::timer::HZ;
            Ok(SCHED_TIMESLICE_TICKS.store(ms.div_ceil(tick) as usize, Ordering::Relaxed))
        }),
    },
    Entry { path: "kernel/sched_wakeup_preempt", get: || format!("{}", SCHED_WAKEUP_PREEMPT.load(Ordering::Relaxed)), set: Some(|s| Ok(SCHED_WAKEUP_PREEMPT.store(num(s, 0, 1)?, Ordering::Relaxed))) },
    Entry { path: "kernel/random/entropy_avail", get: || format!("{}", crate::rand::entropy()), set: None },
    Entry { path: "kernel/random/poolsize", get: || "256".to_string(), set: None },
    Entry { path: "kernel/random/boot_id", get: boot_id, set: None },
    Entry { path: "kernel/random/uuid", get: random_uuid, set: None },
    Entry { path: "fs/inotify/max_queued_events", get: || format!("{}", INOTIFY_MAX_QUEUED.load(Ordering::Relaxed)), set: Some(|s| Ok(INOTIFY_MAX_QUEUED.store(num(s, 16, 1 << 20)?, Ordering::Relaxed))) },
    Entry { path: "fs/inotify/max_user_watches", get: || format!("{}", INOTIFY_MAX_WATCHES.load(Ordering::Relaxed)), set: Some(|s| Ok(INOTIFY_MAX_WATCHES.store(num(s, 1, 1 << 20)?, Ordering::Relaxed))) },
    Entry { path: "fs/nr_open", get: || format!("{}", crate::proc::NOFILE), set: None },
    Entry { path: "fs/suid_dumpable", get: || format!("{}", SUID_DUMPABLE.load(Ordering::Relaxed)), set: Some(|s| Ok(SUID_DUMPABLE.store(num(s, 0, 2)?, Ordering::Relaxed))) },
    Entry {
        path: "vm/min_free_kbytes",
        get: || format!("{}", MIN_FREE_PAGES.load(Ordering::Relaxed) * crate::memlayout::PGSIZE / 1024),
        // 64 KiB から 1 GiB
        set: Some(|s| Ok(MIN_FREE_PAGES.store(num(s, 64, 1 << 20)? * 1024 / crate::memlayout::PGSIZE, Ordering::Relaxed))),
    },
    // conntrack (conntrack.rs。Linux の nf_conntrack_standalone.c と同じ名前と既定)
    Entry { path: "net/netfilter/nf_conntrack_acct", get: || format!("{}", crate::conntrack::KNOBS.get().acct as u8), set: Some(|s| { crate::conntrack::KNOBS.get().acct = num(s, 0, 1)? == 1; Ok(()) }) },
    Entry { path: "net/netfilter/nf_conntrack_buckets", get: || format!("{}", crate::conntrack::buckets()), set: None },
    Entry { path: "net/netfilter/nf_conntrack_checksum", get: || format!("{}", crate::conntrack::KNOBS.get().checksum as u8), set: Some(|s| { crate::conntrack::KNOBS.get().checksum = num(s, 0, 1)? == 1; Ok(()) }) },
    Entry { path: "net/netfilter/nf_conntrack_count", get: || format!("{}", crate::conntrack::count()), set: None },
    Entry { path: "net/netfilter/nf_conntrack_generic_timeout", get: || format!("{}", crate::conntrack::TIMEOUTS.get().generic), set: Some(|s| { crate::conntrack::TIMEOUTS.get().generic = num(s, 0, 1 << 30)? as u64; Ok(()) }) },
    Entry { path: "net/netfilter/nf_conntrack_icmp_timeout", get: || format!("{}", crate::conntrack::TIMEOUTS.get().icmp), set: Some(|s| { crate::conntrack::TIMEOUTS.get().icmp = num(s, 0, 1 << 30)? as u64; Ok(()) }) },
    Entry { path: "net/netfilter/nf_conntrack_icmpv6_timeout", get: || format!("{}", crate::conntrack::TIMEOUTS.get().icmpv6), set: Some(|s| { crate::conntrack::TIMEOUTS.get().icmpv6 = num(s, 0, 1 << 30)? as u64; Ok(()) }) },
    Entry { path: "net/netfilter/nf_conntrack_max", get: || format!("{}", crate::conntrack::max()), set: Some(|s| { crate::conntrack::KNOBS.get().max = num(s, 1, 1 << 30)?; Ok(()) }) },
    Entry { path: "net/netfilter/nf_conntrack_tcp_be_liberal", get: || format!("{}", crate::conntrack::KNOBS.get().tcp_be_liberal as u8), set: Some(|s| { crate::conntrack::KNOBS.get().tcp_be_liberal = num(s, 0, 1)? == 1; Ok(()) }) },
    Entry { path: "net/netfilter/nf_conntrack_tcp_ignore_invalid_rst", get: || format!("{}", crate::conntrack::KNOBS.get().tcp_ignore_invalid_rst as u8), set: Some(|s| { crate::conntrack::KNOBS.get().tcp_ignore_invalid_rst = num(s, 0, 1)? == 1; Ok(()) }) },
    Entry { path: "net/netfilter/nf_conntrack_tcp_loose", get: || format!("{}", crate::conntrack::KNOBS.get().tcp_loose as u8), set: Some(|s| { crate::conntrack::KNOBS.get().tcp_loose = num(s, 0, 1)? == 1; Ok(()) }) },
    Entry { path: "net/netfilter/nf_conntrack_tcp_max_retrans", get: || format!("{}", crate::conntrack::KNOBS.get().tcp_max_retrans), set: Some(|s| { crate::conntrack::KNOBS.get().tcp_max_retrans = num(s, 0, 255)? as u8; Ok(()) }) },
    Entry { path: "net/netfilter/nf_conntrack_tcp_timeout_close", get: || format!("{}", crate::conntrack::TIMEOUTS.get().tcp[8]), set: Some(|s| { crate::conntrack::TIMEOUTS.get().tcp[8] = num(s, 0, 1 << 30)? as u64; Ok(()) }) },
    Entry { path: "net/netfilter/nf_conntrack_tcp_timeout_close_wait", get: || format!("{}", crate::conntrack::TIMEOUTS.get().tcp[5]), set: Some(|s| { crate::conntrack::TIMEOUTS.get().tcp[5] = num(s, 0, 1 << 30)? as u64; Ok(()) }) },
    Entry { path: "net/netfilter/nf_conntrack_tcp_timeout_established", get: || format!("{}", crate::conntrack::TIMEOUTS.get().tcp[3]), set: Some(|s| { crate::conntrack::TIMEOUTS.get().tcp[3] = num(s, 0, 1 << 30)? as u64; Ok(()) }) },
    Entry { path: "net/netfilter/nf_conntrack_tcp_timeout_fin_wait", get: || format!("{}", crate::conntrack::TIMEOUTS.get().tcp[4]), set: Some(|s| { crate::conntrack::TIMEOUTS.get().tcp[4] = num(s, 0, 1 << 30)? as u64; Ok(()) }) },
    Entry { path: "net/netfilter/nf_conntrack_tcp_timeout_last_ack", get: || format!("{}", crate::conntrack::TIMEOUTS.get().tcp[6]), set: Some(|s| { crate::conntrack::TIMEOUTS.get().tcp[6] = num(s, 0, 1 << 30)? as u64; Ok(()) }) },
    Entry { path: "net/netfilter/nf_conntrack_tcp_timeout_max_retrans", get: || format!("{}", crate::conntrack::TIMEOUTS.get().tcp[12]), set: Some(|s| { crate::conntrack::TIMEOUTS.get().tcp[12] = num(s, 0, 1 << 30)? as u64; Ok(()) }) },
    Entry { path: "net/netfilter/nf_conntrack_tcp_timeout_syn_recv", get: || format!("{}", crate::conntrack::TIMEOUTS.get().tcp[2]), set: Some(|s| { crate::conntrack::TIMEOUTS.get().tcp[2] = num(s, 0, 1 << 30)? as u64; Ok(()) }) },
    Entry { path: "net/netfilter/nf_conntrack_tcp_timeout_syn_sent", get: || format!("{}", crate::conntrack::TIMEOUTS.get().tcp[1]), set: Some(|s| { crate::conntrack::TIMEOUTS.get().tcp[1] = num(s, 0, 1 << 30)? as u64; Ok(()) }) },
    Entry { path: "net/netfilter/nf_conntrack_tcp_timeout_time_wait", get: || format!("{}", crate::conntrack::TIMEOUTS.get().tcp[7]), set: Some(|s| { crate::conntrack::TIMEOUTS.get().tcp[7] = num(s, 0, 1 << 30)? as u64; Ok(()) }) },
    Entry { path: "net/netfilter/nf_conntrack_tcp_timeout_unacknowledged", get: || format!("{}", crate::conntrack::TIMEOUTS.get().tcp[13]), set: Some(|s| { crate::conntrack::TIMEOUTS.get().tcp[13] = num(s, 0, 1 << 30)? as u64; Ok(()) }) },
    Entry { path: "net/netfilter/nf_conntrack_udp_timeout", get: || format!("{}", crate::conntrack::TIMEOUTS.get().udp), set: Some(|s| { crate::conntrack::TIMEOUTS.get().udp = num(s, 0, 1 << 30)? as u64; Ok(()) }) },
    Entry { path: "net/netfilter/nf_conntrack_udp_timeout_stream", get: || format!("{}", crate::conntrack::TIMEOUTS.get().udp_stream), set: Some(|s| { crate::conntrack::TIMEOUTS.get().udp_stream = num(s, 0, 1 << 30)? as u64; Ok(()) }) },
    Entry { path: "net/nf_conntrack_max", get: || format!("{}", crate::conntrack::max()), set: Some(|s| { crate::conntrack::KNOBS.get().max = num(s, 1, 1 << 30)?; Ok(()) }) },
    // ping ソケット (SOCK_DGRAM の ICMP と ICMPv6) を作れる group (既定の 1 0 はだれも。aios の IPv4 の ping はだれでも)
    Entry { path: "net/ipv4/ping_group_range", get: || "1\t0".to_string(), set: None },
    // IPv6 の口ごとの値 (addrconf::CONF: all と default と lo と eth0)。addr_gen_mode、disable_ipv6、forwarding は読むだけ
    Entry { path: "net/ipv6/conf/all/accept_dad", get: || conf_get(0, |c| c.accept_dad), set: Some(|s| conf_set(0, "accept_dad", s)) },
    Entry { path: "net/ipv6/conf/all/accept_ra", get: || conf_get(0, |c| c.accept_ra), set: Some(|s| conf_set(0, "accept_ra", s)) },
    Entry { path: "net/ipv6/conf/all/addr_gen_mode", get: || "0".to_string(), set: None },
    Entry { path: "net/ipv6/conf/all/autoconf", get: || conf_get(0, |c| c.autoconf), set: Some(|s| conf_set(0, "autoconf", s)) },
    Entry { path: "net/ipv6/conf/all/dad_transmits", get: || conf_get(0, |c| c.dad_transmits), set: Some(|s| conf_set(0, "dad_transmits", s)) },
    Entry { path: "net/ipv6/conf/all/disable_ipv6", get: || "0".to_string(), set: None },
    Entry { path: "net/ipv6/conf/all/forwarding", get: || "0".to_string(), set: None },
    Entry { path: "net/ipv6/conf/all/hop_limit", get: || conf_get(0, |c| c.hop_limit), set: Some(|s| conf_set(0, "hop_limit", s)) },
    Entry { path: "net/ipv6/conf/all/max_addresses", get: || conf_get(0, |c| c.max_addresses), set: Some(|s| conf_set(0, "max_addresses", s)) },
    Entry { path: "net/ipv6/conf/all/max_desync_factor", get: || conf_get(0, |c| c.max_desync_factor), set: Some(|s| conf_set(0, "max_desync_factor", s)) },
    Entry { path: "net/ipv6/conf/all/mtu", get: || conf_get(0, |c| c.mtu), set: Some(|s| conf_set(0, "mtu", s)) },
    Entry { path: "net/ipv6/conf/all/regen_max_retry", get: || conf_get(0, |c| c.regen_max_retry), set: Some(|s| conf_set(0, "regen_max_retry", s)) },
    Entry { path: "net/ipv6/conf/all/regen_min_advance", get: || conf_get(0, |c| c.regen_min_advance), set: Some(|s| conf_set(0, "regen_min_advance", s)) },
    Entry { path: "net/ipv6/conf/all/temp_prefered_lft", get: || conf_get(0, |c| c.temp_prefered_lft), set: Some(|s| conf_set(0, "temp_prefered_lft", s)) },
    Entry { path: "net/ipv6/conf/all/temp_valid_lft", get: || conf_get(0, |c| c.temp_valid_lft), set: Some(|s| conf_set(0, "temp_valid_lft", s)) },
    Entry { path: "net/ipv6/conf/all/use_tempaddr", get: || conf_get(0, |c| c.use_tempaddr), set: Some(|s| conf_set(0, "use_tempaddr", s)) },
    Entry { path: "net/ipv6/conf/default/accept_dad", get: || conf_get(1, |c| c.accept_dad), set: Some(|s| conf_set(1, "accept_dad", s)) },
    Entry { path: "net/ipv6/conf/default/accept_ra", get: || conf_get(1, |c| c.accept_ra), set: Some(|s| conf_set(1, "accept_ra", s)) },
    Entry { path: "net/ipv6/conf/default/addr_gen_mode", get: || "0".to_string(), set: None },
    Entry { path: "net/ipv6/conf/default/autoconf", get: || conf_get(1, |c| c.autoconf), set: Some(|s| conf_set(1, "autoconf", s)) },
    Entry { path: "net/ipv6/conf/default/dad_transmits", get: || conf_get(1, |c| c.dad_transmits), set: Some(|s| conf_set(1, "dad_transmits", s)) },
    Entry { path: "net/ipv6/conf/default/disable_ipv6", get: || "0".to_string(), set: None },
    Entry { path: "net/ipv6/conf/default/forwarding", get: || "0".to_string(), set: None },
    Entry { path: "net/ipv6/conf/default/hop_limit", get: || conf_get(1, |c| c.hop_limit), set: Some(|s| conf_set(1, "hop_limit", s)) },
    Entry { path: "net/ipv6/conf/default/max_addresses", get: || conf_get(1, |c| c.max_addresses), set: Some(|s| conf_set(1, "max_addresses", s)) },
    Entry { path: "net/ipv6/conf/default/max_desync_factor", get: || conf_get(1, |c| c.max_desync_factor), set: Some(|s| conf_set(1, "max_desync_factor", s)) },
    Entry { path: "net/ipv6/conf/default/mtu", get: || conf_get(1, |c| c.mtu), set: Some(|s| conf_set(1, "mtu", s)) },
    Entry { path: "net/ipv6/conf/default/regen_max_retry", get: || conf_get(1, |c| c.regen_max_retry), set: Some(|s| conf_set(1, "regen_max_retry", s)) },
    Entry { path: "net/ipv6/conf/default/regen_min_advance", get: || conf_get(1, |c| c.regen_min_advance), set: Some(|s| conf_set(1, "regen_min_advance", s)) },
    Entry { path: "net/ipv6/conf/default/temp_prefered_lft", get: || conf_get(1, |c| c.temp_prefered_lft), set: Some(|s| conf_set(1, "temp_prefered_lft", s)) },
    Entry { path: "net/ipv6/conf/default/temp_valid_lft", get: || conf_get(1, |c| c.temp_valid_lft), set: Some(|s| conf_set(1, "temp_valid_lft", s)) },
    Entry { path: "net/ipv6/conf/default/use_tempaddr", get: || conf_get(1, |c| c.use_tempaddr), set: Some(|s| conf_set(1, "use_tempaddr", s)) },
    Entry { path: "net/ipv6/conf/lo/accept_dad", get: || conf_get(2, |c| c.accept_dad), set: Some(|s| conf_set(2, "accept_dad", s)) },
    Entry { path: "net/ipv6/conf/lo/accept_ra", get: || conf_get(2, |c| c.accept_ra), set: Some(|s| conf_set(2, "accept_ra", s)) },
    Entry { path: "net/ipv6/conf/lo/addr_gen_mode", get: || "0".to_string(), set: None },
    Entry { path: "net/ipv6/conf/lo/autoconf", get: || conf_get(2, |c| c.autoconf), set: Some(|s| conf_set(2, "autoconf", s)) },
    Entry { path: "net/ipv6/conf/lo/dad_transmits", get: || conf_get(2, |c| c.dad_transmits), set: Some(|s| conf_set(2, "dad_transmits", s)) },
    Entry { path: "net/ipv6/conf/lo/disable_ipv6", get: || "0".to_string(), set: None },
    Entry { path: "net/ipv6/conf/lo/forwarding", get: || "0".to_string(), set: None },
    Entry { path: "net/ipv6/conf/lo/hop_limit", get: || conf_get(2, |c| c.hop_limit), set: Some(|s| conf_set(2, "hop_limit", s)) },
    Entry { path: "net/ipv6/conf/lo/max_addresses", get: || conf_get(2, |c| c.max_addresses), set: Some(|s| conf_set(2, "max_addresses", s)) },
    Entry { path: "net/ipv6/conf/lo/max_desync_factor", get: || conf_get(2, |c| c.max_desync_factor), set: Some(|s| conf_set(2, "max_desync_factor", s)) },
    Entry { path: "net/ipv6/conf/lo/mtu", get: || conf_get(2, |c| c.mtu), set: Some(|s| conf_set(2, "mtu", s)) },
    Entry { path: "net/ipv6/conf/lo/regen_max_retry", get: || conf_get(2, |c| c.regen_max_retry), set: Some(|s| conf_set(2, "regen_max_retry", s)) },
    Entry { path: "net/ipv6/conf/lo/regen_min_advance", get: || conf_get(2, |c| c.regen_min_advance), set: Some(|s| conf_set(2, "regen_min_advance", s)) },
    Entry { path: "net/ipv6/conf/lo/temp_prefered_lft", get: || conf_get(2, |c| c.temp_prefered_lft), set: Some(|s| conf_set(2, "temp_prefered_lft", s)) },
    Entry { path: "net/ipv6/conf/lo/temp_valid_lft", get: || conf_get(2, |c| c.temp_valid_lft), set: Some(|s| conf_set(2, "temp_valid_lft", s)) },
    Entry { path: "net/ipv6/conf/lo/use_tempaddr", get: || conf_get(2, |c| c.use_tempaddr), set: Some(|s| conf_set(2, "use_tempaddr", s)) },
    Entry { path: "net/ipv6/conf/eth0/accept_dad", get: || conf_get(3, |c| c.accept_dad), set: Some(|s| conf_set(3, "accept_dad", s)) },
    Entry { path: "net/ipv6/conf/eth0/accept_ra", get: || conf_get(3, |c| c.accept_ra), set: Some(|s| conf_set(3, "accept_ra", s)) },
    Entry { path: "net/ipv6/conf/eth0/addr_gen_mode", get: || "0".to_string(), set: None },
    Entry { path: "net/ipv6/conf/eth0/autoconf", get: || conf_get(3, |c| c.autoconf), set: Some(|s| conf_set(3, "autoconf", s)) },
    Entry { path: "net/ipv6/conf/eth0/dad_transmits", get: || conf_get(3, |c| c.dad_transmits), set: Some(|s| conf_set(3, "dad_transmits", s)) },
    Entry { path: "net/ipv6/conf/eth0/disable_ipv6", get: || "0".to_string(), set: None },
    Entry { path: "net/ipv6/conf/eth0/forwarding", get: || "0".to_string(), set: None },
    Entry { path: "net/ipv6/conf/eth0/hop_limit", get: || conf_get(3, |c| c.hop_limit), set: Some(|s| conf_set(3, "hop_limit", s)) },
    Entry { path: "net/ipv6/conf/eth0/max_addresses", get: || conf_get(3, |c| c.max_addresses), set: Some(|s| conf_set(3, "max_addresses", s)) },
    Entry { path: "net/ipv6/conf/eth0/max_desync_factor", get: || conf_get(3, |c| c.max_desync_factor), set: Some(|s| conf_set(3, "max_desync_factor", s)) },
    Entry { path: "net/ipv6/conf/eth0/mtu", get: || conf_get(3, |c| c.mtu), set: Some(|s| conf_set(3, "mtu", s)) },
    Entry { path: "net/ipv6/conf/eth0/regen_max_retry", get: || conf_get(3, |c| c.regen_max_retry), set: Some(|s| conf_set(3, "regen_max_retry", s)) },
    Entry { path: "net/ipv6/conf/eth0/regen_min_advance", get: || conf_get(3, |c| c.regen_min_advance), set: Some(|s| conf_set(3, "regen_min_advance", s)) },
    Entry { path: "net/ipv6/conf/eth0/temp_prefered_lft", get: || conf_get(3, |c| c.temp_prefered_lft), set: Some(|s| conf_set(3, "temp_prefered_lft", s)) },
    Entry { path: "net/ipv6/conf/eth0/temp_valid_lft", get: || conf_get(3, |c| c.temp_valid_lft), set: Some(|s| conf_set(3, "temp_valid_lft", s)) },
    Entry { path: "net/ipv6/conf/eth0/use_tempaddr", get: || conf_get(3, |c| c.use_tempaddr), set: Some(|s| conf_set(3, "use_tempaddr", s)) },
];

/// パスを / で分けたものの d 番目まで
fn prefix(path: &str, d: usize) -> impl Iterator<Item = &str> {
    path.split('/').take(d)
}

/// 表の i 番のパスの、はじめの d 個が、j 番のものと同じか
pub fn same_dir(i: usize, j: usize, d: usize) -> bool {
    TABLE[j].path.split('/').count() > d && prefix(TABLE[i].path, d).eq(prefix(TABLE[j].path, d))
}

/// ディレクトリ (i 番のはじめの d 個) の中の name。(表の番号, ファイルか)。番号はそこを通るものの最初
pub fn lookup(i: usize, d: usize, name: &str) -> Option<(usize, bool)> {
    let j = (0..TABLE.len()).find(|&j| same_dir(i, j, d) && TABLE[j].path.split('/').nth(d) == Some(name))?;
    Some((j, TABLE[j].path.split('/').count() == d + 1))
}

/// ディレクトリの中身: (名前, 表の番号, ファイルか)
pub fn list(i: usize, d: usize) -> alloc::vec::Vec<(&'static str, usize, bool)> {
    let mut v: alloc::vec::Vec<(&'static str, usize, bool)> = alloc::vec::Vec::new();
    for j in 0..TABLE.len() {
        if !same_dir(i, j, d) {
            continue;
        }
        let Some(name) = TABLE[j].path.split('/').nth(d) else { continue };
        if !v.iter().any(|x| x.0 == name) {
            v.push((name, j, TABLE[j].path.split('/').count() == d + 1));
        }
    }
    v
}

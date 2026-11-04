// つながりの見張り (conntrack: doc/aios.md の「ファイアウォール (nftables)」の段階 3)。Linux 7.3 の
// net/netfilter/nf_conntrack_core.c、nf_conntrack_proto_tcp.c (状態の表と窓のたしかめ)、nf_conntrack_proto_udp.c、
// nf_conntrack_proto_icmp.c、nf_conntrack_proto_generic.c、nf_conntrack_standalone.c (/proc/net/nf_conntrack と
// /proc/sys/net/netfilter) と同じ答え。
//
// - 動くのは、決まりに ct の式 (か notrack) があるときだけ (Linux の nf_ct_netns_get: nft_ct が使うときにフックを入れる)。
//   フックは PRE_ROUTING と LOCAL_OUT の priority -200 (nf_conntrack_in: 見て、なければ作る) と、LOCAL_IN と
//   POST_ROUTING のいちばん後 (confirm: 表に入れる)。そのあいだで落ちた新しいものは表に入らない
// - つながり (Conn) は向きごとの組 (tuple: 送り元、あて先、protocol、ポートか ICMP の type・code・id) を 2 つ持ち、表は
//   組から (番号, 向き) を引く。パケットには ctinfo (ESTABLISHED、RELATED、NEW、と答えの向きなら +3) がつく
// - TCP: tcp_conntracks の表で状態を変え、tcp_in_window で窓をたしかめる (外れは INVALID か IGNORE)。tcp_loose 1 で
//   途中からも拾う。UDP: 答えが来たら、2 秒より後のパケットで 120 秒にのばして ASSURED。ICMP: echo などの問いだけが
//   つながりになり、エラーは中のつながりの RELATED
// - 中どうし (lo) は aios はパケットを作らない (nfhook.rs) ので、Linux が lo に流すものと同じものを作って通す: TCP は
//   つなぐときの SYN、SYN-ACK、3 つ目の ACK と、閉じるときの FIN、ACK、RST。データの segment は通さない
//   (ESTABLISHED のまま。時間切れの数は 5 日のまま)
use crate::cell::Bkl;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

const HZ_NS: u64 = 1_000_000_000;

pub const DIR_ORIG: u8 = 0;
pub const DIR_REPLY: u8 = 1;

/// ctinfo (enum ip_conntrack_info)
pub const CT_ESTABLISHED: u8 = 0;
pub const CT_RELATED: u8 = 1;
pub const CT_NEW: u8 = 2;
pub const CT_IS_REPLY: u8 = 3;
pub const CT_ESTABLISHED_REPLY: u8 = 3;
pub const CT_RELATED_REPLY: u8 = 4;

/// ct->status (IPS_*)
pub const IPS_EXPECTED: u32 = 1;
pub const IPS_SEEN_REPLY: u32 = 2;
pub const IPS_ASSURED: u32 = 4;
pub const IPS_CONFIRMED: u32 = 8;
pub const IPS_FIXED_TIMEOUT: u32 = 1 << 10;

/// TCP の状態 (enum tcp_conntrack)
const NO: u8 = 0;
const SS: u8 = 1;
const SR: u8 = 2;
const ES: u8 = 3;
const FW: u8 = 4;
const CW: u8 = 5;
const LA: u8 = 6;
const TW: u8 = 7;
const CL: u8 = 8;
const S2: u8 = 9;
/// TCP_CONNTRACK_MAX (おかしい) と TCP_CONNTRACK_IGNORE
const IV: u8 = 10;
const IG: u8 = 11;
/// TCP_CONNTRACK_RETRANS と TCP_CONNTRACK_UNACK (時間切れの表の番号)
const RETRANS: usize = 12;
const UNACK: usize = 13;

const TCP_NAMES: [&str; 10] = ["NONE", "SYN_SENT", "SYN_RECV", "ESTABLISHED", "FIN_WAIT", "CLOSE_WAIT", "LAST_ACK", "TIME_WAIT", "CLOSE", "SYN_SENT2"];

/// tcp_conntracks[向き][フラグの組][いまの状態]
const TCP_TABLE: [[[u8; 10]; 6]; 2] = [
    [
        // ORIGINAL       sNO sSS sSR sES sFW sCW sLA sTW sCL sS2
        /* syn    */ [SS, SS, IG, IG, IG, IG, IG, SS, SS, S2],
        /* synack */ [IV, IV, SR, IV, IV, IV, IV, IV, IV, SR],
        /* fin    */ [IV, IV, FW, FW, LA, LA, LA, TW, CL, IV],
        /* ack    */ [ES, IV, ES, ES, CW, CW, TW, TW, CL, IV],
        /* rst    */ [IV, CL, CL, CL, CL, CL, CL, CL, CL, CL],
        /* none   */ [IV, IV, IV, IV, IV, IV, IV, IV, IV, IV],
    ],
    [
        // REPLY          sNO sSS sSR sES sFW sCW sLA sTW sCL sS2
        /* syn    */ [IV, S2, IV, IV, IV, IV, IV, SS, IV, S2],
        /* synack */ [IV, SR, IG, IG, IG, IG, IG, IG, IG, SR],
        /* fin    */ [IV, IV, FW, FW, LA, LA, LA, TW, CL, IV],
        /* ack    */ [IV, IG, SR, ES, CW, CW, TW, TW, CL, IG],
        /* rst    */ [IV, CL, CL, CL, CL, CL, CL, CL, CL, CL],
        /* none   */ [IV, IV, IV, IV, IV, IV, IV, IV, IV, IV],
    ],
];

const SYN_SET: u8 = 0;
const SYNACK_SET: u8 = 1;
const FIN_SET: u8 = 2;
const ACK_SET: u8 = 3;
const RST_SET: u8 = 4;
const NONE_SET: u8 = 5;

/// ip_ct_tcp_state.flags
const F_WINDOW_SCALE: u8 = 0x01;
const F_SACK_PERM: u8 = 0x02;
const F_CLOSE_INIT: u8 = 0x04;
const F_BE_LIBERAL: u8 = 0x08;
const F_DATA_UNACKNOWLEDGED: u8 = 0x10;
const F_MAXACK_SET: u8 = 0x20;
const F_EXP_CHALLENGE_ACK: u8 = 0x40;
const F_SIMULTANEOUS_OPEN: u8 = 0x80;

const MAXACKWINCONST: u32 = 66000;

/// 時間切れ (秒)。/proc/sys/net/netfilter の nf_conntrack_*_timeout* で変えられる (Linux の既定)
pub struct Timeouts {
    pub tcp: [u64; 14],
    pub udp: u64,
    pub udp_stream: u64,
    pub icmp: u64,
    pub icmpv6: u64,
    pub generic: u64,
}

pub static TIMEOUTS: Bkl<Timeouts> = Bkl::new(Timeouts {
    // NONE SYN_SENT SYN_RECV ESTABLISHED FIN_WAIT CLOSE_WAIT LAST_ACK TIME_WAIT CLOSE SYN_SENT2 (IV IG) RETRANS UNACK
    tcp: [0, 120, 60, 432000, 120, 60, 30, 120, 10, 120, 0, 0, 300, 300],
    udp: 30,
    udp_stream: 120,
    icmp: 30,
    icmpv6: 30,
    generic: 600,
});

/// sysctl: nf_conntrack_tcp_loose、tcp_be_liberal、tcp_max_retrans、checksum、acct、tcp_ignore_invalid_rst
pub struct Knobs {
    pub tcp_loose: bool,
    pub tcp_be_liberal: bool,
    pub tcp_max_retrans: u8,
    pub tcp_ignore_invalid_rst: bool,
    pub checksum: bool,
    pub acct: bool,
    pub max: usize,
}

pub static KNOBS: Bkl<Knobs> = Bkl::new(Knobs { tcp_loose: true, tcp_be_liberal: false, tcp_max_retrans: 3, tcp_ignore_invalid_rst: false, checksum: true, acct: false, max: 0 });

/// 向きごとの組 (ICMP と ICMPv6 は sport が id、dport が type << 8 | code)。番地は IPv4 なら下の 32 ビット
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Tuple {
    /// 4 か 6
    pub l3: u8,
    pub src: u128,
    pub dst: u128,
    pub proto: u8,
    pub sport: u16,
    pub dport: u16,
}

#[derive(Clone, Copy, Default)]
struct TcpSide {
    td_end: u32,
    td_maxend: u32,
    td_maxwin: u32,
    td_maxack: u32,
    td_scale: u8,
    flags: u8,
}

#[derive(Clone, Copy, Default)]
struct TcpCt {
    seen: [TcpSide; 2],
    state: u8,
    last_dir: u8,
    retrans: u8,
    last_index: u8,
    last_seq: u32,
    last_ack: u32,
    last_end: u32,
    last_win: u16,
    last_wscale: u8,
    last_flags: u8,
}

#[derive(Clone)]
enum Proto {
    Tcp(TcpCt),
    Udp { stream_ts: u64 },
    Icmp,
    Generic,
}

/// 1 つのつながり (struct nf_conn)
#[derive(Clone)]
pub struct Conn {
    pub id: u32,
    pub tuple: [Tuple; 2],
    pub status: u32,
    /// confirm したものは消える時刻 (起きてからの ns)、まだのものは長さ (ns)
    timeout: u64,
    pub mark: u32,
    /// [向き][packets, bytes] (acct のとき)
    acct: Option<[[u64; 2]; 2]>,
    proto: Proto,
}

/// パケットについたつながり (skb->_nfct)
#[derive(Default)]
pub enum Ct {
    #[default]
    None,
    /// notrack
    Untracked,
    /// 表にあるもの (番号) と ctinfo
    Known(u32, u8),
    /// まだ表にないもの (confirm で入れる) と ctinfo
    New(Box<Conn>, u8),
}

impl Ct {
    pub fn info(&self) -> Option<u8> {
        match self {
            Ct::Known(_, i) | Ct::New(_, i) => Some(*i),
            _ => None,
        }
    }
}

struct Table {
    conns: BTreeMap<u32, Conn>,
    by_tuple: BTreeMap<Tuple, (u32, u8)>,
    next_id: u32,
}

static T: Bkl<Option<Table>> = Bkl::new(None);

fn table() -> &'static mut Table {
    T.get().get_or_insert_with(|| Table { conns: BTreeMap::new(), by_tuple: BTreeMap::new(), next_id: crate::rand::u32() | 1 })
}

fn now() -> u64 {
    crate::timer::uptime_ns()
}

fn be16(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

fn before(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}

fn after(a: u32, b: u32) -> bool {
    before(b, a)
}

/// nf_conntrack_max (Linux の nf_conntrack_init_start: RAM の 16 KiB ごとに 1 つの hlist、1 GiB より多ければ 65536、
/// 4 GiB より多ければ 262144、すくなくとも 1024)
pub fn max() -> usize {
    let k = KNOBS.get();
    if k.max == 0 {
        k.max = buckets();
    }
    k.max
}

pub fn buckets() -> usize {
    let pages = crate::memlayout::ram_size() / 4096;
    let mut n = (pages * 4096 / 16384) / 8;
    if pages > 4 * (1 << 18) {
        n = 262144;
    } else if pages > 1 << 18 {
        n = 65536;
    }
    n.max(1024)
}

pub fn count() -> usize {
    let t = now();
    table().conns.values().filter(|c| c.timeout > t).count()
}

// ---- 組 ----

/// パケットの組 (nf_ct_get_tuple)。ICMP のエラーやおかしな頭は None
fn tuple_of(ip: &[u8]) -> Option<Tuple> {
    let (l3, ihl, src, dst, proto) = if *ip.first()? >> 4 == 6 {
        let (thoff, proto, _) = crate::nfhook::v6_l4(ip)?;
        (6, thoff, u128::from_be_bytes(ip[8..24].try_into().ok()?), u128::from_be_bytes(ip[24..40].try_into().ok()?), proto)
    } else {
        (4, (ip[0] & 15) as usize * 4, be32(&ip[12..16]) as u128, be32(&ip[16..20]) as u128, ip[9])
    };
    let l4 = ip.get(ihl..)?;
    let (sport, dport) = match proto {
        6 | 17 => (be16(l4.get(0..2)?), be16(l4.get(2..4)?)),
        1 | 58 => {
            let h = l4.get(0..8)?;
            (be16(&h[4..6]), (h[0] as u16) << 8 | h[1] as u16)
        }
        _ => (0, 0),
    };
    Some(Tuple { l3, src, dst, proto, sport, dport })
}

/// 逆向きの組 (nf_ct_invert_tuple)。ICMP は問いと答えの type を入れかえる (それ以外は作れない)
fn invert(t: &Tuple) -> Option<Tuple> {
    let (sport, dport) = match t.proto {
        1 => {
            let (ty, code) = ((t.dport >> 8) as u8, t.dport as u8);
            let inv = match ty {
                8 => 0,
                0 => 8,
                13 => 14,
                14 => 13,
                15 => 16,
                16 => 15,
                17 => 18,
                18 => 17,
                _ => return None,
            };
            (t.sport, (inv as u16) << 8 | code as u16)
        }
        // ICMPv6 (icmpv6_invert_tuple の invmap): echo と node information の問いと答え
        58 => {
            let (ty, code) = ((t.dport >> 8) as u8, t.dport as u8);
            let inv = match ty {
                128 => 129,
                129 => 128,
                139 => 140,
                140 => 139,
                _ => return None,
            };
            (t.sport, (inv as u16) << 8 | code as u16)
        }
        6 | 17 => (t.dport, t.sport),
        _ => (0, 0),
    };
    Some(Tuple { l3: t.l3, src: t.dst, dst: t.src, proto: t.proto, sport, dport })
}

/// 表で引く (時間切れのものは消す)
fn find(t: &Tuple) -> Option<(u32, u8)> {
    let tb = table();
    let (id, dir) = *tb.by_tuple.get(t)?;
    if tb.conns.get(&id).is_some_and(|c| c.timeout <= now()) {
        kill(id);
        return None;
    }
    Some((id, dir))
}

/// 消す (nf_ct_kill)
pub fn kill(id: u32) {
    let tb = table();
    if let Some(c) = tb.conns.remove(&id) {
        tb.by_tuple.remove(&c.tuple[0]);
        tb.by_tuple.remove(&c.tuple[1]);
    }
}

fn gc() {
    let t = now();
    let dead: Vec<u32> = table().conns.values().filter(|c| c.timeout <= t).map(|c| c.id).collect();
    for id in dead {
        kill(id);
    }
}

/// nf_ct_expires (ns)。まだ表にないもの (長さを持っている) は 0 (Linux も負になって 0)
pub fn expires_ns(c: &Conn) -> u64 {
    if c.status & IPS_CONFIRMED != 0 { c.timeout.saturating_sub(now()) } else { 0 }
}

pub fn acct(c: &Conn) -> Option<[[u64; 2]; 2]> {
    c.acct
}

/// パケットのつながり (nft_ct の get の相手): 表のものか、まだ表にないもの
pub fn conn_of(ct: &Ct) -> Option<&Conn> {
    match ct {
        Ct::Known(id, _) => table().conns.get(id),
        Ct::New(c, _) => Some(c),
        _ => None,
    }
}

pub fn conn_of_mut(ct: &mut Ct) -> Option<&mut Conn> {
    match ct {
        Ct::Known(id, _) => table().conns.get_mut(id),
        Ct::New(c, _) => Some(c),
        _ => None,
    }
}

// ---- 入口 (nf_conntrack_in: PRE_ROUTING と LOCAL_OUT の -200) ----

/// trace (/proc/ai/ctl の fw trace) のあいだ: 表を変えずに、つながりを見るだけ
static DRY: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

pub fn set_dry(on: bool) {
    DRY.store(on, core::sync::atomic::Ordering::Relaxed);
}

fn dry() -> bool {
    DRY.load(core::sync::atomic::Ordering::Relaxed)
}

/// パケットのつながりを決める。ip は IPv4 の頭から、hook は PRE_ROUTING か LOCAL_OUT。返すのは NF_ACCEPT (1) か
/// NF_DROP (0: 表がいっぱい)
pub fn conntrack_in(ip: &[u8], hook: u32, ct: &mut Ct) -> i32 {
    // もう見たもの (lo の受ける側) と notrack はそのまま
    if !matches!(ct, Ct::None) {
        return 1;
    }
    let v6 = ip.first().is_some_and(|b| b >> 4 == 6);
    let (ihl, proto) = if v6 {
        // IPv6: 拡張ヘッダーをとばして。断片はまとめないので見ない (Linux は nf_defrag_ipv6 でまとめる)
        let Some((thoff, proto, frag)) = crate::nfhook::v6_l4(ip) else { return 1 };
        if frag != 0 {
            return 1;
        }
        (thoff, proto)
    } else {
        if ip.len() < 20 || u16::from_be_bytes([ip[6], ip[7]]) & 0x1fff != 0 {
            return 1;
        }
        ((ip[0] & 15) as usize * 4, ip[9])
    };
    // ICMPv6 (nf_conntrack_icmpv6_error): 近所さがしと MLD は untracked、エラーは中のパケットのつながり (related)
    if proto == 58 {
        let Some(h) = ip.get(ihl..ihl + 8) else { return 1 };
        if KNOBS.get().checksum && hook == crate::nfhook::PRE_ROUTING && !l4_ok(ip, ihl, 58) {
            return 1;
        }
        if matches!(h[0], 130..=136 | 143) {
            *ct = Ct::Untracked;
            return 1;
        }
        if h[0] < 128 {
            if let Some((id, info)) = inner_error(ip, ihl + 8, |c, dir| c.tuple[dir as usize].dst == u128::from_be_bytes(ip[24..40].try_into().unwrap())) {
                *ct = Ct::Known(id, info);
            }
            return 1;
        }
    }
    if proto == 1 && !v6 {
        match icmp_error(ip, ihl, hook) {
            // エラーでない ICMP: ふつうに
            None => {}
            Some(found) => {
                if let Some((id, info)) = found {
                    *ct = Ct::Known(id, info);
                }
                return 1;
            }
        }
    }
    let Some(t) = tuple_of(ip) else { return 1 };
    if dry() {
        let tb = table();
        match tb.by_tuple.get(&t).copied().filter(|(id, _)| tb.conns.get(id).is_some_and(|c| c.timeout > now())) {
            Some((id, dir)) => {
                let st = tb.conns[&id].status;
                let info = if dir == DIR_REPLY { CT_ESTABLISHED_REPLY } else if st & IPS_SEEN_REPLY != 0 { CT_ESTABLISHED } else { CT_NEW };
                *ct = Ct::Known(id, info);
            }
            None => {
                if let Some(r) = invert(&t) {
                    let proto = match proto {
                        6 => Proto::Tcp(TcpCt::default()),
                        17 => Proto::Udp { stream_ts: 0 },
                        1 | 58 => Proto::Icmp,
                        _ => Proto::Generic,
                    };
                    *ct = Ct::New(Box::new(Conn { id: 0, tuple: [t, r], status: 0, timeout: 0, mark: 0, acct: None, proto }), CT_NEW);
                }
            }
        }
        return 1;
    }
    // TCP がしまったつながりをまた開こうとしたら、消してもう一度 (-NF_REPEAT)
    for _ in 0..2 {
        let (mut c, info) = match find(&t) {
            Some((id, dir)) => {
                let st = table().conns[&id].status;
                let info = if dir == DIR_REPLY {
                    CT_ESTABLISHED_REPLY
                } else if st & IPS_SEEN_REPLY != 0 {
                    CT_ESTABLISHED
                } else if st & IPS_EXPECTED != 0 {
                    CT_RELATED
                } else {
                    CT_NEW
                };
                (Ct::Known(id, info), info)
            }
            None => {
                // 逆向きの作れないもの (問いでも答えでもない ICMP) はつながりにならない
                let Some(r) = invert(&t) else { return 1 };
                if table().conns.len() >= max() {
                    gc();
                    if table().conns.len() >= max() {
                        crate::kmsg::push("nf_conntrack: table full, dropping packet\n");
                        return 0;
                    }
                }
                let proto_state = match proto {
                    6 => Proto::Tcp(TcpCt::default()),
                    17 => Proto::Udp { stream_ts: 0 },
                    1 | 58 => Proto::Icmp,
                    _ => Proto::Generic,
                };
                let tb = table();
                let id = tb.next_id;
                tb.next_id = tb.next_id.wrapping_add(0x9e37_79b9) | 1;
                let acct = KNOBS.get().acct.then_some([[0; 2]; 2]);
                let c = Conn { id, tuple: [t, r], status: 0, timeout: 0, mark: 0, acct, proto: proto_state };
                (Ct::New(Box::new(c), CT_NEW), CT_NEW)
            }
        };
        let dir = if info >= CT_IS_REPLY { DIR_REPLY } else { DIR_ORIG };
        let res = {
            let Some(conn) = conn_of_mut(&mut c) else { return 1 };
            match proto {
                6 => tcp_packet(conn, ip, ihl, dir, hook),
                17 => udp_packet(conn, ip, ihl, hook),
                1 | 58 => icmp_packet(conn),
                _ => {
                    let to = TIMEOUTS.get().generic;
                    refresh(conn, to);
                    Res::Accept
                }
            }
        };
        match res {
            Res::Accept => {}
            Res::Invalid => return 1,
            Res::Repeat => {
                if let Ct::Known(id, _) = c {
                    kill(id);
                }
                continue;
            }
            Res::Kill => {
                // つながりは消えるが、このパケットはつながりを持ったまま (nf_ct_kill_acct のあとも skb は ct を持つ)
                match c {
                    Ct::Known(id, i) => {
                        let snap = table().conns.get(&id).cloned();
                        kill(id);
                        if let Some(sn) = snap {
                            *ct = Ct::New(Box::new(sn), i);
                        }
                    }
                    other => *ct = other,
                }
                return 1;
            }
        }
        if let Some(conn) = conn_of_mut(&mut c) {
            account(conn, dir, ip.len());
            if info == CT_ESTABLISHED_REPLY {
                conn.status |= IPS_SEEN_REPLY;
            }
        }
        *ct = c;
        return 1;
    }
    1
}

/// 数える (nf_ct_acct_update)
fn account(c: &mut Conn, dir: u8, len: usize) {
    if let Some(a) = c.acct.as_mut() {
        a[dir as usize][0] += 1;
        a[dir as usize][1] += len as u64;
    }
}

/// 時間切れをのばす (nf_ct_refresh: FIXED_TIMEOUT でなければ。まだ表にないものは長さで)
fn refresh(c: &mut Conn, secs: u64) {
    if c.status & IPS_FIXED_TIMEOUT != 0 {
        return;
    }
    let d = secs * HZ_NS;
    c.timeout = if c.status & IPS_CONFIRMED != 0 { now() + d } else { d };
}

enum Res {
    Accept,
    /// つながりはそのまま、パケットはつながりなし (ct state invalid)
    Invalid,
    /// 消してもう一度
    Repeat,
    /// 消す (答えが RST だけのもの)
    Kill,
}

// ---- confirm (LOCAL_IN と POST_ROUTING のいちばん後) ----

/// まだ表にないものを入れる (__nf_conntrack_confirm)。おなじ組がもうあれば (ほかが先に入れた) 何もしない
pub fn confirm(ct: &mut Ct) {
    if dry() {
        return;
    }
    let Ct::New(c, info) = ct else { return };
    // reject の返すもの (答えの向き) は入れない
    if *info >= CT_IS_REPLY {
        return;
    }
    if c.status & IPS_CONFIRMED != 0 {
        return;
    }
    let tb = table();
    if tb.by_tuple.contains_key(&c.tuple[0]) || tb.by_tuple.contains_key(&c.tuple[1]) {
        return;
    }
    let mut conn = (**c).clone();
    conn.timeout += now();
    conn.status |= IPS_CONFIRMED;
    let (id, info) = (conn.id, *info);
    tb.by_tuple.insert(conn.tuple[0], (id, DIR_ORIG));
    tb.by_tuple.insert(conn.tuple[1], (id, DIR_REPLY));
    tb.conns.insert(id, conn);
    *ct = Ct::Known(id, info);
}

/// RST で reject したつながりは CLOSE に (nf_ct_set_closing → nf_conntrack_tcp_set_closing: 表にあるものだけ、
/// 時間切れは close の 10 秒)
pub fn set_closing(ct: &Ct) {
    if dry() {
        return;
    }
    let Ct::Known(id, _) = ct else { return };
    let Some(c) = table().conns.get_mut(id) else { return };
    let Proto::Tcp(t) = &mut c.proto else { return };
    let old = t.state;
    t.state = CL;
    if old == CL || c.status & IPS_FIXED_TIMEOUT != 0 {
        return;
    }
    c.timeout = now() + TIMEOUTS.get().tcp[CL as usize] * HZ_NS;
}

/// reject の返すものに、もとのつながりをつける (nf_conntrack_attach: もとが元の向きなら RELATED_REPLY)
pub fn attach(orig: &mut Ct) -> Ct {
    let rel = |i: u8| if i >= CT_IS_REPLY { CT_RELATED } else { CT_RELATED_REPLY };
    match core::mem::take(orig) {
        Ct::Known(id, i) => Ct::Known(id, rel(i)),
        Ct::New(c, i) => Ct::New(c, rel(i)),
        other => {
            *orig = other;
            Ct::None
        }
    }
}

// ---- ICMP ----

/// ICMP のエラーなら、中のパケットのつながりを探す (nf_conntrack_icmpv4_error → nf_conntrack_inet_error)。
/// エラーでなければ None、エラーなら Some (見つかったつながりと ctinfo。なければ None で、つながりなし)
fn icmp_error(ip: &[u8], ihl: usize, hook: u32) -> Option<Option<(u32, u8)>> {
    let h = ip.get(ihl..ihl + 8);
    let Some(h) = h else { return Some(None) };
    if KNOBS.get().checksum && hook == crate::nfhook::PRE_ROUTING && crate::nfhook::csum_fold(crate::nfhook::csum_add(0, &ip[ihl..])) != 0 {
        return Some(None);
    }
    let ty = h[0];
    if ty > 18 {
        return Some(None);
    }
    if !matches!(ty, 3 | 4 | 5 | 11 | 12) {
        return None;
    }
    let outer = be32(&ip[16..20]) as u128;
    Some(inner_error(ip, ihl + 8, |c, dir| c.tuple[dir as usize].dst == outer))
}

/// ICMP / ICMPv6 のエラーの中のパケット (at から) のつながり (nf_conntrack_inet_error): 中の組を逆にして引き、外の
/// あて先と、見つけた組のあて先が同じこと (same_dst)
fn inner_error(ip: &[u8], at: usize, same_dst: impl Fn(&Conn, u8) -> bool) -> Option<(u32, u8)> {
    let inner = ip.get(at..)?;
    if inner.len() < 20 {
        return None;
    }
    let orig = tuple_of(inner)?;
    let want = invert(&orig)?;
    let (id, dir) = find(&want)?;
    if !same_dst(&table().conns[&id], dir) {
        return None;
    }
    Some((id, if dir == DIR_REPLY { CT_RELATED_REPLY } else { CT_RELATED }))
}

/// nf_conntrack_icmp_packet と nf_conntrack_icmpv6_packet: 問い (ICMP は echo、timestamp、info、address mask、ICMPv6 は
/// echo と node information) だけがつながりになる
fn icmp_packet(c: &mut Conn) -> Res {
    let ty = (c.tuple[0].dport >> 8) as u8;
    let v6 = c.tuple[0].proto == 58;
    if !(if v6 { matches!(ty, 128 | 139) } else { matches!(ty, 8 | 13 | 15 | 17) }) {
        return Res::Invalid;
    }
    let to = if v6 { TIMEOUTS.get().icmpv6 } else { TIMEOUTS.get().icmp };
    refresh(c, to);
    Res::Accept
}

// ---- UDP ----

fn udp_packet(c: &mut Conn, ip: &[u8], ihl: usize, hook: u32) -> Res {
    // udp_error: 頭の長さ、チェックサム (PRE_ROUTING だけ。0 は見ない)
    let Some(uh) = ip.get(ihl..ihl + 8) else { return Res::Invalid };
    let ulen = be16(&uh[4..6]) as usize;
    if ulen > ip.len() - ihl || ulen < 8 {
        return Res::Invalid;
    }
    if KNOBS.get().checksum && hook == crate::nfhook::PRE_ROUTING && be16(&uh[6..8]) != 0 && !l4_ok(ip, ihl, 17) {
        return Res::Invalid;
    }
    if let Proto::Udp { stream_ts } = &mut c.proto {
        if c.status & IPS_CONFIRMED == 0 {
            *stream_ts = now() + 2 * HZ_NS;
        }
        let (to, ts) = (TIMEOUTS.get(), *stream_ts);
        if c.status & IPS_SEEN_REPLY != 0 {
            let mut extra = to.udp;
            let mut stream = false;
            if now() > ts {
                extra = to.udp_stream;
                stream = c.status & IPS_ASSURED == 0;
            }
            refresh(c, extra);
            if stream {
                c.status |= IPS_ASSURED;
            }
        } else {
            let u = to.udp;
            refresh(c, u);
        }
    }
    Res::Accept
}

fn l4_ok(ip: &[u8], ihl: usize, proto: u8) -> bool {
    let seg = &ip[ihl..];
    if ip[0] >> 4 == 6 {
        let src = u128::from_be_bytes(ip[8..24].try_into().unwrap());
        let dst = u128::from_be_bytes(ip[24..40].try_into().unwrap());
        return crate::nfhook::l4sum6(src, dst, proto, seg) == 0;
    }
    let mut ph = [0u8; 12];
    ph[0..8].copy_from_slice(&ip[12..20]);
    ph[9] = ip[9];
    ph[10..12].copy_from_slice(&(seg.len() as u16).to_be_bytes());
    crate::nfhook::csum_fold(crate::nfhook::csum_add(crate::nfhook::csum_add(0, &ph), seg)) == 0
}

// ---- TCP (nf_conntrack_proto_tcp.c) ----

fn tcp_index(f: u8) -> u8 {
    if f & 0x04 != 0 {
        RST_SET
    } else if f & 0x02 != 0 {
        if f & 0x10 != 0 { SYNACK_SET } else { SYN_SET }
    } else if f & 0x01 != 0 {
        FIN_SET
    } else if f & 0x10 != 0 {
        ACK_SET
    } else {
        NONE_SET
    }
}

/// tcp_valid_flags (ECE CWR PSH をのぞいて): SYN、SYN|URG、SYN|ACK、RST、RST|ACK、FIN|ACK、FIN|ACK|URG、ACK、ACK|URG
fn tcp_valid_flags(f: u8) -> bool {
    matches!(f & !(0x40 | 0x80 | 0x08), 0x02 | 0x22 | 0x12 | 0x04 | 0x14 | 0x11 | 0x31 | 0x10 | 0x30)
}

/// seq + 中身の長さ + SYN + FIN
fn seq_plus_len(seq: u32, ip_len: usize, ihl: usize, th: &[u8]) -> u32 {
    let doff = (th[12] >> 4) as usize * 4;
    let len = ip_len.saturating_sub(ihl + doff) as u32;
    seq.wrapping_add(len).wrapping_add((th[13] & 2 != 0) as u32).wrapping_add((th[13] & 1 != 0) as u32)
}

/// tcp_options: window scale と SACK_PERM
fn tcp_options(th: &[u8], side: &mut TcpSide) {
    let doff = (th[12] >> 4) as usize * 4;
    let opt = &th[20.min(th.len())..doff.min(th.len())];
    if opt.is_empty() {
        return;
    }
    side.td_scale = 0;
    side.flags &= F_BE_LIBERAL;
    let mut i = 0;
    while i < opt.len() {
        match opt[i] {
            0 => return,
            1 => i += 1,
            k => {
                let Some(&l) = opt.get(i + 1) else { return };
                if l < 2 || i + l as usize > opt.len() {
                    return;
                }
                if k == 4 && l == 2 {
                    side.flags |= F_SACK_PERM;
                } else if k == 3 && l == 3 {
                    side.flags |= F_WINDOW_SCALE;
                    side.td_scale = opt[i + 2].min(14);
                }
                i += l as usize;
            }
        }
    }
}

/// tcp_sack: SACK のいちばん大きなものを sack に
fn tcp_sack(th: &[u8], sack: &mut u32) {
    let doff = (th[12] >> 4) as usize * 4;
    let opt = &th[20.min(th.len())..doff.min(th.len())];
    let mut i = 0;
    while i < opt.len() {
        match opt[i] {
            0 => return,
            1 => i += 1,
            k => {
                let Some(&l) = opt.get(i + 1) else { return };
                if l < 2 || i + l as usize > opt.len() {
                    return;
                }
                if k == 5 && l >= 10 && (l - 2) % 8 == 0 {
                    let mut j = i + 2;
                    while j + 8 <= i + l as usize {
                        let e = be32(&opt[j + 4..j + 8]);
                        if after(e, *sack) {
                            *sack = e;
                        }
                        j += 8;
                    }
                    return;
                }
                i += l as usize;
            }
        }
    }
}

fn tcp_new(t: &mut TcpCt, ip: &[u8], ihl: usize, th: &[u8]) -> bool {
    let ns = TCP_TABLE[0][tcp_index(th[13]) as usize][NO as usize];
    if ns >= IV {
        return false;
    }
    let win = be16(&th[14..16]) as u32;
    let end = seq_plus_len(be32(&th[4..8]), ip.len(), ihl, th);
    if ns == SS {
        *t = TcpCt::default();
        t.seen[0].td_end = end;
        t.seen[0].td_maxwin = if win == 0 { 1 } else { win };
        t.seen[0].td_maxend = end;
        tcp_options(th, &mut t.seen[0]);
    } else if !KNOBS.get().tcp_loose {
        return false;
    } else {
        *t = TcpCt::default();
        t.seen[0].td_end = end;
        t.seen[0].td_maxwin = if win == 0 { 1 } else { win };
        t.seen[0].td_maxend = end.wrapping_add(t.seen[0].td_maxwin);
        t.seen[0].flags = F_SACK_PERM | F_BE_LIBERAL;
        t.seen[1].flags = F_SACK_PERM | F_BE_LIBERAL;
    }
    t.last_index = NONE_SET;
    true
}

#[derive(PartialEq)]
enum Win {
    Accept,
    Ignore,
    Invalid,
}

/// tcp_init_sender: SYN-ACK (SYN への答え) か、同時に開くときの答えの向きの SYN
fn tcp_init_sender(t: &mut TcpCt, dir: usize, th: &[u8], end: u32, win: u32) {
    let (s, r) = (dir, 1 - dir);
    t.seen[s].td_end = end;
    t.seen[s].td_maxend = end;
    t.seen[s].td_maxwin = if win == 0 { 1 } else { win };
    let mut side = t.seen[s];
    tcp_options(th, &mut side);
    t.seen[s] = side;
    if !(t.seen[s].flags & F_WINDOW_SCALE != 0 && t.seen[r].flags & F_WINDOW_SCALE != 0) {
        t.seen[s].td_scale = 0;
        t.seen[r].td_scale = 0;
    }
}

fn tcp_in_window(t: &mut TcpCt, dir: usize, index: u8, ip: &[u8], ihl: usize, th: &[u8]) -> Win {
    let liberal_all = KNOBS.get().tcp_be_liberal;
    let mut seq = be32(&th[4..8]);
    let mut ack = be32(&th[8..12]);
    let mut sack = ack;
    let win_raw = be16(&th[14..16]);
    let mut win = win_raw as u32;
    let mut end = seq_plus_len(seq, ip.len(), ihl, th);
    let flags = th[13];
    let (syn, ackf, rst) = (flags & 2 != 0, flags & 0x10 != 0, flags & 4 != 0);
    if t.seen[1 - dir].flags & F_SACK_PERM != 0 {
        tcp_sack(th, &mut sack);
    }
    let state = t.state;
    let maxackwin = |s: &TcpSide| s.td_maxwin.max(MAXACKWINCONST);
    let invalid = |t: &TcpCt, r: Win| if t.seen[dir].flags & F_BE_LIBERAL != 0 || liberal_all { Win::Accept } else { r };
    if t.seen[dir].td_maxwin == 0 {
        if syn {
            tcp_init_sender(t, dir, th, end, win);
            if !ackf {
                return Win::Accept;
            }
        } else {
            let s = dir;
            t.seen[s].td_end = end;
            let swin = win << t.seen[s].td_scale;
            t.seen[s].td_maxwin = if swin == 0 { 1 } else { swin };
            t.seen[s].td_maxend = end.wrapping_add(t.seen[s].td_maxwin);
            let r = 1 - dir;
            if t.seen[r].td_maxwin == 0 {
                t.seen[r].td_end = sack;
                t.seen[r].td_maxend = sack;
            } else if sack == t.seen[r].td_end.wrapping_add(1) {
                t.seen[r].td_end = t.seen[r].td_end.wrapping_add(1);
            }
        }
    } else if syn && after(end, t.seen[dir].td_end) && (state == SS || state == SR) {
        tcp_init_sender(t, dir, th, end, win);
        if dir == 1 && !ackf {
            return Win::Accept;
        }
    }
    let r = 1 - dir;
    // ACK のないもの、ACK と RST で ack が 0 のもの (こわれた TCP) は、受ける側の終わりを ack に
    if !ackf || (flags & 0x14 == 0x14 && ack == 0) {
        ack = t.seen[r].td_end;
        sack = ack;
    }
    if rst && seq == 0 && state == SS {
        seq = t.seen[dir].td_end;
        end = seq;
    }
    let seq_ok = before(seq, t.seen[dir].td_maxend.wrapping_add(1));
    if !seq_ok {
        let overshot = end.wrapping_sub(t.seen[dir].td_maxend).wrapping_add(1);
        let ack_ok = after(sack, t.seen[r].td_end.wrapping_sub(maxackwin(&t.seen[dir])).wrapping_sub(1));
        let in_recv = t.seen[r].td_maxwin != 0 && after(end, t.seen[dir].td_end.wrapping_sub(t.seen[r].td_maxwin).wrapping_sub(1));
        if in_recv && ack_ok && overshot <= t.seen[r].td_maxwin && before(sack, t.seen[r].td_end.wrapping_add(1)) {
            t.seen[dir].td_end = end;
            t.seen[dir].flags |= F_DATA_UNACKNOWLEDGED;
            return invalid(t, Win::Ignore);
        }
        return invalid(t, Win::Invalid);
    }
    if !before(sack, t.seen[r].td_end.wrapping_add(1)) {
        return invalid(t, Win::Invalid);
    }
    let in_recv = t.seen[r].td_maxwin == 0 || after(end, t.seen[dir].td_end.wrapping_sub(t.seen[r].td_maxwin).wrapping_sub(1));
    if !in_recv {
        return invalid(t, Win::Ignore);
    }
    if !after(sack, t.seen[r].td_end.wrapping_sub(maxackwin(&t.seen[dir])).wrapping_sub(1)) {
        return invalid(t, Win::Ignore);
    }
    if !syn {
        win <<= t.seen[dir].td_scale;
    }
    let swin = win.wrapping_add(sack.wrapping_sub(ack));
    if t.seen[dir].td_maxwin < swin {
        t.seen[dir].td_maxwin = swin;
    }
    if after(end, t.seen[dir].td_end) {
        t.seen[dir].td_end = end;
        t.seen[dir].flags |= F_DATA_UNACKNOWLEDGED;
    }
    if ackf {
        if t.seen[dir].flags & F_MAXACK_SET == 0 {
            t.seen[dir].td_maxack = ack;
            t.seen[dir].flags |= F_MAXACK_SET;
        } else if after(ack, t.seen[dir].td_maxack) {
            t.seen[dir].td_maxack = ack;
        }
    }
    if t.seen[r].td_maxwin != 0 && after(end, t.seen[dir].td_maxend) {
        t.seen[r].td_maxwin = t.seen[r].td_maxwin.wrapping_add(end.wrapping_sub(t.seen[dir].td_maxend));
    }
    if after(sack.wrapping_add(win), t.seen[r].td_maxend.wrapping_sub(1)) {
        t.seen[r].td_maxend = sack.wrapping_add(win);
        if win == 0 {
            t.seen[r].td_maxend = t.seen[r].td_maxend.wrapping_add(1);
        }
    }
    if ack == t.seen[r].td_end {
        t.seen[r].flags &= !F_DATA_UNACKNOWLEDGED;
    }
    if index == ACK_SET {
        if t.last_dir as usize == dir && t.last_seq == seq && t.last_ack == ack && t.last_end == end && t.last_win == win_raw {
            t.retrans = t.retrans.saturating_add(1);
        } else {
            t.last_dir = dir as u8;
            t.last_seq = seq;
            t.last_ack = ack;
            t.last_end = end;
            t.last_win = win_raw;
            t.retrans = 0;
        }
    }
    Win::Accept
}

fn tcp_packet(c: &mut Conn, ip: &[u8], ihl: usize, dir: u8, hook: u32) -> Res {
    let Some(th) = ip.get(ihl..ihl + 20) else { return Res::Invalid };
    let doff = (th[12] >> 4) as usize * 4;
    // tcp_error
    if doff < 20 || ip.len() - ihl < doff {
        return Res::Invalid;
    }
    let th = &ip[ihl..ihl + doff];
    if KNOBS.get().checksum && hook == crate::nfhook::PRE_ROUTING && !l4_ok(ip, ihl, 6) {
        return Res::Invalid;
    }
    if !tcp_valid_flags(th[13]) {
        return Res::Invalid;
    }
    let confirmed = c.status & IPS_CONFIRMED != 0;
    let (status, seen_reply, assured) = (c.status, c.status & IPS_SEEN_REPLY != 0, c.status & IPS_ASSURED != 0);
    let Proto::Tcp(t) = &mut c.proto else { return Res::Accept };
    if !confirmed && !tcp_new(t, ip, ihl, th) {
        return Res::Invalid;
    }
    let d = dir as usize;
    let mut old = t.state;
    let index = tcp_index(th[13]);
    let mut new = TCP_TABLE[d][index as usize][old as usize];
    let seq = be32(&th[4..8]);
    let ackseq = be32(&th[8..12]);
    let mut skip_window = false;
    // sSS へもどる (TIME_WAIT から SYN): しまったか RST で終わったつながりを開きなおすなら、消してもう一度
    if new == SS && old >= TW && ((t.seen[d].flags | t.seen[1 - d].flags) & F_CLOSE_INIT != 0 || (t.last_dir == dir && t.last_index == RST_SET)) {
        return Res::Repeat;
    }
    if new == IG || (new == SS && old >= TW) {
        // 見なかったことにする (sIG): 前に見なかった SYN への SYN-ACK なら、それでそろえる
        if index == SYNACK_SET && t.last_index == SYN_SET && t.last_dir != dir && ackseq == t.last_end {
            old = SS;
            new = SR;
            let ld = t.last_dir as usize;
            t.seen[ld].td_end = t.last_end;
            t.seen[ld].td_maxend = t.last_end;
            t.seen[ld].td_maxwin = if t.last_win == 0 { 1 } else { t.last_win as u32 };
            t.seen[ld].td_scale = t.last_wscale;
            t.last_flags &= !F_EXP_CHALLENGE_ACK;
            t.seen[ld].flags = t.last_flags;
            t.seen[d] = TcpSide::default();
        } else {
            t.last_index = index;
            t.last_dir = dir;
            t.last_seq = seq;
            t.last_end = seq_plus_len(seq, ip.len(), ihl, th);
            t.last_win = be16(&th[14..16]);
            if index == SYN_SET && dir == DIR_ORIG {
                let mut sd = TcpSide::default();
                t.last_flags = 0;
                t.last_wscale = 0;
                tcp_options(th, &mut sd);
                if sd.flags & F_WINDOW_SCALE != 0 {
                    t.last_flags |= F_WINDOW_SCALE;
                    t.last_wscale = sd.td_scale;
                }
                if sd.flags & F_SACK_PERM != 0 {
                    t.last_flags |= F_SACK_PERM;
                }
                if old == LA {
                    t.last_flags |= F_EXP_CHALLENGE_ACK;
                }
            }
            if old == SS && index == ACK_SET && dir == DIR_REPLY {
                t.last_ack = ackseq;
            }
            return Res::Accept;
        }
    }
    match new {
        IV => return Res::Invalid,
        TW => {
            // RFC 5961 の challenge ACK は、さいごの FIN への ACK ではない
            if old == LA && index == ACK_SET && t.last_dir != dir && t.last_index == SYN_SET && t.last_flags & F_EXP_CHALLENGE_ACK != 0 {
                t.last_flags &= !F_EXP_CHALLENGE_ACK;
                return Res::Accept;
            }
        }
        S2 => t.last_flags |= F_SIMULTANEOUS_OPEN,
        SR => {
            if dir == DIR_REPLY && index == ACK_SET && t.last_flags & F_SIMULTANEOUS_OPEN != 0 {
                new = ES;
            }
        }
        CL if index == RST_SET => 'rst: {
            // しまいかけのもの (tcp_can_early_drop) は窓を見ない
            if matches!(old, FW | LA | TW | CL | CW) {
                skip_window = true;
                break 'rst;
            }
            let established = old == ES && assured;
            if t.seen[1 - d].flags & F_MAXACK_SET != 0 && t.last_index != SYN_SET {
                if seq == 0 && !established {
                    break 'rst;
                }
                if before(seq, t.seen[1 - d].td_maxack) && !KNOBS.get().tcp_ignore_invalid_rst {
                    return Res::Invalid;
                }
                if !established || seq == t.seen[1 - d].td_maxack {
                    break 'rst;
                }
                if t.last_index == ACK_SET && t.last_dir == dir && seq == t.last_end {
                    break 'rst;
                }
                new = old;
            }
            if ((seen_reply && t.last_index == SYN_SET) || (!assured && t.last_index == ACK_SET)) && ackseq == t.last_end {
                skip_window = true;
                break 'rst;
            }
            if old == SS && t.last_index == ACK_SET && t.last_dir == DIR_REPLY && seq == t.last_ack {
                skip_window = true;
            }
        }
        _ => {}
    }
    if !skip_window {
        match tcp_in_window(t, d, index, ip, ihl, th) {
            Win::Ignore => return Res::Accept,
            Win::Invalid => {
                // nf_tcp_handle_invalid: ASSURED の FIN / RST の外れは、時間を短く
                let mut shorten = false;
                if assured && status & IPS_FIXED_TIMEOUT == 0 && matches!(index, RST_SET | FIN_SET) {
                    if t.last_dir != dir && matches!(t.last_index, FIN_SET | RST_SET) {
                        shorten = true;
                    } else {
                        t.last_index = index;
                        t.last_dir = dir;
                    }
                }
                if shorten {
                    let ex = expires_ns_raw(c.timeout, confirmed);
                    let un = TIMEOUTS.get().tcp[UNACK] * HZ_NS;
                    if ex >= 120 * HZ_NS && ex > un {
                        c.timeout = now() + un;
                    }
                }
                return Res::Invalid;
            }
            Win::Accept => {}
        }
    }
    t.last_index = index;
    t.last_dir = dir;
    t.state = new;
    if old != new && new == FW {
        t.seen[d].flags |= F_CLOSE_INIT;
    }
    let to = TIMEOUTS.get();
    let unack = (t.seen[0].flags | t.seen[1].flags) & F_DATA_UNACKNOWLEDGED != 0;
    let mut timeout = if t.retrans >= KNOBS.get().tcp_max_retrans && to.tcp[new as usize] > to.tcp[RETRANS] {
        to.tcp[RETRANS]
    } else if index == RST_SET {
        to.tcp[CL as usize]
    } else if unack && to.tcp[new as usize] > to.tcp[UNACK] {
        to.tcp[UNACK]
    } else if t.last_win == 0 && to.tcp[new as usize] > to.tcp[RETRANS] {
        to.tcp[RETRANS]
    } else {
        to.tcp[new as usize]
    };
    let rst = th[13] & 4 != 0;
    if !seen_reply {
        if rst {
            return Res::Kill;
        }
        if index == SYN_SET && old == SS {
            return Res::Accept;
        }
        if new == ES && timeout > to.tcp[UNACK] {
            timeout = to.tcp[UNACK];
        }
    } else if !assured && (old == SR || old == ES) && new == ES {
        c.status |= IPS_ASSURED;
    }
    refresh(c, timeout);
    Res::Accept
}

fn expires_ns_raw(timeout: u64, confirmed: bool) -> u64 {
    if confirmed { timeout.saturating_sub(now()) } else { timeout }
}

// ---- /proc/net/nf_conntrack (ct_seq_show) ----

fn dotted(a: u32) -> String {
    let b = a.to_be_bytes();
    alloc::format!("{}.{}.{}.{}", b[0], b[1], b[2], b[3])
}

/// /proc/net/nf_conntrack の番地 (IPv4 は 4 つの 10 進、IPv6 は %pI6: 4 けたの 16 進を 8 つ)
fn addr_text(l3: u8, a: u128) -> String {
    if l3 == 6 {
        (0..8).map(|i| alloc::format!("{:04x}", (a >> (112 - 16 * i)) as u16)).collect::<alloc::vec::Vec<_>>().join(":")
    } else {
        dotted(a as u32)
    }
}

/// AI の見る口の番地 (IPv6 はちぢめた形)
fn addr_short(l3: u8, a: u128) -> String {
    if l3 == 6 { alloc::format!("{}", core::net::Ipv6Addr::from(a)) } else { dotted(a as u32) }
}

fn tuple_text(s: &mut String, t: &Tuple) {
    let _ = write!(s, "src={} dst={} ", addr_text(t.l3, t.src), addr_text(t.l3, t.dst));
    match t.proto {
        6 | 17 => {
            let _ = write!(s, "sport={} dport={} ", t.sport, t.dport);
        }
        1 | 58 => {
            let _ = write!(s, "type={} code={} id={} ", t.dport >> 8, t.dport & 0xff, t.sport);
        }
        _ => {}
    }
}

fn proto_name(p: u8) -> &'static str {
    match p {
        1 => "icmp",
        6 => "tcp",
        17 => "udp",
        58 => "icmpv6",
        33 => "dccp",
        47 => "gre",
        132 => "sctp",
        136 => "udplite",
        _ => "unknown",
    }
}

/// 1 つの行
pub fn line(c: &Conn) -> String {
    let mut s = String::new();
    let p = c.tuple[0].proto;
    let (l3, l3num) = if c.tuple[0].l3 == 6 { ("ipv6", 10) } else { ("ipv4", 2) };
    let _ = write!(s, "{:<8} {} {:<8} {} ", l3, l3num, proto_name(p), p);
    let _ = write!(s, "{} ", expires_ns(c) / HZ_NS);
    if let Proto::Tcp(t) = &c.proto {
        let _ = write!(s, "{} ", TCP_NAMES[t.state.min(9) as usize]);
    }
    tuple_text(&mut s, &c.tuple[0]);
    if let Some(a) = c.acct {
        let _ = write!(s, "packets={} bytes={} ", a[0][0], a[0][1]);
    }
    if c.status & IPS_SEEN_REPLY == 0 {
        s.push_str("[UNREPLIED] ");
    }
    tuple_text(&mut s, &c.tuple[1]);
    if let Some(a) = c.acct {
        let _ = write!(s, "packets={} bytes={} ", a[1][0], a[1][1]);
    }
    if c.status & IPS_ASSURED != 0 {
        s.push_str("[ASSURED] ");
    }
    let _ = writeln!(s, "mark={} use=2", c.mark);
    s
}

pub fn proc_text() -> String {
    gc();
    let mut s = String::new();
    for c in table().conns.values() {
        s.push_str(&line(c));
    }
    s
}

/// つながりのまとめ (/proc/ai/fw): 数と、状態ごとの数
pub fn summary() -> String {
    gc();
    let mut by: BTreeMap<&'static str, usize> = BTreeMap::new();
    for c in table().conns.values() {
        let k = match &c.proto {
            Proto::Tcp(t) => TCP_NAMES[t.state.min(9) as usize],
            Proto::Udp { .. } => {
                if c.status & IPS_ASSURED != 0 {
                    "udp_assured"
                } else if c.status & IPS_SEEN_REPLY != 0 {
                    "udp_replied"
                } else {
                    "udp_unreplied"
                }
            }
            Proto::Icmp if c.tuple[0].proto == 58 => "icmpv6",
            Proto::Icmp => "icmp",
            Proto::Generic => "other",
        };
        *by.entry(k).or_insert(0) += 1;
    }
    let mut s = alloc::format!("{{\"conntrack\":{{\"count\":{},\"max\":{}", table().conns.len(), max());
    for (k, v) in by {
        let _ = write!(s, ",\"{}\":{}", k, v);
    }
    s.push_str("}}\n");
    s
}

/// つながりを JSON で (/proc/ai/fw の ct: 1 行 1 つ)
pub fn json_lines(limit: usize) -> String {
    gc();
    let mut s = String::new();
    for c in table().conns.values().take(limit) {
        let st = match &c.proto {
            Proto::Tcp(t) => TCP_NAMES[t.state.min(9) as usize],
            _ => "",
        };
        let t = &c.tuple[0];
        let _ = writeln!(
            s,
            "{{\"ct\":{{\"id\":{},\"proto\":\"{}\",\"state\":\"{}\",\"src\":\"{}\",\"dst\":\"{}\",\"sport\":{},\"dport\":{},\"replied\":{},\"assured\":{},\"expires_s\":{},\"mark\":{}}}}}",
            c.id,
            proto_name(t.proto),
            st,
            addr_short(t.l3, t.src),
            addr_short(t.l3, t.dst),
            t.sport,
            t.dport,
            c.status & IPS_SEEN_REPLY != 0,
            c.status & IPS_ASSURED != 0,
            expires_ns(c) / HZ_NS,
            c.mark
        );
    }
    s
}

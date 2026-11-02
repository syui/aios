// nftables のフック (doc/aios.md の「ファイアウォール (nftables)」の段階 2): パケットが決まり (nftables.rs の hook) を
// とおる道と、reject の返すもの。Linux 7.3 の nf_hook (ip_rcv、ip_local_deliver、__ip_local_out、ip_output)、
// nf_reject_ipv4.c (nf_send_reset、nf_send_unreach)、icmp.c (__icmp_send、icmp_unreach)、nf_log_syslog.c
// (nf_log_ip_packet) と同じ形と答え。
//
// - パケット (Pkt): IPv4 の頭から tot_len まで、受けたものならリンク層の頭 (Ethernet の 14 バイト)、口 (lo 1、eth0 2)、
//   mark、priority、ソケットの持ち主 (meta skuid / skgid)
// - 道 (path):
//   - eth0 から来たもの (net.rs の受け): PRE_ROUTING (iif eth0)、aios あて (自分のアドレス、ブロードキャスト、マルチ
//     キャスト) なら LOCAL_IN。ほかの口へは送らない (ip_forward は 0) ので FORWARD はない
//   - eth0 へ出すもの: LOCAL_OUT と POST_ROUTING (oif eth0)。smoltcp が作る TCP と ICMP は出す前 (net.rs の Tx)、UDP は
//     sendmsg のとき (inet.rs: Linux と同じく、出す側で落とされると EPERM を返すため)。DHCP は通さない (Linux の
//     dhclient や ipconfig は AF_PACKET で、netfilter を通らない。受けるほうは通すが、落とされても DHCP には届く)
//   - 中どうし (lo): aios はパケットを作らない (inet.rs) ので、Linux が lo に流すのと同じ形のパケットを作って
//     LOCAL_OUT、POST_ROUTING (oif lo)、PRE_ROUTING、LOCAL_IN (iif lo) を順に。TCP はつなぐときの SYN と SYN-ACK
//     (と断るときの RST)、UDP はデータグラムごと
// - reject: 判定は drop。返すもの (TCP の RST か ICMP の到達不能) は Linux と同じ形で作り、それも決まりの道をとおす
//   (自分あてなら lo の 4 つ、外へは eth0 の LOCAL_OUT と POST_ROUTING)。ICMP は lo でなければ相手ごとに 1 秒に 1 つ
//   (はじめは 6 つまで: Linux の icmp_ratelimit と XRLIM_BURST_FACTOR)
// - log: Linux の nf_log_syslog と同じ 1 行を kmsg に (dmesg で見える。シリアルの画面には出さない)
use crate::conntrack::{self, Ct};
use crate::nftables;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

pub const IF_LO: u32 = 1;
pub const IF_ETH0: u32 = 2;

pub const PRE_ROUTING: u32 = 0;
pub const LOCAL_IN: u32 = 1;
pub const LOCAL_OUT: u32 = 3;
pub const POST_ROUTING: u32 = 4;

pub const PROTO_ICMP: u8 = 1;
pub const PROTO_TCP: u8 = 6;
pub const PROTO_UDP: u8 = 17;

/// PACKET_HOST など (skb->pkt_type)
pub const PACKET_HOST: u8 = 0;
pub const PACKET_BROADCAST: u8 = 1;
pub const PACKET_MULTICAST: u8 = 2;

/// TCP の頭のフラグ
pub const FIN: u8 = 0x01;
pub const SYN: u8 = 0x02;
pub const RST: u8 = 0x04;
pub const PSH: u8 = 0x08;
pub const ACK: u8 = 0x10;

/// 決まりを通るパケット
pub struct Pkt<'a> {
    /// IPv4 の頭から tot_len まで
    pub ip: &'a mut [u8],
    /// 受けたもの: リンク層の頭 (Ethernet の 14 バイト。lo では 0 の MAC と 0x0800)
    pub ll: Option<&'a [u8]>,
    pub iif: u32,
    pub oif: u32,
    pub mark: u32,
    pub priority: u32,
    /// 出すソケットの持ち主 (fsuid, fsgid)。なければ meta skuid は当たらない
    pub sk: Option<(u32, u32)>,
    pub pkttype: u8,
    /// つながり (conntrack.rs。skb->_nfct)
    pub ct: Ct,
}

impl Pkt<'_> {
    /// 頭がおかしくないか (nft_set_pktinfo_ipv4_validate)
    pub fn valid(&self) -> bool {
        let ip = &*self.ip;
        if ip.len() < 20 || ip[0] >> 4 != 4 || ip[0] & 15 < 5 {
            return false;
        }
        let tot = u16::from_be_bytes([ip[2], ip[3]]) as usize;
        let thoff = (ip[0] & 15) as usize * 4;
        ip.len() >= tot && tot >= thoff
    }
    pub fn len(&self) -> usize {
        self.ip.len()
    }
    pub fn thoff(&self) -> usize {
        (self.ip[0] & 15) as usize * 4
    }
    pub fn tprot(&self) -> u8 {
        self.ip[9]
    }
    pub fn fragoff(&self) -> u16 {
        u16::from_be_bytes([self.ip[6], self.ip[7]]) & 0x1fff
    }
    pub fn daddr(&self) -> u32 {
        be32(&self.ip[16..20])
    }
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

fn be16(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}

/// 判定
pub enum Verdict {
    Accept,
    Drop { reject: Option<Reject>, why: Why },
}

/// reject の返すもの: TCP の RST か、ICMP の到達不能 (code は ICMP の番号。icmpx は直したもの)
#[derive(Clone, Copy)]
pub struct Reject {
    pub rst: bool,
    pub code: u8,
}

/// 落としたところ (/proc/ai/why に): 表、鎖、決まりの番号 (0 なら鎖の policy)
#[derive(Clone)]
pub struct Why {
    pub family: &'static str,
    pub table: String,
    pub chain: String,
    pub handle: u64,
}

impl Why {
    pub fn text(&self) -> String {
        if self.handle == 0 {
            alloc::format!("nftables {} {} {} policy drop", self.family, self.table, self.chain)
        } else {
            alloc::format!("nftables {} {} {} handle {}", self.family, self.table, self.chain, self.handle)
        }
    }
}

/// 決まりを入れたか (基の鎖が 1 つでもあるか、conntrack が動いている。なければパケットを作らない)
pub fn active() -> bool {
    nftables::active() || nftables::ct_on()
}

/// フックを順に。落ちたら (reject, わけ, フック)。落としたものは /proc/ai/fw の輪にも
fn hooks(hs: &[u32], p: &mut Pkt) -> Option<(Option<Reject>, Why, u32)> {
    for &h in hs {
        if let Verdict::Drop { reject, why } = nftables::hook(h, p) {
            remember_drop(p, h, &why, reject);
            return Some((reject, why, h));
        }
    }
    None
}

/// 道をとおった答え。drop なら落としたところ、それと返すもの (届いたものだけ)
pub enum Path {
    Pass,
    /// 出す側 (LOCAL_OUT、POST_ROUTING) で落ちた: sendmsg は EPERM
    Out(Option<Vec<u8>>, Why),
    /// 受ける側 (PRE_ROUTING、LOCAL_IN) で落ちた
    In(Option<Vec<u8>>, Why),
}

/// 中どうし (lo) のパケット。返すもの (reject の RST か ICMP) は、それも lo の道をとおって届いたものだけ
pub fn lo(ip: &mut [u8], sk: Option<(u32, u32)>, mark: u32, priority: u32) -> Path {
    lo_depth(ip, sk, mark, priority, Ct::None, 0)
}

const LO_LL: [u8; 14] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 8, 0];

fn lo_depth(ip: &mut [u8], sk: Option<(u32, u32)>, mark: u32, priority: u32, ct: Ct, depth: u32) -> Path {
    let r = {
        let mut p = Pkt { ip: &mut *ip, ll: None, iif: 0, oif: IF_LO, mark, priority, sk, pkttype: PACKET_HOST, ct };
        if let Some(d) = hooks(&[LOCAL_OUT, POST_ROUTING], &mut p) {
            Some((d, true, p.mark, conntrack::attach(&mut p.ct)))
        } else {
            // lo をとおっても mark と priority とつながりはそのまま (loopback_xmit)。ソケットは外れる (skb_orphan)
            let (mark, priority, ct) = (p.mark, p.priority, core::mem::take(&mut p.ct));
            let mut p = Pkt { ip: &mut *ip, ll: Some(&LO_LL), iif: IF_LO, oif: 0, mark, priority, sk: None, pkttype: PACKET_HOST, ct };
            hooks(&[PRE_ROUTING, LOCAL_IN], &mut p).map(|d| (d, false, p.mark, conntrack::attach(&mut p.ct)))
        }
    };
    let Some(((rj, why, hook), out, mark, rct)) = r else { return Path::Pass };
    // 返すもの: 自分あて (送ったもの) へ、lo の道で。もとのつながりを持つ (nf_ct_attach: related。RST なら TCP の
    // つながりは CLOSE に: nf_ct_set_closing)
    let reply = rj.filter(|_| depth == 0).and_then(|rj| reply(ip, rj, hook, if out { 0 } else { IF_LO }, PACKET_HOST, mark).map(|r| (r, rj.rst))).and_then(|(mut r, rst)| {
        if rst {
            conntrack::set_closing(&rct);
        }
        match lo_depth(&mut r, None, 0, 0, rct, depth + 1) {
            Path::Pass => Some(r),
            _ => None,
        }
    });
    if out { Path::Out(reply, why) } else { Path::In(reply, why) }
}

/// 外 (eth0) へ出すもの: LOCAL_OUT と POST_ROUTING。返すもの (自分あて) は lo の道で届いたものだけ
pub fn out_eth(ip: &mut [u8], sk: Option<(u32, u32)>, mark: u32, priority: u32) -> Path {
    let r = {
        let mut p = Pkt { ip: &mut *ip, ll: None, iif: 0, oif: IF_ETH0, mark, priority, sk, pkttype: PACKET_HOST, ct: Ct::None };
        hooks(&[LOCAL_OUT, POST_ROUTING], &mut p).map(|d| (d, p.mark, conntrack::attach(&mut p.ct)))
    };
    let Some(((rj, why, hook), mark, rct)) = r else { return Path::Pass };
    let reply = rj.and_then(|rj| reply(ip, rj, hook, 0, PACKET_HOST, mark).map(|r| (r, rj.rst))).and_then(|(mut r, rst)| {
        if rst {
            conntrack::set_closing(&rct);
        }
        match lo_depth(&mut r, None, 0, 0, rct, 1) {
            Path::Pass => Some(r),
            _ => None,
        }
    });
    Path::Out(reply, why)
}

/// eth0 から来たもの (frame は Ethernet の頭から)。local は aios あてか (LOCAL_IN をとおす)。落ちたら、eth0 へ返す
/// フレーム (LOCAL_OUT と POST_ROUTING をとおったもの) も
pub enum EthIn {
    Pass,
    Drop(Option<Vec<u8>>),
}

pub fn in_eth(frame: &mut [u8], local: bool, pkttype: u8) -> EthIn {
    let (ll, ip) = frame.split_at_mut(14);
    let tot = u16::from_be_bytes([ip[2], ip[3]]) as usize;
    let ip = &mut ip[..tot];
    let ll: &[u8] = ll;
    let r = {
        let mut p = Pkt { ip: &mut *ip, ll: Some(ll), iif: IF_ETH0, oif: 0, mark: 0, priority: 0, sk: None, pkttype, ct: Ct::None };
        let hs: &[u32] = if local { &[PRE_ROUTING, LOCAL_IN] } else { &[PRE_ROUTING] };
        hooks(hs, &mut p).map(|d| (d, p.mark, conntrack::attach(&mut p.ct)))
    };
    let Some(((rj, _why, hook), mark, rct)) = r else { return EthIn::Pass };
    let Some(rj) = rj else { return EthIn::Drop(None) };
    let Some(mut r) = reply(ip, rj, hook, IF_ETH0, pkttype, mark) else { return EthIn::Drop(None) };
    if rj.rst {
        conntrack::set_closing(&rct);
    }
    if let Path::Pass = out_reply(&mut r, rct) {
        let mut f = Vec::with_capacity(14 + r.len());
        f.extend_from_slice(&ll[6..12]);
        f.extend_from_slice(&ll[0..6]);
        f.extend_from_slice(&[8, 0]);
        f.extend_from_slice(&r);
        return EthIn::Drop(Some(f));
    }
    EthIn::Drop(None)
}

/// 外へ返すもの (reject の RST と ICMP): LOCAL_OUT と POST_ROUTING (返すもののために返すものは作らない)
fn out_reply(r: &mut [u8], ct: Ct) -> Path {
    let mut p = Pkt { ip: r, ll: None, iif: 0, oif: IF_ETH0, mark: 0, priority: 0, sk: None, pkttype: PACKET_HOST, ct };
    match hooks(&[LOCAL_OUT, POST_ROUTING], &mut p) {
        None => Path::Pass,
        Some((_, why, _)) => Path::Out(None, why),
    }
}


// ---- 落としたものの輪 (/proc/ai/fw の drop。Linux にはない: nft monitor trace や log の決まりを入れないと見えない) ----

struct Drop {
    ns: u64,
    hook: u32,
    iif: u32,
    oif: u32,
    head: [u8; 40],
    len: u16,
    why: String,
    reject: Option<Reject>,
    uid: Option<u32>,
}

static DROPS: crate::cell::Bkl<(Vec<Drop>, usize, u64)> = crate::cell::Bkl::new((Vec::new(), 0, 0));
const DROPS_MAX: usize = 64;

fn remember_drop(p: &Pkt, hook: u32, why: &Why, reject: Option<Reject>) {
    if nftables::tracing() {
        return;
    }
    let mut head = [0u8; 40];
    let n = p.ip.len().min(40);
    head[..n].copy_from_slice(&p.ip[..n]);
    let d = Drop { ns: crate::timer::uptime_ns(), hook, iif: p.iif, oif: p.oif, head, len: p.ip.len() as u16, why: why.text(), reject, uid: p.sk.map(|s| s.0) };
    let (v, at, total) = DROPS.get();
    if v.len() < DROPS_MAX {
        v.push(d);
    } else {
        v[*at] = d;
    }
    *at = (*at + 1) % DROPS_MAX;
    *total += 1;
}

pub fn hook_name(h: u32) -> &'static str {
    ["prerouting", "input", "forward", "output", "postrouting", "ingress"].get(h as usize).copied().unwrap_or("?")
}

/// 落としたもの (新しい順、1 行 1 つの JSON) と、いままでの数
pub fn drops_json() -> String {
    let (v, at, total) = DROPS.get();
    let now = crate::timer::uptime_ns();
    let mut s = alloc::format!("{{\"drops\":{},\"kept\":{}}}\n", total, v.len());
    let n = v.len();
    for k in 0..n {
        let d = &v[(*at + n - 1 - k) % n];
        let ip = &d.head;
        let ihl = (ip[0] & 15) as usize * 4;
        let proto = ip[9];
        let (sp, dp, flags) = if (proto == 6 || proto == 17) && ihl + 4 <= 40 {
            let f = if proto == 6 && ihl + 14 <= 40 { ip[ihl + 13] } else { 0 };
            (be16(&ip[ihl..ihl + 2]), be16(&ip[ihl + 2..ihl + 4]), f)
        } else {
            (0, 0, 0)
        };
        let pname = match proto {
            1 => "icmp",
            6 => "tcp",
            17 => "udp",
            _ => "other",
        };
        let mut fl = String::new();
        for (bit, name) in [(SYN, "syn"), (ACK, "ack"), (FIN, "fin"), (RST, "rst"), (PSH, "psh")] {
            if proto == 6 && flags & bit != 0 {
                if !fl.is_empty() {
                    fl.push(',');
                }
                fl.push_str(name);
            }
        }
        let _ = write!(
            s,
            "{{\"ago_ms\":{},\"hook\":\"{}\",\"iif\":\"{}\",\"oif\":\"{}\",\"proto\":\"{}\",\"src\":\"{}\",\"dst\":\"{}\"",
            (now - d.ns) / 1_000_000,
            hook_name(d.hook),
            ifname(d.iif),
            ifname(d.oif),
            pname,
            dotted(&ip[12..16]),
            dotted(&ip[16..20])
        );
        if proto == 6 || proto == 17 {
            let _ = write!(s, ",\"sport\":{},\"dport\":{}", sp, dp);
        }
        if proto == 1 && ihl + 2 <= 40 {
            let _ = write!(s, ",\"type\":{},\"code\":{}", ip[ihl], ip[ihl + 1]);
        }
        if !fl.is_empty() {
            let _ = write!(s, ",\"flags\":\"{}\"", fl);
        }
        let _ = write!(s, ",\"len\":{},\"by\":\"{}\"", d.len, d.why);
        match d.reject {
            Some(r) if r.rst => s.push_str(",\"reject\":\"tcp reset\""),
            Some(r) => {
                let _ = write!(s, ",\"reject\":\"icmp code {}\"", r.code);
            }
            None => {}
        }
        if let Some(u) = d.uid {
            let _ = write!(s, ",\"uid\":{}", u);
        }
        s.push_str("}\n");
    }
    s
}

// ---- 返すもの (reject) ----

/// IPv4 の頭 (20 バイト)。チェックサムも
#[allow(clippy::too_many_arguments)]
pub fn iphdr(b: &mut Vec<u8>, tos: u8, len: usize, id: u16, df: bool, ttl: u8, proto: u8, src: u32, dst: u32) {
    let at = b.len();
    b.extend_from_slice(&[0x45, tos]);
    b.extend_from_slice(&(len as u16).to_be_bytes());
    b.extend_from_slice(&id.to_be_bytes());
    b.extend_from_slice(&(if df { 0x4000u16 } else { 0 }).to_be_bytes());
    b.extend_from_slice(&[ttl, proto, 0, 0]);
    b.extend_from_slice(&src.to_be_bytes());
    b.extend_from_slice(&dst.to_be_bytes());
    let c = csum_fold(csum_add(0, &b[at..at + 20]));
    b[at + 10..at + 12].copy_from_slice(&c.to_be_bytes());
}

/// 1 の補数の和 (32 ビットにためる)
pub fn csum_add(mut sum: u32, b: &[u8]) -> u32 {
    let mut i = 0;
    while i + 1 < b.len() {
        sum = sum.wrapping_add(u16::from_be_bytes([b[i], b[i + 1]]) as u32);
        if sum & 0x8000_0000 != 0 {
            sum = (sum & 0xffff) + (sum >> 16);
        }
        i += 2;
    }
    if i < b.len() {
        sum = sum.wrapping_add((b[i] as u32) << 8);
    }
    sum
}

pub fn csum_fold(mut sum: u32) -> u16 {
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

/// TCP / UDP の疑似ヘッダつきのチェックサム (seg はその頭から)
fn l4sum(src: u32, dst: u32, proto: u8, seg: &[u8]) -> u16 {
    let mut ph = [0u8; 12];
    ph[0..4].copy_from_slice(&src.to_be_bytes());
    ph[4..8].copy_from_slice(&dst.to_be_bytes());
    ph[9] = proto;
    ph[10..12].copy_from_slice(&(seg.len() as u16).to_be_bytes());
    csum_fold(csum_add(csum_add(0, &ph), seg))
}

/// TCP のパケット (中どうしの SYN、SYN-ACK、RST を、Linux が lo に流すものと同じ形で)
#[allow(clippy::too_many_arguments)]
pub fn tcp(src: u32, sport: u16, dst: u32, dport: u16, seq: u32, ack: u32, flags: u8, win: u16, opts: &[u8], tos: u8, ttl: u8, id: u16) -> Vec<u8> {
    let hl = 20 + opts.len();
    let mut b = Vec::with_capacity(20 + hl);
    iphdr(&mut b, tos, 20 + hl, id, true, ttl, PROTO_TCP, src, dst);
    b.extend_from_slice(&sport.to_be_bytes());
    b.extend_from_slice(&dport.to_be_bytes());
    b.extend_from_slice(&seq.to_be_bytes());
    b.extend_from_slice(&ack.to_be_bytes());
    b.extend_from_slice(&[((hl / 4) as u8) << 4, flags]);
    b.extend_from_slice(&win.to_be_bytes());
    b.extend_from_slice(&[0, 0, 0, 0]);
    b.extend_from_slice(opts);
    let c = l4sum(src, dst, PROTO_TCP, &b[20..]);
    b[36..38].copy_from_slice(&c.to_be_bytes());
    b
}

/// TCP のパケットに中身もつけて (チェックサムも中身ごと)
#[allow(clippy::too_many_arguments)]
pub fn tcp_data(src: u32, sport: u16, dst: u32, dport: u16, seq: u32, ack: u32, flags: u8, win: u16, opts: &[u8], tos: u8, ttl: u8, id: u16, data: &[u8]) -> Vec<u8> {
    let mut b = tcp(src, sport, dst, dport, seq, ack, flags, win, opts, tos, ttl, id);
    let tot = b.len() + data.len();
    b[2..4].copy_from_slice(&(tot as u16).to_be_bytes());
    b[10..12].copy_from_slice(&[0, 0]);
    let c = csum_fold(csum_add(0, &b[..20]));
    b[10..12].copy_from_slice(&c.to_be_bytes());
    b.extend_from_slice(data);
    b[36..38].copy_from_slice(&[0, 0]);
    let c = l4sum(src, dst, PROTO_TCP, &b[20..]);
    b[36..38].copy_from_slice(&c.to_be_bytes());
    b
}

/// SYN と SYN-ACK の TCP のオプション (Linux の tcp_syn_options: MSS、SACK_PERM と timestamp、NOP と window scale 7)
pub fn syn_opts(mss: u16, tsval: u32, tsecr: u32) -> [u8; 20] {
    let mut o = [0u8; 20];
    o[0] = 2;
    o[1] = 4;
    o[2..4].copy_from_slice(&mss.to_be_bytes());
    o[4] = 4;
    o[5] = 2;
    o[6] = 8;
    o[7] = 10;
    o[8..12].copy_from_slice(&tsval.to_be_bytes());
    o[12..16].copy_from_slice(&tsecr.to_be_bytes());
    o[16] = 1;
    o[17] = 3;
    o[18] = 3;
    o[19] = 7;
    o
}

/// UDP のパケット
#[allow(clippy::too_many_arguments)]
pub fn udp(src: u32, sport: u16, dst: u32, dport: u16, data: &[u8], tos: u8, ttl: u8, id: u16, df: bool) -> Vec<u8> {
    let mut b = Vec::with_capacity(28 + data.len());
    iphdr(&mut b, tos, 28 + data.len(), id, df, ttl, PROTO_UDP, src, dst);
    b.extend_from_slice(&sport.to_be_bytes());
    b.extend_from_slice(&dport.to_be_bytes());
    b.extend_from_slice(&((8 + data.len()) as u16).to_be_bytes());
    b.extend_from_slice(&[0, 0]);
    b.extend_from_slice(data);
    let mut c = l4sum(src, dst, PROTO_UDP, &b[20..]);
    if c == 0 {
        c = 0xffff;
    }
    b[26..28].copy_from_slice(&c.to_be_bytes());
    b
}

/// aios のアドレスか (送り元を決めるため: 127.0.0.0/8 と eth0 のもの)
pub fn is_local(a: u32) -> bool {
    a >> 24 == 127 || (a != 0 && a == crate::net::eth_addr())
}

/// ブロードキャストかマルチキャストか (eth0 のネットワークのブロードキャストも)
pub fn is_bcast_or_mcast(a: u32) -> bool {
    if a >> 28 == 0xe || a == u32::MAX {
        return true;
    }
    let (base, p) = (crate::net::eth_addr(), crate::net::eth_prefix() as u32);
    base != 0 && p < 31 && a == base | (u32::MAX >> p)
}

/// 次に渡す相手 (rt nexthop: 同じネットワークか自分ならあて先、ほかはゲートウェイ)
pub fn nexthop(daddr: u32, oif: u32) -> u32 {
    if oif == IF_LO || is_local(daddr) {
        return daddr;
    }
    let (base, p) = (crate::net::eth_addr(), crate::net::eth_prefix() as u32);
    let mask = if p == 0 { 0 } else { u32::MAX << (32 - p) };
    if base != 0 && daddr & mask == base & mask {
        return daddr;
    }
    crate::net::eth_gateway()
}

/// reject の返すもの (Linux の nf_send_reset と nf_send_unreach → __icmp_send)。返せないもの (RST への RST、ICMP の
/// エラーへのエラー、断片、ブロードキャストとマルチキャスト、チェックサムのちがうもの) は None。iif は受けた口 (出す
/// ものなら 0)
fn reply(ip: &[u8], rj: Reject, hook: u32, iif: u32, pkttype: u8, mark: u32) -> Option<Vec<u8>> {
    let _ = mark;
    let ihl = (ip[0] & 15) as usize * 4;
    let frag = u16::from_be_bytes([ip[6], ip[7]]) & 0x1fff;
    if frag != 0 {
        return None;
    }
    let (saddr, daddr, proto) = (be32(&ip[12..16]), be32(&ip[16..20]), ip[9]);
    if is_bcast_or_mcast(daddr) {
        return None;
    }
    let incoming = matches!(hook, PRE_ROUTING | LOCAL_IN);
    if rj.rst {
        // nf_reject_ip_tcphdr_get: TCP で RST でないもの、受けたものはチェックサムも
        if proto != PROTO_TCP || ip.len() < ihl + 20 {
            return None;
        }
        let th = &ip[ihl..];
        if th[13] & RST != 0 {
            return None;
        }
        if incoming && iif == IF_ETH0 && l4sum(saddr, daddr, PROTO_TCP, th) != 0 {
            return None;
        }
        let doff = (th[12] >> 4) as usize * 4;
        let (seq, ack) = (be32(&th[4..8]), be32(&th[8..12]));
        let (rseq, rack, flags) = if th[13] & ACK != 0 {
            (ack, 0, RST)
        } else {
            let len = (ip.len() - ihl).saturating_sub(doff) as u32;
            (0, seq.wrapping_add((th[13] & SYN != 0) as u32 + (th[13] & FIN != 0) as u32 + len), RST | ACK)
        };
        return Some(tcp(daddr, be16(&th[2..4]), saddr, be16(&th[0..2]), rseq, rack, flags, 0, &[], 0, 64, 0));
    }
    // __icmp_send
    if pkttype != PACKET_HOST {
        return None;
    }
    if proto == PROTO_ICMP {
        // ICMP のエラーには返さない (知らない type もエラーとみなす)
        let t = *ip.get(ihl)?;
        if t > 18 || matches!(t, 3 | 4 | 5 | 11 | 12) {
            return None;
        }
    }
    // チェックサムを確かめる (nf_reject_verify_csum: UDP の 0 は確かめない。受けたものだけ)
    if incoming && iif == IF_ETH0 {
        let seg = &ip[ihl..];
        let check = match proto {
            PROTO_UDP => seg.len() >= 8 && be16(&seg[6..8]) != 0,
            PROTO_TCP | PROTO_ICMP => true,
            _ => false,
        };
        let ok = match proto {
            PROTO_ICMP => csum_fold(csum_add(0, seg)) == 0,
            _ => l4sum(saddr, daddr, proto, seg) == 0,
        };
        if check && !ok {
            return None;
        }
    }
    // 送り元: 自分あてに来たものならそのあて先、外へ出すものなら (自分へ返すので) 自分
    let src = if is_local(daddr) { daddr } else { saddr };
    // 外 (eth0 の向こう) へは相手ごとに数をしぼる
    if !is_local(saddr) && !ratelimit(saddr) {
        return None;
    }
    let keep = (ip.len()).min(576 - 20 - 8);
    let tos = (ip[1] & 0x1e) | 0xc0;
    let mut b = Vec::with_capacity(28 + keep);
    iphdr(&mut b, tos, 28 + keep, crate::rand::u32() as u16, false, 64, PROTO_ICMP, src, saddr);
    b.extend_from_slice(&[3, rj.code, 0, 0, 0, 0, 0, 0]);
    b.extend_from_slice(&ip[..keep]);
    let c = csum_fold(csum_add(0, &b[20..]));
    b[22..24].copy_from_slice(&c.to_be_bytes());
    Some(b)
}

/// カーネルが返す ICMP の到達不能 (UDP のだれもいないポートへ: __udp4_lib_rcv の icmp_send)。ip は届いたもの (lo)
pub fn kernel_unreach(ip: &[u8], code: u8) -> Option<Vec<u8>> {
    reply(ip, Reject { rst: false, code }, LOCAL_IN, IF_LO, PACKET_HOST, 0)
}

/// ICMP のエラーを相手ごとにしぼる (inet_peer_xrlim_allow: 1 秒に 1 つ、ためておけるのは 6 つ。はじめはいっぱい)
fn ratelimit(peer: u32) -> bool {
    use crate::cell::Bkl;
    static PEERS: Bkl<Vec<(u32, u64, u64)>> = Bkl::new(Vec::new());
    const HZ_NS: u64 = 1_000_000_000;
    let now = crate::timer::uptime_ns();
    let v = PEERS.get();
    let i = match v.iter().position(|p| p.0 == peer) {
        Some(i) => i,
        None => {
            if v.len() >= 64 {
                v.remove(0);
            }
            v.push((peer, 0, now.saturating_sub(60 * HZ_NS)));
            v.len() - 1
        }
    };
    let (_, tokens, last) = &mut v[i];
    let mut t = *tokens + now.saturating_sub(*last);
    *last = now;
    t = t.min(6 * HZ_NS);
    let ok = t >= HZ_NS;
    if ok {
        t -= HZ_NS;
    }
    *tokens = t;
    ok
}

/// 返すものの種類 (受けるソケットのため): RST なら None、ICMP なら (type, code)
pub fn icmp_of(r: &[u8]) -> Option<(u8, u8)> {
    (r.len() >= 28 && r[9] == PROTO_ICMP).then(|| (r[20], r[21]))
}

/// ICMP の到達不能の code の errno と、致命か (Linux の icmp_err_convert)
pub fn icmp_errno(code: u8) -> (i64, bool) {
    const T: [(i64, bool); 16] = [
        (101, false), // NET_UNREACH: ENETUNREACH
        (113, false), // HOST_UNREACH: EHOSTUNREACH
        (92, true),   // PROT_UNREACH: ENOPROTOOPT
        (111, true),  // PORT_UNREACH: ECONNREFUSED
        (90, false),  // FRAG_NEEDED: EMSGSIZE
        (95, false),  // SR_FAILED: EOPNOTSUPP
        (101, true),  // NET_UNKNOWN
        (112, true),  // HOST_UNKNOWN: EHOSTDOWN
        (64, true),   // HOST_ISOLATED: ENONET
        (101, true),  // NET_ANO
        (113, true),  // HOST_ANO
        (101, false), // NET_UNR_TOS
        (113, false), // HOST_UNR_TOS
        (113, true),  // PKT_FILTERED
        (113, true),  // PREC_VIOLATION
        (113, true),  // PREC_CUTOFF
    ];
    T.get(code as usize).copied().unwrap_or((113, false))
}

// ---- log (nf_log_syslog の nf_log_ip_packet) ----

const NF_LOG_TCPSEQ: u32 = 0x01;
const NF_LOG_TCPOPT: u32 = 0x02;
const NF_LOG_IPOPT: u32 = 0x04;
const NF_LOG_UID: u32 = 0x08;
const NF_LOG_MACDECODE: u32 = 0x20;

fn ifname(i: u32) -> &'static str {
    match i {
        IF_LO => "lo",
        IF_ETH0 => crate::net::IFNAME,
        _ => "",
    }
}

/// log の 1 行を kmsg へ (level はいまは見せかただけ: kmsg に段がない)
pub fn log(p: &Pkt, prefix: &str, level: u32, flags: u32) {
    let _ = level;
    let mut m = String::new();
    let _ = write!(m, "{}IN={} OUT={} ", prefix, ifname(p.iif), ifname(p.oif));
    if p.iif != 0 {
        // dump_mac_header: MACDECODE なら Ethernet (eth0) は送り元とあて先と型、ほか (lo は ARPHRD_LOOPBACK) は頭の
        // 長さ (14) だけ並べる
        match p.ll {
            Some(ll) if flags & NF_LOG_MACDECODE != 0 && p.iif == IF_ETH0 => {
                let mac = |b: &[u8]| alloc::format!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", b[0], b[1], b[2], b[3], b[4], b[5]);
                let _ = write!(m, "MACSRC={} MACDST={} MACPROTO={:04x} ", mac(&ll[6..12]), mac(&ll[0..6]), be16(&ll[12..14]));
            }
            _ => {
                m.push_str("MAC=");
                if let Some(ll) = p.ll {
                    for (i, b) in ll.iter().enumerate() {
                        let _ = write!(m, "{}{:02x}", if i > 0 { ":" } else { "" }, b);
                    }
                }
                m.push(' ');
            }
        }
    }
    dump_ip(&mut m, p.ip, 0, flags);
    if flags & NF_LOG_UID != 0
        && let Some((u, g)) = p.sk
    {
        let _ = write!(m, "UID={} GID={} ", u, g);
    }
    if p.mark != 0 {
        let _ = write!(m, "MARK=0x{:x} ", p.mark);
    }
    m.push('\n');
    crate::kmsg::push(&m);
}

fn dotted(a: &[u8]) -> String {
    alloc::format!("{}.{}.{}.{}", a[0], a[1], a[2], a[3])
}

fn dump_ip(m: &mut String, ip: &[u8], depth: u32, flags: u32) {
    if ip.len() < 20 {
        let _ = write!(m, "TRUNCATED");
        return;
    }
    let ihl = (ip[0] & 15) as usize * 4;
    let _ = write!(m, "SRC={} DST={} ", dotted(&ip[12..16]), dotted(&ip[16..20]));
    let fo = be16(&ip[6..8]);
    let _ = write!(m, "LEN={} TOS=0x{:02X} PREC=0x{:02X} TTL={} ID={} ", be16(&ip[2..4]), ip[1] & 0x1e, ip[1] & 0xe0, ip[8], be16(&ip[4..6]));
    if fo & 0x8000 != 0 {
        m.push_str("CE ");
    }
    if fo & 0x4000 != 0 {
        m.push_str("DF ");
    }
    if fo & 0x2000 != 0 {
        m.push_str("MF ");
    }
    let off = fo & 0x1fff;
    if off != 0 {
        let _ = write!(m, "FRAG:{} ", off);
    }
    if flags & NF_LOG_IPOPT != 0 && ihl > 20 && ip.len() >= ihl {
        m.push_str("OPT (");
        for b in &ip[20..ihl] {
            let _ = write!(m, "{:02X}", b);
        }
        m.push_str(") ");
    }
    let rest = ip.get(ihl..).unwrap_or(&[]);
    match ip[9] {
        PROTO_TCP => {
            m.push_str("PROTO=TCP ");
            if off != 0 {
                return;
            }
            if rest.len() < 20 {
                let _ = write!(m, "INCOMPLETE [{} bytes] ", rest.len());
                return;
            }
            let th = rest;
            let _ = write!(m, "SPT={} DPT={} ", be16(&th[0..2]), be16(&th[2..4]));
            if flags & NF_LOG_TCPSEQ != 0 {
                let _ = write!(m, "SEQ={} ACK={} ", be32(&th[4..8]), be32(&th[8..12]));
            }
            let _ = write!(m, "WINDOW={} ", be16(&th[14..16]));
            let _ = write!(m, "RES=0x{:02x} ", ((be32(&th[12..16]) & 0x0f00_0000) >> 22) as u8);
            let f = th[13];
            for (bit, name) in [(0x80, "CWR "), (0x40, "ECE "), (0x20, "URG "), (ACK, "ACK "), (PSH, "PSH "), (RST, "RST "), (SYN, "SYN "), (FIN, "FIN ")] {
                if f & bit != 0 {
                    m.push_str(name);
                }
            }
            let _ = write!(m, "URGP={} ", be16(&th[18..20]));
            let doff = (th[12] >> 4) as usize * 4;
            if flags & NF_LOG_TCPOPT != 0 && doff > 20 && th.len() >= doff {
                m.push_str("OPT (");
                for b in &th[20..doff] {
                    let _ = write!(m, "{:02X}", b);
                }
                m.push_str(") ");
            }
        }
        PROTO_UDP => {
            m.push_str("PROTO=UDP ");
            if off != 0 {
                return;
            }
            if rest.len() < 8 {
                let _ = write!(m, "INCOMPLETE [{} bytes] ", rest.len());
                return;
            }
            let _ = write!(m, "SPT={} DPT={} LEN={} ", be16(&rest[0..2]), be16(&rest[2..4]), be16(&rest[4..6]));
        }
        PROTO_ICMP => {
            m.push_str("PROTO=ICMP ");
            if off != 0 {
                return;
            }
            if rest.len() < 4 {
                let _ = write!(m, "INCOMPLETE [{} bytes] ", rest.len());
                return;
            }
            let (t, c) = (rest[0], rest[1]);
            let _ = write!(m, "TYPE={} CODE={} ", t, c);
            // required_len: echo とエラーは 8 バイト (+ エラーは中の IP の頭 20)
            let need = match t {
                0 | 8 | 13 | 14 | 15 | 16 | 17 | 18 => 8,
                3 | 4 | 5 | 11 | 12 => 8 + 20,
                _ => 0,
            };
            if need != 0 && rest.len() < need {
                let _ = write!(m, "INCOMPLETE [{} bytes] ", rest.len());
                return;
            }
            match t {
                0 | 8 => {
                    let _ = write!(m, "ID={} SEQ={} ", be16(&rest[4..6]), be16(&rest[6..8]));
                }
                12 => {
                    let _ = write!(m, "PARAMETER={} ", rest[4]);
                }
                _ => {}
            }
            if t == 5 {
                let _ = write!(m, "GATEWAY={} ", dotted(&rest[4..8]));
            }
            if matches!(t, 3 | 4 | 5 | 11) {
                if depth == 0 {
                    m.push('[');
                    dump_ip(m, &rest[8..], depth + 1, flags);
                    m.push_str("] ");
                }
                if t == 3 && c == 4 {
                    let _ = write!(m, "MTU={} ", be16(&rest[6..8]));
                }
            }
        }
        p => {
            let _ = write!(m, "PROTO={} ", p);
        }
    }
}

// ---- trace (/proc/ai/ctl の fw trace: パケットを流さずに道を試す) ----

/// `fw trace in|out eth0|lo tcp|udp|icmp SRC[:PORT] DST[:PORT] [syn|ack|synack|fin|rst]`。in eth0 は PRE_ROUTING と
/// (aios あてなら) LOCAL_IN、out eth0 は LOCAL_OUT と POST_ROUTING、lo は 4 つ。決まりを写したものに通すので、counter
/// も limit も conntrack も変わらない。道は /proc/ai/fw の trace の行に
pub fn trace_cmd(arg: &str) -> Result<(), i64> {
    let w: Vec<&str> = arg.split_whitespace().collect();
    if w.len() < 5 {
        return Err(crate::why::inval());
    }
    let ep = |s: &str| -> Result<(u32, u16), i64> {
        let (a, p) = s.split_once(':').map_or((s, "0"), |(a, p)| (a, p));
        let mut b = [0u8; 4];
        let mut n = 0;
        for (i, x) in a.split('.').enumerate() {
            if i >= 4 {
                return Err(crate::why::inval());
            }
            b[i] = x.parse::<u8>().map_err(|_| crate::why::inval())?;
            n += 1;
        }
        if n != 4 {
            return Err(crate::why::inval());
        }
        Ok((u32::from_be_bytes(b), p.parse::<u16>().map_err(|_| crate::why::inval())?))
    };
    let (dir, dev, proto) = (w[0], w[1], w[2]);
    let ((src, sp), (dst, dp)) = (ep(w[3])?, ep(w[4])?);
    let fl = w.get(5).copied().unwrap_or(if proto == "tcp" { "syn" } else { "" });
    let mut pkt = match proto {
        "tcp" => {
            let f = match fl {
                "syn" => SYN,
                "ack" => ACK,
                "synack" => SYN | ACK,
                "fin" => FIN | ACK,
                "rst" => RST,
                _ => return Err(crate::why::inval()),
            };
            let opts = if f & SYN != 0 { syn_opts(1460, 1, 0).to_vec() } else { Vec::new() };
            tcp(src, sp, dst, dp, 1, if f & ACK != 0 { 1 } else { 0 }, f, 64240, &opts, 0, 64, 0)
        }
        "udp" => udp(src, sp, dst, dp, b"trace", 0, 64, 0, true),
        "icmp" => {
            let mut b = Vec::with_capacity(28);
            iphdr(&mut b, 0, 28, 0, true, 64, PROTO_ICMP, src, dst);
            b.extend_from_slice(&[8, 0, 0, 0, 0, 1, 0, 1]);
            let c = csum_fold(csum_add(0, &b[20..]));
            b[22..24].copy_from_slice(&c.to_be_bytes());
            b
        }
        _ => return Err(crate::why::inval()),
    };
    let what = alloc::format!("{} {} {} {}:{} > {}:{} {}", dir, dev, proto, dotted(&src.to_be_bytes()), sp, dotted(&dst.to_be_bytes()), dp, fl);
    let mut res = Ok(());
    nftables::run_trace(&what, || {
        let v = match (dir, dev) {
            (_, "lo") => match lo(&mut pkt, None, 0, 0) {
                Path::Pass => String::from("result: accept (delivered on lo)"),
                Path::Out(_, why) => alloc::format!("result: drop on output by {}", why.text()),
                Path::In(_, why) => alloc::format!("result: drop on input by {}", why.text()),
            },
            ("out", "eth0") => match out_eth(&mut pkt, None, 0, 0) {
                Path::Pass => String::from("result: accept (sent on eth0)"),
                Path::Out(_, why) | Path::In(_, why) => alloc::format!("result: drop on output by {} (sendmsg gets EPERM)", why.text()),
            },
            ("in", "eth0") => {
                let local = (dst != 0 && dst == crate::net::eth_addr()) || is_bcast_or_mcast(dst);
                let mut f = Vec::with_capacity(14 + pkt.len());
                f.extend_from_slice(&[0x52, 0x54, 0, 0x12, 0x34, 0x56, 0x52, 0x55, 0x0a, 0, 2, 2, 8, 0]);
                if let Some(m) = crate::net::mac() {
                    f[..6].copy_from_slice(&m);
                }
                f.extend_from_slice(&pkt);
                match in_eth(&mut f, local, PACKET_HOST) {
                    EthIn::Pass if local => String::from("result: accept (delivered to aios)"),
                    EthIn::Pass => String::from("result: accept on prerouting (not addressed to aios: not delivered)"),
                    EthIn::Drop(r) => alloc::format!("result: drop{}", if r.is_some() { " (a reject reply would be sent back)" } else { "" }),
                }
            }
            _ => {
                res = Err(crate::why::inval());
                String::new()
            }
        };
        nftables::trace_note(v);
    });
    res
}

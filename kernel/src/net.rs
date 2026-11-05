// ネットワーク: virtio-net を smoltcp (TCP/IP) につなぐ
// アドレスは DHCP でもらう (Linux の ip=dhcp のように、カーネルの中で)。
// もらった DNS は /proc/net/pnp に出す (/etc/resolv.conf はそこへのリンク)
// アドレスを手で決める (ioctl の SIOCSIFADDR、ip addr add、networkd) と DHCP はやめる。
// 0.0.0.0 にすると DHCP にもどす。インターフェースは 1 つ (eth0)
// ループバック: 127.0.0.1/8 もいつも持ち、自分あて (127.x と自分のアドレス) のフレームは
// virtio に出さずに受け取りの列へもどす (ARP も自分で答える)
// IPv6: eth0 の番地と道は addrconf.rs (link-local と RA からの SLAAC)。それを smoltcp に渡し (sync6)、RS を出す。
// 自分の IPv6 の番地あてのフレーム (と、それを問う NS) も受け取りの列へ。NS / NA / RA は近くの番地の表に (netlink.rs)、
// eth0 から来た ICMPv6 のエラーは poll のあとで inet.rs に
use crate::cell::Bkl;
use crate::proc;
use crate::timer;
use crate::virtio_net::VirtioNet;
use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};
use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, IpCidr, Ipv4Address};

use smoltcp::socket::dhcpv4;
use smoltcp::wire::Ipv4Cidr;

pub struct Net {
    pub iface: Interface,
    pub sockets: SocketSet<'static>,
    dev: NetDev,
    /// close されたが、まだ FIN のやりとりが残っている TCP
    orphans: Vec<SocketHandle>,
    /// DHCP のソケット (アドレスを手で決めたら None)
    dhcp: Option<SocketHandle>,
    /// DHCP でもらったもの、または手で決めたもの (dns は DHCP でもらっていたもの)
    pub lease: Option<Lease>,
    /// smoltcp に渡した IPv6 の番地と道の版 (addrconf::generation)
    v6gen: u64,
}

#[derive(Clone)]
pub struct Lease {
    pub addr: Ipv4Cidr,
    pub router: Option<Ipv4Address>,
    pub dns: Vec<Ipv4Address>,
}

static NET: Bkl<Option<Net>> = Bkl::new(None);

pub fn now() -> Instant {
    Instant::from_micros((timer::uptime_ns() / 1000) as i64)
}

/// virtio-net と、自分あてのフレームの列 (ループバック。入れるときにファイアウォールを通してある)
pub struct NetDev {
    virtio: VirtioNet,
    lo: VecDeque<Vec<u8>>,
    mac: [u8; 6],
}

/// eth0 のいまのアドレス (自分あてかを見分ける。なければ 0)、ネットマスクの長さ、ゲートウェイ (ファイアウォールが
/// smoltcp の poll の中から見るので、Net を通さずに)
static ETH_ADDR: AtomicU32 = AtomicU32::new(0);
static ETH_PREFIX: AtomicU32 = AtomicU32::new(0);
static ETH_GW: AtomicU32 = AtomicU32::new(0);

pub fn eth_addr() -> u32 {
    ETH_ADDR.load(Ordering::Relaxed)
}

/// netlink で足した IPv4 の番地 (ip addr add: inet_rtm_newaddr)。lease (DHCP か SIOCSIFADDR の番地) のほかに。どれが
/// primary でどれが secondary か (同じ網の 2 つ目) は netlink.rs が決める
#[derive(Clone)]
pub struct Addr4 {
    pub addr: u32,
    pub prefix: u8,
    /// IFA_BROADCAST (0 はなし)
    pub brd: u32,
    pub label: alloc::string::String,
    /// IFA_F_* (IFA_F_PERMANENT と IFA_F_NOPREFIXROUTE など。SECONDARY は netlink.rs が)
    pub flags: u32,
    pub proto: u8,
    pub scope: u8,
    /// lifetime (秒、u32::MAX はいつまでも。tstamp から数える)
    pub valid: u32,
    pub preferred: u32,
    /// 作った時刻と lifetime を決めた時刻 (ミリ秒、now)
    pub cstamp: u64,
    pub tstamp: u64,
}

static ADDRS4: Bkl<Vec<Addr4>> = Bkl::new(Vec::new());
/// ADDRS4 の数 (フレームごとの自分あてのたしかめを、ないときは速く)
static ADDRS4_N: AtomicU32 = AtomicU32::new(0);

pub fn extra4() -> Vec<Addr4> {
    if ADDRS4_N.load(Ordering::Relaxed) == 0 {
        return Vec::new();
    }
    ADDRS4.get().clone()
}

/// eth0 の IPv4 の番地か (lease と netlink で足したもの)
pub fn is_eth_addr(a: u32) -> bool {
    a != 0 && (a == ETH_ADDR.load(Ordering::Relaxed) || (ADDRS4_N.load(Ordering::Relaxed) != 0 && ADDRS4.get().iter().any(|x| x.addr == a)))
}

/// netlink で足した番地を入れかえて smoltcp にも
pub fn set_extra4(v: Vec<Addr4>) {
    ADDRS4_N.store(v.len() as u32, Ordering::Relaxed);
    *ADDRS4.get() = v;
    if let Some(n) = get() {
        let v4 = n.lease.as_ref().map(|l| l.addr);
        set_addrs(&mut n.iface, v4);
    }
}

/// lease (DHCP か SIOCSIFADDR の番地) を消す (ip addr del でその番地を: DHCP もやめる。ゲートウェイも)
pub fn clear_lease() {
    let Some(n) = get() else { return };
    if let Some(h) = n.dhcp.take() {
        n.sockets.remove(h);
    }
    n.lease = None;
    n.iface.routes_mut().remove_default_ipv4_route();
    set_gw(None);
    set_addrs(&mut n.iface, None);
}

/// 送り元 (あて先 dst への道の prefsrc。netlink で足した番地がなければ eth0 のもの)
pub fn src4(dst: u32) -> u32 {
    if ADDRS4_N.load(Ordering::Relaxed) == 0 {
        return eth_addr();
    }
    crate::netlink::src_for(dst).unwrap_or_else(eth_addr)
}

pub fn eth_prefix() -> u8 {
    ETH_PREFIX.load(Ordering::Relaxed) as u8
}

pub fn eth_gateway() -> u32 {
    ETH_GW.load(Ordering::Relaxed)
}

fn set_gw(r: Option<Ipv4Address>) {
    ETH_GW.store(r.map_or(0, |a| u32::from_be_bytes(a.octets())), Ordering::Relaxed);
}

/// ファイアウォールが返した ICMP のエラーで、smoltcp の外 (inet.rs) のソケットに届けるもの (poll のあとで)
static ICMP_LATER: Bkl<Vec<Vec<u8>>> = Bkl::new(Vec::new());
/// eth0 から来た ICMPv6 のエラー (IPv6 の頭から): smoltcp は TCP と UDP のソケットに当てないので、poll のあとで inet.rs に
static ICMP6_LATER: Bkl<Vec<Vec<u8>>> = Bkl::new(Vec::new());
/// 送り受けの数 (eth0 と lo): 受けたバイト, 受けたフレーム, 出したバイト, 出したフレーム
static ETH_STATS: [core::sync::atomic::AtomicU64; 4] = [const { core::sync::atomic::AtomicU64::new(0) }; 4];
static LO_STATS: [core::sync::atomic::AtomicU64; 4] = [const { core::sync::atomic::AtomicU64::new(0) }; 4];

fn count(lo: bool, rx: bool, len: usize) {
    let s = if lo { &LO_STATS } else { &ETH_STATS };
    let i = if rx { 0 } else { 2 };
    s[i].fetch_add(len as u64, Ordering::Relaxed);
    s[i + 1].fetch_add(1, Ordering::Relaxed);
}

/// (受けたバイト, 受けたフレーム, 出したバイト, 出したフレーム) (/sys/class/net と /proc/net/dev)
pub fn stats(lo: bool) -> [u64; 4] {
    let s = if lo { &LO_STATS } else { &ETH_STATS };
    [0, 1, 2, 3].map(|i| s[i].load(Ordering::Relaxed))
}

/// 127.0.0.0/8 か eth0 のアドレス
fn is_local(ip: &[u8]) -> bool {
    ip[0] == 127 || is_eth_addr(u32::from_be_bytes([ip[0], ip[1], ip[2], ip[3]]))
}

/// 出すフレームが自分あてか (IPv4 の宛先、ARP の問い合わせ先、IPv6 の宛先と NS の問い合わせ先で見る)
fn loops_back(f: &[u8]) -> bool {
    if f.len() < 14 {
        return false;
    }
    let mine6 = |b: &[u8]| crate::addrconf::is_mine(u128::from_be_bytes(b.try_into().unwrap()));
    match u16::from_be_bytes([f[12], f[13]]) {
        0x0800 if f.len() >= 34 => is_local(&f[30..34]),
        0x0806 if f.len() >= 42 => is_local(&f[38..42]),
        // NS (ICMPv6 の 135) は問い合わせる番地 (target) で
        0x86dd if f.len() >= 54 => mine6(&f[38..54]) || (f.len() >= 78 && f[20] == 58 && f[54] == 135 && mine6(&f[62..78])),
        _ => false,
    }
}

impl Device for NetDev {
    type RxToken<'a> = Rx;
    type TxToken<'a> = Tx<'a>;

    fn receive(&mut self, _t: Instant) -> Option<(Rx, Tx<'_>)> {
        if let Some(frame) = self.lo.pop_front() {
            count(true, true, frame.len());
            return Some((Rx(frame), Tx(self)));
        }
        loop {
            if !self.virtio.can_send() {
                return None;
            }
            let mut frame = self.virtio.recv()?;
            count(false, true, frame.len());
            // ARP は近くの番地の表に (netlink の RTM_GETNEIGH。自分の MAC あてなら PACKET_HOST)
            if frame.len() >= 14 && frame[12..14] == [8, 6] {
                crate::netlink::arp_in(&frame, frame[..6] == self.mac);
            }
            // ファイアウォール (PRE_ROUTING と LOCAL_IN)。落とせば次のフレームを
            if !self.filter_in(&mut frame) {
                continue;
            }
            // IPv6 (決まりを通ったもの): RA は addrconf が読み、NS / NA / RA は近くの番地の表に (どれも smoltcp にもわたす)
            if frame.len() >= 54 && frame[12..14] == [0x86, 0xdd] {
                crate::addrconf::rx(&frame);
                crate::netlink::nd_in(&frame, frame[..6] == self.mac);
                // ICMPv6 のエラー (type が 128 より小さいもの) で自分あてのもの
                if frame.len() >= 54 + 48 && frame[20] == 58 && frame[54] < 128 && crate::addrconf::is_mine(u128::from_be_bytes(frame[38..54].try_into().unwrap())) {
                    ICMP6_LATER.get().push(frame[14..].to_vec());
                }
            }
            return Some((Rx(frame), Tx(self)));
        }
    }

    fn transmit(&mut self, _t: Instant) -> Option<Tx<'_>> {
        self.virtio.can_send().then_some(Tx(self))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut c = DeviceCapabilities::default();
        c.medium = Medium::Ethernet;
        c.max_transmission_unit = 1514;
        c
    }
}

pub struct Rx(Vec<u8>);

impl RxToken for Rx {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}

pub struct Tx<'a>(&'a mut NetDev);

impl TxToken for Tx<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        // 先に作ってみて、自分あてなら受け取りの列へ、ほかは virtio へ (どちらもファイアウォールを通ったものだけ)
        let mut buf = alloc::vec![0u8; len];
        let r = f(&mut buf);
        if loops_back(&buf) {
            if self.0.filter_out(&mut buf, true) {
                count(true, false, len);
                self.0.lo.push_back(buf);
            }
        } else if self.0.filter_out(&mut buf, false) {
            count(false, false, len);
            if len >= 14 && buf[12..14] == [8, 6] {
                crate::netlink::arp_out(&buf);
            }
            if len >= 54 && buf[12..14] == [0x86, 0xdd] {
                crate::netlink::nd_out(&buf);
            }
            self.0.virtio.send(len, |b| b.copy_from_slice(&buf));
        }
        r
    }
}

/// IPv4 のフレームなら、IP の頭がおかしくないか (版、長さ、頭のチェックサム)。おかしいものは決まりに通さず smoltcp に
/// まかせる (Linux の ip_rcv_core はフックの前に捨てる)
fn ipv4_ok(f: &[u8]) -> bool {
    if f.len() < 34 || u16::from_be_bytes([f[12], f[13]]) != 0x0800 {
        return false;
    }
    let ip = &f[14..];
    let ihl = (ip[0] & 15) as usize * 4;
    let tot = u16::from_be_bytes([ip[2], ip[3]]) as usize;
    ip[0] >> 4 == 4 && ihl >= 20 && tot >= ihl && tot <= ip.len() && crate::nfhook::csum_fold(crate::nfhook::csum_add(0, &ip[..ihl])) == 0
}

/// IPv6 のフレームなら、頭がおかしくないか (版と長さ: ip6_rcv_core)
fn ipv6_ok(f: &[u8]) -> bool {
    f.len() >= 54 && f[12..14] == [0x86, 0xdd] && f[14] >> 4 == 6 && 54 + u16::from_be_bytes([f[18], f[19]]) as usize <= f.len()
}

/// aios が受けるマルチキャスト (ff02::1 と、自分の番地の solicited-node の ff02::1:ffXX:XXXX)
fn mc_joined(a: u128) -> bool {
    a == 0xff02u128 << 112 | 1
        || (a >> 24 == (0xff02u128 << 88 | 1 << 8 | 0xff) && crate::addrconf::addrs().iter().any(|x| x.addr & 0xff_ffff == a & 0xff_ffff))
}

/// IPv6 の外への (送るときに決まりを通した) raw の ICMPv6 のパケット: filter_out は通しなおさない
static RAW6_PASSED: Bkl<Vec<Vec<u8>>> = Bkl::new(Vec::new());

pub fn raw6_passed(ip: Vec<u8>) {
    let v = RAW6_PASSED.get();
    if v.len() >= 16 {
        v.remove(0);
    }
    v.push(ip);
}

/// 自分あてのもの (IPv6 の頭から: ファイアウォールが返した ICMPv6 や RST) を受け取りの列に (raw のソケットと smoltcp が
/// 受ける)。ICMPv6 のエラーは poll のあとで inet.rs にも
pub fn inject6(ip: Vec<u8>) {
    let Some(n) = get() else { return };
    inject_to(n, ip);
}

/// inject6 と、IPv4 の ICMP のエラー (受け取りの列と、poll のあとで inet.rs に)
fn inject_to(n: &mut Net, ip: Vec<u8>) {
    let v6 = ip.first().is_some_and(|b| b >> 4 == 6);
    if v6 && ip.len() > 40 && ip[6] == 58 && ip[40] < 128 {
        ICMP6_LATER.get().push(ip.clone());
    }
    if !v6 && ip.len() > 20 && ip[9] == 1 && matches!(ip[(ip[0] & 15) as usize * 4], 3 | 11) {
        ICMP_LATER.get().push(ip.clone());
    }
    let mut f = Vec::with_capacity(14 + ip.len());
    f.extend_from_slice(&n.dev.mac);
    f.extend_from_slice(&n.dev.mac);
    f.extend_from_slice(if v6 { &[0x86, 0xdd] } else { &[0x08, 0x00] });
    f.extend_from_slice(&ip);
    n.dev.lo.push_back(f);
}

/// 近所さがしが MAX_PROBES (3) 度答えをもらえず、smoltcp が捨てたパケット (IP の頭から) に、Linux と同じく自分から自分へ
/// 「たどりつけない」を返す (neigh_invalidate → arp_error_report / ndisc_error_report → ipv4_link_failure の
/// icmp_send (ICMP_HOST_UNREACH)、ip6_link_failure の icmpv6_send (ICMPV6_ADDR_UNREACH))。送り元とあて先は、捨てた
/// パケットの送り元 (自分)。中身は捨てたものの頭から (IPv4 は ICMP のパケットが 576 バイト、IPv6 は 1280 バイトまで)。
/// ICMP のエラーと、断片の 2 つめから先には返さない。受けるのは raw のソケットと、送った TCP (connect が
/// EHOSTUNREACH) と UDP (つないだもの)
fn link_failure(n: &mut Net, p: &[u8]) {
    use crate::nfhook::{csum_add, csum_fold};
    match p.first().map(|b| b >> 4) {
        Some(4) if p.len() >= 20 => {
            let ihl = (p[0] & 15) as usize * 4;
            if p.len() < ihl || u16::from_be_bytes([p[6], p[7]]) & 0x1fff != 0 {
                return;
            }
            // icmp_pointers の error のもの (到達不能、source quench、redirect、時間切れ、parameter problem)
            if p[9] == 1 && p.get(ihl).is_some_and(|&ty| matches!(ty, 3 | 4 | 5 | 11 | 12)) {
                return;
            }
            let src = &p[12..16];
            let quote = &p[..p.len().min(576 - 28)];
            let mut ip = Vec::with_capacity(28 + quote.len());
            // tos は IPTOS_TOS と IPTOS_PREC_INTERNETCONTROL、DF なし、TTL 64
            ip.extend_from_slice(&[0x45, (p[1] & 0x1e) | 0xc0]);
            ip.extend_from_slice(&((28 + quote.len()) as u16).to_be_bytes());
            ip.extend_from_slice(&[0, 0, 0, 0, 64, 1, 0, 0]);
            ip.extend_from_slice(src);
            ip.extend_from_slice(src);
            let c = csum_fold(csum_add(0, &ip[..20]));
            ip[10..12].copy_from_slice(&c.to_be_bytes());
            ip.extend_from_slice(&[3, 1, 0, 0, 0, 0, 0, 0]);
            ip.extend_from_slice(quote);
            let c = csum_fold(csum_add(0, &ip[20..]));
            ip[22..24].copy_from_slice(&c.to_be_bytes());
            inject_to(n, ip);
        }
        Some(6) if p.len() >= 40 => {
            // ICMPv6 のエラー (type が 128 より小さいもの) には返さない (icmpv6_send の is_ineligible)
            if let Some((thoff, 58, _)) = crate::nfhook::v6_l4(p)
                && p.get(thoff).is_some_and(|&ty| ty < 128)
            {
                return;
            }
            let src = u128::from_be_bytes(p[8..24].try_into().unwrap());
            let quote = &p[..p.len().min(1280 - 48)];
            let mut body = Vec::with_capacity(8 + quote.len());
            body.extend_from_slice(&[1, 3, 0, 0, 0, 0, 0, 0]);
            body.extend_from_slice(quote);
            let c = crate::addrconf::icmp6_sum(src, src, &body);
            body[2..4].copy_from_slice(&c.to_be_bytes());
            let mut ip = Vec::with_capacity(40 + body.len());
            ip.extend_from_slice(&[0x60, 0, 0, 0]);
            ip.extend_from_slice(&(body.len() as u16).to_be_bytes());
            ip.extend_from_slice(&[58, 64]);
            ip.extend_from_slice(&src.to_be_bytes());
            ip.extend_from_slice(&src.to_be_bytes());
            ip.extend_from_slice(&body);
            inject_to(n, ip);
        }
        _ => {}
    }
}

impl NetDev {
    /// eth0 から来たフレームを決まりに通す (false なら捨てる)。DHCP の答え (UDP 67 → 68) は落とされても届ける
    /// (Linux の DHCP は AF_PACKET で受けるので、netfilter に関わらず届く)
    fn filter_in(&mut self, f: &mut [u8]) -> bool {
        if !crate::nfhook::active() {
            return true;
        }
        if ipv6_ok(f) {
            return self.filter_in6(f);
        }
        if !ipv4_ok(f) {
            return true;
        }
        use crate::nfhook::{PACKET_BROADCAST, PACKET_HOST, PACKET_MULTICAST};
        let pkttype = if f[..6] == [0xff; 6] {
            PACKET_BROADCAST
        } else if f[0] & 1 != 0 {
            PACKET_MULTICAST
        } else if f[..6] == self.mac {
            PACKET_HOST
        } else {
            // ほかのあて先 (PACKET_OTHERHOST): Linux はフックの前に捨てる
            return true;
        };
        let ip = &f[14..];
        let (saddr, daddr) = (u32::from_be_bytes([ip[12], ip[13], ip[14], ip[15]]), u32::from_be_bytes([ip[16], ip[17], ip[18], ip[19]]));
        // aios あてか (ip_route_input: おかしな送り元とあて先 (martian) は、PRE_ROUTING のあとで捨てる)
        let martian = saddr >> 24 == 127 || saddr >> 28 == 0xe || saddr == u32::MAX || (saddr == 0 && daddr != u32::MAX) || daddr >> 24 == 127 || daddr == 0;
        let me = eth_addr();
        let local = !martian && ((me != 0 && daddr == me) || crate::nfhook::is_bcast_or_mcast(daddr));
        let ihl = (ip[0] & 15) as usize * 4;
        let dhcp = ip[9] == 17 && ip.len() >= ihl + 4 && ip[ihl..ihl + 4] == [0, 67, 0, 68];
        match crate::nfhook::in_eth(f, local, pkttype) {
            crate::nfhook::EthIn::Pass => true,
            crate::nfhook::EthIn::Drop(reply) => {
                if let Some(r) = reply
                    && self.virtio.can_send()
                {
                    count(false, false, r.len());
                    self.virtio.send(r.len(), |b| b.copy_from_slice(&r));
                }
                dhcp
            }
        }
    }

    /// eth0 から来た IPv6 のフレーム (ip6_rcv_finish と ip6_input): 自分の番地か、受けるマルチキャストあてなら LOCAL_IN
    /// も。ほかのあて先 (PACKET_OTHERHOST) は決まりの前に捨てる (smoltcp も)
    fn filter_in6(&mut self, f: &mut [u8]) -> bool {
        use crate::nfhook::{PACKET_HOST, PACKET_MULTICAST};
        let pkttype = if f[0] & 1 != 0 {
            PACKET_MULTICAST
        } else if f[..6] == self.mac {
            PACKET_HOST
        } else {
            return true;
        };
        let dst = u128::from_be_bytes(f[38..54].try_into().unwrap());
        let local = crate::addrconf::is_mine(dst) || (dst >> 120 == 0xff && mc_joined(dst));
        let tot = 54 + u16::from_be_bytes([f[18], f[19]]) as usize;
        match crate::nfhook::in_eth(&mut f[..tot], local, pkttype) {
            crate::nfhook::EthIn::Pass => true,
            crate::nfhook::EthIn::Drop(reply) => {
                if let Some(r) = reply
                    && self.virtio.can_send()
                {
                    count(false, false, r.len());
                    self.virtio.send(r.len(), |b| b.copy_from_slice(&r));
                }
                false
            }
        }
    }

    /// smoltcp が出すフレームを決まりに通す (false なら出さない)。lo は自分あて (4 つのフック)、ほかは eth0
    /// (LOCAL_OUT と POST_ROUTING)。UDP は sendmsg のときに通してある (DHCP は通さない) ので、ここでは見ない
    fn filter_out(&mut self, f: &mut [u8], lo: bool) -> bool {
        if !crate::nfhook::active() {
            return true;
        }
        if ipv6_ok(f) {
            return self.filter_out6(f, lo);
        }
        if !ipv4_ok(f) {
            return true;
        }
        let tot = u16::from_be_bytes([f[16], f[17]]) as usize;
        let proto = f[23];
        if proto == 17 && !lo {
            return true;
        }
        let ip = &mut f[14..14 + tot];
        let sk = crate::inet::ext_owner(ip);
        let path = if lo { crate::nfhook::lo(ip, sk, 0, 0) } else { crate::nfhook::out_eth(ip, sk, 0, 0) };
        let reply = match path {
            crate::nfhook::Path::Pass => return true,
            crate::nfhook::Path::Out(r, _) | crate::nfhook::Path::In(r, _) => r,
        };
        // 返すもの (自分あて): RST は smoltcp に受けさせ、ICMP は poll のあとで inet.rs に
        if let Some(r) = reply {
            if crate::nfhook::icmp_of(&r).is_some() {
                ICMP_LATER.get().push(r);
            } else {
                let mut fr = Vec::with_capacity(14 + r.len());
                fr.extend_from_slice(&self.mac);
                fr.extend_from_slice(&f[..6]);
                fr.extend_from_slice(&[8, 0]);
                fr.extend_from_slice(&r);
                self.lo.push_back(fr);
            }
        }
        false
    }

    /// IPv6 のフレーム (ip6_local_out: LOCAL_OUT と POST_ROUTING、自分あては lo の 4 つ)。UDP は sendmsg のときに、
    /// raw の ICMPv6 は sendto のときに通してある。自分の番地のための NS と NA (Linux にはない: 自分あては近所を
    /// さがさない) は通さない。返すもの (自分あて: reject の RST と ICMPv6) は受け取りの列に
    fn filter_out6(&mut self, f: &mut [u8], lo: bool) -> bool {
        let tot = 54 + u16::from_be_bytes([f[18], f[19]]) as usize;
        let nh = f[20];
        if nh == 17 {
            return true;
        }
        if nh == 58 {
            let v = RAW6_PASSED.get();
            if let Some(i) = v.iter().position(|p| p[..] == f[14..tot]) {
                v.remove(i);
                return true;
            }
            if lo && f.len() > 54 && matches!(f[54], 135 | 136) {
                return true;
            }
        }
        let ip = &mut f[14..tot];
        let sk = crate::inet::ext_owner(ip);
        let path = if lo { crate::nfhook::lo(ip, sk, 0, 0) } else { crate::nfhook::out_eth(ip, sk, 0, 0) };
        let reply = match path {
            crate::nfhook::Path::Pass => return true,
            crate::nfhook::Path::Out(r, _) | crate::nfhook::Path::In(r, _) => r,
        };
        if let Some(r) = reply {
            if r.len() > 40 && r[6] == 58 && r[40] < 128 {
                ICMP6_LATER.get().push(r.clone());
            }
            let mut fr = Vec::with_capacity(14 + r.len());
            fr.extend_from_slice(&self.mac);
            fr.extend_from_slice(&f[..6]);
            fr.extend_from_slice(&[0x86, 0xdd]);
            fr.extend_from_slice(&r);
            self.lo.push_back(fr);
        }
        false
    }
}

/// インターフェースのアドレス: eth0 のもの、それにいつも 127.0.0.1/8、そして eth0 の IPv6 のもの (addrconf)。
/// eth0 を先に置く (smoltcp は同じネットワークのものがなければ先頭を送り元にするので、外へは eth0 から出る)
fn set_addrs(iface: &mut Interface, eth: Option<Ipv4Cidr>) {
    // primary は lease、なければ netlink で足したはじめのもの
    let extra = extra4();
    let first = eth.map(|c| (u32::from_be_bytes(c.address().octets()), c.prefix_len())).or(extra.first().map(|x| (x.addr, x.prefix)));
    ETH_ADDR.store(first.map_or(0, |x| x.0), Ordering::Relaxed);
    ETH_PREFIX.store(first.map_or(0, |x| x.1 as u32), Ordering::Relaxed);
    let v6 = crate::addrconf::addrs();
    iface.update_ip_addrs(|a| {
        a.clear();
        if let Some(c) = eth {
            let _ = a.push(IpCidr::Ipv4(c));
        }
        // smoltcp の番地は 8 つまで: 127.0.0.1 と IPv6 の場所をのこして
        let room = a.capacity().saturating_sub(a.len() + 1 + v6.len());
        for x in extra.iter().take(room) {
            let _ = a.push(IpCidr::Ipv4(Ipv4Cidr::new(Ipv4Address::from_bits(x.addr), x.prefix)));
        }
        let _ = a.push(IpCidr::Ipv4(Ipv4Cidr::new(Ipv4Address::new(127, 0, 0, 1), 8)));
        for x in v6 {
            let _ = a.push(IpCidr::Ipv6(smoltcp::wire::Ipv6Cidr::new(smoltcp::wire::Ipv6Address::from_bits(x.addr), x.prefix)));
        }
    });
}

/// netlink で足した IPv4 の番地の lifetime (check_lifetime: 時間切れは消し、preferred が切れたら deprecated に)
fn expire4(_n: &mut Net) {
    if ADDRS4_N.load(Ordering::Relaxed) != 0 {
        crate::netlink::addr4_tick();
    }
}

/// IPv6 の番地と既定のルーターが変わったら smoltcp に (addrconf の版で見る)
fn sync6(n: &mut Net) {
    let g = crate::addrconf::generation();
    if g == n.v6gen {
        return;
    }
    n.v6gen = g;
    let v4 = n.lease.as_ref().map(|l| l.addr);
    set_addrs(&mut n.iface, v4);
    match crate::addrconf::router() {
        Some(r) => {
            let _ = n.iface.routes_mut().add_default_ipv6_route(smoltcp::wire::Ipv6Address::from_bits(r));
        }
        None => {
            n.iface.routes_mut().remove_default_ipv6_route();
        }
    }
    routes_rebuild(n);
    crate::netlink::changed();
    crate::netlink::changed6();
}

pub fn init() {
    let Some(virtio) = VirtioNet::probe() else { return };
    let mac = EthernetAddress(virtio.mac);
    let mut dev = NetDev { mac: virtio.mac, virtio, lo: VecDeque::new() };
    let mut cfg = Config::new(mac.into());
    cfg.random_seed = crate::rand::u64();
    let mut iface = Interface::new(cfg, &mut dev, now());
    crate::addrconf::init(virtio_mac(&dev));
    set_addrs(&mut iface, None);
    let mut sockets = SocketSet::new(Vec::new());
    let dhcp = sockets.add(dhcpv4::Socket::new());
    crate::irq::enable(dev.virtio.mmio.irq);
    println!("net: {} (dhcp)", mac);
    *NET.get() = Some(Net { iface, sockets, dev, orphans: Vec::new(), dhcp: Some(dhcp), lease: None, v6gen: crate::addrconf::generation() });
    poll();
}

fn virtio_mac(d: &NetDev) -> [u8; 6] {
    d.mac
}

/// 今のアドレス (まだなければ 0.0.0.0)
pub fn addr() -> Ipv4Address {
    get().and_then(|n| n.lease.as_ref()).map_or(Ipv4Address::UNSPECIFIED, |l| l.addr.address())
}

/// インターフェースの名前 (1 つだけ)
pub const IFNAME: &str = "eth0";

/// DHCP でアドレスをもらっているか (false なら手で決めた)
pub fn is_dhcp() -> bool {
    get().is_some_and(|n| n.dhcp.is_some())
}

pub fn mac() -> Option<[u8; 6]> {
    get().map(|n| n.dev.virtio.mac)
}

/// eth0 の virtio の装置の、Linux の virtio バスでの番号 (virtioN)
pub fn virtio_index() -> Option<usize> {
    get().map(|n| crate::virtio::linux_index(n.dev.virtio.mmio.base))
}

/// 今のアドレスとネットマスクの長さ
pub fn cidr() -> Option<Ipv4Cidr> {
    get().and_then(|n| n.lease.as_ref()).map(|l| l.addr)
}

pub fn gateway() -> Option<Ipv4Address> {
    get().and_then(|n| n.lease.as_ref()).and_then(|l| l.router)
}

/// アドレスを手で決める (DHCP はやめる)。0.0.0.0 なら DHCP にもどす。prefix がなければ今のまま (はじめは 24)
pub fn set_addr(addr: Ipv4Address, prefix: Option<u8>) {
    let Some(n) = get() else { return };
    if addr == Ipv4Address::UNSPECIFIED {
        if n.dhcp.is_none() {
            n.dhcp = Some(n.sockets.add(dhcpv4::Socket::new()));
            set_addrs(&mut n.iface, None);
            n.iface.routes_mut().remove_default_ipv4_route();
            set_gw(None);
            n.lease = None;
            println!("net: dhcp");
        }
        poll();
        return;
    }
    if let Some(h) = n.dhcp.take() {
        n.sockets.remove(h);
    }
    let prefix = prefix.or(n.lease.as_ref().map(|l| l.addr.prefix_len())).unwrap_or(24);
    let c = Ipv4Cidr::new(addr, prefix);
    set_addrs(&mut n.iface, Some(c));
    // ゲートウェイと DNS はそのまま (Linux の ip addr と同じく、DNS はさわらない)
    let (router, dns) = n.lease.as_ref().map_or((None, Vec::new()), |l| (l.router, l.dns.clone()));
    let changed = n.lease.as_ref().is_none_or(|l| l.addr != c);
    n.lease = Some(Lease { addr: c, router, dns });
    if changed {
        println!("net: static {}", c);
    }
    poll();
}

/// デフォルトのゲートウェイ (None で消す)
pub fn set_gateway(gw: Option<Ipv4Address>) {
    let Some(n) = get() else { return };
    match gw {
        Some(r) => {
            let _ = n.iface.routes_mut().add_default_ipv4_route(r);
        }
        None => {
            n.iface.routes_mut().remove_default_ipv4_route();
        }
    }
    if let Some(l) = n.lease.as_mut() {
        l.router = gw;
    }
    set_gw(gw);
    poll();
}

/// ip route add の道 (宛先、prefix の長さ、ゲートウェイ)
static EXTRA4: Bkl<Vec<(u32, u8, u32)>> = Bkl::new(Vec::new());

/// ip route add の道を smoltcp の道の表に (既定の道はそのまま。IPv6 の ip -6 route add のものも入れなおす)。入りきらなければ
/// false
pub fn set_routes(rs: &[(u32, u8, u32)]) -> bool {
    *EXTRA4.get() = rs.to_vec();
    let Some(n) = get() else { return false };
    routes_rebuild(n)
}

fn routes_rebuild(n: &mut Net) -> bool {
    let mut ok = true;
    let v4 = EXTRA4.get().clone();
    let v6 = crate::addrconf::user_routes();
    n.iface.routes_mut().update(|v| {
        v.retain(|r| r.cidr.prefix_len() == 0);
        for &(dst, len, gw) in &v4 {
            let r = smoltcp::iface::Route {
                cidr: smoltcp::wire::IpCidr::Ipv4(Ipv4Cidr::new(Ipv4Address::from_bits(dst), len)),
                via_router: smoltcp::wire::IpAddress::Ipv4(Ipv4Address::from_bits(gw)),
                preferred_until: None,
                expires_at: None,
            };
            if v.push(r).is_err() {
                ok = false;
            }
        }
        for &(dst, len, gw) in &v6 {
            // 既定の道 (::/0) は RA のものと同じ場所なので入れない
            if len == 0 {
                continue;
            }
            let r = smoltcp::iface::Route {
                cidr: smoltcp::wire::IpCidr::Ipv6(smoltcp::wire::Ipv6Cidr::new(smoltcp::wire::Ipv6Address::from_bits(dst), len)),
                via_router: smoltcp::wire::IpAddress::Ipv6(smoltcp::wire::Ipv6Address::from_bits(gw)),
                preferred_until: None,
                expires_at: None,
            };
            if v.push(r).is_err() {
                ok = false;
            }
        }
    });
    ok
}

/// DHCP の知らせをインターフェースに映す
fn dhcp_event(n: &mut Net) {
    let Some(h) = n.dhcp else { return };
    let ev = n.sockets.get_mut::<dhcpv4::Socket>(h).poll();
    match ev {
        Some(dhcpv4::Event::Configured(c)) => {
            set_addrs(&mut n.iface, Some(c.address));
            match c.router {
                Some(r) => {
                    let _ = n.iface.routes_mut().add_default_ipv4_route(r);
                }
                None => {
                    n.iface.routes_mut().remove_default_ipv4_route();
                }
            }
            set_gw(c.router);
            let lease = Lease { addr: c.address, router: c.router, dns: c.dns_servers.iter().copied().collect() };
            let changed = n.lease.as_ref().is_none_or(|l| l.addr != lease.addr || l.router != lease.router);
            if changed {
                print!("net: dhcp {}", lease.addr);
                if let Some(r) = lease.router {
                    print!(" gw {}", r);
                }
                for d in &lease.dns {
                    print!(" dns {}", d);
                }
                println!();
            }
            n.lease = Some(lease);
            if changed {
                crate::netlink::changed();
            }
        }
        Some(dhcpv4::Event::Deconfigured) => {
            // 始めにも来るので、持っていたときだけ知らせる
            if n.lease.is_some() {
                println!("net: dhcp lease lost");
            }
            let had = n.lease.is_some();
            set_addrs(&mut n.iface, None);
            n.iface.routes_mut().remove_default_ipv4_route();
            set_gw(None);
            n.lease = None;
            if had {
                crate::netlink::changed();
            }
        }
        None => {}
    }
}

#[track_caller]
pub fn get() -> Option<&'static mut Net> {
    NET.get().as_mut()
}

pub fn irq() -> Option<u32> {
    get().map(|n| n.dev.virtio.mmio.irq)
}

/// ソケットを待っている人が眠る channel
pub fn chan() -> usize {
    &NET as *const _ as usize
}

/// パケットを出し入れし、待っている人を起こす。タイマ、割り込み、システムコールから呼ぶ
pub fn poll() {
    let Some(n) = get() else { return };
    // IPv6: RS を出す、lifetime の切れたものを捨てる、変わったものを smoltcp に
    if let Some(mut rs) = crate::addrconf::tick()
        && n.dev.virtio.can_send()
        && n.dev.filter_out(&mut rs, false)
    {
        count(false, false, rs.len());
        n.dev.virtio.send(rs.len(), |b| b.copy_from_slice(&rs));
    }
    sync6(n);
    let mut changed = n.iface.poll(now(), &mut n.dev, &mut n.sockets) == smoltcp::iface::PollResult::SocketStateChanged;
    // 自分あてに出したものを受け取る (ARP の問い合わせ → 答え → パケット、と続くので何回か)
    for _ in 0..16 {
        // 近所さがしがしくじって捨てたものには、自分から自分へ「たどりつけない」を (lo の列と、inet.rs のソケットに)
        for p in n.iface.take_neighbor_failures() {
            link_failure(n, &p);
            changed = true;
        }
        if n.dev.lo.is_empty() {
            break;
        }
        changed |= n.iface.poll(now(), &mut n.dev, &mut n.sockets) == smoltcp::iface::PollResult::SocketStateChanged;
    }
    dhcp_event(n);
    sync6(n);
    expire4(n);
    crate::netlink::neigh_tick();
    // ファイアウォールが返した ICMP のエラーを、受けるソケットに (smoltcp の poll の外で)
    let later = core::mem::take(ICMP_LATER.get());
    for r in later {
        crate::inet::icmp_error(&r);
    }
    let later6 = core::mem::take(ICMP6_LATER.get());
    for r in later6 {
        crate::inet::icmp6_error(&r);
    }
    // 終わった孤児を片付ける
    let sockets = &mut n.sockets;
    n.orphans.retain(|&h| {
        let s = sockets.get::<smoltcp::socket::tcp::Socket>(h);
        let done = matches!(s.state(), smoltcp::socket::tcp::State::Closed | smoltcp::socket::tcp::State::TimeWait);
        if done {
            sockets.remove(h);
        }
        !done
    });
    // 起こすのはソケットが変わったときだけ。readiness からも呼ばれるので、いつも起こすと
    // poll で待つものどうしが起こしあって CPU を使いきる (sshd と sshd-session で固まった)
    if changed {
        GEN.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        proc::wakeup(chan());
        // poll で見張っているもののうち、ソケット (印は chan()) か何でも (印なし) を見ているものだけ
        proc::poll_wake(chan());
    }
}

/// ソケットが変わった回数 (データが来た、送れるようになった、つながった、閉じた)。epoll の EPOLLET は
/// これが進んだら知らせる (読みきって空になったあとにまた来たのを、ビットだけでは見分けられない)
static GEN: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

pub fn generation() -> u64 {
    GEN.load(core::sync::atomic::Ordering::Relaxed)
}

pub fn intr() {
    if let Some(n) = get() {
        n.dev.virtio.mmio.ack();
    }
    poll();
}

/// TCP を閉じる。FIN が終わるまで持っておく
pub fn orphan(h: SocketHandle) {
    if let Some(n) = get() {
        n.sockets.get_mut::<smoltcp::socket::tcp::Socket>(h).close();
        n.orphans.push(h);
    }
}

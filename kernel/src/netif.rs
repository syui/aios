// ネットワークのインターフェースの ioctl (Linux と同じ番号と形。ソケットの fd に)
//   見る:   SIOCGIFCONF (一覧) SIOCGIFFLAGS SIOCGIFADDR SIOCGIFNETMASK SIOCGIFBRDADDR
//           SIOCGIFHWADDR (MAC) SIOCGIFMTU SIOCGIFINDEX
//   変える: SIOCSIFADDR (0.0.0.0 なら DHCP にもどす) SIOCSIFNETMASK
//           SIOCADDRT SIOCDELRT (デフォルトのゲートウェイだけ)  ← root だけ
// struct ifreq は 名前 16 バイト + 中身 (sockaddr など) 24 バイト。インターフェースは eth0 だけ
use crate::net;
use crate::proc;
use smoltcp::wire::Ipv4Address;

const SIOCADDRT: u64 = 0x890b;
const SIOCDELRT: u64 = 0x890c;
const SIOCGIFCONF: u64 = 0x8912;
const SIOCGIFFLAGS: u64 = 0x8913;
const SIOCGIFADDR: u64 = 0x8915;
const SIOCSIFADDR: u64 = 0x8916;
const SIOCGIFBRDADDR: u64 = 0x8919;
const SIOCGIFNETMASK: u64 = 0x891b;
const SIOCSIFNETMASK: u64 = 0x891c;
const SIOCGIFMTU: u64 = 0x8921;
const SIOCGIFHWADDR: u64 = 0x8927;
const SIOCGIFINDEX: u64 = 0x8933;

const EPERM: i64 = 1;
const EFAULT: i64 = 14;
const ENODEV: i64 = 19;
const EINVAL: i64 = 22;
const ENOTTY: i64 = 25;
const EADDRNOTAVAIL: i64 = 99;

const AF_INET: u16 = 2;
const ARPHRD_ETHER: u16 = 1;
const IFF_UP: u16 = 0x1;
const IFF_BROADCAST: u16 = 0x2;
const IFF_RUNNING: u16 = 0x40;
const IFF_MULTICAST: u16 = 0x1000;
const RTF_GATEWAY: u16 = 0x2;
const IFREQ: usize = 40;

type R = Result<i64, i64>;

/// このリクエストを扱うか
pub fn handles(req: u64) -> bool {
    matches!(
        req,
        SIOCADDRT | SIOCDELRT | SIOCGIFCONF | SIOCGIFFLAGS | SIOCGIFADDR | SIOCSIFADDR | SIOCGIFBRDADDR | SIOCGIFNETMASK | SIOCSIFNETMASK | SIOCGIFMTU | SIOCGIFHWADDR | SIOCGIFINDEX
    )
}

fn copy_in(buf: &mut [u8], va: usize) -> Result<(), i64> {
    proc::current().pt().copy_in(buf, va).ok_or(-EFAULT)
}

fn copy_out(va: usize, buf: &[u8]) -> Result<(), i64> {
    proc::current().pt().copy_out(va, buf).ok_or(-EFAULT)
}

fn root() -> Result<(), i64> {
    if proc::current().cred.euid != 0 {
        return Err(-EPERM);
    }
    Ok(())
}

/// sockaddr_in (AF_INET, ポート 0, アドレス)
fn sockaddr(a: Ipv4Address) -> [u8; 16] {
    let mut s = [0u8; 16];
    s[0..2].copy_from_slice(&AF_INET.to_le_bytes());
    s[4..8].copy_from_slice(&a.octets());
    s
}

fn addr_of(s: &[u8]) -> Ipv4Address {
    Ipv4Address::new(s[4], s[5], s[6], s[7])
}

fn mask_of(prefix: u8) -> Ipv4Address {
    let m = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix as u32) };
    Ipv4Address::from_bits(m)
}

/// ネットマスクの長さ (つながった 1 でなければ None)
fn prefix_of(m: Ipv4Address) -> Option<u8> {
    let b = m.to_bits();
    let n = b.leading_ones();
    (b.checked_shl(n).unwrap_or(0) == 0).then_some(n as u8)
}

pub fn ioctl(req: u64, arg: usize) -> R {
    if net::get().is_none() {
        return Err(-ENODEV);
    }
    if req == SIOCGIFCONF {
        // struct ifconf { int ifc_len; char *ifc_buf; }
        let mut c = [0u8; 16];
        copy_in(&mut c, arg)?;
        let len = i32::from_le_bytes(c[0..4].try_into().unwrap()) as usize;
        let buf = u64::from_le_bytes(c[8..16].try_into().unwrap()) as usize;
        let mut n = 0;
        if buf != 0 && len >= IFREQ {
            let mut r = [0u8; IFREQ];
            r[..net::IFNAME.len()].copy_from_slice(net::IFNAME.as_bytes());
            r[16..32].copy_from_slice(&sockaddr(net::addr()));
            copy_out(buf, &r)?;
            n = IFREQ;
        } else if buf == 0 {
            n = IFREQ;
        }
        copy_out(arg, &(n as i32).to_le_bytes())?;
        return Ok(0);
    }
    if req == SIOCADDRT || req == SIOCDELRT {
        root()?;
        // struct rtentry: rt_pad1 (8), rt_dst (16), rt_gateway (16), rt_genmask (16), rt_flags (2)
        let mut rt = [0u8; 58];
        copy_in(&mut rt, arg)?;
        let (dst, gw, mask) = (addr_of(&rt[8..24]), addr_of(&rt[24..40]), addr_of(&rt[40..56]));
        let flags = u16::from_le_bytes([rt[56], rt[57]]);
        // デフォルトの経路 (0.0.0.0/0) だけ
        if dst != Ipv4Address::UNSPECIFIED || mask != Ipv4Address::UNSPECIFIED {
            return Err(-EINVAL);
        }
        if req == SIOCDELRT {
            net::set_gateway(None);
        } else {
            if flags & RTF_GATEWAY == 0 || gw == Ipv4Address::UNSPECIFIED {
                return Err(-EINVAL);
            }
            net::set_gateway(Some(gw));
        }
        return Ok(0);
    }
    if !handles(req) {
        return Err(-ENOTTY);
    }
    let mut r = [0u8; IFREQ];
    copy_in(&mut r, arg)?;
    let name_len = r[..16].iter().position(|&b| b == 0).unwrap_or(16);
    if &r[..name_len] != net::IFNAME.as_bytes() {
        return Err(-ENODEV);
    }
    let cidr = net::cidr();
    match req {
        SIOCGIFFLAGS => {
            let f = IFF_UP | IFF_BROADCAST | IFF_RUNNING | IFF_MULTICAST;
            r[16..18].copy_from_slice(&f.to_le_bytes());
        }
        SIOCGIFADDR => {
            let c = cidr.ok_or(-EADDRNOTAVAIL)?;
            r[16..32].copy_from_slice(&sockaddr(c.address()));
        }
        SIOCGIFNETMASK => {
            let c = cidr.ok_or(-EADDRNOTAVAIL)?;
            r[16..32].copy_from_slice(&sockaddr(mask_of(c.prefix_len())));
        }
        SIOCGIFBRDADDR => {
            let c = cidr.ok_or(-EADDRNOTAVAIL)?;
            let b = c.address().to_bits() | !mask_of(c.prefix_len()).to_bits();
            r[16..32].copy_from_slice(&sockaddr(Ipv4Address::from_bits(b)));
        }
        SIOCGIFHWADDR => {
            r[16..18].copy_from_slice(&ARPHRD_ETHER.to_le_bytes());
            r[18..24].copy_from_slice(&net::mac().unwrap_or_default());
        }
        SIOCGIFMTU => r[16..20].copy_from_slice(&1500i32.to_le_bytes()),
        SIOCGIFINDEX => r[16..20].copy_from_slice(&1i32.to_le_bytes()),
        SIOCSIFADDR => {
            root()?;
            net::set_addr(addr_of(&r[16..32]), None);
            return Ok(0);
        }
        SIOCSIFNETMASK => {
            root()?;
            let p = prefix_of(addr_of(&r[16..32])).ok_or(-EINVAL)?;
            let c = cidr.ok_or(-EADDRNOTAVAIL)?;
            net::set_addr(c.address(), Some(p));
            return Ok(0);
        }
        _ => return Err(-ENOTTY),
    }
    copy_out(arg, &r)?;
    Ok(0)
}

/// /proc/net/route (Linux と同じ形。アドレスは 16 進で、メモリの並びのまま)
pub fn proc_route() -> alloc::string::String {
    use alloc::format;
    let hex = |a: Ipv4Address| format!("{:08X}", u32::from_le_bytes(a.octets()));
    let mut s = alloc::string::String::from("Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n");
    if let Some(c) = net::cidr() {
        if let Some(g) = net::gateway() {
            s += &format!("{}\t{}\t{}\t0003\t0\t0\t0\t{}\t0\t0\t0\n", net::IFNAME, hex(Ipv4Address::UNSPECIFIED), hex(g), hex(Ipv4Address::UNSPECIFIED));
        }
        let m = mask_of(c.prefix_len());
        let netw = Ipv4Address::from_bits(c.address().to_bits() & m.to_bits());
        s += &format!("{}\t{}\t{}\t0001\t0\t0\t0\t{}\t0\t0\t0\n", net::IFNAME, hex(netw), hex(Ipv4Address::UNSPECIFIED), hex(m));
    }
    s
}

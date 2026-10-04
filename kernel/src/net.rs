// ネットワーク: virtio-net を smoltcp (TCP/IP) につなぐ
// アドレスは DHCP でもらう (Linux の ip=dhcp のように、カーネルの中で)。
// もらった DNS は /proc/net/pnp に出す (/etc/resolv.conf はそこへのリンク)
// アドレスを手で決める (ioctl の SIOCSIFADDR、ip addr add、networkd) と DHCP はやめる。
// 0.0.0.0 にすると DHCP にもどす。インターフェースは 1 つ (eth0)
use crate::proc;
use crate::timer;
use crate::virtio_net::VirtioNet;
use alloc::vec::Vec;
use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, IpCidr, Ipv4Address};

use smoltcp::socket::dhcpv4;
use smoltcp::wire::Ipv4Cidr;

pub struct Net {
    pub iface: Interface,
    pub sockets: SocketSet<'static>,
    dev: VirtioNet,
    /// close されたが、まだ FIN のやりとりが残っている TCP
    orphans: Vec<SocketHandle>,
    /// DHCP のソケット (アドレスを手で決めたら None)
    dhcp: Option<SocketHandle>,
    /// DHCP でもらったもの、または手で決めたもの (dns は DHCP でもらっていたもの)
    pub lease: Option<Lease>,
}

#[derive(Clone)]
pub struct Lease {
    pub addr: Ipv4Cidr,
    pub router: Option<Ipv4Address>,
    pub dns: Vec<Ipv4Address>,
}

static mut NET: Option<Net> = None;

pub fn now() -> Instant {
    Instant::from_micros((timer::uptime_ns() / 1000) as i64)
}

impl Device for VirtioNet {
    type RxToken<'a> = Rx;
    type TxToken<'a> = Tx<'a>;

    fn receive(&mut self, _t: Instant) -> Option<(Rx, Tx<'_>)> {
        if !self.can_send() {
            return None;
        }
        let frame = self.recv()?;
        Some((Rx(frame), Tx(self)))
    }

    fn transmit(&mut self, _t: Instant) -> Option<Tx<'_>> {
        self.can_send().then_some(Tx(self))
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

pub struct Tx<'a>(&'a mut VirtioNet);

impl TxToken for Tx<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        self.0.send(len, f)
    }
}

pub fn init() {
    let Some(mut dev) = VirtioNet::probe() else { return };
    let mac = EthernetAddress(dev.mac);
    let mut cfg = Config::new(mac.into());
    cfg.random_seed = crate::rand::next();
    let iface = Interface::new(cfg, &mut dev, now());
    let mut sockets = SocketSet::new(Vec::new());
    let dhcp = sockets.add(dhcpv4::Socket::new());
    crate::irq::enable(dev.mmio.irq);
    println!("net: {} (dhcp)", mac);
    unsafe { *(&raw mut NET) = Some(Net { iface, sockets, dev, orphans: Vec::new(), dhcp: Some(dhcp), lease: None }) };
    poll();
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
    get().map(|n| n.dev.mac)
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
            n.iface.update_ip_addrs(|a| a.clear());
            n.iface.routes_mut().remove_default_ipv4_route();
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
    n.iface.update_ip_addrs(|a| {
        a.clear();
        let _ = a.push(IpCidr::Ipv4(c));
    });
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
    poll();
}

/// DHCP の知らせをインターフェースに映す
fn dhcp_event(n: &mut Net) {
    let Some(h) = n.dhcp else { return };
    let ev = n.sockets.get_mut::<dhcpv4::Socket>(h).poll();
    match ev {
        Some(dhcpv4::Event::Configured(c)) => {
            n.iface.update_ip_addrs(|a| {
                a.clear();
                let _ = a.push(IpCidr::Ipv4(c.address));
            });
            match c.router {
                Some(r) => {
                    let _ = n.iface.routes_mut().add_default_ipv4_route(r);
                }
                None => {
                    n.iface.routes_mut().remove_default_ipv4_route();
                }
            }
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
        }
        Some(dhcpv4::Event::Deconfigured) => {
            // 始めにも来るので、持っていたときだけ知らせる
            if n.lease.is_some() {
                println!("net: dhcp lease lost");
            }
            n.iface.update_ip_addrs(|a| a.clear());
            n.iface.routes_mut().remove_default_ipv4_route();
            n.lease = None;
        }
        None => {}
    }
}

pub fn get() -> Option<&'static mut Net> {
    unsafe { (*(&raw mut NET)).as_mut() }
}

pub fn irq() -> Option<u32> {
    get().map(|n| n.dev.mmio.irq)
}

/// ソケットを待っている人が眠る channel
pub fn chan() -> usize {
    (&raw const NET) as usize
}

/// パケットを出し入れし、待っている人を起こす。タイマ、割り込み、システムコールから呼ぶ
pub fn poll() {
    let Some(n) = get() else { return };
    let changed = n.iface.poll(now(), &mut n.dev, &mut n.sockets) == smoltcp::iface::PollResult::SocketStateChanged;
    dhcp_event(n);
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
        proc::wakeup(chan());
        proc::wakeup(proc::poll_chan());
    }
}

pub fn intr() {
    if let Some(n) = get() {
        n.dev.mmio.ack();
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

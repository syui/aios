// ネットワーク: virtio-net を smoltcp (TCP/IP) につなぐ
use crate::proc;
use crate::timer;
use crate::virtio_net::VirtioNet;
use alloc::vec::Vec;
use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, IpCidr, Ipv4Address};

/// QEMU の user ネットワーク (slirp) の決まった値
pub const ADDR: Ipv4Address = Ipv4Address::new(10, 0, 2, 15);
pub const GATEWAY: Ipv4Address = Ipv4Address::new(10, 0, 2, 2);
const PREFIX: u8 = 24;

pub struct Net {
    pub iface: Interface,
    pub sockets: SocketSet<'static>,
    dev: VirtioNet,
    /// close されたが、まだ FIN のやりとりが残っている TCP
    orphans: Vec<SocketHandle>,
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
    let mut iface = Interface::new(cfg, &mut dev, now());
    iface.update_ip_addrs(|a| {
        let _ = a.push(IpCidr::new(ADDR.into(), PREFIX));
    });
    let _ = iface.routes_mut().add_default_ipv4_route(GATEWAY);
    crate::gic::enable(dev.mmio.irq);
    println!("net: {} addr {}/{} gw {}", mac, ADDR, PREFIX, GATEWAY);
    unsafe { *(&raw mut NET) = Some(Net { iface, sockets: SocketSet::new(Vec::new()), dev, orphans: Vec::new() }) };
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
    n.iface.poll(now(), &mut n.dev, &mut n.sockets);
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
    proc::wakeup(chan());
    proc::wakeup(proc::poll_chan());
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

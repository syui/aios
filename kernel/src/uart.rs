// PL011 UART。場所と割り込みは DTB から (なければ qemu virt の PA 0x0900_0000, SPI 1)
//
// コンソールへの write はここの輪 (TX) にためて、送信の割り込みで少しずつ出す。
// 1 文字ごとの MMIO は仮想化だと 10 µs ほどかかるので、送り終わるまで待つと write 1 回で
// 何 ms も大きなロックを持つことになる (/proc/bkl の write)。カーネルの println はそのまま出す
// (止まったときにも見えるように)。輪に残っているものを先に出すので順番は変わらない
use crate::spinlock::SpinLock;
use core::fmt;
use core::sync::atomic::{AtomicBool, Ordering};
use crate::mmio;

const DR: usize = 0x00;
const FR: usize = 0x18;
const IMSC: usize = 0x38;
const MIS: usize = 0x40;
const ICR: usize = 0x44;
const FR_RXFE: u32 = 1 << 4;
const FR_TXFF: u32 = 1 << 5;
const INT_RX: u32 = 1 << 4;
const INT_TX: u32 = 1 << 5;
const INT_RT: u32 = 1 << 6;

/// コンソールへの出力の輪 (送信の割り込みで出す)
struct Ring {
    buf: [u8; TXQ],
    head: usize,
    tail: usize,
}
const TXQ: usize = 32 * 1024;
static TX: SpinLock<Ring> = SpinLock::new(Ring { buf: [0; TXQ], head: 0, tail: 0 });
/// 輪が空くのを待って眠っている write がある (起こすには大きなロックがいる)
static TX_WAITING: AtomicBool = AtomicBool::new(false);

impl Ring {
    fn len(&self) -> usize {
        self.head - self.tail
    }
    /// FIFO にはいるだけ出す (QEMU の FIFO はいっぱいにならないので、1 回 32 文字まで。残りは割り込みで)。
    /// 空になったら送信の割り込みを止める
    fn drain(&mut self) {
        let upto = self.tail + 32;
        while self.tail != self.head && self.tail != upto && mmio::r32(reg(FR)) & FR_TXFF == 0 {
            mmio::w32(reg(DR), self.buf[self.tail % TXQ] as u32);
            self.tail += 1;
        }
        let im = mmio::r32(reg(IMSC));
        let want = if self.tail != self.head { im | INT_TX } else { im & !INT_TX };
        if want != im {
            mmio::w32(reg(IMSC), want);
        }
    }
    /// 全部出す (println の前と、止まるとき)
    fn flush(&mut self) {
        while self.tail != self.head {
            putc(self.buf[self.tail % TXQ]);
            self.tail += 1;
        }
    }
}

/// 輪の空き (コンソールの write はこれだけ書ける)
pub fn tx_room() -> usize {
    TXQ - TX.lock().len()
}

/// 空くのを待つ場所 (proc::sleep / wakeup)。眠る前に呼ぶ
pub fn tx_chan() -> usize {
    TX_WAITING.store(true, Ordering::Release);
    &TX as *const _ as usize
}

/// 割り込みのうち、大きなロックなしで片づくもの: 送信だけなら輪から次を出して true。
/// 受信があるか、空くのを待っている write を起こすなら false (intr へ)
pub fn intr_fast() -> bool {
    let mis = mmio::r32(reg(MIS));
    if mis & (INT_RX | INT_RT) != 0 {
        return false;
    }
    mmio::w32(reg(ICR), INT_TX);
    TX.lock().drain();
    !TX_WAITING.load(Ordering::Acquire)
}

/// 輪に足して、はいるぶんだけ出しはじめる。空きより多ければ余りは捨てる (tx_room で確かめてから)
pub fn tx_push(b: &[u8]) {
    if unsafe { BASE } == 0 {
        return;
    }
    let mut r = TX.lock();
    for &c in b.iter().take(TXQ - r.len()) {
        let h = r.head;
        r.buf[h % TXQ] = c;
        r.head += 1;
    }
    r.drain();
}

static mut BASE: usize = 0;
static mut IRQ: u32 = 33;

fn reg(off: usize) -> usize {
    unsafe { BASE + off }
}

/// 何よりも先に (dtb::init のすぐ後): 出力の場所を決める
pub fn early_init() {
    let pa = crate::dtb::reg_of("arm,pl011", 0).map_or(0x0900_0000, |(a, _)| a as usize);
    unsafe { BASE = crate::memlayout::p2v(pa) };
}

pub fn irq() -> u32 {
    unsafe { IRQ }
}

/// 受信の割り込みを有効にする (irq::init の後)
pub fn init() {
    if let Some(i) = crate::irq::from_dt("arm,pl011") {
        unsafe { IRQ = i };
    }
    mmio::w32(reg(IMSC), INT_RX | INT_RT);
    crate::irq::enable(irq());
}

/// 受信した文字をすべてコンソールへ渡す
pub fn intr() {
    // 先に下げてから読む (読んだ後に下げると、その間に来た文字の割り込みを消してしまう)
    mmio::w32(reg(ICR), INT_RX | INT_RT | INT_TX);
    {
        let mut r = TX.lock();
        r.drain();
        // 待っていた write を起こす
        if TX_WAITING.swap(false, Ordering::AcqRel) {
            crate::proc::wakeup(&TX as *const _ as usize);
        }
    }
    rx();
}

/// 受信を止めている (端末の入力がいっぱい)
static RX_STOPPED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// 端末が読まれて場所が空いたら (tty::read から): 止めていた受信を再開して、たまっていたものを読む
pub fn rx_resume() {
    if unsafe { BASE } == 0 || !RX_STOPPED.swap(false, Ordering::AcqRel) {
        return;
    }
    let im = mmio::r32(reg(IMSC));
    mmio::w32(reg(IMSC), im | INT_RX | INT_RT);
    rx();
}

/// FIFO から読んで端末へ。端末がいっぱいなら、読まずに受信の割り込みを止める (流れの制御: 文字は QEMU に残り、
/// 送り手 (端末に打つ人や test/vm.py) が待たされる。捨てない)
fn rx() {
    while mmio::r32(reg(FR)) & FR_RXFE == 0 {
        if !crate::tty::console_room() {
            let im = mmio::r32(reg(IMSC));
            mmio::w32(reg(IMSC), im & !(INT_RX | INT_RT));
            RX_STOPPED.store(true, Ordering::Release);
            return;
        }
        let c = mmio::r32(reg(DR)) as u8;
        // Ctrl-] : 固まって見えるときのために、すべてのスレッドの状態をじかに出す
        // (シェルが動かなくても、カーネルが生きていれば出る。Linux の SysRq のかわり)
        if c == 0x1d {
            crate::println!("\n{}", crate::proc::threads_text());
            continue;
        }
        crate::console::intr(c);
    }
}

pub fn putc(c: u8) {
    unsafe {
        if BASE == 0 {
            return;
        }
        while mmio::r32(reg(FR)) & FR_TXFF != 0 {}
        mmio::w32(reg(DR), c as u32);
    }
}

pub struct Uart;

pub static LOCK: SpinLock<()> = SpinLock::new(());

impl fmt::Write for Uart {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        crate::kmsg::push(s);
        // 先に輪のぶんを出してから (順番を守る)
        if let Some(mut r) = TX.try_lock() {
            r.flush();
        }
        for b in s.bytes() {
            if b == b'\n' {
                putc(b'\r');
            }
            putc(b);
        }
        Ok(())
    }
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {{
        use core::fmt::Write;
        let _g = $crate::uart::LOCK.lock();
        let _ = write!($crate::uart::Uart, $($arg)*);
    }};
}

#[macro_export]
macro_rules! println {
    () => { $crate::print!("\n") };
    ($($arg:tt)*) => { $crate::print!("{}\n", format_args!($($arg)*)) };
}

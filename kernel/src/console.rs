// コンソール入力 (PL011 の受信割り込み) と行編集
use crate::proc;
use crate::uart;

const BUF: usize = 1024;

struct Input {
    buf: [u8; BUF],
    r: usize, // read が読んだところ
    w: usize, // 行として確定したところ
    e: usize, // 編集中のところ
}

static mut INPUT: Input = Input { buf: [0; BUF], r: 0, w: 0, e: 0 };

fn input() -> &'static mut Input {
    unsafe { &mut *(&raw mut INPUT) }
}

fn chan() -> usize {
    (&raw const INPUT) as usize
}

const CTRL_D: u8 = 4;
const CTRL_U: u8 = 0x15;
const BS: u8 = 0x08;
const DEL: u8 = 0x7f;

fn echo(c: u8) {
    let _g = uart::LOCK.lock();
    uart::putc(c);
}

/// 割り込みから 1 文字ずつ
pub fn intr(c: u8) {
    let i = input();
    match c {
        BS | DEL => {
            if i.e != i.w {
                i.e -= 1;
                let _g = uart::LOCK.lock();
                uart::putc(BS);
                uart::putc(b' ');
                uart::putc(BS);
            }
        }
        CTRL_U => {
            while i.e != i.w && i.buf[(i.e - 1) % BUF] != b'\n' {
                i.e -= 1;
                let _g = uart::LOCK.lock();
                uart::putc(BS);
                uart::putc(b' ');
                uart::putc(BS);
            }
        }
        _ => {
            if i.e - i.r >= BUF {
                return;
            }
            let c = if c == b'\r' { b'\n' } else { c };
            i.buf[i.e % BUF] = c;
            i.e += 1;
            if c == b'\n' {
                echo(b'\r');
            }
            if c != CTRL_D {
                echo(c);
            }
            if c == b'\n' || c == CTRL_D || i.e - i.r == BUF {
                i.w = i.e;
                proc::wakeup(chan());
            }
        }
    }
}

/// 1 行 (または Ctrl-D まで) 読む
pub fn read(dst: &mut [u8]) -> usize {
    let i = input();
    while i.r == i.w {
        proc::sleep(chan());
    }
    let mut n = 0;
    while n < dst.len() && i.r != i.w {
        let c = i.buf[i.r % BUF];
        i.r += 1;
        if c == CTRL_D {
            // 行の途中なら次の read で 0 を返すよう残す
            if n > 0 {
                i.r -= 1;
            }
            break;
        }
        dst[n] = c;
        n += 1;
        if c == b'\n' {
            break;
        }
    }
    n
}

pub fn write(src: &[u8]) {
    let _g = uart::LOCK.lock();
    for &c in src {
        if c == b'\n' {
            uart::putc(b'\r');
        }
        uart::putc(c);
    }
}

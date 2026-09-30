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

/// 前にいるプロセスグループ (Ctrl-C の SIGINT を受け取る)
static mut FG_PGRP: u32 = 0;

pub fn fg_pgrp() -> u32 {
    unsafe { FG_PGRP }
}

pub fn set_fg_pgrp(pg: u32) {
    unsafe { FG_PGRP = pg };
}

/// 打った文字を画面に出すか (termios の ECHO)
static mut ECHO: bool = true;

pub fn echo_enabled() -> bool {
    unsafe { ECHO }
}

pub fn set_echo(on: bool) {
    unsafe { ECHO = on };
}

fn input() -> &'static mut Input {
    unsafe { &mut *(&raw mut INPUT) }
}

fn chan() -> usize {
    (&raw const INPUT) as usize
}

const CTRL_D: u8 = 4;
const CTRL_C: u8 = 0x03;
const CTRL_BACKSLASH: u8 = 0x1c;
const CTRL_P: u8 = 0x10;
const CTRL_U: u8 = 0x15;
const BS: u8 = 0x08;
const DEL: u8 = 0x7f;

fn echo(c: u8) {
    if !echo_enabled() {
        return;
    }
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
                if !echo_enabled() {
                    return;
                }
                let _g = uart::LOCK.lock();
                uart::putc(BS);
                uart::putc(b' ');
                uart::putc(BS);
            }
        }
        CTRL_P => proc::dump(),
        CTRL_C | CTRL_BACKSLASH => {
            // 編集中の行を捨て、前にいるグループへ SIGINT / SIGQUIT
            i.e = i.w;
            {
                let _g = uart::LOCK.lock();
                for &ch in if c == CTRL_C { b"^C\r\n" } else { b"^\\\r\n" } {
                    uart::putc(ch);
                }
            }
            let sig = if c == CTRL_C { crate::signal::SIGINT } else { crate::signal::SIGQUIT };
            let pg = fg_pgrp();
            if pg != 0 {
                crate::signal::send_pgrp(pg, sig, crate::signal::SigInfo { code: crate::signal::SI_KERNEL, ..crate::signal::SigInfo::ZERO });
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
            if c == b'\n' && echo_enabled() {
                echo(b'\r');
            }
            if c != CTRL_D {
                echo(c);
            }
            if c == b'\n' || c == CTRL_D || i.e - i.r == BUF {
                i.w = i.e;
                proc::wakeup(chan());
                proc::wakeup(proc::poll_chan());
            }
        }
    }
}

/// 読める行があるか
pub fn ready() -> bool {
    let i = input();
    i.r != i.w
}

/// 1 行 (または Ctrl-D まで) 読む
pub fn read(dst: &mut [u8]) -> Result<usize, i64> {
    let i = input();
    while i.r == i.w {
        proc::sleep(chan())?;
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
    Ok(n)
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

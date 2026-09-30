// 端末 (tty) と行規律 (termios)。コンソールと疑似端末 (pty) で同じものを使う
//
// 疑似端末は /dev/ptmx を開くと 1 組でき、親 (master) の口がその fd、
// 子 (slave) の口が /dev/pts/N になる。親に書いたものは子の入力として
// 行規律を通り、子に書いたものは親から読める。
use crate::proc;
use crate::signal::{self, SigInfo};
use crate::uart;
use alloc::collections::VecDeque;
use alloc::rc::Rc;
use alloc::vec::Vec;
use core::cell::RefCell;

const EIO: i64 = 5;
const EAGAIN: i64 = 11;
const EPERM: i64 = 1;
const ENOTTY: i64 = 25;
const EFAULT: i64 = 14;
const EINVAL: i64 = 22;

// c_iflag
const ISTRIP: u32 = 0o40;
const INLCR: u32 = 0o100;
const IGNCR: u32 = 0o200;
const ICRNL: u32 = 0o400;
const IUTF8: u32 = 0o40000;
// c_oflag
const OPOST: u32 = 0o1;
const ONLCR: u32 = 0o4;
const OCRNL: u32 = 0o10;
// c_cflag (B38400 | CS8 | CREAD | HUPCL)
const CFLAG: u32 = 0o17 | 0o60 | 0o200 | 0o2000;
// c_lflag
const ISIG: u32 = 0o1;
const ICANON: u32 = 0o2;
const ECHO: u32 = 0o10;
const ECHOE: u32 = 0o20;
const ECHOK: u32 = 0o40;
const ECHONL: u32 = 0o100;
const NOFLSH: u32 = 0o200;
const ECHOCTL: u32 = 0o1000;
const ECHOKE: u32 = 0o4000;
const IEXTEN: u32 = 0o100000;

// c_cc の添字
const VINTR: usize = 0;
const VQUIT: usize = 1;
const VERASE: usize = 2;
const VKILL: usize = 3;
const VEOF: usize = 4;
const VTIME: usize = 5;
const VMIN: usize = 6;
const VSUSP: usize = 10;
const VEOL: usize = 11;
const VWERASE: usize = 14;
const VLNEXT: usize = 15;
const VEOL2: usize = 16;
const NCCS: usize = 19;

const SIGHUP: i32 = 1;
const SIGTSTP: i32 = 20;

/// 入力をためておける量
const INQ: usize = 4096;
/// 子が書いて親がまだ読んでいない量の上限
const OUTQ: usize = 64 * 1024;
/// 入力の列で「ここでファイルの終わり」(行頭の Ctrl-D) を表す印
const EOF_MARK: u16 = 0x100;

/// カーネルの struct termios (36 バイト)
#[derive(Clone, Copy)]
pub struct Termios {
    pub iflag: u32,
    pub oflag: u32,
    pub cflag: u32,
    pub lflag: u32,
    pub line: u8,
    pub cc: [u8; NCCS],
}

impl Termios {
    const DEFAULT: Termios = Termios {
        iflag: ICRNL | IUTF8,
        oflag: OPOST | ONLCR,
        cflag: CFLAG,
        lflag: ISIG | ICANON | ECHO | ECHOE | ECHOK | ECHOCTL | ECHOKE | IEXTEN,
        line: 0,
        // ^C ^\ DEL ^U ^D 0 1 0 ^Q ^S ^Z 0 ^R ^O ^W ^V 0 0 0
        cc: [3, 0x1c, 0x7f, 0x15, 4, 0, 1, 0, 0x11, 0x13, 0x1a, 0, 0x12, 0x0f, 0x17, 0x16, 0, 0, 0],
    };

    fn to_bytes(&self) -> [u8; 36] {
        let mut b = [0u8; 36];
        b[0..4].copy_from_slice(&self.iflag.to_le_bytes());
        b[4..8].copy_from_slice(&self.oflag.to_le_bytes());
        b[8..12].copy_from_slice(&self.cflag.to_le_bytes());
        b[12..16].copy_from_slice(&self.lflag.to_le_bytes());
        b[16] = self.line;
        b[17..36].copy_from_slice(&self.cc);
        b
    }

    fn from_bytes(b: &[u8; 36]) -> Termios {
        let w = |i: usize| u32::from_le_bytes(b[i..i + 4].try_into().unwrap());
        let mut cc = [0u8; NCCS];
        cc.copy_from_slice(&b[17..36]);
        Termios { iflag: w(0), oflag: w(4), cflag: w(8), lflag: w(12), line: b[16], cc }
    }
}

/// 疑似端末の親子の状態
pub struct Pty {
    pub index: usize,
    /// 親の口が開いているか
    pub master: bool,
    /// 開いている子の口の数
    pub slaves: usize,
    /// unlockpt するまでは子を開けない
    pub locked: bool,
    /// 子が書いて、親が読むもの
    out: VecDeque<u8>,
}

pub enum Dev {
    Console,
    Pty(Pty),
}

pub struct Tty {
    pub t: Termios,
    /// 読める入力 (カノニカルでは確定した行)
    inq: VecDeque<u16>,
    /// inq の中の行の区切りの数
    lines: usize,
    /// 編集中の行
    edit: Vec<u8>,
    /// Ctrl-V の直後 (次の 1 文字をそのまま入れる)
    lnext: bool,
    /// 前にいるプロセスグループ
    pub pgrp: u32,
    /// この端末を制御端末にしているセッション (0 はなし)
    pub session: u32,
    winsize: [u8; 8],
    pub dev: Dev,
}

pub type TtyRef = Rc<RefCell<Tty>>;

impl Tty {
    fn new(dev: Dev) -> Tty {
        let mut winsize = [0u8; 8];
        winsize[0..2].copy_from_slice(&24u16.to_le_bytes());
        winsize[2..4].copy_from_slice(&80u16.to_le_bytes());
        Tty {
            t: Termios::DEFAULT,
            inq: VecDeque::new(),
            lines: 0,
            edit: Vec::new(),
            lnext: false,
            pgrp: 0,
            session: 0,
            winsize,
            dev,
        }
    }

    fn lflag(&self, f: u32) -> bool {
        self.t.lflag & f != 0
    }

    fn canon(&self) -> bool {
        self.lflag(ICANON)
    }

    fn pty(&mut self) -> Option<&mut Pty> {
        match &mut self.dev {
            Dev::Pty(p) => Some(p),
            Dev::Console => None,
        }
    }

    /// 親が閉じられた疑似端末
    fn hung_up(&self) -> bool {
        matches!(&self.dev, Dev::Pty(p) if !p.master)
    }

    /// 出力処理 (OPOST) をして、画面 (または親の口) へ
    fn emit(&mut self, src: &[u8]) {
        let post = self.t.oflag & OPOST != 0;
        let onlcr = post && self.t.oflag & ONLCR != 0;
        let ocrnl = post && self.t.oflag & OCRNL != 0;
        match &mut self.dev {
            Dev::Console => {
                let _g = uart::LOCK.lock();
                for &c in src {
                    match c {
                        b'\n' if onlcr => {
                            uart::putc(b'\r');
                            uart::putc(b'\n');
                        }
                        b'\r' if ocrnl => uart::putc(b'\n'),
                        _ => uart::putc(c),
                    }
                }
            }
            Dev::Pty(p) => {
                for &c in src {
                    match c {
                        b'\n' if onlcr => p.out.extend([b'\r', b'\n']),
                        b'\r' if ocrnl => p.out.push_back(b'\n'),
                        _ => p.out.push_back(c),
                    }
                }
            }
        }
    }

    /// 打った文字を見せる (制御文字は ^X の形で)
    fn echo(&mut self, c: u8) {
        if !self.lflag(ECHO) {
            if c == b'\n' && self.lflag(ECHONL) && self.canon() {
                self.emit(b"\n");
            }
            return;
        }
        if self.lflag(ECHOCTL) && (c < 0x20 && c != b'\t' && c != b'\n' || c == 0x7f) {
            self.emit(&[b'^', c ^ 0x40]);
        } else {
            self.emit(&[c]);
        }
    }

    /// 画面上の幅 (^X で出した文字は 2)
    fn width(&self, c: u8) -> usize {
        if self.lflag(ECHOCTL) && (c < 0x20 && c != b'\t' || c == 0x7f) { 2 } else { 1 }
    }

    /// 編集中の行から 1 文字消す (UTF-8 の続きのバイトもまとめて)
    fn erase_one(&mut self) -> bool {
        let Some(mut c) = self.edit.pop() else { return false };
        while c & 0xc0 == 0x80 {
            match self.edit.pop() {
                Some(p) => c = p,
                None => break,
            }
        }
        if self.lflag(ECHO) && self.lflag(ECHOE) {
            for _ in 0..self.width(c) {
                self.emit(b"\x08 \x08");
            }
        }
        true
    }

    fn flush_input(&mut self) {
        self.inq.clear();
        self.lines = 0;
        self.edit.clear();
    }

    /// 編集中の行を確定して読めるようにする
    fn commit(&mut self, end: Option<u16>) {
        self.inq.extend(self.edit.drain(..).map(u16::from));
        if let Some(e) = end {
            self.inq.push_back(e);
        }
        self.lines += 1;
    }

    fn is_eol(&self, c: u8) -> bool {
        c == b'\n' || (c != 0 && (c == self.t.cc[VEOL] || c == self.t.cc[VEOL2]))
    }

    /// 入力 1 文字。送るべきシグナルがあれば返す (Err は入力がいっぱいでたまらなかった)
    fn input(&mut self, mut c: u8) -> Result<Option<i32>, ()> {
        let cc = self.t.cc;
        if self.lnext {
            self.lnext = false;
            return self.put(c).map(|_| None);
        }
        if self.t.iflag & ISTRIP != 0 {
            c &= 0x7f;
        }
        if c == b'\r' {
            if self.t.iflag & IGNCR != 0 {
                return Ok(None);
            }
            if self.t.iflag & ICRNL != 0 {
                c = b'\n';
            }
        } else if c == b'\n' && self.t.iflag & INLCR != 0 {
            c = b'\r';
        }
        if self.lflag(ISIG) && c != 0 {
            let sig = if c == cc[VINTR] {
                Some(signal::SIGINT)
            } else if c == cc[VQUIT] {
                Some(signal::SIGQUIT)
            } else if c == cc[VSUSP] {
                Some(SIGTSTP)
            } else {
                None
            };
            if let Some(sig) = sig {
                if !self.lflag(NOFLSH) {
                    self.flush_input();
                }
                self.echo(c);
                return Ok(Some(sig));
            }
        }
        if !self.canon() {
            return self.put(c).map(|_| None);
        }
        if self.lflag(IEXTEN) && c != 0 && c == cc[VLNEXT] {
            self.lnext = true;
            return Ok(None);
        }
        if c != 0 && (c == cc[VERASE] || c == 0x08) {
            self.erase_one();
        } else if c != 0 && c == cc[VKILL] {
            while self.erase_one() {}
            if self.lflag(ECHO) && !self.lflag(ECHOE) && self.lflag(ECHOK) {
                self.emit(b"\n");
            }
        } else if self.lflag(IEXTEN) && c != 0 && c == cc[VWERASE] {
            while self.edit.last().is_some_and(|&b| b == b' ' || b == b'\t') {
                self.erase_one();
            }
            while self.edit.last().is_some_and(|&b| b != b' ' && b != b'\t') {
                self.erase_one();
            }
        } else if c != 0 && c == cc[VEOF] {
            self.commit(Some(EOF_MARK));
            self.wake();
        } else if self.is_eol(c) {
            self.edit.push(c);
            self.commit(None);
            self.echo(c);
            self.wake();
        } else {
            // 1 行にはいりきらない分は捨てる (最後の改行のぶんは空けておく)
            if self.inq.len() + self.edit.len() + 1 < INQ {
                self.edit.push(c);
                self.echo(c);
            }
        }
        Ok(None)
    }

    /// ノンカノニカル (と Ctrl-V) で 1 文字入れる
    fn put(&mut self, c: u8) -> Result<(), ()> {
        if self.canon() {
            if self.inq.len() + self.edit.len() + 1 < INQ {
                self.edit.push(c);
                self.echo(c);
            }
            return Ok(());
        }
        if self.inq.len() >= INQ {
            return Err(());
        }
        self.inq.push_back(c as u16);
        self.echo(c);
        self.wake();
        Ok(())
    }

    fn wake(&self) {
        proc::wakeup(self.chan(READ));
        proc::wakeup(proc::poll_chan());
    }

    fn chan(&self, what: usize) -> usize {
        self as *const Tty as usize + what
    }

    /// 読める (poll)
    fn readable(&self) -> bool {
        if self.hung_up() {
            return true;
        }
        if self.canon() { self.lines > 0 } else { self.inq.iter().any(|&c| c != EOF_MARK) }
    }

    /// ICANON を切り替えたときに行の数を数えなおす
    fn recount(&mut self) {
        if self.canon() {
            let eol = |c: u16| c == EOF_MARK || c < 0x100 && self.is_eol(c as u8);
            self.lines = self.inq.iter().filter(|&&c| eol(c)).count();
        } else {
            // 編集中だったものはそのまま読めるように
            let e: Vec<u8> = self.edit.drain(..).collect();
            self.inq.extend(e.into_iter().map(u16::from));
            self.lines = 0;
        }
    }
}

// sleep に使うチャンネル (Tty のアドレス + これ)
const READ: usize = 0; // 子 (とコンソール) が入力を待つ
const MREAD: usize = 1; // 親が出力を待つ
const SWRITE: usize = 2; // 子が親の読むのを待つ
const MWRITE: usize = 3; // 親が子の読むのを待つ

static mut CONSOLE: Option<TtyRef> = None;
static mut PTYS: Vec<Option<TtyRef>> = Vec::new();

pub fn console() -> TtyRef {
    unsafe {
        let c = &mut *(&raw mut CONSOLE);
        c.get_or_insert_with(|| Rc::new(RefCell::new(Tty::new(Dev::Console)))).clone()
    }
}

fn ptys() -> &'static mut Vec<Option<TtyRef>> {
    unsafe { &mut *(&raw mut PTYS) }
}

/// 使える端末ぜんぶ
fn all() -> Vec<TtyRef> {
    let mut v = alloc::vec![console()];
    v.extend(ptys().iter().flatten().cloned());
    v
}

/// シグナルは借用を返してから送る
fn send(pgrp: u32, sig: i32) {
    if pgrp != 0 {
        signal::send_pgrp(pgrp, sig, SigInfo { code: signal::SI_KERNEL, ..SigInfo::ZERO });
    }
}

/// UART の受信割り込みから
pub fn console_input(c: u8) {
    let tty = console();
    let r = tty.borrow_mut().input(c);
    if let Ok(Some(sig)) = r {
        let pg = tty.borrow().pgrp;
        send(pg, sig);
    }
}

/// 子の口 (とコンソール) から読む
pub fn read(tty: &TtyRef, dst: &mut [u8], nonblock: bool) -> Result<usize, i64> {
    if dst.is_empty() {
        return Ok(0);
    }
    let mut deadline = 0u64;
    loop {
        {
            let mut t = tty.borrow_mut();
            if t.canon() {
                if t.lines > 0 {
                    let mut n = 0;
                    while n < dst.len() {
                        let Some(c) = t.inq.pop_front() else { break };
                        if c == EOF_MARK {
                            t.lines -= 1;
                            break;
                        }
                        let c = c as u8;
                        dst[n] = c;
                        n += 1;
                        if t.is_eol(c) {
                            t.lines -= 1;
                            break;
                        }
                    }
                    t.wake_writers();
                    return Ok(n);
                }
            } else {
                let vmin = t.t.cc[VMIN] as usize;
                let vtime = t.t.cc[VTIME] as u64;
                let have = t.inq.iter().filter(|&&c| c != EOF_MARK).count();
                let timed_out = deadline != 0 && crate::timer::ticks() >= deadline;
                if have > 0 && (have >= vmin.min(dst.len()) || timed_out) || vmin == 0 && (vtime == 0 || timed_out) {
                    let mut n = 0;
                    while n < dst.len() {
                        match t.inq.pop_front() {
                            Some(EOF_MARK) => continue,
                            Some(c) => {
                                dst[n] = c as u8;
                                n += 1;
                            }
                            None => break,
                        }
                    }
                    t.wake_writers();
                    return Ok(n);
                }
                // VTIME は 0.1 秒単位 (tick は 10ms)。VMIN > 0 なら 1 文字来てからの時間
                if vtime > 0 && deadline == 0 && (vmin == 0 || have > 0) {
                    deadline = crate::timer::ticks() + vtime * 10;
                }
            }
            if t.hung_up() {
                return Ok(0);
            }
            if nonblock {
                return Err(-EAGAIN);
            }
        }
        let chan = tty.borrow().chan(READ);
        if deadline != 0 {
            proc::sleep_until(chan, deadline)?;
        } else {
            proc::sleep(chan)?;
        }
    }
}

impl Tty {
    /// 入力を読んだので、親の書き込みを待っているものを起こす
    fn wake_writers(&self) {
        if let Dev::Pty(_) = self.dev {
            proc::wakeup(self.chan(MWRITE));
            proc::wakeup(proc::poll_chan());
        }
    }
}

/// 子の口 (とコンソール) へ書く
pub fn write(tty: &TtyRef, src: &[u8], nonblock: bool) -> Result<usize, i64> {
    let mut done = 0;
    loop {
        {
            let mut t = tty.borrow_mut();
            let room = match &t.dev {
                Dev::Console => usize::MAX,
                Dev::Pty(p) if !p.master => return Err(-EIO),
                Dev::Pty(p) => OUTQ.saturating_sub(p.out.len()),
            };
            if room > 0 {
                let n = (src.len() - done).min(room);
                t.emit(&src[done..done + n]);
                done += n;
                if let Dev::Pty(_) = t.dev {
                    proc::wakeup(t.chan(MREAD));
                    proc::wakeup(proc::poll_chan());
                }
            }
            if done == src.len() {
                return Ok(done);
            }
            if nonblock {
                return if done > 0 { Ok(done) } else { Err(-EAGAIN) };
            }
        }
        let chan = tty.borrow().chan(SWRITE);
        if let Err(e) = proc::sleep(chan) {
            return if done > 0 { Ok(done) } else { Err(e) };
        }
    }
}

/// 親の口から読む (子が書いたもの)
pub fn master_read(tty: &TtyRef, dst: &mut [u8], nonblock: bool) -> Result<usize, i64> {
    loop {
        {
            let mut t = tty.borrow_mut();
            let chan = t.chan(SWRITE);
            let p = t.pty().unwrap();
            if !p.out.is_empty() {
                let n = dst.len().min(p.out.len());
                for (d, c) in dst.iter_mut().zip(p.out.drain(..n)) {
                    *d = c;
                }
                proc::wakeup(chan);
                proc::wakeup(proc::poll_chan());
                return Ok(n);
            }
            if p.slaves == 0 {
                return Err(-EIO);
            }
            if nonblock {
                return Err(-EAGAIN);
            }
        }
        let chan = tty.borrow().chan(MREAD);
        proc::sleep(chan)?;
    }
}

/// 親の口へ書く (子への入力になる)
pub fn master_write(tty: &TtyRef, src: &[u8], nonblock: bool) -> Result<usize, i64> {
    let mut done = 0;
    loop {
        while done < src.len() {
            let r = tty.borrow_mut().input(src[done]);
            match r {
                Ok(sig) => {
                    done += 1;
                    if let Some(sig) = sig {
                        let pg = tty.borrow().pgrp;
                        send(pg, sig);
                    }
                }
                Err(()) => break,
            }
        }
        {
            // 子のエコーが親の読むものに入ったかもしれない
            let t = tty.borrow();
            proc::wakeup(t.chan(MREAD));
            proc::wakeup(proc::poll_chan());
        }
        if done == src.len() {
            return Ok(done);
        }
        if nonblock {
            return if done > 0 { Ok(done) } else { Err(-EAGAIN) };
        }
        let chan = tty.borrow().chan(MWRITE);
        if let Err(e) = proc::sleep(chan) {
            return if done > 0 { Ok(done) } else { Err(e) };
        }
    }
}

/// poll: (読める, 書ける, 切れた)
pub fn readiness(tty: &TtyRef) -> (bool, bool, bool) {
    let t = tty.borrow();
    match &t.dev {
        Dev::Console => (t.readable(), true, false),
        Dev::Pty(p) => (t.readable(), !p.master || p.out.len() < OUTQ, !p.master),
    }
}

pub fn master_readiness(tty: &TtyRef) -> (bool, bool, bool) {
    let t = tty.borrow();
    let Dev::Pty(p) = &t.dev else { return (false, false, false) };
    let room = t.canon() || t.inq.len() < INQ;
    (!p.out.is_empty() || p.slaves == 0, room, p.slaves == 0 && p.out.is_empty())
}

// ---- 疑似端末を作る・閉じる ----

/// /dev/ptmx を開いた: 新しい組を作る
pub fn open_ptmx() -> Result<TtyRef, i64> {
    let list = ptys();
    let index = match list.iter().position(|p| p.is_none()) {
        Some(i) => i,
        None => {
            if list.len() >= 256 {
                return Err(-28); // ENOSPC
            }
            list.push(None);
            list.len() - 1
        }
    };
    let pty = Pty { index, master: true, slaves: 0, locked: true, out: VecDeque::new() };
    let tty = Rc::new(RefCell::new(Tty::new(Dev::Pty(pty))));
    list[index] = Some(tty.clone());
    // /dev/pts/N を作り、開いた人のものにする
    let c = proc::current_cred();
    if let Ok(dir) = crate::vfs::resolve("", "dev/pts", true) {
        let _ = dir.unlink(&alloc::format!("{}", index), false);
        if let Ok(node) = dir.create(&alloc::format!("{}", index), 0o620, crate::vfs::NewNode::Dev(136, index as u32)) {
            let _ = node.set_owner(Some(c.uid), Some(5)); // tty グループ
        }
    }
    Ok(tty)
}

/// /dev/pts/N を開いた
pub fn open_slave(index: usize) -> Result<TtyRef, i64> {
    let tty = ptys().get(index).cloned().flatten().ok_or(-EIO)?;
    {
        let mut t = tty.borrow_mut();
        let p = t.pty().unwrap();
        if p.locked || !p.master {
            return Err(-EIO);
        }
        p.slaves += 1;
    }
    Ok(tty)
}

/// 子の口を閉じた (最後の OpenFile が消えた)
pub fn close_slave(tty: &TtyRef) {
    let mut t = tty.borrow_mut();
    let chan = t.chan(MREAD);
    if let Some(p) = t.pty() {
        p.slaves -= 1;
    }
    proc::wakeup(chan);
    proc::wakeup(proc::poll_chan());
}

/// 親の口を閉じた: 子の側はハングアップ
pub fn close_master(tty: &TtyRef) {
    let (index, pgrp, session) = {
        let mut t = tty.borrow_mut();
        let (pgrp, session) = (t.pgrp, t.session);
        t.session = 0;
        t.pgrp = 0;
        let p = t.pty().unwrap();
        p.master = false;
        let index = p.index;
        t.wake();
        proc::wakeup(t.chan(SWRITE));
        (index, pgrp, session)
    };
    if let Some(slot) = ptys().get_mut(index) {
        *slot = None;
    }
    if let Ok(dir) = crate::vfs::resolve("", "dev/pts", true) {
        let _ = dir.unlink(&alloc::format!("{}", index), false);
    }
    if session != 0 {
        send(pgrp, SIGHUP);
        send(pgrp, signal::SIGCONT);
        if let Some(l) = proc::find_leader(session) {
            if l.pgid != pgrp {
                send(l.pgid, SIGHUP);
            }
        }
    }
}

// ---- 制御端末 ----

/// そのセッションがまだ生きているか
fn session_alive(sid: u32) -> bool {
    sid != 0 && proc::all_leader_procs().iter().any(|p| p.sid == sid)
}

/// 今のプロセスの制御端末
pub fn controlling() -> Option<TtyRef> {
    let sid = proc::current().sid;
    all().into_iter().find(|t| t.borrow().session == sid)
}

/// O_NOCTTY なしで端末を開いた: セッションリーダーで制御端末がなければそれにする
pub fn maybe_acquire(tty: &TtyRef) {
    let me = proc::current();
    if me.sid != me.tgid || controlling().is_some() {
        return;
    }
    let mut t = tty.borrow_mut();
    if t.hung_up() || session_alive(t.session) {
        return;
    }
    t.session = me.sid;
    t.pgrp = me.pgid;
}

// ---- ioctl ----

const TCGETS: u64 = 0x5401;
const TCSETS: u64 = 0x5402;
const TCSETSW: u64 = 0x5403;
const TCSETSF: u64 = 0x5404;
const TCSBRK: u64 = 0x5409;
const TCXONC: u64 = 0x540a;
const TCFLSH: u64 = 0x540b;
const TIOCEXCL: u64 = 0x540c;
const TIOCNXCL: u64 = 0x540d;
const TIOCSCTTY: u64 = 0x540e;
const TIOCGPGRP: u64 = 0x540f;
const TIOCSPGRP: u64 = 0x5410;
const TIOCOUTQ: u64 = 0x5411;
const TIOCGWINSZ: u64 = 0x5413;
const TIOCSWINSZ: u64 = 0x5414;
const FIONREAD: u64 = 0x541b;
const TIOCNOTTY: u64 = 0x5422;
const TIOCGSID: u64 = 0x5429;
const TIOCGPTN: u64 = 0x80045430;
const TIOCSPTLCK: u64 = 0x40045431;
const TIOCSIG: u64 = 0x40045436;

fn out(va: usize, b: &[u8]) -> Result<(), i64> {
    proc::current().pt().copy_out(va, b).ok_or(-EFAULT)
}

fn inp<const N: usize>(va: usize) -> Result<[u8; N], i64> {
    let mut b = [0u8; N];
    proc::current().pt().copy_in(&mut b, va).ok_or(-EFAULT)?;
    Ok(b)
}

fn in_u32(va: usize) -> Result<u32, i64> {
    Ok(u32::from_le_bytes(inp::<4>(va)?))
}

pub fn ioctl(tty: &TtyRef, master: bool, req: u64, arg: usize) -> Result<i64, i64> {
    match req {
        TCGETS => {
            let b = tty.borrow().t.to_bytes();
            out(arg, &b)?;
        }
        TCSETS | TCSETSW | TCSETSF => {
            let t = Termios::from_bytes(&inp::<36>(arg)?);
            let mut tt = tty.borrow_mut();
            if req == TCSETSF {
                tt.flush_input();
            }
            let was = tt.canon();
            tt.t = t;
            if was != tt.canon() {
                tt.recount();
            }
            tt.wake();
        }
        TCSBRK | TCXONC | TIOCEXCL | TIOCNXCL => {}
        TCFLSH => {
            let mut t = tty.borrow_mut();
            let (inq, outq) = match arg {
                0 => (true, false),
                1 => (false, true),
                2 => (true, true),
                _ => return Err(-EINVAL),
            };
            // 親から見ると入出力が逆
            let (inq, outq) = if master { (outq, inq) } else { (inq, outq) };
            if inq {
                t.flush_input();
                t.wake_writers();
            }
            if outq {
                let chan = t.chan(SWRITE);
                if let Some(p) = t.pty() {
                    p.out.clear();
                    proc::wakeup(chan);
                }
            }
        }
        TIOCSCTTY => {
            let me = proc::current();
            let current = controlling_of(me.sid);
            if current.as_ref().is_some_and(|c| Rc::ptr_eq(c, tty)) {
                return Ok(0);
            }
            if me.sid != me.tgid || current.is_some() {
                return Err(-EPERM);
            }
            let mut t = tty.borrow_mut();
            if session_alive(t.session) && !(arg == 1 && proc::current_cred().euid == 0) {
                return Err(-EPERM);
            }
            t.session = me.sid;
            t.pgrp = me.pgid;
        }
        TIOCNOTTY => {
            let me = proc::current();
            let (pgrp, leader) = {
                let mut t = tty.borrow_mut();
                if t.session != me.sid {
                    return Err(-ENOTTY);
                }
                let leader = me.sid == me.tgid;
                let pgrp = t.pgrp;
                if leader {
                    t.session = 0;
                    t.pgrp = 0;
                }
                (pgrp, leader)
            };
            if leader {
                send(pgrp, SIGHUP);
                send(pgrp, signal::SIGCONT);
            }
        }
        TIOCGPGRP => {
            let pg = tty.borrow().pgrp;
            let pg = if pg == 0 { proc::current().pgid } else { pg };
            out(arg, &(pg as i32).to_le_bytes())?;
        }
        TIOCSPGRP => {
            let pg = in_u32(arg)?;
            if pg == 0 || proc::leaders_in_pgrp(pg).is_empty() {
                return Err(-EPERM);
            }
            tty.borrow_mut().pgrp = pg;
        }
        TIOCGSID => {
            let sid = tty.borrow().session;
            if sid == 0 {
                return Err(-ENOTTY);
            }
            out(arg, &(sid as i32).to_le_bytes())?;
        }
        TIOCGWINSZ => {
            let ws = tty.borrow().winsize;
            out(arg, &ws)?;
        }
        TIOCSWINSZ => {
            let ws = inp::<8>(arg)?;
            let (changed, pg) = {
                let mut t = tty.borrow_mut();
                let changed = t.winsize != ws;
                t.winsize = ws;
                (changed, t.pgrp)
            };
            if changed {
                send(pg, signal::SIGWINCH);
            }
        }
        FIONREAD | TIOCOUTQ => {
            let mut t = tty.borrow_mut();
            let queued = t.inq.iter().filter(|&&c| c != EOF_MARK).count();
            let pending = t.pty().map_or(0, |p| p.out.len());
            // 親から見ると FIONREAD は子の出力、子から見ると入力
            let n = match (req == FIONREAD, master) {
                (true, false) | (false, true) => queued,
                _ => pending,
            };
            out(arg, &(n as i32).to_le_bytes())?;
        }
        TIOCGPTN if master => {
            let i = tty.borrow_mut().pty().unwrap().index as u32;
            out(arg, &i.to_le_bytes())?;
        }
        TIOCSPTLCK if master => {
            let v = in_u32(arg)?;
            tty.borrow_mut().pty().unwrap().locked = v != 0;
        }
        TIOCSIG if master => {
            let sig = arg as i32;
            if !(1..64).contains(&sig) {
                return Err(-EINVAL);
            }
            let pg = tty.borrow().pgrp;
            send(pg, sig);
        }
        _ => return Err(-ENOTTY),
    }
    Ok(0)
}

/// セッションの制御端末の (番号, 前にいるグループ) (/proc/PID/stat 用)
pub fn of_session(sid: u32) -> Option<(u64, u32)> {
    let t = controlling_of(sid)?;
    let r = rdev(&t, false);
    let pg = t.borrow().pgrp;
    Some((r, pg))
}

fn controlling_of(sid: u32) -> Option<TtyRef> {
    all().into_iter().find(|t| t.borrow().session == sid)
}

/// 開いた口の名前 (/proc/PID/fd/N)
pub fn name(tty: &TtyRef, master: bool) -> alloc::string::String {
    match &tty.borrow().dev {
        Dev::Console => "/dev/console".into(),
        Dev::Pty(_) if master => "/dev/ptmx".into(),
        Dev::Pty(p) => alloc::format!("/dev/pts/{}", p.index),
    }
}

/// 端末のデバイス番号 (stat 用)。疑似端末の子は Linux と同じく major 136
pub fn rdev(tty: &TtyRef, master: bool) -> u64 {
    match &tty.borrow().dev {
        Dev::Console => (5 << 8) | 1,
        Dev::Pty(_) if master => (5 << 8) | 2,
        Dev::Pty(p) => (136 << 8) | p.index as u64,
    }
}

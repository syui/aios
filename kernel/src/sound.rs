// virtio-sound: 音を出す。Linux の ALSA と同じ形 (alsa-lib がそのまま使える) で見せる
//   /dev/snd/controlC0 (char 116, 0): カードの情報
//   /dev/snd/pcmC0D0p  (char 116, 16): 再生の口。ioctl で形 (hw_params) を決めて、WRITEI_FRAMES で書く
//     (mmap でリングバッファに直に書いて SYNC_PTR で知らせてもよい)
//
// リングバッファはカーネルのページの集まり。書かれたところから 1 ページずつ (ヘッダー + 音 + 返事)
// デバイスの txq に渡す。デバイスは鳴らし終わったものを返してくるので (割り込み)、そこまでを
// 「鳴った」(hw_ptr) として進め、空いたところへ書く人を起こす。
// 形の決め方 (HW_REFINE / HW_PARAMS) は Linux のカーネルと同じく、範囲 (interval) を互いの関係で
// 狭めていく。デバイスが言う形のうち、S16 / S32 / FLOAT のリトルエンディアン、1 か 2 チャンネルだけ使う
// (ほかの形やレートは alsa-lib の plug がかえる)。録音 (rxq) はまだ
use crate::kalloc;
use crate::memlayout::{v2p, PGSIZE};
use crate::proc;
use crate::virtio::{self, Mmio, Queue, DESC_NEXT, DESC_WRITE};
use alloc::rc::Rc;
use alloc::vec::Vec;

const DEVICE_SND: u32 = 25;
const CTRLQ: u32 = 0;
const TXQ: u32 = 2;

// 制御の要求 (virtio_snd_hdr の code) と返事
const R_PCM_INFO: u32 = 0x0100;
const R_PCM_SET_PARAMS: u32 = 0x0101;
const R_PCM_PREPARE: u32 = 0x0102;
const R_PCM_RELEASE: u32 = 0x0103;
const R_PCM_START: u32 = 0x0104;
const R_PCM_STOP: u32 = 0x0105;
const S_OK: u32 = 0x8000;
const D_OUTPUT: u8 = 0;

const EFAULT: i64 = 14;
const EBUSY: i64 = 16;
const ENODEV: i64 = 19;
const EINVAL: i64 = 22;
const ENOTTY: i64 = 25;
const EAGAIN: i64 = 11;
const ENOMEM: i64 = 12;
const EPIPE: i64 = 32;
const EBADFD: i64 = 77;
const ENXIO: i64 = 6;

// ALSA の PCM の状態
const ST_OPEN: i32 = 0;
const ST_SETUP: i32 = 1;
const ST_PREPARED: i32 = 2;
const ST_RUNNING: i32 = 3;
const ST_XRUN: i32 = 4;
const ST_DRAINING: i32 = 5;
const ST_PAUSED: i32 = 6;

// hw_params の番号
const P_ACCESS: usize = 0;
const P_FORMAT: usize = 1;
const P_SUBFORMAT: usize = 2;
const P_SAMPLE_BITS: usize = 8;
const P_FRAME_BITS: usize = 9;
const P_CHANNELS: usize = 10;
const P_RATE: usize = 11;
const P_PERIOD_TIME: usize = 12;
const P_PERIOD_SIZE: usize = 13;
const P_PERIOD_BYTES: usize = 14;
const P_PERIODS: usize = 15;
const P_BUFFER_TIME: usize = 16;
const P_BUFFER_SIZE: usize = 17;
const P_BUFFER_BYTES: usize = 18;
const P_TICK_TIME: usize = 19;

// アクセスの形
const ACCESS_MMAP_INTERLEAVED: u32 = 0;
const ACCESS_RW_INTERLEAVED: u32 = 3;
// 音の形: (ALSA の番号, virtio の番号, ビット数)
const FORMATS: [(u32, u8, u32); 3] = [(2, 5, 16), (10, 17, 32), (14, 19, 32)]; // S16_LE, S32_LE, FLOAT_LE
// virtio のレートの番号 → Hz
const RATES: [u32; 14] = [5512, 8000, 11025, 16000, 22050, 32000, 44100, 48000, 64000, 88200, 96000, 176400, 192000, 384000];

// hw_params の info
const INFO_MMAP: u32 = 0x1;
const INFO_MMAP_VALID: u32 = 0x2;
const INFO_INTERLEAVED: u32 = 0x100;
const INFO_BLOCK_TRANSFER: u32 = 0x10000;
const INFO_PAUSE: u32 = 0x80000;

/// リングバッファの最大 (バイト)
const MAX_BUFFER: u32 = 512 * 1024;
/// デバイスへ一度に渡す大きさ (1 ページ)
const CHUNK: usize = PGSIZE;

/// 範囲 [min, max] (端を含まない印つき)。Linux の struct snd_interval
#[derive(Clone, Copy, PartialEq)]
struct Iv {
    min: u32,
    max: u32,
    openmin: bool,
    openmax: bool,
    integer: bool,
    empty: bool,
}

impl Iv {
    fn read(b: &[u8]) -> Iv {
        let u = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        let f = u(8);
        Iv { min: u(0), max: u(4), openmin: f & 1 != 0, openmax: f & 2 != 0, integer: f & 4 != 0, empty: f & 8 != 0 }
    }

    fn write(&self, b: &mut [u8]) {
        b[0..4].copy_from_slice(&self.min.to_le_bytes());
        b[4..8].copy_from_slice(&self.max.to_le_bytes());
        let f = self.openmin as u32 | (self.openmax as u32) << 1 | (self.integer as u32) << 2 | (self.empty as u32) << 3;
        b[8..12].copy_from_slice(&f.to_le_bytes());
    }

    fn of(min: u64, max: u64) -> Iv {
        Iv { min: min.min(u32::MAX as u64) as u32, max: max.min(u32::MAX as u64) as u32, openmin: false, openmax: false, integer: false, empty: min > max }
    }

    /// 整数の範囲 (端を含む) に直す。整数の値 (integer) なら端を含まない印のぶん内へ。
    /// そうでない値 (周期の数、時間) は (3, 4) のように整数をはさまないこともあるので、外側の整数にする
    fn lo(&self) -> u64 {
        self.min as u64 + (self.integer && self.openmin && self.min < u32::MAX) as u64
    }

    fn hi(&self) -> u64 {
        (self.max as u64).saturating_sub((self.integer && self.openmax && self.max > 0) as u64)
    }

    fn single(&self) -> bool {
        !self.empty && self.lo() == self.hi()
    }

    /// v と重なるところだけにする。変わったら true
    fn refine(&mut self, v: Iv) -> bool {
        if self.empty {
            return false;
        }
        let old = *self;
        let (lo, hi) = (self.lo().max(v.lo()), self.hi().min(v.hi()));
        if lo > hi || v.empty {
            self.empty = true;
        } else {
            // 端が変わらなければ、端を含まない印もそのまま
            let openmin = lo == old.min as u64 && old.openmin;
            let openmax = hi == old.max as u64 && old.openmax;
            *self = Iv { min: lo as u32, max: hi as u32, openmin, openmax, integer: self.integer || v.integer, empty: false };
        }
        *self != old
    }
}

/// a * b / k (範囲を外さないように、下は切り捨て、上は切り上げ)
fn mulk(a: Iv, b: Iv, k: u64) -> Iv {
    if a.empty || b.empty {
        return Iv { empty: true, ..a };
    }
    Iv::of(a.lo() * b.lo() / k, (a.hi() * b.hi()).div_ceil(k))
}

/// a * k / b
fn kdiv(a: Iv, k: u64, b: Iv) -> Iv {
    if a.empty || b.empty {
        return Iv { empty: true, ..a };
    }
    let lo = if b.hi() == 0 { u32::MAX as u64 } else { a.lo() * k / b.hi() };
    let hi = if b.lo() == 0 { u32::MAX as u64 } else { (a.hi() * k).div_ceil(b.lo()) };
    Iv::of(lo, hi)
}

/// hw_params (608 バイト) の読み書き
struct Hw {
    b: [u8; 608],
}

impl Hw {
    fn mask(&self, p: usize) -> u32 {
        // 使う形はどれも 32 番より下なので、最初の u32 だけ見る
        let o = 4 + p * 32;
        u32::from_le_bytes(self.b[o..o + 4].try_into().unwrap())
    }

    fn set_mask(&mut self, p: usize, m: u32) {
        let o = 4 + p * 32;
        self.b[o..o + 4].copy_from_slice(&m.to_le_bytes());
        self.b[o + 4..o + 32].fill(0);
    }

    fn iv(&self, p: usize) -> Iv {
        let o = 260 + (p - P_SAMPLE_BITS) * 12;
        Iv::read(&self.b[o..o + 12])
    }

    fn set_iv(&mut self, p: usize, v: Iv) {
        let o = 260 + (p - P_SAMPLE_BITS) * 12;
        v.write(&mut self.b[o..o + 12]);
    }

    /// p を v と重ねる。変わったら true
    fn narrow(&mut self, p: usize, v: Iv) -> bool {
        let mut cur = self.iv(p);
        let ch = cur.refine(v);
        self.set_iv(p, cur);
        ch
    }

    fn u32_at(&self, o: usize) -> u32 {
        u32::from_le_bytes(self.b[o..o + 4].try_into().unwrap())
    }

    fn put_u32(&mut self, o: usize, v: u32) {
        self.b[o..o + 4].copy_from_slice(&v.to_le_bytes());
    }

    fn empty(&self) -> bool {
        [P_ACCESS, P_FORMAT, P_SUBFORMAT].iter().any(|&p| self.mask(p) == 0) || (P_SAMPLE_BITS..=P_TICK_TIME).any(|p| self.iv(p).empty)
    }
}

/// 開いている再生の口
struct Pcm {
    state: i32,
    channels: u32,
    frame_bytes: usize,
    buffer_frames: u64,
    /// リングバッファ (mmap でも見せる)
    pages: Rc<Vec<*mut u8>>,
    hw_ptr: u64,
    appl_ptr: u64,
    /// デバイスへ渡したところまで
    sent_ptr: u64,
    boundary: u64,
    avail_min: u64,
    start_threshold: u64,
    stop_threshold: u64,
    /// 渡している塊の (先頭の記述子, フレーム数)。返ってくる順に
    inflight: alloc::collections::VecDeque<(u16, u64)>,
    /// 空いている記述子の組 (3 つずつ) の番号
    free_slots: Vec<u16>,
    trigger_ns: u64,
}

struct Snd {
    mmio: Mmio,
    ctrl: Queue,
    tx: Queue,
    /// 制御の要求と返事に使うページ
    req: *mut u8,
    resp: *mut u8,
    /// txq の塊ごとのヘッダー (4 バイト) と返事 (8 バイト) を置くページ
    hdrs: *mut u8,
    stream: u32,
    formats: u64,
    rates: u64,
    channels_min: u32,
    channels_max: u32,
    pcm: Option<Pcm>,
    /// 再生の口が開いているか (ひとりだけ)
    opened: bool,
}

static mut SND: Option<Snd> = None;

fn get() -> Option<&'static mut Snd> {
    unsafe { (*(&raw mut SND)).as_mut() }
}

pub fn present() -> bool {
    get().is_some()
}

/// 書く人が眠る channel (と poll の印)
pub fn chan() -> usize {
    (&raw const SND) as usize
}

impl Snd {
    /// 制御の要求を送って、返事 (resp_len バイト) の状態を返す
    fn cmd(&mut self, req: &[u8], resp_len: usize) -> u32 {
        unsafe {
            core::ptr::copy_nonoverlapping(req.as_ptr(), self.req, req.len());
            core::ptr::write_bytes(self.resp, 0, resp_len);
        }
        *self.ctrl.desc(0) = virtio::Desc { addr: v2p(self.req as usize) as u64, len: req.len() as u32, flags: DESC_NEXT, next: 1 };
        *self.ctrl.desc(1) = virtio::Desc { addr: v2p(self.resp as usize) as u64, len: resp_len as u32, flags: DESC_WRITE, next: 0 };
        self.ctrl.push(0);
        self.ctrl.notify(&self.mmio);
        while self.ctrl.pop_used().is_none() {
            core::hint::spin_loop();
        }
        u32::from_le_bytes(unsafe { core::slice::from_raw_parts(self.resp, 4) }.try_into().unwrap())
    }

    fn simple(&mut self, code: u32) -> u32 {
        let mut r = [0u8; 8];
        r[0..4].copy_from_slice(&code.to_le_bytes());
        r[4..8].copy_from_slice(&self.stream.to_le_bytes());
        self.cmd(&r, 4)
    }

    /// まだ渡していない書かれた音を、空いている記述子のあるだけデバイスへ渡す
    fn send(&mut self) {
        let Some(p) = self.pcm.as_mut() else { return };
        if !matches!(p.state, ST_RUNNING | ST_DRAINING | ST_PREPARED) {
            return;
        }
        let bytes_total = p.buffer_frames as usize * p.frame_bytes;
        let mut sent = false;
        while p.sent_ptr < p.appl_ptr {
            let Some(slot) = p.free_slots.pop() else { break };
            let pos = (p.sent_ptr % p.buffer_frames) as usize * p.frame_bytes;
            // ページの終わりかバッファの終わりか、書かれたところまで
            let left = (p.appl_ptr - p.sent_ptr) as usize * p.frame_bytes;
            let n = left.min(CHUNK - pos % PGSIZE).min(bytes_total - pos);
            let frames = (n / p.frame_bytes) as u64;
            let d = slot as usize * 3;
            let hdr = unsafe { self.hdrs.add(slot as usize * 16) };
            unsafe { core::ptr::copy_nonoverlapping(self.stream.to_le_bytes().as_ptr(), hdr, 4) };
            let data = unsafe { p.pages[pos / PGSIZE].add(pos % PGSIZE) };
            *self.tx.desc(d) = virtio::Desc { addr: v2p(hdr as usize) as u64, len: 4, flags: DESC_NEXT, next: (d + 1) as u16 };
            *self.tx.desc(d + 1) = virtio::Desc { addr: v2p(data as usize) as u64, len: n as u32, flags: DESC_NEXT, next: (d + 2) as u16 };
            *self.tx.desc(d + 2) = virtio::Desc { addr: v2p(hdr as usize + 8) as u64, len: 8, flags: DESC_WRITE, next: 0 };
            self.tx.push(d as u16);
            p.inflight.push_back((d as u16, frames));
            p.sent_ptr += frames;
            sent = true;
        }
        if sent {
            self.tx.notify(&self.mmio);
        }
    }

    /// 返ってきた塊の分だけ hw_ptr を進める。空になったら止める (XRUN か、DRAIN の終わり)
    fn reap(&mut self) -> bool {
        let mut got = false;
        while let Some((head, _)) = self.tx.pop_used() {
            let Some(p) = self.pcm.as_mut() else { continue };
            if let Some(i) = p.inflight.iter().position(|&(h, _)| h == head) {
                let (_, frames) = p.inflight.remove(i).unwrap();
                p.hw_ptr += frames;
                p.free_slots.push(head / 3);
                got = true;
            }
        }
        let Some(p) = self.pcm.as_mut() else { return got };
        if got && p.state == ST_RUNNING && p.inflight.is_empty() && p.hw_ptr >= p.appl_ptr && p.stop_threshold < p.boundary {
            p.state = ST_XRUN;
            self.simple(R_PCM_STOP);
        } else if p.state == ST_DRAINING && p.inflight.is_empty() && p.hw_ptr >= p.appl_ptr {
            p.state = ST_SETUP;
            self.simple(R_PCM_STOP);
        }
        got
    }
}

fn byte(info: &[u8], o: usize) -> u8 {
    info[o]
}

/// 見つかったら用意して true
pub fn init() -> bool {
    let Some((mmio, _)) = virtio::probe(DEVICE_SND, 0) else { return false };
    let Some(ctrl) = Queue::new(&mmio, CTRLQ, 64) else { return false };
    let Some(_ev) = Queue::new(&mmio, 1, 64) else { return false };
    let tx = [256, 128, 64].iter().find_map(|&n| Queue::new(&mmio, TXQ, n));
    let Some(tx) = tx else { return false };
    let Some(_rx) = Queue::new(&mmio, 3, 64) else { return false };
    virtio::ready(&mmio);
    let (Some(req), Some(resp), Some(hdrs)) = (kalloc::alloc(), kalloc::alloc(), kalloc::alloc()) else { return false };
    // config: jacks, streams, chmaps (u32)
    let streams = mmio.config64(4) as u32;
    let mut s = Snd { mmio, ctrl, tx, req, resp, hdrs, stream: u32::MAX, formats: 0, rates: 0, channels_min: 0, channels_max: 0, pcm: None, opened: false };
    // PCM_INFO: hdr, start_id, count, size → hdr + virtio_snd_pcm_info (32 バイト) × count
    let count = streams.min(64);
    let mut q = Vec::new();
    for v in [R_PCM_INFO, 0, count, 32] {
        q.extend_from_slice(&v.to_le_bytes());
    }
    if s.cmd(&q, 4 + 32 * count as usize) != S_OK {
        println!("virtio-snd: cannot read the streams");
        return false;
    }
    let all = unsafe { core::slice::from_raw_parts(s.resp.add(4), 32 * count as usize) };
    for (i, info) in all.chunks(32).enumerate() {
        let u64_at = |o: usize| u64::from_le_bytes(info[o..o + 8].try_into().unwrap());
        if byte(info, 24) == D_OUTPUT {
            s.stream = i as u32;
            s.formats = u64_at(8);
            s.rates = u64_at(16);
            s.channels_min = byte(info, 25) as u32;
            s.channels_max = byte(info, 26) as u32;
            break;
        }
    }
    let ok_formats = FORMATS.iter().any(|&(_, v, _)| s.formats & 1 << v != 0);
    if s.stream == u32::MAX || !ok_formats || s.rates == 0 || s.channels_min > 2 {
        println!("virtio-snd: no output stream we can use ({} streams)", streams);
        return false;
    }
    let rates: Vec<u32> = (0..RATES.len()).filter(|&i| s.rates & 1 << i != 0).map(|i| RATES[i]).collect();
    println!("virtio-snd: output stream {}, {}-{} channels, {:?} Hz (/dev/snd/pcmC0D0p)", s.stream, s.channels_min, s.channels_max.min(2), rates);
    crate::irq::enable(s.mmio.irq);
    unsafe { *(&raw mut SND) = Some(s) };
    true
}

/// 割り込みの番号がこれなら受けて true
pub fn intr(irq: u32) -> bool {
    let Some(s) = get() else { return false };
    if s.mmio.irq != irq {
        return false;
    }
    s.mmio.ack();
    if s.reap() {
        s.send();
        proc::wakeup(chan());
        proc::poll_wake(chan());
    }
    true
}

// ---- hw_params ----

/// デバイスにできることと、互いの関係で範囲を狭める (Linux の snd_pcm_hw_refine)
fn refine(s: &Snd, h: &mut Hw) -> Result<(), i64> {
    // アクセス、形、サブフォーマット
    h.set_mask(P_ACCESS, h.mask(P_ACCESS) & (1 << ACCESS_MMAP_INTERLEAVED | 1 << ACCESS_RW_INTERLEAVED));
    let fm = FORMATS.iter().filter(|&&(_, v, _)| s.formats & 1 << v != 0).fold(0, |m, &(a, _, _)| m | 1 << a);
    h.set_mask(P_FORMAT, h.mask(P_FORMAT) & fm);
    h.set_mask(P_SUBFORMAT, h.mask(P_SUBFORMAT) & 1);
    h.narrow(P_CHANNELS, Iv::of(s.channels_min.max(1) as u64, s.channels_max.min(2) as u64));
    h.narrow(P_PERIOD_BYTES, Iv::of(64, MAX_BUFFER as u64 / 2));
    h.narrow(P_BUFFER_BYTES, Iv::of(128, MAX_BUFFER as u64));
    h.narrow(P_PERIODS, Iv::of(2, 1024));
    // 周期の数 (PERIODS) は整数でなくてよい (バッファが周期の整数倍でなくてもよい。Linux と同じ)
    for p in [P_SAMPLE_BITS, P_FRAME_BITS, P_CHANNELS, P_PERIOD_SIZE, P_PERIOD_BYTES, P_BUFFER_SIZE, P_BUFFER_BYTES] {
        let mut v = h.iv(p);
        v.integer = true;
        h.set_iv(p, v);
    }
    let rates: Vec<u32> = (0..RATES.len()).filter(|&i| s.rates & 1 << i != 0).map(|i| RATES[i]).collect();
    // 互いの関係 (変わらなくなるまで)
    for _ in 0..64 {
        let mut ch = false;
        // 形 ↔ サンプルのビット数
        let fmts = h.mask(P_FORMAT);
        let bits: Vec<u32> = FORMATS.iter().filter(|f| fmts & 1 << f.0 != 0).map(|f| f.2).collect();
        let sb = Iv::of(bits.iter().copied().min().unwrap_or(1) as u64, bits.iter().copied().max().unwrap_or(0) as u64);
        ch |= h.narrow(P_SAMPLE_BITS, sb);
        let sbv = h.iv(P_SAMPLE_BITS);
        let keep = FORMATS.iter().filter(|f| fmts & 1 << f.0 != 0 && (f.2 as u64) >= sbv.lo() && (f.2 as u64) <= sbv.hi()).fold(0, |m, f| m | 1 << f.0);
        if keep != fmts {
            h.set_mask(P_FORMAT, keep);
            ch = true;
        }
        // レートはデバイスの言う値のどれか
        let r = h.iv(P_RATE);
        let inr: Vec<u32> = rates.iter().copied().filter(|&x| (x as u64) >= r.lo() && (x as u64) <= r.hi()).collect();
        ch |= h.narrow(P_RATE, Iv::of(inr.first().copied().unwrap_or(1) as u64, inr.last().copied().unwrap_or(0) as u64));
        let g = |h: &Hw, p| h.iv(p);
        // frame_bits = sample_bits * channels
        ch |= h.narrow(P_FRAME_BITS, mulk(g(h, P_SAMPLE_BITS), g(h, P_CHANNELS), 1));
        ch |= h.narrow(P_SAMPLE_BITS, kdiv(g(h, P_FRAME_BITS), 1, g(h, P_CHANNELS)));
        ch |= h.narrow(P_CHANNELS, kdiv(g(h, P_FRAME_BITS), 1, g(h, P_SAMPLE_BITS)));
        // bytes = size * frame_bits / 8
        for (size, bytes) in [(P_PERIOD_SIZE, P_PERIOD_BYTES), (P_BUFFER_SIZE, P_BUFFER_BYTES)] {
            ch |= h.narrow(bytes, mulk(g(h, size), g(h, P_FRAME_BITS), 8));
            ch |= h.narrow(size, kdiv(g(h, bytes), 8, g(h, P_FRAME_BITS)));
            ch |= h.narrow(P_FRAME_BITS, kdiv(g(h, bytes), 8, g(h, size)));
        }
        // size = time * rate / 1000000
        for (size, time) in [(P_PERIOD_SIZE, P_PERIOD_TIME), (P_BUFFER_SIZE, P_BUFFER_TIME)] {
            ch |= h.narrow(size, mulk(g(h, time), g(h, P_RATE), 1_000_000));
            ch |= h.narrow(time, kdiv(g(h, size), 1_000_000, g(h, P_RATE)));
            ch |= h.narrow(P_RATE, kdiv(g(h, size), 1_000_000, g(h, time)));
        }
        // buffer_size = period_size * periods
        ch |= h.narrow(P_BUFFER_SIZE, mulk(g(h, P_PERIOD_SIZE), g(h, P_PERIODS), 1));
        ch |= h.narrow(P_PERIOD_SIZE, kdiv(g(h, P_BUFFER_SIZE), 1, g(h, P_PERIODS)));
        ch |= h.narrow(P_PERIODS, kdiv(g(h, P_BUFFER_SIZE), 1, g(h, P_PERIOD_SIZE)));
        if !ch || h.empty() {
            break;
        }
    }
    if h.empty() {
        return Err(-EINVAL);
    }
    let rmask = h.u32_at(512);
    h.put_u32(516, rmask); // cmask: 見たもの
    h.put_u32(520, INFO_MMAP | INFO_MMAP_VALID | INFO_INTERLEAVED | INFO_BLOCK_TRANSFER | INFO_PAUSE);
    let sb = h.iv(P_SAMPLE_BITS);
    if sb.single() {
        h.put_u32(524, sb.min); // msbits
    }
    let r = h.iv(P_RATE);
    if r.single() {
        h.put_u32(528, r.lo() as u32); // rate_num
        h.put_u32(532, 1); // rate_den
    }
    Ok(())
}

/// ひとつに決める: 小さいほうを選ぶ (バッファの大きさだけは大きいほう)。Linux の snd_pcm_hw_params_choose。
/// 時間とバイト数と周期の数は、決めたフレーム数から決まる (端数があってもよい) ので選ばない
fn choose(s: &Snd, h: &mut Hw) -> Result<(), i64> {
    refine(s, h)?;
    for p in [P_ACCESS, P_FORMAT, P_SUBFORMAT] {
        let m = h.mask(p);
        // RW を好む (mmap を頼まれていなければ)
        let pick = if p == P_ACCESS && m & 1 << ACCESS_RW_INTERLEAVED != 0 { 1 << ACCESS_RW_INTERLEAVED } else { m & m.wrapping_neg() };
        h.set_mask(p, pick);
        refine(s, h)?;
    }
    for p in [P_CHANNELS, P_RATE, P_PERIOD_SIZE, P_BUFFER_SIZE] {
        let v = h.iv(p);
        if v.single() || v.empty {
            continue;
        }
        let x = if p == P_BUFFER_SIZE { v.hi() } else { v.lo() };
        h.narrow(p, Iv::of(x, x));
        refine(s, h)?;
    }
    Ok(())
}

// ---- 開く・閉じる ----

/// 形を決める前 (OPEN) の口
fn idle() -> Pcm {
    Pcm {
        state: ST_OPEN,
        channels: 0,
        frame_bytes: 0,
        buffer_frames: 0,
        pages: Rc::new(Vec::new()),
        hw_ptr: 0,
        appl_ptr: 0,
        sent_ptr: 0,
        boundary: 1,
        avail_min: 1,
        start_threshold: 1,
        stop_threshold: 0,
        inflight: alloc::collections::VecDeque::new(),
        free_slots: Vec::new(),
        trigger_ns: 0,
    }
}

/// /dev/snd/pcmC0D0p を開いた (ひとりだけ)
pub fn open_pcm() -> Result<(), i64> {
    let s = get().ok_or(-ENODEV)?;
    if s.opened {
        return Err(-EBUSY);
    }
    s.opened = true;
    s.pcm = Some(idle());
    Ok(())
}

/// 閉じた: 止めて、リングバッファを返す
pub fn close_pcm() {
    let Some(s) = get() else { return };
    release(s);
    s.opened = false;
    s.pcm = None;
}

fn release(s: &mut Snd) {
    if let Some(state) = s.pcm.as_ref().map(|p| p.state) {
        if matches!(state, ST_RUNNING | ST_DRAINING | ST_PAUSED) {
            s.simple(R_PCM_STOP);
        }
        if state != ST_OPEN {
            s.simple(R_PCM_RELEASE);
        }
        // 返ってくるのを待つ (デバイスはリリースで残りを返す)
        for _ in 0..1_000_000 {
            s.reap();
            if s.pcm.as_ref().is_none_or(|p| p.inflight.is_empty()) {
                break;
            }
            core::hint::spin_loop();
        }
    }
    if let Some(p) = s.pcm.replace(idle()) {
        for &pg in p.pages.iter() {
            kalloc::put(pg);
        }
    }
}

/// mmap: リングバッファのページ (off 0 から)。状態と制御のページ (0x80000000、0x81000000) は無い
/// (alsa-lib は SYNC_PTR の ioctl を使う)
pub fn mmap_pages(off: usize, len: usize) -> Result<Rc<Vec<*mut u8>>, i64> {
    let s = get().ok_or(-ENODEV)?;
    let p = s.pcm.as_ref().ok_or(-ENXIO)?;
    if p.pages.is_empty() || off + len > p.pages.len() * PGSIZE {
        return Err(-ENXIO);
    }
    Ok(p.pages.clone())
}

// ---- 状態 ----

fn avail(p: &Pcm) -> u64 {
    (p.hw_ptr + p.buffer_frames).saturating_sub(p.appl_ptr)
}

/// poll: (読める, 書ける, エラー)
pub fn readiness() -> (bool, bool, bool) {
    let Some(p) = get().and_then(|s| s.pcm.as_ref()) else { return (false, false, true) };
    match p.state {
        ST_PREPARED | ST_RUNNING | ST_PAUSED => (false, avail(p) >= p.avail_min.max(1), false),
        ST_DRAINING => (false, false, false),
        // アンダーラン: 書けるとだけ答える。書くと EPIPE が返り、アプリが snd_pcm_recover (prepare) で立てなおす。
        // POLLERR を返すと、cpal は書かずにエラーを出しつづけて止まったままになる
        ST_XRUN => (false, true, false),
        _ => (false, true, true),
    }
}

fn now_ts() -> [u8; 16] {
    let ns = crate::timer::uptime_ns();
    let mut b = [0u8; 16];
    b[0..8].copy_from_slice(&(ns / 1_000_000_000).to_le_bytes());
    b[8..16].copy_from_slice(&(ns % 1_000_000_000).to_le_bytes());
    b
}

fn start(s: &mut Snd) -> Result<(), i64> {
    let p = s.pcm.as_mut().ok_or(-EBADFD)?;
    if p.state != ST_PREPARED {
        return Err(-EBADFD);
    }
    p.state = ST_RUNNING;
    p.trigger_ns = crate::timer::uptime_ns();
    s.send();
    if s.simple(R_PCM_START) != S_OK {
        return Err(-EINVAL);
    }
    Ok(())
}

/// snd_pcm_info (288 バイト)
fn pcm_info() -> [u8; 288] {
    let mut b = [0u8; 288];
    // device 0, subdevice 0, stream 0 (再生), card 0
    let id = b"virtio-snd";
    b[16..16 + id.len()].copy_from_slice(id);
    let name = b"virtio sound";
    b[80..80 + name.len()].copy_from_slice(name);
    b[160..160 + 9].copy_from_slice(b"subdev #0");
    b[200..204].copy_from_slice(&1u32.to_le_bytes()); // subdevices_count
    b[204..208].copy_from_slice(&1u32.to_le_bytes()); // subdevices_avail
    b
}

/// 再生の口の ioctl (Linux の SNDRV_PCM_IOCTL_*)
pub fn pcm_ioctl(req: u64, arg: usize, nonblock: bool) -> Result<i64, i64> {
    let s = get().ok_or(-ENODEV)?;
    let pt = || proc::current().pt();
    let out = |b: &[u8]| pt().copy_out(arg, b).ok_or(-EFAULT);
    let inb = |n: usize| -> Result<Vec<u8>, i64> {
        let mut b = alloc::vec![0u8; n];
        pt().copy_in(&mut b, arg).ok_or(-EFAULT)?;
        Ok(b)
    };
    let nr = req & 0xff;
    if (req >> 8) & 0xff != b'A' as u64 {
        return Err(-ENOTTY);
    }
    match nr {
        // PVERSION: 2.0.15
        0x00 => out(&0x2000fu32.to_le_bytes()).map(|_| 0),
        // INFO
        0x01 => out(&pcm_info()).map(|_| 0),
        // TSTAMP, TTSTAMP, USER_PVERSION
        0x02..=0x04 => Ok(0),
        // HW_REFINE / HW_PARAMS
        0x10 | 0x11 => {
            let mut h = Hw { b: inb(608)?.try_into().unwrap() };
            if nr == 0x10 {
                refine(s, &mut h)?;
                return out(&h.b).map(|_| 0);
            }
            if s.pcm.as_ref().is_some_and(|p| matches!(p.state, ST_RUNNING | ST_DRAINING | ST_PAUSED)) {
                return Err(-EBADFD);
            }
            choose(s, &mut h)?;
            hw_params(s, &h)?;
            out(&h.b).map(|_| 0)
        }
        // HW_FREE
        0x12 => {
            release(s);
            Ok(0)
        }
        // SW_PARAMS (136 バイト)
        0x13 => {
            let b = inb(136)?;
            let p = s.pcm.as_mut().ok_or(-EBADFD)?;
            let q = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
            p.avail_min = q(16).max(1);
            p.start_threshold = q(32);
            p.stop_threshold = q(40);
            if q(64) != 0 {
                p.boundary = q(64);
            }
            Ok(0)
        }
        // STATUS / STATUS_EXT (152 バイト)
        0x20 | 0x24 => {
            let p = s.pcm.as_ref().ok_or(-EBADFD)?;
            let mut b = [0u8; 152];
            b[0..4].copy_from_slice(&p.state.to_le_bytes());
            let t = p.trigger_ns;
            b[8..16].copy_from_slice(&(t / 1_000_000_000).to_le_bytes());
            b[16..24].copy_from_slice(&(t % 1_000_000_000).to_le_bytes());
            b[24..40].copy_from_slice(&now_ts());
            b[40..48].copy_from_slice(&(p.appl_ptr % p.boundary).to_le_bytes());
            b[48..56].copy_from_slice(&(p.hw_ptr % p.boundary).to_le_bytes());
            b[56..64].copy_from_slice(&(p.appl_ptr.saturating_sub(p.hw_ptr)).to_le_bytes()); // delay
            b[64..72].copy_from_slice(&avail(p).to_le_bytes());
            b[72..80].copy_from_slice(&avail(p).to_le_bytes()); // avail_max
            b[96..112].copy_from_slice(&now_ts());
            out(&b).map(|_| 0)
        }
        // DELAY
        0x21 => {
            let p = s.pcm.as_ref().ok_or(-EBADFD)?;
            if p.state == ST_XRUN {
                return Err(-EPIPE);
            }
            out(&(p.appl_ptr.saturating_sub(p.hw_ptr) as i64).to_le_bytes()).map(|_| 0)
        }
        // HWSYNC
        0x22 => {
            let p = s.pcm.as_ref().ok_or(-EBADFD)?;
            if p.state == ST_XRUN {
                return Err(-EPIPE);
            }
            Ok(0)
        }
        // SYNC_PTR (136 バイト): flags, status (8..72), control (72..136)
        0x23 => {
            let mut b = inb(136)?;
            let p = s.pcm.as_mut().ok_or(-EBADFD)?;
            let flags = u32::from_le_bytes(b[0..4].try_into().unwrap());
            const APPL: u32 = 2;
            const AVAIL_MIN: u32 = 4;
            if flags & APPL == 0 {
                // mmap で書いた分: appl_ptr を進める (boundary で回る値)
                let a = u64::from_le_bytes(b[72..80].try_into().unwrap());
                let cur = p.appl_ptr % p.boundary;
                let d = (a + p.boundary - cur) % p.boundary;
                if d <= p.buffer_frames {
                    p.appl_ptr += d;
                }
            }
            if flags & AVAIL_MIN == 0 {
                p.avail_min = u64::from_le_bytes(b[80..88].try_into().unwrap()).max(1);
            }
            let state = p.state;
            b[8..12].copy_from_slice(&state.to_le_bytes());
            b[16..24].copy_from_slice(&(p.hw_ptr % p.boundary).to_le_bytes());
            b[24..40].copy_from_slice(&now_ts());
            b[48..64].copy_from_slice(&now_ts());
            b[72..80].copy_from_slice(&(p.appl_ptr % p.boundary).to_le_bytes());
            b[80..88].copy_from_slice(&p.avail_min.to_le_bytes());
            s.send();
            out(&b).map(|_| 0)
        }
        // CHANNEL_INFO: channel, offset (バイト), first (ビット), step (ビット)
        0x32 => {
            let mut b = inb(24)?;
            let p = s.pcm.as_ref().ok_or(-EBADFD)?;
            let c = u32::from_le_bytes(b[0..4].try_into().unwrap());
            if c >= p.channels {
                return Err(-EINVAL);
            }
            let bits = (p.frame_bytes * 8) as u32;
            b[8..16].fill(0);
            b[16..20].copy_from_slice(&(c * bits / p.channels).to_le_bytes());
            b[20..24].copy_from_slice(&bits.to_le_bytes());
            out(&b).map(|_| 0)
        }
        // PREPARE
        0x40 => {
            let p = s.pcm.as_mut().ok_or(-EBADFD)?;
            if p.state == ST_OPEN {
                return Err(-EBADFD);
            }
            if matches!(p.state, ST_RUNNING | ST_XRUN | ST_PAUSED | ST_DRAINING) {
                s.simple(R_PCM_STOP);
            }
            prepare(s)
        }
        // RESET: 書いたものを捨てる
        0x41 => {
            let p = s.pcm.as_mut().ok_or(-EBADFD)?;
            p.appl_ptr = p.sent_ptr.max(p.hw_ptr);
            Ok(0)
        }
        // START
        0x42 => start(s).map(|_| 0),
        // DROP: すぐ止める
        0x43 => {
            let p = s.pcm.as_mut().ok_or(-EBADFD)?;
            if p.state == ST_OPEN {
                return Err(-EBADFD);
            }
            if matches!(p.state, ST_RUNNING | ST_DRAINING | ST_PAUSED) {
                s.simple(R_PCM_STOP);
            }
            prepare(s)?;
            let p = s.pcm.as_mut().unwrap();
            p.state = ST_SETUP;
            Ok(0)
        }
        // DRAIN: 書いたものを鳴らし終わるまで待つ
        0x44 => {
            let p = s.pcm.as_mut().ok_or(-EBADFD)?;
            match p.state {
                ST_OPEN => return Err(-EBADFD),
                ST_PREPARED if p.appl_ptr > p.hw_ptr => {
                    start(s)?;
                }
                ST_PREPARED | ST_XRUN | ST_SETUP => {
                    p.state = ST_SETUP;
                    return Ok(0);
                }
                _ => {}
            }
            let p = s.pcm.as_mut().unwrap();
            if p.state == ST_RUNNING {
                p.state = ST_DRAINING;
            }
            if nonblock {
                return Err(-EAGAIN);
            }
            loop {
                let Some(p) = get().and_then(|s| s.pcm.as_ref()) else { return Ok(0) };
                if p.state != ST_DRAINING {
                    return Ok(0);
                }
                proc::sleep(chan())?;
            }
        }
        // PAUSE
        0x45 => {
            let p = s.pcm.as_mut().ok_or(-EBADFD)?;
            match (arg != 0, p.state) {
                (true, ST_RUNNING) => {
                    p.state = ST_PAUSED;
                    s.simple(R_PCM_STOP);
                }
                (false, ST_PAUSED) => {
                    p.state = ST_RUNNING;
                    s.send();
                    s.simple(R_PCM_START);
                }
                _ => return Err(-EBADFD),
            }
            Ok(0)
        }
        // REWIND / FORWARD: 動かさない (0 フレーム)
        0x46 | 0x49 => out(&0u64.to_le_bytes()).map(|_| 0),
        // RESUME: 眠らないので、できない
        0x47 => Err(-38),
        // XRUN
        0x48 => {
            let p = s.pcm.as_mut().ok_or(-EBADFD)?;
            if matches!(p.state, ST_RUNNING | ST_PREPARED | ST_PAUSED) {
                if p.state != ST_PREPARED {
                    s.simple(R_PCM_STOP);
                }
                s.pcm.as_mut().unwrap().state = ST_XRUN;
            }
            Ok(0)
        }
        // WRITEI_FRAMES: { result (sframes), buf, frames }
        0x50 => {
            let mut b = inb(24)?;
            let buf = usize::from_le_bytes(b[8..16].try_into().unwrap());
            let frames = u64::from_le_bytes(b[16..24].try_into().unwrap());
            let n = writei(buf, frames, nonblock)?;
            b[0..8].copy_from_slice(&(n as i64).to_le_bytes());
            out(&b).map(|_| 0)
        }
        _ => Err(-ENOTTY),
    }
}

/// 決まった形でリングバッファを用意し、デバイスに伝える
fn hw_params(s: &mut Snd, h: &Hw) -> Result<(), i64> {
    release(s);
    let fm = h.mask(P_FORMAT);
    let &(_, vformat, bits) = FORMATS.iter().find(|f| fm & 1 << f.0 != 0).ok_or(-EINVAL)?;
    let channels = h.iv(P_CHANNELS).lo() as u32;
    let rate = h.iv(P_RATE).lo() as u32;
    let period = h.iv(P_PERIOD_SIZE).lo();
    let buffer = h.iv(P_BUFFER_SIZE).lo();
    let frame_bytes = (bits / 8 * channels) as usize;
    if channels == 0 || period == 0 || buffer < period || frame_bytes == 0 {
        return Err(-EINVAL);
    }
    let bytes = buffer as usize * frame_bytes;
    let mut pages = Vec::new();
    for _ in 0..bytes.div_ceil(PGSIZE) {
        let Some(pg) = kalloc::alloc() else {
            for p in pages {
                kalloc::put(p);
            }
            return Err(-ENOMEM);
        };
        pages.push(pg);
    }
    // デバイスへ: SET_PARAMS { hdr, stream_id, buffer_bytes, period_bytes, features, channels, format, rate, pad }
    let rix = RATES.iter().position(|&r| r == rate).ok_or(-EINVAL)? as u8;
    let mut q = Vec::new();
    for v in [R_PCM_SET_PARAMS, s.stream, bytes as u32, (CHUNK as u32).min(bytes as u32), 0] {
        q.extend_from_slice(&v.to_le_bytes());
    }
    q.extend_from_slice(&[channels as u8, vformat, rix, 0]);
    if s.cmd(&q, 4) != S_OK {
        for p in pages {
            kalloc::put(p);
        }
        return Err(-EINVAL);
    }
    let slots = (s.tx.size / 3) as u16;
    // boundary: バッファの大きさの 2 の累乗倍で、long に収まる大きなもの (alsa-lib と同じ考え)
    let mut boundary = buffer;
    while boundary * 2 <= (i64::MAX as u64) / 2 - buffer {
        boundary *= 2;
    }
    s.pcm = Some(Pcm {
        state: ST_SETUP,
        channels,
        frame_bytes,
        buffer_frames: buffer,
        pages: Rc::new(pages),
        hw_ptr: 0,
        appl_ptr: 0,
        sent_ptr: 0,
        boundary,
        avail_min: period,
        start_threshold: 1,
        stop_threshold: buffer,
        inflight: alloc::collections::VecDeque::new(),
        free_slots: (0..slots).rev().collect(),
        trigger_ns: 0,
    });
    Ok(())
}

/// PREPARE: 位置を 0 にして、デバイスを用意する
fn prepare(s: &mut Snd) -> Result<i64, i64> {
    // 渡したままのものが返るのを待つ (STOP の後、デバイスは残りを返す)
    for _ in 0..1_000_000 {
        s.reap();
        if s.pcm.as_ref().is_none_or(|p| p.inflight.is_empty()) {
            break;
        }
        core::hint::spin_loop();
    }
    let p = s.pcm.as_mut().ok_or(-EBADFD)?;
    p.hw_ptr = 0;
    p.appl_ptr = 0;
    p.sent_ptr = 0;
    p.inflight.clear();
    let slots = (s.tx.size / 3) as u16;
    p.free_slots = (0..slots).rev().collect();
    if s.simple(R_PCM_PREPARE) != S_OK {
        return Err(-EINVAL);
    }
    s.pcm.as_mut().unwrap().state = ST_PREPARED;
    proc::wakeup(chan());
    proc::poll_wake(chan());
    Ok(0)
}

/// WRITEI_FRAMES: ユーザーの buf から frames フレームをリングバッファへ。空くまで待つ
fn writei(buf: usize, frames: u64, nonblock: bool) -> Result<u64, i64> {
    let mut done = 0u64;
    loop {
        let s = get().ok_or(-ENODEV)?;
        let p = s.pcm.as_mut().ok_or(-EBADFD)?;
        match p.state {
            ST_XRUN => return if done > 0 { Ok(done) } else { Err(-EPIPE) },
            ST_PREPARED | ST_RUNNING | ST_PAUSED => {}
            _ => return Err(-EBADFD),
        }
        if done == frames {
            return Ok(done);
        }
        let room = avail(p);
        if room == 0 {
            if nonblock {
                return if done > 0 { Ok(done) } else { Err(-EAGAIN) };
            }
            proc::sleep(chan())?;
            continue;
        }
        // リングバッファの終わりかページの終わりまでずつ写す
        let n = room.min(frames - done);
        let mut k = 0u64;
        while k < n {
            let pos = ((p.appl_ptr + k) % p.buffer_frames) as usize * p.frame_bytes;
            let left = (n - k) as usize * p.frame_bytes;
            let c = left.min(PGSIZE - pos % PGSIZE).min(p.buffer_frames as usize * p.frame_bytes - pos);
            let dst = unsafe { core::slice::from_raw_parts_mut(p.pages[pos / PGSIZE].add(pos % PGSIZE), c) };
            proc::current().pt().copy_in(dst, buf + (done + k) as usize * p.frame_bytes).ok_or(-EFAULT)?;
            k += (c / p.frame_bytes) as u64;
        }
        let p = s.pcm.as_mut().unwrap();
        p.appl_ptr += n;
        done += n;
        if p.state == ST_PREPARED && p.appl_ptr - p.hw_ptr >= p.start_threshold.max(1) {
            start(s)?;
        } else {
            s.send();
        }
    }
}

// ---- 制御の口 (/dev/snd/controlC0) ----

/// 制御の口の ioctl (Linux の SNDRV_CTL_IOCTL_*)。音量などの部品 (elem) はまだない
pub fn ctl_ioctl(req: u64, arg: usize) -> Result<i64, i64> {
    let pt = || proc::current().pt();
    let out = |b: &[u8]| pt().copy_out(arg, b).ok_or(-EFAULT);
    if (req >> 8) & 0xff != b'U' as u64 {
        return Err(-ENOTTY);
    }
    match req & 0xff {
        // PVERSION: 2.0.9
        0x00 => out(&0x20009u32.to_le_bytes()).map(|_| 0),
        // CARD_INFO (376 バイト): card, pad, id[16], driver[16], name[32], longname[80], reserved[16], mixername[80], components[128]
        0x01 => {
            let mut b = [0u8; 376];
            let put = |b: &mut [u8; 376], o: usize, s: &[u8]| b[o..o + s.len()].copy_from_slice(s);
            put(&mut b, 8, b"VirtIOSound");
            put(&mut b, 24, b"virtio-snd");
            put(&mut b, 40, b"VirtIO SoundCard");
            put(&mut b, 72, b"VirtIO SoundCard at virtio-mmio");
            put(&mut b, 168, b"virtio-snd");
            out(&b).map(|_| 0)
        }
        // ELEM_LIST (80 バイト): offset, space, used, count, pids。部品は 0 個
        0x10 => {
            let mut b = [0u8; 80];
            pt().copy_in(&mut b, arg).ok_or(-EFAULT)?;
            b[8..16].fill(0);
            out(&b).map(|_| 0)
        }
        // SUBSCRIBE_EVENTS
        0x16 => Ok(0),
        // PCM_NEXT_DEVICE: -1 の次は 0、0 の次はない (-1)
        0x30 => {
            let mut b = [0u8; 4];
            pt().copy_in(&mut b, arg).ok_or(-EFAULT)?;
            let d = i32::from_le_bytes(b);
            let next: i32 = if d < 0 { 0 } else { -1 };
            out(&next.to_le_bytes()).map(|_| 0)
        }
        // PCM_INFO: device 0、再生 (stream 0) だけ
        0x31 => {
            let mut b = [0u8; 12];
            pt().copy_in(&mut b, arg).ok_or(-EFAULT)?;
            let (dev, sub, stream) = (u32::from_le_bytes(b[0..4].try_into().unwrap()), u32::from_le_bytes(b[4..8].try_into().unwrap()), i32::from_le_bytes(b[8..12].try_into().unwrap()));
            if dev != 0 || sub != 0 || stream != 0 {
                return Err(-ENXIO);
            }
            out(&pcm_info()).map(|_| 0)
        }
        // PCM_PREFER_SUBDEVICE
        0x32 => Ok(0),
        // POWER_STATE: D0
        0xd1 => out(&0u32.to_le_bytes()).map(|_| 0),
        _ => Err(-ENOTTY),
    }
}

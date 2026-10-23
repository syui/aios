// 乱数 (Linux の random.c と同じ作り)。getrandom、/dev/random と /dev/urandom、AT_RANDOM、カーネルの中 (ext4、ネット)
//   池: 乱数のもとを BLAKE2s にまぜていく。もとは
//     DTB の /chosen/rng-seed (QEMU やブートローダーが入れてくれる 32 バイトほど。読んだら DTB から消す)、
//     UEFI の EFI_RNG_PROTOCOL (efi.rs が ExitBootServices の前にもらっておく)、CPU の RNDR (FEAT_RNG があれば)、
//     乱数の装置 (QEMU の -device virtio-rng-device は virtio_rng.rs、ラズパイのものは bcm2835_rng.rs。
//     起こしたときに入れ、set_source でつなぐ)。どれも数える (seed)。
//     時計のカウンタや /dev/urandom に書かれたものもまぜるが、数えない (mix)
//     数えたものが 256 ビットになれば ready (/proc/sys/kernel/random/entropy_avail が 256)。ready でないうちは
//     getrandom の GRND_NONBLOCK は EAGAIN。待たせはしない (aios では、あとからもとが来ることがないので)
//   鍵: 池から ChaCha20 の鍵 (32 バイト) を作る (reseed: 数えるもとが来たときと、60 秒ごと)。
//     出すたびに、ChaCha20 の最初のブロックの頭 32 バイトを次の鍵にして古い鍵を消し (fast key erasure)、
//     残りの 32 バイトと、古い鍵の続きのブロックを渡す。出したものから、前に出したものも次に出すものもわからない
use crate::crypto::{self, Blake2s};
use crate::spinlock::SpinLock;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};

struct State {
    /// 池 (前の reseed で作った鍵つきの BLAKE2s。はじめは鍵なし)
    pool: Blake2s,
    /// 数えたもとのビット
    credit: u32,
    /// ChaCha20 の鍵
    key: [u8; 32],
    /// 数えたもとの名前とバイト数 (起動のときに出す)
    from: [(&'static str, usize); 4],
    /// 起動のときのもとを入れたか
    seeded: bool,
}

static STATE: SpinLock<State> = SpinLock::new(State { pool: Blake2s::new(&[]), credit: 0, key: [0; 32], from: [("", 0); 4], seeded: false });
static READY: AtomicBool = AtomicBool::new(false);
/// 前の reseed の時刻 (起動からの ns)
static LAST: AtomicU64 = AtomicU64::new(0);
/// 60 秒ごとにもとを取りにいくところ (乱数の装置)。取れたバイト数を返す
type Source = fn(&mut [u8]) -> usize;
static SOURCE: SpinLock<Option<Source>> = SpinLock::new(None);
const RESEED_NS: u64 = 60_000_000_000;

fn counter() -> u64 {
    let c: u64;
    unsafe { core::arch::asm!("mrs {}, cntvct_el0", out(reg) c, options(nomem, nostack, preserves_flags)) };
    c
}

/// RNDR があるか (0: まだ見ていない、1: ない、2: ある)
static RNDR: AtomicU8 = AtomicU8::new(0);

/// CPU の乱数 (FEAT_RNG の RNDRRS: 読むたびに種をまきなおしたもの)。ないか、しくじりつづけたら None
fn cpu_random() -> Option<u64> {
    if RNDR.load(Ordering::Relaxed) == 0 {
        let isar0: u64;
        unsafe { core::arch::asm!("mrs {}, id_aa64isar0_el1", out(reg) isar0, options(nomem, nostack, preserves_flags)) };
        RNDR.store(if (isar0 >> 60) & 0xf != 0 { 2 } else { 1 }, Ordering::Relaxed);
    }
    if RNDR.load(Ordering::Relaxed) != 2 {
        return None;
    }
    for _ in 0..10 {
        let (v, ok): (u64, u64);
        // しくじると Z が立つ (値は 0)
        unsafe { core::arch::asm!("mrs {v}, s3_3_c2_c4_1", "cset {ok}, ne", v = out(reg) v, ok = out(reg) ok, options(nomem, nostack)) };
        if ok != 0 {
            return Some(v);
        }
    }
    None
}

/// 数えるもとを池に入れる
fn credit(s: &mut State, data: &[u8]) {
    s.pool.update(data);
    s.credit = s.credit.saturating_add(data.len() as u32 * 8);
    if s.credit >= 256 {
        READY.store(true, Ordering::Relaxed);
    }
}

/// 起動のときに出す、もとの名前とバイト数
fn note(s: &mut State, from: &'static str, len: usize) {
    if let Some(f) = s.from.iter_mut().find(|f| f.0 == from || f.1 == 0) {
        *f = (from, f.1 + len);
    }
}

/// 池から鍵を作り、池はそこから作った次の鍵で始めなおす (Linux の extract_entropy と crng_reseed)
fn reseed(s: &mut State) {
    // RNDR があればそれを、なければ時計のカウンタを 32 バイトと、0 / 1 (次の池の鍵か、ChaCha20 の鍵か)
    let mut block = [0u8; 40];
    for c in block[..32].chunks_exact_mut(8) {
        c.copy_from_slice(&cpu_random().unwrap_or_else(counter).to_le_bytes());
    }
    let mut seed = core::mem::replace(&mut s.pool, Blake2s::new(&[])).finish();
    let mut next = crypto::blake2s(&seed, &block);
    s.pool = Blake2s::new(&next);
    block[32] = 1;
    s.key = crypto::blake2s(&seed, &block);
    crypto::wipe(&mut seed);
    crypto::wipe(&mut next);
    crypto::wipe(&mut block);
    LAST.store(crate::timer::uptime_ns(), Ordering::Relaxed);
    // CPU ごとにまとめて作っておいたもの (u32) は、新しい鍵で作りなおす
    GEN.fetch_add(1, Ordering::Relaxed);
}

/// 鍵を作りなおした回数 (Batch はこれが変わったら捨てる)
static GEN: AtomicU64 = AtomicU64::new(0);

/// CPU ごとに、まとめて作った乱数 (Linux の get_random_u32 の batched_entropy)
struct Batch {
    buf: [u8; 64],
    /// buf のまだ渡していないところの始め
    pos: usize,
    generation: u64,
}

static BATCH: crate::cell::PerCpu<Batch> = crate::cell::PerCpu::new([const { Batch { buf: [0; 64], pos: 64, generation: 0 } }; crate::smp::MAXCPU]);

/// 32 ビットの乱数を、CPU ごとにまとめて作ったものから (inode の generation など、たびたび要る小さいもの)。u64() と
/// fill() は呼ぶたびに鍵を進めて ChaCha20 のブロックを作る (TCG では重く、時計も読む) ので、64 バイトずつ作っておき、
/// 渡したところは消す。鍵を作りなおしたら捨てる。カーネルは割り込みを止めて動き、この中で眠らないので、CPU ごとの
/// ものを取りあうことはない
pub fn u32() -> u32 {
    let b = BATCH.get();
    let g = GEN.load(Ordering::Relaxed);
    if b.pos + 4 > b.buf.len() || b.generation != g {
        fill(&mut b.buf);
        b.pos = 0;
        b.generation = g;
    }
    let v = u32::from_le_bytes(b.buf[b.pos..b.pos + 4].try_into().unwrap());
    crypto::wipe(&mut b.buf[b.pos..b.pos + 4]);
    b.pos += 4;
    v
}

/// 数えるもと (ブートローダー、UEFI、virtio-rng のくれた乱数) を池に入れて、鍵を作りなおす
pub fn seed(from: &'static str, data: &[u8]) {
    if data.is_empty() {
        return;
    }
    let mut s = STATE.lock();
    credit(&mut s, data);
    note(&mut s, from, data.len());
    reseed(&mut s);
}

/// 数えないもと (/dev/urandom に書かれたものなど) を池にまぜる。鍵に入るのは次の reseed
pub fn mix(data: &[u8]) {
    STATE.lock().pool.update(data);
}

/// 60 秒ごとにもとを取りにいくところ (乱数の装置)
pub fn set_source(f: Source) {
    *SOURCE.lock() = Some(f);
}

/// 起動のときのもと (時計、DTB の rng-seed と kaslr-seed、RNDR) を池に入れて、はじめの鍵を作る。
/// 乱数を使う前に kmain から (使う人が先に来たら、そこで)。2 度目からは何もしない
pub fn init() {
    boot(&mut STATE.lock());
}

fn boot(s: &mut State) {
    if s.seeded {
        return;
    }
    s.seeded = true;
    s.pool.update(&counter().to_le_bytes());
    crate::dtb::take_seed("rng-seed", |b| {
        credit(s, b);
        note(s, "rng-seed", b.len());
    });
    crate::dtb::take_seed("kaslr-seed", |b| s.pool.update(b));
    let mut b = [0u8; 32];
    if b.chunks_exact_mut(8).all(|c| cpu_random().map(|v| c.copy_from_slice(&v.to_le_bytes())).is_some()) {
        credit(s, &b);
        note(s, "RNDR", b.len());
    }
    crypto::wipe(&mut b);
    reseed(s);
}

/// 起動のときに、どこからもとをもらったかを出す
pub fn summary() {
    let (bits, from) = {
        let s = STATE.lock();
        (s.credit, s.from)
    };
    if bits >= 256 {
        print!("rand: ready (ChaCha20) from");
        for (i, (n, len)) in from.iter().filter(|f| f.1 > 0).enumerate() {
            print!("{} {} {} bytes", if i > 0 { "," } else { "" }, n, len);
        }
        println!();
    } else {
        println!("rand: NOT ready, {} bits: no rng-seed in the DTB, EFI_RNG_PROTOCOL, RNDR, virtio-rng or bcm2835-rng (QEMU: -device virtio-rng-device); random numbers can be guessed", bits);
    }
}

/// 数えたもとのビット (256 まで。/proc/sys/kernel/random/entropy_avail)
pub fn entropy() -> u32 {
    STATE.lock().credit.min(256)
}

/// 数えたもとが 256 ビットになったか
pub fn ready() -> bool {
    READY.load(Ordering::Relaxed)
}

/// 60 秒たっていたら、もと (乱数の装置。RNDR は reseed の中) を取りにいって、鍵を作りなおす。行くのは 1 つの CPU だけ
fn refresh() {
    let last = LAST.load(Ordering::Relaxed);
    let now = crate::timer::uptime_ns();
    if now.wrapping_sub(last) < RESEED_NS || LAST.compare_exchange(last, now, Ordering::AcqRel, Ordering::Relaxed).is_err() {
        return;
    }
    let f = *SOURCE.lock();
    let mut b = [0u8; 32];
    let n = f.map_or(0, |f| f(&mut b));
    let mut s = STATE.lock();
    credit(&mut s, &b[..n]);
    s.pool.update(&now.to_le_bytes());
    reseed(&mut s);
    crypto::wipe(&mut b);
}

/// 鍵を 1 度だけ進めて、長いものを続けて出す (getrandom、/dev/urandom)。消えるときに状態を消す
pub struct Stream {
    st: [u32; 16],
    buf: [u8; 64],
    /// buf のまだ渡していないところの始め
    pos: usize,
}

pub fn stream() -> Stream {
    refresh();
    let mut buf = [0u8; 64];
    let st = {
        let mut s = STATE.lock();
        boot(&mut s);
        let mut st = crypto::chacha_state(&s.key, 0, &[0; 12]);
        crypto::chacha20_block(&mut st, &mut buf);
        s.key.copy_from_slice(&buf[..32]);
        st
    };
    // 頭の 32 バイトは次の鍵なので渡さない
    crypto::wipe(&mut buf[..32]);
    Stream { st, buf, pos: 32 }
}

impl Stream {
    pub fn fill(&mut self, dst: &mut [u8]) {
        let mut off = 0;
        while off < dst.len() {
            if self.pos == 64 {
                crypto::chacha20_block(&mut self.st, &mut self.buf);
                if self.st[12] == 0 {
                    self.st[13] = self.st[13].wrapping_add(1);
                }
                self.pos = 0;
            }
            let k = (64 - self.pos).min(dst.len() - off);
            dst[off..off + k].copy_from_slice(&self.buf[self.pos..self.pos + k]);
            self.pos += k;
            off += k;
        }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        crypto::wipe32(&mut self.st);
        crypto::wipe(&mut self.buf);
    }
}

/// dst を乱数で満たす
pub fn fill(dst: &mut [u8]) {
    stream().fill(dst);
}

pub fn u64() -> u64 {
    let mut b = [0u8; 8];
    fill(&mut b);
    u64::from_le_bytes(b)
}

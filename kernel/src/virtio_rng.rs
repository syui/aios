// virtio-rng (QEMU の -device virtio-rng-device): ホストの乱数を rand の池に入れる (rand.rs)。
// 起こしたときに 64 バイト、あとは rand が 60 秒ごとに取りにくる (set_source)。どちらも返るまで回って待つ
// (QEMU はすぐ返す。0.1 秒で返らなければ、それからは使わない)。割り込みは使わない
use crate::memlayout::v2p;
use crate::spinlock::SpinLock;
use crate::virtio::{self, Mmio, Queue, DESC_WRITE};

const DEVICE_ENTROPY: u32 = 4;

struct Rng {
    mmio: Mmio,
    q: Queue,
    /// 装置が書くところ
    buf: [u8; 64],
    /// 返ってこなかった (もう使わない)
    dead: bool,
}

// Queue は生のポインタを持つが、さわるのは RNG のロックを持った人だけ
unsafe impl Send for Rng {}

static RNG: SpinLock<Option<Rng>> = SpinLock::new(None);

/// 見つかったら、64 バイトを rand へ入れて true
pub fn init() -> bool {
    let Some((mmio, _)) = virtio::probe(DEVICE_ENTROPY, 0) else { return false };
    let Some(q) = Queue::new(&mmio, 0, 2) else { return false };
    virtio::ready(&mmio);
    *RNG.lock() = Some(Rng { mmio, q, buf: [0; 64], dead: false });
    let mut b = [0u8; 64];
    let n = read(&mut b);
    crate::rand::seed("virtio-rng", &b[..n]);
    crate::crypto::wipe(&mut b);
    crate::rand::set_source(read);
    true
}

/// out にホストの乱数を (64 バイトまで)。入れたバイト数 (ほかの人が使っているところなら 0)
fn read(out: &mut [u8]) -> usize {
    let Some(mut g) = RNG.try_lock() else { return 0 };
    let Some(r) = g.as_mut().filter(|r| !r.dead) else { return 0 };
    let n = out.len().min(r.buf.len());
    *r.q.desc(0) = virtio::Desc { addr: v2p(r.buf.as_mut_ptr() as usize) as u64, len: n as u32, flags: DESC_WRITE, next: 0 };
    r.q.push(0);
    r.q.notify(&r.mmio);
    let t0 = crate::timer::uptime_ns();
    loop {
        if let Some((_, len)) = r.q.pop_used() {
            r.mmio.ack();
            let len = (len as usize).min(n);
            out[..len].copy_from_slice(&r.buf[..len]);
            crate::crypto::wipe(&mut r.buf);
            return len;
        }
        if crate::timer::uptime_ns().wrapping_sub(t0) > 100_000_000 {
            r.dead = true;
            println!("virtio-rng: no answer in 0.1 s (not used any more)");
            return 0;
        }
        core::hint::spin_loop();
    }
}

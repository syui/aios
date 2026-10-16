#!/usr/bin/env python3
# kernel/src/tlsf.rs (カーネルヒープの空きの管理) を確かめる。CI と手元で: python3 test/heap.py
# 開発のマシンの rustc でビルドして (tlsf.rs は core のほかを使わない)、2 つの区画で
#   でたらめに取って返す (小さいもの、4 KiB まで、64 KiB まで、1 MiB まで): 取ったものの中身には印を書いておき、
#     返すときに変わっていないか (重なり、ヒープが中身に書いた)。ときどき Tlsf::check (つながり、印、リスト、ビット)
#   取れなくなるまで取る (64 KiB、32 バイト)、1 つおきに返して別の大きさで取る (割れたところ)
#   どれも、ぜんぶ返したら区画ごとに 1 つの空きに戻るか
# 終わりの番号はしくじりの数
import os
import subprocess
import sys
import tempfile

src = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "kernel", "src", "tlsf.rs")
HARNESS = r'''
#[path = "%s"]
#[allow(dead_code)]
mod tlsf;
use tlsf::Tlsf;

const R1: usize = 8 << 20;
const R2: usize = 1 << 20;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 { self.0 ^= self.0 << 13; self.0 ^= self.0 >> 7; self.0 ^= self.0 << 17; self.0 }
    fn below(&mut self, n: u64) -> u64 { self.next() %% n }
}

/// heap.rs と同じ: 中身を 16 にそろえて、ヘッダの 16 バイトを足す
fn need_of(size: usize) -> usize { ((size.max(1) + 15) & !15) + 16 }

struct T { t: Tlsf, fails: u32, live: Vec<(*mut u8, usize, u8)>, lo: [usize; 2], hi: [usize; 2] }

impl T {
    fn fail(&mut self, what: &str) { self.fails += 1; if self.fails <= 10 { println!("FAIL {}", what); } }
    fn check(&mut self, at: &str) {
        if let Err(e) = unsafe { self.t.check() } { self.fail(&format!("{}: {}", at, e)); }
    }
    /// size バイトを取って、2 語目から終わりまでに印を書く (取れなければ false)
    fn take(&mut self, size: usize, pat: u8) -> bool {
        let need = need_of(size);
        let b = unsafe { self.t.alloc(need) };
        if b.is_null() { return false; }
        let bs = unsafe { tlsf::block_size(b) };
        let a = b as usize;
        let inside = (a >= self.lo[0] && a + bs <= self.hi[0]) || (a >= self.lo[1] && a + bs <= self.hi[1]);
        if bs < need || a %% 16 != 0 || !inside { self.fail(&format!("bad block {:#x} size {} for {}", a, bs, need)); return false; }
        unsafe { std::ptr::write_bytes(b.add(8), pat, bs - 8) };
        self.live.push((b, bs, pat));
        true
    }
    fn give(&mut self, k: usize) {
        let (b, bs, pat) = self.live.swap_remove(k);
        let s = unsafe { std::slice::from_raw_parts(b.add(8), bs - 8) };
        if s.iter().any(|&x| x != pat) { self.fail("a block in use was written over"); }
        unsafe { self.t.free(b) };
    }
    fn give_all(&mut self, at: &str) {
        while !self.live.is_empty() { let k = self.live.len() - 1; self.give(k); }
        self.check(at);
        let (n, big, bytes) = self.t.stats();
        if n != 2 || big != R1 - 16 || bytes != R1 - 16 + R2 - 16 {
            self.fail(&format!("{}: not back to one free block per region ({} blocks, largest {}, {} bytes)", at, n, big, bytes));
        }
    }
}

fn main() {
    let r1 = vec![0u128; R1 / 16];
    let r2 = vec![0u128; R2 / 16];
    let (p1, p2) = (r1.as_ptr() as *mut u8, r2.as_ptr() as *mut u8);
    let mut t = T { t: Tlsf::new(), fails: 0, live: Vec::new(), lo: [p1 as usize, p2 as usize], hi: [p1 as usize + R1 - 16, p2 as usize + R2 - 16] };
    unsafe { t.t.add_region(p1, R1); t.t.add_region(p2, R2); }
    t.check("start");
    t.give_all("start");

    // でたらめに
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let (mut taken, mut full) = (0u64, 0u64);
    for i in 0..300_000u64 {
        if t.live.is_empty() || rng.below(100) < 55 {
            let size = match rng.below(100) {
                0..=69 => rng.below(256) + 1,
                70..=89 => rng.below(4096) + 1,
                90..=98 => rng.below(65536) + 1,
                _ => rng.below(1 << 20) + 1,
            } as usize;
            if t.take(size, (i %% 251) as u8) { taken += 1 } else { full += 1 }
        } else {
            let k = rng.below(t.live.len() as u64) as usize;
            t.give(k);
        }
        if i %% 10_000 == 0 { t.check(&format!("random step {}", i)); }
    }
    t.give_all("random");
    println!("random: {} taken, {} not (full)", taken, full);

    // 取れなくなるまで: 64 KiB、それから 32 バイト
    let mut n = 0;
    while t.take(65536, 7) { n += 1; }
    while t.take(16, 9) { n += 1; }
    t.check("full");
    let (fb, _, fbytes) = t.t.stats();
    if fbytes > 64 << 10 { t.fail(&format!("full: {} bytes still free in {} blocks", fbytes, fb)); }
    t.give_all("full");
    println!("full: {} blocks", n);

    // 割れたところ: 小さいものでいっぱいにして、アドレスの順に 1 つおきに返す。少し大きなものは取れず、
    // 同じ大きさなら穴がみな使われる
    while t.take(16, 3) {}
    t.live.sort_by_key(|e| e.0 as usize);
    let all = std::mem::take(&mut t.live);
    let mut holes = 0;
    for (j, e) in all.into_iter().enumerate() {
        t.live.push(e);
        if j %% 2 == 0 { let k = t.live.len() - 1; t.give(k); holes += 1; }
    }
    t.check("every other");
    let mut bigger = 0;
    while t.take(40, 5) { bigger += 1; }
    if bigger != 0 { t.fail(&format!("took {} blocks of 64 bytes from 32-byte holes", bigger)); }
    let mut refilled = 0;
    while t.take(16, 6) { refilled += 1; }
    if refilled != holes { t.fail(&format!("refilled {} of {} holes", refilled, holes)); }
    t.check("refill");
    t.give_all("fragmented");
    println!("fragmented: {} holes refilled", refilled);

    // 大きさのクラスのさかい目 (1 つずつ。R1 - 32 は区画まるごと: 上のクラスには空きがなく、同じクラスから)
    for s in [1usize, 15, 16, 17, 1000, 1008, 1009, 1024, 2032, 2048, 4096, 65536, 65537, 1 << 19, R2 - 32, R1 - 32] {
        if !t.take(s, 1) { t.fail(&format!("could not take {} bytes", s)); }
        t.check(&format!("size {}", s));
        t.give_all(&format!("size {}", s));
    }
    // 取れないほど大きなもの
    if unsafe { !t.t.alloc(1 << 40).is_null() } || unsafe { !t.t.alloc(R1 * 2).is_null() } { t.fail("took more than there is"); }
    t.check("too big");

    println!("heap: {} failed", t.fails);
    std::process::exit(t.fails.min(125) as i32);
}
'''

tmp = tempfile.mkdtemp(prefix="heap-")
exe = os.path.join(tmp, "heaptest")
with open(os.path.join(tmp, "t.rs"), "w") as f:
    f.write(HARNESS % os.path.abspath(src))
subprocess.run(["rustc", "--edition", "2024", "-O", "-o", exe, os.path.join(tmp, "t.rs")], check=True)
r = subprocess.run([exe])
sys.exit(r.returncode)

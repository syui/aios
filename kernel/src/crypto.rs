// 暗号の部品: ChaCha20 (RFC 8439) と BLAKE2s (RFC 7693)。乱数 (rand.rs) の鍵と池に使う
// core のほかは使わない (ヒープもロックもいらない) ので、開発のマシンでも答え合わせができる:
// python3 test/crypto.py (rustc でビルドして、Python の hashlib の BLAKE2s と、RFC の例、openssl の ChaCha20 とくらべる)

/// "expand 32-byte k"
const CONST: [u32; 4] = [0x6170_7865, 0x3320_646e, 0x7962_2d32, 0x6b20_6574];

fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

/// ChaCha20 の状態 (16 語): 定数 4 語、鍵 8 語、数え (12 語め)、nonce 3 語
pub fn chacha_state(key: &[u8; 32], counter: u32, nonce: &[u8; 12]) -> [u32; 16] {
    let mut s = [0u32; 16];
    s[..4].copy_from_slice(&CONST);
    for i in 0..8 {
        s[4 + i] = le32(&key[i * 4..]);
    }
    s[12] = counter;
    for i in 0..3 {
        s[13 + i] = le32(&nonce[i * 4..]);
    }
    s
}

fn quarter(x: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    x[a] = x[a].wrapping_add(x[b]);
    x[d] = (x[d] ^ x[a]).rotate_left(16);
    x[c] = x[c].wrapping_add(x[d]);
    x[b] = (x[b] ^ x[c]).rotate_left(12);
    x[a] = x[a].wrapping_add(x[b]);
    x[d] = (x[d] ^ x[a]).rotate_left(8);
    x[c] = x[c].wrapping_add(x[d]);
    x[b] = (x[b] ^ x[c]).rotate_left(7);
}

/// ChaCha20 の 1 ブロック (64 バイト) を out に。数え (12 語め) を 1 進める (13 語めへのくり上げは呼ぶ側)
pub fn chacha20_block(s: &mut [u32; 16], out: &mut [u8; 64]) {
    let mut x = *s;
    for _ in 0..10 {
        quarter(&mut x, 0, 4, 8, 12);
        quarter(&mut x, 1, 5, 9, 13);
        quarter(&mut x, 2, 6, 10, 14);
        quarter(&mut x, 3, 7, 11, 15);
        quarter(&mut x, 0, 5, 10, 15);
        quarter(&mut x, 1, 6, 11, 12);
        quarter(&mut x, 2, 7, 8, 13);
        quarter(&mut x, 3, 4, 9, 14);
    }
    for i in 0..16 {
        out[i * 4..i * 4 + 4].copy_from_slice(&x[i].wrapping_add(s[i]).to_le_bytes());
    }
    wipe32(&mut x);
    s[12] = s[12].wrapping_add(1);
}

const IV: [u32; 8] = [0x6a09_e667, 0xbb67_ae85, 0x3c6e_f372, 0xa54f_f53a, 0x510e_527f, 0x9b05_688c, 0x1f83_d9ab, 0x5be0_cd19];

/// 10 回のラウンドで、メッセージの 16 語をまぜる順
const PERM: [[usize; 16]; 10] = [
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
    [14, 10, 4, 8, 9, 15, 13, 6, 1, 12, 0, 2, 11, 7, 5, 3],
    [11, 8, 12, 0, 5, 2, 15, 13, 10, 14, 3, 6, 7, 1, 9, 4],
    [7, 9, 3, 1, 13, 12, 11, 14, 2, 6, 5, 10, 4, 0, 15, 8],
    [9, 0, 5, 7, 2, 4, 10, 15, 14, 1, 11, 12, 6, 8, 3, 13],
    [2, 12, 6, 10, 0, 11, 8, 3, 4, 13, 7, 5, 15, 14, 1, 9],
    [12, 5, 1, 15, 14, 13, 4, 10, 0, 7, 6, 3, 9, 2, 8, 11],
    [13, 11, 7, 14, 12, 1, 3, 9, 5, 0, 15, 4, 8, 6, 2, 10],
    [6, 15, 14, 9, 11, 3, 0, 8, 12, 2, 13, 7, 1, 4, 10, 5],
    [10, 2, 8, 4, 7, 6, 1, 5, 15, 11, 9, 14, 3, 12, 13, 0],
];

fn g(v: &mut [u32; 16], (a, b, c, d): (usize, usize, usize, usize), x: u32, y: u32) {
    v[a] = v[a].wrapping_add(v[b]).wrapping_add(x);
    v[d] = (v[d] ^ v[a]).rotate_right(16);
    v[c] = v[c].wrapping_add(v[d]);
    v[b] = (v[b] ^ v[c]).rotate_right(12);
    v[a] = v[a].wrapping_add(v[b]).wrapping_add(y);
    v[d] = (v[d] ^ v[a]).rotate_right(8);
    v[c] = v[c].wrapping_add(v[d]);
    v[b] = (v[b] ^ v[c]).rotate_right(7);
}

/// BLAKE2s で 32 バイトを出す。鍵 (32 バイトまで) があれば鍵つき (PRF)。消えるときに中身を消す
pub struct Blake2s {
    h: [u32; 8],
    /// まぜたバイト数
    t: u64,
    buf: [u8; 64],
    /// buf にたまっているバイト数。64 になっても次が来るまでは圧縮しない (最後のブロックは印をつけて圧縮するので)
    n: usize,
}

impl Blake2s {
    pub const fn new(key: &[u8]) -> Self {
        assert!(key.len() <= 32);
        let mut h = IV;
        h[0] ^= 0x0101_0000 ^ ((key.len() as u32) << 8) ^ 32;
        let mut buf = [0u8; 64];
        let mut i = 0;
        while i < key.len() {
            buf[i] = key[i];
            i += 1;
        }
        Blake2s { h, t: 0, buf, n: if key.is_empty() { 0 } else { 64 } }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        while !data.is_empty() {
            if self.n == 64 {
                self.t += 64;
                self.compress(false);
                self.n = 0;
            }
            let k = (64 - self.n).min(data.len());
            self.buf[self.n..self.n + k].copy_from_slice(&data[..k]);
            self.n += k;
            data = &data[k..];
        }
    }

    pub fn finish(mut self) -> [u8; 32] {
        self.t += self.n as u64;
        self.buf[self.n..].fill(0);
        self.compress(true);
        let mut out = [0u8; 32];
        for i in 0..8 {
            out[i * 4..i * 4 + 4].copy_from_slice(&self.h[i].to_le_bytes());
        }
        out
    }

    fn compress(&mut self, last: bool) {
        let mut m = [0u32; 16];
        for (i, w) in m.iter_mut().enumerate() {
            *w = le32(&self.buf[i * 4..]);
        }
        let mut v = [0u32; 16];
        v[..8].copy_from_slice(&self.h);
        v[8..].copy_from_slice(&IV);
        v[12] ^= self.t as u32;
        v[13] ^= (self.t >> 32) as u32;
        if last {
            v[14] = !v[14];
        }
        for p in &PERM {
            g(&mut v, (0, 4, 8, 12), m[p[0]], m[p[1]]);
            g(&mut v, (1, 5, 9, 13), m[p[2]], m[p[3]]);
            g(&mut v, (2, 6, 10, 14), m[p[4]], m[p[5]]);
            g(&mut v, (3, 7, 11, 15), m[p[6]], m[p[7]]);
            g(&mut v, (0, 5, 10, 15), m[p[8]], m[p[9]]);
            g(&mut v, (1, 6, 11, 12), m[p[10]], m[p[11]]);
            g(&mut v, (2, 7, 8, 13), m[p[12]], m[p[13]]);
            g(&mut v, (3, 4, 9, 14), m[p[14]], m[p[15]]);
        }
        for i in 0..8 {
            self.h[i] ^= v[i] ^ v[i + 8];
        }
        wipe32(&mut v);
        wipe32(&mut m);
    }
}

impl Drop for Blake2s {
    fn drop(&mut self) {
        wipe32(&mut self.h);
        wipe(&mut self.buf);
    }
}

/// 鍵つきの BLAKE2s を 1 度で
pub fn blake2s(key: &[u8; 32], data: &[u8]) -> [u8; 32] {
    let mut h = Blake2s::new(key);
    h.update(data);
    h.finish()
}

/// 0 で消す (使い終わった鍵など。最適化で書くのを省かれないように volatile で)
pub fn wipe(b: &mut [u8]) {
    for x in b.iter_mut() {
        unsafe { core::ptr::write_volatile(x, 0) };
    }
    core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
}

pub fn wipe32(b: &mut [u32]) {
    for x in b.iter_mut() {
        unsafe { core::ptr::write_volatile(x, 0) };
    }
    core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
}

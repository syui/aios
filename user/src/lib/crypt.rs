// sha512-crypt ($6$salt$hash)。glibc の crypt() と同じ計算
#![allow(dead_code)]
use sha2::{Digest, Sha512};

const B64: &[u8] = b"./0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
const ROUNDS_DEFAULT: usize = 5000;

fn b64_from_24bit(out: &mut String, b2: u8, b1: u8, b0: u8, n: usize) {
    let mut w = ((b2 as u32) << 16) | ((b1 as u32) << 8) | b0 as u32;
    for _ in 0..n {
        out.push(B64[(w & 0x3f) as usize] as char);
        w >>= 6;
    }
}

fn sha512crypt(pw: &[u8], salt: &[u8], rounds: usize, custom_rounds: bool) -> String {
    let salt = &salt[..salt.len().min(16)];
    // B = sha512(pw salt pw)
    let b = Sha512::new().chain_update(pw).chain_update(salt).chain_update(pw).finalize();
    // A
    let mut a = Sha512::new();
    a.update(pw);
    a.update(salt);
    let mut n = pw.len();
    while n > 64 {
        a.update(b);
        n -= 64;
    }
    a.update(&b[..n]);
    let mut n = pw.len();
    while n > 0 {
        if n & 1 != 0 {
            a.update(b);
        } else {
            a.update(pw);
        }
        n >>= 1;
    }
    let a = a.finalize();
    // P
    let mut dp = Sha512::new();
    for _ in 0..pw.len() {
        dp.update(pw);
    }
    let dp = dp.finalize();
    let p: Vec<u8> = dp.iter().cycle().take(pw.len()).copied().collect();
    // S
    let mut ds = Sha512::new();
    for _ in 0..16 + a[0] as usize {
        ds.update(salt);
    }
    let ds = ds.finalize();
    let s: Vec<u8> = ds.iter().cycle().take(salt.len()).copied().collect();
    // 回す
    let mut c = a.to_vec();
    for i in 0..rounds {
        let mut h = Sha512::new();
        if i & 1 != 0 {
            h.update(&p);
        } else {
            h.update(&c);
        }
        if i % 3 != 0 {
            h.update(&s);
        }
        if i % 7 != 0 {
            h.update(&p);
        }
        if i & 1 != 0 {
            h.update(&c);
        } else {
            h.update(&p);
        }
        c = h.finalize().to_vec();
    }
    let mut out = String::from("$6$");
    if custom_rounds {
        out.push_str(&format!("rounds={}$", rounds));
    }
    out.push_str(&String::from_utf8_lossy(salt));
    out.push('$');
    const ORDER: [(usize, usize, usize); 21] = [
        (0, 21, 42), (22, 43, 1), (44, 2, 23), (3, 24, 45), (25, 46, 4), (47, 5, 26), (6, 27, 48),
        (28, 49, 7), (50, 8, 29), (9, 30, 51), (31, 52, 10), (53, 11, 32), (12, 33, 54), (34, 55, 13),
        (56, 14, 35), (15, 36, 57), (37, 58, 16), (59, 17, 38), (18, 39, 60), (40, 61, 19), (62, 20, 41),
    ];
    for (x, y, z) in ORDER {
        b64_from_24bit(&mut out, c[x], c[y], c[z], 4);
    }
    b64_from_24bit(&mut out, 0, 0, c[63], 2);
    out
}

/// "$6$[rounds=N$]salt$..." の設定で pw をハッシュする
pub fn crypt(pw: &str, setting: &str) -> Option<String> {
    let rest = setting.strip_prefix("$6$")?;
    let (rounds, custom, rest) = match rest.strip_prefix("rounds=") {
        Some(r) => {
            let (n, rest) = r.split_once('$')?;
            (n.parse::<usize>().ok()?.clamp(1000, 999_999_999), true, rest)
        }
        None => (ROUNDS_DEFAULT, false, rest),
    };
    let salt = rest.split('$').next()?;
    Some(sha512crypt(pw.as_bytes(), salt.as_bytes(), rounds, custom))
}

/// shadow のハッシュと照らし合わせる ("!" や "*" で始まるものは鍵がかかっている)
pub fn verify(pw: &str, hash: &str) -> bool {
    if hash.is_empty() {
        return pw.is_empty();
    }
    match crypt(pw, hash) {
        Some(h) => h.len() == hash.len() && h.bytes().zip(hash.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0,
        None => false,
    }
}

/// 新しいハッシュ (塩は getrandom から)
pub fn hash(pw: &str) -> String {
    let mut raw = [0u8; 16];
    unsafe { libc::getrandom(raw.as_mut_ptr() as *mut _, raw.len(), 0) };
    let salt: String = raw.iter().map(|b| B64[(b & 0x3f) as usize] as char).collect();
    crypt(pw, &format!("$6${}$", salt)).unwrap()
}

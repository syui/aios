// aios の PAM
//
// sudo-rs が使う Linux-PAM の関数を、/etc/passwd と /etc/shadow (sha512-crypt) だけで実装する。
// aios は静的リンクなので、PAM モジュールは読みこまない (pam_unix だけがあるのと同じ)。
// bin/pkg-sudo-rs.sh がこのファイルを src/pam/ に置き、#[link(name = "pam")] の代わりに読みこむ。
#![allow(clippy::missing_safety_doc, clippy::undocumented_unsafe_blocks)]

use super::sys::{pam_conv, pam_handle_t, pam_message, pam_response};
use std::ffi::{CStr, CString, c_char, c_int, c_void};

const PAM_SUCCESS: c_int = 0;
const PAM_BUF_ERR: c_int = 5;
const PAM_AUTH_ERR: c_int = 7;
const PAM_USER_UNKNOWN: c_int = 10;
const PAM_CONV_ERR: c_int = 19;
const PAM_AUTHTOK_ERR: c_int = 20;
const PAM_BAD_ITEM: c_int = 29;
const PAM_DISALLOW_NULL_AUTHTOK: c_int = 1;
const PAM_PROMPT_ECHO_OFF: c_int = 1;

// pam_set_item / pam_get_item の種類
const PAM_SERVICE: c_int = 1;
const PAM_USER: c_int = 2;
const PAM_TTY: c_int = 3;
const PAM_RHOST: c_int = 4;
const PAM_CONV: c_int = 5;
const PAM_RUSER: c_int = 8;
const PAM_USER_PROMPT: c_int = 9;

struct Handle {
    items: [Option<CString>; 10],
    conv: pam_conv,
}

unsafe fn handle<'a>(pamh: *const pam_handle_t) -> Option<&'a mut Handle> {
    unsafe { (pamh as *mut Handle).as_mut() }
}

unsafe fn cstr(p: *const c_char) -> Option<CString> {
    if p.is_null() { None } else { Some(unsafe { CStr::from_ptr(p) }.to_owned()) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pam_start(service: *const c_char, user: *const c_char, conv: *const pam_conv, pamh: *mut *mut pam_handle_t) -> c_int {
    if conv.is_null() || pamh.is_null() {
        return PAM_BUF_ERR;
    }
    let mut h = Handle { items: Default::default(), conv: unsafe { *conv } };
    h.items[PAM_SERVICE as usize] = unsafe { cstr(service) };
    h.items[PAM_USER as usize] = unsafe { cstr(user) };
    unsafe { *pamh = Box::into_raw(Box::new(h)) as *mut pam_handle_t };
    PAM_SUCCESS
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pam_end(pamh: *mut pam_handle_t, _status: c_int) -> c_int {
    if !pamh.is_null() {
        drop(unsafe { Box::from_raw(pamh as *mut Handle) });
    }
    PAM_SUCCESS
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pam_set_item(pamh: *mut pam_handle_t, item_type: c_int, item: *const c_void) -> c_int {
    let Some(h) = (unsafe { handle(pamh) }) else { return PAM_BUF_ERR };
    match item_type {
        PAM_CONV if !item.is_null() => h.conv = unsafe { *(item as *const pam_conv) },
        PAM_SERVICE | PAM_USER | PAM_TTY | PAM_RHOST | PAM_RUSER | PAM_USER_PROMPT => {
            h.items[item_type as usize] = unsafe { cstr(item as *const c_char) };
        }
        _ => return PAM_BAD_ITEM,
    }
    PAM_SUCCESS
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pam_get_item(pamh: *const pam_handle_t, item_type: c_int, item: *mut *const c_void) -> c_int {
    let Some(h) = (unsafe { handle(pamh) }) else { return PAM_BUF_ERR };
    if item.is_null() {
        return PAM_BUF_ERR;
    }
    let v = match item_type {
        PAM_CONV => &h.conv as *const pam_conv as *const c_void,
        PAM_SERVICE | PAM_USER | PAM_TTY | PAM_RHOST | PAM_RUSER | PAM_USER_PROMPT => {
            h.items[item_type as usize].as_ref().map_or(std::ptr::null(), |s| s.as_ptr() as *const c_void)
        }
        _ => return PAM_BAD_ITEM,
    };
    unsafe { *item = v };
    PAM_SUCCESS
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pam_strerror(_pamh: *mut pam_handle_t, errnum: c_int) -> *const c_char {
    let s: &CStr = match errnum {
        PAM_SUCCESS => c"Success",
        PAM_AUTH_ERR => c"Authentication failure",
        PAM_USER_UNKNOWN => c"User not known to the underlying authentication module",
        PAM_CONV_ERR => c"Conversation error",
        PAM_AUTHTOK_ERR => c"Authentication token manipulation error",
        PAM_BUF_ERR => c"Memory buffer error",
        _ => c"PAM error",
    };
    s.as_ptr()
}

/// PAM が足す環境変数はない (malloc した NULL だけの配列。呼んだ側が free する)
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pam_getenvlist(_pamh: *mut pam_handle_t) -> *mut *mut c_char {
    let p = unsafe { libc::malloc(size_of::<*mut c_char>()) } as *mut *mut c_char;
    if !p.is_null() {
        unsafe { *p = std::ptr::null_mut() };
    }
    p
}

fn field(path: &str, name: &str, i: usize) -> Option<String> {
    std::fs::read_to_string(path).ok()?.lines().find_map(|l| {
        let f: Vec<&str> = l.split(':').collect();
        (f.first() == Some(&name)).then(|| f.get(i).unwrap_or(&"").to_string())
    })
}

/// 会話関数でパスワードを聞く
unsafe fn ask_password(h: &Handle) -> Option<String> {
    let conv = h.conv.conv?;
    let msg = pam_message { msg_style: PAM_PROMPT_ECHO_OFF, msg: c"Password: ".as_ptr() };
    let mut msgs = [&msg as *const pam_message];
    let mut resp: *mut pam_response = std::ptr::null_mut();
    if unsafe { conv(1, msgs.as_mut_ptr(), &mut resp, h.conv.appdata_ptr) } != PAM_SUCCESS || resp.is_null() {
        return None;
    }
    let r = unsafe { &mut *resp };
    let pw = if r.resp.is_null() {
        None
    } else {
        let s = unsafe { CStr::from_ptr(r.resp) }.to_string_lossy().into_owned();
        // 返事は会話関数が malloc したもの。消してから返す
        unsafe {
            let len = libc::strlen(r.resp);
            std::ptr::write_bytes(r.resp, 0, len);
            libc::free(r.resp as *mut c_void);
        }
        Some(s)
    };
    unsafe { libc::free(resp as *mut c_void) };
    pw
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pam_authenticate(pamh: *mut pam_handle_t, flags: c_int) -> c_int {
    let Some(h) = (unsafe { handle(pamh) }) else { return PAM_BUF_ERR };
    let Some(user) = h.items[PAM_USER as usize].as_ref().map(|u| u.to_string_lossy().into_owned()) else { return PAM_USER_UNKNOWN };
    if field("/etc/passwd", &user, 0).is_none() {
        return PAM_USER_UNKNOWN;
    }
    let hash = field("/etc/shadow", &user, 1).unwrap_or_else(|| "!".into());
    let Some(pw) = (unsafe { ask_password(h) }) else { return PAM_CONV_ERR };
    let ok = if hash.is_empty() { flags & PAM_DISALLOW_NULL_AUTHTOK == 0 && pw.is_empty() } else { sha512crypt::verify(&pw, &hash) };
    if ok { PAM_SUCCESS } else { PAM_AUTH_ERR }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pam_acct_mgmt(pamh: *mut pam_handle_t, _flags: c_int) -> c_int {
    let Some(h) = (unsafe { handle(pamh) }) else { return PAM_BUF_ERR };
    let user = h.items[PAM_USER as usize].as_ref().map(|u| u.to_string_lossy().into_owned()).unwrap_or_default();
    if field("/etc/passwd", &user, 0).is_some() { PAM_SUCCESS } else { PAM_USER_UNKNOWN }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pam_setcred(_pamh: *mut pam_handle_t, _flags: c_int) -> c_int {
    PAM_SUCCESS
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pam_open_session(_pamh: *mut pam_handle_t, _flags: c_int) -> c_int {
    PAM_SUCCESS
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pam_close_session(_pamh: *mut pam_handle_t, _flags: c_int) -> c_int {
    PAM_SUCCESS
}

/// パスワードの変更は passwd に任せる
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pam_chauthtok(_pamh: *mut pam_handle_t, _flags: c_int) -> c_int {
    PAM_AUTHTOK_ERR
}

/// sha512-crypt ($6$) と SHA-512 (ほかのクレートに頼らない)
mod sha512crypt {
    const K: [u64; 80] = [
        0x428a2f98d728ae22, 0x7137449123ef65cd, 0xb5c0fbcfec4d3b2f, 0xe9b5dba58189dbbc, 0x3956c25bf348b538, 0x59f111f1b605d019, 0x923f82a4af194f9b, 0xab1c5ed5da6d8118,
        0xd807aa98a3030242, 0x12835b0145706fbe, 0x243185be4ee4b28c, 0x550c7dc3d5ffb4e2, 0x72be5d74f27b896f, 0x80deb1fe3b1696b1, 0x9bdc06a725c71235, 0xc19bf174cf692694,
        0xe49b69c19ef14ad2, 0xefbe4786384f25e3, 0x0fc19dc68b8cd5b5, 0x240ca1cc77ac9c65, 0x2de92c6f592b0275, 0x4a7484aa6ea6e483, 0x5cb0a9dcbd41fbd4, 0x76f988da831153b5,
        0x983e5152ee66dfab, 0xa831c66d2db43210, 0xb00327c898fb213f, 0xbf597fc7beef0ee4, 0xc6e00bf33da88fc2, 0xd5a79147930aa725, 0x06ca6351e003826f, 0x142929670a0e6e70,
        0x27b70a8546d22ffc, 0x2e1b21385c26c926, 0x4d2c6dfc5ac42aed, 0x53380d139d95b3df, 0x650a73548baf63de, 0x766a0abb3c77b2a8, 0x81c2c92e47edaee6, 0x92722c851482353b,
        0xa2bfe8a14cf10364, 0xa81a664bbc423001, 0xc24b8b70d0f89791, 0xc76c51a30654be30, 0xd192e819d6ef5218, 0xd69906245565a910, 0xf40e35855771202a, 0x106aa07032bbd1b8,
        0x19a4c116b8d2d0c8, 0x1e376c085141ab53, 0x2748774cdf8eeb99, 0x34b0bcb5e19b48a8, 0x391c0cb3c5c95a63, 0x4ed8aa4ae3418acb, 0x5b9cca4f7763e373, 0x682e6ff3d6b2b8a3,
        0x748f82ee5defb2fc, 0x78a5636f43172f60, 0x84c87814a1f0ab72, 0x8cc702081a6439ec, 0x90befffa23631e28, 0xa4506cebde82bde9, 0xbef9a3f7b2c67915, 0xc67178f2e372532b,
        0xca273eceea26619c, 0xd186b8c721c0c207, 0xeada7dd6cde0eb1e, 0xf57d4f7fee6ed178, 0x06f067aa72176fba, 0x0a637dc5a2c898a6, 0x113f9804bef90dae, 0x1b710b35131c471b,
        0x28db77f523047d84, 0x32caab7b40c72493, 0x3c9ebe0a15c9bebc, 0x431d67c49c100d4c, 0x4cc5d4becb3e42b6, 0x597f299cfc657e2a, 0x5fcb6fab3ad6faec, 0x6c44198c4a475817,
    ];

    #[derive(Clone)]
    struct Sha512 {
        h: [u64; 8],
        buf: Vec<u8>,
        len: u128,
    }

    impl Sha512 {
        fn new() -> Self {
            Sha512 {
                h: [
                    0x6a09e667f3bcc908, 0xbb67ae8584caa73b, 0x3c6ef372fe94f82b, 0xa54ff53a5f1d36f1, 0x510e527fade682d1, 0x9b05688c2b3e6c1f, 0x1f83d9abfb41bd6b, 0x5be0cd19137e2179,
                ],
                buf: Vec::new(),
                len: 0,
            }
        }

        fn block(&mut self, b: &[u8]) {
            let mut w = [0u64; 80];
            for i in 0..16 {
                w[i] = u64::from_be_bytes(b[i * 8..i * 8 + 8].try_into().unwrap());
            }
            for i in 16..80 {
                let s0 = w[i - 15].rotate_right(1) ^ w[i - 15].rotate_right(8) ^ (w[i - 15] >> 7);
                let s1 = w[i - 2].rotate_right(19) ^ w[i - 2].rotate_right(61) ^ (w[i - 2] >> 6);
                w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
            }
            let mut v = self.h;
            for i in 0..80 {
                let s1 = v[4].rotate_right(14) ^ v[4].rotate_right(18) ^ v[4].rotate_right(41);
                let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
                let t1 = v[7].wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
                let s0 = v[0].rotate_right(28) ^ v[0].rotate_right(34) ^ v[0].rotate_right(39);
                let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
                let t2 = s0.wrapping_add(maj);
                v = [t1.wrapping_add(t2), v[0], v[1], v[2], v[3].wrapping_add(t1), v[4], v[5], v[6]];
            }
            for (h, x) in self.h.iter_mut().zip(v) {
                *h = h.wrapping_add(x);
            }
        }

        fn update(&mut self, d: &[u8]) -> &mut Self {
            self.len += d.len() as u128;
            self.buf.extend_from_slice(d);
            while self.buf.len() >= 128 {
                let b: Vec<u8> = self.buf.drain(..128).collect();
                self.block(&b);
            }
            self
        }

        fn finish(&mut self) -> [u8; 64] {
            let bits = self.len * 8;
            let mut pad = vec![0x80u8];
            while (self.buf.len() + pad.len()) % 128 != 112 {
                pad.push(0);
            }
            pad.extend_from_slice(&bits.to_be_bytes());
            let len = self.len;
            self.update(&pad);
            self.len = len;
            let mut out = [0u8; 64];
            for (i, h) in self.h.iter().enumerate() {
                out[i * 8..i * 8 + 8].copy_from_slice(&h.to_be_bytes());
            }
            out
        }
    }

    const B64: &[u8] = b"./0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

    fn crypt(pw: &[u8], salt: &[u8], rounds: usize, custom: bool) -> String {
        let salt = &salt[..salt.len().min(16)];
        let b = Sha512::new().update(pw).update(salt).update(pw).finish();
        let mut a = Sha512::new();
        a.update(pw).update(salt);
        let mut n = pw.len();
        while n > 64 {
            a.update(&b);
            n -= 64;
        }
        a.update(&b[..n]);
        let mut n = pw.len();
        while n > 0 {
            if n & 1 != 0 { a.update(&b) } else { a.update(pw) };
            n >>= 1;
        }
        let a = a.finish();
        let mut dp = Sha512::new();
        for _ in 0..pw.len() {
            dp.update(pw);
        }
        let dp = dp.finish();
        let p: Vec<u8> = dp.iter().cycle().take(pw.len()).copied().collect();
        let mut ds = Sha512::new();
        for _ in 0..16 + a[0] as usize {
            ds.update(salt);
        }
        let ds = ds.finish();
        let s: Vec<u8> = ds.iter().cycle().take(salt.len()).copied().collect();
        let mut c = a.to_vec();
        for i in 0..rounds {
            let mut h = Sha512::new();
            if i & 1 != 0 { h.update(&p) } else { h.update(&c) };
            if i % 3 != 0 {
                h.update(&s);
            }
            if i % 7 != 0 {
                h.update(&p);
            }
            if i & 1 != 0 { h.update(&c) } else { h.update(&p) };
            c = h.finish().to_vec();
        }
        let mut out = String::from("$6$");
        if custom {
            out.push_str(&format!("rounds={rounds}$"));
        }
        out.push_str(&String::from_utf8_lossy(salt));
        out.push('$');
        let mut put = |b2: u8, b1: u8, b0: u8, n: usize| {
            let mut w = ((b2 as u32) << 16) | ((b1 as u32) << 8) | b0 as u32;
            for _ in 0..n {
                out.push(B64[(w & 0x3f) as usize] as char);
                w >>= 6;
            }
        };
        const ORDER: [(usize, usize, usize); 21] = [
            (0, 21, 42), (22, 43, 1), (44, 2, 23), (3, 24, 45), (25, 46, 4), (47, 5, 26), (6, 27, 48),
            (28, 49, 7), (50, 8, 29), (9, 30, 51), (31, 52, 10), (53, 11, 32), (12, 33, 54), (34, 55, 13),
            (56, 14, 35), (15, 36, 57), (37, 58, 16), (59, 17, 38), (18, 39, 60), (40, 61, 19), (62, 20, 41),
        ];
        for (x, y, z) in ORDER {
            put(c[x], c[y], c[z], 4);
        }
        put(0, 0, c[63], 2);
        out
    }

    /// shadow のハッシュ ($6$ だけ) と照らし合わせる。"!" や "*" は鍵がかかっている
    pub fn verify(pw: &str, hash: &str) -> bool {
        let Some(rest) = hash.strip_prefix("$6$") else { return false };
        let (rounds, custom, rest) = match rest.strip_prefix("rounds=") {
            Some(r) => match r.split_once('$').and_then(|(n, rest)| n.parse::<usize>().ok().map(|n| (n.clamp(1000, 999_999_999), rest))) {
                Some((n, rest)) => (n, true, rest),
                None => return false,
            },
            None => (5000, false, rest),
        };
        let salt = rest.split('$').next().unwrap_or("");
        let h = crypt(pw.as_bytes(), salt.as_bytes(), rounds, custom);
        h.len() == hash.len() && h.bytes().zip(hash.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn vectors() {
            assert!(super::verify("hello", "$6$abcdefgh$bp46WUxDu2cphMRE0PvVJlqas2imuSq186YS793XKfmqn9XUEL17HECskUTZInaNigNndLLYc7A6yWBmFEFn3/"));
            assert!(super::verify("Hello world!", "$6$rounds=10000$saltstringsaltst$OW1/O6BYHV6BcXZu8QVeXbDWra3Oeqh0sbHbbMCVNSnCM/UrjmM0Dp8vOuZeHBy/YTBmSK6H9qs/y3RnOaw5v."));
            assert!(!super::verify("hellO", "$6$abcdefgh$bp46WUxDu2cphMRE0PvVJlqas2imuSq186YS793XKfmqn9XUEL17HECskUTZInaNigNndLLYc7A6yWBmFEFn3/"));
        }
    }
}

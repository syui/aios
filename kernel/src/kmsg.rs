// カーネルのメッセージを覚えておく (Linux の dmesg と同じ)。println! はシリアルの画面にしか出ないので、
// ssh や aiwm の端末、Claude からも読めるように、出したものをここにもためる。/proc/kmsg で読む。
// 行の頭に起動してからの時間 ([    12.345678]) をつける (シリアルの画面にはつけない)
use alloc::string::String;
use alloc::vec::Vec;

/// ためる大きさ (古いものから消える)
const SIZE: usize = 256 * 1024;

struct Ring {
    buf: [u8; SIZE],
    /// いままでに書いたバイトの数 (buf の位置は head % SIZE)
    head: usize,
    /// 次に書くのが行の頭か
    bol: bool,
}

static mut RING: Ring = Ring { buf: [0; SIZE], head: 0, bol: true };

fn ring() -> &'static mut Ring {
    unsafe { &mut *(&raw mut RING) }
}

fn put(r: &mut Ring, b: u8) {
    r.buf[r.head % SIZE] = b;
    r.head += 1;
}

/// 出したものを足す (uart::Uart から。uart::LOCK を持って呼ぶ)
pub fn push(s: &str) {
    let r = ring();
    for b in s.bytes() {
        if r.bol {
            r.bol = false;
            let ns = crate::timer::uptime_ns();
            let mut t = [0u8; 32];
            let n = fmt_time(&mut t, ns);
            for &c in &t[..n] {
                put(r, c);
            }
        }
        put(r, b);
        if b == b'\n' {
            r.bol = true;
        }
    }
}

/// "[    12.345678] " を書いて長さを返す (format! はヒープを使うので、ここでは使わない)
fn fmt_time(out: &mut [u8; 32], ns: u64) -> usize {
    let (sec, us) = (ns / 1_000_000_000, ns / 1000 % 1_000_000);
    let mut digits = [0u8; 20];
    let mut n = 0;
    let mut v = sec;
    loop {
        digits[n] = b'0' + (v % 10) as u8;
        n += 1;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    let mut i = 0;
    out[i] = b'[';
    i += 1;
    for _ in n..5 {
        out[i] = b' ';
        i += 1;
    }
    for k in (0..n).rev() {
        out[i] = digits[k];
        i += 1;
    }
    out[i] = b'.';
    i += 1;
    let mut d = 100_000;
    while d > 0 {
        out[i] = b'0' + (us / d % 10) as u8;
        i += 1;
        d /= 10;
    }
    out[i] = b']';
    out[i + 1] = b' ';
    i + 2
}

/// ためてあるもの (古いものが消えて途中から始まるなら、最初の行の残りは捨てる)
pub fn text() -> String {
    let _g = crate::uart::LOCK.lock();
    let r = ring();
    let bytes: Vec<u8> = if r.head <= SIZE {
        r.buf[..r.head].to_vec()
    } else {
        let at = r.head % SIZE;
        let mut v = r.buf[at..].to_vec();
        v.extend_from_slice(&r.buf[..at]);
        match v.iter().position(|&c| c == b'\n') {
            Some(i) => v.split_off(i + 1),
            None => v,
        }
    };
    String::from_utf8_lossy(&bytes).into_owned()
}

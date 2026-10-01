// 端末の大きさを端末そのものに聞く (xterm の resize と同じやりかた)。
// シリアルのコンソールでは、つないだ先の画面の大きさがカーネルに届かない (いつも 24x80)。
// そこでカーソルを右下のはし (999,999) へ動かし、位置を聞いて (ESC [ 6 n → ESC [ 行 ; 列 R)、
// それを TIOCSWINSZ で端末に教える。答えない端末なら何もしない

/// fd の端末に聞いた (行, 列)。答えがなければ None
pub fn query_size(fd: i32) -> Option<(u16, u16)> {
    if unsafe { libc::isatty(fd) } == 0 {
        return None;
    }
    let mut old: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(fd, &mut old) } != 0 {
        return None;
    }
    // 1 文字ずつ、0.3 秒まで待って読む (エコーなし)
    let mut raw = old;
    raw.c_lflag &= !(libc::ICANON | libc::ECHO);
    raw.c_cc[libc::VMIN] = 0;
    raw.c_cc[libc::VTIME] = 3;
    unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) };
    // 位置を覚える → 右下へ → 位置を聞く → もとへ
    let ask = b"\x1b7\x1b[999;999H\x1b[6n\x1b8";
    unsafe { libc::write(fd, ask.as_ptr() as *const _, ask.len()) };
    let mut buf = Vec::new();
    let start = std::time::Instant::now();
    while buf.len() < 32 && start.elapsed() < std::time::Duration::from_secs(1) {
        let mut c = 0u8;
        if unsafe { libc::read(fd, &mut c as *mut u8 as *mut _, 1) } != 1 {
            continue;
        }
        buf.push(c);
        if c == b'R' {
            break;
        }
    }
    unsafe { libc::tcsetattr(fd, libc::TCSANOW, &old) };
    let s = String::from_utf8_lossy(&buf);
    let body = s.rsplit("\x1b[").next()?.strip_suffix('R')?;
    let (r, c) = body.split_once(';')?;
    let (rows, cols) = (r.parse::<u16>().ok()?, c.parse::<u16>().ok()?);
    // ありえない大きさ (答えをまちがえた) は使わない
    (rows >= 2 && cols >= 10).then_some((rows, cols))
}

/// 端末に聞いた大きさを fd の端末に設定する。設定した (行, 列) を返す
pub fn fit(fd: i32) -> Option<(u16, u16)> {
    let (rows, cols) = query_size(fd)?;
    let ws = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
    (unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &ws) } == 0).then_some((rows, cols))
}

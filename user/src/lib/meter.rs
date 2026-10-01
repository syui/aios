// 進み具合のメーター (pacman と同じ形)。標準出力が端末のときだけ、同じ行を書きなおす
//   名前                         44.4 MiB  5.20 MiB/s 00:03 [######--------------]  31%
// 大きさがわからないとき (total = 0) は、棒と % を出さない
use std::io::Write;
use std::time::{Duration, Instant};

pub struct Meter {
    label: String,
    total: u64,
    start: Instant,
    last: Option<Instant>,
    tty: bool,
}

impl Meter {
    /// 端末でなければ、label の 1 行だけを出す
    pub fn new(label: &str, total: u64) -> Meter {
        let tty = unsafe { libc::isatty(1) } == 1;
        if !tty {
            println!("{}", label);
        }
        Meter { label: label.to_string(), total, start: Instant::now(), last: None, tty }
    }

    pub fn set_total(&mut self, total: u64) {
        if total > 0 {
            self.total = total;
        }
    }

    /// done バイトまで進んだ (0.2 秒に 1 回くらい書く)
    pub fn update(&mut self, done: u64) {
        if !self.tty || self.last.is_some_and(|t| t.elapsed() < Duration::from_millis(200)) {
            return;
        }
        self.last = Some(Instant::now());
        self.draw(done, false);
    }

    /// 終わり: 最後の値を書いて改行
    pub fn finish(&mut self, done: u64) {
        if self.tty {
            self.draw(done, true);
            println!();
        }
    }

    fn draw(&self, done: u64, end: bool) {
        let secs = self.start.elapsed().as_secs_f64().max(0.001);
        let rate = done as f64 / secs;
        // 時間: 終わったらかかった時間、途中は残りの見込み
        let t = if end || self.total == 0 || rate <= 0.0 {
            secs
        } else {
            self.total.saturating_sub(done) as f64 / rate
        };
        let time = format!("{:02}:{:02}", (t as u64) / 60 % 100, (t as u64) % 60);
        let mut right = format!(" {:>9} {:>11} {}", size(done.max(if end { self.total } else { 0 })), format!("{}/s", size(rate as u64)), time);
        if self.total > 0 {
            let pct = (done.min(self.total) * 100 / self.total) as usize;
            let w = 20;
            let fill = pct * w / 100;
            right.push_str(&format!(" [{}{}] {:>3}%", "#".repeat(fill), "-".repeat(w - fill), pct));
        }
        let cols = cols();
        let room = cols.saturating_sub(right.chars().count() + 1).max(8);
        let mut label: String = self.label.chars().take(room).collect();
        while label.chars().count() < room {
            label.push(' ');
        }
        let mut out = std::io::stdout();
        let _ = write!(out, "\r{}{}", label, right);
        let _ = out.flush();
    }
}

/// 1.5 KiB / 12.3 MiB のように
pub fn size(n: u64) -> String {
    let units = ["B", "KiB", "MiB", "GiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i + 1 < units.len() {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{} B", n) } else { format!("{:.1} {}", v, units[i]) }
}

/// 端末の幅 (わからなければ 80)
fn cols() -> usize {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) } == 0 && ws.ws_col > 20 {
        ws.ws_col as usize
    } else {
        80
    }
}

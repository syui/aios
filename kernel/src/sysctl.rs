// /proc/sys: カーネルの値を読み書きする (Linux の sysctl と同じ場所と形)
//
//   /proc/sys/kernel/hostname                uname の nodename (sethostname と同じ)
//   /proc/sys/kernel/ostype, osrelease       読むだけ
//   /proc/sys/fs/inotify/max_queued_events   inotify にためておくできごとの数
//   /proc/sys/fs/inotify/max_user_watches    1 つの inotify の watch の数
//   /proc/sys/fs/nr_open                     読むだけ (開けるファイルの数の上限)
//   /proc/sys/vm/min_free_kbytes             空きがこれを割ったら swap へ回収する (その 4 倍まで)
//
// 書けるのは root だけ。値は 1 行 (終わりの改行や空白はなくてよい)。起動のときに init が
// /etc/sysctl.d/*.conf を読んでここに書く (aios apply の sysctl)
use alloc::format;
use alloc::string::{String, ToString};
use core::sync::atomic::{AtomicUsize, Ordering};

const EINVAL: i64 = 22;

/// inotify にためておくできごとの数。こえたら IN_Q_OVERFLOW
pub static INOTIFY_MAX_QUEUED: AtomicUsize = AtomicUsize::new(16384);
/// 1 つの inotify の watch の数
pub static INOTIFY_MAX_WATCHES: AtomicUsize = AtomicUsize::new(8192);
/// 空きがこれ (ページ) を割ったら回収する (4 MiB)。回収はこの 4 倍になるまで
pub static MIN_FREE_PAGES: AtomicUsize = AtomicUsize::new(1024);

pub struct Entry {
    /// /proc/sys の下のパス
    pub path: &'static str,
    get: fn() -> String,
    /// None なら読むだけ
    set: Option<fn(&str) -> Result<(), i64>>,
}

impl Entry {
    pub fn writable(&self) -> bool {
        self.set.is_some()
    }

    pub fn read(&self) -> String {
        let mut s = (self.get)();
        s.push('\n');
        s
    }

    pub fn write(&self, b: &[u8]) -> Result<(), i64> {
        let set = self.set.ok_or(-EINVAL)?;
        let s = core::str::from_utf8(b).map_err(|_| -EINVAL)?;
        set(s.trim_end_matches(['\n', ' ', '\t']))
    }
}

/// 数を lo..=hi の中で受ける
fn num(s: &str, lo: usize, hi: usize) -> Result<usize, i64> {
    s.trim().parse::<usize>().ok().filter(|n| (lo..=hi).contains(n)).ok_or(-EINVAL)
}

pub static TABLE: &[Entry] = &[
    Entry { path: "kernel/hostname", get: crate::syscall::hostname, set: Some(|s| crate::syscall::set_hostname(s.as_bytes())) },
    Entry { path: "kernel/ostype", get: || "aios".to_string(), set: None },
    Entry { path: "kernel/osrelease", get: || env!("AIOS_RELEASE").to_string(), set: None },
    Entry { path: "fs/inotify/max_queued_events", get: || format!("{}", INOTIFY_MAX_QUEUED.load(Ordering::Relaxed)), set: Some(|s| Ok(INOTIFY_MAX_QUEUED.store(num(s, 16, 1 << 20)?, Ordering::Relaxed))) },
    Entry { path: "fs/inotify/max_user_watches", get: || format!("{}", INOTIFY_MAX_WATCHES.load(Ordering::Relaxed)), set: Some(|s| Ok(INOTIFY_MAX_WATCHES.store(num(s, 1, 1 << 20)?, Ordering::Relaxed))) },
    Entry { path: "fs/nr_open", get: || format!("{}", crate::proc::NOFILE), set: None },
    Entry {
        path: "vm/min_free_kbytes",
        get: || format!("{}", MIN_FREE_PAGES.load(Ordering::Relaxed) * crate::memlayout::PGSIZE / 1024),
        // 64 KiB から 1 GiB
        set: Some(|s| Ok(MIN_FREE_PAGES.store(num(s, 64, 1 << 20)? * 1024 / crate::memlayout::PGSIZE, Ordering::Relaxed))),
    },
];

/// パスを / で分けたものの d 番目まで
fn prefix(path: &str, d: usize) -> impl Iterator<Item = &str> {
    path.split('/').take(d)
}

/// 表の i 番のパスの、はじめの d 個が、j 番のものと同じか
pub fn same_dir(i: usize, j: usize, d: usize) -> bool {
    TABLE[j].path.split('/').count() > d && prefix(TABLE[i].path, d).eq(prefix(TABLE[j].path, d))
}

/// ディレクトリ (i 番のはじめの d 個) の中の name。(表の番号, ファイルか)。番号はそこを通るものの最初
pub fn lookup(i: usize, d: usize, name: &str) -> Option<(usize, bool)> {
    let j = (0..TABLE.len()).find(|&j| same_dir(i, j, d) && TABLE[j].path.split('/').nth(d) == Some(name))?;
    Some((j, TABLE[j].path.split('/').count() == d + 1))
}

/// ディレクトリの中身: (名前, 表の番号, ファイルか)
pub fn list(i: usize, d: usize) -> alloc::vec::Vec<(&'static str, usize, bool)> {
    let mut v: alloc::vec::Vec<(&'static str, usize, bool)> = alloc::vec::Vec::new();
    for j in 0..TABLE.len() {
        if !same_dir(i, j, d) {
            continue;
        }
        let Some(name) = TABLE[j].path.split('/').nth(d) else { continue };
        if !v.iter().any(|x| x.0 == name) {
            v.push((name, j, TABLE[j].path.split('/').count() == d + 1));
        }
    }
    v
}

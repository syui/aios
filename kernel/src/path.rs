// パスの正規化と、シンボリックリンクをたどった initrd の探索
use crate::initrd::{self, Entry};
use alloc::string::String;
use alloc::vec::Vec;

const ENOENT: i64 = 2;
const ENOTDIR: i64 = 20;
const ELOOP: i64 = 40;

/// cwd (先頭 / なし) を基準に path を絶対化し、. と .. を畳む
pub fn normalize(cwd: &str, path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    let base = if path.starts_with('/') { "" } else { cwd };
    for c in base.split('/').chain(path.split('/')) {
        match c {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            c => parts.push(c),
        }
    }
    parts.join("/")
}

/// path を探す。途中のリンクは常に、最後のリンクは follow のときだけたどる
pub fn resolve(cwd: &str, path: &str, follow: bool) -> Result<Entry, i64> {
    let mut path = normalize(cwd, path);
    for _ in 0..16 {
        let mut cur = String::new();
        let comps: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
        let mut restart = None;
        for (i, c) in comps.iter().enumerate() {
            let parent = cur.clone();
            if !cur.is_empty() {
                cur.push('/');
            }
            cur.push_str(c);
            let e = initrd::lookup(&cur).ok_or(-ENOENT)?;
            let last = i + 1 == comps.len();
            if e.is_symlink() && (!last || follow) {
                let target = core::str::from_utf8(e.data).map_err(|_| -ENOENT)?;
                let rest = comps[i + 1..].join("/");
                let mut next = normalize(&parent, target);
                if !rest.is_empty() {
                    next.push('/');
                    next.push_str(&rest);
                }
                restart = Some(next);
                break;
            }
            if !last && !e.is_dir() {
                return Err(-ENOTDIR);
            }
        }
        match restart {
            Some(next) => path = next,
            None => return initrd::lookup(&cur).ok_or(-ENOENT),
        }
    }
    Err(-ELOOP)
}

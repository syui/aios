// 使ったパスの順位 (zoxide / fasd と同じ考え: 使った回数 × 新しさ)
//   コマンドの行に出てきた、ほんとうにあるファイルとディレクトリを覚える (preexec の行を precmd で確かめる。
//   動かしたあとにできたファイルも入る)。cd したディレクトリも
//   ~/.cache/aish/paths に 1 行 1 つの JSON { path, rank, time }。.cache なのでイメージには入らない
use aish_plugin::{Value, json};
use std::path::{Component, Path, PathBuf};

/// 順位の合計がこれを超えたら、みな 0.9 倍にして古いものを忘れる
const AGE_AT: f64 = 5000.0;
const MAX: usize = 2000;

pub struct Entry {
    pub path: String,
    rank: f64,
    time: u64,
}

pub struct Db {
    file: String,
    home: String,
    pub list: Vec<Entry>,
    /// 読んだときのファイルの時刻 (ほかの aish (人のと aish --mcp) が書いたら読みなおす)
    seen: Option<std::time::SystemTime>,
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

impl Db {
    pub fn open(home: &str, histfile: &str) -> Db {
        let file = if home.is_empty() { String::new() } else { format!("{}/.cache/aish/paths", home) };
        let mut db = Db { file, home: home.to_string(), list: Vec::new(), seen: None };
        if !db.load() {
            // はじめて: 履歴にある / か ~ で始まるパスから
            db.seed(home, histfile);
        }
        db
    }

    fn mtime(&self) -> Option<std::time::SystemTime> {
        std::fs::metadata(&self.file).and_then(|m| m.modified()).ok()
    }

    /// ファイルから読む (なければ false)
    fn load(&mut self) -> bool {
        let Ok(text) = std::fs::read_to_string(&self.file) else { return false };
        self.seen = self.mtime();
        self.list.clear();
        for l in text.lines() {
            let Ok(v) = serde_json_from(l) else { continue };
            if let Some(p) = v["path"].as_str() {
                self.list.push(Entry { path: p.to_string(), rank: v["rank"].as_f64().unwrap_or(1.0), time: v["time"].as_u64().unwrap_or(0) });
            }
        }
        true
    }

    /// ほかの aish が書きかえていたら読みなおす
    fn refresh(&mut self) {
        if self.mtime() != self.seen {
            self.load();
        }
    }

    fn seed(&mut self, home: &str, histfile: &str) {
        let Ok(hist) = std::fs::read_to_string(histfile) else { return };
        let t = std::fs::metadata(histfile).and_then(|m| m.modified()).ok().and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
        for line in hist.lines() {
            for p in words(line, home, "") {
                if p.is_absolute() && p.exists() {
                    self.bump(&p.display().to_string(), t);
                }
            }
        }
        self.save();
    }

    pub fn bump(&mut self, path: &str, t: u64) {
        match self.list.iter_mut().find(|e| e.path == path) {
            Some(e) => {
                e.rank += 1.0;
                e.time = e.time.max(t);
            }
            None => self.list.push(Entry { path: path.to_string(), rank: 1.0, time: t }),
        }
    }

    /// 行に出てきた、あるパスを覚える
    pub fn learn(&mut self, line: &str, pwd: &str, home: &str) {
        self.refresh();
        let t = now();
        let mut any = false;
        let mut seen = Vec::new();
        for p in words(line, home, pwd) {
            // / と HOME そのもの、/dev /proc /sys は覚えない (どこでも出てくる)。1 つの行で 1 回
            if !skip(&p, home) && !seen.contains(&p) && p.exists() {
                seen.push(p.clone());
                self.bump(&p.display().to_string(), t);
                any = true;
            }
        }
        if any {
            self.save();
        }
    }

    pub fn save(&mut self) {
        if self.file.is_empty() {
            return;
        }
        if self.list.iter().map(|e| e.rank).sum::<f64>() > AGE_AT {
            for e in &mut self.list {
                e.rank *= 0.9;
            }
            self.list.retain(|e| e.rank >= 1.0);
        }
        if self.list.len() > MAX {
            let t = now();
            self.list.sort_by(|a, b| b.score(t).total_cmp(&a.score(t)));
            self.list.truncate(MAX);
        }
        let mut out = String::new();
        for e in &self.list {
            out.push_str(&json!({ "path": e.path, "rank": (e.rank * 100.0).round() / 100.0, "time": e.time }).to_string());
            out.push('\n');
        }
        if let Some(d) = Path::new(&self.file).parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let _ = std::fs::write(&self.file, out);
        self.seen = self.mtime();
    }

    /// 順位の高いものから。terms はみな、この順にパスに入っていること (大文字小文字は区別しない)。
    /// kind は "file" / "dir" / "" (どちらも)。なくなったものはのぞく
    pub fn rank(&mut self, terms: &[String], kind: &str, limit: usize) -> Vec<Value> {
        self.refresh();
        let t = now();
        let mut v: Vec<&Entry> = self.list.iter().filter(|e| matches(&e.path, terms) && !skip(Path::new(&e.path), &self.home)).collect();
        v.sort_by(|a, b| b.score(t).total_cmp(&a.score(t)));
        let mut out = Vec::new();
        for e in v {
            let Ok(m) = std::fs::metadata(&e.path) else { continue };
            let k = if m.is_dir() { "dir" } else { "file" };
            if !kind.is_empty() && kind != k {
                continue;
            }
            out.push(json!({ "path": e.path, "kind": k, "score": (e.score(t) * 100.0).round() / 100.0, "time": e.time }));
            if out.len() >= limit {
                break;
            }
        }
        out
    }
}

impl Entry {
    /// 使った回数 × 新しさ (aish_plugin::frecency)
    fn score(&self, now: u64) -> f64 {
        aish_plugin::frecency(self.rank, self.time, now)
    }
}

/// 覚えないもの: / と HOME そのもの (どこでも出てくる)、/dev /proc /sys
fn skip(p: &Path, home: &str) -> bool {
    p == Path::new("/") || p == Path::new(home) || ["/dev", "/proc", "/sys"].iter().any(|d| p.starts_with(d))
}

fn matches(path: &str, terms: &[String]) -> bool {
    let p = path.to_lowercase();
    let mut at = 0;
    for t in terms {
        let t = t.to_lowercase();
        match p[at..].find(&t) {
            Some(i) => at += i + t.len(),
            None => return false,
        }
    }
    true
}

fn serde_json_from(l: &str) -> Result<Value, ()> {
    aish_plugin::parse(l).ok_or(())
}

/// 行の中の、パスかもしれない語 (pwd から見たパスにする)。コマンドの名前 (/ のないもの)、-x、$ や * のあるものはのぞく
fn words(line: &str, home: &str, pwd: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut first = true;
    let mut w = String::new();
    let flush = |w: &mut String, first: &mut bool, out: &mut Vec<PathBuf>| {
        if w.is_empty() {
            return;
        }
        let mut s = w.trim_matches(|c| c == '\'' || c == '"').to_string();
        w.clear();
        let was_first = std::mem::replace(first, false);
        if let Some(rest) = s.strip_prefix('-') {
            match rest.split_once('=') {
                Some((_, v)) => s = v.to_string(),
                None => return,
            }
        }
        if s.is_empty() || s == "." || s == ".." || s.contains(['$', '*', '?', '`']) || s.contains("://") || (was_first && !s.contains('/')) || s.contains('=') && !s.contains('/') {
            return;
        }
        let s = match s.strip_prefix('~') {
            Some(r) if r.is_empty() || r.starts_with('/') => format!("{}{}", home, r),
            _ => s,
        };
        let p = Path::new(&s);
        let p = if p.is_absolute() { p.to_path_buf() } else if pwd.is_empty() { return } else { Path::new(pwd).join(p) };
        out.push(normalize(&p));
    };
    for c in line.chars() {
        if c.is_whitespace() {
            flush(&mut w, &mut first, &mut out);
        } else if ";|&()<>".contains(c) {
            flush(&mut w, &mut first, &mut out);
            if ";|&(".contains(c) {
                first = true;
            }
        } else {
            w.push(c);
        }
    }
    flush(&mut w, &mut first, &mut out);
    out
}

/// . と .. を (ファイルシステムを見ずに) 取りのぞく
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

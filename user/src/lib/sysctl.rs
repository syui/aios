// カーネルの値 (/proc/sys) を読み書きする: sysctl コマンド、init (起動のとき)、aios (apply と get) が使う
#![allow(dead_code)]
//   名前は点でつなぐ (vm.min_free_kbytes → /proc/sys/vm/min_free_kbytes)。/ でもよい
//   起動のときに入れるものは /etc/sysctl.conf と /etc/sysctl.d/*.conf (名前の順。systemd-sysctl と同じ形):
//     # コメント
//     vm.min_free_kbytes = 8192
//     -fs.inotify.max_user_watches = 65536   (- で始まるものは、しくじっても知らせない)
use std::fs;

pub const DIR: &str = "/proc/sys";
pub const CONF: &str = "/etc/sysctl.conf";
pub const CONF_D: &str = "/etc/sysctl.d";
/// aios apply (/etc/aios.json の sysctl) が書くファイル
pub const AIOS_CONF: &str = "/etc/sysctl.d/aios.conf";

/// 名前として受けるもの (パスの外に出られないように)
pub fn ok_key(k: &str) -> bool {
    !k.is_empty() && k.split(['.', '/']).all(|c| !c.is_empty() && c != ".." && c.chars().all(|ch| ch.is_ascii_alphanumeric() || "-_".contains(ch)))
}

/// 名前 → /proc/sys のパス
pub fn path(key: &str) -> String {
    format!("{}/{}", DIR, key.replace('.', "/"))
}

/// パス → 点でつないだ名前
fn key_of(path: &str) -> String {
    path.trim_start_matches(DIR).trim_start_matches('/').replace('/', ".")
}

pub fn get(key: &str) -> Result<String, String> {
    if !ok_key(key) {
        return Err(format!("{}: bad name", key));
    }
    fs::read_to_string(path(key)).map(|s| s.trim_end().to_string()).map_err(|e| format!("{}: {}", key, e))
}

pub fn set(key: &str, val: &str) -> Result<(), String> {
    if !ok_key(key) {
        return Err(format!("{}: bad name", key));
    }
    fs::write(path(key), format!("{}\n", val)).map_err(|e| format!("{}: {}", key, e))
}

/// /proc/sys の下のぜんぶ (名前, 値)。読めないものは飛ばす
pub fn all() -> Vec<(String, String)> {
    let mut v = Vec::new();
    walk(DIR, &mut v);
    v
}

fn walk(dir: &str, v: &mut Vec<(String, String)>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    let mut es: Vec<_> = rd.flatten().map(|e| e.path()).collect();
    es.sort();
    for p in es {
        let s = p.display().to_string();
        if p.is_dir() {
            walk(&s, v);
        } else if let Ok(t) = fs::read_to_string(&p) {
            v.push((key_of(&s), t.trim_end().to_string()));
        }
    }
}

/// 設定ファイルの 1 行: (名前, 値, しくじっても知らせないか)
pub fn parse(text: &str) -> Vec<(String, String, bool)> {
    let mut v = Vec::new();
    for l in text.lines() {
        let l = l.trim();
        if l.is_empty() || l.starts_with('#') || l.starts_with(';') {
            continue;
        }
        let Some((k, val)) = l.split_once('=') else { continue };
        let (k, quiet) = match k.trim().strip_prefix('-') {
            Some(k) => (k, true),
            None => (k.trim(), false),
        };
        v.push((k.trim().to_string(), val.trim().to_string(), quiet));
    }
    v
}

/// 起動のときに入れるファイル: /etc/sysctl.conf と /etc/sysctl.d/*.conf (名前の順)
pub fn boot_files() -> Vec<String> {
    let mut files: Vec<String> = fs::read_dir(CONF_D)
        .map(|d| d.flatten().map(|e| e.path().display().to_string()).filter(|p| p.ends_with(".conf")).collect())
        .unwrap_or_default();
    files.sort();
    if std::path::Path::new(CONF).is_file() {
        files.insert(0, CONF.to_string());
    }
    files
}

/// ファイルの中身を入れる。しくじったもの (知らせるもの) の一覧
pub fn apply_file(file: &str) -> Vec<String> {
    let text = match fs::read_to_string(file) {
        Ok(t) => t,
        Err(e) => return vec![format!("{}: {}", file, e)],
    };
    parse(&text).into_iter().filter_map(|(k, val, quiet)| set(&k, &val).err().filter(|_| !quiet)).collect()
}

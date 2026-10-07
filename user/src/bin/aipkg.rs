// aipkg: pacman と同じ形式 (.pkg.tar.zst, .PKGINFO, repo の .db) を扱う小さなパッケージマネージャ
//
//   aipkg -S pkg...     リポジトリから入れる (依存も)
//   aipkg -Sy / -Syu [pkg...]   データベースの更新 / 全部を新しくする (pkg も一緒に入れる)
//   aipkg -Ss [word]    リポジトリを探す
//   aipkg -Si pkg       リポジトリのパッケージの情報
//   aipkg -U file...    パッケージファイルを入れる
//   aipkg -R pkg...     外す
//   aipkg -Q / -Qi / -Ql [pkg]  入っているもの / 情報 / ファイル一覧
//   aipkg -Qo FILE...          そのファイルのパッケージ (/ のない名前は PATH から)
//   -S --needed で、同じ版が入っているものは飛ばす
//
// 設定は /etc/aipkg.conf (pacman.conf と同じ書き方で、[repo] と Server を読む)。
// どのリポジトリも、Server のところの db は aios.db (aarch64/rust/aios.db, aarch64/c/aios.db)。
// パッケージはメモリにためずに /var/cache/aipkg へ書き (切れたら続きから)、そこから入れる
#[path = "../lib/http.rs"]
mod http;
#[path = "../lib/meter.rs"]
mod meter;
#[path = "../lib/tls.rs"]
mod tls;

use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, BufRead, BufReader, Read};
use std::process::exit;

const CONF: &str = "/etc/aipkg.conf";
const DBPATH: &str = "/var/lib/aipkg";
const CACHE: &str = "/var/cache/aipkg";
/// Server のところにあるデータベースの名前 (リポジトリの名前によらない)
const DB_FILE: &str = "aios.db";
const ROOT: &str = "/";
const ARCH: &str = "aarch64";

type Desc = BTreeMap<String, Vec<String>>;

fn die(msg: impl std::fmt::Display) -> ! {
    eprintln!("error: {}", msg);
    exit(1)
}

/// %KEY%\n値\n値\n\n の並び (pacman の desc 形式)
fn parse_desc(s: &str) -> Desc {
    let mut d = Desc::new();
    let mut key: Option<String> = None;
    for line in s.lines() {
        if line.len() > 2 && line.starts_with('%') && line.ends_with('%') {
            key = Some(line[1..line.len() - 1].to_string());
            d.entry(key.clone().unwrap()).or_default();
        } else if line.is_empty() {
            key = None;
        } else if let Some(k) = &key {
            d.get_mut(k).unwrap().push(line.to_string());
        }
    }
    d
}

fn write_desc(d: &Desc) -> String {
    let mut s = String::new();
    for (k, vs) in d {
        s.push_str(&format!("%{}%\n", k));
        for v in vs {
            s.push_str(v);
            s.push('\n');
        }
        s.push('\n');
    }
    s
}

fn get<'a>(d: &'a Desc, k: &str) -> &'a str {
    d.get(k).and_then(|v| v.first()).map_or("", |s| s.as_str())
}

fn list<'a>(d: &'a Desc, k: &str) -> &'a [String] {
    d.get(k).map_or(&[], |v| v.as_slice())
}

/// 依存の書き方 (name>=1.0 など) から名前だけ
fn dep_name(dep: &str) -> &str {
    dep.split(['<', '>', '=']).next().unwrap_or(dep)
}

/// .PKGINFO (key = value) を desc の形に
fn parse_pkginfo(s: &str) -> Desc {
    let map = [
        ("pkgname", "NAME"),
        ("pkgbase", "BASE"),
        ("pkgver", "VERSION"),
        ("pkgdesc", "DESC"),
        ("url", "URL"),
        ("builddate", "BUILDDATE"),
        ("packager", "PACKAGER"),
        ("size", "SIZE"),
        ("arch", "ARCH"),
        ("license", "LICENSE"),
        ("depend", "DEPENDS"),
        ("provides", "PROVIDES"),
        ("conflict", "CONFLICTS"),
        ("replaces", "REPLACES"),
        ("backup", "BACKUP"),
    ];
    let mut d = Desc::new();
    for line in s.lines() {
        let Some((k, v)) = line.split_once(" = ") else { continue };
        if let Some((_, key)) = map.iter().find(|(pk, _)| *pk == k.trim()) {
            d.entry(key.to_string()).or_default().push(v.trim().to_string());
        }
    }
    d
}

// ---- バージョン比較 (pacman の vercmp とだいたい同じ) ----

fn split_evr(v: &str) -> (u64, &str, &str) {
    let (epoch, rest) = match v.split_once(':') {
        Some((e, r)) => (e.parse().unwrap_or(0), r),
        None => (0, v),
    };
    match rest.rsplit_once('-') {
        Some((ver, rel)) => (epoch, ver, rel),
        None => (epoch, rest, ""),
    }
}

fn rpmvercmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    let seg = |s: &str| -> Vec<String> {
        let mut out = vec![];
        let mut cur = String::new();
        let mut digit = None;
        for c in s.chars() {
            if !c.is_ascii_alphanumeric() {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                digit = None;
                continue;
            }
            let d = c.is_ascii_digit();
            if digit.is_some_and(|x| x != d) {
                out.push(std::mem::take(&mut cur));
            }
            digit = Some(d);
            cur.push(c);
        }
        if !cur.is_empty() {
            out.push(cur);
        }
        out
    };
    let (sa, sb) = (seg(a), seg(b));
    for (x, y) in sa.iter().zip(sb.iter()) {
        let (xd, yd) = (x.as_bytes()[0].is_ascii_digit(), y.as_bytes()[0].is_ascii_digit());
        let o = match (xd, yd) {
            (true, true) => {
                let (x, y) = (x.trim_start_matches('0'), y.trim_start_matches('0'));
                x.len().cmp(&y.len()).then(x.cmp(y))
            }
            (true, false) => Greater,
            (false, true) => Less,
            (false, false) => x.cmp(y),
        };
        if o != Equal {
            return o;
        }
    }
    sa.len().cmp(&sb.len())
}

fn vercmp(a: &str, b: &str) -> std::cmp::Ordering {
    let (ea, va, ra) = split_evr(a);
    let (eb, vb, rb) = split_evr(b);
    ea.cmp(&eb).then_with(|| rpmvercmp(va, vb)).then_with(|| {
        if ra.is_empty() || rb.is_empty() { std::cmp::Ordering::Equal } else { rpmvercmp(ra, rb) }
    })
}

// ---- 設定とリポジトリ ----

struct Repo {
    name: String,
    servers: Vec<String>,
}

fn read_conf() -> Vec<Repo> {
    parse_conf(CONF)
}

fn parse_conf(path: &str) -> Vec<Repo> {
    let text = fs::read_to_string(path).unwrap_or_default();
    let mut repos: Vec<Repo> = vec![];
    for line in text.lines().map(str::trim) {
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            if name != "options" {
                repos.push(Repo { name: name.to_string(), servers: vec![] });
            }
            continue;
        }
        if let (Some((k, v)), Some(r)) = (line.split_once('='), repos.last_mut()) {
            if k.trim() == "Server" {
                r.servers.push(v.trim().replace("$repo", &r.name).replace("$arch", ARCH));
            }
        }
    }
    repos
}

fn fetch(url: &str) -> io::Result<Vec<u8>> {
    if let Some(path) = url.strip_prefix("file://") {
        return fs::read(path);
    }
    http::get(url, Some(tls::connect))
}

/// url を path へ (http(s) は続きから取れる download、file:// は写す)
fn fetch_to(url: &str, path: &str, progress: &mut dyn FnMut(u64, u64)) -> io::Result<()> {
    if let Some(src) = url.strip_prefix("file://") {
        return fs::copy(src, path).map(|n| progress(n, n));
    }
    http::download(url, Some(tls::connect), path, progress)
}

/// servers のどれかから file を path へ
fn fetch_any_to(servers: &[String], file: &str, path: &str, progress: &mut dyn FnMut(u64, u64)) -> io::Result<()> {
    let mut last = io::Error::other("no Server configured");
    for s in servers {
        match fetch_to(&format!("{}/{}", s.trim_end_matches('/'), file), path, &mut *progress) {
            Ok(()) => return Ok(()),
            Err(e) => last = e,
        }
    }
    Err(last)
}

fn fetch_any(servers: &[String], file: &str) -> io::Result<Vec<u8>> {
    let mut last = io::Error::other("no Server configured");
    for s in servers {
        match fetch(&format!("{}/{}", s.trim_end_matches('/'), file)) {
            Ok(b) => return Ok(b),
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// zstd / gzip / 無圧縮の tar (頭の数バイトで見分ける)
fn tar_reader<'a>(mut r: Box<dyn BufRead + 'a>) -> Box<dyn Read + 'a> {
    let head = r.fill_buf().map(|b| b.get(..4).map(|h| h.to_vec()).unwrap_or_default()).unwrap_or_default();
    match head.as_slice() {
        [0x28, 0xb5, 0x2f, 0xfd] => match ruzstd::decoding::StreamingDecoder::new(r) {
            Ok(d) => Box::new(d),
            Err(e) => die(format!("bad zstd data: {}", e)),
        },
        [0x1f, 0x8b, ..] => Box::new(flate2::read::GzDecoder::new(r)),
        _ => Box::new(r),
    }
}

/// 読んだバイト数を数えて progress(合わせて何バイト目か) を呼ぶ
struct Count<'a, R: Read> {
    r: R,
    n: u64,
    progress: &'a mut dyn FnMut(u64),
}

impl<R: Read> Read for Count<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let k = self.r.read(buf)?;
        self.n += k as u64;
        (self.progress)(self.n);
        Ok(k)
    }
}

/// パッケージのファイルを tar として開く。読み進めるたびに progress(読んだバイト数) を呼ぶ
fn open_pkg<'a>(path: &str, progress: &'a mut dyn FnMut(u64)) -> io::Result<Box<dyn Read + 'a>> {
    let f = fs::File::open(path)?;
    let c = Count { r: f, n: 0, progress };
    Ok(tar_reader(Box::new(BufReader::with_capacity(256 * 1024, c))))
}

/// ファイルの sha256 (少しずつ読む)
fn sha256_file(path: &str) -> io::Result<String> {
    let mut f = fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|x| format!("{:02x}", x)).collect())
}

/// 同期データベース (repo.db) の中身: パッケージ名 → desc
fn sync_db(repo: &str) -> BTreeMap<String, Desc> {
    let mut out = BTreeMap::new();
    let Ok(bytes) = fs::read(format!("{}/sync/{}.db", DBPATH, repo)) else { return out };
    let mut ar = tar::Archive::new(tar_reader(Box::new(io::Cursor::new(bytes))));
    let Ok(entries) = ar.entries() else { return out };
    for e in entries.flatten() {
        let mut e = e;
        let path = e.path().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
        if path.ends_with("/desc") {
            let mut s = String::new();
            if e.read_to_string(&mut s).is_ok() {
                let d = parse_desc(&s);
                out.insert(get(&d, "NAME").to_string(), d);
            }
        }
    }
    out
}

/// パッケージ名 → (リポジトリ, desc)。先に書いたリポジトリが勝つ
fn sync_all(repos: &[Repo]) -> BTreeMap<String, (usize, Desc)> {
    let mut out = BTreeMap::new();
    for (i, r) in repos.iter().enumerate() {
        for (name, d) in sync_db(&r.name) {
            out.entry(name).or_insert((i, d));
        }
    }
    out
}

// ---- ローカルデータベース ----

fn local_dir(name: &str, ver: &str) -> String {
    format!("{}/local/{}-{}", DBPATH, name, ver)
}

/// 入っているもの: 名前 → (desc, ファイル一覧)
fn installed() -> BTreeMap<String, (Desc, Vec<String>)> {
    let mut out = BTreeMap::new();
    let Ok(rd) = fs::read_dir(format!("{}/local", DBPATH)) else { return out };
    for e in rd.flatten() {
        let dir = e.path();
        let Ok(desc) = fs::read_to_string(dir.join("desc")) else { continue };
        let d = parse_desc(&desc);
        let files = fs::read_to_string(dir.join("files")).map(|f| list(&parse_desc(&f), "FILES").to_vec()).unwrap_or_default();
        out.insert(get(&d, "NAME").to_string(), (d, files));
    }
    out
}

// ---- 入れる / 外す ----

/// パッケージを入れる。中身は 1 回だけ読む (先頭の .PKGINFO を読んでから、ファイルを順に展開する)。
/// ほかのパッケージのファイルとぶつかったら、ここで作ったファイルを消して止まる
fn install(path: &str, explicit: bool) {
    let size = fs::metadata(path).map_or(0, |m| m.len());
    let read = std::cell::Cell::new(0u64);
    let mut progress = |n: u64| read.set(n);
    let mut ar = tar::Archive::new(open_pkg(path, &mut progress).unwrap_or_else(|e| die(format!("{}: {}", path, e))));
    ar.set_preserve_permissions(true);
    ar.set_preserve_mtime(true);
    ar.set_overwrite(true);

    // .PKGINFO を読んでから決まるもの
    struct Ready<'a> {
        info: Desc,
        name: String,
        ver: String,
        old: Option<&'a (Desc, Vec<String>)>,
        /// ほかのパッケージのファイル → そのパッケージ
        owners: BTreeMap<&'a str, &'a str>,
        backup: BTreeSet<String>,
        old_hashes: BTreeMap<String, String>,
        meter: meter::Meter,
    }
    let db = installed();
    let mut ready: Option<Ready> = None;
    let mut files: Vec<String> = vec![];
    let mut created: Vec<String> = vec![];
    // 新しい版が置いたリンク (古い版だけのファイルを消すとき、この先はたどらない)
    let mut new_links: Vec<String> = vec![];
    let mut new_backup = vec![];
    // .INSTALL (post_install などの関数)
    let mut script: Option<String> = None;
    let fail = |created: &[String], msg: String| -> ! {
        // メーターの行のあとに出す
        if unsafe { libc::isatty(1) } == 1 {
            println!();
        }
        // 作りかけのファイルを片付ける (前からあったものは触らない)
        for f in created.iter().rev() {
            let p = format!("{}{}", ROOT, f);
            let _ = if f.ends_with('/') { fs::remove_dir(&p) } else { fs::remove_file(&p) };
        }
        die(msg)
    };
    for e in ar.entries().unwrap_or_else(|e| die(e)) {
        let mut e = e.unwrap_or_else(|e| fail(&created, format!("{}: {}", path, e)));
        let epath = e.path().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
        if epath.starts_with('.') {
            if epath == ".PKGINFO" {
                let mut s = String::new();
                e.read_to_string(&mut s).unwrap_or_else(|err| die(format!("{}: {}", path, err)));
                let info = parse_pkginfo(&s);
                let name = get(&info, "NAME").to_string();
                let ver = get(&info, "VERSION").to_string();
                let arch = get(&info, "ARCH");
                if arch != ARCH && arch != "any" {
                    die(format!("{}: package is for {}, not {}", name, arch, ARCH));
                }
                // replaces: -U でも、置きかえられる古いパッケージ (入っていれば) を先に外す (-S と同じ)
                let dep_n = |d: &String| dep_name(d).to_string();
                let replaced: Vec<String> = list(&info, "REPLACES").iter().map(dep_n).filter(|r| *r != name && db.contains_key(r)).collect();
                if !replaced.is_empty() {
                    println!(":: replacing {} with {}", replaced.join(", "), name);
                    remove_with(&replaced, false);
                    return install(path, explicit);
                }
                // conflicts: いっしょに入れられないもの (どちらに書いてあっても)。広げはじめる前に止める
                if let Some(c) = list(&info, "CONFLICTS").iter().map(dep_n).find(|c| *c != name && db.contains_key(c)) {
                    die(format!("{}: conflicts with {} (installed). remove it first, or keep {}", name, c, c));
                }
                if let Some((c, _)) = db.iter().find(|(o, (od, _))| **o != name && list(od, "CONFLICTS").iter().any(|x| dep_name(x) == name)) {
                    die(format!("{}: {} (installed) conflicts with it", name, c));
                }
                let old = db.get(&name);
                let mut owners = BTreeMap::new();
                for (other, (_, ofiles)) in &db {
                    if *other != name {
                        for f in ofiles.iter().filter(|f| !f.ends_with('/')) {
                            owners.insert(f.as_str(), other.as_str());
                        }
                    }
                }
                let label = match old {
                    Some((od, _)) => format!("upgrading {} ({} -> {})", name, get(od, "VERSION"), ver),
                    None => format!("installing {} ({})", name, ver),
                };
                let backup = list(&info, "BACKUP").iter().cloned().collect();
                let old_hashes = old
                    .map(|(od, _)| list(od, "BACKUP").iter().filter_map(|b| b.split_once('\t')).map(|(a, b)| (a.to_string(), b.to_string())).collect())
                    .unwrap_or_default();
                ready = Some(Ready { info, name, ver, old, owners, backup, old_hashes, meter: meter::Meter::new(&label, size) });
            } else if epath == ".INSTALL" {
                let mut s = String::new();
                if e.read_to_string(&mut s).is_ok() {
                    script = Some(s);
                }
            }
            continue;
        }
        let Some(r) = ready.as_mut() else { die(format!("{}: no .PKGINFO before the files", path)) };
        r.meter.update(read.get());
        let mut rel = epath.trim_start_matches("./").to_string();
        if e.header().entry_type().is_dir() && !rel.ends_with('/') {
            rel.push('/');
        }
        if let Some(other) = r.owners.get(rel.as_str()) {
            let msg = format!("{}: /{} exists in {}", r.name, rel, other);
            fail(&created, msg);
        }
        files.push(rel.clone());
        let dest = format!("{}{}", ROOT, rel.trim_end_matches('/'));
        if e.header().entry_type().is_symlink() {
            new_links.push(rel.clone());
        }
        // 種類がかわった (古い版のディレクトリがリンクに、リンクがディレクトリに。xkeyboard-config 2.48 のように
        // 配布元が置き場所を変えたとき): 古い版のもの (か、だれのものでもないリンク) なら外してから広げる。
        // ほかのパッケージのものはそのまま (広げられずに止まる)
        if let Ok(m) = fs::symlink_metadata(&dest) {
            let is_dir = e.header().entry_type().is_dir();
            let key = rel.trim_end_matches('/');
            if is_dir && m.file_type().is_symlink() && !r.owners.contains_key(key) {
                let _ = fs::remove_file(&dest);
            } else if !is_dir && m.is_dir() {
                let old_files: BTreeSet<&str> = r.old.map(|(_, of)| of.iter().map(|f| f.trim_end_matches('/')).collect()).unwrap_or_default();
                if only_these(&dest, key, &old_files) {
                    let _ = fs::remove_dir_all(&dest);
                }
            }
        }
        if fs::symlink_metadata(&dest).is_err() {
            created.push(rel.clone());
        }
        if r.backup.contains(&epath) {
            let mut data = vec![];
            e.read_to_end(&mut data).unwrap_or_else(|err| fail(&created, format!("{}: /{}: {}", r.name, epath, err)));
            let mode = e.header().mode().unwrap_or(0o644);
            let hash = sha256_hex(&data);
            let old_hash = r.old_hashes.get(&epath).map(String::as_str);
            // 今あるものが新しい版とも、前に入れた版とも違えば、手で変えたもの
            let changed = fs::read(&dest).is_ok_and(|cur| {
                let h = sha256_hex(&cur);
                h != hash && old_hash != Some(h.as_str())
            });
            // 手で変えていても、パッケージの版が前と同じなら今のものをそのまま使う (pacman と同じ)
            if changed && old_hash == Some(hash.as_str()) {
                new_backup.push(format!("{}\t{}", epath, hash));
                continue;
            }
            let target = if changed {
                println!("warning: /{} installed as /{}.pacnew", epath, epath);
                format!("{}.pacnew", dest)
            } else {
                dest
            };
            write_file(&target, &data, mode).unwrap_or_else(|err| fail(&created, format!("{}: {}: {}", r.name, target, err)));
            new_backup.push(format!("{}\t{}", epath, hash));
            continue;
        }
        // ハードリンク (gzip の gunzip → uncompress、git など): 入れなおしや更新で同じ名前があると、tar の
        // unpack は作れずに止まる (ふつうのファイルは上書きするのに)。先に外す
        if e.header().entry_type().is_hard_link() && fs::symlink_metadata(&dest).is_ok_and(|m| !m.is_dir()) {
            let _ = fs::remove_file(&dest);
        }
        if let Err(err) = e.unpack_in(ROOT) {
            let msg = format!("{}: /{}: {}", r.name, epath, err);
            fail(&created, msg);
        }
    }
    drop(ar);
    let Some(Ready { info, name, ver, old, mut meter, .. }) = ready else { die(format!("{}: no .PKGINFO in package", path)) };
    meter.finish(size);

    let old_ver = old.map(|(od, _)| get(od, "VERSION").to_string());
    // 古い版にだけあったファイルを消す
    if let Some((od, ofiles)) = old {
        let keep: BTreeSet<&String> = files.iter().collect();
        // 新しい版がリンクにしたところの下は、たどると新しい版のファイルなので消さない
        let under_link = |f: &String| new_links.iter().any(|l| f.starts_with(&format!("{}/", l)));
        remove_files(ofiles.iter().filter(|f| !keep.contains(f) && !under_link(f)));
        let _ = fs::remove_dir_all(local_dir(&name, get(od, "VERSION")));
    }

    let mut desc = info.clone();
    desc.insert("INSTALLDATE".into(), vec![now().to_string()]);
    // BACKUP は「パス<TAB>入れたときの sha256」で覚えておく (次の更新と削除で比べる)
    if !new_backup.is_empty() {
        desc.insert("BACKUP".into(), new_backup);
    }
    let reason = match old {
        Some((od, _)) => get(od, "REASON").to_string(),
        None if explicit => "0".into(),
        None => "1".into(),
    };
    desc.insert("REASON".into(), vec![reason]);
    let dir = local_dir(&name, &ver);
    fs::create_dir_all(&dir).unwrap_or_else(|e| die(format!("{}: {}", dir, e)));
    fs::write(format!("{}/desc", dir), write_desc(&desc)).unwrap_or_else(|e| die(e));
    let mut fd = Desc::new();
    fd.insert("FILES".into(), files);
    fs::write(format!("{}/files", dir), write_desc(&fd)).unwrap_or_else(|e| die(e));
    // .INSTALL は pacman と同じく local の install に残す (消すときの pre_remove / post_remove のため)
    if let Some(s) = script {
        let at = format!("{}/install", dir);
        fs::write(&at, s).unwrap_or_else(|e| die(e));
        match &old_ver {
            Some(o) => run_script(&at, "post_upgrade", &[&ver, o]),
            None => run_script(&at, "post_install", &[&ver]),
        }
    }
}

/// .INSTALL の関数 (post_install など) を、あれば動かす (pacman と同じく bash の書き方。brush で、なければ sh で)。
/// しくじっても入れたもの・消したものはそのまま (pacman と同じ)
fn run_script(script: &str, func: &str, args: &[&str]) {
    let Ok(text) = fs::read_to_string(script) else { return };
    let defined = text.lines().any(|l| {
        let l = l.trim_start();
        l.strip_prefix(func).is_some_and(|r| r.trim_start().starts_with("()")) || l.strip_prefix("function ").is_some_and(|r| r.trim_start().starts_with(func))
    });
    if !defined || ROOT != "/" {
        return;
    }
    let shell = ["/usr/bin/brush", "/bin/brush", "/bin/sh"].into_iter().find(|p| fs::metadata(p).is_ok()).unwrap_or("/bin/sh");
    println!(":: running {}", func);
    let st = std::process::Command::new(shell).arg("-c").arg(format!(". \"$0\"; {} \"$@\"", func)).arg(script).args(args).status();
    if !st.is_ok_and(|s| s.success()) {
        println!("warning: {} of {} failed", func, script);
    }
}

fn sha256_hex(b: &[u8]) -> String {
    Sha256::digest(b).iter().map(|x| format!("{:02x}", x)).collect()
}

/// 別名に書いてから置きかえる (途中で止まっても半端なファイルを残さない)
fn write_file(path: &str, data: &[u8], mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let tmp = format!("{}.aipkg-new", path);
    fs::write(&tmp, data)?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(mode & 0o7777))?;
    fs::rename(&tmp, path)
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// 基本のディレクトリ (Arch では filesystem パッケージが持つもの)。空になっても消さない
const KEEP_DIRS: &[&str] = &[
    "bin/", "etc/", "home/", "lib/", "opt/", "root/", "run/", "srv/", "tmp/", "var/", "var/lib/", "var/cache/", "var/log/",
    "var/lib/aipkg/", "var/lib/aipkg/local/", "var/cache/aipkg/",
    "usr/", "usr/bin/", "usr/lib/", "usr/share/", "usr/share/licenses/", "usr/local/",
];

/// ファイルを消し、空になったディレクトリも片付ける
/// dir (パッケージの中の名前は rel) の下にあるものが、みな files (古い版のファイル) か。空でもよい
fn only_these(dir: &str, rel: &str, files: &BTreeSet<&str>) -> bool {
    let Ok(rd) = fs::read_dir(dir) else { return false };
    rd.flatten().all(|e| {
        let name = e.file_name().to_string_lossy().into_owned();
        let r = format!("{}/{}", rel, name);
        let is_dir = e.file_type().is_ok_and(|t| t.is_dir());
        files.contains(r.as_str()) && (!is_dir || only_these(&format!("{}/{}", dir, name), &r, files))
    })
}

fn remove_files<'a>(files: impl Iterator<Item = &'a String>) {
    let mut dirs = vec![];
    for f in files {
        let p = format!("{}{}", ROOT, f);
        if f.ends_with('/') {
            if !KEEP_DIRS.contains(&f.as_str()) {
                dirs.push(p);
            }
        } else {
            let _ = fs::remove_file(&p);
        }
    }
    dirs.sort_by_key(|d| std::cmp::Reverse(d.len()));
    for d in dirs {
        let _ = fs::remove_dir(&d);
    }
}

fn remove(names: &[String]) {
    remove_with(names, true);
}

/// check_deps: ほかのパッケージが依存していれば止める (replaces で置きかえるときは見ない。
/// 新しいパッケージが provides で同じ名前を持つ)
fn remove_with(names: &[String], check_deps: bool) {
    let db = installed();
    for name in names {
        let Some((d, files)) = db.get(name) else { die(format!("target not found: {}", name)) };
        for (other, (od, _)) in db.iter().filter(|_| check_deps) {
            if !names.contains(other) && list(od, "DEPENDS").iter().any(|x| dep_name(x) == name) {
                die(format!("{} is required by {}", name, other));
            }
        }
        println!("removing {} ({})", name, get(d, "VERSION"));
        // 手で変えた設定ファイルは .pacsave として残す
        for (path, hash) in list(d, "BACKUP").iter().filter_map(|b| b.split_once('\t')) {
            let p = format!("{}{}", ROOT, path);
            if fs::read(&p).is_ok_and(|cur| sha256_hex(&cur) != hash) && fs::rename(&p, format!("{}.pacsave", p)).is_ok() {
                println!("warning: /{} saved as /{}.pacsave", path, path);
            }
        }
        let ver = get(d, "VERSION");
        let script = format!("{}/install", local_dir(name, ver));
        // 消すまえに pre_remove。post_remove は local を消すまえに、とっておいた中身で
        let saved = fs::read_to_string(&script).ok();
        run_script(&script, "pre_remove", &[ver]);
        remove_files(files.iter());
        let _ = fs::remove_dir_all(local_dir(name, ver));
        if let Some(text) = saved {
            let tmp = format!("/tmp/aipkg-{}.install", name);
            if fs::write(&tmp, text).is_ok() {
                run_script(&tmp, "post_remove", &[ver]);
                let _ = fs::remove_file(&tmp);
            }
        }
    }
}

// ---- 同期 ----

fn refresh(repos: &[Repo]) {
    fs::create_dir_all(format!("{}/sync", DBPATH)).unwrap_or_else(|e| die(e));
    for r in repos {
        match fetch_any(&r.servers, DB_FILE) {
            Ok(b) => {
                fs::write(format!("{}/sync/{}.db", DBPATH, r.name), b).unwrap_or_else(|e| die(e));
                println!(":: {} is up to date", r.name);
            }
            Err(e) => eprintln!("error: failed to update {} ({})", r.name, e),
        }
    }
}

/// targets を依存が先に来る順に並べる
fn resolve(targets: &[String], sync: &BTreeMap<String, (usize, Desc)>) -> Vec<String> {
    fn visit(n: &str, sync: &BTreeMap<String, (usize, Desc)>, local: &BTreeSet<String>, seen: &mut BTreeSet<String>, out: &mut Vec<String>, top: bool) {
        if !seen.insert(n.to_string()) {
            return;
        }
        if !top && local.contains(n) {
            return;
        }
        let Some((_, d)) = sync.get(n) else { die(format!("target not found: {}", n)) };
        for dep in list(d, "DEPENDS") {
            visit(dep_name(dep), sync, local, seen, out, false);
        }
        out.push(n.to_string());
    }
    // 入っているものと、それが provides で持つ名前 (unix を置きかえた aikernel など)
    let mut local = BTreeSet::new();
    for (n, (d, _)) in installed() {
        local.extend(list(&d, "PROVIDES").iter().map(|p| dep_name(p).to_string()));
        local.insert(n);
    }
    let mut seen = BTreeSet::new();
    let mut out = vec![];
    for t in targets {
        visit(t, sync, &local, &mut seen, &mut out, true);
    }
    out
}

fn sync_install(repos: &[Repo], targets: &[String], explicit: &[String]) {
    let sync = sync_all(repos);
    // まだデータベースを取ってきていないリポジトリがあれば、-Sy をすすめる
    if let Some(t) = targets.iter().find(|t| !sync.contains_key(*t)) {
        let missing: Vec<&str> =
            repos.iter().filter(|r| fs::metadata(format!("{}/sync/{}.db", DBPATH, r.name)).is_err()).map(|r| r.name.as_str()).collect();
        if !missing.is_empty() {
            die(format!("target not found: {} (no database for {} yet; run aipkg -Sy)", t, missing.join(", ")));
        }
        // 設定を書きかえていると、更新で増えたリポジトリは /etc/aipkg.conf.pacnew にだけある
        let new: Vec<String> = parse_conf(&format!("{}.pacnew", CONF))
            .into_iter()
            .filter(|n| !repos.iter().any(|r| r.name == n.name))
            .map(|n| format!("[{}]", n.name))
            .collect();
        if !new.is_empty() {
            die(format!(
                "target not found: {} ({} is not in {}; see {}.pacnew, add it and run aipkg -Sy)",
                t,
                new.join(", "),
                CONF,
                CONF
            ));
        }
    }
    let order = resolve(targets, &sync);
    if order.is_empty() {
        println!(" there is nothing to do");
        return;
    }
    let mut line = String::from("Packages:");
    for n in &order {
        line.push_str(&format!(" {}-{}", n, get(&sync[n].1, "VERSION")));
    }
    println!("{}", line);
    fs::create_dir_all(CACHE).unwrap_or_else(|e| die(e));
    for n in &order {
        let (ri, d) = &sync[n];
        let file = get(d, "FILENAME");
        let want = get(d, "SHA256SUM");
        let path = format!("{}/{}", CACHE, file);
        // キャッシュにあってチェックサムが合えば、それを使う
        let cached = !want.is_empty() && sha256_file(&path).is_ok_and(|h| h == want);
        if !cached {
            let csize = get(d, "CSIZE").parse().unwrap_or(0);
            let mut m = meter::Meter::new(file, csize);
            let mut done = 0;
            let r = fetch_any_to(&repos[*ri].servers, file, &path, &mut |n, total| {
                m.set_total(total);
                m.update(n);
                done = n;
            });
            m.finish(done);
            r.unwrap_or_else(|e| die(format!("{}: {}", file, e)));
            if !want.is_empty() && sha256_file(&path).ok().as_deref() != Some(want) {
                let _ = fs::remove_file(&path);
                die(format!("{}: checksum mismatch", file));
            }
        }
        // replaces: 置きかえられる古いパッケージ (入っていれば) を外してから入れる (同じファイルを持っているので)
        let db = installed();
        let old: Vec<String> = list(d, "REPLACES").iter().map(|r| dep_name(r).to_string()).filter(|r| r != n && db.contains_key(r)).collect();
        if !old.is_empty() {
            println!(":: replacing {} with {}", old.join(", "), n);
            remove_with(&old, false);
        }
        install(&path, explicit.contains(n) || old.iter().any(|o| db.get(o).is_some_and(|(od, _)| get(od, "REASON") != "1")));
    }
}

/// -Su: 新しい版のあるものを全部。名前を渡されたら、それも一緒に入れる (pacman -Syu pkg と同じ)
fn upgrade_all(repos: &[Repo], extra: &[String]) {
    let sync = sync_all(repos);
    let mut targets = vec![];
    for (name, (d, _)) in installed() {
        if let Some((_, sd)) = sync.get(&name) {
            if vercmp(get(sd, "VERSION"), get(&d, "VERSION")) == std::cmp::Ordering::Greater {
                targets.push(name);
            }
        }
    }
    // replaces: 入っているものを置きかえる新しいパッケージ (unix → aikernel のような名前の変更)
    let local = installed();
    for (name, (_, sd)) in &sync {
        if !local.contains_key(name) && list(sd, "REPLACES").iter().any(|r| local.contains_key(dep_name(r))) && !targets.contains(name) {
            targets.push(name.clone());
        }
    }
    // まだどのリポジトリにもない名前は、更新で増えるリポジトリにあるかもしれないので後で入れる
    let (now, later): (Vec<String>, Vec<String>) = extra.iter().cloned().partition(|t| sync.contains_key(t));
    for t in &now {
        if !targets.contains(t) {
            targets.push(t.clone());
        }
    }
    if targets.is_empty() && later.is_empty() {
        println!(" there is nothing to do");
        return;
    }
    if !targets.is_empty() {
        sync_install(repos, &targets, &now);
    }
    let repos = refresh_new(repos);
    if !later.is_empty() {
        sync_install(&repos, &later, &later);
    }
}

/// 更新で /etc/aipkg.conf が新しくなり、リポジトリが増えていたら、そのデータベースも取ってくる
/// (aipkg -Syu の一度で、base の新しい設定にある [desktop] なども使えるように)
fn refresh_new(old: &[Repo]) -> Vec<Repo> {
    let repos = read_conf();
    let new: Vec<Repo> = repos
        .iter()
        .filter(|r| !old.iter().any(|o| o.name == r.name) || fs::metadata(format!("{}/sync/{}.db", DBPATH, r.name)).is_err())
        .map(|r| Repo { name: r.name.clone(), servers: r.servers.clone() })
        .collect();
    if !new.is_empty() {
        let names: Vec<String> = new.iter().map(|r| format!("[{}]", r.name)).collect();
        println!(":: new repository {} in {}", names.join(", "), CONF);
        refresh(&new);
    }
    repos
}

fn info(d: &Desc, repo: Option<&str>) {
    if let Some(r) = repo {
        println!("Repository      : {}", r);
    }
    let rows = [
        ("Name", "NAME"),
        ("Version", "VERSION"),
        ("Description", "DESC"),
        ("Architecture", "ARCH"),
        ("URL", "URL"),
        ("Licenses", "LICENSE"),
        ("Depends On", "DEPENDS"),
        ("Packager", "PACKAGER"),
    ];
    for (label, k) in rows {
        let v = list(d, k).join("  ");
        println!("{:<16}: {}", label, if v.is_empty() { "None" } else { &v });
    }
    println!();
}

fn main() {
    // 読み手のいないパイプに書いたら、ほかのコマンドと同じように静かに終わる
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(op) = args.first().filter(|a| a.starts_with('-') && !a.starts_with("--")) else {
        eprintln!("usage: aipkg -S|-Sy|-Syu|-Ss|-Si|-U|-R|-Q|-Qi|-Ql|-Qo [targets]");
        exit(1);
    };
    let flags: BTreeSet<char> = op[1..].chars().collect();
    let targets: Vec<String> = args[1..].iter().filter(|a| !a.starts_with("--")).cloned().collect();
    let repos = read_conf();

    if flags.contains(&'S') {
        if flags.contains(&'y') {
            refresh(&repos);
        }
        if flags.contains(&'s') {
            let word = targets.first().map_or("", |s| s.as_str());
            for (name, (ri, d)) in sync_all(&repos) {
                if name.contains(word) || get(&d, "DESC").contains(word) {
                    println!("{}/{} {}\n    {}", repos[ri].name, name, get(&d, "VERSION"), get(&d, "DESC"));
                }
            }
        } else if flags.contains(&'i') {
            let sync = sync_all(&repos);
            for t in &targets {
                let Some((ri, d)) = sync.get(t) else { die(format!("package '{}' was not found", t)) };
                info(d, Some(&repos[*ri].name));
            }
        } else if flags.contains(&'u') {
            upgrade_all(&repos, &targets);
        } else if !targets.is_empty() {
            // --needed: もう同じ版が入っているものは入れなおさない (pacman と同じ)
            let targets: Vec<String> = if args.iter().any(|a| a == "--needed") {
                let (sync, db) = (sync_all(&repos), installed());
                let (skip, keep): (Vec<String>, Vec<String>) = targets.iter().cloned().partition(|t| {
                    matches!((sync.get(t), db.get(t)), (Some((_, sd)), Some((d, _))) if get(sd, "VERSION") == get(d, "VERSION"))
                });
                for t in &skip {
                    eprintln!("warning: {} is up to date -- skipping", t);
                }
                keep
            } else {
                targets
            };
            if !targets.is_empty() {
                sync_install(&repos, &targets, &targets);
            }
        }
    } else if flags.contains(&'U') {
        for t in &targets {
            install(t, true);
        }
    } else if flags.contains(&'R') {
        remove(&targets);
    } else if flags.contains(&'Q') && flags.contains(&'o') {
        // -Qo FILE: どのパッケージのファイルか (pacman と同じ。/ のない名前は PATH から)
        let db = installed();
        let mut st = 0;
        for t in &targets {
            let path = if t.contains('/') {
                std::path::PathBuf::from(t)
            } else {
                match std::env::var("PATH").unwrap_or_default().split(':').map(|d| std::path::Path::new(d).join(t)).find(|p| p.exists()) {
                    Some(p) => p,
                    None => {
                        eprintln!("error: failed to find '{}' in PATH", t);
                        st = 1;
                        continue;
                    }
                }
            };
            // ディレクトリのリンク (/bin → usr/bin など) はたどる。ファイルそのもののリンクはたどらない
            let abs = std::fs::canonicalize(path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(std::path::Path::new(".")))
                .map(|d| d.join(path.file_name().unwrap_or_default()))
                .unwrap_or(path.clone());
            let rel = abs.to_string_lossy().trim_start_matches('/').to_string();
            let owners: Vec<_> = db.iter().filter(|(_, (_, files))| files.iter().any(|f| f.trim_end_matches('/') == rel)).collect();
            if owners.is_empty() {
                eprintln!("error: No package owns {}", abs.display());
                st = 1;
            }
            for (name, (d, _)) in owners {
                println!("{} is owned by {} {}", abs.display(), name, get(d, "VERSION"));
            }
        }
        exit(st);
    } else if flags.contains(&'Q') {
        let db = installed();
        let pick: Vec<(&String, &(Desc, Vec<String>))> = if targets.is_empty() {
            db.iter().collect()
        } else {
            let find = |t: &String| db.get_key_value(t).unwrap_or_else(|| die(format!("package '{}' was not found", t)));
            targets.iter().map(find).collect()
        };
        for (name, (d, files)) in pick {
            if flags.contains(&'i') {
                info(d, None);
            } else if flags.contains(&'l') {
                for f in files {
                    println!("{} /{}", name, f);
                }
            } else {
                println!("{} {}", name, get(d, "VERSION"));
            }
        }
    } else {
        die(format!("invalid option '{}'", op));
    }
}

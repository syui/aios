// aipkg: pacman と同じ形式 (.pkg.tar.zst, .PKGINFO, repo の .db) を扱う小さなパッケージマネージャ
//
//   aipkg -S pkg...     リポジトリから入れる (依存も)
//   aipkg -Sy / -Syu [pkg...]   データベースの更新 / 全部を新しくする (pkg も一緒に入れる)
//   aipkg -Ss [word]    リポジトリを探す
//   aipkg -Si pkg       リポジトリのパッケージの情報
//   aipkg -U file...    パッケージファイルを入れる
//   aipkg -R pkg...     外す
//   aipkg -Q / -Qi / -Ql [pkg]  入っているもの / 情報 / ファイル一覧
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
    let text = fs::read_to_string(CONF).unwrap_or_default();
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

/// パッケージの中身を読む: (.PKGINFO, ファイル一覧)
fn scan(path: &str) -> io::Result<(Desc, Vec<String>)> {
    let size = fs::metadata(path)?.len();
    let file = path.rsplit('/').next().unwrap_or(path);
    let mut m = meter::Meter::new(&format!("checking {}", file), size);
    let mut progress = |n: u64| m.update(n);
    let mut ar = tar::Archive::new(open_pkg(path, &mut progress)?);
    let mut info = None;
    let mut files = vec![];
    for e in ar.entries()? {
        let mut e = e?;
        let path = e.path()?.to_string_lossy().to_string();
        if path == ".PKGINFO" {
            let mut s = String::new();
            e.read_to_string(&mut s)?;
            info = Some(parse_pkginfo(&s));
        } else if !path.starts_with('.') {
            let mut p = path.trim_start_matches("./").to_string();
            if e.header().entry_type().is_dir() && !p.ends_with('/') {
                p.push('/');
            }
            files.push(p);
        }
    }
    drop(ar);
    m.finish(size);
    let info = info.ok_or_else(|| io::Error::other("no .PKGINFO in package"))?;
    Ok((info, files))
}

fn install(path: &str, explicit: bool) {
    let (info, files) = scan(path).unwrap_or_else(|e| die(format!("{}: {}", path, e)));
    let name = get(&info, "NAME").to_string();
    let ver = get(&info, "VERSION").to_string();
    let arch = get(&info, "ARCH");
    if arch != ARCH && arch != "any" {
        die(format!("{}: package is for {}, not {}", name, arch, ARCH));
    }
    let db = installed();
    let old = db.get(&name);

    // 他のパッケージが持っているファイルとぶつからないか
    for (other, (_, ofiles)) in &db {
        if *other == name {
            continue;
        }
        for f in files.iter().filter(|f| !f.ends_with('/')) {
            if ofiles.contains(f) {
                die(format!("{}: /{} exists in {}", name, f, other));
            }
        }
    }

    let label = match old {
        Some((od, _)) => format!("upgrading {} ({} -> {})", name, get(od, "VERSION"), ver),
        None => format!("installing {} ({})", name, ver),
    };
    let size = fs::metadata(path).map_or(0, |m| m.len());
    let mut m = meter::Meter::new(&label, size);
    let mut progress = |n: u64| m.update(n);

    // 設定ファイル (backup): 入っている版を変えていたら上書きせず、新しいものは .pacnew に
    // (パッケージの版が前と変わっていなければ、何もしない)
    let backup: BTreeSet<&String> = list(&info, "BACKUP").iter().collect();
    let old_hashes: BTreeMap<&str, &str> =
        old.map(|(od, _)| list(od, "BACKUP").iter().filter_map(|b| b.split_once('\t')).collect()).unwrap_or_default();
    let mut new_backup = vec![];

    let mut ar = tar::Archive::new(open_pkg(path, &mut progress).unwrap_or_else(|e| die(format!("{}: {}", path, e))));
    ar.set_preserve_permissions(true);
    ar.set_preserve_mtime(true);
    ar.set_overwrite(true);
    for e in ar.entries().unwrap_or_else(|e| die(e)) {
        let mut e = e.unwrap_or_else(|e| die(e));
        let path = e.path().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
        if path.starts_with('.') {
            continue;
        }
        if backup.contains(&path) {
            let mut data = vec![];
            e.read_to_end(&mut data).unwrap_or_else(|err| die(format!("{}: /{}: {}", name, path, err)));
            let mode = e.header().mode().unwrap_or(0o644);
            let hash = sha256_hex(&data);
            let dest = format!("{}{}", ROOT, path);
            // 今あるものが新しい版とも、前に入れた版とも違えば、手で変えたもの
            let changed = fs::read(&dest).is_ok_and(|cur| {
                let h = sha256_hex(&cur);
                h != hash && old_hashes.get(path.as_str()) != Some(&h.as_str())
            });
            // 手で変えていても、パッケージの版が前と同じなら今のものをそのまま使う (pacman と同じ)
            if changed && old_hashes.get(path.as_str()) == Some(&hash.as_str()) {
                new_backup.push(format!("{}\t{}", path, hash));
                continue;
            }
            let target = if changed {
                println!("warning: /{} installed as /{}.pacnew", path, path);
                format!("{}.pacnew", dest)
            } else {
                dest
            };
            write_file(&target, &data, mode).unwrap_or_else(|err| die(format!("{}: {}: {}", name, target, err)));
            new_backup.push(format!("{}\t{}", path, hash));
            continue;
        }
        if let Err(err) = e.unpack_in(ROOT) {
            die(format!("{}: /{}: {}", name, path, err));
        }
    }
    drop(ar);
    m.finish(size);

    // 古い版にだけあったファイルを消す
    if let Some((od, ofiles)) = old {
        let keep: BTreeSet<&String> = files.iter().collect();
        remove_files(ofiles.iter().filter(|f| !keep.contains(f)));
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
    let db = installed();
    for name in names {
        let Some((d, files)) = db.get(name) else { die(format!("target not found: {}", name)) };
        for (other, (od, _)) in &db {
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
        remove_files(files.iter());
        let _ = fs::remove_dir_all(local_dir(name, get(d, "VERSION")));
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
    let local: BTreeSet<String> = installed().into_keys().collect();
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
        install(&path, explicit.contains(n));
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
    for t in extra {
        if !targets.contains(t) {
            targets.push(t.clone());
        }
    }
    if targets.is_empty() {
        println!(" there is nothing to do");
        return;
    }
    sync_install(repos, &targets, extra);
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
        eprintln!("usage: aipkg -S|-Sy|-Syu|-Ss|-Si|-U|-R|-Q|-Qi|-Ql [targets]");
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
            sync_install(&repos, &targets, &targets);
        }
    } else if flags.contains(&'U') {
        for t in &targets {
            install(t, true);
        }
    } else if flags.contains(&'R') {
        remove(&targets);
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

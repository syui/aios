// aipkg: pacman と同じ形式 (.pkg.tar.zst, .PKGINFO, repo の .db) を扱う小さなパッケージマネージャ
//
//   aipkg -S pkg...     リポジトリから入れる (依存も)
//   aipkg -Sy / -Syu    データベースの更新 / 全部を新しくする
//   aipkg -Ss [word]    リポジトリを探す
//   aipkg -Si pkg       リポジトリのパッケージの情報
//   aipkg -U file...    パッケージファイルを入れる
//   aipkg -R pkg...     外す
//   aipkg -Q / -Qi / -Ql [pkg]  入っているもの / 情報 / ファイル一覧
//
// 設定は /etc/aipkg.conf (pacman.conf と同じ書き方で、[repo] と Server を読む)
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Read};
use std::process::exit;

const CONF: &str = "/etc/aipkg.conf";
const DBPATH: &str = "/var/lib/aipkg";
const CACHE: &str = "/var/cache/aipkg";
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
    Err(io::Error::other(format!("{}: only file:// is supported for now (no network yet)", url)))
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

/// zstd / gzip / 無圧縮の tar
fn tar_reader(bytes: &[u8]) -> Box<dyn Read + '_> {
    match bytes {
        [0x28, 0xb5, 0x2f, 0xfd, ..] => match ruzstd::decoding::StreamingDecoder::new(bytes) {
            Ok(d) => Box::new(d),
            Err(e) => die(format!("bad zstd data: {}", e)),
        },
        [0x1f, 0x8b, ..] => Box::new(flate2::read::GzDecoder::new(bytes)),
        _ => Box::new(bytes),
    }
}

/// 同期データベース (repo.db) の中身: パッケージ名 → desc
fn sync_db(repo: &str) -> BTreeMap<String, Desc> {
    let mut out = BTreeMap::new();
    let Ok(bytes) = fs::read(format!("{}/sync/{}.db", DBPATH, repo)) else { return out };
    let mut ar = tar::Archive::new(tar_reader(&bytes));
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
fn scan(bytes: &[u8]) -> io::Result<(Desc, Vec<String>)> {
    let mut ar = tar::Archive::new(tar_reader(bytes));
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
    let info = info.ok_or_else(|| io::Error::other("no .PKGINFO in package"))?;
    Ok((info, files))
}

fn install(bytes: &[u8], explicit: bool) {
    let (info, files) = scan(bytes).unwrap_or_else(|e| die(e));
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

    match old {
        Some((od, _)) => println!("upgrading {} ({} -> {})", name, get(od, "VERSION"), ver),
        None => println!("installing {} ({})", name, ver),
    }

    let mut ar = tar::Archive::new(tar_reader(bytes));
    ar.set_preserve_permissions(true);
    ar.set_preserve_mtime(true);
    ar.set_overwrite(true);
    for e in ar.entries().unwrap_or_else(|e| die(e)) {
        let mut e = e.unwrap_or_else(|e| die(e));
        let path = e.path().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
        if path.starts_with('.') {
            continue;
        }
        if let Err(err) = e.unpack_in(ROOT) {
            die(format!("{}: /{}: {}", name, path, err));
        }
    }

    // 古い版にだけあったファイルを消す
    if let Some((od, ofiles)) = old {
        let keep: BTreeSet<&String> = files.iter().collect();
        remove_files(ofiles.iter().filter(|f| !keep.contains(f)));
        let _ = fs::remove_dir_all(local_dir(&name, get(od, "VERSION")));
    }

    let mut desc = info.clone();
    desc.insert("INSTALLDATE".into(), vec![now().to_string()]);
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

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// 基本のディレクトリ (Arch では filesystem パッケージが持つもの)。空になっても消さない
const KEEP_DIRS: &[&str] = &[
    "bin/", "etc/", "home/", "lib/", "opt/", "root/", "srv/", "tmp/", "var/", "var/lib/", "var/cache/",
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
        remove_files(files.iter());
        let _ = fs::remove_dir_all(local_dir(name, get(d, "VERSION")));
    }
}

// ---- 同期 ----

fn refresh(repos: &[Repo]) {
    fs::create_dir_all(format!("{}/sync", DBPATH)).unwrap_or_else(|e| die(e));
    for r in repos {
        match fetch_any(&r.servers, &format!("{}.db", r.name)) {
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
        let bytes = fetch_any(&repos[*ri].servers, file).unwrap_or_else(|e| die(format!("{}: {}", file, e)));
        let want = get(d, "SHA256SUM");
        if !want.is_empty() {
            let got: String = Sha256::digest(&bytes).iter().map(|b| format!("{:02x}", b)).collect();
            if got != want {
                die(format!("{}: checksum mismatch", file));
            }
        }
        let _ = fs::write(format!("{}/{}", CACHE, file), &bytes);
        install(&bytes, explicit.contains(n));
    }
}

fn upgrade_all(repos: &[Repo]) {
    let sync = sync_all(repos);
    let mut targets = vec![];
    for (name, (d, _)) in installed() {
        if let Some((_, sd)) = sync.get(&name) {
            if vercmp(get(sd, "VERSION"), get(&d, "VERSION")) == std::cmp::Ordering::Greater {
                targets.push(name);
            }
        }
    }
    if targets.is_empty() {
        println!(" there is nothing to do");
        return;
    }
    sync_install(repos, &targets, &[]);
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
            upgrade_all(&repos);
        } else if !targets.is_empty() {
            sync_install(&repos, &targets, &targets);
        }
    } else if flags.contains(&'U') {
        for t in &targets {
            let bytes = fs::read(t).unwrap_or_else(|e| die(format!("{}: {}", t, e)));
            install(&bytes, true);
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

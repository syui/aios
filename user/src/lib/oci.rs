// ap image: aios のパッケージから最小の container image (OCI) を作る (Google の distroless と同じ考え: 動かすものと、
// それが要るものだけ。シェルもパッケージの道具もない)
//
//   ap image [--conf FILE] [-o OUT] DEF.json
//
// DEF.json (image/node.json など):
//   name      image の名前 (aios-node)。版は packages のはじめのものの版 (pkgrel を除く: 26.13.1)
//   packages  使うパッケージ (依存もたどる)
//   files     入れるパス。ELF なら、musl のローダーと同じ探し方 (PT_INTERP、DT_NEEDED、RPATH と RUNPATH、
//             /etc/ld-musl-aarch64.path、なければ /lib:/usr/local/lib:/usr/lib) で要るライブラリもいっしょに。
//             ディレクトリなら中をみな、リンクはその先も
//   config    OCI の config (Entrypoint、Cmd、User、WorkingDir、Env ...)
// 層: はじめは aios の土台 (/etc/passwd と group: root、nobody、nonroot 65532、/etc/os-release、/tmp、/home/nonroot、/root、
// /lib → usr/lib)、あとはパッケージごとに 1 層 (依存が先。同じパッケージの同じファイルなら、ほかの image と同じ層)。
// 入れたファイルのパッケージの usr/share/licenses も入れる。どのパッケージのどの版かは、層と image の annotation
// (ai.syui.aios.package、ai.syui.aios.packages) に書く (image から、どのパッケージのどの版かがわかる)。
// 時刻はパッケージのもの (土台は 0、image はパッケージのいちばん新しい BUILDDATE)、持ち主は 0 (nonroot の家だけ 65532)、
// 名前の順、gzip に時刻を入れない: 同じパッケージからはいつも同じ digest。
// OUT が .tar なら OCI archive (docker load、skopeo copy oci-archive:)、ほかは OCI layout のディレクトリ。既定は NAME.tar。
// パッケージは /etc/aipkg.conf (--conf) の Server から取る。置き場は $XDG_CACHE_HOME/aipkg か ~/.cache/aipkg
// (/var/cache/aipkg にチェックサムの合うものがあれば、それを使う)。root でなくてよい
use crate::{dep_name, die, fetch_any, fetch_any_to, get, list, parse_conf, parse_desc, provider, sha256_file, tar_reader, Desc, CACHE, CONF, DB_FILE};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Read, Write};

/// nonroot (distroless と同じ番号)
const NONROOT: u32 = 65532;

/// パッケージの中のもの
#[derive(Clone)]
enum Kind {
    File,
    Dir,
    Link(String),
}

#[derive(Clone)]
struct Meta {
    kind: Kind,
    mode: u32,
    mtime: u64,
    /// どの層 (0 は土台、1 からはパッケージの順)
    layer: usize,
}

struct Image {
    /// 広げた中身 (ファイルだけ)
    root: String,
    meta: BTreeMap<String, Meta>,
    /// 入れるもの (先頭の / を除いたパス)
    want: BTreeSet<String>,
    /// ELF をたどったもの
    seen: BTreeSet<String>,
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{:02x}", x)).collect()
}

fn norm(p: &str) -> String {
    let mut out: Vec<&str> = vec![];
    for c in p.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out.join("/")
}

fn parent(p: &str) -> &str {
    p.rsplit_once('/').map_or("", |(d, _)| d)
}

/// 置き場 ($XDG_CACHE_HOME/aipkg か ~/.cache/aipkg)
fn cache_dir() -> String {
    match std::env::var("XDG_CACHE_HOME") {
        Ok(d) if !d.is_empty() => format!("{}/aipkg", d),
        _ => format!("{}/.cache/aipkg", std::env::var("HOME").unwrap_or_else(|_| "/tmp".into())),
    }
}

/// aios.db (tar) の中身: 名前 → desc
fn read_db(bytes: Vec<u8>) -> BTreeMap<String, Desc> {
    let mut out = BTreeMap::new();
    let mut ar = tar::Archive::new(tar_reader(Box::new(io::Cursor::new(bytes))));
    let Ok(entries) = ar.entries() else { return out };
    for mut e in entries.flatten() {
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

/// packages と、その依存 (依存が先)
fn closure(pkgs: &[String], sync: &BTreeMap<String, (usize, Desc)>) -> Vec<String> {
    fn visit(n: &str, sync: &BTreeMap<String, (usize, Desc)>, seen: &mut BTreeSet<String>, out: &mut Vec<String>) {
        let n = provider(sync, n);
        if !seen.insert(n.clone()) {
            return;
        }
        let Some((_, d)) = sync.get(&n) else { die(format!("image: package not found: {}", n)) };
        for dep in list(d, "DEPENDS") {
            visit(dep_name(dep), sync, seen, out);
        }
        out.push(n);
    }
    let (mut seen, mut out) = (BTreeSet::new(), vec![]);
    for p in pkgs {
        visit(p, sync, &mut seen, &mut out);
    }
    out
}

/// パッケージを root に広げ、中のものを meta に (ファイルの中身だけを書く。持ち主と mode は meta に)
fn extract(path: &str, root: &str, layer: usize, meta: &mut BTreeMap<String, Meta>) -> io::Result<()> {
    let f = fs::File::open(path)?;
    let mut ar = tar::Archive::new(tar_reader(Box::new(io::BufReader::with_capacity(256 * 1024, f))));
    for e in ar.entries()? {
        let mut e = e?;
        let p = norm(&e.path()?.to_string_lossy());
        if p.is_empty() || p.starts_with('.') {
            continue;
        }
        let h = e.header();
        let (mode, mtime) = (h.mode().unwrap_or(0o644) & 0o7777, h.mtime().unwrap_or(0));
        let kind = match h.entry_type() {
            tar::EntryType::Directory => Kind::Dir,
            tar::EntryType::Symlink => Kind::Link(e.link_name()?.map(|l| l.to_string_lossy().to_string()).unwrap_or_default()),
            tar::EntryType::Link => {
                // ハードリンク: 先のファイルの写し
                let to = norm(&e.link_name()?.map(|l| l.to_string_lossy().to_string()).unwrap_or_default());
                fs::copy(format!("{}/{}", root, to), format!("{}/{}", root, p))?;
                Kind::File
            }
            _ => {
                let dst = format!("{}/{}", root, p);
                fs::create_dir_all(format!("{}/{}", root, parent(&p)))?;
                let mut out = fs::File::create(&dst)?;
                io::copy(&mut e, &mut out)?;
                Kind::File
            }
        };
        // ディレクトリは、はじめに見たパッケージのもの (あとのパッケージの層にも同じ形で入る)。setuid と setgid は落とす
        // (パッケージを作ったところのなごり。sticky は残す)
        if matches!(kind, Kind::Dir) {
            meta.entry(p).or_insert(Meta { kind, mode: mode & 0o1777, mtime, layer });
        } else {
            meta.insert(p, Meta { kind, mode, mtime, layer });
        }
    }
    Ok(())
}

impl Image {
    /// パスをたどる (とちゅうのリンクは record なら want に入れて、その先へ)。たどりついたところ (なければ None)
    fn resolve(&mut self, path: &str, record: bool) -> Option<String> {
        let mut todo: Vec<String> = norm(path).split('/').filter(|c| !c.is_empty()).map(String::from).collect();
        todo.reverse();
        let mut cur = String::new();
        let mut hops = 0;
        while let Some(c) = todo.pop() {
            let next = if cur.is_empty() { c.clone() } else { format!("{}/{}", cur, c) };
            match self.meta.get(&next).map(|m| m.kind.clone()) {
                Some(Kind::Link(to)) => {
                    hops += 1;
                    if hops > 40 {
                        return None;
                    }
                    if record {
                        self.want.insert(next.clone());
                    }
                    let target = if to.starts_with('/') { norm(&to) } else { norm(&format!("{}/{}", cur, to)) };
                    // リンクの先を、のこりの前に
                    let mut parts: Vec<String> = target.split('/').filter(|c| !c.is_empty()).map(String::from).collect();
                    parts.reverse();
                    todo.extend(parts);
                    cur.clear();
                }
                Some(_) => cur = next,
                None => return None,
            }
        }
        Some(cur)
    }

    /// path を入れる (ELF ならライブラリも、ディレクトリなら中もみな)
    fn add(&mut self, path: &str, why: &str) {
        let Some(p) = self.resolve(path, true) else { die(format!("image: {} ({}) is not in the packages", path, why)) };
        if p.is_empty() {
            return;
        }
        match self.meta.get(&p).map(|m| m.kind.clone()) {
            Some(Kind::Dir) => {
                self.want.insert(p.clone());
                let under: Vec<String> = self.meta.range(format!("{}/", p)..).take_while(|(k, _)| k.starts_with(&format!("{}/", p))).map(|(k, _)| k.clone()).collect();
                for k in under {
                    self.add(&format!("/{}", k), why);
                }
            }
            Some(Kind::File) => {
                self.want.insert(p.clone());
                self.elf(&p);
            }
            _ => {}
        }
    }

    /// ELF のインタプリタとライブラリ (musl のローダーと同じ探し方)
    fn elf(&mut self, p: &str) {
        if !self.seen.insert(p.to_string()) {
            return;
        }
        let Ok(b) = fs::read(format!("{}/{}", self.root, p)) else { return };
        let Some((interp, needed, paths)) = elf_deps(&b) else { return };
        if let Some(i) = interp {
            self.add(&i, &format!("interpreter of /{}", p));
            // musl のローダーは /etc/ld-musl-ARCH.path で探す場所を読む
            if i.contains("ld-musl-") {
                let conf = format!("etc/{}", i.rsplit('/').next().unwrap_or("").replace(".so.1", ".path"));
                if self.meta.contains_key(&conf) {
                    self.add(&format!("/{}", conf), "musl's search path");
                }
            }
        }
        let origin = parent(p).to_string();
        let mut dirs: Vec<String> = paths.iter().flat_map(|r| r.split(':').map(|d| d.replace("$ORIGIN", &format!("/{}", origin)).replace("${ORIGIN}", &format!("/{}", origin))).collect::<Vec<_>>()).collect();
        let conf = self.meta.keys().find(|k| k.starts_with("etc/ld-musl-") && k.ends_with(".path")).cloned();
        match conf.and_then(|c| fs::read_to_string(format!("{}/{}", self.root, c)).ok()) {
            Some(s) => dirs.extend(s.split(|c: char| c == ':' || c.is_whitespace()).filter(|d| !d.is_empty()).map(String::from)),
            None => dirs.extend(["/lib", "/usr/local/lib", "/usr/lib"].map(String::from)),
        }
        for n in needed {
            // libc、libpthread、librt、libm、libdl、libutil、libxnet は musl そのもの
            if let Some(rest) = n.strip_prefix("lib") {
                if ["c.", "pthread.", "rt.", "m.", "dl.", "util.", "xnet."].iter().any(|r| rest.starts_with(r)) {
                    continue;
                }
            }
            let found = if n.contains('/') {
                Some(n.clone())
            } else {
                dirs.iter().map(|d| format!("{}/{}", d.trim_end_matches('/'), n)).find(|c| self.resolve(c, false).is_some())
            };
            match found {
                Some(f) => self.add(&f, &format!("needed by /{}", p)),
                None => die(format!("image: {} (needed by /{}) is not in the packages; add the package that has it", n, p)),
            }
        }
    }
}

/// ELF (64 ビット、リトルエンディアン) の (PT_INTERP, DT_NEEDED, RPATH と RUNPATH)。ELF でなければ None
fn elf_deps(b: &[u8]) -> Option<(Option<String>, Vec<String>, Vec<String>)> {
    if b.len() < 64 || &b[..4] != b"\x7fELF" || b[4] != 2 || b[5] != 1 {
        return None;
    }
    let u16_ = |o: usize| b.get(o..o + 2).map(|x| u16::from_le_bytes([x[0], x[1]]) as usize);
    let u32_ = |o: usize| b.get(o..o + 4).map(|x| u32::from_le_bytes(x.try_into().unwrap()));
    let u64_ = |o: usize| b.get(o..o + 8).map(|x| u64::from_le_bytes(x.try_into().unwrap()) as usize);
    let cstr = |o: usize| b.get(o..).map(|s| String::from_utf8_lossy(&s[..s.iter().position(|&c| c == 0).unwrap_or(s.len())]).to_string());
    let (phoff, phent, phnum) = (u64_(32)?, u16_(54)?, u16_(56)?);
    let (mut loads, mut interp, mut dynamic) = (vec![], None, None);
    for i in 0..phnum {
        let p = phoff + i * phent;
        let (typ, off, va, sz) = (u32_(p)?, u64_(p + 8)?, u64_(p + 16)?, u64_(p + 32)?);
        match typ {
            1 => loads.push((va, off, sz)),
            2 => dynamic = Some((off, sz)),
            3 => interp = cstr(off),
            _ => {}
        }
    }
    let at = |va: usize| loads.iter().find(|(v, _, s)| va >= *v && va < v + s).map(|(v, o, _)| o + (va - v));
    let (mut strtab, mut needed, mut paths) = (None, vec![], vec![]);
    if let Some((off, sz)) = dynamic {
        for i in 0..sz / 16 {
            let (tag, val) = (u64_(off + i * 16)?, u64_(off + i * 16 + 8)?);
            match tag {
                0 => break,
                1 => needed.push(val),
                5 => strtab = at(val),
                15 | 29 => paths.push(val),
                _ => {}
            }
        }
    }
    let s = |o: usize| strtab.and_then(|t| cstr(t + o));
    Some((interp, needed.into_iter().filter_map(s).collect(), paths.into_iter().filter_map(s).collect()))
}

/// 書いたバイトの sha256 と数を数える
struct Hash<W: Write> {
    w: W,
    h: Sha256,
    n: u64,
}

impl<W: Write> Write for Hash<W> {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        let k = self.w.write(b)?;
        self.h.update(&b[..k]);
        self.n += k as u64;
        Ok(k)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.w.flush()
    }
}

/// 層 (gzip の tar) を blobs に書く: (digest, 大きさ, diff_id)
fn write_layer(blobs: &str, entries: &[(String, Meta, Option<String>)], root: &str) -> io::Result<(String, u64, String)> {
    let tmp = format!("{}/layer.tmp", blobs);
    let file = Hash { w: fs::File::create(&tmp)?, h: Sha256::new(), n: 0 };
    let gz = flate2::GzBuilder::new().mtime(0).write(file, flate2::Compression::default());
    let mut b = tar::Builder::new(Hash { w: gz, h: Sha256::new(), n: 0 });
    for (p, m, data) in entries {
        let mut h = tar::Header::new_gnu();
        h.set_mode(m.mode);
        h.set_mtime(m.mtime);
        let id = if p == "home/nonroot" { NONROOT as u64 } else { 0 };
        h.set_uid(id);
        h.set_gid(id);
        match &m.kind {
            Kind::Dir => {
                h.set_entry_type(tar::EntryType::Directory);
                h.set_size(0);
                b.append_data(&mut h, format!("{}/", p), io::empty())?;
            }
            Kind::Link(to) => {
                h.set_entry_type(tar::EntryType::Symlink);
                h.set_size(0);
                b.append_link(&mut h, p, to)?;
            }
            Kind::File => {
                h.set_entry_type(tar::EntryType::Regular);
                match data {
                    Some(d) => {
                        h.set_size(d.len() as u64);
                        b.append_data(&mut h, p, d.as_bytes())?;
                    }
                    None => {
                        let f = fs::File::open(format!("{}/{}", root, p))?;
                        h.set_size(f.metadata()?.len());
                        b.append_data(&mut h, p, f)?;
                    }
                }
            }
        }
    }
    let inner = b.into_inner()?;
    let diff = hex(&inner.h.finalize());
    let mut file = inner.w.finish()?;
    file.flush()?;
    let (digest, size) = (hex(&file.h.finalize()), file.n);
    fs::rename(&tmp, format!("{}/{}", blobs, digest))?;
    Ok((digest, size, diff))
}

/// JSON を blobs に書く: (digest, 大きさ)
fn write_json(blobs: &str, v: &Value) -> io::Result<(String, u64)> {
    let b = serde_json::to_vec(v).map_err(io::Error::other)?;
    let d = hex(&Sha256::digest(&b));
    fs::write(format!("{}/{}", blobs, d), &b)?;
    Ok((d, b.len() as u64))
}

/// 1970 年からの秒を RFC 3339 に
fn rfc3339(t: u64) -> String {
    let (days, s) = ((t / 86400) as i64, t % 86400);
    // civil_from_days (Howard Hinnant)
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, m, d, s / 3600, s / 60 % 60, s % 60)
}

/// OCI layout のディレクトリを tar に (名前の順、時刻 t)
fn archive(dir: &str, out: &str, t: u64) -> io::Result<()> {
    let mut b = tar::Builder::new(fs::File::create(out)?);
    let mut paths = vec!["oci-layout".to_string(), "index.json".to_string(), "blobs".to_string(), "blobs/sha256".to_string()];
    let mut blobs: Vec<String> = fs::read_dir(format!("{}/blobs/sha256", dir))?.flatten().map(|e| format!("blobs/sha256/{}", e.file_name().to_string_lossy())).collect();
    blobs.sort();
    paths.extend(blobs);
    for p in paths {
        let full = format!("{}/{}", dir, p);
        let mut h = tar::Header::new_gnu();
        h.set_mtime(t);
        h.set_uid(0);
        h.set_gid(0);
        if fs::metadata(&full)?.is_dir() {
            h.set_entry_type(tar::EntryType::Directory);
            h.set_mode(0o755);
            h.set_size(0);
            b.append_data(&mut h, format!("{}/", p), io::empty())?;
        } else {
            let f = fs::File::open(&full)?;
            h.set_entry_type(tar::EntryType::Regular);
            h.set_mode(0o644);
            h.set_size(f.metadata()?.len());
            b.append_data(&mut h, &p, f)?;
        }
    }
    b.into_inner()?.flush()
}

pub fn main(args: &[String]) {
    let (mut conf, mut out, mut def) = (CONF.to_string(), None, None);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--conf" => conf = it.next().cloned().unwrap_or_else(|| die("--conf FILE")),
            "-o" => out = it.next().cloned(),
            a if a.starts_with('-') => die(format!("image: unknown option {}", a)),
            a => def = Some(a.to_string()),
        }
    }
    let Some(def) = def else { die("usage: ap image [--conf FILE] [-o OUT] DEF.json") };
    let d: Value = serde_json::from_str(&fs::read_to_string(&def).unwrap_or_else(|e| die(format!("{}: {}", def, e)))).unwrap_or_else(|e| die(format!("{}: {}", def, e)));
    let strs = |k: &str| -> Vec<String> { d[k].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default() };
    let name = d["name"].as_str().unwrap_or_else(|| die(format!("{}: no name", def))).to_string();
    let (pkgs, files) = (strs("packages"), strs("files"));
    if pkgs.is_empty() {
        die(format!("{}: no packages", def));
    }
    let repos = parse_conf(&conf);
    if repos.is_empty() {
        die(format!("image: no repositories in {}", conf));
    }

    // 同期データベース (いつも取りなおす: 小さいので)。取れないリポジトリは -Sy と同じく知らせて飛ばす
    // (足りないパッケージは closure が言う)
    let mut sync: BTreeMap<String, (usize, Desc)> = BTreeMap::new();
    for (i, r) in repos.iter().enumerate() {
        match fetch_any(&r.servers, DB_FILE) {
            Ok(b) => {
                for (n, desc) in read_db(b) {
                    sync.entry(n).or_insert((i, desc));
                }
            }
            Err(e) => eprintln!("warning: image: {}: {}", r.name, e),
        }
    }
    let order = closure(&pkgs, &sync);
    let cache = cache_dir();
    let work = format!("{}/image-{}", cache, std::process::id());
    let root = format!("{}/root", work);
    fs::create_dir_all(&root).unwrap_or_else(|e| die(format!("{}: {}", root, e)));

    // 取ってきて広げる (層 1 から: パッケージの順)
    let mut meta: BTreeMap<String, Meta> = BTreeMap::new();
    let mut built = 0u64;
    for (k, n) in order.iter().enumerate() {
        let (ri, desc) = &sync[n];
        let (file, want) = (get(desc, "FILENAME"), get(desc, "SHA256SUM"));
        built = built.max(get(desc, "BUILDDATE").parse().unwrap_or(0));
        let sys = format!("{}/{}", CACHE, file);
        let mine = format!("{}/{}", cache, file);
        let ok = |p: &str| !want.is_empty() && sha256_file(p).is_ok_and(|h| h == want);
        let path = if ok(&sys) {
            sys
        } else {
            if !ok(&mine) {
                println!("fetch: {}", file);
                fetch_any_to(&repos[*ri].servers, file, &mine, &mut |_, _| {}).unwrap_or_else(|e| die(format!("{}: {}", file, e)));
                if !ok(&mine) {
                    let _ = fs::remove_file(&mine);
                    die(format!("{}: checksum mismatch", file));
                }
            }
            mine
        };
        extract(&path, &root, k + 1, &mut meta).unwrap_or_else(|e| die(format!("{}: {}", file, e)));
    }

    // 土台 (層 0): ユーザー、os-release、/tmp、家、/lib → usr/lib (aios のローダーは /lib/ld-musl-aarch64.so.1)。
    // 時刻は 0 (どの image でも同じ層: 分けあえる)
    let dir = |mode| Meta { kind: Kind::Dir, mode, mtime: 0, layer: 0 };
    let file = Meta { kind: Kind::File, mode: 0o644, mtime: 0, layer: 0 };
    let mut base: Vec<(String, Meta, Option<String>)> = vec![
        ("etc".into(), dir(0o755), None),
        ("etc/group".into(), file.clone(), Some("root:x:0:\ntty:x:5:\nnobody:x:65534:\nnonroot:x:65532:\n".into())),
        ("etc/os-release".into(), file.clone(), Some("NAME=\"aios\"\nID=aios\nPRETTY_NAME=\"aios (distroless)\"\nHOME_URL=\"https://git.syui.ai/ai/os\"\n".into())),
        (
            "etc/passwd".into(),
            file.clone(),
            Some("root:x:0:0:root:/root:/sbin/nologin\nnobody:x:65534:65534:nobody:/nonexistent:/sbin/nologin\nnonroot:x:65532:65532:nonroot:/home/nonroot:/sbin/nologin\n".into()),
        ),
        ("home".into(), dir(0o755), None),
        ("home/nonroot".into(), dir(0o700), None),
        ("lib".into(), Meta { kind: Kind::Link("usr/lib".into()), mode: 0o777, mtime: 0, layer: 0 }, None),
        ("root".into(), dir(0o700), None),
        ("tmp".into(), dir(0o1777), None),
        ("usr".into(), dir(0o755), None),
        ("usr/lib".into(), dir(0o755), None),
    ];
    for (p, m, _) in &base {
        if !meta.contains_key(p) || p == "lib" {
            meta.insert(p.clone(), m.clone());
        }
    }

    // 入れるものを選ぶ
    let mut img = Image { root: root.clone(), meta, want: BTreeSet::new(), seen: BTreeSet::new() };
    for f in &files {
        img.add(f, "files");
    }
    // 入れたファイルのパッケージの usr/share/licenses
    let used: BTreeSet<usize> = img.want.iter().filter_map(|p| img.meta.get(p).map(|m| m.layer)).collect();
    for (k, n) in order.iter().enumerate() {
        let lic = format!("usr/share/licenses/{}", n);
        if used.contains(&(k + 1)) && img.meta.get(&lic).is_some_and(|m| m.layer == k + 1) {
            img.add(&format!("/{}", lic), "license");
        }
    }

    // 層ごとに: 入れるものと、その親のディレクトリ (土台にあるものは土台に)
    let mut layers: Vec<Vec<(String, Meta, Option<String>)>> = vec![vec![]; order.len() + 1];
    let base_paths: BTreeSet<String> = base.iter().map(|(p, _, _)| p.clone()).collect();
    for p in &img.want {
        if base_paths.contains(p) {
            continue;
        }
        let m = img.meta[p].clone();
        let l = m.layer;
        let mut q = parent(p).to_string();
        while !q.is_empty() {
            if !base_paths.contains(&q) && !layers[l].iter().any(|(x, _, _)| *x == q) {
                let dm = img.meta.get(&q).filter(|m| matches!(m.kind, Kind::Dir)).cloned().unwrap_or(Meta { kind: Kind::Dir, mode: 0o755, mtime: m.mtime, layer: l });
                layers[l].push((q.clone(), dm, None));
            }
            q = parent(&q).to_string();
        }
        layers[l].push((p.clone(), m, None));
    }
    layers[0].append(&mut base);
    for l in layers.iter_mut() {
        l.sort_by(|a, b| a.0.cmp(&b.0));
        l.dedup_by(|a, b| a.0 == b.0);
    }

    // OCI layout
    let layout = format!("{}/oci", work);
    let blobs = format!("{}/blobs/sha256", layout);
    fs::create_dir_all(&blobs).unwrap_or_else(|e| die(e));
    let created = rfc3339(built);
    let (mut descs, mut diffs, mut history, mut names) = (vec![], vec![], vec![], vec![]);
    for (k, l) in layers.iter().enumerate() {
        if l.is_empty() {
            continue;
        }
        let what = if k == 0 { "aios base (users, os-release, /tmp)".to_string() } else { format!("{} {}", order[k - 1], get(&sync[&order[k - 1]].1, "VERSION")) };
        let (digest, size, diff) = write_layer(&blobs, l, &root).unwrap_or_else(|e| die(format!("image: layer {}: {}", what, e)));
        let mut desc = json!({"mediaType": "application/vnd.oci.image.layer.v1.tar+gzip", "digest": format!("sha256:{}", digest), "size": size});
        if k > 0 {
            desc["annotations"] = json!({"ai.syui.aios.package": what});
            names.push(what.replace(' ', "="));
        }
        println!("layer: {} ({} files, {} KiB)", what, l.iter().filter(|(_, m, _)| matches!(m.kind, Kind::File)).count(), size / 1024);
        descs.push(desc);
        diffs.push(format!("sha256:{}", diff));
        history.push(json!({"created": created, "created_by": format!("ap image: {}", what)}));
    }
    let version = get(&sync[&provider(&sync, &pkgs[0])].1, "VERSION").rsplit_once('-').map_or("", |(v, _)| v).to_string();
    let version = version.split_once(':').map_or(version.as_str(), |(_, v)| v).to_string();
    // 中身の名前と出どころ (docker inspect の Labels。ghcr.io は source でリポジトリにつなぐ)。DEF の config.Labels が勝つ
    let source = "https://github.com/syui/aios";
    let about = d.get("description").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let mut labels = json!({
        "org.opencontainers.image.title": name,
        "org.opencontainers.image.version": version,
        "org.opencontainers.image.source": source,
    });
    if !about.is_empty() {
        labels["org.opencontainers.image.description"] = json!(about);
    }
    let mut cfg = d.get("config").cloned().unwrap_or(json!({}));
    if let (Some(l), Some(c)) = (labels.as_object_mut(), cfg.as_object_mut()) {
        if let Some(mine) = c.get("Labels").and_then(|v| v.as_object()) {
            l.extend(mine.clone());
        }
        c.insert("Labels".into(), labels);
    }
    let config = json!({
        "architecture": "arm64",
        "os": "linux",
        "created": created,
        "config": cfg,
        "rootfs": {"type": "layers", "diff_ids": diffs},
        "history": history,
    });
    let (cd, cs) = write_json(&blobs, &config).unwrap_or_else(|e| die(e));
    let mut manifest = json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": format!("sha256:{}", cd), "size": cs},
        "layers": descs,
        "annotations": {
            "org.opencontainers.image.title": name,
            "org.opencontainers.image.version": version,
            "org.opencontainers.image.created": created,
            "org.opencontainers.image.source": source,
            "ai.syui.aios.packages": names.join(" "),
        },
    });
    if !about.is_empty() {
        manifest["annotations"]["org.opencontainers.image.description"] = json!(about);
    }
    let (md, ms) = write_json(&blobs, &manifest).unwrap_or_else(|e| die(e));
    let index = json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [{
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": format!("sha256:{}", md),
            "size": ms,
            "platform": {"architecture": "arm64", "os": "linux"},
            "annotations": {"org.opencontainers.image.ref.name": version},
        }],
    });
    fs::write(format!("{}/index.json", layout), serde_json::to_vec(&index).unwrap()).unwrap_or_else(|e| die(e));
    fs::write(format!("{}/oci-layout", layout), b"{\"imageLayoutVersion\":\"1.0.0\"}").unwrap_or_else(|e| die(e));

    let out = out.unwrap_or_else(|| format!("{}.tar", name));
    if out.ends_with(".tar") {
        archive(&layout, &out, built).unwrap_or_else(|e| die(format!("{}: {}", out, e)));
    } else {
        let _ = fs::remove_dir_all(&out);
        fs::rename(&layout, &out).unwrap_or_else(|e| die(format!("{}: {}", out, e)));
    }
    let _ = fs::remove_dir_all(&work);
    println!("image: {}:{} sha256:{} ({})", name, version, md, out);
}

// container image の置き場と、image の根。ap image pull (registry から取る) と aibox --image (動かす) が使う
//   置き場: $XDG_CACHE_HOME/aios/images (なければ ~/.cache/aios/images)。OCI の image layout (oci-layout、index.json、
//           blobs/sha256/HEX) で、index.json の manifests の org.opencontainers.image.ref.name が名前
//           (ghcr.io/syui/aios-node:26.13.1、docker.io/library/alpine:3.22、ap image で作ったものは localhost/NAME:VER)
//   根:     $XDG_CACHE_HOME/aios/rootfs/HEX (HEX は manifest の sha256)。層を順にひろげたもの。whiteout (.wh.NAME と
//           .wh..wh..opq) は下の層のものを消す。持ち主は自分 (root でなくてよい。aibox はユーザーの namespace で image の
//           User に見せる)。setuid と setgid、デバイスと FIFO は入れない。層のパスがリンクを通るときは根の中でたどる
//           (根の外に書かない)。終わりに、マウントするところ (proc、dev、tmp、etc/hosts、etc/resolv.conf、WorkingDir) を作る
#![allow(dead_code)]
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::io::{self, Read, Seek};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
pub const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
pub const DOCKER_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";
pub const DOCKER_LIST: &str = "application/vnd.docker.distribution.manifest.list.v2+json";

fn err(s: impl Into<String>) -> io::Error {
    io::Error::other(s.into())
}

/// $XDG_CACHE_HOME/aios (なければ ~/.cache/aios)
pub fn base() -> PathBuf {
    let c = std::env::var("XDG_CACHE_HOME")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| std::env::var("HOME").ok().map(|h| format!("{}/.cache", h)))
        .unwrap_or_else(|| "/tmp".into());
    PathBuf::from(c).join("aios")
}

/// image の置き場 (OCI layout)
pub fn store() -> Layout {
    Layout { dir: base().join("images") }
}

/// image の名前: registry、リポジトリ、タグ (か sha256:...)
pub struct Ref {
    pub registry: String,
    pub repo: String,
    pub tag: String,
}

impl Ref {
    /// alpine → docker.io/library/alpine:latest、ghcr.io/syui/aios-node:26.13.1、NAME@sha256:...
    pub fn parse(s: &str) -> Ref {
        let (name, tag) = match s.split_once('@') {
            Some((n, d)) => (n, d.to_string()),
            None => match s.rfind(':') {
                Some(i) if i > s.rfind('/').unwrap_or(0) => (&s[..i], s[i + 1..].to_string()),
                _ => (s, "latest".to_string()),
            },
        };
        let (registry, repo) = match name.split_once('/') {
            Some((h, r)) if h.contains('.') || h.contains(':') || h == "localhost" => (h.to_string(), r.to_string()),
            _ if name.contains('/') => ("docker.io".to_string(), name.to_string()),
            _ => ("docker.io".to_string(), format!("library/{}", name)),
        };
        Ref { registry, repo, tag }
    }

    pub fn full(&self) -> String {
        let sep = if self.tag.starts_with("sha256:") { '@' } else { ':' };
        format!("{}/{}{}{}", self.registry, self.repo, sep, self.tag)
    }

    /// registry の API のホスト (docker.io は registry-1.docker.io)
    pub fn host(&self) -> String {
        if self.registry == "docker.io" { "registry-1.docker.io".to_string() } else { self.registry.clone() }
    }

    /// リポジトリの終わりの名前 (aios-node。ホスト名などに)
    pub fn short(&self) -> String {
        self.repo.rsplit('/').next().unwrap_or(&self.repo).to_string()
    }
}

/// sha256:HEX の HEX (形がおかしければ None)
pub fn hex_of(d: &str) -> Option<&str> {
    d.strip_prefix("sha256:").filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()))
}

pub fn sha256(b: &[u8]) -> String {
    Sha256::digest(b).iter().map(|x| format!("{:02x}", x)).collect()
}

pub fn sha256_file(p: &Path) -> io::Result<String> {
    let mut f = fs::File::open(p)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|x| format!("{:02x}", x)).collect())
}

/// OCI の image layout のディレクトリ (置き場、または ap image -o DIR で作ったもの)
pub struct Layout {
    pub dir: PathBuf,
}

impl Layout {
    pub fn blob(&self, digest: &str) -> io::Result<PathBuf> {
        let h = hex_of(digest).ok_or_else(|| err(format!("bad digest: {}", digest)))?;
        Ok(self.dir.join("blobs/sha256").join(h))
    }

    pub fn has(&self, digest: &str) -> bool {
        self.blob(digest).is_ok_and(|p| p.exists())
    }

    /// blob を読んで、sha256 が名前と同じかを見る (manifest や config など小さいもの)
    pub fn read(&self, digest: &str) -> io::Result<Vec<u8>> {
        let b = fs::read(self.blob(digest)?)?;
        if format!("sha256:{}", sha256(&b)) != digest {
            return Err(err(format!("{}: sha256 mismatch", digest)));
        }
        Ok(b)
    }

    pub fn read_json(&self, digest: &str) -> io::Result<Value> {
        serde_json::from_slice(&self.read(digest)?).map_err(|e| err(format!("{}: {}", digest, e)))
    }

    fn init(&self) -> io::Result<()> {
        fs::create_dir_all(self.dir.join("blobs/sha256"))?;
        let l = self.dir.join("oci-layout");
        if !l.exists() {
            fs::write(l, b"{\"imageLayoutVersion\":\"1.0.0\"}")?;
        }
        Ok(())
    }

    /// 小さな blob (manifest) を置く。digest を返す
    pub fn put(&self, b: &[u8]) -> io::Result<String> {
        self.init()?;
        let d = format!("sha256:{}", sha256(b));
        let p = self.blob(&d)?;
        if !p.exists() {
            let tmp = p.with_extension(format!("tmp-{}", std::process::id()));
            fs::write(&tmp, b)?;
            fs::rename(&tmp, &p)?;
        }
        Ok(d)
    }

    /// tmp (書き終えたファイル) を digest の blob として置く (sha256 が合わなければ消してしくじる)
    pub fn put_file(&self, tmp: &Path, digest: &str) -> io::Result<()> {
        self.init()?;
        let got = sha256_file(tmp)?;
        if Some(got.as_str()) != hex_of(digest) {
            let _ = fs::remove_file(tmp);
            return Err(err(format!("{}: sha256 mismatch (got {})", digest, got)));
        }
        fs::rename(tmp, self.blob(digest)?)
    }

    pub fn index(&self) -> Value {
        fs::read(self.dir.join("index.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_else(|| json!({"schemaVersion": 2, "mediaType": OCI_INDEX, "manifests": []}))
    }

    /// index.json の名前 (ref.name) の manifest の記述
    pub fn find(&self, name: &str) -> Option<Value> {
        let idx = self.index();
        idx["manifests"].as_array()?.iter().find(|m| m["annotations"]["org.opencontainers.image.ref.name"].as_str() == Some(name)).cloned()
    }

    /// 名前を desc (manifest の記述) に向ける (同じ名前の前のものはのぞく)
    pub fn tag(&self, name: &str, mut desc: Value) -> io::Result<()> {
        self.init()?;
        let mut idx = self.index();
        desc["annotations"]["org.opencontainers.image.ref.name"] = json!(name);
        let list = idx["manifests"].as_array_mut().ok_or_else(|| err("index.json: no manifests"))?;
        list.retain(|m| m["annotations"]["org.opencontainers.image.ref.name"].as_str() != Some(name));
        list.push(desc);
        let tmp = self.dir.join(format!("index.json.tmp-{}", std::process::id()));
        fs::write(&tmp, serde_json::to_vec(&idx).map_err(|e| err(e.to_string()))?)?;
        fs::rename(tmp, self.dir.join("index.json"))
    }
}

/// index (OCI の index か docker の manifest list) から linux/arm64 のもの (platform のないものが 1 つだけならそれ)
pub fn pick(index: &Value) -> Option<Value> {
    let list = index["manifests"].as_array()?;
    let ok = |m: &&Value| m["platform"]["os"].as_str() == Some("linux") && m["platform"]["architecture"].as_str() == Some("arm64");
    // 名前のある (attestation でない) もの
    list.iter().filter(ok).find(|m| m["platform"]["variant"].as_str().is_none_or(|v| v == "v8")).or_else(|| list.iter().find(ok)).or_else(|| (list.len() == 1).then(|| &list[0])).cloned()
}

pub fn is_index(v: &Value, media: &str) -> bool {
    let mt = v["mediaType"].as_str().unwrap_or(media);
    mt == OCI_INDEX || mt == DOCKER_LIST || (mt.is_empty() && v["manifests"].is_array())
}

/// desc (manifest か index の記述) から manifest を: (manifest, その digest)
pub fn manifest(l: &Layout, desc: &Value) -> io::Result<(Value, String)> {
    let mut d = desc["digest"].as_str().ok_or_else(|| err("no digest"))?.to_string();
    let mut media = desc["mediaType"].as_str().unwrap_or("").to_string();
    for _ in 0..2 {
        let v = l.read_json(&d)?;
        if !is_index(&v, &media) {
            return Ok((v, d));
        }
        let m = pick(&v).ok_or_else(|| err(format!("{}: no linux/arm64 image", d)))?;
        d = m["digest"].as_str().ok_or_else(|| err("no digest"))?.to_string();
        media = m["mediaType"].as_str().unwrap_or("").to_string();
    }
    Err(err("index in index in index"))
}

/// OCI のアーカイブ (ap image -o X.tar、skopeo の oci-archive) を置き場に入れる。入れた (名前, desc) の並び
/// (名前は ref.name が NAME:TAG ならそれ、タグだけなら localhost/TITLE:TAG。TITLE は manifest の title か、ファイルの名前)
pub fn import_archive(path: &Path, st: &Layout) -> io::Result<Vec<(String, Value)>> {
    st.init()?;
    let mut ar = tar::Archive::new(io::BufReader::new(fs::File::open(path)?));
    let mut index: Option<Value> = None;
    for e in ar.entries()? {
        let mut e = e?;
        let p = e.path()?.to_string_lossy().trim_start_matches("./").to_string();
        if p == "index.json" {
            let mut b = vec![];
            e.read_to_end(&mut b)?;
            index = Some(serde_json::from_slice(&b).map_err(|x| err(format!("index.json: {}", x)))?);
        } else if let Some(h) = p.strip_prefix("blobs/sha256/") {
            let d = format!("sha256:{}", h);
            if hex_of(&d).is_none() || st.has(&d) {
                continue;
            }
            let dst = st.blob(&d)?;
            let tmp = dst.with_extension(format!("tmp-{}", std::process::id()));
            io::copy(&mut e, &mut fs::File::create(&tmp)?)?;
            st.put_file(&tmp, &d)?;
        }
    }
    let index = index.ok_or_else(|| err(format!("{}: not an OCI archive (no index.json)", path.display())))?;
    let stem = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "image".into());
    let mut out = vec![];
    for desc in index["manifests"].as_array().into_iter().flatten() {
        let tag = desc["annotations"]["org.opencontainers.image.ref.name"].as_str().unwrap_or("latest").to_string();
        let name = if tag.contains('/') || (tag.contains(':') && !tag.starts_with("sha256:")) {
            Ref::parse(&tag).full()
        } else {
            let title = manifest(st, desc).ok().and_then(|(m, _)| m["annotations"]["org.opencontainers.image.title"].as_str().map(String::from));
            format!("localhost/{}:{}", title.unwrap_or_else(|| stem.clone()), tag)
        };
        let mut d = desc.clone();
        if let Some(a) = d["annotations"].as_object_mut() {
            a.remove("org.opencontainers.image.ref.name");
        }
        st.tag(&name, d.clone())?;
        out.push((name, d));
    }
    Ok(out)
}

/// rel (層のパス) を根の中のパスに。途中のリンクは根の中でたどり (/ で始まるものは根から、.. は根より上に出ない)、
/// 最後の名前はたどらない
fn inside(root: &Path, rel: &str) -> Option<PathBuf> {
    let mut parts: Vec<String> = vec![];
    let mut queue: VecDeque<String> = rel.split('/').filter(|c| !c.is_empty() && *c != ".").map(String::from).collect();
    let mut hops = 0;
    while let Some(c) = queue.pop_front() {
        if c == ".." {
            parts.pop();
            continue;
        }
        if queue.is_empty() {
            parts.push(c);
            break;
        }
        let p = root.join(parts.join("/")).join(&c);
        match fs::symlink_metadata(&p) {
            Ok(m) if m.file_type().is_symlink() => {
                hops += 1;
                if hops > 40 {
                    return None;
                }
                let t = fs::read_link(&p).ok()?.to_string_lossy().to_string();
                if t.starts_with('/') {
                    parts.clear();
                }
                let mut q: VecDeque<String> = t.split('/').filter(|c| !c.is_empty() && *c != ".").map(String::from).collect();
                q.extend(queue.drain(..));
                queue = q;
            }
            _ => parts.push(c),
        }
    }
    Some(root.join(parts.join("/")))
}

/// path を消す (ディレクトリは中ごと。書けないディレクトリも自分のものなら書けるようにしてから)。なければ何もしない
pub fn remove_all(path: &Path) -> io::Result<()> {
    let Ok(m) = fs::symlink_metadata(path) else { return Ok(()) };
    if m.is_dir() {
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
        for e in fs::read_dir(path)?.flatten() {
            remove_all(&e.path())?;
        }
        fs::remove_dir(path)
    } else {
        fs::remove_file(path)
    }
}

/// 層 (tar、gzip か zstd か そのまま) を根に重ねる。dirs にディレクトリの mode を (終わりにまとめて付ける)。
/// 入れなかったもの (デバイス、FIFO、根の外を指すハードリンク) の数を返す
fn apply_layer(root: &Path, blob: &Path, dirs: &mut BTreeMap<PathBuf, u32>) -> io::Result<usize> {
    let mut f = fs::File::open(blob)?;
    let mut magic = [0u8; 4];
    let n = f.read(&mut magic)?;
    f.seek(io::SeekFrom::Start(0))?;
    let r: Box<dyn Read> = if n >= 2 && magic[..2] == [0x1f, 0x8b] {
        Box::new(flate2::read::MultiGzDecoder::new(io::BufReader::new(f)))
    } else if n == 4 && magic == [0x28, 0xb5, 0x2f, 0xfd] {
        Box::new(ruzstd::decoding::StreamingDecoder::new(io::BufReader::new(f)).map_err(|e| err(e.to_string()))?)
    } else {
        Box::new(io::BufReader::new(f))
    };
    let mut ar = tar::Archive::new(r);
    // この層で置いたもの (同じ層の .wh..wh..opq は、下の層のものだけを消す)
    let mut mine: BTreeSet<PathBuf> = BTreeSet::new();
    let mut skipped = 0;
    for e in ar.entries()? {
        let mut e = e?;
        let raw = e.path()?.to_string_lossy().to_string();
        let rel = raw.trim_start_matches("./").trim_start_matches('/').trim_end_matches('/').to_string();
        let (dir_rel, name) = rel.rsplit_once('/').unwrap_or(("", rel.as_str()));
        if name == ".wh..wh..opq" {
            if let Some(d) = inside(root, &format!("{}/x", dir_rel)).and_then(|p| p.parent().map(Path::to_path_buf)) {
                for c in fs::read_dir(&d).into_iter().flatten().flatten() {
                    if !mine.contains(&c.path()) {
                        remove_all(&c.path())?;
                    }
                }
            }
            continue;
        }
        if let Some(gone) = name.strip_prefix(".wh.") {
            if let Some(p) = inside(root, &format!("{}/{}", dir_rel, gone)).filter(|p| p != root) {
                remove_all(&p)?;
            }
            continue;
        }
        let Some(p) = inside(root, &rel).filter(|p| p != root) else { continue };
        let h = e.header();
        let et = h.entry_type();
        let mode = h.mode().unwrap_or(0o644);
        let old = fs::symlink_metadata(&p).ok();
        if old.as_ref().is_some_and(|m| !(et.is_dir() && m.is_dir())) {
            remove_all(&p)?;
        }
        if let Some(par) = p.parent() {
            fs::create_dir_all(par)?;
        }
        match et {
            tar::EntryType::Directory => {
                if old.as_ref().is_none_or(|m| !m.is_dir()) {
                    fs::create_dir(&p)?;
                }
                dirs.insert(p.clone(), mode & 0o1777);
            }
            tar::EntryType::Regular | tar::EntryType::Continuous => {
                let mut out = fs::File::create(&p)?;
                io::copy(&mut e, &mut out)?;
                out.set_permissions(fs::Permissions::from_mode(mode & 0o777))?;
            }
            tar::EntryType::Symlink => {
                let t = e.link_name()?.ok_or_else(|| err(format!("{}: symlink without target", rel)))?;
                std::os::unix::fs::symlink(t, &p)?;
            }
            tar::EntryType::Link => {
                let t = e.link_name()?.ok_or_else(|| err(format!("{}: link without target", rel)))?.to_string_lossy().to_string();
                match inside(root, &t) {
                    Some(tp) if fs::hard_link(&tp, &p).is_ok() || fs::copy(&tp, &p).is_ok() => {}
                    _ => skipped += 1,
                }
            }
            _ => skipped += 1,
        }
        mine.insert(p);
    }
    Ok(skipped)
}

/// manifest の image の根 (なければ層をひろげて作る)。config の WorkingDir も作る。根のパスを返す
pub fn rootfs(l: &Layout, manifest: &Value, digest: &str, config: &Value, say: &mut dyn FnMut(&str)) -> io::Result<PathBuf> {
    let hex = hex_of(digest).ok_or_else(|| err("bad digest"))?;
    let dir = base().join("rootfs");
    let root = dir.join(hex);
    if root.is_dir() {
        return Ok(root);
    }
    fs::create_dir_all(&dir)?;
    let tmp = dir.join(format!("{}.tmp-{}", hex, std::process::id()));
    remove_all(&tmp)?;
    fs::create_dir(&tmp)?;
    let mut dirs = BTreeMap::new();
    let layers = manifest["layers"].as_array().ok_or_else(|| err("manifest: no layers"))?;
    let mut skipped = 0;
    for (i, layer) in layers.iter().enumerate() {
        let d = layer["digest"].as_str().ok_or_else(|| err("layer: no digest"))?;
        say(&format!("layer {}/{} {} ({} KiB)", i + 1, layers.len(), &d[..19.min(d.len())], layer["size"].as_u64().unwrap_or(0) / 1024));
        skipped += apply_layer(&tmp, &l.blob(d)?, &mut dirs).map_err(|e| err(format!("layer {}: {}", d, e)))?;
    }
    if skipped > 0 {
        say(&format!("skipped {} entries (devices, fifos, broken links)", skipped));
    }
    // マウントするところ (Docker の init の層と同じ): proc、dev、tmp、etc/hosts と etc/resolv.conf (リンクならファイルに)、WorkingDir
    for d in ["proc", "dev", "tmp", "etc"] {
        let p = inside(&tmp, d).ok_or_else(|| err(d))?;
        if !p.is_dir() {
            remove_all(&p)?;
            fs::create_dir_all(&p)?;
        }
    }
    for f in ["etc/hosts", "etc/resolv.conf"] {
        let p = tmp.join(f);
        if fs::symlink_metadata(&p).map_or(true, |m| !m.is_file()) {
            remove_all(&p)?;
            fs::write(&p, b"")?;
            fs::set_permissions(&p, fs::Permissions::from_mode(0o644))?;
        }
    }
    if let Some(w) = config["config"]["WorkingDir"].as_str().filter(|w| !w.is_empty()) {
        if let Some(p) = inside(&tmp, &format!("{}/x", w)).and_then(|p| p.parent().map(Path::to_path_buf)) {
            fs::create_dir_all(p)?;
        }
    }
    // ディレクトリの mode は終わりに (深いものから)
    for (p, mode) in dirs.iter().rev() {
        let _ = fs::set_permissions(p, fs::Permissions::from_mode(*mode));
    }
    match fs::rename(&tmp, &root) {
        Ok(()) => Ok(root),
        // ほかの aibox が先に作った
        Err(_) if root.is_dir() => {
            remove_all(&tmp)?;
            Ok(root)
        }
        Err(e) => Err(e),
    }
}

/// /etc/passwd の形のファイルから、名前か番号で (uid, gid, home)
pub fn passwd(root: &Path, who: &str) -> Option<(u32, u32, String)> {
    let text = fs::read_to_string(inside(root, "etc/passwd")?).ok()?;
    text.lines().map(|l| l.split(':').collect::<Vec<_>>()).filter(|f| f.len() >= 6).find(|f| f[0] == who || f[2] == who).and_then(|f| Some((f[2].parse().ok()?, f[3].parse().ok()?, f[5].to_string())))
}

/// /etc/group の名前を番号に
pub fn group(root: &Path, name: &str) -> Option<u32> {
    let text = fs::read_to_string(inside(root, "etc/group")?).ok()?;
    text.lines().map(|l| l.split(':').collect::<Vec<_>>()).filter(|f| f.len() >= 3).find(|f| f[0] == name).and_then(|f| f[2].parse().ok())
}

/// 根の中のパス (リンクは根の中でたどる。最後もたどる)
pub fn path_in(root: &Path, p: &str) -> Option<PathBuf> {
    let mut q = inside(root, p)?;
    for _ in 0..40 {
        match fs::symlink_metadata(&q) {
            Ok(m) if m.file_type().is_symlink() => {
                let t = fs::read_link(&q).ok()?.to_string_lossy().to_string();
                let rel = q.strip_prefix(root).ok()?.parent().map(|x| x.to_string_lossy().to_string()).unwrap_or_default();
                q = inside(root, &if t.starts_with('/') { t } else { format!("{}/{}", rel, t) })?;
            }
            _ => return Some(q),
        }
    }
    None
}

/// 置き場の名前の一覧 (名前, manifest か index の digest)
pub fn list(l: &Layout) -> Vec<(String, String)> {
    l.index()["manifests"].as_array().into_iter().flatten().filter_map(|m| Some((m["annotations"]["org.opencontainers.image.ref.name"].as_str()?.to_string(), m["digest"].as_str()?.to_string()))).collect()
}

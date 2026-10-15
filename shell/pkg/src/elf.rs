// ELF の動的なリンクのところを読む (object クレート)。pkg_test の copies と needs、pkg_build の soname の見はりで使う
//   needs: NEEDED のライブラリが、パッケージと depends (と musl) の中にみなあるか (なければ aios の上で動かない)
//   soname: ライブラリの新しい版で SONAME が変わる (libfoo.so.1 → .2) と、前のものを NEEDED に書いたパッケージは
//   作りなおすまで動かない
//   copies: PIC でないプログラム (ET_EXEC) は、ライブラリのデータ (stdout、environ など) を COPY の再配置で
//   自分の中に写す。lld は、ライブラリで同じ番地にある名前を別名と見て、1 つの場所にまとめる (environ と __environ は
//   同じ場所でないと、プログラムと musl が別のものを見る)。Zig 0.17 の libc.so の見本 (lib/libc/musl/libc.S) は
//   データがみな同じ番地なので、zig.sh で直す前に作ったものは stdin と stdout と stderr が 1 つになり
//   (pcre2grep の答えが標準エラーに出る)、__stack_chk_guard と __environ も重なった (VLC が setenv で落ちた)。
//   ほんとうのライブラリ (musl の libc.so など) の番地とくらべて、別のものなのにまとめたものを見つける
use object::read::elf::{Dyn, ElfFile64, FileHeader};
use object::{Object, ObjectSymbol, RelocationFlags, RelocationTarget, elf};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::{Path, PathBuf};

/// ライブラリを探すところ (musl の etc/ld-musl-aarch64.path と同じ。lib は usr/lib)
const LIB_DIRS: [&str; 4] = ["usr/lib", "usr/local/lib", "opt/c/lib", "lib"];

/// ファイルを読むだけで写す (読むのは触ったページだけ。libxul は 170 MB ある)
struct Map {
    ptr: *mut libc::c_void,
    len: usize,
}

impl Map {
    fn open(path: &Path) -> Option<Map> {
        use std::os::unix::io::AsRawFd;
        let f = std::fs::File::open(path).ok()?;
        let len = f.metadata().ok().filter(|m| m.is_file())?.len() as usize;
        if len < 64 {
            return None;
        }
        let ptr = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_PRIVATE, f.as_raw_fd(), 0) };
        (ptr != libc::MAP_FAILED).then_some(Map { ptr, len })
    }

    fn data(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr as *const u8, self.len) }
    }
}

impl Drop for Map {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.ptr, self.len) };
    }
}

/// ELF 1 つの動的なリンクのところ
#[derive(Default)]
pub struct Info {
    pub soname: Option<String>,
    pub needed: Vec<String>,
    pub runpath: Vec<String>,
    /// 定義している動的なシンボル: 名前 → 番地
    defs: HashMap<String, u64>,
    /// COPY の再配置: 写した場所 → (写した名前, その場所にある名前みな)
    copies: BTreeMap<u64, (String, Vec<String>)>,
}

impl Info {
    /// ライブラリのデータを COPY で写しているか
    pub fn copies(&self) -> bool {
        !self.copies.is_empty()
    }
}

/// 64 ビットのリトルエンディアンの ELF (aarch64、x86_64、riscv64) なら読む
pub fn read(path: &Path) -> Option<Info> {
    let map = Map::open(path)?;
    let data = map.data();
    if !data.starts_with(b"\x7fELF\x02\x01") {
        return None;
    }
    let f = ElfFile64::<object::Endianness>::parse(data).ok()?;
    let endian = f.endian();
    let head = f.elf_header();
    let mut info = Info::default();
    let sections = f.elf_section_table();
    if let Ok(Some((dyns, link))) = sections.dynamic(endian, data)
        && let Ok(strs) = sections.strings(endian, data, link)
    {
        for d in dyns {
            let s = || d.string(endian, strs).ok().map(|b| String::from_utf8_lossy(b).into_owned());
            match d.tag32(endian) {
                Some(elf::DT_NULL) => break,
                Some(elf::DT_NEEDED) => info.needed.extend(s()),
                Some(elf::DT_SONAME) => info.soname = s(),
                Some(elf::DT_RUNPATH | elf::DT_RPATH) => info.runpath.extend(s().iter().flat_map(|p| p.split(':')).filter(|p| !p.is_empty()).map(String::from)),
                _ => {}
            }
        }
    }
    let mut at: HashMap<u64, Vec<String>> = HashMap::new();
    let mut names: HashMap<usize, String> = HashMap::new();
    for s in f.dynamic_symbols() {
        let Ok(n) = s.name() else { continue };
        if n.is_empty() {
            continue;
        }
        names.insert(s.index().0, n.to_string());
        if !s.is_undefined() {
            info.defs.insert(n.to_string(), s.address());
            at.entry(s.address()).or_default().push(n.to_string());
        }
    }
    let copy = match head.e_machine(endian) {
        elf::EM_AARCH64 => elf::R_AARCH64_COPY,
        elf::EM_X86_64 => elf::R_X86_64_COPY,
        elf::EM_RISCV => elf::R_RISCV_COPY,
        _ => return Some(info),
    };
    for (off, r) in f.dynamic_relocations().into_iter().flatten() {
        if let (RelocationFlags::Elf { r_type }, RelocationTarget::Symbol(i)) = (r.flags(), r.target())
            && r_type == copy
            && let Some(n) = names.get(&i.0)
        {
            let mut all = at.get(&off).cloned().unwrap_or_default();
            all.sort();
            info.copies.insert(off, (n.clone(), all));
        }
    }
    Some(info)
}

/// 別のものなのに 1 つの場所にまとめた COPY: ライブラリ (NEEDED の名前) と、ライブラリでの番地ごとの名前の組
/// (同じ組の名前はライブラリでも同じ番地にある、ほんとうの別名)
pub struct Shared {
    pub lib: String,
    pub sets: Vec<Vec<String>>,
}

/// ライブラリを読んだもの (広げたところの下から探す)
pub struct Libs {
    roots: Vec<PathBuf>,
    read: HashMap<PathBuf, Option<Info>>,
}

impl Libs {
    pub fn new(roots: Vec<PathBuf>) -> Libs {
        Libs { roots, read: HashMap::new() }
    }

    /// NEEDED の name を探す: needer の RUNPATH ($ORIGIN は needer のあるところ。/ で始まるものは広げたところの下) と LIB_DIRS
    fn find(&self, name: &str, needer: &Path, runpath: &[String]) -> Option<PathBuf> {
        let origin = needer.parent().unwrap_or(Path::new("/"));
        let mut dirs: Vec<PathBuf> = Vec::new();
        for r in runpath {
            if let Some(rest) = r.strip_prefix("$ORIGIN").or_else(|| r.strip_prefix("${ORIGIN}")) {
                dirs.push(origin.join(rest.trim_start_matches('/')));
            } else {
                dirs.extend(self.roots.iter().map(|root| root.join(r.trim_start_matches('/'))));
            }
        }
        dirs.extend(self.roots.iter().flat_map(|root| LIB_DIRS.iter().map(move |d| root.join(d))));
        dirs.into_iter().find_map(|d| self.real(&d.join(name)))
    }

    /// p (広げたところの下) のリンクをたどったファイル。/ で始まるリンク先 (ld-musl-aarch64.so.1 → /usr/lib/libc.so など) は
    /// 広げたところの下として (このマシンのものを読まないように)
    fn real(&self, p: &Path) -> Option<PathBuf> {
        let root = self.roots.iter().find(|r| p.starts_with(r))?;
        let mut p = p.to_path_buf();
        for _ in 0..16 {
            let m = std::fs::symlink_metadata(&p).ok()?;
            if m.is_file() {
                return Some(p);
            }
            if !m.file_type().is_symlink() {
                return None;
            }
            let t = std::fs::read_link(&p).ok()?;
            p = match t.strip_prefix("/") {
                Ok(abs) => root.join(abs),
                Err(_) => p.parent()?.join(t),
            };
        }
        None
    }

    /// プログラムの使うライブラリを、ld.so と同じ順 (NEEDED を幅優先) に
    fn order(&mut self, exe: &Path, info: &Info) -> Vec<(String, PathBuf)> {
        let mut out: Vec<(String, PathBuf)> = Vec::new();
        let mut todo: VecDeque<(String, PathBuf, Vec<String>)> = info.needed.iter().map(|n| (n.clone(), exe.to_path_buf(), info.runpath.clone())).collect();
        while let Some((name, needer, runpath)) = todo.pop_front() {
            if out.iter().any(|(n, _)| *n == name) {
                continue;
            }
            let Some(p) = self.find(&name, &needer, &runpath) else { continue };
            let lib = self.read.entry(p.clone()).or_insert_with(|| read(&p));
            if let Some(l) = lib {
                todo.extend(l.needed.iter().map(|n| (n.clone(), p.clone(), l.runpath.clone())));
            }
            out.push((name, p));
        }
        out
    }

    /// exe (読んだものが info) の COPY で、ライブラリでは別の番地なのにまとめたもの
    pub fn shared(&mut self, exe: &Path, info: &Info) -> Vec<Shared> {
        if info.copies.values().all(|(_, all)| all.len() < 2) {
            return vec![];
        }
        let order = self.order(exe, info);
        let mut out = Vec::new();
        for (copied, all) in info.copies.values() {
            if all.len() < 2 {
                continue;
            }
            // 写したもの (copied) を定義している、いちばん前のライブラリ
            let Some((lib, defs)) = order.iter().find_map(|(n, p)| self.read.get(p).and_then(Option::as_ref).filter(|l| l.defs.contains_key(copied)).map(|l| (n, &l.defs))) else { continue };
            let mut sets: BTreeMap<u64, Vec<String>> = BTreeMap::new();
            for n in all {
                if let Some(a) = defs.get(n) {
                    sets.entry(*a).or_default().push(n.clone());
                }
            }
            if sets.len() > 1 {
                out.push(Shared { lib: lib.clone(), sets: sets.into_values().collect() });
            }
        }
        out
    }
}

/// パッケージのライブラリと、使うライブラリ
#[derive(Default, Clone, PartialEq, Debug)]
pub struct Dynamic {
    /// SONAME (ほかのものが NEEDED に書く名前)
    pub sonames: Vec<String>,
    /// SONAME と *.so* のファイル (リンクも) の名前。ld.so は NEEDED をファイルの名前で探す (SONAME のない musl の
    /// libc.so も、ファイルの名前が NEEDED になる)
    pub provides: Vec<String>,
    /// NEEDED のうち、このパッケージの中にないもの (ほかのパッケージから使う)
    pub needs: Vec<String>,
}

/// 広げたところ dir の、パッケージのファイル files (dir の下) の Dynamic
pub fn dynamic(dir: &Path, files: &[PathBuf]) -> Dynamic {
    let (mut sonames, mut provides, mut needed) = (BTreeSet::new(), BTreeSet::new(), BTreeSet::new());
    for r in files {
        let name = r.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if name.contains(".so") {
            provides.insert(name);
        }
        let p = dir.join(r);
        if !std::fs::symlink_metadata(&p).is_ok_and(|m| m.is_file()) {
            continue;
        }
        let Some(info) = read(&p) else { continue };
        if let Some(s) = info.soname {
            provides.insert(s.clone());
            sonames.insert(s);
        }
        needed.extend(info.needed);
    }
    let needs = needed.into_iter().filter(|n| !provides.contains(n)).collect();
    Dynamic { sonames: sonames.into_iter().collect(), provides: provides.into_iter().collect(), needs }
}

/// 1 行で: stdin, stdout, stderr (libc.so)。別名は = でつなぐ (__environ=environ)。多ければ n 組だけ
pub fn describe(s: &Shared, n: usize) -> String {
    let mut sets: Vec<String> = s.sets.iter().take(n).map(|v| v.join("=")).collect();
    if s.sets.len() > n {
        sets.push(format!("… ({} in all)", s.sets.len()));
    }
    format!("{} ({})", sets.join(", "), s.lib)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describe_sets() {
        let s = Shared { lib: "libc.so".into(), sets: vec![vec!["stdin".into()], vec!["stdout".into()], vec!["___environ".into(), "__environ".into(), "environ".into()]] };
        assert_eq!(describe(&s, 8), "stdin, stdout, ___environ=__environ=environ (libc.so)");
        assert_eq!(describe(&s, 2), "stdin, stdout, … (3 in all) (libc.so)");
    }

    /// ライブラリの名前: *.so* のファイルとリンク。NEEDED のうちパッケージの中にあるものは needs に入れない
    #[test]
    fn dynamic_of_a_dir() {
        let me = std::env::current_exe().unwrap();
        let Some(info) = read(&me) else { return };
        let dir = std::env::temp_dir().join(format!("aish-pkg-dynamic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("usr/lib")).unwrap();
        std::fs::copy(&me, dir.join("usr/lib/libme.so.1.2")).unwrap();
        std::os::unix::fs::symlink("libme.so.1.2", dir.join("usr/lib/libme.so.1")).unwrap();
        let files = [PathBuf::from("usr/lib/libme.so.1.2"), PathBuf::from("usr/lib/libme.so.1")];
        let d = dynamic(&dir, &files);
        assert_eq!(d.provides, ["libme.so.1", "libme.so.1.2"]);
        let mut want: Vec<String> = info.needed.clone();
        want.sort();
        want.dedup();
        assert_eq!(d.needs, want);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// このテストのプログラム自身 (cargo test の、ふつうは PIE) は読めて、COPY でまちがってまとめたものはない
    #[test]
    fn reads_itself() {
        let me = std::env::current_exe().unwrap();
        if let Some(info) = read(&me) {
            assert!(info.copies.len() < 1000);
            assert!(Libs::new(vec![PathBuf::from("/")]).shared(&me, &info).is_empty());
        }
    }
}

// landlock の砂場をいまのプロセスにかける (aibox、aish --mcp)。カーネルの landlock.rs と同じ形
//   読む・動かすのはどこでも、書く (作る・消す・名前を変える) のは write に書いたところの下だけ。
//   net が Some なら、TCP でつなげる口はそこに書いたものだけ (空ならどこへもつなげない)
//   かけたら外せない。子にも exec のあとにも引き継がれ、setuid (sudo) でも外へ出られない
use std::ffi::CString;

const CREATE_RULESET: libc::c_long = 444;
const ADD_RULE: libc::c_long = 445;
const RESTRICT_SELF: libc::c_long = 446;
const PATH_BENEATH: libc::c_long = 1;
const NET_PORT: libc::c_long = 2;

const EXECUTE: u64 = 1 << 0;
const WRITE_FILE: u64 = 1 << 1;
const READ_FILE: u64 = 1 << 2;
const READ_DIR: u64 = 1 << 3;
const TRUNCATE: u64 = 1 << 14;
/// ABI 4 までのファイルの権利ぜんぶ
const FS_ALL: u64 = (1 << 15) - 1;
const READ: u64 = EXECUTE | READ_FILE | READ_DIR;
/// ファイル (ディレクトリでないもの) に書ける権利
const FILE_RW: u64 = EXECUTE | WRITE_FILE | READ_FILE | TRUNCATE;
const CONNECT_TCP: u64 = 1 << 1;
const BIND_TCP: u64 = 1 << 0;

fn err(what: &str) -> String {
    format!("{}: {}", what, std::io::Error::last_os_error())
}

/// カーネルの landlock の版 (0 ならない)
pub fn abi() -> i64 {
    let r = unsafe { libc::syscall(CREATE_RULESET, std::ptr::null::<u8>(), 0usize, 1u32) };
    if r < 0 { 0 } else { r }
}

/// path に rights を許す決まりを足す (なければ false)
fn allow(fd: i32, path: &str, rights: u64) -> Result<bool, String> {
    let c = CString::new(path).map_err(|e| e.to_string())?;
    let pfd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
    if pfd < 0 {
        return Ok(false);
    }
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    unsafe { libc::fstat(pfd, &mut st) };
    let rights = if st.st_mode & libc::S_IFMT == libc::S_IFDIR { rights } else { rights & FILE_RW };
    // struct landlock_path_beneath_attr (packed): u64 + i32
    let mut b = [0u8; 12];
    b[..8].copy_from_slice(&rights.to_le_bytes());
    b[8..].copy_from_slice(&pfd.to_le_bytes());
    let r = unsafe { libc::syscall(ADD_RULE, fd, PATH_BENEATH, b.as_ptr(), 0u32) };
    unsafe { libc::close(pfd) };
    if r < 0 { Err(err(path)) } else { Ok(true) }
}

/// 砂場をかける。返すのは、なくて許せなかった write のパス
pub fn restrict(write: &[String], net: Option<&[u16]>) -> Result<Vec<String>, String> {
    if abi() < 1 {
        return Err("landlock is not available in this kernel".into());
    }
    let handled_net = if net.is_some() { CONNECT_TCP | BIND_TCP } else { 0 };
    let attr: [u64; 2] = [FS_ALL, handled_net];
    let fd = unsafe { libc::syscall(CREATE_RULESET, attr.as_ptr(), 16usize, 0u32) } as i32;
    if fd < 0 {
        return Err(err("landlock_create_ruleset"));
    }
    allow(fd, "/", READ)?;
    let mut missing = Vec::new();
    for w in write {
        if !allow(fd, w, FS_ALL)? {
            missing.push(w.clone());
        }
    }
    for &port in net.unwrap_or(&[]) {
        let attr: [u64; 2] = [CONNECT_TCP, port as u64];
        if unsafe { libc::syscall(ADD_RULE, fd, NET_PORT, attr.as_ptr(), 0u32) } < 0 {
            return Err(err(&format!("port {}", port)));
        }
    }
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } < 0 {
        return Err(err("prctl(PR_SET_NO_NEW_PRIVS)"));
    }
    if unsafe { libc::syscall(RESTRICT_SELF, fd, 0u32) } < 0 {
        return Err(err("landlock_restrict_self"));
    }
    unsafe { libc::close(fd) };
    Ok(missing)
}

/// 書けるところの既定: いまのディレクトリ、/tmp、/dev (端末や /dev/null)
pub fn default_write() -> Vec<String> {
    let mut v: Vec<String> = std::env::current_dir().ok().map(|d| d.display().to_string()).into_iter().collect();
    v.push("/tmp".into());
    v.push("/dev".into());
    v
}

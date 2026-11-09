// しくじったわけ: 書くほう (core)。aios だけのもの: Linux はシステムコールがしくじると errno (EACCES、ENOENT ...)
// だけを返し、パスのどこで止まったのか、どの許しが足りなかったのか、どの決まりが止めたのかは、だれも教えてくれない
// (strace でも errno まで)。aios は返す errno もふるまいも Linux と同じまま、わけをカーネルの中に残す。
//   しくじるところ (cred の許し、vfs のパスをたどるところ、seccomp、landlock、fail、切ったところ ...) が note で、いまの
//   スレッドのいまのシステムコールのわけを書く (Proc の why。システムコールのはじめに clear で消す)。パスは at で足す。
//   EINVAL を返すところはみな `Err(why::inval())` なので、core のファイルもこれを使う: ここは Proc の why に書くだけで、
//   ためておく輪と /proc/ai/why (読むほう) は whylog.rs (extra)。出口 (syscall::dispatch) で whylog::done が輪に入れる

/// わけにつけるパスの長さ (長いものは前を切る: 終わりのほうがわけに近い)
pub const PATH: usize = 120;
/// 名前 (たどれなかった 1 つ) の長さ
const NAME: usize = 64;

/// わけの種類
#[derive(Clone, Copy, PartialEq, Debug)]
#[repr(u8)]
pub enum Code {
    None = 0,
    /// 許しが足りない (want の r/w/x が、mode・持ち主のものに、呼んだ人の uid/gid でない)。path はそのファイルかディレクトリ
    Perm,
    /// ない: path まではあって、その中に name がない
    Missing,
    /// パスの覚え (ないものも覚える) から答えた ENOENT。path はパスぜんぶ
    MissingCached,
    /// ディレクトリでないもの (path) の下をたどろうとした
    NotDir,
    /// シンボリックリンクが多すぎる (40 より) か、たどってはいけないところのリンク
    Loop,
    /// 名前 (name) が 255 バイトより長い
    NameTooLong,
    /// seccomp のフィルタがこの errno を返させた
    Seccomp,
    /// landlock の決まり (num は足りなかった LANDLOCK_ACCESS_*、want が 1 なら TCP の口で path は "port N")
    Landlock,
    /// /proc/ai/ctl の fail でわざと
    Injected,
    /// 切ったところ (aios.off= か ctl part) のシステムコール
    PartOff,
    /// aios にないシステムコール
    NoSys,
    /// readlink したものがシンボリックリンクでない (EINVAL)。path はたのんだパス
    NotLink,
    /// fd (num) が開いていない (EBADF)
    BadFd,
    /// EINVAL を返したカーネルのソースの場所 (name はファイル、num は行。そこに断ったわけが書いてある)
    Where,
    /// ファイアウォール (nftables) の決まりが止めた。path は "nftables FAMILY TABLE CHAIN handle N" (policy なら
    /// "policy drop")
    Firewall,
}

/// Code の名前 (Code の順。/proc/ai/why の code)
pub const CODES: [&str; 16] = ["", "perm", "missing", "missing_cached", "not_dir", "loop", "name_too_long", "seccomp", "landlock", "injected", "part_off", "no_sys", "not_link", "bad_fd", "where", "firewall"];

/// 1 つのわけ (Proc に 1 つ。輪にも)
#[derive(Clone, Copy)]
pub struct Note {
    pub code: Code,
    /// このわけで返る errno (正の数)
    pub errno: u16,
    /// Perm: ほしかったもの (cred::R W X のビット)
    pub want: u8,
    pub plen: u8,
    pub nlen: u8,
    /// path は、わけを書いたところではなく、プログラムがたのんだパス (システムコールの引数。whylog::done がつけた)
    pub asked: bool,
    /// Perm: そのものの mode・uid・gid と、呼んだ人の (実効の) uid・gid。ほかでは num に数
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub cuid: u32,
    pub cgid: u32,
    pub num: u64,
    pub path: [u8; PATH],
    pub name: [u8; NAME],
}

impl Note {
    /// システムコールのはじめに (syscall::dispatch): 前のわけを消す
    #[inline]
    pub fn clear(&mut self) {
        self.code = Code::None;
    }
}

pub const NONE: Note = Note { code: Code::None, errno: 0, want: 0, plen: 0, nlen: 0, asked: false, mode: 0, uid: 0, gid: 0, cuid: 0, cgid: 0, num: 0, path: [0; PATH], name: [0; NAME] };

pub fn copy<const L: usize>(dst: &mut [u8; L], s: &str) -> u8 {
    // 長ければ終わりのほうを (文字の途中で切らない)
    let mut b = s.as_bytes();
    if b.len() > L {
        let mut i = b.len() - L;
        while i < b.len() && b[i] & 0xc0 == 0x80 {
            i += 1;
        }
        b = &b[i..];
    }
    dst[..b.len()].copy_from_slice(b);
    b.len() as u8
}

fn me() -> Option<&'static mut Note> {
    crate::proc::try_current().map(|p| &mut p.why)
}

/// 許しが足りなかった (cred::check。access(2) は real で実 uid/gid)。num に、答えを決めた組 (Cred::class)
#[cold]
pub fn perm(m: &crate::vfs::Meta, want: u32, c: &crate::cred::Cred, real: bool) {
    if let Some(n) = me() {
        let (cuid, cgid) = if real { (c.uid, c.gid) } else { (c.euid, c.egid) };
        *n = Note { code: Code::Perm, errno: crate::cred::EACCES as u16, want: want as u8, mode: m.mode, uid: m.uid, gid: m.gid, cuid, cgid, num: c.class(m, real) as u64, ..NONE };
    }
}

/// わけを書く (path と name はあとから at で)
#[cold]
pub fn note(code: Code, errno: i64, num: u64) {
    if let Some(n) = me() {
        *n = Note { code, errno: errno.unsigned_abs() as u16, num, ..NONE };
    }
}

/// EINVAL を返すところ (`Err(crate::why::inval())`): どこで断ったか (カーネルのソースのファイルと行) を、まだわけが
/// なければ残す。EINVAL は Linux でもわけがいくつもあって (知らないフラグ、大きさ、そろっていない場所 ...) 番号だけでは
/// わからないので、その行 (と、その上に書いたわけ) を見られるように。-EINVAL を返す
#[track_caller]
#[cold]
pub fn inval() -> i64 {
    let l = core::panic::Location::caller();
    if let Some(n) = me()
        && (n.code == Code::None || n.errno != 22)
    {
        *n = Note { code: Code::Where, errno: 22, num: l.line() as u64, ..NONE };
        n.nlen = copy(&mut n.name, l.file());
    }
    -22
}

/// readlink したものがシンボリックリンクでない。path はたのんだパス。-EINVAL を返す
#[cold]
pub fn not_link(path: &str) -> i64 {
    if let Some(n) = me() {
        *n = Note { code: Code::NotLink, errno: 22, asked: true, ..NONE };
        n.plen = copy(&mut n.path, path);
    }
    -22
}

/// fd が開いていない。-EBADF を返す
#[cold]
pub fn bad_fd(fd: u64) -> i64 {
    if let Some(n) = me() {
        *n = Note { code: Code::BadFd, errno: 9, num: fd, ..NONE };
    }
    -9
}

/// いまのわけに、どこで (path: そこまでたどったところ、name: 次の名前) をつける。まだついていなければ。e をそのまま返す
/// (`.map_err(|e| why::at(e, &walked, ""))` のように)。わけがなく e が ENOENT・ENOTDIR・ELOOP・ENAMETOOLONG なら、そのわけも
#[cold]
pub fn at(e: i64, path: &str, name: &str) -> i64 {
    if let Some(n) = me() {
        if n.code == Code::None || n.errno as i64 != -e {
            let code = match -e {
                2 => Code::Missing,
                20 => Code::NotDir,
                40 => Code::Loop,
                36 => Code::NameTooLong,
                _ => return e,
            };
            *n = Note { code, errno: (-e) as u16, ..NONE };
        }
        if n.plen == 0 && n.nlen == 0 {
            n.plen = copy(&mut n.path, path);
            n.nlen = copy(&mut n.name, name);
        }
    }
    e
}

/// landlock の決まりが止めた (landlock::check_fs と check_net)。access は足りなかった LANDLOCK_ACCESS_*、net なら TCP の口
/// (what は "port 80")、でなければ what はパス。EACCES を返す
#[cold]
pub fn landlock(access: u64, net: bool, what: &str) -> i64 {
    if let Some(n) = me() {
        *n = Note { code: Code::Landlock, errno: crate::cred::EACCES as u16, want: net as u8, num: access, ..NONE };
        n.plen = copy(&mut n.path, what);
    }
    -crate::cred::EACCES
}

/// landlock の scope か permission が止めた (抽象 unix ソケット、シグナル、ptrace、namespace、capability)。what は
/// 止めたこと。EPERM を返す
#[cold]
pub fn landlock_scope(what: &str) -> i64 {
    if let Some(n) = me() {
        *n = Note { code: Code::Landlock, errno: crate::cred::EPERM as u16, want: 2, ..NONE };
        n.plen = copy(&mut n.path, what);
    }
    -crate::cred::EPERM
}

/// ファイアウォールが、いまのシステムコールのパケットを落とした (nfhook.rs の道から)。errno は返すときに決まる
/// (sendmsg の EPERM、connect の ECONNREFUSED など) ので、done がそのとき合わせる
#[cold]
pub fn firewall(what: &str) {
    if let Some(n) = me() {
        *n = Note { code: Code::Firewall, errno: 0, ..NONE };
        n.plen = copy(&mut n.path, what);
    }
}

/// パスの覚えにあった「ない」 (vfs::lookup)。path はパスぜんぶ。ENOENT を返す
#[cold]
pub fn missing_cached(path: &str) -> i64 {
    if let Some(n) = me() {
        *n = Note { code: Code::MissingCached, errno: 2, ..NONE };
        n.plen = copy(&mut n.path, path);
    }
    -2
}

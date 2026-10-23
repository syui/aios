// 大きなロックなしの作る・消す: O_CREAT の openat と unlinkat (syscall::fast から)。
// ふつうの道は大きなロック (smp.rs) の中で、パスをたどり (vfs)、ext4 を書きかえ、fd を置く。いくつもの CPU でそれぞれの
// ディレクトリにファイルを作って消すと、1 つずつしか動かない (bench/smp.md の create-unlink: 4 つで Linux の 1.47 倍の時間。
// Linux はディレクトリごとの i_rwsem と、グループごとのロックで、ちがうディレクトリなら同時に動く)。
// ここでは、ext4 の中は ext4 のロックをいっしょに持って (extfs.rs の fs_lock_shared: ちがうディレクトリなら同時に、同じ
// ディレクトリはディレクトリごとのロックで 1 つずつ)、そのほかはロックなしで:
//   - 親のディレクトリは statc の表で引く (ふつうの道の O_CREAT の openat と unlinkat が statc::learn_dir で覚えさせる)。
//     通ったディレクトリの版とマウントの版が同じなら、パスはいまもその inode をさす。許し (書けて通れるか) と、
//     いまもディレクトリかは、ext4 のロックの中で inode から読む
//   - ext4 の中は ExtFs::fast_create / fast_unlink: ふつうの道と同じ書きかたで、書きかえる前にみな確かめる (読むブロックが
//     みなキャッシュにある、名前がある / ない、すき間がある、使われていない ...)。どれかがだめなら何も変えずにふつうの道へ
//   - 名前が変わったことは、ふつうの道と同じく vfs::name_changed で、書きかえたあとに知らせる (パスの覚えの版、getdents の
//     版、statc の版の順)。statc の表に書くふつうの道は、statc の版を読んでから、パスの覚えがいまも正しいかを確かめる
//     (vfs::fast_stat_trail) ので、引いたあとにここで変えたものは、確かめで落ちるか、読んだ版が古いかのどちらか
//   - fd は、大きなロックなしの open (file::fast_open) と同じく、表ごとのスピンロックの中で、いちばん小さい空いたところに
// ふつうの道へ行くもの (答えはふつうの道が出す: しくじりのわけも /proc/ai/why に): 作るもの・消すもののできごとを待つ
// inotify の見張りがある (置かれたことがある)、砂場 (landlock)、読むだけのマウントがある、fail の決まりがある、fd の表を
// ほかと分けている、ext4 でない、パスが statc の表で引けない (.. のあるもの、/dev や /proc、開いたディレクトリからのもの)、
// 名前がもうある (O_CREAT だけなら開く)、ないものを消す、ディレクトリ、索引 (htree) のあるディレクトリ、いま使われている
// もの (開いている、パスの覚えにある、棚にある)、読むブロックがキャッシュにない、そろそろ書き出すころ
use crate::file::{self, Kind};
use crate::proc::Proc;
use core::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// 数: [0] 作った、[1] 消した、ふつうの道へ: [2] 形 (フラグ、見張り、砂場、マウント、fd の表)、[3] 親が statc の表にない、
/// [4] ext4 が断った (名前がある / ない、すき間、キャッシュにない、使われている、許し ...)。/proc/bkl に出す
pub static FAST_NS: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];

fn note(i: usize) {
    FAST_NS[i].fetch_add(1, Relaxed);
}

const O_ACCMODE: u64 = 0o3;
const O_CREAT: u64 = 0o100;
const O_EXCL: u64 = 0o200;
const O_NOCTTY: u64 = 0o400;
const O_TRUNC: u64 = 0o1000;
const O_LARGEFILE: u64 = 0o400000;
const O_CLOEXEC: u64 = 0o2000000;

/// どちらにも要るもの (ロックの外で見られるもの)。だめなら None
fn usable(me: &Proc) -> Option<&'static alloc::rc::Rc<crate::extfs::ExtFs>> {
    if crate::fail::ON.load(Relaxed) || me.cred.landlock.is_some() || !watch_free() {
        return None;
    }
    if !me.files.as_ref()?.private() {
        return None;
    }
    crate::extfs::ExtFs::root_fs()
}

/// 作る・消すのできごとを待つ見張りも、読むだけのマウントもない (ext4 のロックの中でもう一度見る: inotify::add_watch と
/// vfs::mount_ro は、印を立ててから ext4 のロックを取って、いま書きかえているものが終わるのを待つ)
fn watch_free() -> bool {
    !crate::inotify::NAME_WATCHED.load(Relaxed) && !file::OPEN_WATCHED.load(Relaxed) && !crate::vfs::RO_MOUNTED.load(Relaxed)
}

/// ユーザーのパスを buf に写して、(親のディレクトリの部分の長さ, 最後の名前) に分ける (/name の親は /、name の親は
/// いまのディレクトリ: 長さ 0)。最後が / のもの、. や .. は None
fn split<'a>(me: &Proc, pathp: usize, buf: &'a mut [u8; 256]) -> Option<(usize, &'a str)> {
    let n = crate::statc::user_path(me, pathp, buf)?;
    let path = &buf[..n];
    if path.is_empty() || path.ends_with(b"/") {
        return None;
    }
    let cut = path.iter().rposition(|&b| b == b'/');
    let name = core::str::from_utf8(&path[cut.map_or(0, |c| c + 1)..]).ok()?;
    if name == "." || name == ".." || name.len() > 255 {
        return None;
    }
    let dlen = match cut {
        Some(0) => 1,
        Some(c) => c,
        None => 0,
    };
    Some((dlen, name))
}

/// 名前を別のところへ写す (buf の親の部分を statc に渡すあいだも使えるように)
fn copy_name<'a>(name: &str, out: &'a mut [u8; 256]) -> &'a str {
    out[..name.len()].copy_from_slice(name.as_bytes());
    core::str::from_utf8(&out[..name.len()]).unwrap_or("")
}

/// syscall::fast の openat で O_CREAT のもの (file::fast_open が引けなかったとき)
pub fn fast_create(me: &Proc, dirfd: i64, pathp: usize, flags: u64, mode: u64) -> Option<i64> {
    if flags & O_CREAT == 0 {
        return None;
    }
    if flags & !(O_ACCMODE | O_CREAT | O_EXCL | O_NOCTTY | O_TRUNC | O_LARGEFILE | O_CLOEXEC) != 0 || flags & O_ACCMODE == O_ACCMODE {
        note(2);
        return None;
    }
    let Some(fs) = usable(me) else {
        note(2);
        return None;
    };
    let files = me.files.as_ref()?;
    let t = files.get();
    // いちばん小さい空いた fd (表を伸ばすのはふつうの道で)。表はこのスレッドだけのものなので、作るあいだも空いたまま
    let Some(fd) = (0..t.fds.len().min(t.nofile)).find(|&i| t.fds[i].is_none()) else {
        note(2);
        return None;
    };
    let mut buf = [0u8; 256];
    let mut name_buf = [0u8; 256];
    let Some((dlen, name)) = split(me, pathp, &mut buf).map(|(d, n)| (d, copy_name(n, &mut name_buf))) else {
        note(2);
        return None;
    };
    let mut norm = [0u8; crate::statc::PATH];
    let Some((dir, klen)) = crate::statc::dir_lookup(me, dirfd, &buf[..dlen], &mut norm) else {
        note(3);
        return None;
    };
    let mode = mode as u32 & 0o7777 & !t.umask;
    // 作る前の (親, 名前) の箱の版 (作ったあとに statc::learn_new で、新しい名前を表に書くかどうか)
    let before = crate::statc::box_ver(crate::statc::name_box((fs.fs_id(), dir), crate::vfs::name_hash(name)));
    let ino = {
        let _e = crate::extfs::fs_lock_shared();
        if !watch_free() { None } else { fs.fast_create(dir as u32, name, mode, &me.cred) }
    };
    let Some((ino, st, ver)) = ino else {
        note(4);
        return None;
    };
    // 書きかえたあとに (ext4 のロックの外でよい: 版を進めるのは、書きかえたあとならいつでも)
    crate::vfs::name_changed((fs.fs_id(), dir), name);
    // 作ったばかりのファイルの stat も、ロックなしで答えられるように (Linux の d_instantiate)
    crate::statc::learn_new(&norm[..klen], name, (fs.fs_id(), dir), before, ino.id(), st, ver);
    // 開いたファイル (ふつうの道の open_found と同じ: パスは先頭 / なしの畳んだもの)
    let key = core::str::from_utf8(&norm[..klen]).unwrap_or("");
    let full = if key.is_empty() { alloc::string::String::from(name) } else { alloc::format!("{}/{}", key, name) };
    let f = file::new(Kind::Inode(ino, full), crate::sysfile::file_flags(flags));
    file::fast_install(t, fd, f, flags & O_CLOEXEC != 0);
    note(0);
    Some(fd as i64)
}

/// syscall::fast の unlinkat (flags が 0 のもの)
pub fn fast_unlink(me: &Proc, dirfd: i64, pathp: usize, flags: u64) -> Option<i64> {
    if flags != 0 {
        note(2);
        return None;
    }
    let Some(fs) = usable(me) else {
        note(2);
        return None;
    };
    let mut buf = [0u8; 256];
    let mut name_buf = [0u8; 256];
    let Some((dlen, name)) = split(me, pathp, &mut buf).map(|(d, n)| (d, copy_name(n, &mut name_buf))) else {
        note(2);
        return None;
    };
    let mut norm = [0u8; crate::statc::PATH];
    let Some((dir, _)) = crate::statc::dir_lookup(me, dirfd, &buf[..dlen], &mut norm) else {
        note(3);
        return None;
    };
    use crate::extfs::FastUnlink;
    let mut done = {
        let _e = crate::extfs::fs_lock_shared();
        if watch_free() { fs.fast_unlink(dir as u32, name, &me.cred) } else { FastUnlink::No }
    };
    // リンクがいくつもあるか、中身のブロックがあるもの: ext4 のロックをひとりで持って
    if done == FastUnlink::Alone {
        let _e = crate::extfs::fs_lock();
        done = if watch_free() { fs.fast_unlink(dir as u32, name, &me.cred) } else { FastUnlink::No };
    }
    if done != FastUnlink::Done {
        note(4);
        return None;
    }
    crate::vfs::name_changed((fs.fs_id(), dir), name);
    note(1);
    Some(0)
}

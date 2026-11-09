// splice、tee、sendfile、copy_file_range (Linux の fs/splice.c と、fs/read_write.c の do_sendfile と vfs_copy_file_range)。
// パイプの箱をじかにあやつるもの (Linux の fs/splice.c が include/linux/pipe_fs_i.h の形で pipe_inode_info を見るのと同じく、
// file::Pipe の中を見る) もここ。パイプそのものは file.rs (Linux の fs/pipe.c)。main.rs が syscall::register と
// syscall::register_fast_splice (大きなロックなしの splice) でつなぐ
use crate::file::{self, FileRef, Kind, PageQueue, Pipe, PipeCell, EBADF, EPIPE, FAST_RW, O_PATH};
use crate::memlayout::PGSIZE;
use crate::proc;
use crate::sysfile::{file_of, inode_path, ops_of, out, pipe_of, sigpipe, uninit, verify_pos, CHUNK, EISDIR, MAX_RW_COUNT};
use crate::vfs::notify as inotify;
use crate::vfs::{self, InodeRef, S_IFMT};
use alloc::rc::Rc;
use alloc::vec::Vec;

type R = Result<i64, i64>;
const EFAULT: i64 = 14;

pub const SYSCALLS: &[u64] = {
    use crate::syscall::nr::*;
    &[SENDFILE, SPLICE, TEE, COPY_FILE_RANGE]
};

pub fn sys(n: u64, a: &[u64; 6]) -> R {
    use crate::syscall::nr::*;
    match n {
        SENDFILE => sendfile(a[0], a[1], a[2] as usize, a[3] as usize),
        COPY_FILE_RANGE => copy_file_range(a[0], a[1] as usize, a[2], a[3] as usize, a[4] as usize, a[5]),
        SPLICE => splice(a[0], a[1] as usize, a[2], a[3] as usize, a[4] as usize, a[5]),
        TEE => tee(a[0], a[1], a[2] as usize, a[3]),
        _ => Err(-38),
    }
}

/// 大きなロックなしの splice (syscall::fast から。main.rs が register_fast_splice で置く)
pub fn fast(me: &proc::Proc, a: &[u64; 6]) -> Option<i64> {
    fast_splice(me, a[0], a[1] as usize, a[2], a[3] as usize, a[4] as usize, a[5])
}

const SPLICE_F_NONBLOCK: u64 = 2;
/// SPLICE_F_MOVE | SPLICE_F_NONBLOCK | SPLICE_F_MORE | SPLICE_F_GIFT
const SPLICE_F_ALL: u64 = 0x0f;

/// splice(2) (Linux の splice、__do_splice、do_splice の順): 長さ 0 なら 0、flags (SPLICE_F_ALL のほかは EINVAL)、
/// 両方の fd (O_PATH も EBADF)、パイプの側にわたされた位置 (ESPIPE)、位置を写し (EFAULT)、読める口と書ける口か
/// (EBADF)。それから:
///   パイプ → パイプ: 同じパイプは EINVAL。箱ごとに動かす (Pipe::splice_pipe)
///   パイプ → ほか: わたされた位置 (位置のないものは EINVAL) かファイルの位置、位置のしらべ (EINVAL)、中身を待って
///     (書き手がいなければ 0、nonblock は EAGAIN) そこに書き、書けた分を読み減らす (splice_pipe_to)
///   ほか → パイプ: わたされた位置かファイルの位置、位置のしらべ、splice_file_to_pipe (sendfile と同じ)
///   どちらもパイプでなければ EINVAL
/// nonblock は SPLICE_F_NONBLOCK か、パイプの側の O_NONBLOCK。O_APPEND のファイルへは、Linux は EINVAL だが、aios は
/// 書く (aios がえらんだちがい copy-append)
fn splice(fd_in: u64, off_in: usize, fd_out: u64, off_out: usize, len: usize, flags: u64) -> R {
    if len == 0 {
        return Ok(0);
    }
    if flags & !SPLICE_F_ALL != 0 {
        return Err(crate::why::inval());
    }
    let fi = file_of(fd_in)?;
    let fo = file_of(fd_out)?;
    if fi.borrow().flags & file::O_PATH != 0 || fo.borrow().flags & file::O_PATH != 0 {
        return Err(-EBADF);
    }
    let (pin, pout) = (pipe_of(fd_in), pipe_of(fd_out));
    if (pin.is_some() && off_in != 0) || (pout.is_some() && off_out != 0) {
        return Err(-file::ESPIPE);
    }
    let at_out = if off_out != 0 { Some(read_off(off_out)?) } else { None };
    let at_in = if off_in != 0 { Some(read_off(off_in)?) } else { None };
    if !fi.borrow().readable() || !fo.borrow().writable() {
        return Err(-EBADF);
    }
    let nb = |f: &FileRef| flags & SPLICE_F_NONBLOCK != 0 || f.borrow().flags & file::O_NONBLOCK != 0;
    // 位置をわたせるのは、位置のあるものだけ (Linux の FMODE_PREAD / FMODE_PWRITE)
    let positional = |f: &FileRef| f.borrow().kind.positional();
    match (pin, pout) {
        (Some(a), Some(b)) => {
            if alloc::rc::Rc::ptr_eq(&a, &b) {
                return Err(crate::why::inval());
            }
            let r = Pipe::splice_pipe(&a, &b, len, nb(&fi) || nb(&fo));
            if r == Err(-file::EPIPE) {
                sigpipe();
            }
            Ok(r? as i64)
        }
        (Some(a), None) => {
            if at_out.is_some() && !positional(&fo) {
                return Err(crate::why::inval());
            }
            let mut pos = at_out.unwrap_or(fo.borrow().offset as i64);
            // aios がえらんだちがい (copy-append): O_APPEND で開いたファイルへも書く。位置がなければ write と同じく
            // 終わりに足す
            if at_out.is_none()
                && fo.borrow().flags & file::O_APPEND != 0
                && let Some((io, _)) = inode_path(&fo)
            {
                pos = io.meta().size as i64;
            }
            verify_pos(pos, len)?;
            let n = splice_pipe_to(&a, &fo, pos, len, nb(&fi))?;
            if n > 0 {
                pos += n as i64;
                match at_out {
                    Some(_) => out(off_out, &pos.to_le_bytes())?,
                    None => fo.borrow_mut().offset = pos as usize,
                }
            }
            Ok(n as i64)
        }
        (None, Some(b)) => {
            if at_in.is_some() && !positional(&fi) {
                return Err(crate::why::inval());
            }
            let mut pos = at_in.unwrap_or(fi.borrow().offset as i64);
            verify_pos(pos, len)?;
            let n = if matches!(fi.borrow().kind, Kind::Inode(..)) {
                splice_file_to_pipe(&fi, &mut pos, &b, len, nb(&fo))?
            } else {
                splice_stream_to_pipe(&fi, &b, len, nb(&fo))?
            };
            match at_in {
                Some(_) => out(off_in, &pos.to_le_bytes())?,
                None => fi.borrow_mut().offset = pos as usize,
            }
            Ok(n as i64)
        }
        (None, None) => Err(crate::why::inval()),
    }
}

/// パイプ a の中身を fo の pos へ len バイトまで書く (Linux の iter_file_splice_write): 中身が来るまで待ち (書き手が
/// いなければ 0、nonblock は EAGAIN)、パイプにあるだけ (len まで) 書いて、書けた分を読み減らす。ふつうのファイルは
/// 大きさの上限 (EFBIG) を見てから setuid / setgid を落として書く。/dev/null は捨てるだけ、位置のないもの (端末、
/// ソケット) はふつうに書く
fn splice_pipe_to(a: &alloc::rc::Rc<file::PipeCell>, fo: &FileRef, pos: i64, len: usize, nonblock: bool) -> Result<usize, i64> {
    if matches!(fo.borrow().kind, Kind::Null) {
        return Pipe::discard(a, len, nonblock);
    }
    let data = Pipe::peek_wait(a, len, nonblock)?;
    if data.is_empty() {
        return Ok(0);
    }
    let w = match inode_path(fo) {
        Some((io, path)) => {
            const EFBIG: i64 = 27;
            let max = io.max_bytes() as i64;
            if pos >= max {
                return Err(-EFBIG);
            }
            let n = data.len().min((max - pos) as usize);
            vfs::write_sealed(&io, pos as usize, n)?;
            vfs::drop_privs(&io)?;
            let w = io.write_at(pos as usize, &data[..n])?;
            if w > 0 {
                inotify::file_event(&path, &io, inotify::IN_MODIFY);
            }
            w
        }
        None => {
            let mut done = 0;
            while done < data.len() {
                match file::write(fo, &data[done..]) {
                    Ok(0) => break,
                    Ok(w) => done += w,
                    Err(e) if done == 0 => {
                        if e == -file::EPIPE {
                            sigpipe();
                        }
                        return Err(e);
                    }
                    Err(_) => break,
                }
            }
            done
        }
    };
    let _ = Pipe::discard(a, w, true);
    Ok(w)
}

/// 位置のないもの (端末、ソケットなど) からパイプへ (Linux の copy_splice_read): 箱があくのを待ち (読み手が
/// いなければ SIGPIPE と EPIPE、nonblock は EAGAIN)、あいた箱 × 4KiB まで読んで、4KiB ずつの足さない箱に入れる
fn splice_stream_to_pipe(f: &FileRef, p: &alloc::rc::Rc<file::PipeCell>, len: usize, nonblock: bool) -> Result<usize, i64> {
    if let Err(e) = Pipe::wait_space(p, nonblock) {
        if e == -file::EPIPE {
            sigpipe();
        }
        return Err(e);
    }
    // AF_UNIX のソケット (FileOps::splice_to_pipe: Linux の sock_splice_read)
    if let Some(r) = ops_of(f).and_then(|o| o.splice_to_pipe(f, 0, p, len, nonblock)) {
        return r;
    }
    let free = {
        let pp = p.borrow();
        pp.slots().saturating_sub(pp.used_slots())
    };
    let mut tmp = uninit(len.min(free * crate::memlayout::PGSIZE))?;
    let n = file::read(f, &mut tmp)?;
    Ok(Pipe::splice_in(p, 0, &tmp[..n]))
}

fn read_off(va: usize) -> Result<i64, i64> {
    let mut b = [0u8; 8];
    proc::current().pt().copy_in(&mut b, va).ok_or(-EFAULT)?;
    Ok(i64::from_le_bytes(b))
}

/// ふつうのファイルを、ページキャッシュ (ext4) か inode からそのまま読む (pread と同じ)
fn read_inode_at(ino: &InodeRef, pos: usize, buf: &mut [u8]) -> Result<usize, i64> {
    if ino.page_cacheable() { file::read_sleepable(ino, pos, buf) } else { ino.read_at(pos, buf) }
}

/// ふつうのファイル f の *pos から、パイプ p へ len バイトまで (Linux の splice_file_to_pipe と
/// filemap_splice_read / shmem_file_splice_read): 箱があくのを待ち (読み手がいなければ SIGPIPE と EPIPE、
/// nonblock なら EAGAIN)、あいた箱 × 4KiB まで読んで、ファイルの 1 ページの中のかけらごとの箱に入れる。
/// ファイルの終わりより先は 0。*pos を入れた分すすめる。
/// aios がえらんだちがい (splice-snapshot): 中身はそのときのものを箱に写す。あとでファイルを書いたり切りつめたり
/// しても、パイプの中身は変わらない。Linux はページキャッシュのページそのものを箱に入れるので、読む前にファイルが
/// 変わればパイプからもそれが読め、切りつめると端のページの残りは 0 になる。doc/aios.md の「aios がえらんだちがい」
fn splice_file_to_pipe(f: &FileRef, pos: &mut i64, p: &alloc::rc::Rc<file::PipeCell>, len: usize, nonblock: bool) -> Result<usize, i64> {
    if let Err(e) = Pipe::wait_space(p, nonblock) {
        if e == -file::EPIPE {
            sigpipe();
        }
        return Err(e);
    }
    if len == 0 {
        return Ok(0);
    }
    if let Some(r) = ops_of(f).and_then(|o| o.splice_to_pipe(f, *pos, p, len, nonblock)) {
        return r;
    }
    // AF_INET のソケットは位置のないものとして (Linux の tcp_splice_read と copy_splice_read: ソケットの O_NONBLOCK で
    // 待つか決め、TCP は来ているだけ、UDP はメッセージ 1 つ)
    if matches!(&f.borrow().kind, Kind::Ops(o) if o.splice_stream()) {
        return splice_stream_to_pipe(f, p, len, nonblock);
    }
    // ファイルのほかは splice で読めない (ディレクトリ、パイプ: Linux も EINVAL)
    let Some((ino, path)) = inode_path(f).filter(|(i, _)| i.meta().mode & S_IFMT == vfs::S_IFREG) else { return Err(crate::why::inval()) };
    let room = p.borrow().splice_room(*pos as u64, len.min(MAX_RW_COUNT));
    verify_pos(*pos, room)?;
    let mut tmp = uninit(room)?;
    let n = read_inode_at(&ino, *pos as usize, &mut tmp)?;
    let done = Pipe::splice_in(p, *pos as u64, &tmp[..n]);
    *pos += done as i64;
    if done > 0 {
        inotify::file_event(&path, &ino, inotify::IN_ACCESS);
    }
    Ok(done)
}

/// ファイル in の pos から len バイトを、ファイル out の opos へ写す (Linux の do_splice_direct で、
/// copy_file_range と sendfile が使う)。splice_direct_to_actor と同じく、16 ページの内側のパイプに入る分
/// (位置がページの途中なら、そのぶん少ない) ずつ、読めなくなる (ファイルの終わり) か、書くのが短くなるまで。
/// 同じファイルのときは、呼ぶほうが重ならない範囲にしている (copy_file_range と sendfile-self)。写せたバイト数
fn copy_inode_range(src: &InodeRef, pos: i64, dst: &InodeRef, opos: i64, len: usize) -> Result<usize, i64> {
    const PG: usize = crate::memlayout::PGSIZE;
    let mut done = 0;
    while done < len {
        let at_in = pos as usize + done;
        let mut tmp = uninit((len - done).min(16 * PG - at_in % PG))?;
        let r = read_inode_at(src, at_in, &mut tmp);
        let n = match r {
            Ok(n) => n,
            Err(e) if done == 0 => return Err(e),
            Err(_) => break,
        };
        if n == 0 {
            break;
        }
        let at = opos as usize + done;
        // 書くときに setuid / setgid を落とす (Linux の file_remove_privs)
        if done == 0 {
            vfs::drop_privs(dst)?;
        }
        let w = match vfs::write_sealed(dst, at, n).and_then(|_| dst.write_at(at, &tmp[..n])) {
            Ok(w) => w,
            Err(e) if done == 0 => return Err(e),
            Err(_) => break,
        };
        done += w;
        if w < n {
            break;
        }
    }
    Ok(done)
}

/// copy_file_range(2) (Linux の vfs_copy_file_range): ext4 も tmpfs も自分の copy_file_range を持たないので、
/// 同じファイルシステムの中は splice で写し、ちがうファイルシステムは EXDEV
fn copy_file_range(fd_in: u64, off_in: usize, fd_out: u64, off_out: usize, len: usize, flags: u64) -> R {
    const EOVERFLOW: i64 = 75;
    // Linux の順: 両方の fd (O_PATH も EBADF)、位置を写し (EFAULT)、flags (EINVAL)、generic_file_rw_checks
    // (ディレクトリは EISDIR、ふつうのファイルでなければ EINVAL、読めない・書けないなら EBADF)、
    // ファイルシステムがちがえば EXDEV、位置 + 長さがあふれたら EOVERFLOW、長さを読むほうの終わりまでに縮め、
    // 同じファイルで重なれば EINVAL、位置のしらべ (EINVAL)、長さ 0 なら 0
    let fi = file_of(fd_in)?;
    let fo = file_of(fd_out)?;
    if fi.borrow().flags & file::O_PATH != 0 || fo.borrow().flags & file::O_PATH != 0 {
        return Err(-EBADF);
    }
    let mut pos_in = if off_in != 0 { read_off(off_in)? } else { fi.borrow().offset as i64 };
    let mut pos_out = if off_out != 0 { read_off(off_out)? } else { fo.borrow().offset as i64 };
    if flags as u32 != 0 {
        return Err(crate::why::inval());
    }
    let (mi, mo) = (fi.borrow().stat().mode & S_IFMT, fo.borrow().stat().mode & S_IFMT);
    if mi == vfs::S_IFDIR || mo == vfs::S_IFDIR {
        return Err(-EISDIR);
    }
    if mi != vfs::S_IFREG || mo != vfs::S_IFREG {
        return Err(crate::why::inval());
    }
    if !fi.borrow().readable() || !fo.borrow().writable() {
        return Err(-EBADF);
    }
    let (Some((ii, pin)), Some((io, pout))) = (inode_path(&fi), inode_path(&fo)) else { return Err(crate::why::inval()) };
    if ii.id().0 != io.id().0 {
        return Err(-vfs::EXDEV);
    }
    // aios がえらんだちがい (copy-append): O_APPEND で開いたものへも写せる。位置をわたされたらそこへ (pwrite-append と
    // 同じ)、わたされなければ write と同じく終わりへ足す。Linux は EBADF (sendfile は EINVAL) で、写す人は read と
    // write でやりなおす。doc/aios.md の「aios がえらんだちがい」
    if fo.borrow().flags & file::O_APPEND != 0 && off_out == 0 {
        pos_out = io.meta().size as i64;
    }
    let count = len as u64;
    if (pos_in as u64).checked_add(count).is_none() || (pos_out as u64).checked_add(count).is_none() {
        return Err(-EOVERFLOW);
    }
    let size = ii.meta().size;
    let count = if pos_in >= size as i64 { 0 } else { count.min(size.wrapping_sub(pos_in as u64)) };
    // 書くほうの大きさの上限 (generic_write_check_limits)。写すものがなければ見ない (Linux 7.4 の
    // generic_copy_file_checks。7.3 までは 0 バイトでも上限をこえた off_out は EFBIG)
    const EFBIG: i64 = 27;
    let max = io.max_bytes() as i64;
    if count != 0 && pos_out >= max {
        return Err(-EFBIG);
    }
    let count = if count == 0 { 0 } else { count.min(max.wrapping_sub(pos_out) as u64) as usize };
    if ii.id() == io.id() && pos_out as u64 + count as u64 > pos_in as u64 && (pos_out as u64) < pos_in as u64 + count as u64 {
        return Err(crate::why::inval());
    }
    verify_pos(pos_in, count)?;
    verify_pos(pos_out, count)?;
    if count == 0 {
        return Ok(0);
    }
    let n = copy_inode_range(&ii, pos_in, &io, pos_out, count.min(MAX_RW_COUNT))?;
    if n > 0 {
        pos_in += n as i64;
        pos_out += n as i64;
        if off_in != 0 {
            out(off_in, &pos_in.to_le_bytes())?;
        } else {
            fi.borrow_mut().offset = pos_in as usize;
        }
        if off_out != 0 {
            out(off_out, &pos_out.to_le_bytes())?;
        } else {
            fo.borrow_mut().offset = pos_out as usize;
        }
        inotify::file_event(&pin, &ii, inotify::IN_ACCESS);
        inotify::file_event(&pout, &io, inotify::IN_MODIFY);
    }
    Ok(n as i64)
}

/// sendfile(2) (Linux の sendfile64): 位置 (offset) は先に写し、終わったら (しくじっても) 書きもどす
fn sendfile(out_fd: u64, in_fd: u64, offp: usize, count: usize) -> R {
    let mut pos = if offp != 0 { Some(read_off(offp)?) } else { None };
    let r = do_sendfile(out_fd, in_fd, pos.as_mut(), count);
    if let Some(p) = pos {
        out(offp, &p.to_le_bytes())?;
    }
    r
}

/// Linux の do_sendfile の順: 読むほうの fd (読めなければ EBADF)、位置 (わたされたのに位置のないものなら ESPIPE)、
/// 位置のしらべ (EINVAL)、書くほうの fd (書けなければ EBADF)。書くほうがパイプならそこへ splice
/// (splice_file_to_pipe)、ほかは位置のしらべ、読むほうに位置がなければ EINVAL (do_splice_direct)、それから写す。
/// O_APPEND なら終わりに足す (aios がえらんだちがい copy-append。Linux は EINVAL)
fn do_sendfile(out_fd: u64, in_fd: u64, ppos: Option<&mut i64>, count: usize) -> R {
    let fi = file_of(in_fd)?;
    if !fi.borrow().readable() {
        return Err(-EBADF);
    }
    let seekable = fi.borrow().kind.seekable();
    let pos = match ppos.as_deref() {
        Some(_) if !seekable => return Err(-file::ESPIPE),
        Some(&p) => p,
        None => fi.borrow().offset as i64,
    };
    verify_pos(pos, count)?;
    let count = count.min(MAX_RW_COUNT);
    let fo = file_of(out_fd)?;
    if !fo.borrow().writable() {
        return Err(-EBADF);
    }
    // aios がえらんだちがい (copy-append): O_APPEND で開いたファイルへは、write と同じく終わりに足す
    let out_pos = match &fo.borrow().kind {
        Kind::Inode(io, _) if fo.borrow().flags & file::O_APPEND != 0 => io.meta().size as i64,
        _ => fo.borrow().offset as i64,
    };
    // 両方のファイルの大きさの上限 (s_maxbytes) をこえるなら EOVERFLOW (読みはじめがもうこえているとき)、
    // こえる分は短く
    const EOVERFLOW: i64 = 75;
    let maxb = |f: &FileRef| inode_path(f).map_or(i64::MAX, |(i, _)| i.max_bytes() as i64);
    let max = maxb(&fi).min(maxb(&fo));
    let mut count = count;
    if pos.saturating_add(count as i64) > max {
        if pos >= max {
            return Err(-EOVERFLOW);
        }
        count = (max - pos) as usize;
    }
    let n = if let Some(p) = pipe_of(out_fd) {
        let nonblock = fo.borrow().flags & file::O_NONBLOCK != 0;
        let mut at = pos;
        splice_file_to_pipe(&fi, &mut at, &p, count, nonblock)?
    } else {
        verify_pos(out_pos, count)?;
        if !seekable {
            return Err(crate::why::inval());
        }
        // aios がえらんだちがい (sendfile-self): 同じファイルの中なら copy_file_range と同じく、読みはじめたときの終わり
        // までを写し、読むところと書くところが重なれば EINVAL。Linux は重なっても写し、読むのがページキャッシュのページ
        // そのもの (写しをとらない) なので、同じ回に書いたものが読みにまざり、答えが内側のくぎり (16 ページ) やページの
        // まとまり (folio) の大きさで変わる。doc/aios.md の「aios がえらんだちがい」
        let mut count = count;
        if let (Some((ii, _)), Some((io, _))) = (inode_path(&fi), inode_path(&fo))
            && ii.id() == io.id()
            && ii.meta().mode & S_IFMT == vfs::S_IFREG
        {
            let size = ii.meta().size as i64;
            count = if pos >= size { 0 } else { count.min((size - pos) as usize) };
            if count > 0 && out_pos < pos + count as i64 && pos < out_pos + count as i64 {
                return Err(crate::why::inval());
            }
        }
        if count == 0 {
            0
        } else {
            // 読めるのはふつうのファイルだけ (ディレクトリは Linux でも splice_read がなく EINVAL)
            let Some((ii, ipath)) = inode_path(&fi).filter(|(i, _)| i.meta().mode & S_IFMT == vfs::S_IFREG) else { return Err(crate::why::inval()) };
            let n = match inode_path(&fo) {
                Some((io, opath)) => {
                    let n = copy_inode_range(&ii, pos, &io, out_pos, count)?;
                    if n > 0 {
                        inotify::file_event(&opath, &io, inotify::IN_MODIFY);
                    }
                    n
                }
                // 端末、ソケット、/dev/null など: ふつうに書く
                None => {
                    let mut done = 0;
                    while done < count {
                        let mut tmp = uninit((count - done).min(CHUNK))?;
                        let r = read_inode_at(&ii, pos as usize + done, &mut tmp);
                        let k = match r {
                            Ok(0) => break,
                            Ok(k) => k,
                            Err(e) if done == 0 => return Err(e),
                            Err(_) => break,
                        };
                        let w = match file::write(&fo, &tmp[..k]) {
                            Ok(w) => w,
                            Err(e) if done == 0 => {
                                if e == -file::EPIPE {
                                    sigpipe();
                                }
                                return Err(e);
                            }
                            Err(_) => break,
                        };
                        done += w;
                        if w < k {
                            break;
                        }
                    }
                    done
                }
            };
            if n > 0 {
                inotify::file_event(&ipath, &ii, inotify::IN_ACCESS);
                fo.borrow_mut().offset = (out_pos + n as i64) as usize;
            }
            n
        }
    };
    if n > 0 {
        let end = pos + n as i64;
        match ppos {
            Some(p) => *p = end,
            None => fi.borrow_mut().offset = end as usize,
        }
    }
    Ok(n as i64)
}

/// tee(2) (Linux の順): flags (SPLICE_F_ALL のほかは EINVAL)、長さ 0 なら 0、両方の fd (O_PATH も EBADF)、読める口と
/// 書ける口か (EBADF)、両方がパイプで同じでなければ EINVAL。それから Pipe::tee_pipe (読み減らさずに箱を写す)。
/// nonblock は SPLICE_F_NONBLOCK かどちらかの O_NONBLOCK
fn tee(fd_in: u64, fd_out: u64, len: usize, flags: u64) -> R {
    if flags & !SPLICE_F_ALL != 0 {
        return Err(crate::why::inval());
    }
    if len == 0 {
        return Ok(0);
    }
    let fi = file_of(fd_in)?;
    let fo = file_of(fd_out)?;
    if fi.borrow().flags & file::O_PATH != 0 || fo.borrow().flags & file::O_PATH != 0 {
        return Err(-EBADF);
    }
    if !fi.borrow().readable() || !fo.borrow().writable() {
        return Err(-EBADF);
    }
    let (Some(a), Some(b)) = (pipe_of(fd_in), pipe_of(fd_out)) else { return Err(crate::why::inval()) };
    if alloc::rc::Rc::ptr_eq(&a, &b) {
        return Err(crate::why::inval());
    }
    let nonblock = flags & SPLICE_F_NONBLOCK != 0 || (fi.borrow().flags | fo.borrow().flags) & file::O_NONBLOCK != 0;
    let r = Pipe::tee_pipe(&a, &b, len, nonblock);
    if r == Err(-file::EPIPE) {
        sigpipe();
    }
    Ok(r? as i64)
}


/// 大きなロックなしの splice (syscall::fast)。uutils の cat は、ファイル → (自分で作った) パイプ → 出力 と
/// 2 回 splice する。眠らずにすむものだけ:
///   パイプ → /dev/null: パイプのロックだけで捨てる (中身があるとき)
///   ext4 のファイル → パイプ: 読むところがページキャッシュに全部あり、パイプに全部入るとき (半分だけ
///   入れることはしない)。オフセットは off_in (ユーザーのメモリ) か、だれとも分けていない開いたファイルの
/// ほかは None でふつうの道へ (flags のまちがい、O_PATH、読めない口・書けない口、ふつうのファイルでないものも、
/// ふつうの道が Linux の順でエラーにする)
fn fast_splice(me: &proc::Proc, fd_in: u64, off_in: usize, fd_out: u64, off_out: usize, len: usize, flags: u64) -> Option<i64> {
    if len == 0 || off_out != 0 || flags & !0x0f != 0 {
        return None;
    }
    let len = len.min(64 * 1024);
    let files = me.files.as_ref()?;
    let private = files.private();
    let _gate = if private { None } else { Some(files.get().gate.read()?) };
    let fi = files.get().get(fd_in)?;
    let fo = files.get().get(fd_out)?;
    let (ofi, ofo) = unsafe { (&mut *fi.as_ptr(), &*fo.as_ptr()) };
    if (ofi.flags | ofo.flags) & O_PATH != 0 || !ofi.readable() || !ofo.writable() {
        return None;
    }
    let wake = |p: &Rc<PipeCell>| {
        FAST_RW[6].fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        crate::smp::defer_wake(Rc::as_ptr(p) as usize);
    };
    match (&ofi.kind, &ofo.kind) {
        (Kind::PipeRead(p), Kind::Null) if off_in == 0 => {
            let mut pp = p.borrow_mut();
            if pp.data.len == 0 {
                return None;
            }
            let k = len.min(pp.data.len);
            pp.data.skip(k);
            pp.taken += k as u64;
            let w = pp.sleepers > 0 || pp.watchers > 0 || pp.epolled;
            drop(pp);
            if w {
                wake(p);
            }
            Some(k as i64)
        }
        (Kind::Inode(ino, _), Kind::PipeWrite(p)) => {
            // ino.meta() は見ない: inode の中身は大きなロックで守られている (ほかの CPU が書いているかもしれない)。
            // ページキャッシュにはふつうのファイルのページしか入らない (vm::read_cached と mmap) ので、ディレクトリなどは
            // with_cached が None でふつうの道へ (fast_pread と同じ)
            if !ino.page_cacheable() || crate::vfs::notify::ACCESS_WATCHED.load(core::sync::atomic::Ordering::Relaxed) != 0 {
                return None;
            }
            // オフセット: off_in があればそこ (あとで書きもどす)、なければ開いたファイルの (だれとも分けていないとき)
            let off = if off_in != 0 {
                let mut b = [0u8; 8];
                if !me.pt().copy_in_nofault(off_in, &mut b) {
                    return None;
                }
                let o = i64::from_le_bytes(b);
                if o < 0 {
                    return None;
                }
                o as usize
            } else {
                if !private || Rc::strong_count(fi) != 1 {
                    return None;
                }
                ofi.offset
            };
            // 読み手がいない (EPIPE と SIGPIPE) か箱がない (待つか EAGAIN) なら、ファイルの終わりでもふつうの道
            // (Linux の wait_for_space は読む前)
            {
                let pp = p.borrow();
                if pp.readers == 0 || pp.full() {
                    return None;
                }
            }
            let mut woke = false;
            let n = crate::vm::with_cached(ino.id(), off, len, |parts| {
                let total: usize = parts.iter().map(|x| x.len()).sum();
                let mut pp = p.borrow_mut();
                // ファイルのページのかけらごとに箱ひとつ (Linux の splice と同じ)。全部入るときだけ。箱にはそのときの
                // 中身を写す (aios がえらんだちがい (splice-snapshot): sysfile::splice_file_to_pipe と同じ)
                if pp.readers == 0 || pp.splice_room(off as u64, total) < total {
                    return false;
                }
                if off_in != 0 && !me.pt().copy_out_nofault(off_in, &((off + total) as i64).to_le_bytes()) {
                    return false;
                }
                let mut pos = off;
                for x in parts {
                    let mut i = 0;
                    while i < x.len() {
                        let k = (PGSIZE - pos % PGSIZE).min(x.len() - i);
                        if pp.data.push_piece(&x[i..i + k]).is_err() {
                            // ページがとれなかった: 入れた分はそのまま (大きなロックの道でも同じく途中までになる)
                            break;
                        }
                        i += k;
                        pos += k;
                    }
                }
                pp.generation += 1;
                pp.wrote += total as u64;
                woke = pp.sleepers > 0 || pp.watchers > 0 || pp.epolled;
                true
            })?;
            if off_in == 0 {
                ofi.offset = off + n;
            }
            if woke {
                wake(p);
            }
            Some(n as i64)
        }
        _ => None,
    }
}


// ---- パイプの箱 (Linux の fs/splice.c の、pipe_inode_info をじかに見るところ) ----

impl Pipe {
    /// splice で、ファイルの位置 pos から len バイトまでを入れるとき (Linux の filemap_splice_read と
    /// shmem_file_splice_read): 長さはあいた箱 × 4KiB まで、ファイルの 1 ページの中のかけらごとに箱ひとつ。
    /// 入るバイト数
    pub fn splice_room(&self, pos: u64, len: usize) -> usize {
        let free = self.slots().saturating_sub(self.used_slots());
        let mut len = len.min(free * PGSIZE);
        let (mut done, mut pos, mut n) = (0, pos, 0);
        while len > 0 && n < free {
            let k = (PGSIZE - (pos % PGSIZE as u64) as usize).min(len);
            done += k;
            len -= k;
            pos += k as u64;
            n += 1;
        }
        done
    }

    /// splice でパイプに入れる前に、箱があくのを待つ (Linux の wait_for_space): 読み手がいなければ EPIPE
    /// (SIGPIPE は呼ぶほうで)、いっぱいなら nonblock は EAGAIN、ほかは待つ
    pub fn wait_space(p: &Rc<PipeCell>, nonblock: bool) -> Result<(), i64> {
        loop {
            let pp = p.borrow_mut();
            if pp.readers == 0 {
                return Err(-EPIPE);
            }
            if !pp.full() {
                return Ok(());
            }
            if nonblock {
                return Err(-11); // EAGAIN
            }
            Pipe::sleep(p, pp)?;
        }
    }

    /// splice で入れる: ファイルの位置 pos から読んだ data を、ファイルの 1 ページの中のかけらごとに足さない箱へ
    /// (あいている箱の分だけ)。入れたバイト数。読み手を起こす
    pub fn splice_in(p: &Rc<PipeCell>, mut pos: u64, data: &[u8]) -> usize {
        let mut done = 0;
        {
            let mut pp = p.borrow_mut();
            while done < data.len() && !pp.full() {
                let k = (PGSIZE - (pos % PGSIZE as u64) as usize).min(data.len() - done);
                if pp.data.push_piece(&data[done..done + k]).is_err() {
                    break;
                }
                done += k;
                pos += k as u64;
            }
            if done > 0 {
                pp.generation += 1;
                pp.wrote += done as u64;
            }
        }
        if done > 0 {
            Pipe::wake(p);
        }
        done
    }

    /// read_ex と同じく待つが、写さずに n バイトまで捨てる (splice の読み減らしと、/dev/null へ)
    pub fn discard(p: &Rc<PipeCell>, n: usize, nonblock: bool) -> Result<usize, i64> {
        loop {
            let mut pp = p.borrow_mut();
            if pp.data.len > 0 {
                let k = n.min(pp.data.len);
                pp.data.skip(k);
                pp.taken += k as u64;
                drop(pp);
                Pipe::wake(p);
                return Ok(k);
            }
            if pp.writers == 0 {
                return Ok(0);
            }
            if nonblock {
                return Err(-11); // EAGAIN
            }
            Pipe::sleep(p, pp)?;
        }
    }

    /// splice でパイプから書くとき (Linux の splice_from_pipe_next と iter_file_splice_write): 中身が来るまで待ち
    /// (書き手がいなければ空。nonblock は EAGAIN)、先頭から max バイトまでを読み減らさずに写す。書けた分は、あとで
    /// discard で読み減らす
    pub fn peek_wait(p: &Rc<PipeCell>, max: usize, nonblock: bool) -> Result<Vec<u8>, i64> {
        loop {
            let mut pp = p.borrow_mut();
            if pp.data.len > 0 {
                let n = max.min(pp.data.len);
                // pop が n バイトみな写す (0 で埋めない)
                let mut v = Vec::with_capacity(n);
                unsafe { v.set_len(n) };
                pp.data.pop(&mut v, false);
                return Ok(v);
            }
            if pp.writers == 0 {
                return Ok(Vec::new());
            }
            if nonblock {
                return Err(-11); // EAGAIN
            }
            Pipe::sleep(p, pp)?;
        }
    }

    /// パイプ a からパイプ b へ len バイトまで (Linux の splice_pipe_to_pipe): 箱ごとそのまま動かす (足してよいかの
    /// 印もそのまま)。len が箱より短ければ、その分だけを足さない新しい箱に写し、a の箱は残りを持つ。はじめに、a が空なら
    /// 待ち (書き手がいなければそのまま。nonblock は EAGAIN)、b がいっぱいなら待つ (読み手がいなければ EPIPE。nonblock
    /// は EAGAIN)。b の読み手がいなければ EPIPE (SIGPIPE は呼ぶほうで)、a が空で書き手がいなければ 0
    pub fn splice_pipe(a: &Rc<PipeCell>, b: &Rc<PipeCell>, len: usize, nonblock: bool) -> Result<usize, i64> {
        const EAGAIN: i64 = 11;
        'retry: loop {
            // ipipe_prep
            loop {
                let pa = a.borrow_mut();
                if pa.data.len > 0 || pa.writers == 0 {
                    break;
                }
                if nonblock {
                    return Err(-EAGAIN);
                }
                Pipe::sleep(a, pa)?;
            }
            // opipe_prep
            loop {
                let pb = b.borrow_mut();
                if !pb.full() {
                    break;
                }
                if pb.readers == 0 {
                    return Err(-EPIPE);
                }
                if nonblock {
                    return Err(-EAGAIN);
                }
                Pipe::sleep(b, pb)?;
            }
            // 2 つのロックは場所の順に (Linux の pipe_double_lock)
            let (mut pa, mut pb) = if (Rc::as_ptr(a) as usize) < (Rc::as_ptr(b) as usize) {
                let x = a.borrow_mut();
                (x, b.borrow_mut())
            } else {
                let y = b.borrow_mut();
                (a.borrow_mut(), y)
            };
            let (mut done, mut left, mut freed) = (0, len, false);
            loop {
                if pb.readers == 0 {
                    if done == 0 {
                        return Err(-EPIPE);
                    }
                    break;
                }
                if pa.data.len == 0 && pa.writers == 0 {
                    break;
                }
                if pa.data.len == 0 || pb.full() {
                    if done > 0 {
                        break;
                    }
                    if nonblock {
                        return Err(-EAGAIN);
                    }
                    drop(pa);
                    drop(pb);
                    continue 'retry;
                }
                let (pg, r, w, merge) = pa.data.pages[0];
                let k = w - r;
                if left >= k {
                    pa.data.pages.pop_front();
                    pa.data.len -= k;
                    pb.data.pages.push_back((pg, r, w, merge));
                    pb.data.len += k;
                    freed = true;
                    done += k;
                    left -= k;
                } else {
                    let Some(np) = pb.data.page() else {
                        if done == 0 {
                            return Err(-12); // ENOMEM
                        }
                        break;
                    };
                    unsafe { core::ptr::copy_nonoverlapping(pg.add(r), np, left) };
                    pb.data.pages.push_back((np, 0, left, false));
                    pb.data.len += left;
                    pa.data.pages[0].1 += left;
                    pa.data.len -= left;
                    done += left;
                    left = 0;
                }
                if left == 0 {
                    break;
                }
            }
            if done > 0 {
                pa.taken += done as u64;
                pb.wrote += done as u64;
                pb.generation += 1;
            }
            drop(pa);
            drop(pb);
            if done > 0 {
                Pipe::wake(b);
            }
            if freed {
                Pipe::wake(a);
            }
            return Ok(done);
        }
    }

    /// tee: パイプ a の中身を読み減らさずに、パイプ b へ len バイトまで写す (Linux の link_pipe): a の箱ごとに、
    /// 足さない箱を b に作って写す (len で切れる箱は、その分だけ)。はじめに a が空なら待ち (書き手がいなければそのまま
    /// 0。nonblock は EAGAIN)、b がいっぱいなら待つ (読み手がいなければ EPIPE。nonblock は EAGAIN)。写せたバイト数
    pub fn tee_pipe(a: &Rc<PipeCell>, b: &Rc<PipeCell>, len: usize, nonblock: bool) -> Result<usize, i64> {
        const EAGAIN: i64 = 11;
        loop {
            let pa = a.borrow_mut();
            if pa.data.len > 0 || pa.writers == 0 {
                break;
            }
            if nonblock {
                return Err(-EAGAIN);
            }
            Pipe::sleep(a, pa)?;
        }
        loop {
            let pb = b.borrow_mut();
            if !pb.full() {
                break;
            }
            if pb.readers == 0 {
                return Err(-EPIPE);
            }
            if nonblock {
                return Err(-EAGAIN);
            }
            Pipe::sleep(b, pb)?;
        }
        let (pa, mut pb) = if (Rc::as_ptr(a) as usize) < (Rc::as_ptr(b) as usize) {
            let x = a.borrow_mut();
            (x, b.borrow_mut())
        } else {
            let y = b.borrow_mut();
            (a.borrow_mut(), y)
        };
        let (mut done, mut left, mut i) = (0, len, 0);
        while left > 0 {
            if pb.readers == 0 {
                if done == 0 {
                    return Err(-EPIPE);
                }
                break;
            }
            if i >= pa.data.pages.len() || pb.full() {
                break;
            }
            let (pg, r, w, _) = pa.data.pages[i];
            let k = (w - r).min(left);
            let Some(np) = pb.data.page() else {
                if done == 0 {
                    return Err(-12); // ENOMEM
                }
                break;
            };
            unsafe { core::ptr::copy_nonoverlapping(pg.add(r), np, k) };
            pb.data.pages.push_back((np, 0, k, false));
            pb.data.len += k;
            done += k;
            left -= k;
            i += 1;
        }
        if done > 0 {
            pb.wrote += done as u64;
            pb.generation += 1;
        }
        drop(pa);
        drop(pb);
        if done > 0 {
            Pipe::wake(b);
        }
        Ok(done)
    }

}

impl PageQueue {
    /// splice で入れるかけら (ファイルの 1 ページの中のもの。空でなく 4KiB まで) を、足さない箱ひとつに
    fn push_piece(&mut self, src: &[u8]) -> Result<(), i64> {
        let p = self.page().ok_or(-12i64)?; // ENOMEM
        unsafe { core::ptr::copy_nonoverlapping(src.as_ptr(), p, src.len()) };
        self.pages.push_back((p, 0, src.len(), false));
        self.len += src.len();
        Ok(())
    }

}

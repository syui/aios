// DRM の syncobj (Linux の drivers/gpu/drm/drm_syncobj.c): 終わりの印 (fence) を入れる箱。ハンドルは DRM のファイルごと
// (1 から、いちばん小さいあき: Linux の xa_alloc)。fd にすれば (anon_inode:syncobj_file) ほかのプロセスへ渡せ、中の fence
// を sync_file として出し入れもできる。2 値の箱は fence を 1 つ、timeline の箱は点の番号のついた fence の列
// (dma_fence_chain) を持つ。
//   CREATE、DESTROY、HANDLE_TO_FD と FD_TO_HANDLE (EXPORT_SYNC_FILE、IMPORT_SYNC_FILE、TIMELINE)、WAIT、RESET、SIGNAL、
//   TIMELINE_WAIT、QUERY、TRANSFER、TIMELINE_SIGNAL、EVENTFD
// と、virtio_gpu の EXECBUFFER の in / out の syncobj (parse_deps と parse_post_deps)。エラーの順も Linux と同じ。
// aios の GPU の命令はホストが終えてから返るので、箱に入る fence はみな終わっている。だから待つのは「fence が入るまで」
// (WAIT_FOR_SUBMIT と WAIT_AVAILABLE: ほかのスレッドやプロセスが SIGNAL するまで) だけ。timeline の前の点も終わって
// いるので、Linux の dma_fence_chain_walk と同じく、たどるとすぐ消える: 箱はいちばん新しい点だけを覚えればよい
use crate::cell::Bkl;
use crate::file::{self, FileOps, Kind};
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering::Relaxed};

const ENOENT: i64 = 2;
const EBADF: i64 = 9;
const ENOMEM: i64 = 12;
const EFAULT: i64 = 14;
const EINVAL: i64 = 22;
const EMFILE: i64 = 24;
const ETIME: i64 = 62;

// ioctl の番号 (drm.h)
const NR_CREATE: u32 = 0xbf;
const NR_DESTROY: u32 = 0xc0;
const NR_HANDLE_TO_FD: u32 = 0xc1;
const NR_FD_TO_HANDLE: u32 = 0xc2;
const NR_WAIT: u32 = 0xc3;
const NR_RESET: u32 = 0xc4;
const NR_SIGNAL: u32 = 0xc5;
const NR_TIMELINE_WAIT: u32 = 0xca;
const NR_QUERY: u32 = 0xcb;
const NR_TRANSFER: u32 = 0xcc;
const NR_TIMELINE_SIGNAL: u32 = 0xcd;
const NR_EVENTFD: u32 = 0xcf;

const CREATE_SIGNALED: u32 = 1;
const IMPORT_SYNC_FILE: u32 = 1;
const EXPORT_SYNC_FILE: u32 = 1;
const FD_TIMELINE: u32 = 2;
const WAIT_ALL: u32 = 1;
const WAIT_FOR_SUBMIT: u32 = 2;
const WAIT_AVAILABLE: u32 = 4;
const WAIT_DEADLINE: u32 = 8;
const QUERY_LAST_SUBMITTED: u32 = 1;
/// EXECBUFFER の in の syncobj: 出したあと空にする (VIRTGPU_EXECBUF_SYNCOBJ_RESET)
const EXEC_RESET: u32 = 1;
/// kmalloc のいちばん大きいもの (4 KiB のページで MAX_PAGE_ORDER 10: 4 MiB)。ハンドルの表がこれをこえれば ENOMEM
const KMALLOC_MAX: usize = 4 << 20;
/// WAIT_FOR_SUBMIT で 1 つの fence を待つ長さ (DRM_SYNCOBJ_WAIT_FOR_SUBMIT_TIMEOUT: 5 秒)
const SUBMIT_TIMEOUT_NS: u64 = 5_000_000_000;

/// ioctl のカーネルの形の大きさ (syncobj のものでなければ None)
pub fn size(nr: u32) -> Option<usize> {
    Some(match nr {
        NR_CREATE | NR_DESTROY => 8,
        NR_HANDLE_TO_FD | NR_FD_TO_HANDLE => 24,
        NR_WAIT => 40,
        NR_RESET | NR_SIGNAL => 16,
        NR_TIMELINE_WAIT => 48,
        NR_QUERY | NR_TIMELINE_SIGNAL | NR_EVENTFD => 24,
        NR_TRANSFER => 32,
        _ => return None,
    })
}

/// fence (Linux の dma_fence) のうち、syncobj と sync_file が持つもの。どれも終わっている (ts: 終わった時刻 ns)
#[derive(Clone)]
pub enum Fence {
    /// virtio-gpu の fence (番号)。EXECBUFFER の印は、ホストが終えるまで ts が 0
    Gpu { seq: u64, ts: u64 },
    /// 印だけの終わった fence (dma_fence_get_stub と dma_fence_allocate_private_stub)
    Stub { ts: u64 },
    /// timeline の点 (dma_fence_chain): 点の番号 (seqno)、同じ流れの前の点の番号 (prev_seqno: 流れが変われば 0)、流れ
    /// (context)、中身。前の点はみな終わっていて、たどればすぐ消えるので持たない。signaled は終わった印 (Linux の
    /// DMA_FENCE_FLAG_SIGNALED_BIT): 中身が終わっていても、だれかが終わったかを聞くまで立たない (dma_fence_is_signaled)。
    /// 名前が変わる (sync_file_get_name)
    Chain { seqno: u64, prev_seqno: u64, ctx: u64, inner: Rc<Fence>, signaled: Rc<core::cell::Cell<bool>> },
}

/// dma_fence_get_stub の終わった時刻 (Linux は起動のとき。aios ははじめて使うとき)
static STUB_TS: AtomicU64 = AtomicU64::new(0);
/// dma_fence_context_alloc (0 は stub、1 は virtio-gpu の controlq)
static NEXT_CTX: AtomicU64 = AtomicU64::new(2);

impl Fence {
    /// だれでも使う終わった印 (dma_fence_get_stub)
    pub fn stub() -> Fence {
        if STUB_TS.load(Relaxed) == 0 {
            STUB_TS.store(crate::timer::uptime_ns().max(1), Relaxed);
        }
        Fence::Stub { ts: STUB_TS.load(Relaxed) }
    }

    /// いま終わった印 (dma_fence_allocate_private_stub(ktime_get()))
    pub fn now() -> Fence {
        Fence::Stub { ts: crate::timer::uptime_ns() }
    }

    pub fn ts(&self) -> u64 {
        match self {
            Fence::Gpu { ts, .. } | Fence::Stub { ts } => *ts,
            Fence::Chain { inner, .. } => inner.ts(),
        }
    }

    /// 中の fence (dma_fence_unwrap_for_each で出てくるもの)
    pub fn leaves(&self) -> Vec<Fence> {
        match self {
            Fence::Chain { inner, .. } => inner.leaves(),
            f => vec![f.clone()],
        }
    }

    /// 終わった印が立っているか (dma_fence_test_signaled_flag)。GPU の fence は答えが来たとき、stub は作ったときに立つ
    fn flagged(&self) -> bool {
        match self {
            Fence::Gpu { ts, .. } => *ts != 0,
            Fence::Stub { .. } => true,
            Fence::Chain { signaled, .. } => signaled.get(),
        }
    }

    /// 終わったかを聞く (dma_fence_is_signaled): 中身はみな終わっているので、timeline の点にも印が立つ
    pub fn check(&self) {
        if let Fence::Chain { signaled, .. } = self {
            signaled.set(true);
        }
    }

    /// (流れの名前、ドライバの名前、context、seqno)。終わった印が立っていれば、Linux 6.17 からの dma_fence_driver_name と
    /// dma_fence_timeline_name と同じく detached-driver と signaled-timeline (ドライバが外れていてもよいように)
    fn ids(&self) -> (&'static str, &'static str, u64, u64) {
        let (tl, drv, ctx, seq) = match self {
            Fence::Gpu { seq, .. } => ("controlq", "virtio_gpu", 1, *seq),
            Fence::Stub { .. } => ("stub", "stub", 0, 0),
            Fence::Chain { seqno, ctx, .. } => ("unbound", "dma_fence_chain", *ctx, *seqno),
        };
        if self.flagged() { ("signaled-timeline", "detached-driver", ctx, seq) } else { (tl, drv, ctx, seq) }
    }

    /// sync_file の名前 (sync_file_get_name: ドライバ-流れの名前 context-seqno。FILE_INFO で 31 文字に切れる)
    pub fn name(&self) -> String {
        let (tl, drv, ctx, seq) = self.ids();
        format!("{}-{}{}-{}", drv, tl, ctx, seq)
    }

    /// sync_fence_info の (obj_name、driver_name): 流れとドライバの名前
    pub fn names(&self) -> (&'static str, &'static str) {
        let (tl, drv, _, _) = self.ids();
        (tl, drv)
    }

    /// 点 point の fence (dma_fence_chain_find_seqno)。0 ならこれ、timeline でないか、まだその点がなければ None。前の点
    /// (たどって消えたところ) なら stub
    fn find_seqno(&self, point: u64) -> Option<Fence> {
        if point == 0 {
            return Some(self.clone());
        }
        match self {
            Fence::Chain { seqno, prev_seqno, .. } if *seqno >= point => Some(if *prev_seqno < point { self.clone() } else { Fence::stub() }),
            _ => None,
        }
    }

    /// 終わったものをまとめた fence (dma_fence_unwrap_merge: 終わっていないものがなければ、いちばん遅い時刻の stub)
    pub fn merged(fs: &[Fence]) -> Fence {
        Fence::Stub { ts: fs.iter().flat_map(|f| f.leaves()).map(|f| f.ts()).max().unwrap_or(0) }
    }
}

/// syncobj (Linux の struct drm_syncobj)。DRM のファイルのハンドルと、syncobj の fd が持つ
pub struct Syncobj {
    st: Bkl<Inner>,
}

struct Inner {
    fence: Option<Fence>,
    /// EVENTFD で頼まれたもの (eventfd, 点, flags): 点の fence が入ったら 1 を足して外す
    eventfds: Vec<(crate::epoll::EventFdRef, u64, u32)>,
}

/// syncobj を待つものが眠るところ (どの箱が変わっても起こす: 待つ人は自分の箱をまた見る)
static WAIT_CHAN: u8 = 0;

fn chan() -> usize {
    &raw const WAIT_CHAN as usize
}

impl Syncobj {
    fn new(fence: Option<Fence>) -> Rc<Syncobj> {
        Rc::new(Syncobj { st: Bkl::new(Inner { fence, eventfds: Vec::new() }) })
    }

    /// 点 point の fence (まだなければ None)
    fn find(&self, point: u64) -> Option<Fence> {
        self.st.get().fence.as_ref().and_then(|f| f.find_seqno(point))
    }

    /// QUERY の答え: timeline ならいちばん新しい点の番号 (前の点もみな終わっている)、でなければ 0。LAST_SUBMITTED でなければ
    /// いちばん新しい点が終わったかを聞く (印が立つ)
    fn point(&self, last_submitted: bool) -> u64 {
        match &self.st.get().fence {
            Some(f @ Fence::Chain { seqno, .. }) => {
                if !last_submitted {
                    f.check();
                }
                *seqno
            }
            _ => 0,
        }
    }

    /// fence を入れかえる (drm_syncobj_replace_fence。None で空に)
    pub fn replace(&self, f: Option<Fence>) {
        self.st.get().fence = f;
        self.changed();
    }

    /// timeline に点を足す (drm_syncobj_add_point と dma_fence_chain_init: 前の点が同じ流れで point がそのあとなら流れを
    /// つぐ。でなければ新しい流れで、番号は前の点より小さくならない)
    pub fn add_point(&self, f: Fence, point: u64) {
        let st = self.st.get();
        let chain = match &st.fence {
            Some(Fence::Chain { seqno, ctx, .. }) if point > *seqno => Fence::Chain { seqno: point, prev_seqno: *seqno, ctx: *ctx, inner: Rc::new(f), signaled: Default::default() },
            prev => {
                let seqno = match prev {
                    Some(Fence::Chain { seqno, .. }) => point.max(*seqno),
                    _ => point,
                };
                Fence::Chain { seqno, prev_seqno: 0, ctx: NEXT_CTX.fetch_add(1, Relaxed), inner: Rc::new(f), signaled: Default::default() }
            }
        };
        st.fence = Some(chain);
        self.changed();
    }

    /// 中身が変わった: 点の fence が入った EVENTFD に知らせ (fence はみな終わっているので、入ればすぐ)、待つ人を起こす
    fn changed(&self) {
        let st = self.st.get();
        let mut i = 0;
        while i < st.eventfds.len() {
            let (point, flags) = (st.eventfds[i].1, st.eventfds[i].2);
            if let Some(f) = st.fence.as_ref().and_then(|f| f.find_seqno(point)) {
                // WAIT_AVAILABLE でなければ、終わりを待つ (dma_fence_add_callback: timeline の点にも印が立つ)
                if flags & WAIT_AVAILABLE == 0 {
                    f.check();
                }
                let (e, _, _) = st.eventfds.remove(i);
                crate::epoll::signal(&e);
            } else {
                i += 1;
            }
        }
        crate::proc::wakeup(chan());
    }
}

/// DRM のファイルの syncobj のハンドル
pub type Table = BTreeMap<u32, Rc<Syncobj>>;

/// ハンドルを足す (いちばん小さい空いた番号: xa_alloc の XA_FLAGS_ALLOC1)
fn add_handle(tab: &Bkl<Table>, s: Rc<Syncobj>) -> u32 {
    let t = tab.get();
    let mut h = 1;
    while t.contains_key(&h) {
        h += 1;
    }
    t.insert(h, s);
    h
}

fn find(tab: &Bkl<Table>, h: u32) -> Option<Rc<Syncobj>> {
    tab.get().get(&h).cloned()
}

/// syncobj の fd (drm_syncobj_file_fops: 閉じるだけ)
pub struct SyncobjFile(Rc<Syncobj>);

impl FileOps for SyncobjFile {
    fn describe(&self) -> String {
        "anon_inode:syncobj_file".into()
    }
}

fn le32(a: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(a[o..o + 4].try_into().unwrap())
}

fn le64(a: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(a[o..o + 8].try_into().unwrap())
}

fn set32(a: &mut [u8], o: usize, v: u32) {
    a[o..o + 4].copy_from_slice(&v.to_le_bytes());
}

/// 絶対の CLOCK_MONOTONIC (ns) の期限を tick に (drm_timeout_abs_to_jiffies)。0 や過ぎた時刻は待たない (None)
fn deadline(abs_ns: i64) -> Option<u64> {
    let now = crate::timer::uptime_ns();
    if abs_ns <= 0 || abs_ns as u64 <= now {
        return None;
    }
    Some(crate::timer::ticks_after(abs_ns as u64 - now))
}

/// 期限 (tick) まで眠る。シグナルなら EINTR (Linux の ERESTARTSYS: SA_RESTART ならやり直す)
fn sleep(dl: u64) -> Result<(), i64> {
    crate::proc::sleep_until(chan(), dl).map(|_| ())
}

/// ハンドルの syncobj の点 point の fence (drm_syncobj_find_fence)。flags は WAIT_FOR_SUBMIT だけで、そのときは入るまで
/// 5 秒待つ (ETIME)
fn find_fence(tab: &Bkl<Table>, h: u32, point: u64, flags: u32) -> Result<Fence, i64> {
    let s = find(tab, h).ok_or(-ENOENT)?;
    if flags & !WAIT_FOR_SUBMIT != 0 {
        return Err(-EINVAL);
    }
    if let Some(f) = s.find(point) {
        return Ok(f);
    }
    if flags & WAIT_FOR_SUBMIT == 0 {
        return Err(-EINVAL);
    }
    let dl = crate::timer::ticks_after(SUBMIT_TIMEOUT_NS);
    loop {
        if let Some(f) = s.find(point) {
            return Ok(f);
        }
        if crate::timer::ticks() >= dl {
            return Err(-ETIME);
        }
        sleep(dl)?;
    }
}

/// ユーザーのハンドルの列を syncobj に (drm_syncobj_array_find)
fn array_find(tab: &Bkl<Table>, ptr: u64, count: u32) -> Result<Vec<Rc<Syncobj>>, i64> {
    let n = count as usize;
    if n * 4 > KMALLOC_MAX {
        return Err(-ENOMEM);
    }
    let mut hb = vec![0u8; n * 4];
    crate::proc::current().pt().copy_in(&mut hb, ptr as usize).ok_or(-EFAULT)?;
    if n * 8 > KMALLOC_MAX {
        return Err(-ENOMEM);
    }
    (0..n).map(|i| find(tab, le32(&hb, i * 4)).ok_or(-ENOENT)).collect()
}

/// ユーザーの点の列 (ptr が 0 ならみな 0)
fn points(ptr: u64, n: usize) -> Result<Vec<u64>, i64> {
    if ptr == 0 {
        return Ok(vec![0; n]);
    }
    let mut b = vec![0u8; n * 8];
    crate::proc::current().pt().copy_in(&mut b, ptr as usize).ok_or(-EFAULT)?;
    Ok((0..n).map(|i| le64(&b, i * 8)).collect())
}

/// WAIT と TIMELINE_WAIT (drm_syncobj_array_wait_timeout)。はじめに見て、入っていないものがあれば (WAIT_FOR_SUBMIT と
/// WAIT_AVAILABLE でなければ EINVAL)、入るまで期限まで眠る (ETIME)。答えは first_signaled: はじめに見たときにあった
/// いちばん前のもの。WAIT_ALL でなければ待ったあとに入ったいちばん前のもの (WAIT_ALL で待ったなら、はじめの答えのまま:
/// なければ ~0)
fn array_wait(objs: &[Rc<Syncobj>], points: &[u64], flags: u32, timeout: i64) -> Result<u32, i64> {
    let submit = flags & (WAIT_FOR_SUBMIT | WAIT_AVAILABLE) != 0;
    let all = flags & WAIT_ALL != 0;
    let mut first = !0;
    let mut signaled = 0;
    // 入っている fence は終わったかを聞く (WAIT_AVAILABLE でなければ: dma_fence_is_signaled。timeline の点に印が立つ)
    let found = |i: usize, s: &Rc<Syncobj>| {
        let f = s.find(points[i]);
        if let Some(f) = &f
            && flags & WAIT_AVAILABLE == 0
        {
            f.check();
        }
        f.is_some()
    };
    for (i, s) in objs.iter().enumerate() {
        if !found(i, s) {
            if submit {
                continue;
            }
            return Err(-EINVAL);
        }
        if signaled == 0 {
            first = i as u32;
        }
        signaled += 1;
    }
    if signaled == objs.len() || (signaled > 0 && !all) {
        return Ok(first);
    }
    let dl = deadline(timeout);
    loop {
        let mut n = 0;
        for (i, s) in objs.iter().enumerate() {
            if found(i, s) {
                if !all {
                    return Ok(i as u32);
                }
                n += 1;
            }
        }
        if n == objs.len() {
            return Ok(first);
        }
        match dl {
            Some(dl) if crate::timer::ticks() < dl => sleep(dl)?,
            _ => return Err(-ETIME),
        }
    }
}

/// syncobj の ioctl (DRIVER_SYNCOBJ と DRIVER_SYNCOBJ_TIMELINE: virtio_gpu はどちらもある)。a はカーネルの形
pub fn ioctl(tab: &Bkl<Table>, nr: u32, a: &mut [u8]) -> Result<(), i64> {
    let me = crate::proc::current();
    match nr {
        NR_CREATE => {
            if le32(a, 4) & !CREATE_SIGNALED != 0 {
                return Err(-EINVAL);
            }
            let s = Syncobj::new((le32(a, 4) & CREATE_SIGNALED != 0).then(Fence::now));
            set32(a, 0, add_handle(tab, s));
            Ok(())
        }
        NR_DESTROY => {
            if le32(a, 4) != 0 {
                return Err(-EINVAL);
            }
            tab.get().remove(&le32(a, 0)).map(|_| ()).ok_or(-EINVAL)
        }
        // handle, flags, fd, pad, point
        NR_HANDLE_TO_FD => {
            let (h, flags) = (le32(a, 0), le32(a, 4));
            if le32(a, 12) != 0 || flags & !(EXPORT_SYNC_FILE | FD_TIMELINE) != 0 {
                return Err(-EINVAL);
            }
            let point = if flags & FD_TIMELINE != 0 { le64(a, 16) } else { 0 };
            let ops: Rc<dyn FileOps> = if flags & EXPORT_SYNC_FILE != 0 {
                Rc::new(crate::drm::SyncFile::new(find_fence(tab, h, point, 0)?))
            } else {
                if le64(a, 16) != 0 {
                    return Err(-EINVAL);
                }
                Rc::new(SyncobjFile(find(tab, h).ok_or(-EINVAL)?))
            };
            let fd = me.files().add(file::new(Kind::Ops(ops), file::O_RDONLY), true, 0).ok_or(-EMFILE)?;
            set32(a, 8, fd as u32);
            Ok(())
        }
        NR_FD_TO_HANDLE => {
            let (h, flags, fd) = (le32(a, 0), le32(a, 4), le32(a, 8) as i32);
            if le32(a, 12) != 0 || flags & !(IMPORT_SYNC_FILE | FD_TIMELINE) != 0 {
                return Err(-EINVAL);
            }
            let point = if flags & FD_TIMELINE != 0 { le64(a, 16) } else { 0 };
            if flags & IMPORT_SYNC_FILE != 0 {
                let f = crate::drm::sync_file_fence(fd).ok_or(-EINVAL)?;
                let s = find(tab, h).ok_or(-ENOENT)?;
                if point != 0 { s.add_point(f, point) } else { s.replace(Some(f)) }
                return Ok(());
            }
            if le64(a, 16) != 0 {
                return Err(-EINVAL);
            }
            let f = if fd < 0 { None } else { me.files().get(fd as u64).cloned() };
            let s = f.and_then(|f| f.borrow().kind.ops::<SyncobjFile>()).ok_or(-EINVAL)?;
            set32(a, 0, add_handle(tab, s.0.clone()));
            Ok(())
        }
        // handles, timeout_nsec, count_handles, flags, first_signaled, pad, deadline_nsec
        NR_WAIT => {
            let (count, flags) = (le32(a, 16), le32(a, 20));
            if flags & !(WAIT_ALL | WAIT_FOR_SUBMIT | WAIT_DEADLINE) != 0 {
                return Err(-EINVAL);
            }
            if count == 0 {
                return Ok(());
            }
            let objs = array_find(tab, le64(a, 0), count)?;
            let first = array_wait(&objs, &vec![0; objs.len()], flags, le64(a, 8) as i64)?;
            set32(a, 24, first);
            Ok(())
        }
        // handles, points, timeout_nsec, count_handles, flags, first_signaled, pad, deadline_nsec
        NR_TIMELINE_WAIT => {
            let (count, flags) = (le32(a, 24), le32(a, 28));
            if flags & !(WAIT_ALL | WAIT_FOR_SUBMIT | WAIT_AVAILABLE | WAIT_DEADLINE) != 0 {
                return Err(-EINVAL);
            }
            if count == 0 {
                return Ok(());
            }
            let objs = array_find(tab, le64(a, 0), count)?;
            let pts = points(le64(a, 8), objs.len())?;
            let first = array_wait(&objs, &pts, flags, le64(a, 16) as i64)?;
            set32(a, 32, first);
            Ok(())
        }
        // handles, count_handles, pad
        NR_RESET | NR_SIGNAL => {
            if le32(a, 12) != 0 || le32(a, 8) == 0 {
                return Err(-EINVAL);
            }
            for s in array_find(tab, le64(a, 0), le32(a, 8))? {
                s.replace(if nr == NR_SIGNAL { Some(Fence::now()) } else { None });
            }
            Ok(())
        }
        // handles, points, count_handles, flags
        NR_TIMELINE_SIGNAL | NR_QUERY => {
            let (count, flags) = (le32(a, 16), le32(a, 20));
            let ok = if nr == NR_QUERY { QUERY_LAST_SUBMITTED } else { 0 };
            if flags & !ok != 0 || count == 0 {
                return Err(-EINVAL);
            }
            let objs = array_find(tab, le64(a, 0), count)?;
            if nr == NR_TIMELINE_SIGNAL {
                for (s, p) in objs.iter().zip(points(le64(a, 8), objs.len())?) {
                    s.add_point(Fence::stub(), p);
                }
                return Ok(());
            }
            // 点を 1 つずつ書く (書けなければそこで EFAULT)
            for (i, s) in objs.iter().enumerate() {
                me.pt().copy_out(le64(a, 8) as usize + i * 8, &s.point(flags != 0).to_le_bytes()).ok_or(-EFAULT)?;
            }
            Ok(())
        }
        // src_handle, dst_handle, src_point, dst_point, flags, pad
        NR_TRANSFER => {
            if le32(a, 28) != 0 {
                return Err(-EINVAL);
            }
            let dst = find(tab, le32(a, 4)).ok_or(-ENOENT)?;
            let f = find_fence(tab, le32(a, 0), le64(a, 8), le32(a, 24))?;
            match le64(a, 16) {
                // 2 値の箱へは、そのまま (timeline の点なら、点の fence のまま)
                0 => dst.replace(Some(f)),
                // timeline へは、まとめた終わった印を点に
                p => dst.add_point(Fence::merged(&[f]), p),
            }
            Ok(())
        }
        // handle, flags, point, fd, pad
        NR_EVENTFD => {
            if le32(a, 4) & !WAIT_AVAILABLE != 0 || le32(a, 20) != 0 {
                return Err(-EINVAL);
            }
            let s = find(tab, le32(a, 0)).ok_or(-ENOENT)?;
            let fd = le32(a, 16) as i32;
            let f = if fd < 0 { None } else { me.files().get(fd as u64).cloned() }.ok_or(-EBADF)?;
            let e = f.borrow().kind.ops::<crate::epoll::EventFdFile>().ok_or(-EINVAL)?.0.clone();
            s.st.get().eventfds.push((e, le64(a, 8), le32(a, 4)));
            s.changed();
            Ok(())
        }
        _ => Err(-EINVAL),
    }
}

/// EXECBUFFER の syncobj (drm_virtgpu_execbuffer_syncobj: handle, flags, point を stride ずつ)。ユーザーの形は stride までを
/// 読み、のこりは 0
fn exec_desc(ptr: u64, stride: u32, i: u32) -> Result<(u32, u32, u64), i64> {
    let mut d = [0u8; 16];
    let n = (stride as usize).min(16);
    let at = ptr.wrapping_add(i as u64 * stride as u64) as usize;
    crate::proc::current().pt().copy_in(&mut d[..n], at).ok_or(-EFAULT)?;
    Ok((le32(&d, 0), le32(&d, 4), le64(&d, 8)))
}

/// out の syncobj (virtio_gpu_parse_post_deps): flags は 0、ないハンドルは EINVAL。(箱, 点) の列
pub fn exec_out(tab: &Bkl<Table>, ptr: u64, stride: u32, n: u32) -> Result<Vec<(Rc<Syncobj>, u64)>, i64> {
    let mut v = Vec::new();
    for i in 0..n {
        let (h, flags, point) = exec_desc(ptr, stride, i)?;
        if flags != 0 {
            return Err(-EINVAL);
        }
        v.push((find(tab, h).ok_or(-EINVAL)?, point));
    }
    Ok(v)
}

/// in の syncobj (virtio_gpu_parse_deps): flags は RESET だけ、点の fence がなければ EINVAL (待たない)。fence は終わって
/// いるので待たない。RESET のものは出したあとで空にする (その列を返す)
pub fn exec_in(tab: &Bkl<Table>, ptr: u64, stride: u32, n: u32) -> Result<Vec<Rc<Syncobj>>, i64> {
    let mut reset = Vec::new();
    for i in 0..n {
        let (h, flags, point) = exec_desc(ptr, stride, i)?;
        if flags & !EXEC_RESET != 0 {
            return Err(-EINVAL);
        }
        find_fence(tab, h, point, 0)?;
        if flags & EXEC_RESET != 0 {
            reset.push(find(tab, h).ok_or(-EINVAL)?);
        }
    }
    Ok(reset)
}

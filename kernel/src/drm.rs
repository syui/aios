// DRM (Linux の drivers/gpu/drm): GPU に描かせる口。/dev/dri/card0 と /dev/dri/renderD128 (major 226、minor 0 と 128)
//
// いまは virtio-gpu の 3D (virgl: QEMU の virtio-gpu-gl-device) があるときだけ置く。Mesa の virgl は renderD128 を開いて、
//   GETPARAM と GET_CAPS     ホストの力 (capset: ホストの GL が何をできるか) を聞く
//   RESOURCE_CREATE          GPU のメモリ (BO: GEM のもの) を作る。中身はカーネルのページで、ホストにも渡す (ATTACH_BACKING)
//   MAP と mmap              BO のページをそのままユーザーに見せる (MAP の答えのオフセットで DRM の fd を mmap)
//   EXECBUFFER               virgl の命令の列 (シェーダー、描く、写す) をホストへ (SUBMIT_3D)。終わりの印 (sync_file) を返す
//   WAIT                     BO を使う命令が終わるまで待つ
// を使う (Linux の drivers/gpu/drm/virtio/virtgpu_ioctl.c、virtgpu_submit.c と同じ番号と形。test/drm-linux.sh の答え)。
// 命令はどれも、ホストが終えて答えるまで待つ (SUBMIT_3D には fence の印をつけるので、答えはホストが描き終えたとき)。
// だから印 (sync_file) はいつも「もう終わった」で、WAIT は待たない (Linux は待たずに返して、あとで fence で知らせる:
// 速さはちがうが、見える答えは同じ)。長い SUBMIT_3D は大きなロックを放して待つ (gpu::unlocked)。
// 画面に出す KMS (card0 の MODE_*) と PRIME (dma-buf) と syncobj はまだ: render だけのドライバ (Linux の
// DRIVER_MODESET のないもの) と同じ答え (KMS の ioctl と cap は EOPNOTSUPP、DRM_CAP_PRIME は 0)
use crate::cell::Bkl;
use crate::file::{self, FileOps, FileRef, Kind};
use crate::gpu;
use crate::kalloc;
use crate::memlayout::{v2p, PGSIZE};
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::Cell;

const MAJOR: u32 = 226;
const RENDER: u32 = 128;

const ENOENT: i64 = 2;
const EBADF: i64 = 9;
const ENOMEM: i64 = 12;
const EACCES: i64 = 13;
const EFAULT: i64 = 14;
const ENODEV: i64 = 19;
const EINVAL: i64 = 22;
const EMFILE: i64 = 24;
const ENOTTY: i64 = 25;
const EOPNOTSUPP: i64 = 95;

// virtio-gpu の 3D の命令 (virtio_gpu.h)
const RESOURCE_UNREF: u32 = 0x0102;
const RESOURCE_ATTACH_BACKING: u32 = 0x0106;
const GET_CAPSET: u32 = 0x0109;
const CTX_CREATE: u32 = 0x0200;
const CTX_DESTROY: u32 = 0x0201;
const CTX_ATTACH_RESOURCE: u32 = 0x0202;
const CTX_DETACH_RESOURCE: u32 = 0x0203;
const RESOURCE_CREATE_3D: u32 = 0x0204;
const TRANSFER_TO_HOST_3D: u32 = 0x0205;
const TRANSFER_FROM_HOST_3D: u32 = 0x0206;
const SUBMIT_3D: u32 = 0x0207;
const RESP_OK_CAPSET: u32 = 0x1103;

// ioctl の番号 (DRM_IOCTL_NR。種類は 'd')。0x40 から 0x9f はドライバ (virtgpu)、0xa0 からは KMS
const NR_VERSION: u32 = 0x00;
const NR_GET_UNIQUE: u32 = 0x01;
const NR_GEM_CLOSE: u32 = 0x09;
const NR_GET_CAP: u32 = 0x0c;
const NR_SET_CLIENT_CAP: u32 = 0x0d;
const NR_MAP: u32 = 0x41;
const NR_EXECBUFFER: u32 = 0x42;
const NR_GETPARAM: u32 = 0x43;
const NR_RESOURCE_CREATE: u32 = 0x44;
const NR_RESOURCE_INFO: u32 = 0x45;
const NR_TRANSFER_FROM_HOST: u32 = 0x46;
const NR_TRANSFER_TO_HOST: u32 = 0x47;
const NR_WAIT: u32 = 0x48;
const NR_GET_CAPS: u32 = 0x49;
const NR_RESOURCE_CREATE_BLOB: u32 = 0x4a;
const NR_CONTEXT_INIT: u32 = 0x4b;
const NR_PRIME_HANDLE_TO_FD: u32 = 0x2d;
const NR_PRIME_FD_TO_HANDLE: u32 = 0x2e;
const NR_SYNCOBJ_FIRST: u32 = 0xbf;
const NR_SYNCOBJ_LAST: u32 = 0xcf;
const COMMAND_BASE: u32 = 0x40;
const COMMAND_END: u32 = 0xa0;

/// mmap のオフセットの始まり (Linux の DRM_FILE_PAGE_OFFSET_START: 4 GiB)
const OFFSET_START: u64 = 1 << 32;

/// GPU のメモリ (GEM のもの)。ハンドルの表 (ファイルごと) が持ち、命令を出しているあいだは EXECBUFFER も持つ
struct Bo {
    /// ホストの資源の番号
    res: u32,
    pages: Rc<Vec<*mut u8>>,
    /// mmap のオフセット (MAP ではじめて決める。0 はまだ)
    off: Cell<u64>,
}

impl Bo {
    fn size(&self) -> usize {
        self.pages.len() * PGSIZE
    }
}

impl Drop for Bo {
    /// 写し (mmap) がページを持っていることがあるので、すぐには返さない: reap が、だれも持たなくなってから返す
    fn drop(&mut self) {
        DEV.get().dead.push((self.res, self.pages.clone()));
    }
}

struct Dev {
    /// 使っている資源の番号 (1 は画面: gpu.rs)
    res: Vec<u32>,
    /// 使っているコンテキストの番号
    ctx: Vec<u32>,
    /// 次の mmap のオフセット
    next_off: u64,
    /// 次の fence の番号
    fence: u64,
    /// capset の中身 (id, 版, 中身): GET_CAPS で聞いたもの (Linux の capset の cache)
    caps: Vec<(u32, u32, Vec<u8>)>,
    /// 落ちた BO (資源の番号、ページ)
    dead: Vec<(u32, Rc<Vec<*mut u8>>)>,
}

static DEV: Bkl<Dev> = Bkl::new(Dev { res: Vec::new(), ctx: Vec::new(), next_off: OFFSET_START, fence: 0, caps: Vec::new(), dead: Vec::new() });

/// いちばん小さい空いた番号 (min から。Linux の ida_alloc)
fn id_alloc(used: &mut Vec<u32>, min: u32) -> u32 {
    let mut id = min;
    while used.contains(&id) {
        id += 1;
    }
    used.push(id);
    id
}

fn id_free(used: &mut Vec<u32>, id: u32) {
    used.retain(|&x| x != id);
}

fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

fn le64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

fn set32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}

fn set64(b: &mut [u8], o: usize, v: u64) {
    b[o..o + 8].copy_from_slice(&v.to_le_bytes());
}

fn put32(b: &mut Vec<u8>, v: u32) {
    b.extend_from_slice(&v.to_le_bytes());
}

fn gpu() -> Result<&'static mut gpu::Gpu, i64> {
    gpu::get().ok_or(-ENODEV)
}

/// 答えのいらない命令 (答えの型は見ない: Linux もエラーの答えは dmesg に出すだけ)
fn send(req: &[u8]) -> Result<(), i64> {
    let mut r = [0u8; 24];
    gpu()?.call(req, &[], 0, 24, &mut r);
    Ok(())
}

/// 落ちた BO のうち、写しもないものをホストから外してページを返す (Linux の virtio_gpu_cleanup_object)
fn reap() {
    let d = DEV.get();
    let mut i = 0;
    while i < d.dead.len() {
        if Rc::strong_count(&d.dead[i].1) > 1 {
            i += 1;
            continue;
        }
        let (res, pages) = d.dead.swap_remove(i);
        let mut q = gpu::hdr3d(RESOURCE_UNREF, 0, 0, 0);
        put32(&mut q, res);
        put32(&mut q, 0);
        let _ = send(&q);
        for &p in pages.iter() {
            kalloc::put(p);
        }
        id_free(&mut d.res, res);
    }
}

/// /dev/dri を置く (gpu.rs が 3D のある virtio-gpu を見つけたとき)
pub fn init(pa: usize) {
    let parent = format!("{:x}.virtio_mmio", pa);
    let uevent = format!(
        "DRIVER=virtio-mmio\nOF_NAME=virtio_mmio\nOF_FULLNAME=/virtio_mmio@{:x}\nOF_COMPATIBLE_0=virtio,mmio\nOF_COMPATIBLE_N=1\nMODALIAS=of:Nvirtio_mmioT(null)Cvirtio,mmio\n",
        pa
    );
    for (name, minor) in [("card0", 0), ("renderD128", RENDER)] {
        // Linux の devtmpfs は 0600 で、udev が video / render のグループに 0660 にする。aios には udev がないので、
        // /dev/fb0 と同じく、だれでも使える 0666
        crate::fs::add_dev(&format!("dri/{}", name), 0o666, false, MAJOR, minor);
        crate::sysfs::add_chrdev(crate::sysfs::ChrDev {
            major: MAJOR,
            minor,
            name: name.into(),
            devname: format!("dri/{}", name),
            devtype: "drm_minor",
            class: "drm",
            parent: parent.clone(),
            parent_uevent: uevent.clone(),
        });
    }
    file::register_chrdev(MAJOR, |mi, _| (mi == 0 || mi == RENDER).then(|| Ok(Kind::Ops(Rc::new(DrmFile::new(mi))))));
    println!("drm: /dev/dri/card0 and renderD128 (virtio_gpu: virgl)");
}

/// 開いた DRM の口 (Linux の drm_file)
pub struct DrmFile {
    minor: u32,
    st: Bkl<State>,
}

struct State {
    /// このファイルのコンテキストの番号 (開いたときに決め、はじめて使うときにホストに作る: Linux の virtio_gpu_create_context)
    ctx: u32,
    ctx_made: bool,
    /// GEM のハンドル (1 から)
    handles: BTreeMap<u32, Rc<Bo>>,
}

impl DrmFile {
    fn new(minor: u32) -> DrmFile {
        let ctx = id_alloc(&mut DEV.get().ctx, 1);
        DrmFile { minor, st: Bkl::new(State { ctx, ctx_made: false, handles: BTreeMap::new() }) }
    }

    fn render(&self) -> bool {
        self.minor >= RENDER
    }

    fn bo(&self, h: u32) -> Option<Rc<Bo>> {
        self.st.get().handles.get(&h).cloned()
    }

    /// ホストにこのファイルのコンテキストを作る (はじめて使うとき。名前はプロセスの名前: Linux と同じ)
    fn context(&self) -> Result<u32, i64> {
        let st = self.st.get();
        if !st.ctx_made {
            let name = crate::proc::current().comm().as_bytes().to_vec();
            let mut q = gpu::hdr3d(CTX_CREATE, 0, 0, st.ctx);
            put32(&mut q, name.len().min(64) as u32);
            put32(&mut q, 0);
            let mut dbg = [0u8; 64];
            let n = name.len().min(64);
            dbg[..n].copy_from_slice(&name[..n]);
            q.extend_from_slice(&dbg);
            send(&q)?;
            st.ctx_made = true;
        }
        Ok(st.ctx)
    }

    fn ctx_resource(&self, typ: u32, res: u32) {
        let st = self.st.get();
        if st.ctx_made {
            let mut q = gpu::hdr3d(typ, 0, 0, st.ctx);
            put32(&mut q, res);
            put32(&mut q, 0);
            let _ = send(&q);
        }
    }

    fn do_ioctl(&self, req: u64, arg: usize) -> Result<i64, i64> {
        // 種類が 'd' でないものは ENOTTY (Linux の drm_ioctl)
        if (req >> 8) & 0xff != 0x64 {
            return Err(-ENOTTY);
        }
        reap();
        let nr = (req & 0xff) as u32;
        let usize_ = ((req >> 16) & 0x3fff) as usize;
        // (カーネルの形の大きさ, 読み書きの向き (bit 0 は書きこみ: IOC_IN、bit 1 は読みだし: IOC_OUT), render の口でも使えるか)
        const W: u8 = 1;
        const RW: u8 = 3;
        let (ksize, kdir, render_ok) = match nr {
            NR_VERSION => (64, RW, true),
            NR_GET_UNIQUE => (16, RW, false),
            NR_GEM_CLOSE => (8, W, true),
            NR_GET_CAP => (16, RW, true),
            NR_SET_CLIENT_CAP => (16, W, false),
            NR_MAP => (16, RW, true),
            NR_EXECBUFFER => (64, RW, true),
            NR_GETPARAM => (16, RW, true),
            NR_RESOURCE_CREATE => (56, RW, true),
            NR_RESOURCE_INFO => (16, RW, true),
            NR_TRANSFER_FROM_HOST | NR_TRANSFER_TO_HOST => (44, RW, true),
            NR_WAIT => (8, RW, true),
            NR_GET_CAPS => (24, RW, true),
            NR_RESOURCE_CREATE_BLOB => (48, RW, true),
            NR_CONTEXT_INIT => (16, RW, true),
            // PRIME (dma-buf) と syncobj はまだ (GET_CAP で 0 と答えるもの): Linux の、その力のないドライバと同じ EOPNOTSUPP
            NR_PRIME_HANDLE_TO_FD | NR_PRIME_FD_TO_HANDLE | NR_SYNCOBJ_FIRST..=NR_SYNCOBJ_LAST => return Err(-EOPNOTSUPP),
            // KMS: 画面を出すことはまだできない (render だけのドライバの答え)。render の口では EACCES
            n if n >= COMMAND_END => (0, 0, false),
            // ほかの core の ioctl (GET_MAGIC、AUTH_MAGIC、SET_MASTER、GEM_FLINK、昔のもの ...) は render の口では使えない
            n if n < COMMAND_BASE && self.render() => return Err(-EACCES),
            _ => return Err(-EINVAL),
        };
        if !render_ok && self.render() {
            return Err(-EACCES);
        }
        if nr >= COMMAND_END {
            return Err(-EOPNOTSUPP);
        }
        // 写すのは、ユーザーの番号とカーネルの形のどちらも、その向きのとき (Linux の drm_ioctl)
        let dir_in = req & (1 << 30) != 0 && kdir & W != 0;
        let dir_out = req & (1 << 31) != 0 && kdir & 2 != 0;
        let mut k = vec![0u8; ksize.max(usize_)];
        if dir_in && usize_ > 0 {
            crate::proc::current().pt().copy_in(&mut k[..usize_], arg).ok_or(-EFAULT)?;
        }
        let r = self.call(nr, &mut k[..ksize]);
        // 答えの形は、失敗しても書きもどす (Linux の drm_ioctl)
        if dir_out && usize_ > 0 && crate::proc::current().pt().copy_out(arg, &k[..usize_]).is_none() {
            return Err(-EFAULT);
        }
        r.map(|_| 0)
    }

    fn call(&self, nr: u32, a: &mut [u8]) -> Result<(), i64> {
        match nr {
            NR_VERSION => version(a),
            NR_GET_UNIQUE => {
                // unique はない (長さ 0。Linux 6.18 の virtio_gpu も同じ)
                set64(a, 0, 0);
                Ok(())
            }
            NR_GEM_CLOSE => {
                let h = le32(a, 0);
                let bo = self.st.get().handles.remove(&h).ok_or(-EINVAL)?;
                self.ctx_resource(CTX_DETACH_RESOURCE, bo.res);
                Ok(())
            }
            NR_GET_CAP => get_cap(a),
            // KMS のない (render だけの) ドライバには、決められる cap がない
            NR_SET_CLIENT_CAP => Err(-EOPNOTSUPP),
            NR_GETPARAM => getparam(a),
            NR_GET_CAPS => get_caps(a),
            NR_RESOURCE_CREATE => self.resource_create(a),
            NR_RESOURCE_INFO => {
                let bo = self.bo(le32(a, 0)).ok_or(-ENOENT)?;
                set32(a, 4, bo.res);
                set32(a, 8, bo.size() as u32);
                set32(a, 12, 0);
                Ok(())
            }
            NR_MAP => {
                let bo = self.bo(le32(a, 8)).ok_or(-ENOENT)?;
                if bo.off.get() == 0 {
                    let d = DEV.get();
                    bo.off.set(d.next_off);
                    d.next_off += bo.size() as u64;
                }
                set64(a, 0, bo.off.get());
                Ok(())
            }
            NR_EXECBUFFER => self.execbuffer(a),
            NR_TRANSFER_FROM_HOST => self.transfer(a, TRANSFER_FROM_HOST_3D),
            NR_TRANSFER_TO_HOST => self.transfer(a, TRANSFER_TO_HOST_3D),
            NR_WAIT => {
                // 命令はホストが終えてから返るので、待つものはない (NOWAIT でも EBUSY にならない)。flags は見ない (Linux 6.18 も)
                self.bo(le32(a, 0)).map(|_| ()).ok_or(-ENOENT)
            }
            // blob の資源と、capset をえらんで作るコンテキストは、デバイスにない (RESOURCE_BLOB、CONTEXT_INIT が 0)
            _ => Err(-EINVAL),
        }
    }

    /// RESOURCE_CREATE (drm_virtgpu_resource_create): target, format, bind, width, height, depth, array_size, last_level,
    /// nr_samples, flags, bo_handle (答え), res_handle (答え), size, stride
    fn resource_create(&self, a: &mut [u8]) -> Result<(), i64> {
        let g = gpu()?;
        let ctx = self.context()?;
        let size = match le32(a, 48) as usize {
            0 => PGSIZE,
            n => n,
        };
        let n = size.div_ceil(PGSIZE);
        // 1 ページに 256 項目の並びを、キューに入るだけ
        if n.div_ceil(PGSIZE / 16) + 2 > 256 {
            return Err(-ENOMEM);
        }
        let mut pages = Vec::with_capacity(n);
        for _ in 0..n {
            match kalloc::alloc() {
                Some(p) => pages.push(p),
                None => {
                    for p in pages {
                        kalloc::put(p);
                    }
                    return Err(-ENOMEM);
                }
            }
        }
        let res = id_alloc(&mut DEV.get().res, 2);
        let mut q = gpu::hdr3d(RESOURCE_CREATE_3D, 0, 0, ctx);
        put32(&mut q, res);
        for i in 0..9 {
            put32(&mut q, le32(a, i * 4));
        }
        // flags と padding
        put32(&mut q, le32(a, 36));
        put32(&mut q, 0);
        let _ = send(&q);
        // ページの並びをホストへ (ATTACH_BACKING: 項目は addr u64、len u32、padding u32)
        let mut lists = Vec::new();
        for chunk in pages.chunks(PGSIZE / 16) {
            let Some(l) = kalloc::alloc() else { break };
            let b = unsafe { core::slice::from_raw_parts_mut(l, PGSIZE) };
            for (k, &p) in chunk.iter().enumerate() {
                set64(b, k * 16, v2p(p as usize) as u64);
                set32(b, k * 16 + 8, PGSIZE as u32);
            }
            lists.push(l);
        }
        let bo = Rc::new(Bo { res, pages: Rc::new(pages), off: Cell::new(0) });
        if lists.len() != n.div_ceil(PGSIZE / 16) {
            for l in lists {
                kalloc::put(l);
            }
            return Err(-ENOMEM);
        }
        let mut q = gpu::hdr3d(RESOURCE_ATTACH_BACKING, 0, 0, 0);
        put32(&mut q, res);
        put32(&mut q, n as u32);
        let mut r = [0u8; 24];
        g.call(&q, &lists, n * 16, 24, &mut r);
        for l in lists {
            kalloc::put(l);
        }
        self.ctx_resource(CTX_ATTACH_RESOURCE, res);
        // ハンドルはいちばん小さい空いた番号 (1 から: Linux の idr_alloc)
        let st = self.st.get();
        let mut h = 1;
        while st.handles.contains_key(&h) {
            h += 1;
        }
        st.handles.insert(h, bo);
        set32(a, 40, h);
        set32(a, 44, res);
        Ok(())
    }

    /// EXECBUFFER (drm_virtgpu_execbuffer): flags, size, command, bo_handles, num_bo_handles, fence_fd, ring_idx,
    /// syncobj_stride, num_in_syncobjs, num_out_syncobjs, in_syncobjs, out_syncobjs
    fn execbuffer(&self, a: &mut [u8]) -> Result<(), i64> {
        const FENCE_FD_IN: u32 = 1;
        const FENCE_FD_OUT: u32 = 2;
        const RING_IDX: u32 = 4;
        let flags = le32(a, 0);
        let size = le32(a, 4) as usize;
        let (cmd, handles, nh) = (le64(a, 8) as usize, le64(a, 16) as usize, le32(a, 24) as usize);
        let fence_fd = le32(a, 28) as i32;
        if flags & !(FENCE_FD_IN | FENCE_FD_OUT | RING_IDX) != 0 {
            return Err(-EINVAL);
        }
        // ring はコンテキストを capset で作ったときだけ (CONTEXT_INIT がないので、ない)。syncobj もまだ
        if flags & RING_IDX != 0 || le32(a, 40) != 0 || le32(a, 44) != 0 {
            return Err(-EINVAL);
        }
        let ctx = self.context()?;
        let me = crate::proc::current();
        // 前の印 (sync_file): どれももう終わっているので、本物かだけを見る
        if flags & FENCE_FD_IN != 0 {
            let f = me.files().get(fence_fd as u64).cloned().ok_or(-EINVAL)?;
            if f.borrow().kind.ops::<SyncFile>().is_none() {
                return Err(-EINVAL);
            }
        }
        // 命令が使う BO (ホストが終えるまで落とさない)
        let mut hb = vec![0u8; nh * 4];
        if nh > 0 {
            me.pt().copy_in(&mut hb, handles).ok_or(-EFAULT)?;
        }
        let mut bos = Vec::with_capacity(nh);
        for i in 0..nh {
            bos.push(self.bo(le32(&hb, i * 4)).ok_or(-ENOENT)?);
        }
        // 命令の列をカーネルのページへ
        let n = size.div_ceil(PGSIZE);
        if n + 2 > 256 {
            return Err(-ENOMEM);
        }
        let mut pages: Vec<*mut u8> = Vec::with_capacity(n);
        let free = |pages: Vec<*mut u8>| {
            for p in pages {
                kalloc::put(p);
            }
        };
        for i in 0..n {
            let Some(p) = kalloc::alloc_dirty() else {
                free(pages);
                return Err(-ENOMEM);
            };
            pages.push(p);
            let len = (size - i * PGSIZE).min(PGSIZE);
            let b = unsafe { core::slice::from_raw_parts_mut(p, len) };
            if me.pt().copy_in(b, cmd + i * PGSIZE).is_none() {
                free(pages);
                return Err(-EFAULT);
            }
        }
        // 終わりの印の fd は先に作っておく (fd が足りなければ出さない: Linux の get_unused_fd_flags)
        let d = DEV.get();
        d.fence += 1;
        let fence = d.fence;
        let mut out_fd = None;
        if flags & FENCE_FD_OUT != 0 {
            let sf = SyncFile::new(fence);
            match me.files().add(file::new(Kind::Ops(Rc::new(sf)), file::O_RDONLY), true, 0) {
                Some(fd) => out_fd = Some(fd),
                None => {
                    free(pages);
                    return Err(-EMFILE);
                }
            }
        }
        let mut q = gpu::hdr3d(SUBMIT_3D, gpu::FLAG_FENCE, fence, ctx);
        put32(&mut q, size as u32);
        put32(&mut q, 0);
        let g = gpu()?;
        let mut r = [0u8; 24];
        // ホストが描き終えるまで (fence の答え)。大きなロックは放す
        gpu::unlocked(|| g.call(&q, &pages, size, 24, &mut r));
        let now = crate::timer::uptime_ns();
        free(pages);
        drop(bos);
        if let Some(fd) = out_fd {
            if let Some(f) = crate::proc::current().files().get(fd as u64).cloned() {
                if let Some(sf) = f.borrow().kind.ops::<SyncFile>() {
                    sf.signal(now);
                }
            }
            set32(a, 28, fd as u32);
        }
        Ok(())
    }

    /// TRANSFER_FROM_HOST / TO_HOST (drm_virtgpu_3d_transfer_*): bo_handle, box (x, y, z, w, h, d), level, offset, stride,
    /// layer_stride。blob でない資源では stride と layer_stride は 0 (Linux と同じく、ほかは EINVAL)
    fn transfer(&self, a: &mut [u8], typ: u32) -> Result<(), i64> {
        let ctx = self.context()?;
        let bo = self.bo(le32(a, 0)).ok_or(-ENOENT)?;
        if le32(a, 36) != 0 || le32(a, 40) != 0 {
            return Err(-EINVAL);
        }
        let d = DEV.get();
        d.fence += 1;
        let mut q = gpu::hdr3d(typ, gpu::FLAG_FENCE, d.fence, ctx);
        for i in 0..6 {
            put32(&mut q, le32(a, 4 + i * 4));
        }
        q.extend_from_slice(&(le32(a, 32) as u64).to_le_bytes());
        put32(&mut q, bo.res);
        put32(&mut q, le32(a, 28));
        put32(&mut q, 0);
        put32(&mut q, 0);
        let g = gpu()?;
        let mut r = [0u8; 24];
        gpu::unlocked(|| g.call(&q, &[], 0, 24, &mut r));
        Ok(())
    }

    /// 閉じた: ハンドルをコンテキストから外して落とし、コンテキストをこわす (Linux の drm_gem_release と postclose)
    fn close(&self) {
        let st = self.st.get();
        let handles = core::mem::take(&mut st.handles);
        for bo in handles.values() {
            self.ctx_resource(CTX_DETACH_RESOURCE, bo.res);
        }
        drop(handles);
        let st = self.st.get();
        if st.ctx_made {
            let _ = send(&gpu::hdr3d(CTX_DESTROY, 0, 0, st.ctx));
        }
        id_free(&mut DEV.get().ctx, st.ctx);
        reap();
    }
}

/// VERSION (drm_version): 版と、名前・日付・説明 (それぞれ長さと置き場)。置き場が小さければ切って、長さは本当の長さ
fn version(a: &mut [u8]) -> Result<(), i64> {
    set32(a, 0, 0);
    set32(a, 4, 1);
    set32(a, 8, 0);
    let pt = crate::proc::current().pt();
    for (o, v) in [(16usize, "virtio_gpu"), (32, "0"), (48, "virtio GPU")] {
        let (len, buf) = (le64(a, o) as usize, le64(a, o + 8) as usize);
        set64(a, o, v.len() as u64);
        if len > 0 && buf != 0 {
            pt.copy_out(buf, &v.as_bytes()[..len.min(v.len())]).ok_or(-EFAULT)?;
        }
    }
    Ok(())
}

/// GET_CAP (drm_get_cap): render だけのドライバのもの (KMS の cap は EOPNOTSUPP)
fn get_cap(a: &mut [u8]) -> Result<(), i64> {
    const TIMESTAMP_MONOTONIC: u64 = 6;
    const PRIME: u64 = 5;
    const SYNCOBJ: u64 = 0x13;
    const SYNCOBJ_TIMELINE: u64 = 0x14;
    let v = match le64(a, 0) {
        TIMESTAMP_MONOTONIC => 1,
        // dma-buf (PRIME) はまだ: import も export もない
        PRIME | SYNCOBJ | SYNCOBJ_TIMELINE => 0,
        _ => return Err(-EOPNOTSUPP),
    };
    set64(a, 8, v);
    Ok(())
}

/// GETPARAM (drm_virtgpu_getparam): param と、答えの int を書く場所
fn getparam(a: &mut [u8]) -> Result<(), i64> {
    let g = gpu()?;
    let v: u32 = match le64(a, 0) {
        // 3D_FEATURES
        1 => g.virgl as u32,
        // CAPSET_QUERY_FIX
        2 => 1,
        // RESOURCE_BLOB、HOST_VISIBLE、CROSS_DEVICE、CONTEXT_INIT: デバイスにない (feature をえらんでいない)
        3..=6 => 0,
        // SUPPORTED_CAPSET_IDs
        7 => g.capsets.iter().fold(0, |m, c| if c.0 < 32 { m | 1 << c.0 } else { m }),
        // EXPLICIT_DEBUG_NAME (CONTEXT_INIT があるときだけ)
        8 => 0,
        _ => return Err(-EINVAL),
    };
    crate::proc::current().pt().copy_out(le64(a, 8) as usize, &v.to_le_bytes()).ok_or(-EFAULT)?;
    Ok(())
}

/// GET_CAPS (drm_virtgpu_get_caps): cap_set_id、cap_set_ver、addr、size。ホストの capset を、小さいほうの大きさだけ写す
fn get_caps(a: &mut [u8]) -> Result<(), i64> {
    let g = gpu()?;
    let (id, ver, addr, size) = (le32(a, 0), le32(a, 4), le64(a, 8) as usize, le32(a, 16) as usize);
    if g.capsets.is_empty() {
        return Err(-38);
    }
    if size == 0 {
        return Err(-EINVAL);
    }
    let &(_, _, max) = g.capsets.iter().find(|c| c.0 == id && c.1 >= ver).ok_or(-EINVAL)?;
    let d = DEV.get();
    if !d.caps.iter().any(|c| c.0 == id && c.1 == ver) {
        let mut q = gpu::hdr3d(GET_CAPSET, 0, 0, 0);
        put32(&mut q, id);
        put32(&mut q, ver);
        let len = 24 + max as usize;
        let mut r = vec![0u8; len];
        if g.call(&q, &[], 0, len, &mut r) != RESP_OK_CAPSET {
            return Err(-EINVAL);
        }
        d.caps.push((id, ver, r[24..].to_vec()));
    }
    let c = &d.caps.iter().find(|c| c.0 == id && c.1 == ver).unwrap().2;
    crate::proc::current().pt().copy_out(addr, &c[..size.min(c.len())]).ok_or(-EFAULT)?;
    Ok(())
}

impl FileOps for DrmFile {
    fn describe(&self) -> String {
        if self.render() { "/dev/dri/renderD128".into() } else { "/dev/dri/card0".into() }
    }
    fn class(&self) -> usize {
        7
    }
    fn opened(&self) -> bool {
        true
    }
    fn stat(&self) -> file::Stat {
        file::Stat::dev(crate::vfs::S_IFCHR | 0o666, (MAJOR as u64) << 8 | self.minor as u64)
    }
    fn mount(&self) -> (u64, bool) {
        file::dev_mount(&self.describe())
    }
    fn poll_keys(&self) -> Option<Vec<usize>> {
        Some(vec![])
    }
    fn ioctl(&self, req: u64, arg: usize, _nonblock: bool) -> Option<Result<i64, i64>> {
        Some(self.do_ioctl(req, arg))
    }
    /// BO のページをそのまま (オフセットは MAP の答え。大きさは BO より大きくできない: Linux の drm_gem_mmap)
    fn mmap(&self, fr: &FileRef, off: usize, len: usize, _prot: u64) -> Option<Result<crate::vm::Backing, i64>> {
        Some((|| {
            let st = self.st.get();
            let bo = st.handles.values().find(|b| b.off.get() == off as u64 && b.off.get() != 0).ok_or(-EINVAL)?;
            if len > bo.size() {
                return Err(-EINVAL);
            }
            Ok(crate::vm::Backing::Pages { pages: bo.pages.clone(), off: 0, hold: Some(fr.clone()) })
        })())
    }
    fn release(&self) {
        self.close();
    }
}

/// 終わりの印 (Linux の sync_file: anon_inode:sync_file)。命令はホストが終えてから返るので、いつも終わっている
pub struct SyncFile {
    /// (fence の番号, 終わった時刻 ns)。fence はどれもデバイスの 1 本の流れ (Linux の fence context) のもの
    fences: Vec<(u64, Cell<u64>)>,
    /// MERGE でつけた名前 (Linux の user_name。なければ fence から作る)
    name: String,
}

impl SyncFile {
    fn new(fence: u64) -> SyncFile {
        SyncFile { fences: vec![(fence, Cell::new(0))], name: String::new() }
    }

    /// 終わった時刻を書く (命令の答えが来たとき)
    fn signal(&self, now: u64) {
        for f in &self.fences {
            f.1.set(now);
        }
    }

    fn do_ioctl(&self, req: u64, arg: usize) -> Result<i64, i64> {
        const SYNC_IOC_MERGE: u64 = 0xc030_3e03;
        const SYNC_IOC_FILE_INFO: u64 = 0xc038_3e04;
        const SYNC_IOC_SET_DEADLINE: u64 = 0x4010_3e05;
        let me = crate::proc::current();
        match req {
            SYNC_IOC_MERGE => {
                // sync_merge_data: name[32], fd2, fence (答え), flags, pad
                let mut m = [0u8; 48];
                me.pt().copy_in(&mut m, arg).ok_or(-EFAULT)?;
                if le32(&m, 40) != 0 || le32(&m, 44) != 0 {
                    return Err(-EINVAL);
                }
                let f2 = me.files().get(le32(&m, 32) as u64).cloned().ok_or(-EBADF)?;
                let other = f2.borrow().kind.ops::<SyncFile>().ok_or(-EINVAL)?;
                // 同じ流れの fence は、あとのもの 1 つにまとまる (Linux の dma_fence_unwrap_merge)
                let last = self.fences.iter().chain(other.fences.iter()).max_by_key(|f| f.0).cloned();
                let n = m[..32].iter().position(|&c| c == 0).unwrap_or(31).min(31);
                let name = String::from_utf8_lossy(&m[..n]).into_owned();
                let sf = SyncFile { fences: last.into_iter().collect(), name };
                let fd = me.files().add(file::new(Kind::Ops(Rc::new(sf)), file::O_RDONLY), true, 0).ok_or(-EMFILE)?;
                set32(&mut m, 36, fd as u32);
                if me.pt().copy_out(arg, &m).is_none() {
                    drop(me.files().set(fd, None));
                    return Err(-EFAULT);
                }
                Ok(0)
            }
            SYNC_IOC_FILE_INFO => {
                // sync_file_info: name[32], status, flags, num_fences, pad, sync_fence_info (置き場)
                let mut i = [0u8; 56];
                me.pt().copy_in(&mut i, arg).ok_or(-EFAULT)?;
                if le32(&i, 36) != 0 || le32(&i, 44) != 0 {
                    return Err(-EINVAL);
                }
                let (want, buf) = (le32(&i, 40) as usize, le64(&i, 48) as usize);
                let name = match self.fences.as_slice() {
                    _ if !self.name.is_empty() => self.name.clone(),
                    [(seq, _)] => format!("virtio_gpu-controlq1-{}", seq),
                    _ => "dma_fence_array".into(),
                };
                i[..32].fill(0);
                i[..name.len().min(31)].copy_from_slice(&name.as_bytes()[..name.len().min(31)]);
                set32(&mut i, 32, 1);
                if want > 0 {
                    if want < self.fences.len() {
                        return Err(-EINVAL);
                    }
                    // sync_fence_info: obj_name[32], driver_name[32], status, flags, timestamp_ns
                    let mut out = Vec::with_capacity(self.fences.len() * 80);
                    for (_, ts) in &self.fences {
                        let ts = ts.get();
                        let mut e = [0u8; 80];
                        e[..8].copy_from_slice(b"controlq");
                        e[32..42].copy_from_slice(b"virtio_gpu");
                        set32(&mut e, 64, 1);
                        set64(&mut e, 72, ts);
                        out.extend_from_slice(&e);
                    }
                    me.pt().copy_out(buf, &out).ok_or(-EFAULT)?;
                }
                set32(&mut i, 40, self.fences.len() as u32);
                me.pt().copy_out(arg, &i).ok_or(-EFAULT)?;
                Ok(0)
            }
            SYNC_IOC_SET_DEADLINE => Ok(0),
            _ => Err(-ENOTTY),
        }
    }
}

impl FileOps for SyncFile {
    fn describe(&self) -> String {
        "anon_inode:sync_file".into()
    }
    fn pollable(&self) -> bool {
        true
    }
    /// いつも読める (終わっている: POLLIN)
    fn readiness(&self) -> (bool, bool, bool) {
        (true, false, false)
    }
    fn poll_keys(&self) -> Option<Vec<usize>> {
        Some(vec![])
    }
    fn ioctl(&self, req: u64, arg: usize, _nonblock: bool) -> Option<Result<i64, i64>> {
        Some(self.do_ioctl(req, arg))
    }
}

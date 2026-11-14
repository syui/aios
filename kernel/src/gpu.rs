// virtio-gpu (2D): 画面をひとつ用意して、/dev/fb0 (Linux の fbdev と同じ形) にする
//
// 絵のメモリ (フレームバッファ) はカーネルのページの集まり。デバイスに「資源 1 番」として渡し
// (RESOURCE_CREATE_2D + ATTACH_BACKING)、画面 0 に映す (SET_SCANOUT)。
// ユーザーは mmap で直に描くか write で書き、描き終わったら FBIOPAN_DISPLAY (か write、msync) で
// 知らせる。そのときにデバイスへ写して (TRANSFER_TO_HOST_2D) 画面を描きなおす (RESOURCE_FLUSH)。
// FBIO_DAMAGE (aios だけ) は変わった四角 (x, y, w, h) だけを送る (写しなおさない。aiwm が使う)。
// 形は XRGB8888 (メモリの並びは B, G, R, X)。要求は virtio-blk と同じく、出して終わるまで待つ
//
// 3D (VIRTIO_GPU_F_VIRGL: QEMU の virtio-gpu-gl-device) があれば、capset (Mesa の virgl が読むホストの GL の力) を
// 起こすときに聞いておき、drm.rs (/dev/dri) が 3D の命令 (CTX_CREATE、RESOURCE_CREATE_3D、SUBMIT_3D ...) を call で送る。
// 資源の番号は 1 が画面、3D のものは 2 から (Linux の virtio_gpu_resource_id_get と同じく、使い終わったものもまた使う)
use crate::cell::Bkl;
use crate::kalloc;
use crate::memlayout::{v2p, PGSIZE};
use crate::virtio::{self, Mmio, Queue, DESC_NEXT, DESC_WRITE};
use alloc::rc::Rc;
use alloc::vec::Vec;

const DEVICE_GPU: u32 = 16;
const GET_DISPLAY_INFO: u32 = 0x0100;
pub const RESOURCE_CREATE_2D: u32 = 0x0101;
const SET_SCANOUT: u32 = 0x0103;
const RESOURCE_FLUSH: u32 = 0x0104;
const TRANSFER_TO_HOST_2D: u32 = 0x0105;
/// カーソルのキュー (cursorq: virtqueue 1) の命令: 絵をかえる、動かすだけ
const UPDATE_CURSOR: u32 = 0x0300;
const MOVE_CURSOR: u32 = 0x0301;
const RESOURCE_ATTACH_BACKING: u32 = 0x0106;
const GET_CAPSET_INFO: u32 = 0x0108;
const GET_EDID: u32 = 0x010a;
const RESP_OK_EDID: u32 = 0x1104;
/// VIRTIO_GPU_F_EDID: 画面の EDID を聞ける
const F_EDID: u64 = 1 << 1;
pub const RESP_OK_NODATA: u32 = 0x1100;
const RESP_OK_DISPLAY_INFO: u32 = 0x1101;
const RESP_OK_CAPSET_INFO: u32 = 0x1102;
/// VIRTIO_GPU_F_VIRGL: 3D (virgl) の命令が使える
const F_VIRGL: u64 = 1;
/// 要求の頭の flags: この要求が終わったら (ホストが描き終えたら) 答える
pub const FLAG_FENCE: u32 = 1;
/// キューの入れ物の数 (QEMU の control queue は 256): 3D の命令の中身 (Mesa は 256 KiB まで) をページごとに渡す
const QUEUE_MAX: usize = 256;
/// B8G8R8X8 (XRGB8888)
pub const FORMAT_XRGB: u32 = 2;
const RESOURCE: u32 = 1;
/// 1 ページに入る ATTACH_BACKING の項目 (addr u64, len u32, pad u32)
const ENTRIES_PER_PAGE: usize = PGSIZE / 16;

pub struct Gpu {
    mmio: Mmio,
    q: Queue,
    /// カーソルのキュー (Linux の virtio_gpu の cursorq) と、命令を置くページ
    cq: Option<Queue>,
    creq: *mut u8,
    pub width: u32,
    pub height: u32,
    /// フレームバッファのページ (カーネルの仮想アドレス)。並びの順に画面の上から
    pub pages: Rc<Vec<*mut u8>>,
    /// 要求と応答に使うページ
    req: *mut u8,
    resp: *mut u8,
    /// 3D (virgl) が使える
    pub virgl: bool,
    /// capset (id, いちばん新しい版, 大きさ): GET_CAPSET_INFO の答え
    pub capsets: Vec<(u32, u32, u32)>,
    /// 1 本目の virtio-mmio の口の物理番地 (/sys/devices/platform/ADDR.virtio_mmio)
    pub pa: usize,
    /// 画面 0 の EDID (GET_EDID。なければ空) と、GET_DISPLAY_INFO の大きさ (なければ 0)
    pub edid: Vec<u8>,
    pub info: (u32, u32),
}

static GPU: Bkl<Option<Gpu>> = Bkl::new(None);

/// キュー (と要求・応答のページ) のロック: 要求をひとつ出して答えを待つあいだ持つ。デバイスが終わるまで回って待つので、
/// 長くかかるもの (画面へ送る、3D の命令) は大きなロックを放して (unlocked) これだけを持つ (システムコールの中は割り込みが
/// 止まっているので、途中で放しても横取りされない)
static SEND: crate::spinlock::SpinLock<()> = crate::spinlock::SpinLock::new(());

/// 大きなロックを放して f (デバイスへ送る: call が SEND を持つ)。終わったら大きなロックを取りなおす
pub fn unlocked<R>(f: impl FnOnce() -> R) -> R {
    crate::smp::unlock();
    let r = f();
    crate::smp::lock();
    r
}

#[track_caller]
pub fn get() -> Option<&'static mut Gpu> {
    GPU.get().as_mut()
}

fn hdr(typ: u32) -> [u8; 24] {
    let mut h = [0u8; 24];
    h[0..4].copy_from_slice(&typ.to_le_bytes());
    h
}

/// 3D の要求の頭 (virtio_gpu_ctrl_hdr): 型、flags、fence_id、ctx_id
pub fn hdr3d(typ: u32, flags: u32, fence: u64, ctx: u32) -> Vec<u8> {
    let mut h = hdr(typ).to_vec();
    h[4..8].copy_from_slice(&flags.to_le_bytes());
    h[8..16].copy_from_slice(&fence.to_le_bytes());
    h[16..20].copy_from_slice(&ctx.to_le_bytes());
    h
}

fn put(b: &mut Vec<u8>, v: u32) {
    b.extend_from_slice(&v.to_le_bytes());
}

impl Gpu {
    /// 要求 (req) と、ページの並び (extra、デバイスが読む) を送り、応答の型を返す
    fn cmd(&mut self, req: &[u8], extra: &[*mut u8], extra_len: usize, resp_len: usize) -> u32 {
        let mut r = [0u8; 4];
        self.call(req, extra, extra_len, resp_len, &mut r)
    }

    /// 要求を出して、答え (resp_len バイト) の頭を out に写す。答えの型を返す (送れなければ 0)。
    /// SEND を持って (ほかの CPU の要求と混ざらない)、デバイスが答えるまで回って待つ
    pub fn call(&mut self, req: &[u8], extra: &[*mut u8], extra_len: usize, resp_len: usize, out: &mut [u8]) -> u32 {
        if req.len() > PGSIZE || resp_len > PGSIZE || extra.len() + 2 > self.q.size {
            return 0;
        }
        let _s = SEND.lock();
        unsafe {
            core::ptr::copy_nonoverlapping(req.as_ptr(), self.req, req.len());
            core::ptr::write_bytes(self.resp, 0, resp_len);
        }
        let mut i = 0;
        *self.q.desc(i) = virtio::Desc { addr: v2p(self.req as usize) as u64, len: req.len() as u32, flags: DESC_NEXT, next: 1 };
        let mut left = extra_len;
        for &p in extra {
            let n = left.min(PGSIZE);
            left -= n;
            *self.q.desc(i + 1) = virtio::Desc { addr: v2p(p as usize) as u64, len: n as u32, flags: DESC_NEXT, next: (i + 2) as u16 };
            i += 1;
        }
        *self.q.desc(i + 1) = virtio::Desc { addr: v2p(self.resp as usize) as u64, len: resp_len as u32, flags: DESC_WRITE, next: 0 };
        self.q.desc(i).next = (i + 1) as u16;
        self.q.push(0);
        self.q.notify(&self.mmio);
        let t0 = crate::timer::uptime_ns();
        while self.q.pop_used().is_none() {
            core::hint::spin_loop();
        }
        crate::smp::dev_wait(1, t0);
        self.mmio.ack();
        let r = unsafe { core::slice::from_raw_parts(self.resp, resp_len) };
        let n = out.len().min(resp_len);
        out[..n].copy_from_slice(&r[..n]);
        u32::from_le_bytes(r[..4].try_into().unwrap())
    }

    /// (x, y, w, h) の範囲を画面へ
    pub fn flush(&mut self, x: u32, y: u32, w: u32, h: u32) {
        if w == 0 || h == 0 {
            return;
        }
        let rect = |b: &mut Vec<u8>| {
            for v in [x, y, w, h] {
                put(b, v);
            }
        };
        let mut t = hdr(TRANSFER_TO_HOST_2D).to_vec();
        rect(&mut t);
        let off = (y as u64 * self.width as u64 + x as u64) * 4;
        t.extend_from_slice(&off.to_le_bytes());
        put(&mut t, RESOURCE);
        put(&mut t, 0);
        self.cmd(&t, &[], 0, 24);
        let mut f = hdr(RESOURCE_FLUSH).to_vec();
        rect(&mut f);
        put(&mut f, RESOURCE);
        put(&mut f, 0);
        self.cmd(&f, &[], 0, 24);
    }

    pub fn flush_all(&mut self) {
        let (w, h) = (self.width, self.height);
        self.flush(0, 0, w, h);
    }

    /// 画面 0 に資源 res の (x, y, w, h) を映す (SET_SCANOUT。res が 0 なら消す)。/dev/fb0 は資源 1
    pub fn scanout(&mut self, res: u32, x: u32, y: u32, w: u32, h: u32) -> bool {
        let mut s = hdr(SET_SCANOUT).to_vec();
        for v in [x, y, w, h, 0, res] {
            put(&mut s, v);
        }
        self.cmd(&s, &[], 0, 24) == RESP_OK_NODATA
    }

    /// 資源 res の (x, y, w, h) を画面に (RESOURCE_FLUSH。いま映しているものでなければホストは何もしない)
    pub fn flush_res(&mut self, res: u32, x: u32, y: u32, w: u32, h: u32) {
        let mut f = hdr(RESOURCE_FLUSH).to_vec();
        for v in [x, y, w, h, res, 0] {
            put(&mut f, v);
        }
        self.cmd(&f, &[], 0, 24);
    }

    /// ゲストのページからホストの 2D の資源へ写す (TRANSFER_TO_HOST_2D: off はページの中のはじまり)
    pub fn transfer_2d(&mut self, res: u32, x: u32, y: u32, w: u32, h: u32, off: u64) {
        let mut t = hdr(TRANSFER_TO_HOST_2D).to_vec();
        for v in [x, y, w, h] {
            put(&mut t, v);
        }
        t.extend_from_slice(&off.to_le_bytes());
        put(&mut t, res);
        put(&mut t, 0);
        self.cmd(&t, &[], 0, 24);
    }


    /// カーソル (virtio_gpu_cursor_plane_update の cursor ping): update なら絵を資源 res (0 で消す) と hotspot にかえて
    /// (x, y) へ、でなければ動かすだけ。答えは待たない (Linux も cursorq の答えは見ない) が、デバイスが受けとるまで回る
    pub fn cursor(&mut self, update: bool, res: u32, x: i32, y: i32, hot: (u32, u32)) {
        let Some(cq) = self.cq.as_mut() else { return };
        let mut c = hdr(if update { UPDATE_CURSOR } else { MOVE_CURSOR }).to_vec();
        for v in [0, x as u32, y as u32, 0, res, hot.0, hot.1, 0] {
            put(&mut c, v);
        }
        let _s = SEND.lock();
        unsafe { core::ptr::copy_nonoverlapping(c.as_ptr(), self.creq, c.len()) };
        *cq.desc(0) = virtio::Desc { addr: v2p(self.creq as usize) as u64, len: c.len() as u32, flags: 0, next: 0 };
        cq.push(0);
        cq.notify(&self.mmio);
        let t0 = crate::timer::uptime_ns();
        while cq.pop_used().is_none() {
            if crate::timer::uptime_ns() - t0 > 1_000_000_000 {
                return;
            }
            core::hint::spin_loop();
        }
        self.mmio.ack();
    }

    pub fn size(&self) -> usize {
        self.width as usize * self.height as usize * 4
    }
}

/// 見つかったら画面を用意して true
pub fn init() -> bool {
    if !probe_init() {
        return false;
    }
    // /dev/fb0 (Linux の devtmpfs: 装置を見つけたものがノードを置く) と、それを開くもの (Linux の register_chrdev)
    crate::fs::add_dev("fb0", 0o666, false, 29, 0);
    crate::file::register_chrdev(29, |mi, _| (mi == 0 && get().is_some()).then(|| Ok(crate::file::Kind::Ops(Rc::new(FbFile)))));
    // 3D があれば /dev/dri (drm.rs。Linux はいつも置くが、aios は 2D だけのときは置かない: Mesa は llvmpipe のまま)
    if let Some(g) = get().filter(|g| g.virgl) {
        crate::drm::init(g.pa);
    }
    true
}

fn probe_init() -> bool {
    let Some((mmio, feats)) = virtio::probe(DEVICE_GPU, F_VIRGL | F_EDID) else { return false };
    let Some(q) = Queue::new(&mmio, 0, Queue::max(&mmio, 0).min(QUEUE_MAX)) else { return false };
    let cq = Queue::new(&mmio, 1, Queue::max(&mmio, 1).min(16));
    virtio::ready(&mmio);
    let (Some(req), Some(resp), Some(creq)) = (kalloc::alloc(), kalloc::alloc(), kalloc::alloc()) else { return false };
    let pa = v2p(mmio.base);
    let mut g = Gpu { mmio, q, cq, creq, width: 0, height: 0, pages: Rc::new(Vec::new()), req, resp, virgl: feats & F_VIRGL != 0, capsets: Vec::new(), pa, edid: Vec::new(), info: (0, 0) };

    // 3D の capset (virtio_gpu_config の num_capsets は 12 バイト目): Linux の virtio_gpu_get_capsets
    if g.virgl {
        for i in 0..g.mmio.config32(12) {
            let mut q = hdr(GET_CAPSET_INFO).to_vec();
            put(&mut q, i);
            put(&mut q, 0);
            let mut r = [0u8; 40];
            if g.call(&q, &[], 0, 40, &mut r) == RESP_OK_CAPSET_INFO {
                let u = |o: usize| u32::from_le_bytes(r[o..o + 4].try_into().unwrap());
                g.capsets.push((u(24), u(28), u(32)));
            }
        }
    }

    // 画面の大きさ (なければ 1280x800)
    let (mut w, mut h) = (1280, 800);
    let mut r = [0u8; 48];
    if g.call(&hdr(GET_DISPLAY_INFO), &[], 0, 24 + 16 * 24, &mut r) == RESP_OK_DISPLAY_INFO {
        let u = |o: usize| u32::from_le_bytes(r[o..o + 4].try_into().unwrap());
        if u(24 + 16) != 0 && u(24 + 8) > 0 && u(24 + 12) > 0 {
            (w, h) = (u(24 + 8), u(24 + 12));
            g.info = (w, h);
        }
    }
    // 画面 0 の EDID (virtio_gpu_get_edid: 答えは大きさ u32、padding、EDID 1024 バイト)
    if feats & F_EDID != 0 {
        let mut q = hdr(GET_EDID).to_vec();
        put(&mut q, 0);
        put(&mut q, 0);
        let mut r = alloc::vec![0u8; 24 + 8 + 1024];
        if g.call(&q, &[], 0, r.len(), &mut r) == RESP_OK_EDID {
            let n = (u32::from_le_bytes(r[24..28].try_into().unwrap()) as usize).min(1024);
            // はじめのブロックと、その 126 バイト目の数の拡張まで (Linux の drm_edid_read_custom はブロックごとに読む)
            let n = if n >= 128 { n.min((1 + r[32 + 126] as usize) * 128) } else { n };
            g.edid = r[32..32 + n].to_vec();
        }
    }
    g.width = w;
    g.height = h;

    // フレームバッファのページ
    let n = g.size().div_ceil(PGSIZE);
    let mut pages = Vec::with_capacity(n);
    for _ in 0..n {
        let Some(p) = kalloc::alloc() else {
            println!("virtio-gpu: no memory for a {}x{} screen", w, h);
            return false;
        };
        unsafe { core::ptr::write_bytes(p, 0, PGSIZE) };
        pages.push(p);
    }
    g.pages = Rc::new(pages);

    let mut c = hdr(RESOURCE_CREATE_2D).to_vec();
    for v in [RESOURCE, FORMAT_XRGB, w, h] {
        put(&mut c, v);
    }
    if g.cmd(&c, &[], 0, 24) != RESP_OK_NODATA {
        println!("virtio-gpu: cannot create the screen");
        return false;
    }

    // ページの並び (ページ 1 枚に 256 項目ずつ)
    let mut lists = Vec::new();
    for chunk in g.pages.clone().chunks(ENTRIES_PER_PAGE) {
        let Some(l) = kalloc::alloc() else { return false };
        let b = unsafe { core::slice::from_raw_parts_mut(l, PGSIZE) };
        for (k, &p) in chunk.iter().enumerate() {
            b[k * 16..k * 16 + 8].copy_from_slice(&(v2p(p as usize) as u64).to_le_bytes());
            b[k * 16 + 8..k * 16 + 12].copy_from_slice(&(PGSIZE as u32).to_le_bytes());
            b[k * 16 + 12..k * 16 + 16].fill(0);
        }
        lists.push(l);
    }
    let mut a = hdr(RESOURCE_ATTACH_BACKING).to_vec();
    put(&mut a, RESOURCE);
    put(&mut a, n as u32);
    let ok = g.cmd(&a, &lists, n * 16, 24) == RESP_OK_NODATA;
    for l in lists {
        kalloc::free(l);
    }
    if !ok {
        println!("virtio-gpu: cannot attach the screen memory");
        return false;
    }

    let mut s = hdr(SET_SCANOUT).to_vec();
    for v in [0, 0, w, h, 0, RESOURCE] {
        put(&mut s, v);
    }
    if g.cmd(&s, &[], 0, 24) != RESP_OK_NODATA {
        println!("virtio-gpu: cannot show the screen");
        return false;
    }
    g.flush_all();
    if g.virgl {
        println!("virtio-gpu: {}x{} (/dev/fb0), 3D: virgl, capsets {:?}", w, h, g.capsets);
    } else {
        println!("virtio-gpu: {}x{} (/dev/fb0)", w, h);
    }
    *GPU.get() = Some(g);
    true
}

/// フレームバッファの off から読む / 書く (書いたら、その行を画面へ)
pub fn read(off: usize, dst: &mut [u8]) -> usize {
    let Some(g) = get() else { return 0 };
    let n = dst.len().min(g.size().saturating_sub(off));
    copy(g, off, n, |pg, k, i| unsafe { core::ptr::copy_nonoverlapping(pg, dst[i..].as_mut_ptr(), k) });
    n
}

pub fn write(off: usize, src: &[u8]) -> usize {
    let Some(g) = get() else { return 0 };
    let n = src.len().min(g.size().saturating_sub(off));
    if n == 0 {
        return 0;
    }
    copy(g, off, n, |pg, k, i| unsafe { core::ptr::copy_nonoverlapping(src[i..].as_ptr(), pg, k) });
    let line = g.width as usize * 4;
    let (y0, y1) = (off / line, (off + n - 1) / line + 1);
    let w = g.width;
    g.flush(0, y0 as u32, w, (y1 - y0) as u32);
    n
}

/// フレームバッファの [off, off + n) をページごとに f(ページの中の場所, 長さ, 何バイト目から)
fn copy(g: &Gpu, off: usize, n: usize, mut f: impl FnMut(*mut u8, usize, usize)) {
    let mut i = 0;
    while i < n {
        let pos = off + i;
        let k = (PGSIZE - pos % PGSIZE).min(n - i);
        f(unsafe { g.pages[pos / PGSIZE].add(pos % PGSIZE) }, k, i);
        i += k;
    }
}

/// fbdev の ioctl (Linux と同じ番号と形)
pub fn ioctl(req: u64, arg: usize) -> Result<i64, i64> {
    const FBIOGET_VSCREENINFO: u64 = 0x4600;
    const FBIOPUT_VSCREENINFO: u64 = 0x4601;
    const FBIOGET_FSCREENINFO: u64 = 0x4602;
    const FBIOPAN_DISPLAY: u64 = 0x4606;
    const FBIOBLANK: u64 = 0x4611;
    const FBIO_WAITFORVSYNC: u64 = 0x4004_4620;
    // _IOW('F', 0xa0, u32 x 4): 変わった四角 (x, y, w, h) だけを画面へ
    const FBIO_DAMAGE: u64 = 0x4010_46a0;
    const ENOTTY: i64 = 25;
    const EFAULT: i64 = 14;
    let g = get().ok_or(-19)?;
    let out = |b: &[u8]| crate::proc::current().pt().copy_out(arg, b).ok_or(-EFAULT);
    match req {
        FBIOGET_VSCREENINFO | FBIOPUT_VSCREENINFO => {
            // fb_var_screeninfo (u32 が 40 個)。大きさは変えられないので、PUT も今の値を返す
            let mut v = [0u32; 40];
            v[0] = g.width;
            v[1] = g.height;
            v[2] = g.width;
            v[3] = g.height;
            v[6] = 32;
            // 赤 16..24、緑 8..16、青 0..8 (XRGB8888)
            v[8] = 16;
            v[9] = 8;
            v[11] = 8;
            v[12] = 8;
            v[15] = 8;
            v[22] = u32::MAX; // height (mm): わからない
            v[23] = u32::MAX; // width (mm)
            let b: Vec<u8> = v.iter().flat_map(|x| x.to_le_bytes()).collect();
            out(&b)?;
            Ok(0)
        }
        FBIOGET_FSCREENINFO => {
            // fb_fix_screeninfo (80 バイト)
            let mut b = [0u8; 80];
            b[..15].copy_from_slice(b"virtio_gpu_fb\0\0");
            b[24..28].copy_from_slice(&(g.size() as u32).to_le_bytes()); // smem_len
            b[36..40].copy_from_slice(&2u32.to_le_bytes()); // visual = TRUECOLOR
            b[48..52].copy_from_slice(&(g.width * 4).to_le_bytes()); // line_length
            out(&b)?;
            Ok(0)
        }
        FBIOPAN_DISPLAY => {
            // 描き終わった: 画面へ
            unlocked(|| g.flush_all());
            Ok(0)
        }
        FBIO_DAMAGE => {
            let mut b = [0u8; 16];
            crate::proc::current().pt().copy_in(&mut b, arg).ok_or(-EFAULT)?;
            let v = |i: usize| u32::from_le_bytes(b[i * 4..i * 4 + 4].try_into().unwrap());
            let (x, y) = (v(0).min(g.width), v(1).min(g.height));
            let (w, h) = (v(2).min(g.width - x), v(3).min(g.height - y));
            unlocked(|| g.flush(x, y, w, h));
            Ok(0)
        }
        FBIOBLANK | FBIO_WAITFORVSYNC => Ok(0),
        _ => Err(-ENOTTY),
    }
}

/// 開いた画面 (/dev/fb0: file::Kind::Ops。Linux の fb_fops)
pub struct FbFile;

impl crate::file::FileOps for FbFile {
    fn describe(&self) -> alloc::string::String {
        "/dev/fb0".into()
    }
    fn class(&self) -> usize {
        7
    }
    fn opened(&self) -> bool {
        true
    }
    fn seekable(&self) -> bool {
        true
    }
    fn lockable(&self) -> bool {
        true
    }
    fn read_at(&self, pos: usize, dst: &mut [u8]) -> Result<usize, i64> {
        Ok(read(pos, dst))
    }
    /// 画面の終わりは ENOSPC
    fn write_at(&self, pos: usize, src: &[u8]) -> Result<usize, i64> {
        let n = write(pos, src);
        if n == 0 && !src.is_empty() { Err(-28) } else { Ok(n) }
    }
    fn size(&self) -> Option<i64> {
        Some(get().map_or(0, |g| g.size()) as i64)
    }
    fn stat(&self) -> crate::file::Stat {
        crate::file::Stat::dev(crate::vfs::S_IFCHR | 0o666, 29 << 8)
    }
    fn mount(&self) -> (u64, bool) {
        crate::file::dev_mount(&self.describe())
    }
    fn poll_keys(&self) -> Option<Vec<usize>> {
        Some(alloc::vec![])
    }
    fn ioctl(&self, req: u64, arg: usize, _nonblock: bool) -> Option<Result<i64, i64>> {
        Some(ioctl(req, arg))
    }
    /// フレームバッファのページをそのまま
    fn mmap(&self, _fr: &crate::file::FileRef, off: usize, len: usize, _prot: u64) -> Option<Result<crate::vm::Backing, i64>> {
        Some((|| {
            let g = get().ok_or(-19)?;
            if off + len > crate::vm::pg_up(g.size()) {
                return Err(crate::why::inval());
            }
            Ok(crate::vm::Backing::Pages { pages: g.pages.clone(), off, hold: None })
        })())
    }
}

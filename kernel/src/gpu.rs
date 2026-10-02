// virtio-gpu (2D): 画面をひとつ用意して、/dev/fb0 (Linux の fbdev と同じ形) にする
//
// 絵のメモリ (フレームバッファ) はカーネルのページの集まり。デバイスに「資源 1 番」として渡し
// (RESOURCE_CREATE_2D + ATTACH_BACKING)、画面 0 に映す (SET_SCANOUT)。
// ユーザーは mmap で直に描くか write で書き、描き終わったら FBIOPAN_DISPLAY (か write、msync) で
// 知らせる。そのときにデバイスへ写して (TRANSFER_TO_HOST_2D) 画面を描きなおす (RESOURCE_FLUSH)。
// 形は XRGB8888 (メモリの並びは B, G, R, X)。要求は virtio-blk と同じく、出して終わるまで待つ
use crate::kalloc;
use crate::memlayout::{v2p, PGSIZE};
use crate::virtio::{self, Mmio, Queue, DESC_NEXT, DESC_WRITE};
use alloc::rc::Rc;
use alloc::vec::Vec;

const DEVICE_GPU: u32 = 16;
const GET_DISPLAY_INFO: u32 = 0x0100;
const RESOURCE_CREATE_2D: u32 = 0x0101;
const SET_SCANOUT: u32 = 0x0103;
const RESOURCE_FLUSH: u32 = 0x0104;
const TRANSFER_TO_HOST_2D: u32 = 0x0105;
const RESOURCE_ATTACH_BACKING: u32 = 0x0106;
const RESP_OK_NODATA: u32 = 0x1100;
const RESP_OK_DISPLAY_INFO: u32 = 0x1101;
/// B8G8R8X8 (XRGB8888)
const FORMAT_XRGB: u32 = 2;
const RESOURCE: u32 = 1;
/// 1 ページに入る ATTACH_BACKING の項目 (addr u64, len u32, pad u32)
const ENTRIES_PER_PAGE: usize = PGSIZE / 16;

pub struct Gpu {
    mmio: Mmio,
    q: Queue,
    pub width: u32,
    pub height: u32,
    /// フレームバッファのページ (カーネルの仮想アドレス)。並びの順に画面の上から
    pub pages: Rc<Vec<*mut u8>>,
    /// 要求と応答に使うページ
    req: *mut u8,
    resp: *mut u8,
}

static mut GPU: Option<Gpu> = None;

pub fn get() -> Option<&'static mut Gpu> {
    unsafe { (*(&raw mut GPU)).as_mut() }
}

fn hdr(typ: u32) -> [u8; 24] {
    let mut h = [0u8; 24];
    h[0..4].copy_from_slice(&typ.to_le_bytes());
    h
}

fn put(b: &mut Vec<u8>, v: u32) {
    b.extend_from_slice(&v.to_le_bytes());
}

impl Gpu {
    /// 要求 (req) と、ページの並び (extra、デバイスが読む) を送り、応答の型を返す
    fn cmd(&mut self, req: &[u8], extra: &[*mut u8], extra_len: usize, resp_len: usize) -> u32 {
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
        while self.q.pop_used().is_none() {
            core::hint::spin_loop();
        }
        self.mmio.ack();
        unsafe { u32::from_le_bytes(core::slice::from_raw_parts(self.resp, 4).try_into().unwrap()) }
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

    pub fn size(&self) -> usize {
        self.width as usize * self.height as usize * 4
    }
}

/// 見つかったら画面を用意して true
pub fn init() -> bool {
    let Some((mmio, _)) = virtio::probe(DEVICE_GPU, 0) else { return false };
    let Some(q) = Queue::new(&mmio, 0, 64) else { return false };
    virtio::ready(&mmio);
    let (Some(req), Some(resp)) = (kalloc::alloc(), kalloc::alloc()) else { return false };
    let mut g = Gpu { mmio, q, width: 0, height: 0, pages: Rc::new(Vec::new()), req, resp };

    // 画面の大きさ (なければ 1280x800)
    let (mut w, mut h) = (1280, 800);
    if g.cmd(&hdr(GET_DISPLAY_INFO), &[], 0, 24 + 16 * 24) == RESP_OK_DISPLAY_INFO {
        let r = unsafe { core::slice::from_raw_parts(g.resp, 48) };
        let u = |o: usize| u32::from_le_bytes(r[o..o + 4].try_into().unwrap());
        if u(24 + 16) != 0 && u(24 + 8) > 0 && u(24 + 12) > 0 {
            (w, h) = (u(24 + 8), u(24 + 12));
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
    println!("virtio-gpu: {}x{} (/dev/fb0)", w, h);
    unsafe { *(&raw mut GPU) = Some(g) };
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
            g.flush_all();
            Ok(0)
        }
        FBIOBLANK | FBIO_WAITFORVSYNC => Ok(0),
        _ => Err(-ENOTTY),
    }
}

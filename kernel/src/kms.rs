// KMS (Linux の drivers/gpu/drm の drm_mode_*、drm_crtc.c、drm_plane.c、drm_framebuffer.c、drm_connector.c、
// drm_property.c と virtio/virtgpu_display.c、virtgpu_plane.c): 画面に出す口 (/dev/dri/card0 の MODE_* の ioctl)。
//
// virtio-gpu の画面 0 を、Linux 6.18 の virtio_gpu と同じ番号のもので見せる: plane 33 (primary: XR24) と 34 (cursor:
// AR24)、crtc 37、connector 38 (Virtual-1)、encoder 39、EDID の blob 40、/dev/fb0 の framebuffer 41 (Linux の fbdev の
// もの)。プロパティの番号も同じ (EDID 1、DPMS 2、TILE 4、link-status 5、non-desktop 6、type 8、VRR_ENABLED 24、
// HOTSPOT_X 35、HOTSPOT_Y 36)。モードは EDID から (edid.rs)。
//   framebuffer   ADDFB / ADDFB2 で BO (GEM のハンドル) から作る。形は XRGB8888 と ARGB8888 (virtio_gpu と同じ)
//   dumb          CREATE_DUMB は 2D の資源 (RESOURCE_CREATE_2D、B8G8R8X8)。映すときと DIRTYFB でホストへ写す
//   SETCRTC       モードと framebuffer を映す (SET_SCANOUT と RESOURCE_FLUSH)
//   PAGE_FLIP     framebuffer をかえて、終わったら FLIP_COMPLETE のできごと (read で読む。vblank はないので数は 0)
// 映す命令もホストが終えるまで待つ (drm.rs と同じ)。はじめは /dev/fb0 を映していて (fb 41)、card0 の最後のファイルを
// 閉じると /dev/fb0 にもどす (Linux の fbdev の restore)。master は card0 をはじめに開いたファイル。
// まだ: atomic (DRM_CLIENT_CAP_ATOMIC は EOPNOTSUPP: atomic のないドライバの答え)、カーソル (CURSOR は ENXIO、cursor
// plane の SETPLANE は EINVAL)、vblank (WAIT_VBLANK は EOPNOTSUPP: virtio_gpu と同じ)、lease、画面の大きさがかわったときの
// できごと (hotplug)
use crate::drm::{Bo, DrmFile};
use crate::edid::{self, Mode};
use crate::cell::Bkl;
use alloc::collections::BTreeMap;
use alloc::rc::Rc;
use alloc::vec;
use alloc::vec::Vec;

const ENOENT: i64 = 2;
const ENXIO: i64 = 6;
const ENOMEM: i64 = 12;
const EFAULT: i64 = 14;
const EBUSY: i64 = 16;
const EINVAL: i64 = 22;
const ENOSPC: i64 = 28;
const ERANGE: i64 = 34;
const ENOSYS: i64 = 38;
const EOPNOTSUPP: i64 = 95;
const EPERM: i64 = 1;

pub const PLANE_PRIMARY: u32 = 33;
pub const PLANE_CURSOR: u32 = 34;
pub const CRTC: u32 = 37;
pub const CONNECTOR: u32 = 38;
pub const ENCODER: u32 = 39;
const EDID_BLOB: u32 = 40;
/// /dev/fb0 の framebuffer (Linux の fbdev のもの)
const FBDEV: u32 = 41;
/// ユーザーの framebuffer と blob の番号の始まり (いちばん小さい空いた番号: Linux の idr_alloc)
const FIRST_ID: u32 = 42;

const OBJ_CRTC: u32 = 0xcccc_cccc;
const OBJ_CONNECTOR: u32 = 0xc0c0_c0c0;
const OBJ_ENCODER: u32 = 0xe0e0_e0e0;
const OBJ_PROPERTY: u32 = 0xb0b0_b0b0;
const OBJ_FB: u32 = 0xfbfb_fbfb;
const OBJ_BLOB: u32 = 0xbbbb_bbbb;
const OBJ_PLANE: u32 = 0xeeee_eeee;

const fn fourcc(s: &[u8; 4]) -> u32 {
    s[0] as u32 | (s[1] as u32) << 8 | (s[2] as u32) << 16 | (s[3] as u32) << 24
}
const XRGB8888: u32 = fourcc(b"XR24");
const ARGB8888: u32 = fourcc(b"AR24");

// プロパティの形 (DRM_MODE_PROP_*)
const P_RANGE: u32 = 1 << 1;
const P_IMMUTABLE: u32 = 1 << 2;
const P_ENUM: u32 = 1 << 3;
const P_BLOB: u32 = 1 << 4;
const P_SIGNED_RANGE: u32 = 2 << 6;

struct Prop {
    id: u32,
    name: &'static str,
    flags: u32,
    values: &'static [u64],
    enums: &'static [(u64, &'static str)],
}

const I32_MIN: u64 = i32::MIN as i64 as u64;
const PROPS: [Prop; 9] = [
    Prop { id: 1, name: "EDID", flags: P_BLOB | P_IMMUTABLE, values: &[], enums: &[] },
    Prop { id: 2, name: "DPMS", flags: P_ENUM, values: &[0, 1, 2, 3], enums: &[(0, "On"), (1, "Standby"), (2, "Suspend"), (3, "Off")] },
    Prop { id: 4, name: "TILE", flags: P_BLOB | P_IMMUTABLE, values: &[], enums: &[] },
    Prop { id: 5, name: "link-status", flags: P_ENUM, values: &[0, 1], enums: &[(0, "Good"), (1, "Bad")] },
    Prop { id: 6, name: "non-desktop", flags: P_RANGE | P_IMMUTABLE, values: &[0, 1], enums: &[] },
    Prop { id: 8, name: "type", flags: P_ENUM | P_IMMUTABLE, values: &[0, 1, 2], enums: &[(0, "Overlay"), (1, "Primary"), (2, "Cursor")] },
    Prop { id: 24, name: "VRR_ENABLED", flags: P_RANGE, values: &[0, 1], enums: &[] },
    Prop { id: 35, name: "HOTSPOT_X", flags: P_SIGNED_RANGE, values: &[I32_MIN, i32::MAX as u64], enums: &[] },
    Prop { id: 36, name: "HOTSPOT_Y", flags: P_SIGNED_RANGE, values: &[I32_MIN, i32::MAX as u64], enums: &[] },
];

/// framebuffer (Linux の drm_framebuffer)
struct Fb {
    /// 作ったファイル (0 はカーネルのもの、または CLOSEFB で手放したもの)
    owner: u32,
    /// 作った順 (GETRESOURCES は新しいものから)
    seq: u64,
    w: u32,
    h: u32,
    format: u32,
    pitch: u32,
    offset: u32,
    /// BO (なければ /dev/fb0)
    bo: Option<Rc<Bo>>,
}

struct Kms {
    modes: Vec<Mode>,
    edid: Vec<u8>,
    mm: (u32, u32),
    connected: bool,
    /// CRTC: モード (None はとまっている)、映している framebuffer (0 はなし)、framebuffer の中の左上
    mode: Option<Mode>,
    fb: u32,
    x: u32,
    y: u32,
    dpms: u64,
    link: u64,
    vrr: u64,
    hotspot: (u64, u64),
    fbs: BTreeMap<u32, Fb>,
    /// ユーザーの blob (CREATEPROPBLOB: 作ったファイル、中身)
    blobs: BTreeMap<u32, (u32, Vec<u8>)>,
    seq: u64,
}

static KMS: Bkl<Option<Kms>> = Bkl::new(None);

fn kms() -> Result<&'static mut Kms, i64> {
    KMS.get().as_mut().ok_or(-EOPNOTSUPP)
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

fn copy_out(va: u64, b: &[u8]) -> Result<(), i64> {
    if b.is_empty() {
        return Ok(());
    }
    crate::proc::current().pt().copy_out(va as usize, b).ok_or(-EFAULT)
}

fn copy_in(va: u64, b: &mut [u8]) -> Result<(), i64> {
    if b.is_empty() {
        return Ok(());
    }
    crate::proc::current().pt().copy_in(b, va as usize).ok_or(-EFAULT)
}

/// 番号の並びを、置き場に入るだけ (count は置き場の大きさ。Linux の put_user をくり返すもの)
fn put_ids(va: u64, count: u32, ids: &[u32]) -> Result<(), i64> {
    let n = ids.len().min(count as usize);
    let b: Vec<u8> = ids[..n].iter().flat_map(|x| x.to_le_bytes()).collect();
    copy_out(va, &b)
}

/// 形 (fourcc) の (面の数, 面ごとのバイト, 横と縦の間引き, 深さ)。知らないものは None (drm_fourcc.c の一部)
fn format_info(f: u32) -> Option<(usize, [u32; 3], u32, u32, u32)> {
    let one = |cpp: u32, depth: u32| Some((1, [cpp, 0, 0], 1, 1, depth));
    match &f.to_le_bytes() {
        b"C8  " => one(1, 8),
        b"R8  " => one(1, 0),
        b"RG88" | b"GR88" => one(2, 0),
        b"RG16" | b"BG16" => one(2, 16),
        b"XR15" | b"XB15" => one(2, 15),
        b"AR15" | b"AB15" => one(2, 0),
        b"RG24" | b"BG24" => one(3, 24),
        b"XR24" | b"XB24" | b"RX24" | b"BX24" => one(4, 24),
        b"AR24" | b"AB24" | b"RA24" | b"BA24" => one(4, 32),
        b"XR30" | b"XB30" | b"AR30" | b"AB30" => one(4, 30),
        b"YUYV" | b"YVYU" | b"UYVY" | b"VYUY" => Some((1, [2, 0, 0], 2, 1, 0)),
        b"NV12" | b"NV21" => Some((2, [1, 2, 0], 2, 2, 0)),
        b"YU12" | b"YV12" => Some((3, [1, 1, 1], 2, 2, 0)),
        _ => None,
    }
}

/// /dev/dri を置くとき (drm::init): EDID と画面の大きさからモード、はじめは /dev/fb0 を映している
pub fn init(g: &crate::gpu::Gpu) {
    let modes = edid::modes(&g.edid, g.info.0, g.info.1);
    let mut fbs = BTreeMap::new();
    fbs.insert(FBDEV, Fb { owner: 0, seq: 0, w: g.width, h: g.height, format: XRGB8888, pitch: g.width * 4, offset: 0, bo: None });
    let mode = modes.first().copied().filter(|m| m.hd as u32 <= g.width && m.vd as u32 <= g.height);
    *KMS.get() = Some(Kms {
        modes,
        edid: g.edid.clone(),
        mm: edid::size_mm(&g.edid),
        connected: true,
        mode,
        fb: if mode.is_some() { FBDEV } else { 0 },
        x: 0,
        y: 0,
        dpms: 0,
        link: 0,
        vrr: 0,
        hotspot: (0, 0),
        fbs,
        blobs: BTreeMap::new(),
        seq: 0,
    });
}

/// ioctl の (カーネルの形の大きさ, 向き: bit 0 書きこみ、bit 1 読みだし, master だけか)。KMS でない番号は None
pub fn spec(nr: u32) -> Option<(usize, u8, bool)> {
    const W: u8 = 1;
    const RW: u8 = 3;
    Some(match nr {
        0xa0 => (64, RW, false),  // GETRESOURCES
        0xa1 => (104, RW, false), // GETCRTC
        0xa2 => (104, RW, true),  // SETCRTC
        0xa3 => (28, RW, true),   // CURSOR
        0xa4 => (32, RW, false),  // GETGAMMA
        0xa5 => (32, RW, true),   // SETGAMMA
        0xa6 => (20, RW, false),  // GETENCODER
        0xa7 => (80, RW, false),  // GETCONNECTOR
        0xa8 | 0xa9 => (72, RW, true), // ATTACHMODE / DETACHMODE (昔のもの: 何もしない)
        0xaa => (64, RW, false),  // GETPROPERTY
        0xab => (16, RW, true),   // SETPROPERTY
        0xac => (16, RW, false),  // GETPROPBLOB
        0xad => (28, RW, false),  // GETFB
        0xae => (28, RW, false),  // ADDFB
        0xaf => (4, RW, false),   // RMFB
        0xb0 => (24, RW, true),   // PAGE_FLIP
        0xb1 => (24, RW, true),   // DIRTYFB
        0xb2 => (32, RW, false),  // CREATE_DUMB
        0xb3 => (16, RW, false),  // MAP_DUMB
        0xb4 => (4, RW, false),   // DESTROY_DUMB
        0xb5 => (16, RW, false),  // GETPLANERESOURCES
        0xb6 => (32, RW, false),  // GETPLANE
        0xb7 => (48, RW, true),   // SETPLANE
        0xb8 => (104, RW, false), // ADDFB2
        0xb9 => (32, RW, false),  // OBJ_GETPROPERTIES
        0xba => (24, RW, true),   // OBJ_SETPROPERTY
        0xbb => (36, RW, true),   // CURSOR2
        0xbc => (56, RW, true),   // ATOMIC
        0xbd => (16, RW, false),  // CREATEPROPBLOB
        0xbe => (4, RW, false),   // DESTROYPROPBLOB
        0xce => (104, RW, false), // GETFB2
        0xd0 => (8, W, false),    // CLOSEFB
        _ => return None,
    })
}

/// KMS の ioctl (drm.rs の do_ioctl が、番号、口、master をたしかめてから)
pub fn ioctl(f: &DrmFile, nr: u32, a: &mut [u8]) -> Result<(), i64> {
    let k = kms()?;
    match nr {
        0xa0 => getresources(k, f, a),
        0xa1 => getcrtc(k, f, a),
        0xa2 => setcrtc(k, a),
        // カーソルはまだ (Linux の、カーソルのない CRTC の答え)
        0xa3 | 0xbb => {
            if le32(a, 4) != CRTC {
                return Err(-ENOENT);
            }
            Err(-ENXIO)
        }
        0xa4 | 0xa5 => {
            if le32(a, 0) != CRTC {
                return Err(-ENOENT);
            }
            // gamma の表はない (gamma_size 0)。SETGAMMA は gamma_set のない CRTC の答え
            if nr == 0xa5 {
                return Err(-ENOSYS);
            }
            if le32(a, 4) != 0 { Err(-EINVAL) } else { Ok(()) }
        }
        0xa6 => getencoder(k, a),
        0xa7 => getconnector(k, f, a),
        0xa8 | 0xa9 => Ok(()),
        0xaa => getproperty(a),
        0xab => {
            let (v, p, o) = (le64(a, 0), le32(a, 8), le32(a, 12));
            setprop(k, o, OBJ_CONNECTOR, p, v)
        }
        0xac => getpropblob(k, a),
        0xad => getfb(k, f, a),
        0xae => addfb(k, f, a),
        0xaf => rmfb(k, f, le32(a, 0), true),
        0xb0 => page_flip(k, f, a),
        0xb1 => dirtyfb(k, a),
        0xb2 => create_dumb(f, a),
        0xb3 => {
            set64(a, 8, f.map_offset(le32(a, 0))?);
            Ok(())
        }
        0xb4 => f.gem_close(le32(a, 0)),
        0xb5 => {
            let ids: &[u32] = if f.universal_planes() { &[PLANE_PRIMARY, PLANE_CURSOR] } else { &[] };
            put_ids(le64(a, 0), le32(a, 8), ids)?;
            set32(a, 8, ids.len() as u32);
            Ok(())
        }
        0xb6 => getplane(k, a),
        0xb7 => setplane(k, a),
        0xb8 => addfb2(k, f, a),
        0xb9 => {
            let (id, typ) = (le32(a, 20), le32(a, 24));
            let props = obj_props(k, id, typ)?;
            put_props(a, 0, 8, 16, &props)
        }
        0xba => {
            let (v, p, o, t) = (le64(a, 0), le32(a, 8), le32(a, 12), le32(a, 16));
            setprop(k, o, t, p, v)
        }
        // atomic のないドライバ
        0xbc => Err(-EOPNOTSUPP),
        0xbd => {
            let len = le32(a, 8) as usize;
            if len == 0 || len > i32::MAX as usize - 64 {
                return Err(-EINVAL);
            }
            let mut b = vec![0u8; len];
            copy_in(le64(a, 0), &mut b)?;
            let id = new_id(k);
            k.blobs.insert(id, (f.id(), b));
            set32(a, 12, id);
            Ok(())
        }
        0xbe => {
            let id = le32(a, 0);
            match k.blobs.get(&id) {
                None if id == EDID_BLOB && !k.edid.is_empty() => Err(-EPERM),
                None => Err(-ENOENT),
                Some((o, _)) if *o != f.id() => Err(-EPERM),
                Some(_) => {
                    k.blobs.remove(&id);
                    Ok(())
                }
            }
        }
        0xce => getfb2(k, f, a),
        0xd0 => {
            if le32(a, 4) != 0 {
                return Err(-EINVAL);
            }
            rmfb(k, f, le32(a, 0), false)
        }
        _ => Err(-EINVAL),
    }
}

/// いちばん小さい空いた番号 (framebuffer と blob で同じ数え方)
fn new_id(k: &Kms) -> u32 {
    let mut id = FIRST_ID;
    while k.fbs.contains_key(&id) || k.blobs.contains_key(&id) {
        id += 1;
    }
    id
}

/// CRTC が動いているか (モードがある。framebuffer がなくても: RMFB で primary だけ外れたとき)
fn enabled(k: &Kms) -> bool {
    k.mode.is_some()
}

/// primary plane が CRTC につながっているか
fn active(k: &Kms) -> bool {
    k.mode.is_some() && k.fb != 0
}

fn getresources(k: &Kms, f: &DrmFile, a: &mut [u8]) -> Result<(), i64> {
    // 自分の framebuffer (新しいものから)
    let mut mine: Vec<(u64, u32)> = k.fbs.iter().filter(|(_, fb)| fb.owner == f.id() && fb.owner != 0).map(|(&id, fb)| (fb.seq, id)).collect();
    mine.sort_by(|a, b| b.0.cmp(&a.0));
    let ids: Vec<u32> = mine.into_iter().map(|x| x.1).collect();
    put_ids(le64(a, 0), le32(a, 32), &ids)?;
    set32(a, 32, ids.len() as u32);
    put_ids(le64(a, 8), le32(a, 36), &[CRTC])?;
    set32(a, 36, 1);
    put_ids(le64(a, 16), le32(a, 40), &[CONNECTOR])?;
    set32(a, 40, 1);
    put_ids(le64(a, 24), le32(a, 44), &[ENCODER])?;
    set32(a, 44, 1);
    // min_width、max_width、min_height、max_height (virtio_gpu の XRES_MIN ... YRES_MAX)
    set32(a, 48, 32);
    set32(a, 52, edid::MAX);
    set32(a, 56, 32);
    set32(a, 60, edid::MAX);
    Ok(())
}

fn getcrtc(k: &Kms, f: &DrmFile, a: &mut [u8]) -> Result<(), i64> {
    if le32(a, 12) != CRTC {
        return Err(-ENOENT);
    }
    set32(a, 28, 0);
    set32(a, 16, k.fb);
    set32(a, 20, if k.fb != 0 { k.x } else { 0 });
    set32(a, 24, if k.fb != 0 { k.y } else { 0 });
    match k.mode {
        Some(m) => {
            set32(a, 32, 1);
            a[36..104].copy_from_slice(&m.to_user(f.aspect_ratio()));
        }
        None => set32(a, 32, 0),
    }
    Ok(())
}

/// 映しているものを画面へ (virtio_gpu_primary_plane_update)。dirty は framebuffer の中の変わった四角 (なければ全部)
fn present(k: &Kms, dirty: Option<(u32, u32, u32, u32)>) {
    let Some(g) = crate::gpu::get() else { return };
    let (Some(m), Some(fb)) = (k.mode, k.fbs.get(&k.fb)) else {
        g.scanout(0, 0, 0, 0, 0);
        return;
    };
    if k.dpms != 0 {
        g.scanout(0, 0, 0, 0, 0);
        return;
    }
    let (w, h) = (m.hd as u32, m.vd as u32);
    let (x, y, dw, dh) = dirty.unwrap_or((k.x, k.y, w, h));
    match &fb.bo {
        // /dev/fb0 (資源 1): ページはもうホストへ写してある
        None => {
            g.scanout(1, k.x, k.y, w, h);
            g.flush_all();
        }
        Some(bo) => {
            if bo.dumb {
                // dumb (2D の資源) は、ゲストのページからホストへ写す
                let off = fb.offset as u64 + y as u64 * fb.pitch as u64 + x as u64 * 4;
                g.transfer_2d(bo.res, x, y, dw, dh, off);
            }
            g.scanout(bo.res, k.x, k.y, w, h);
            g.flush_res(bo.res, x, y, dw, dh);
        }
    }
}

/// 映すものの大きさが framebuffer に入るか (drm_crtc_check_viewport)
fn viewport_ok(m: &Mode, x: u32, y: u32, fb: &Fb) -> bool {
    let (w, h) = (m.hd as u32, m.vd as u32);
    w <= fb.w && h <= fb.h && x <= fb.w - w && y <= fb.h - h
}

/// connector のモードとしてよいか (virtio_gpu_conn_mode_valid: preferred は画面の大きさの 16 以内)
fn mode_ok(m: &Mode) -> bool {
    if m.typ & edid::TYPE_PREFERRED == 0 || (m.hd, m.vd) == (1024, 768) {
        return true;
    }
    let (w, h) = crate::gpu::get().map_or((0, 0), |g| g.info);
    m.hd as u32 <= w && m.hd as u32 + 16 >= w && m.vd as u32 <= h && m.vd as u32 + 16 >= h
}

fn setcrtc(k: &mut Kms, a: &mut [u8]) -> Result<(), i64> {
    let (conns, nconn, crtc, fb_id, x, y, valid) = (le64(a, 0), le32(a, 8), le32(a, 12), le32(a, 16), le32(a, 20), le32(a, 24), le32(a, 32));
    if x & 0xffff_0000 != 0 || y & 0xffff_0000 != 0 {
        return Err(-ERANGE);
    }
    if crtc != CRTC {
        return Err(-ENOENT);
    }
    let mut mode = None;
    let mut fb = 0;
    if valid != 0 {
        fb = if fb_id == u32::MAX {
            if k.fb == 0 {
                return Err(-EINVAL);
            }
            k.fb
        } else {
            if !k.fbs.contains_key(&fb_id) {
                return Err(-ENOENT);
            }
            fb_id
        };
        let m = Mode::from_user(&a[36..104])?;
        let f = &k.fbs[&fb];
        if f.format != XRGB8888 {
            return Err(-EINVAL);
        }
        if !viewport_ok(&m, x, y, f) {
            return Err(-ENOSPC);
        }
        mode = Some(m);
    }
    if nconn == 0 && mode.is_some() {
        return Err(-EINVAL);
    }
    if nconn > 0 && mode.is_none() {
        return Err(-EINVAL);
    }
    if nconn > 1 {
        return Err(-EINVAL);
    }
    if nconn == 1 {
        let mut b = [0u8; 4];
        copy_in(conns, &mut b)?;
        if u32::from_le_bytes(b) != CONNECTOR {
            return Err(-ENOENT);
        }
    }
    if let Some(m) = &mode {
        if !mode_ok(m) {
            return Err(-EINVAL);
        }
    }
    k.mode = mode;
    k.fb = fb;
    k.x = if mode.is_some() { x } else { 0 };
    k.y = if mode.is_some() { y } else { 0 };
    if mode.is_some() {
        k.dpms = 0;
    }
    present(k, None);
    gc(k);
    Ok(())
}

/// 手放した framebuffer (CLOSEFB) で、もう映していないものを落とす
fn gc(k: &mut Kms) {
    let cur = k.fb;
    k.fbs.retain(|&id, fb| id == FBDEV || fb.owner != 0 || id == cur);
}

fn getencoder(k: &Kms, a: &mut [u8]) -> Result<(), i64> {
    if le32(a, 0) != ENCODER {
        return Err(-ENOENT);
    }
    // DRM_MODE_ENCODER_VIRTUAL (5)
    set32(a, 4, 5);
    set32(a, 8, if enabled(k) { CRTC } else { 0 });
    set32(a, 12, 1);
    set32(a, 16, 1);
    Ok(())
}

fn getconnector(k: &Kms, f: &DrmFile, a: &mut [u8]) -> Result<(), i64> {
    if le32(a, 48) != CONNECTOR {
        return Err(-ENOENT);
    }
    if le32(a, 40) >= 1 {
        put_ids(le64(a, 0), 1, &[ENCODER])?;
    }
    set32(a, 40, 1);
    // DRM_MODE_CONNECTOR_VIRTUAL (15) の 1 番 (Virtual-1)
    set32(a, 52, 15);
    set32(a, 56, 1);
    set32(a, 64, k.mm.0);
    set32(a, 68, k.mm.1);
    // display_info.subpixel_order (SubPixelUnknown)
    set32(a, 72, 0);
    set32(a, 60, if k.connected { 1 } else { 2 });
    let ar = f.aspect_ratio();
    let list: Vec<Mode> = if ar { k.modes.clone() } else { edid::exposed(&k.modes) };
    let want = le32(a, 32) as usize;
    if want >= list.len() && !list.is_empty() {
        let b: Vec<u8> = list.iter().flat_map(|m| m.to_user(ar)).collect();
        copy_out(le64(a, 8), &b)?;
    }
    set32(a, 32, list.len() as u32);
    set32(a, 44, if enabled(k) { ENCODER } else { 0 });
    let props = obj_props(k, CONNECTOR, OBJ_CONNECTOR)?;
    put_props(a, 16, 24, 36, &props)
}

/// オブジェクトのプロパティ (番号, 値)。ないものは ENOENT、プロパティを持たないもの (encoder、framebuffer、blob ...) は EINVAL
fn obj_props(k: &Kms, id: u32, typ: u32) -> Result<Vec<(u32, u64)>, i64> {
    let t = match id {
        PLANE_PRIMARY | PLANE_CURSOR => OBJ_PLANE,
        CRTC => OBJ_CRTC,
        CONNECTOR => OBJ_CONNECTOR,
        ENCODER => OBJ_ENCODER,
        EDID_BLOB if !k.edid.is_empty() => OBJ_BLOB,
        _ if PROPS.iter().any(|p| p.id == id) => OBJ_PROPERTY,
        _ if k.fbs.contains_key(&id) => OBJ_FB,
        _ if k.blobs.contains_key(&id) => OBJ_BLOB,
        _ => return Err(-ENOENT),
    };
    if typ != 0 && typ != t {
        return Err(-ENOENT);
    }
    Ok(match id {
        PLANE_PRIMARY => vec![(8, 1)],
        PLANE_CURSOR => vec![(8, 2), (35, k.hotspot.0), (36, k.hotspot.1)],
        CRTC => vec![(24, k.vrr)],
        CONNECTOR => vec![(2, k.dpms), (5, k.link), (6, 0), (4, 0), (1, if k.edid.is_empty() { 0 } else { EDID_BLOB as u64 })],
        _ => return Err(-EINVAL),
    })
}

/// プロパティの番号と値を置き場へ (入るだけ)、数を cnt に
fn put_props(a: &mut [u8], ids_at: usize, vals_at: usize, cnt: usize, props: &[(u32, u64)]) -> Result<(), i64> {
    let n = props.len().min(le32(a, cnt) as usize);
    let ids: Vec<u8> = props[..n].iter().flat_map(|p| p.0.to_le_bytes()).collect();
    let vals: Vec<u8> = props[..n].iter().flat_map(|p| p.1.to_le_bytes()).collect();
    copy_out(le64(a, ids_at), &ids)?;
    copy_out(le64(a, vals_at), &vals)?;
    set32(a, cnt, props.len() as u32);
    Ok(())
}

fn getproperty(a: &mut [u8]) -> Result<(), i64> {
    let p = PROPS.iter().find(|p| p.id == le32(a, 16)).ok_or(-ENOENT)?;
    a[24..56].fill(0);
    a[24..24 + p.name.len()].copy_from_slice(p.name.as_bytes());
    set32(a, 20, p.flags);
    let n = p.values.len().min(le32(a, 56) as usize);
    let vals: Vec<u8> = p.values[..n].iter().flat_map(|v| v.to_le_bytes()).collect();
    copy_out(le64(a, 0), &vals)?;
    set32(a, 56, p.values.len() as u32);
    if p.flags & P_ENUM != 0 {
        let n = p.enums.len().min(le32(a, 60) as usize);
        let mut b = Vec::with_capacity(n * 40);
        for (v, name) in &p.enums[..n] {
            let mut e = [0u8; 40];
            e[..8].copy_from_slice(&v.to_le_bytes());
            e[8..8 + name.len()].copy_from_slice(name.as_bytes());
            b.extend_from_slice(&e);
        }
        copy_out(le64(a, 8), &b)?;
        set32(a, 60, p.enums.len() as u32);
    } else if p.flags & P_BLOB != 0 {
        set32(a, 60, 0);
    }
    Ok(())
}

/// プロパティを変える (drm_mode_obj_set_property_ioctl と、atomic のドライバの set_property_atomic)
fn setprop(k: &mut Kms, obj: u32, typ: u32, prop: u32, v: u64) -> Result<(), i64> {
    let props = obj_props(k, obj, typ)?;
    if !props.iter().any(|p| p.0 == prop) {
        return Err(-EINVAL);
    }
    match (obj, prop) {
        // DPMS: On のほかは Off。とまる・映すをかえる
        (CONNECTOR, 2) => {
            let d = if v == 0 { 0 } else { 3 };
            if d != k.dpms {
                k.dpms = d;
                present(k, None);
            }
            Ok(())
        }
        // link-status: Good からはユーザーが下げられない (受けとるだけ)
        (CONNECTOR, 5) => {
            if k.link != 0 {
                k.link = v;
            }
            Ok(())
        }
        (CRTC, 24) => {
            k.vrr = (v != 0) as u64;
            Ok(())
        }
        (PLANE_CURSOR, 35) => {
            k.hotspot.0 = v;
            Ok(())
        }
        (PLANE_CURSOR, 36) => {
            k.hotspot.1 = v;
            Ok(())
        }
        _ => Err(-EINVAL),
    }
}

fn getpropblob(k: &Kms, a: &mut [u8]) -> Result<(), i64> {
    let id = le32(a, 0);
    let data: &[u8] = match k.blobs.get(&id) {
        Some((_, b)) => b,
        None if id == EDID_BLOB && !k.edid.is_empty() => &k.edid,
        None => return Err(-ENOENT),
    };
    if le32(a, 4) as usize == data.len() {
        copy_out(le64(a, 8), data)?;
    }
    set32(a, 4, data.len() as u32);
    Ok(())
}

/// GETFB と GETFB2 のハンドル: master か CAP_SYS_ADMIN のときだけ、このファイルの新しいハンドル (Linux と同じ。ほかは 0)
fn fb_handle(f: &DrmFile, fb: &Fb) -> u32 {
    let Some(bo) = &fb.bo else { return 0 };
    if !f.master() && !crate::cred::current().capable(crate::cred::CAP_SYS_ADMIN) {
        return 0;
    }
    f.new_handle(bo.clone())
}

fn getfb(k: &Kms, f: &DrmFile, a: &mut [u8]) -> Result<(), i64> {
    let fb = k.fbs.get(&le32(a, 0)).ok_or(-ENOENT)?;
    let (planes, cpp, _, _, depth) = format_info(fb.format).ok_or(-EINVAL)?;
    if planes > 1 {
        return Err(-EINVAL);
    }
    set32(a, 4, fb.w);
    set32(a, 8, fb.h);
    set32(a, 12, fb.pitch);
    set32(a, 16, cpp[0] * 8);
    set32(a, 20, depth);
    set32(a, 24, fb_handle(f, fb));
    Ok(())
}

fn getfb2(k: &Kms, f: &DrmFile, a: &mut [u8]) -> Result<(), i64> {
    let fb = k.fbs.get(&le32(a, 0)).ok_or(-ENOENT)?;
    set32(a, 4, fb.w);
    set32(a, 8, fb.h);
    set32(a, 12, fb.format);
    set32(a, 16, 0);
    a[20..104].fill(0);
    set32(a, 20, fb_handle(f, fb));
    set32(a, 36, fb.pitch);
    set32(a, 52, fb.offset);
    Ok(())
}

/// ADDFB (昔の形: bpp と depth から形を決めて ADDFB2 へ。drm_mode_legacy_fb_format)
fn addfb(k: &mut Kms, f: &DrmFile, a: &mut [u8]) -> Result<(), i64> {
    let format = match (le32(a, 16), le32(a, 20)) {
        (8, 8) => fourcc(b"C8  "),
        (16, 15) => fourcc(b"XR15"),
        (16, 16) => fourcc(b"RG16"),
        (24, 24) => fourcc(b"RG24"),
        (32, 24) => XRGB8888,
        (32, 30) => fourcc(b"XR30"),
        (32, 32) => ARGB8888,
        _ => return Err(-EINVAL),
    };
    let mut r = [0u8; 104];
    set32(&mut r, 4, le32(a, 4));
    set32(&mut r, 8, le32(a, 8));
    set32(&mut r, 12, format);
    set32(&mut r, 20, le32(a, 24));
    set32(&mut r, 36, le32(a, 12));
    addfb2(k, f, &mut r)?;
    set32(a, 0, le32(&r, 0));
    Ok(())
}

/// ADDFB2 (drm_mode_fb_cmd2: fb_id、width、height、pixel_format、flags、handles[4]、pitches[4]、offsets[4]、modifier[4])
fn addfb2(k: &mut Kms, f: &DrmFile, a: &mut [u8]) -> Result<(), i64> {
    const INTERLACED: u32 = 1;
    const MODIFIERS: u32 = 2;
    let (w, h, format, flags) = (le32(a, 4), le32(a, 8), le32(a, 12), le32(a, 16));
    let handle = |i: usize| le32(a, 20 + i * 4);
    let pitch = |i: usize| le32(a, 36 + i * 4);
    let offset = |i: usize| le32(a, 52 + i * 4);
    let modifier = |i: usize| le64(a, 72 + i * 8);
    if flags & !(INTERLACED | MODIFIERS) != 0 || !(32..=edid::MAX).contains(&w) || !(32..=edid::MAX).contains(&h) {
        return Err(-EINVAL);
    }
    // virtio_gpu は modifier を受けない (fb_modifiers_not_supported)
    if flags & MODIFIERS != 0 {
        return Err(-EINVAL);
    }
    let (planes, cpp, hsub, vsub, _) = format_info(format).ok_or(-EINVAL)?;
    // framebuffer_check
    for i in 0..planes {
        let (pw, ph) = if i == 0 { (w, h) } else { (w.div_ceil(hsub), h.div_ceil(vsub)) };
        if handle(i) == 0 {
            return Err(-EINVAL);
        }
        let min_pitch = pw as u64 * cpp[i] as u64;
        if min_pitch > u32::MAX as u64 || ph as u64 * pitch(i) as u64 + offset(i) as u64 > u32::MAX as u64 {
            return Err(-ERANGE);
        }
        if (pitch(i) as u64) < min_pitch || modifier(i) != 0 {
            return Err(-EINVAL);
        }
    }
    if (planes..4).any(|i| modifier(i) != 0) {
        return Err(-EINVAL);
    }
    // virtio_gpu_user_framebuffer_create: XRGB8888 と ARGB8888 だけ、ハンドルがなければ EINVAL
    if format != XRGB8888 && format != ARGB8888 {
        return Err(-ENOENT);
    }
    let bo = f.bo(handle(0)).ok_or(-EINVAL)?;
    let id = new_id(k);
    k.seq += 1;
    k.fbs.insert(id, Fb { owner: f.id(), seq: k.seq, w, h, format, pitch: pitch(0), offset: offset(0), bo: Some(bo) });
    set32(a, 0, id);
    Ok(())
}

/// RMFB と CLOSEFB: 自分の framebuffer だけ (ほかは ENOENT)。RMFB は映していれば外す (drm_framebuffer_remove)
fn rmfb(k: &mut Kms, f: &DrmFile, id: u32, remove: bool) -> Result<(), i64> {
    match k.fbs.get(&id) {
        Some(fb) if fb.owner == f.id() && fb.owner != 0 => {}
        _ => return Err(-ENOENT),
    }
    // 映していれば primary plane を外す (atomic_remove_fb: CRTC は動いたまま、画面は消える)
    if remove && k.fb == id {
        k.fb = 0;
        present(k, None);
    }
    if let Some(fb) = k.fbs.get_mut(&id) {
        fb.owner = 0;
    }
    gc(k);
    Ok(())
}

fn page_flip(k: &mut Kms, f: &DrmFile, a: &mut [u8]) -> Result<(), i64> {
    const EVENT: u32 = 1;
    const ASYNC: u32 = 2;
    const TARGET: u32 = 4 | 8;
    let (crtc, fb_id, flags, seq, user) = (le32(a, 0), le32(a, 4), le32(a, 8), le32(a, 12), le64(a, 16));
    if flags & !(EVENT | ASYNC | TARGET) != 0 || (seq != 0 && flags & TARGET == 0) || flags & TARGET == TARGET {
        return Err(-EINVAL);
    }
    // 待たずに替えることも、vblank を選ぶこともできない
    if flags & ASYNC != 0 {
        return Err(-EINVAL);
    }
    if crtc != CRTC {
        return Err(-ENOENT);
    }
    if flags & TARGET != 0 {
        return Err(-EINVAL);
    }
    if k.fb == 0 {
        return Err(-EBUSY);
    }
    let new = k.fbs.get(&fb_id).ok_or(-ENOENT)?;
    let m = k.mode.ok_or(-EINVAL)?;
    if !viewport_ok(&m, k.x, k.y, new) {
        return Err(-ENOSPC);
    }
    if new.format != k.fbs[&k.fb].format {
        return Err(-EINVAL);
    }
    if flags & EVENT != 0 && !f.event_room(32) {
        return Err(-ENOMEM);
    }
    if k.dpms != 0 {
        return Err(-EINVAL);
    }
    k.fb = fb_id;
    present(k, None);
    gc(k);
    if flags & EVENT != 0 {
        // drm_event_vblank: type (FLIP_COMPLETE 2)、length、user_data、tv_sec、tv_usec (CLOCK_MONOTONIC)、sequence、crtc_id
        let now = crate::timer::uptime_ns();
        let mut e = [0u8; 32];
        set32(&mut e, 0, 2);
        set32(&mut e, 4, 32);
        set64(&mut e, 8, user);
        set32(&mut e, 16, (now / 1_000_000_000) as u32);
        set32(&mut e, 20, (now % 1_000_000_000 / 1000) as u32);
        set32(&mut e, 24, 0);
        set32(&mut e, 28, CRTC);
        f.push_event(&e);
    }
    Ok(())
}

/// DIRTYFB: 変わった四角を画面へ (drm_atomic_helper_dirtyfb: 四角はまとめて 1 つの外枠に)
fn dirtyfb(k: &mut Kms, a: &mut [u8]) -> Result<(), i64> {
    const ANNOTATE_COPY: u32 = 1;
    const FLAGS: u32 = 3;
    let (id, flags, n, clips) = (le32(a, 0), le32(a, 4) & FLAGS, le32(a, 12) as usize, le64(a, 16));
    if !k.fbs.contains_key(&id) {
        return Err(-ENOENT);
    }
    if (n == 0) != (clips == 0) || (flags & ANNOTATE_COPY != 0 && n % 2 != 0) || n > 256 {
        return Err(-EINVAL);
    }
    let mut c = vec![0u8; n * 8];
    copy_in(clips, &mut c)?;
    if k.fb != id || !active(k) {
        return Ok(());
    }
    let m = k.mode.unwrap();
    let (x0, y0, x1, y1) = (k.x, k.y, k.x + m.hd as u32, k.y + m.vd as u32);
    let mut r: Option<(u32, u32, u32, u32)> = None;
    for cl in c.chunks(8) {
        let v = |o: usize| u16::from_le_bytes([cl[o], cl[o + 1]]) as u32;
        let (a0, b0, a1, b1) = (v(0).max(x0), v(2).max(y0), v(4).min(x1), v(6).min(y1));
        if a0 >= a1 || b0 >= b1 {
            continue;
        }
        r = Some(match r {
            None => (a0, b0, a1, b1),
            Some((p, q, s, t)) => (p.min(a0), q.min(b0), s.max(a1), t.max(b1)),
        });
    }
    if n == 0 {
        present(k, None);
    } else if let Some((p, q, s, t)) = r {
        present(k, Some((p, q, s - p, t - q)));
    }
    Ok(())
}

fn create_dumb(f: &DrmFile, a: &mut [u8]) -> Result<(), i64> {
    let (h, w, bpp) = (le32(a, 0), le32(a, 4), le32(a, 8));
    if w == 0 || h == 0 || bpp == 0 || bpp > u32::MAX - 8 {
        return Err(-EINVAL);
    }
    let cpp = bpp.div_ceil(8);
    if cpp > u32::MAX / w {
        return Err(-EINVAL);
    }
    let stride = cpp * w;
    if h > u32::MAX / stride || (h * stride).checked_add(4095).is_none() {
        return Err(-EINVAL);
    }
    set32(a, 16, 0);
    set32(a, 20, 0);
    set64(a, 24, 0);
    // virtio_gpu_mode_dumb_create: 32 bpp だけ
    if bpp != 32 {
        return Err(-EINVAL);
    }
    let pitch = w * 4;
    let size = ((pitch as u64 * h as u64) + 4095) & !4095;
    let handle = f.create_dumb(w, h, size as usize)?;
    set32(a, 16, handle);
    set32(a, 20, pitch);
    set64(a, 24, size);
    Ok(())
}

fn getplane(k: &Kms, a: &mut [u8]) -> Result<(), i64> {
    let id = le32(a, 0);
    let fmt = match id {
        PLANE_PRIMARY => XRGB8888,
        PLANE_CURSOR => ARGB8888,
        _ => return Err(-ENOENT),
    };
    let on = id == PLANE_PRIMARY && active(k);
    set32(a, 4, if on { CRTC } else { 0 });
    set32(a, 8, if on { k.fb } else { 0 });
    set32(a, 12, 1);
    set32(a, 16, 0);
    if le32(a, 20) >= 1 {
        copy_out(le64(a, 24), &fmt.to_le_bytes())?;
    }
    set32(a, 20, 1);
    Ok(())
}

/// SETPLANE: primary は CRTC 全体に、大きさをかえずに (virtio_gpu_plane_atomic_check)。カーソルはまだ
fn setplane(k: &mut Kms, a: &mut [u8]) -> Result<(), i64> {
    let (plane, crtc, fb_id) = (le32(a, 0), le32(a, 4), le32(a, 8));
    let (cx, cy, cw, ch) = (le32(a, 16) as i32, le32(a, 20) as i32, le32(a, 24), le32(a, 28));
    let (sx, sy, sh, sw) = (le32(a, 32), le32(a, 36), le32(a, 40), le32(a, 44));
    if plane != PLANE_PRIMARY && plane != PLANE_CURSOR {
        return Err(-ENOENT);
    }
    if fb_id == 0 {
        // primary は CRTC が動いているあいだは外せない
        return if plane == PLANE_CURSOR { Ok(()) } else { Err(-EINVAL) };
    }
    if crtc != CRTC {
        return Err(-ENOENT);
    }
    let fb = k.fbs.get(&fb_id).ok_or(-ENOENT)?;
    if plane == PLANE_CURSOR {
        return Err(-EINVAL);
    }
    if fb.format != XRGB8888 {
        return Err(-EINVAL);
    }
    let m = k.mode.ok_or(-EINVAL)?;
    if cx != 0 || cy != 0 || cw != m.hd as u32 || ch != m.vd as u32 || sw >> 16 != cw || sh >> 16 != ch {
        return Err(-EINVAL);
    }
    let (x, y) = (sx >> 16, sy >> 16);
    if !viewport_ok(&m, x, y, fb) {
        return Err(-ENOSPC);
    }
    k.fb = fb_id;
    k.x = x;
    k.y = y;
    present(k, None);
    gc(k);
    Ok(())
}

/// ファイルを閉じた (drm_fb_release と、ユーザーの blob を消す)。映しているものを作ったファイルなら、primary を外す
pub fn file_closed(id: u32) {
    let Some(k) = KMS.get().as_mut() else { return };
    let mine: Vec<u32> = k.fbs.iter().filter(|(_, fb)| fb.owner == id).map(|(&i, _)| i).collect();
    for i in mine {
        if k.fb == i {
            k.fb = 0;
            present(k, None);
        }
        k.fbs.remove(&i);
    }
    k.blobs.retain(|_, b| b.0 != id);
}

/// 最後の DRM のファイルを閉じた (Linux の drm_lastclose → fbdev の restore): /dev/fb0 を、はじめのモードで映す
pub fn lastclose() {
    let Some(k) = KMS.get().as_mut() else { return };
    let Some(g) = crate::gpu::get() else { return };
    k.mode = k.modes.first().copied().filter(|m| m.hd as u32 <= g.width && m.vd as u32 <= g.height);
    k.fb = if k.mode.is_some() { FBDEV } else { 0 };
    k.x = 0;
    k.y = 0;
    k.dpms = 0;
    present(k, None);
    gc(k);
}

/// GET_CAP の KMS のもの (drm_getcap)。知らないものは EINVAL
pub fn cap(c: u64) -> Result<u64, i64> {
    Ok(match c {
        // DUMB_BUFFER、VBLANK_HIGH_CRTC
        1 | 2 => 1,
        // DUMB_PREFERRED_DEPTH、DUMB_PREFER_SHADOW、ASYNC_PAGE_FLIP (virtio_gpu は決めていない: 0)
        3 | 4 | 7 => 0,
        // CURSOR_WIDTH、CURSOR_HEIGHT (決めていなければ 64)
        8 | 9 => 64,
        // ADDFB2_MODIFIERS (fb_modifiers_not_supported)、PAGE_FLIP_TARGET、ATOMIC_ASYNC_PAGE_FLIP
        0x10 | 0x11 | 0x15 => 0,
        // CRTC_IN_VBLANK_EVENT
        0x12 => 1,
        _ => return Err(-EINVAL),
    })
}

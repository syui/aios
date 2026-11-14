// 画面に描く。/dev/dri/card0 (Linux の DRM の KMS: atomic とカーソルの plane) が使えればそれを、なければ /dev/fb0
// (Linux の fbdev と同じ形) を
//   Fb::open() で開いて mmap、pixels() で描き、present() と present_rect() で画面へ
//   DRM: 画面ぜんぶの dumb の BO を primary plane に映し (MODE_ATOMIC、ALLOW_MODESET)、変わったところは DIRTYFB。
//        カーソルは cursor plane (CURSOR_PLANE_HOTSPOT を言えたとき) に置き、動かすのは plane の CRTC_X と CRTC_Y だけ
//        (下の窓を描きなおさない)。card0 の master はほかの人が持っていない (はじめに開いた) ときだけ。
//        AIWM_FBDEV=1 なら DRM を使わない
//   fbdev: FBIOPAN_DISPLAY と FBIO_DAMAGE。カーソルは使う人が絵に描く (hw_cursor() が false)
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;

const FBIOGET_VSCREENINFO: libc::c_int = 0x4600;
const FBIOGET_FSCREENINFO: libc::c_int = 0x4602;
const FBIOPAN_DISPLAY: libc::c_int = 0x4606;

pub struct Fb {
    file: File,
    mem: *mut u32,
    len: usize,
    pub width: usize,
    pub height: usize,
    /// 1 行の画素の数 (line_length / 4)
    pub stride: usize,
    /// blend が描くところ (x0, y0, x1, y1)。aiwm が変わったところだけ描くときに狭める
    pub clip: (i32, i32, i32, i32),
    drm: Option<drm::Drm>,
}

impl Fb {
    pub fn open() -> io::Result<Fb> {
        if std::env::var_os("AIWM_FBDEV").is_none() {
            match drm::open() {
                Ok(fb) => return Ok(fb),
                Err(e) if std::env::var_os("AIWM_DEBUG").is_some() => eprintln!("aiwm: /dev/dri/card0: {} (use /dev/fb0)", e),
                Err(_) => {}
            }
        }
        Fb::open_fbdev()
    }

    fn open_fbdev() -> io::Result<Fb> {
        let file = OpenOptions::new().read(true).write(true).open("/dev/fb0")?;
        let fd = file.as_raw_fd();
        let mut var = [0u32; 40];
        let mut fix = [0u8; 80];
        unsafe {
            if libc::ioctl(fd, FBIOGET_VSCREENINFO, var.as_mut_ptr()) != 0 || libc::ioctl(fd, FBIOGET_FSCREENINFO, fix.as_mut_ptr()) != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        if var[6] != 32 {
            return Err(io::Error::other(format!("{} bits per pixel (want 32)", var[6])));
        }
        let len = u32::from_le_bytes(fix[24..28].try_into().unwrap()) as usize;
        let line = u32::from_le_bytes(fix[48..52].try_into().unwrap()) as usize;
        let mem = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0) };
        if mem == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let (width, height) = (var[0] as usize, var[1] as usize);
        Ok(Fb { file, mem: mem as *mut u32, len, width, height, stride: line / 4, clip: (0, 0, width as i32, height as i32), drm: None })
    }

    /// どの口で描いているか ("drm" か "fbdev")
    pub fn kind(&self) -> &'static str {
        if self.drm.is_some() { "drm" } else { "fbdev" }
    }

    /// カーソルを画面の側 (DRM の cursor plane) で出せるか。false なら使う人が絵に描く
    pub fn hw_cursor(&self) -> bool {
        self.drm.as_ref().is_some_and(|d| d.cursor.is_some())
    }

    /// カーソルの絵 (w x h の ARGB、premultiplied。熱いところ hot) を決める。hw_cursor() のときだけ
    pub fn set_cursor(&mut self, img: &[u32], w: usize, h: usize, hot: (i32, i32)) {
        let fd = self.file.as_raw_fd();
        if let Some(d) = self.drm.as_mut() {
            d.set_cursor(fd, img, w, h, hot);
        }
    }

    /// カーソルを (x, y) に (熱いところをそこに)。はじめて呼んだときに出す
    pub fn move_cursor(&mut self, x: i32, y: i32) {
        let fd = self.file.as_raw_fd();
        if let Some(d) = self.drm.as_mut() {
            d.move_cursor(fd, x, y);
        }
    }

    pub fn pixels(&mut self) -> &mut [u32] {
        unsafe { std::slice::from_raw_parts_mut(self.mem, self.len / 4) }
    }

    /// (x, y) に色 (0xRRGGBB) を a (0..=255) の濃さで重ねる
    pub fn blend(&mut self, x: i32, y: i32, rgb: u32, a: u32) {
        let (cx0, cy0, cx1, cy1) = self.clip;
        if x < cx0.max(0) || y < cy0.max(0) || x >= cx1.min(self.width as i32) || y >= cy1.min(self.height as i32) || a == 0 {
            return;
        }
        let i = y as usize * self.stride + x as usize;
        let p = &mut self.pixels()[i];
        if a >= 255 {
            *p = rgb;
            return;
        }
        let mix = |s: u32, d: u32| (s * a + d * (255 - a)) / 255;
        let (sr, sg, sb) = (rgb >> 16 & 255, rgb >> 8 & 255, rgb & 255);
        let (dr, dg, db) = (*p >> 16 & 255, *p >> 8 & 255, *p & 255);
        *p = mix(sr, dr) << 16 | mix(sg, dg) << 8 | mix(sb, db);
    }

    pub fn fill(&mut self, rgb: u32) {
        self.pixels().fill(rgb);
    }

    /// 描いたものを画面へ
    pub fn present(&self) {
        if let Some(d) = &self.drm {
            d.dirty(self.file.as_raw_fd(), None);
            return;
        }
        let mut var = [0u32; 40];
        unsafe { libc::ioctl(self.file.as_raw_fd(), FBIOPAN_DISPLAY, var.as_mut_ptr()) };
    }
}

impl Fb {
    #[allow(dead_code)]
    /// 四角 [x0, x1) x [y0, y1) だけ画面へ (FBIO_DAMAGE。古いカーネルで使えなければ、その行を write)
    pub fn present_rect(&self, x0: usize, y0: usize, x1: usize, y1: usize) {
        const FBIO_DAMAGE: libc::c_ulong = 0x4010_46a0;
        let (x1, y1) = (x1.min(self.width), y1.min(self.height));
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        if let Some(d) = &self.drm {
            d.dirty(self.file.as_raw_fd(), Some((x0 as u16, y0 as u16, x1 as u16, y1 as u16)));
            return;
        }
        let r = [x0 as u32, y0 as u32, (x1 - x0) as u32, (y1 - y0) as u32];
        if unsafe { libc::ioctl(self.file.as_raw_fd(), FBIO_DAMAGE as _, r.as_ptr()) } != 0 {
            self.present_rows(y0, y1);
        }
    }

    #[allow(dead_code)]
    /// [y0, y1) の行だけ画面へ (その行を自分自身へ書くと、カーネルがその行だけ送る)
    pub fn present_rows(&self, y0: usize, y1: usize) {
        let (y0, y1) = (y0.min(self.height), y1.min(self.height));
        if y0 >= y1 {
            return;
        }
        if self.drm.is_some() {
            self.present_rect(0, y0, self.width, y1);
            return;
        }
        let off = y0 * self.stride * 4;
        let len = (y1 - y0) * self.stride * 4;
        let fd = self.file.as_raw_fd();
        unsafe {
            libc::lseek(fd, off as _, libc::SEEK_SET);
            let mut done = 0;
            while done < len {
                let n = libc::write(fd, (self.mem as *const u8).add(off + done) as *const _, len - done);
                if n <= 0 {
                    break;
                }
                done += n as usize;
            }
        }
    }
}

impl Drop for Fb {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.mem as *mut _, self.len) };
    }
}

// DRM の KMS (Linux の include/uapi/drm/drm.h と drm_mode.h の ioctl。libdrm は使わない)
mod drm {
    use super::Fb;
    use std::fs::OpenOptions;
    use std::io;
    use std::os::fd::AsRawFd;

    const GET_CAP: u32 = 0xc010_640c;
    const SET_CLIENT_CAP: u32 = 0x4010_640d;
    const GETRESOURCES: u32 = 0xc040_64a0;
    const GETENCODER: u32 = 0xc014_64a6;
    const GETCONNECTOR: u32 = 0xc050_64a7;
    const GETPROPERTY: u32 = 0xc040_64aa;
    const DIRTYFB: u32 = 0xc018_64b1;
    const CREATE_DUMB: u32 = 0xc020_64b2;
    const MAP_DUMB: u32 = 0xc010_64b3;
    const GETPLANERESOURCES: u32 = 0xc010_64b5;
    const GETPLANE: u32 = 0xc020_64b6;
    const ADDFB2: u32 = 0xc068_64b8;
    const OBJ_GETPROPERTIES: u32 = 0xc020_64b9;
    const ATOMIC: u32 = 0xc038_64bc;
    const CREATEPROPBLOB: u32 = 0xc010_64bd;

    const CAP_ATOMIC: u64 = 3;
    const CAP_CURSOR_PLANE_HOTSPOT: u64 = 6;
    const CAP_CURSOR_WIDTH: u64 = 8;
    const CAP_CURSOR_HEIGHT: u64 = 9;
    const ALLOW_MODESET: u32 = 0x400;
    const OBJ_CRTC: u32 = 0xcccc_cccc;
    const OBJ_CONNECTOR: u32 = 0xc0c0_c0c0;
    const OBJ_PLANE: u32 = 0xeeee_eeee;
    const XRGB8888: u32 = 0x3432_5258;
    const ARGB8888: u32 = 0x3432_5241;
    const MODE_PREFERRED: u32 = 1 << 3;
    const PLANE_PRIMARY: u64 = 1;
    const PLANE_CURSOR: u64 = 2;

    fn ioctl(fd: i32, req: u32, a: &mut [u8]) -> io::Result<()> {
        loop {
            if unsafe { libc::ioctl(fd, req as _, a.as_mut_ptr()) } == 0 {
                return Ok(());
            }
            let e = io::Error::last_os_error();
            if !matches!(e.raw_os_error(), Some(libc::EINTR | libc::EAGAIN)) {
                return Err(e);
            }
        }
    }

    fn r32(a: &[u8], o: usize) -> u32 {
        u32::from_le_bytes(a[o..o + 4].try_into().unwrap())
    }

    fn w32(a: &mut [u8], o: usize, v: u32) {
        a[o..o + 4].copy_from_slice(&v.to_le_bytes());
    }

    fn r64(a: &[u8], o: usize) -> u64 {
        u64::from_le_bytes(a[o..o + 8].try_into().unwrap())
    }

    fn w64(a: &mut [u8], o: usize, v: u64) {
        a[o..o + 8].copy_from_slice(&v.to_le_bytes());
    }

    fn addr<T>(v: &mut [T]) -> u64 {
        if v.is_empty() { 0 } else { v.as_mut_ptr() as u64 }
    }

    fn get_cap(fd: i32, cap: u64) -> Option<u64> {
        let mut a = [0u8; 16];
        w64(&mut a, 0, cap);
        ioctl(fd, GET_CAP, &mut a).ok().map(|_| r64(&a, 8))
    }

    fn client_cap(fd: i32, cap: u64, v: u64) -> io::Result<()> {
        let mut a = [0u8; 16];
        w64(&mut a, 0, cap);
        w64(&mut a, 8, v);
        ioctl(fd, SET_CLIENT_CAP, &mut a)
    }

    /// オブジェクトのプロパティ (名前、番号、いまの値)
    struct Props(Vec<(String, u32, u64)>);

    impl Props {
        fn read(fd: i32, obj: u32, typ: u32) -> io::Result<Props> {
            let mut a = [0u8; 32];
            w32(&mut a, 20, obj);
            w32(&mut a, 24, typ);
            ioctl(fd, OBJ_GETPROPERTIES, &mut a)?;
            let n = r32(&a, 16) as usize;
            let (mut ids, mut vals) = (vec![0u32; n], vec![0u64; n]);
            w64(&mut a, 0, addr(&mut ids));
            w64(&mut a, 8, addr(&mut vals));
            ioctl(fd, OBJ_GETPROPERTIES, &mut a)?;
            let n = n.min(r32(&a, 16) as usize);
            let mut out = vec![];
            for i in 0..n {
                let mut p = [0u8; 64];
                w32(&mut p, 16, ids[i]);
                ioctl(fd, GETPROPERTY, &mut p)?;
                let name = &p[24..56];
                let end = name.iter().position(|&c| c == 0).unwrap_or(name.len());
                out.push((String::from_utf8_lossy(&name[..end]).into_owned(), ids[i], vals[i]));
            }
            Ok(Props(out))
        }

        fn id(&self, name: &str) -> io::Result<u32> {
            self.0.iter().find(|p| p.0 == name).map(|p| p.1).ok_or_else(|| io::Error::other(format!("no property {}", name)))
        }

        fn has(&self, name: &str) -> bool {
            self.0.iter().any(|p| p.0 == name)
        }

        fn value(&self, name: &str) -> Option<u64> {
            self.0.iter().find(|p| p.0 == name).map(|p| p.2)
        }
    }

    /// MODE_ATOMIC を 1 回: (オブジェクト, プロパティ, 値) をオブジェクトごとにまとめて
    fn commit(fd: i32, flags: u32, set: &[(u32, u32, u64)]) -> io::Result<()> {
        let mut objs: Vec<u32> = vec![];
        for &(o, _, _) in set {
            if !objs.contains(&o) {
                objs.push(o);
            }
        }
        let (mut counts, mut ps, mut vs) = (vec![], vec![], vec![]);
        for &o in &objs {
            let mut n = 0u32;
            for &(o2, p, v) in set {
                if o2 == o {
                    ps.push(p);
                    vs.push(v);
                    n += 1;
                }
            }
            counts.push(n);
        }
        let mut a = [0u8; 56];
        w32(&mut a, 0, flags);
        w32(&mut a, 4, objs.len() as u32);
        w64(&mut a, 8, addr(&mut objs));
        w64(&mut a, 16, addr(&mut counts));
        w64(&mut a, 24, addr(&mut ps));
        w64(&mut a, 32, addr(&mut vs));
        ioctl(fd, ATOMIC, &mut a)
    }

    /// dumb の BO と、その framebuffer と、mmap した画素: (fb, 画素, バイト数, 1 行のバイト数)
    fn dumb(fd: i32, w: u32, h: u32, format: u32) -> io::Result<(u32, *mut u32, usize, usize)> {
        let mut a = [0u8; 32];
        w32(&mut a, 0, h);
        w32(&mut a, 4, w);
        w32(&mut a, 8, 32);
        ioctl(fd, CREATE_DUMB, &mut a)?;
        let (handle, pitch, size) = (r32(&a, 16), r32(&a, 20), r64(&a, 24) as usize);
        let mut f = [0u8; 104];
        w32(&mut f, 4, w);
        w32(&mut f, 8, h);
        w32(&mut f, 12, format);
        w32(&mut f, 20, handle);
        w32(&mut f, 36, pitch);
        ioctl(fd, ADDFB2, &mut f)?;
        let mut m = [0u8; 16];
        w32(&mut m, 0, handle);
        ioctl(fd, MAP_DUMB, &mut m)?;
        let mem = unsafe { libc::mmap(std::ptr::null_mut(), size, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, r64(&m, 8) as libc::off_t) };
        if mem == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok((r32(&f, 0), mem as *mut u32, size, pitch as usize))
    }

    pub struct Drm {
        fb: u32,
        crtc: u32,
        pub cursor: Option<Cursor>,
    }

    pub struct Cursor {
        plane: u32,
        props: Props,
        fb: u32,
        mem: *mut u32,
        len: usize,
        w: usize,
        h: usize,
        stride: usize,
        hot: (i32, i32),
        shown: bool,
    }

    impl Drop for Cursor {
        fn drop(&mut self) {
            unsafe { libc::munmap(self.mem as *mut _, self.len) };
        }
    }

    /// card0 を開いて、つながった connector のいちばんよいモードで画面ぜんぶの dumb の BO を映す
    pub fn open() -> io::Result<Fb> {
        let file = OpenOptions::new().read(true).write(true).open("/dev/dri/card0")?;
        let fd = file.as_raw_fd();
        client_cap(fd, CAP_ATOMIC, 1)?;
        let hotspot = client_cap(fd, CAP_CURSOR_PLANE_HOTSPOT, 1).is_ok();
        let mut res = [0u8; 64];
        ioctl(fd, GETRESOURCES, &mut res)?;
        let (mut crtcs, mut conns) = (vec![0u32; r32(&res, 36) as usize], vec![0u32; r32(&res, 40) as usize]);
        let mut res = [0u8; 64];
        w64(&mut res, 8, addr(&mut crtcs));
        w64(&mut res, 16, addr(&mut conns));
        w32(&mut res, 36, crtcs.len() as u32);
        w32(&mut res, 40, conns.len() as u32);
        ioctl(fd, GETRESOURCES, &mut res)?;
        // つながった connector とそのモード (PREFERRED のもの、なければはじめのもの)
        let mut found = None;
        for &c in &conns {
            let mut a = [0u8; 80];
            w32(&mut a, 48, c);
            ioctl(fd, GETCONNECTOR, &mut a)?;
            if r32(&a, 60) != 1 || r32(&a, 32) == 0 {
                continue;
            }
            let (nm, ne) = (r32(&a, 32) as usize, r32(&a, 40) as usize);
            let mut modes = vec![0u8; nm * 68];
            let mut encs = vec![0u32; ne];
            let mut b = [0u8; 80];
            w64(&mut b, 0, addr(&mut encs));
            w64(&mut b, 8, addr(&mut modes));
            w32(&mut b, 32, nm as u32);
            w32(&mut b, 40, ne as u32);
            w32(&mut b, 48, c);
            ioctl(fd, GETCONNECTOR, &mut b)?;
            let nm = nm.min(r32(&b, 32) as usize);
            if nm == 0 {
                continue;
            }
            let pick = (0..nm).find(|&i| r32(&modes[i * 68..], 32) & MODE_PREFERRED != 0).unwrap_or(0);
            let enc = if r32(&b, 44) != 0 { r32(&b, 44) } else { encs.first().copied().unwrap_or(0) };
            found = Some((c, modes[pick * 68..pick * 68 + 68].to_vec(), enc));
            break;
        }
        let (conn, mode, enc) = found.ok_or_else(|| io::Error::other("no connected connector"))?;
        // encoder がつながる crtc
        let mut e = [0u8; 20];
        w32(&mut e, 0, enc);
        ioctl(fd, GETENCODER, &mut e)?;
        let ci = match crtcs.iter().position(|&c| c == r32(&e, 8)) {
            Some(i) => i,
            None => (0..crtcs.len()).find(|&i| r32(&e, 12) & (1 << i) != 0).ok_or_else(|| io::Error::other("no crtc"))?,
        };
        let crtc = crtcs[ci];
        // その crtc に使える primary と cursor の plane
        let mut pr = [0u8; 16];
        ioctl(fd, GETPLANERESOURCES, &mut pr)?;
        let mut planes = vec![0u32; r32(&pr, 8) as usize];
        w64(&mut pr, 0, addr(&mut planes));
        ioctl(fd, GETPLANERESOURCES, &mut pr)?;
        let (mut primary, mut cursor) = (None, None);
        for &p in &planes {
            let mut a = [0u8; 32];
            w32(&mut a, 0, p);
            ioctl(fd, GETPLANE, &mut a)?;
            if r32(&a, 12) & (1 << ci) == 0 {
                continue;
            }
            let props = Props::read(fd, p, OBJ_PLANE)?;
            match props.value("type") {
                Some(PLANE_PRIMARY) if primary.is_none() => primary = Some((p, props)),
                Some(PLANE_CURSOR) if cursor.is_none() && hotspot => cursor = Some((p, props)),
                _ => {}
            }
        }
        let (plane, pp) = primary.ok_or_else(|| io::Error::other("no primary plane"))?;
        let (w, h) = (u16::from_le_bytes([mode[4], mode[5]]) as u32, u16::from_le_bytes([mode[14], mode[15]]) as u32);
        let (fb, mem, len, pitch) = dumb(fd, w, h, XRGB8888)?;
        let mut mode = mode;
        let mut b = [0u8; 16];
        w64(&mut b, 0, addr(&mut mode));
        w32(&mut b, 8, 68);
        ioctl(fd, CREATEPROPBLOB, &mut b)?;
        let blob = r32(&b, 12);
        let (cp, rp) = (Props::read(fd, conn, OBJ_CONNECTOR)?, Props::read(fd, crtc, OBJ_CRTC)?);
        let mut set = vec![
            (conn, cp.id("CRTC_ID")?, crtc as u64),
            (crtc, rp.id("ACTIVE")?, 1),
            (crtc, rp.id("MODE_ID")?, blob as u64),
            (plane, pp.id("FB_ID")?, fb as u64),
            (plane, pp.id("CRTC_ID")?, crtc as u64),
            (plane, pp.id("SRC_X")?, 0),
            (plane, pp.id("SRC_Y")?, 0),
            (plane, pp.id("SRC_W")?, (w as u64) << 16),
            (plane, pp.id("SRC_H")?, (h as u64) << 16),
            (plane, pp.id("CRTC_X")?, 0),
            (plane, pp.id("CRTC_Y")?, 0),
            (plane, pp.id("CRTC_W")?, w as u64),
            (plane, pp.id("CRTC_H")?, h as u64),
        ];
        // カーソルの plane は、はじめは消しておく (前に使った人のものが残っていれば)
        let cursor = match cursor {
            Some((cpl, props)) if props.has("FB_ID") && props.has("CRTC_ID") => {
                set.push((cpl, props.id("FB_ID")?, 0));
                set.push((cpl, props.id("CRTC_ID")?, 0));
                let cw = get_cap(fd, CAP_CURSOR_WIDTH).unwrap_or(64).clamp(1, 256) as u32;
                let ch = get_cap(fd, CAP_CURSOR_HEIGHT).unwrap_or(64).clamp(1, 256) as u32;
                dumb(fd, cw, ch, ARGB8888).ok().map(|(cfb, cmem, clen, cpitch)| Cursor {
                    plane: cpl,
                    props,
                    fb: cfb,
                    mem: cmem,
                    len: clen,
                    w: cw as usize,
                    h: ch as usize,
                    stride: cpitch / 4,
                    hot: (0, 0),
                    shown: false,
                })
            }
            _ => None,
        };
        if let Err(e) = commit(fd, ALLOW_MODESET, &set) {
            unsafe { libc::munmap(mem as *mut _, len) };
            return Err(e);
        }
        Ok(Fb {
            file,
            mem,
            len,
            width: w as usize,
            height: h as usize,
            stride: pitch / 4,
            clip: (0, 0, w as i32, h as i32),
            drm: Some(Drm { fb, crtc, cursor }),
        })
    }

    impl Drm {
        /// 変わった四角 (x0, y0, x1, y1) を画面へ (None なら画面ぜんぶ)
        pub fn dirty(&self, fd: i32, r: Option<(u16, u16, u16, u16)>) {
            let mut clip = [0u8; 8];
            let mut a = [0u8; 24];
            w32(&mut a, 0, self.fb);
            if let Some((x0, y0, x1, y1)) = r {
                for (i, v) in [x0, y0, x1, y1].into_iter().enumerate() {
                    clip[i * 2..i * 2 + 2].copy_from_slice(&v.to_le_bytes());
                }
                w32(&mut a, 12, 1);
                w64(&mut a, 16, clip.as_mut_ptr() as u64);
            }
            let _ = ioctl(fd, DIRTYFB, &mut a);
        }

        pub fn set_cursor(&mut self, fd: i32, img: &[u32], w: usize, h: usize, hot: (i32, i32)) {
            let Some(c) = self.cursor.as_mut() else { return };
            let px = unsafe { std::slice::from_raw_parts_mut(c.mem, c.len / 4) };
            for y in 0..c.h {
                for x in 0..c.w {
                    px[y * c.stride + x] = if x < w && y < h { img.get(y * w + x).copied().unwrap_or(0) } else { 0 };
                }
            }
            c.hot = (hot.0.clamp(0, c.w as i32 - 1), hot.1.clamp(0, c.h as i32 - 1));
            // 同じ framebuffer のままでは絵を送りなおさない (virtio_gpu の cursor plane) ので、いったん外す
            if c.shown {
                if let (Ok(f), Ok(cr)) = (c.props.id("FB_ID"), c.props.id("CRTC_ID")) {
                    let _ = commit(fd, 0, &[(c.plane, f, 0), (c.plane, cr, 0)]);
                }
                c.shown = false;
            }
        }

        pub fn move_cursor(&mut self, fd: i32, x: i32, y: i32) {
            let crtc = self.crtc;
            let Some(c) = self.cursor.as_mut() else { return };
            let (cx, cy) = ((x - c.hot.0) as i64 as u64, (y - c.hot.1) as i64 as u64);
            let p = &c.props;
            let set: Vec<(u32, u32, u64)> = if c.shown {
                [("CRTC_X", cx), ("CRTC_Y", cy)].iter().filter_map(|&(n, v)| p.id(n).ok().map(|i| (c.plane, i, v))).collect()
            } else {
                let mut s = vec![
                    ("FB_ID", c.fb as u64),
                    ("CRTC_ID", crtc as u64),
                    ("SRC_X", 0),
                    ("SRC_Y", 0),
                    ("SRC_W", (c.w as u64) << 16),
                    ("SRC_H", (c.h as u64) << 16),
                    ("CRTC_X", cx),
                    ("CRTC_Y", cy),
                    ("CRTC_W", c.w as u64),
                    ("CRTC_H", c.h as u64),
                ];
                if p.has("HOTSPOT_X") && p.has("HOTSPOT_Y") {
                    s.push(("HOTSPOT_X", c.hot.0 as u64));
                    s.push(("HOTSPOT_Y", c.hot.1 as u64));
                }
                s.iter().filter_map(|&(n, v)| p.id(n).ok().map(|i| (c.plane, i, v))).collect()
            };
            match commit(fd, 0, &set) {
                Ok(()) => c.shown = true,
                Err(e) if std::env::var_os("AIWM_DEBUG").is_some() => eprintln!("aiwm: cursor plane: {}", e),
                Err(_) => {}
            }
        }
    }
}

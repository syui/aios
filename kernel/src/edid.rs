// EDID (画面の名札) からモードの一覧を作る (Linux の drivers/gpu/drm/drm_edid.c、drm_modes.c、drm_probe_helper.c と
// virtio/virtgpu_display.c のうち、virtio-gpu の画面に要るところ。drm.rs の KMS が使う):
//   詳しいタイミング (はじめのものが preferred)、CVT の 3 バイトのもの、標準タイミング (DMT の表か、なければ CVT で計算)、
//   確立タイミング (I/II と III)、CTA (CEA-861) の VIC と、60 Hz と 59.94 Hz の兄弟 (1000/1001 の時計)。
//   EDID がなければ virtio_gpu と同じく、DMT の 61 Hz までのものと、画面の大きさの CVT (preferred)
// 集めたものは Linux と同じく、同じものをまとめ (drm_connector_list_update)、たしかめて (大きさ 8192 まで、インターレース
// なし、preferred は画面の大きさの 16 以内)、ならべる (preferred、面積、リフレッシュ、時計の大きい順。drm_mode_sort)。
// 表 (DMT、確立タイミング、CTA の VIC) は Linux 6.18 の drm_edid.c から写したもの (いちばん下)。
// まだ: DisplayID、HDMI の VSDB (4K の VIC)、YCbCr 4:2:0 だけのモード、GTF、範囲からのモード (連続周波数の画面)
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

pub const FLAG_PHSYNC: u32 = 1;
pub const FLAG_NHSYNC: u32 = 2;
pub const FLAG_PVSYNC: u32 = 4;
pub const FLAG_NVSYNC: u32 = 8;
pub const FLAG_INTERLACE: u32 = 16;
pub const FLAG_DBLSCAN: u32 = 32;
/// 立体 (3D) の形 (bit 14 から 5 ビット)
const FLAG_3D_MASK: u32 = 0x1f << 14;
/// 縦横比 (bit 19 から 4 ビット: ユーザーの形だけ)
const FLAG_PIC_AR_MASK: u32 = 0xf << 19;
/// Linux の DRM_MODE_FLAG_ALL (古い BCAST と PIXMUX を除く)
const FLAG_ALL: u32 = 0x3fff & !(1 << 10 | 1 << 11) | FLAG_3D_MASK;
pub const TYPE_PREFERRED: u32 = 1 << 3;
pub const TYPE_USERDEF: u32 = 1 << 5;
pub const TYPE_DRIVER: u32 = 1 << 6;
const TYPE_ALL: u32 = TYPE_PREFERRED | TYPE_USERDEF | TYPE_DRIVER;
/// 一覧に出す大きさの上限 (virtio_gpu の XRES_MAX、YRES_MAX)
pub const MAX: u32 = 8192;
/// 画面の大きさがわからないときの preferred (XRES_DEF、YRES_DEF)
const DEF: (u16, u16) = (1024, 768);

/// 表示のモード (Linux の drm_display_mode のうち、ユーザーに見せるもの)
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Mode {
    pub clock: u32,
    pub hd: u16,
    pub hss: u16,
    pub hse: u16,
    pub ht: u16,
    pub hsk: u16,
    pub vd: u16,
    pub vss: u16,
    pub vse: u16,
    pub vt: u16,
    pub vs: u16,
    pub flags: u32,
    pub typ: u32,
    /// 縦横比 (HDMI_PICTURE_ASPECT_*: 0 なし、1 4:3、2 16:9、3 64:27、4 256:135)
    pub ar: u8,
}

type M = Mode;

#[allow(clippy::too_many_arguments)]
const fn m(clock: u32, hd: u16, hss: u16, hse: u16, ht: u16, hsk: u16, vd: u16, vss: u16, vse: u16, vt: u16, vs: u16, flags: u32, ar: u8) -> Mode {
    Mode { clock, hd, hss, hse, ht, hsk, vd, vss, vse, vt, vs, flags, typ: TYPE_DRIVER, ar }
}

impl Mode {
    /// リフレッシュ (Hz、四捨五入: drm_mode_vrefresh)
    pub fn vrefresh(&self) -> u32 {
        if self.ht == 0 || self.vt == 0 {
            return 0;
        }
        let num = self.clock as u64 * if self.flags & FLAG_INTERLACE != 0 { 2 } else { 1 };
        let mut den = self.ht as u64 * self.vt as u64;
        if self.flags & FLAG_DBLSCAN != 0 {
            den *= 2;
        }
        if self.vs > 1 {
            den *= self.vs as u64;
        }
        ((num * 1000 + den / 2) / den) as u32
    }

    /// 名前 (1920x1080、インターレースなら 1920x1080i: drm_mode_set_name)
    pub fn name(&self) -> String {
        format!("{}x{}{}", self.hd, self.vd, if self.flags & FLAG_INTERLACE != 0 { "i" } else { "" })
    }

    fn timings_eq(&self, o: &Mode) -> bool {
        (self.hd, self.hss, self.hse, self.ht, self.hsk, self.vd, self.vss, self.vse, self.vt, self.vs) == (o.hd, o.hss, o.hse, o.ht, o.hsk, o.vd, o.vss, o.vse, o.vt, o.vs)
    }

    /// 時計はピコ秒にしてくらべる (drm_mode_match_clock)
    fn clock_eq(&self, o: &Mode) -> bool {
        if self.clock != 0 && o.clock != 0 { picos(self.clock) == picos(o.clock) } else { self.clock == o.clock }
    }

    fn flags_eq(&self, o: &Mode) -> bool {
        self.flags & !FLAG_3D_MASK == o.flags & !FLAG_3D_MASK
    }

    /// drm_mode_equal: タイミング、時計、flags (3D も)、縦横比
    fn equal(&self, o: &Mode) -> bool {
        self.timings_eq(o) && self.clock_eq(o) && self.flags == o.flags && self.ar == o.ar
    }

    /// drm_mode_validate_basic
    pub fn valid(&self) -> bool {
        self.typ & !TYPE_ALL == 0
            && self.flags & !FLAG_ALL == 0
            && self.clock != 0
            && self.hd != 0
            && self.hss >= self.hd
            && self.hse >= self.hss
            && self.ht >= self.hse
            && self.vd != 0
            && self.vss >= self.vd
            && self.vse >= self.vss
            && self.vt >= self.vse
    }

    /// ユーザーの形 (drm_mode_modeinfo: 68 バイト)。縦横比は ar (DRM_CLIENT_CAP_ASPECT_RATIO) のときだけ flags に
    pub fn to_user(&self, ar: bool) -> [u8; 68] {
        let mut b = [0u8; 68];
        b[0..4].copy_from_slice(&self.clock.to_le_bytes());
        for (i, v) in [self.hd, self.hss, self.hse, self.ht, self.hsk, self.vd, self.vss, self.vse, self.vt, self.vs].iter().enumerate() {
            b[4 + i * 2..6 + i * 2].copy_from_slice(&v.to_le_bytes());
        }
        b[24..28].copy_from_slice(&self.vrefresh().to_le_bytes());
        let pic = if ar { (self.ar as u32) << 19 } else { 0 };
        b[28..32].copy_from_slice(&(self.flags & !FLAG_PIC_AR_MASK | pic).to_le_bytes());
        b[32..36].copy_from_slice(&self.typ.to_le_bytes());
        let n = self.name();
        let k = n.len().min(31);
        b[36..36 + k].copy_from_slice(&n.as_bytes()[..k]);
        b
    }

    /// ユーザーの形から (drm_mode_convert_umode): 時計やリフレッシュが大きすぎれば ERANGE、形がおかしければ EINVAL
    pub fn from_user(b: &[u8]) -> Result<Mode, i64> {
        let u32_ = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        let u16_ = |o: usize| u16::from_le_bytes(b[o..o + 2].try_into().unwrap());
        if u32_(0) > i32::MAX as u32 || u32_(24) > i32::MAX as u32 {
            return Err(-34);
        }
        let flags = u32_(28);
        let ar = match flags & FLAG_PIC_AR_MASK {
            0 => 0,
            f if f >> 19 <= 4 => (f >> 19) as u8,
            _ => return Err(-22),
        };
        let md = Mode {
            clock: u32_(0),
            hd: u16_(4),
            hss: u16_(6),
            hse: u16_(8),
            ht: u16_(10),
            hsk: u16_(12),
            vd: u16_(14),
            vss: u16_(16),
            vse: u16_(18),
            vt: u16_(20),
            vs: u16_(22),
            flags: flags & !FLAG_PIC_AR_MASK,
            typ: u32_(32) & TYPE_ALL,
            ar,
        };
        if !md.valid() {
            return Err(-22);
        }
        Ok(md)
    }
}

fn picos(khz: u32) -> u32 {
    1_000_000_000 / khz
}

fn mode_is_rb(md: &Mode) -> bool {
    md.ht as i32 - md.hd as i32 == 160 && md.hse as i32 - md.hd as i32 == 80 && md.hse as i32 - md.hss as i32 == 32 && md.vss as i32 - md.vd as i32 == 3
}

/// DMT の表から (drm_mode_find_dmt)
fn find_dmt(w: u32, h: u32, r: u32, rb: bool) -> Option<Mode> {
    DMT.iter().find(|d| d.hd as u32 == w && d.vd as u32 == h && d.vrefresh() == r && mode_is_rb(d) == rb).copied()
}

/// CVT の計算 (drm_cvt_mode。Linux の整数の計算をそのまま)
pub fn cvt(hdisplay: u32, vdisplay: u32, vrefresh: u32, reduced: bool, interlaced: bool, margins: bool) -> Option<Mode> {
    const HV: i64 = 1000;
    const GRAN: i64 = 8;
    const MIN_V_PORCH: i64 = 3;
    const MIN_V_BPORCH: i64 = 6;
    if hdisplay == 0 || vdisplay == 0 {
        return None;
    }
    let (hdisplay, vdisplay) = (hdisplay as i64, vdisplay as i64);
    let vrefresh = if vrefresh == 0 { 60 } else { vrefresh as i64 };
    let vfieldrate = if interlaced { vrefresh * 2 } else { vrefresh };
    let hdisplay_rnd = hdisplay - hdisplay % GRAN;
    let mut hmargin = 0;
    if margins {
        hmargin = hdisplay_rnd * 18 / 1000;
        hmargin -= hmargin % GRAN;
    }
    let hd = hdisplay_rnd + 2 * hmargin;
    let vdisplay_rnd = if interlaced { vdisplay / 2 } else { vdisplay };
    let vmargin = if margins { vdisplay_rnd * 18 / 1000 } else { 0 };
    let vd = vdisplay + 2 * vmargin;
    let interlace = interlaced as i64;
    let vsync = if vdisplay % 3 == 0 && vdisplay * 4 / 3 == hdisplay {
        4
    } else if vdisplay % 9 == 0 && vdisplay * 16 / 9 == hdisplay {
        5
    } else if vdisplay % 10 == 0 && vdisplay * 16 / 10 == hdisplay {
        6
    } else if (vdisplay % 4 == 0 && vdisplay * 5 / 4 == hdisplay) || (vdisplay % 9 == 0 && vdisplay * 15 / 9 == hdisplay) {
        7
    } else {
        10
    };
    let (hperiod, vt, ht, hss, hse, vss, vse);
    if !reduced {
        // u32 の計算 (unsigned int の hperiod と hblank_percentage)
        let tmp1 = (HV * 1_000_000 - 550 * HV * vfieldrate) as i32 as i64;
        let tmp2 = (vdisplay_rnd + 2 * vmargin + MIN_V_PORCH) * 2 + interlace;
        hperiod = ((tmp1 * 2) as i32 as i64 / (tmp2 * vfieldrate)) as u32 as i64;
        if hperiod == 0 {
            return None;
        }
        let t = 550 * HV / hperiod + 1;
        let vsyncandback_porch = if t < vsync + MIN_V_PORCH { vsync + MIN_V_PORCH } else { t };
        vt = vdisplay_rnd + 2 * vmargin + vsyncandback_porch + MIN_V_PORCH;
        let m_prime = 600 * 128 / 256;
        let c_prime = (40 - 20) * 128 / 256 + 20;
        let mut hblank_percentage = (c_prime * HV) as u32 as i64 - (m_prime * hperiod / 1000);
        hblank_percentage = hblank_percentage as u32 as i64;
        if hblank_percentage < 20 * HV {
            hblank_percentage = 20 * HV;
        }
        let mut hblank = (hd * hblank_percentage / (100 * HV - hblank_percentage)) as i32 as i64;
        hblank -= hblank % (2 * GRAN);
        ht = hd + hblank;
        hse = hd + hblank / 2;
        let mut s = hse - ht * 8 / 100;
        s += GRAN - s % GRAN;
        hss = s;
        vss = vd + MIN_V_PORCH;
        vse = vss + vsync;
    } else {
        let tmp1 = HV * 1_000_000 - 460 * HV * vfieldrate;
        let tmp2 = vdisplay_rnd + 2 * vmargin;
        hperiod = (tmp1 / (tmp2 * vfieldrate)) as u32 as i64;
        if hperiod == 0 {
            return None;
        }
        let mut vbilines = 460 * HV / hperiod + 1;
        if vbilines < 3 + vsync + MIN_V_BPORCH {
            vbilines = 3 + vsync + MIN_V_BPORCH;
        }
        vt = vdisplay_rnd + 2 * vmargin + vbilines;
        ht = hd + 160;
        hse = hd + 80;
        hss = hse - 32;
        vss = vd + 3;
        vse = vss + vsync;
    }
    let clock = (ht as u64 * (HV as u64 * 1000) / hperiod as u64) as u32;
    let mut md = Mode { clock, hd: hd as u16, hss: hss as u16, hse: hse as u16, ht: ht as u16, hsk: 0, vd: vd as u16, vss: vss as u16, vse: vse as u16, vt: vt as u16, vs: 0, flags: 0, typ: 0, ar: 0 };
    if interlaced {
        md.vt *= 2;
        md.flags |= FLAG_INTERLACE;
    }
    md.flags |= if reduced { FLAG_PHSYNC | FLAG_NVSYNC } else { FLAG_PVSYNC | FLAG_NHSYNC };
    Some(md)
}

/// VIC の CTA のモード (cea_mode_for_vic)
fn cea_mode(vic: u8) -> Option<&'static Mode> {
    match vic {
        1..=127 => CEA_1.get(vic as usize - 1),
        193..=219 => CEA_193.get(vic as usize - 193),
        _ => None,
    }
}

fn cea_vics() -> impl Iterator<Item = u8> {
    (1..=127u8).chain(193..=219u8)
}

/// 60 Hz と 59.94 Hz の、もう一方の時計 (cea_mode_alternate_clock)
fn cea_alternate_clock(c: &Mode) -> u32 {
    let clock = c.clock as u64;
    if c.vrefresh() % 6 != 0 {
        return c.clock;
    }
    // 表は 240 と 480 の行のものは 59.94 Hz、ほかは 60 Hz のもの
    if c.vd == 240 || c.vd == 480 { ((clock * 1001 + 500) / 1000) as u32 } else { ((clock * 1000 + 500) / 1001) as u32 }
}

/// VIC によっては縦のフロントポーチが 1、2 行のびてもよい (cea_mode_alternate_timings)
fn cea_alternate_timings(vic: u8, md: &mut Mode) -> bool {
    if (matches!(vic, 8 | 9 | 12 | 13) && md.vt < 263) || (matches!(vic, 23 | 24 | 27 | 28) && md.vt < 314) {
        md.vss += 1;
        md.vse += 1;
        md.vt += 1;
        return true;
    }
    false
}

/// CTA のモードと同じか (drm_match_cea_mode と、時計をゆるめた drm_match_cea_mode_clock_tolerance)。VIC か 0
fn match_cea(md: &Mode, tolerance: Option<u32>) -> u8 {
    if md.clock == 0 {
        return 0;
    }
    for vic in cea_vics() {
        let mut c = *cea_mode(vic).unwrap();
        let (c1, c2) = (c.clock, cea_alternate_clock(&c));
        let near = match tolerance {
            None => picos(md.clock) == picos(c1) || picos(md.clock) == picos(c2),
            Some(t) => md.clock.abs_diff(c1) <= t || md.clock.abs_diff(c2) <= t,
        };
        if !near {
            continue;
        }
        loop {
            if md.timings_eq(&c) && md.flags_eq(&c) && (md.ar == 0 || md.ar == c.ar) {
                return vic;
            }
            if !cea_alternate_timings(vic, &mut c) {
                break;
            }
        }
    }
    0
}

/// EDID (はじめの 128 バイトと拡張) の読みとり
struct Edid<'a> {
    b: &'a [u8],
    /// 拡張のブロック (チェックサムの合うもの)
    ext: Vec<&'a [u8]>,
}

impl<'a> Edid<'a> {
    fn parse(b: &'a [u8]) -> Option<Edid<'a>> {
        if b.len() < 128 || b[..8] != [0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0] || b[..128].iter().fold(0u8, |s, &x| s.wrapping_add(x)) != 0 {
            return None;
        }
        let ext = (1..=b[126] as usize)
            .filter_map(|i| b.get(i * 128..i * 128 + 128))
            .filter(|e| e.iter().fold(0u8, |s, &x| s.wrapping_add(x)) == 0)
            .collect();
        Some(Edid { b, ext })
    }

    fn revision(&self) -> u8 {
        self.b[19]
    }

    /// 18 バイトの記述 (はじめのブロックの 4 つと、CTA の拡張の DTD: drm_for_each_detailed_block)
    fn descriptors(&self) -> Vec<&'a [u8]> {
        let mut v: Vec<&[u8]> = (0..4).map(|i| &self.b[54 + i * 18..72 + i * 18]).collect();
        for e in &self.ext {
            if e[0] == 0x02 {
                let d = e[2] as usize;
                if (4..=127).contains(&d) {
                    for i in 0..(127 - d) / 18 {
                        v.push(&e[d + 18 * i..d + 18 * i + 18]);
                    }
                }
            }
        }
        v
    }

    /// 表示の記述 (タイミングでないもの) で型が t
    fn display(d: &[u8], t: u8) -> bool {
        d[0] == 0 && d[1] == 0 && d[2] == 0 && d[3] == t
    }

    fn has_cta(&self) -> bool {
        self.ext.iter().any(|e| e[0] == 0x02)
    }

    /// CTA の映像のブロック (タグ 2) の VIC
    fn vics(&self) -> Vec<u8> {
        let mut v = Vec::new();
        for e in self.ext.iter().filter(|e| e[0] == 0x02 && e[1] >= 3 && (4..=127).contains(&(e[2] as usize))) {
            let end = e[2] as usize;
            let mut i = 4;
            while i < end {
                let (tag, len) = (e[i] >> 5, (e[i] & 0x1f) as usize);
                if i + 1 + len > end {
                    break;
                }
                if tag == 2 {
                    for &svd in &e[i + 1..i + 1 + len] {
                        // 0〜6 ビットが VIC、7 ビット目はネイティブの印 (svd_to_vic)
                        v.push(if (1..=64).contains(&svd) || (129..=192).contains(&svd) { svd & 127 } else { svd });
                    }
                }
                i += 1 + len;
            }
        }
        v
    }

    /// 範囲の記述が縮めたブランキングを言っているか (drm_monitor_supports_rb)
    fn supports_rb(&self) -> bool {
        if self.revision() >= 4 {
            return self.descriptors().iter().any(|d| Self::display(d, 0xfd) && d[10] == 0x04 && d[15] & 0x10 != 0);
        }
        self.b[20] & 0x80 != 0
    }

    /// 標準タイミングの作りかた (standard_timing_level): 1.4 は範囲の記述しだい (なければ CVT)
    fn timing_level(&self) -> u8 {
        const DMT_: u8 = 0;
        const GTF: u8 = 1;
        const GTF2: u8 = 2;
        const CVT: u8 = 3;
        if self.revision() >= 4 {
            let mut r = CVT;
            for d in self.descriptors() {
                if Self::display(d, 0xfd) {
                    match d[10] {
                        0x00 => r = GTF,
                        0x02 => r = GTF2,
                        0x04 => r = CVT,
                        _ => {}
                    }
                }
            }
            r
        } else if self.revision() >= 2 {
            GTF
        } else {
            DMT_
        }
    }
}

/// 詳しいタイミング (drm_mode_detailed)
fn detailed(d: &[u8]) -> Option<Mode> {
    let hactive = ((d[4] as u32 & 0xf0) << 4) | d[2] as u32;
    let vactive = ((d[7] as u32 & 0xf0) << 4) | d[5] as u32;
    let hblank = ((d[4] as u32 & 0xf) << 8) | d[3] as u32;
    let vblank = ((d[7] as u32 & 0xf) << 8) | d[6] as u32;
    let hso = ((d[11] as u32 & 0xc0) << 2) | d[8] as u32;
    let hsw = ((d[11] as u32 & 0x30) << 4) | d[9] as u32;
    let vso = ((d[11] as u32 & 0xc) << 2) | (d[10] as u32 >> 4);
    let vsw = ((d[11] as u32 & 0x3) << 4) | (d[10] as u32 & 0xf);
    let misc = d[17];
    // 小さすぎるもの、立体 (stereo)、同期の幅が 0 のものは使わない
    if hactive < 64 || vactive < 64 || misc & (1 << 5) != 0 || hsw == 0 || vsw == 0 {
        return None;
    }
    let mut md = Mode {
        clock: u16::from_le_bytes([d[0], d[1]]) as u32 * 10,
        hd: hactive as u16,
        hss: (hactive + hso) as u16,
        hse: (hactive + hso + hsw) as u16,
        ht: (hactive + hblank) as u16,
        hsk: 0,
        vd: vactive as u16,
        vss: (vactive + vso) as u16,
        vse: (vactive + vso + vsw) as u16,
        vt: (vactive + vblank) as u16,
        vs: 0,
        flags: 0,
        typ: TYPE_DRIVER,
        ar: 0,
    };
    md.hse = md.hse.min(md.ht);
    md.vse = md.vse.min(md.vt);
    // インターレース: CTA のもので、場 (field) の高さで書いてあるものは 2 倍に (drm_mode_do_interlace_quirk)
    if misc & (1 << 7) != 0 {
        for (w, h) in [(1920, 1080), (720, 480), (1440, 480), (2880, 480), (720, 576), (1440, 576), (2880, 576)] {
            if md.hd == w && md.vd == h / 2 {
                md.vd *= 2;
                md.vss *= 2;
                md.vse *= 2;
                md.vt = md.vt * 2 | 1;
            }
        }
        md.flags |= FLAG_INTERLACE;
    }
    md.flags |= if misc & (1 << 1) != 0 { FLAG_PHSYNC } else { FLAG_NHSYNC };
    md.flags |= if misc & (1 << 2) != 0 { FLAG_PVSYNC } else { FLAG_NVSYNC };
    // 10 kHz きざみなので、CTA のものに近ければその時計に (fixup_detailed_cea_mode_clock)
    let vic = match_cea(&md, Some(5));
    if let Some(c) = cea_mode(vic) {
        let (c1, c2) = (c.clock, cea_alternate_clock(c));
        md.clock = if md.clock.abs_diff(c1) < md.clock.abs_diff(c2) { c1 } else { c2 };
    }
    Some(md)
}

/// 標準タイミング 2 バイト (drm_mode_std)。probed はそれまでに集めたもの
fn standard(e: &Edid, a: u8, b: u8, probed: &[Mode]) -> Option<Mode> {
    if (a == 0 && b == 0) || (a == 1 && b == 1) || (a == 0x20 && b == 0x20) {
        return None;
    }
    let mut h = a as u32 * 8 + 248;
    let r = (b & 0x3f) as u32 + 60;
    let mut v = match b >> 6 {
        0 if e.revision() < 3 => h,
        0 => h * 10 / 16,
        1 => h * 3 / 4,
        2 => h * 4 / 5,
        _ => h * 9 / 16,
    };
    if r == 60 && ((h == 1360 && v == 765) || (h == 1368 && v == 769)) {
        h = 1366;
        v = 768;
    }
    // 詳しいタイミングなどで、もうその大きさとリフレッシュのものがあれば、それを使う
    if probed.iter().any(|p| p.hd as u32 == h && p.vd as u32 == v && p.vrefresh() == r) {
        return None;
    }
    if h == 1366 && v == 768 && r == 60 {
        let mut md = cvt(1366, 768, r, false, false, false)?;
        md.hd = 1366;
        md.hss -= 1;
        md.hse -= 1;
        return Some(md);
    }
    if e.supports_rb() {
        if let Some(md) = find_dmt(h, v, r, true) {
            return Some(md);
        }
    }
    if let Some(md) = find_dmt(h, v, r, false) {
        return Some(md);
    }
    // GTF はまだ (CVT のものだけ計算する)
    if e.timing_level() == 3 { cvt(h, v, r, false, false, false) } else { None }
}

/// EDID のモード (_drm_edid_connector_add_modes の順)
fn edid_modes(e: &Edid) -> Vec<Mode> {
    let mut p: Vec<Mode> = Vec::new();
    // 詳しいタイミング (1.4 は、はじめのものがいつも preferred)
    let mut preferred = e.revision() >= 4 || e.b[24] & 0x02 != 0;
    for d in e.descriptors() {
        if d[0] == 0 && d[1] == 0 {
            continue;
        }
        if let Some(mut md) = detailed(d) {
            if preferred {
                md.typ |= TYPE_PREFERRED;
            }
            p.push(md);
            preferred = false;
        }
    }
    // CVT の 3 バイトのもの (Linux と同じく、j は 1 から: 縮めたブランキングのものは作らない)
    if e.revision() >= 3 {
        for d in e.descriptors().into_iter().filter(|d| Edid::display(d, 0xf8)) {
            for c in d[6..18].chunks(3) {
                if c == [0, 0, 0] {
                    continue;
                }
                let height = (c[0] as u32 + ((c[1] as u32 & 0xf0) << 4) + 1) * 2;
                let width = match c[1] & 0x0c {
                    0x04 => height * 16 / 9,
                    0x08 => height * 16 / 10,
                    0x0c => height * 15 / 9,
                    _ => height * 4 / 3,
                };
                for (j, rate) in [60, 85, 75, 60, 50].into_iter().enumerate().skip(1) {
                    if c[2] & (1 << j) != 0 {
                        if let Some(mut md) = cvt(width, height, rate, false, false, false) {
                            md.typ = 0;
                            p.push(md);
                        }
                    }
                }
            }
        }
    }
    // 標準タイミング (8 つと、記述 0xfa の 6 つずつ)
    for i in 0..8 {
        if let Some(md) = standard(e, e.b[38 + i * 2], e.b[39 + i * 2], &p) {
            p.push(md);
        }
    }
    if e.revision() >= 1 {
        for d in e.descriptors().into_iter().filter(|d| Edid::display(d, 0xfa)) {
            for i in 0..6 {
                if let Some(md) = standard(e, d[5 + i * 2], d[6 + i * 2], &p) {
                    p.push(md);
                }
            }
        }
    }
    // 確立タイミング I/II (17 ビット) と III (記述 0xf7)
    let bits = e.b[35] as u32 | (e.b[36] as u32) << 8 | ((e.b[37] as u32 & 0x80) << 9);
    for (i, md) in EST.iter().enumerate() {
        if bits & (1 << i) != 0 {
            p.push(*md);
        }
    }
    if e.revision() >= 1 {
        for d in e.descriptors().into_iter().filter(|d| Edid::display(d, 0xf7)) {
            for i in 0..6 {
                for j in (0..8).rev() {
                    let k = i * 8 + (7 - j);
                    if k >= EST3.len() {
                        break;
                    }
                    if d[6 + i] & (1 << j) != 0 {
                        let (w, h, r, rb) = EST3[k];
                        if let Some(md) = find_dmt(w as u32, h as u32, r as u32, rb) {
                            p.push(md);
                        }
                    }
                }
            }
        }
    }
    // CTA の VIC
    for vic in e.vics() {
        if let Some(c) = cea_mode(vic) {
            p.push(*c);
        }
    }
    // 60 Hz と 59.94 Hz の兄弟 (CTA の拡張があるときだけ)
    if e.has_cta() {
        let mut add = Vec::new();
        for md in &p {
            let Some(c) = cea_mode(match_cea(md, None)) else { continue };
            let (c1, c2) = (c.clock, cea_alternate_clock(c));
            if c1 == c2 || (md.clock != c1 && md.clock != c2) {
                continue;
            }
            let mut n = *c;
            n.flags |= md.flags & FLAG_3D_MASK;
            n.clock = if md.clock != c1 { c1 } else { c2 };
            add.push(n);
        }
        p.extend(add);
    }
    p
}

/// コネクタのモードの一覧 (virtio_gpu_conn_get_modes と drm_helper_probe_single_connector_modes)。
/// edid は GET_EDID の答え (なければ空)、(w, h) は GET_DISPLAY_INFO の画面の大きさ
pub fn modes(edid: &[u8], w: u32, h: u32) -> Vec<Mode> {
    let mut p = Edid::parse(edid).map(|e| edid_modes(&e)).unwrap_or_default();
    if p.is_empty() {
        // EDID がない: DMT の 61 Hz までのものと、画面の大きさの CVT (preferred)
        p.extend(DMT.iter().filter(|d| d.hd as u32 <= MAX && d.vd as u32 <= MAX && d.vrefresh() <= 61).copied());
        if w == 0 || h == 0 {
            for md in p.iter_mut().filter(|md| (md.hd, md.vd) == DEF) {
                md.typ |= TYPE_PREFERRED;
            }
        } else if let Some(mut md) = cvt(w, h, 60, false, false, false) {
            md.typ |= TYPE_PREFERRED;
            p.push(md);
        }
    }
    // 同じものをまとめる (drm_connector_list_update)
    let mut list: Vec<Mode> = Vec::new();
    for pm in p {
        match list.iter_mut().find(|md| md.equal(&pm)) {
            Some(md) if md.typ & TYPE_PREFERRED == 0 && pm.typ & TYPE_PREFERRED != 0 => {
                let typ = pm.typ | md.typ;
                *md = pm;
                md.typ = typ;
            }
            Some(md) => md.typ |= pm.typ,
            None => list.push(pm),
        }
    }
    // たしかめる: 形、大きさ、インターレースと二重走査と立体はなし、preferred は画面の大きさの 16 以内か 1024x768
    let (dw, dh) = if w == 0 || h == 0 { (DEF.0 as u32, DEF.1 as u32) } else { (w, h) };
    list.retain(|md| {
        md.valid()
            && md.hd as u32 <= MAX
            && md.vd as u32 <= MAX
            && md.flags & (FLAG_INTERLACE | FLAG_DBLSCAN | FLAG_3D_MASK) == 0
            && (md.typ & TYPE_PREFERRED == 0 || (md.hd, md.vd) == DEF || (md.hd as u32 <= dw && md.hd as u32 + 16 >= dw && md.vd as u32 <= dh && md.vd as u32 + 16 >= dh))
    });
    // ならべる (drm_mode_compare: preferred、面積、リフレッシュ、時計の大きい順。同じなら前のまま)
    list.sort_by(|a, b| {
        let key = |m: &Mode| ((m.typ & TYPE_PREFERRED != 0) as i64, m.hd as i64 * m.vd as i64, m.vrefresh() as i64, m.clock as i64);
        key(b).cmp(&key(a))
    });
    list
}

/// ユーザーに見せるもの: 縦横比だけがちがうものは、はじめの 1 つ (drm_mode_expose_to_userspace: ASPECT_RATIO の cap の
/// ないファイル)
pub fn exposed(list: &[Mode]) -> Vec<Mode> {
    let mut out: Vec<Mode> = Vec::new();
    for md in list {
        if !out.iter().any(|o| o.timings_eq(md) && o.clock_eq(md) && o.flags == md.flags) {
            out.push(*md);
        }
    }
    out
}

/// 画面の大きさ (mm: EDID の 21、22 バイト目の cm)
pub fn size_mm(edid: &[u8]) -> (u32, u32) {
    match Edid::parse(edid) {
        Some(e) => (e.b[21] as u32 * 10, e.b[22] as u32 * 10),
        None => (0, 0),
    }
}

// ここから下は Linux 6.18 の drivers/gpu/drm/drm_edid.c の表 (drm_dmt_modes、edid_est_modes、est3_modes、
// edid_cea_modes_1、edid_cea_modes_193)。m(clock, hdisplay, hsync_start, hsync_end, htotal, hskew, vdisplay, vsync_start,
// vsync_end, vtotal, vscan, flags, picture_aspect_ratio)
const DMT: [M; 88] = [
    m(31500, 640, 672, 736, 832, 0, 350, 382, 385, 445, 0, 9, 0),
    m(31500, 640, 672, 736, 832, 0, 400, 401, 404, 445, 0, 6, 0),
    m(35500, 720, 756, 828, 936, 0, 400, 401, 404, 446, 0, 6, 0),
    m(25175, 640, 656, 752, 800, 0, 480, 490, 492, 525, 0, 10, 0),
    m(31500, 640, 664, 704, 832, 0, 480, 489, 492, 520, 0, 10, 0),
    m(31500, 640, 656, 720, 840, 0, 480, 481, 484, 500, 0, 10, 0),
    m(36000, 640, 696, 752, 832, 0, 480, 481, 484, 509, 0, 10, 0),
    m(36000, 800, 824, 896, 1024, 0, 600, 601, 603, 625, 0, 5, 0),
    m(40000, 800, 840, 968, 1056, 0, 600, 601, 605, 628, 0, 5, 0),
    m(50000, 800, 856, 976, 1040, 0, 600, 637, 643, 666, 0, 5, 0),
    m(49500, 800, 816, 896, 1056, 0, 600, 601, 604, 625, 0, 5, 0),
    m(56250, 800, 832, 896, 1048, 0, 600, 601, 604, 631, 0, 5, 0),
    m(73250, 800, 848, 880, 960, 0, 600, 603, 607, 636, 0, 9, 0),
    m(33750, 848, 864, 976, 1088, 0, 480, 486, 494, 517, 0, 5, 0),
    m(44900, 1024, 1032, 1208, 1264, 0, 768, 768, 776, 817, 0, 21, 0),
    m(65000, 1024, 1048, 1184, 1344, 0, 768, 771, 777, 806, 0, 10, 0),
    m(75000, 1024, 1048, 1184, 1328, 0, 768, 771, 777, 806, 0, 10, 0),
    m(78750, 1024, 1040, 1136, 1312, 0, 768, 769, 772, 800, 0, 5, 0),
    m(94500, 1024, 1072, 1168, 1376, 0, 768, 769, 772, 808, 0, 5, 0),
    m(115500, 1024, 1072, 1104, 1184, 0, 768, 771, 775, 813, 0, 9, 0),
    m(108000, 1152, 1216, 1344, 1600, 0, 864, 865, 868, 900, 0, 5, 0),
    m(74250, 1280, 1390, 1430, 1650, 0, 720, 725, 730, 750, 0, 5, 0),
    m(68250, 1280, 1328, 1360, 1440, 0, 768, 771, 778, 790, 0, 9, 0),
    m(79500, 1280, 1344, 1472, 1664, 0, 768, 771, 778, 798, 0, 6, 0),
    m(102250, 1280, 1360, 1488, 1696, 0, 768, 771, 778, 805, 0, 6, 0),
    m(117500, 1280, 1360, 1496, 1712, 0, 768, 771, 778, 809, 0, 6, 0),
    m(140250, 1280, 1328, 1360, 1440, 0, 768, 771, 778, 813, 0, 9, 0),
    m(71000, 1280, 1328, 1360, 1440, 0, 800, 803, 809, 823, 0, 9, 0),
    m(83500, 1280, 1352, 1480, 1680, 0, 800, 803, 809, 831, 0, 6, 0),
    m(106500, 1280, 1360, 1488, 1696, 0, 800, 803, 809, 838, 0, 6, 0),
    m(122500, 1280, 1360, 1496, 1712, 0, 800, 803, 809, 843, 0, 6, 0),
    m(146250, 1280, 1328, 1360, 1440, 0, 800, 803, 809, 847, 0, 9, 0),
    m(108000, 1280, 1376, 1488, 1800, 0, 960, 961, 964, 1000, 0, 5, 0),
    m(148500, 1280, 1344, 1504, 1728, 0, 960, 961, 964, 1011, 0, 5, 0),
    m(175500, 1280, 1328, 1360, 1440, 0, 960, 963, 967, 1017, 0, 9, 0),
    m(108000, 1280, 1328, 1440, 1688, 0, 1024, 1025, 1028, 1066, 0, 5, 0),
    m(135000, 1280, 1296, 1440, 1688, 0, 1024, 1025, 1028, 1066, 0, 5, 0),
    m(157500, 1280, 1344, 1504, 1728, 0, 1024, 1025, 1028, 1072, 0, 5, 0),
    m(187250, 1280, 1328, 1360, 1440, 0, 1024, 1027, 1034, 1084, 0, 9, 0),
    m(85500, 1360, 1424, 1536, 1792, 0, 768, 771, 777, 795, 0, 5, 0),
    m(148250, 1360, 1408, 1440, 1520, 0, 768, 771, 776, 813, 0, 9, 0),
    m(85500, 1366, 1436, 1579, 1792, 0, 768, 771, 774, 798, 0, 5, 0),
    m(72000, 1366, 1380, 1436, 1500, 0, 768, 769, 772, 800, 0, 5, 0),
    m(101000, 1400, 1448, 1480, 1560, 0, 1050, 1053, 1057, 1080, 0, 9, 0),
    m(121750, 1400, 1488, 1632, 1864, 0, 1050, 1053, 1057, 1089, 0, 6, 0),
    m(156000, 1400, 1504, 1648, 1896, 0, 1050, 1053, 1057, 1099, 0, 6, 0),
    m(179500, 1400, 1504, 1656, 1912, 0, 1050, 1053, 1057, 1105, 0, 6, 0),
    m(208000, 1400, 1448, 1480, 1560, 0, 1050, 1053, 1057, 1112, 0, 9, 0),
    m(88750, 1440, 1488, 1520, 1600, 0, 900, 903, 909, 926, 0, 9, 0),
    m(106500, 1440, 1520, 1672, 1904, 0, 900, 903, 909, 934, 0, 6, 0),
    m(136750, 1440, 1536, 1688, 1936, 0, 900, 903, 909, 942, 0, 6, 0),
    m(157000, 1440, 1544, 1696, 1952, 0, 900, 903, 909, 948, 0, 6, 0),
    m(182750, 1440, 1488, 1520, 1600, 0, 900, 903, 909, 953, 0, 9, 0),
    m(108000, 1600, 1624, 1704, 1800, 0, 900, 901, 904, 1000, 0, 5, 0),
    m(162000, 1600, 1664, 1856, 2160, 0, 1200, 1201, 1204, 1250, 0, 5, 0),
    m(175500, 1600, 1664, 1856, 2160, 0, 1200, 1201, 1204, 1250, 0, 5, 0),
    m(189000, 1600, 1664, 1856, 2160, 0, 1200, 1201, 1204, 1250, 0, 5, 0),
    m(202500, 1600, 1664, 1856, 2160, 0, 1200, 1201, 1204, 1250, 0, 5, 0),
    m(229500, 1600, 1664, 1856, 2160, 0, 1200, 1201, 1204, 1250, 0, 5, 0),
    m(268250, 1600, 1648, 1680, 1760, 0, 1200, 1203, 1207, 1271, 0, 9, 0),
    m(119000, 1680, 1728, 1760, 1840, 0, 1050, 1053, 1059, 1080, 0, 9, 0),
    m(146250, 1680, 1784, 1960, 2240, 0, 1050, 1053, 1059, 1089, 0, 6, 0),
    m(187000, 1680, 1800, 1976, 2272, 0, 1050, 1053, 1059, 1099, 0, 6, 0),
    m(214750, 1680, 1808, 1984, 2288, 0, 1050, 1053, 1059, 1105, 0, 6, 0),
    m(245500, 1680, 1728, 1760, 1840, 0, 1050, 1053, 1059, 1112, 0, 9, 0),
    m(204750, 1792, 1920, 2120, 2448, 0, 1344, 1345, 1348, 1394, 0, 6, 0),
    m(261000, 1792, 1888, 2104, 2456, 0, 1344, 1345, 1348, 1417, 0, 6, 0),
    m(333250, 1792, 1840, 1872, 1952, 0, 1344, 1347, 1351, 1423, 0, 9, 0),
    m(218250, 1856, 1952, 2176, 2528, 0, 1392, 1393, 1396, 1439, 0, 6, 0),
    m(288000, 1856, 1984, 2208, 2560, 0, 1392, 1393, 1396, 1500, 0, 6, 0),
    m(356500, 1856, 1904, 1936, 2016, 0, 1392, 1395, 1399, 1474, 0, 9, 0),
    m(148500, 1920, 2008, 2052, 2200, 0, 1080, 1084, 1089, 1125, 0, 10, 0),
    m(154000, 1920, 1968, 2000, 2080, 0, 1200, 1203, 1209, 1235, 0, 9, 0),
    m(193250, 1920, 2056, 2256, 2592, 0, 1200, 1203, 1209, 1245, 0, 6, 0),
    m(245250, 1920, 2056, 2264, 2608, 0, 1200, 1203, 1209, 1255, 0, 6, 0),
    m(281250, 1920, 2064, 2272, 2624, 0, 1200, 1203, 1209, 1262, 0, 6, 0),
    m(317000, 1920, 1968, 2000, 2080, 0, 1200, 1203, 1209, 1271, 0, 9, 0),
    m(234000, 1920, 2048, 2256, 2600, 0, 1440, 1441, 1444, 1500, 0, 6, 0),
    m(297000, 1920, 2064, 2288, 2640, 0, 1440, 1441, 1444, 1500, 0, 6, 0),
    m(380500, 1920, 1968, 2000, 2080, 0, 1440, 1443, 1447, 1525, 0, 9, 0),
    m(162000, 2048, 2074, 2154, 2250, 0, 1152, 1153, 1156, 1200, 0, 5, 0),
    m(268500, 2560, 2608, 2640, 2720, 0, 1600, 1603, 1609, 1646, 0, 9, 0),
    m(348500, 2560, 2752, 3032, 3504, 0, 1600, 1603, 1609, 1658, 0, 6, 0),
    m(443250, 2560, 2768, 3048, 3536, 0, 1600, 1603, 1609, 1672, 0, 6, 0),
    m(505250, 2560, 2768, 3048, 3536, 0, 1600, 1603, 1609, 1682, 0, 6, 0),
    m(552750, 2560, 2608, 2640, 2720, 0, 1600, 1603, 1609, 1694, 0, 9, 0),
    m(556744, 4096, 4104, 4136, 4176, 0, 2160, 2208, 2216, 2222, 0, 9, 0),
    m(556188, 4096, 4104, 4136, 4176, 0, 2160, 2208, 2216, 2222, 0, 9, 0),
];

const EST: [M; 17] = [
    m(40000, 800, 840, 968, 1056, 0, 600, 601, 605, 628, 0, 5, 0),
    m(36000, 800, 824, 896, 1024, 0, 600, 601, 603, 625, 0, 5, 0),
    m(31500, 640, 656, 720, 840, 0, 480, 481, 484, 500, 0, 10, 0),
    m(31500, 640, 664, 704, 832, 0, 480, 489, 492, 520, 0, 10, 0),
    m(30240, 640, 704, 768, 864, 0, 480, 483, 486, 525, 0, 10, 0),
    m(25175, 640, 656, 752, 800, 0, 480, 490, 492, 525, 0, 10, 0),
    m(35500, 720, 738, 846, 900, 0, 400, 421, 423, 449, 0, 10, 0),
    m(28320, 720, 738, 846, 900, 0, 400, 412, 414, 449, 0, 6, 0),
    m(135000, 1280, 1296, 1440, 1688, 0, 1024, 1025, 1028, 1066, 0, 5, 0),
    m(78750, 1024, 1040, 1136, 1312, 0, 768, 769, 772, 800, 0, 5, 0),
    m(75000, 1024, 1048, 1184, 1328, 0, 768, 771, 777, 806, 0, 10, 0),
    m(65000, 1024, 1048, 1184, 1344, 0, 768, 771, 777, 806, 0, 10, 0),
    m(44900, 1024, 1032, 1208, 1264, 0, 768, 768, 776, 817, 0, 21, 0),
    m(57284, 832, 864, 928, 1152, 0, 624, 625, 628, 667, 0, 10, 0),
    m(49500, 800, 816, 896, 1056, 0, 600, 601, 604, 625, 0, 5, 0),
    m(50000, 800, 856, 976, 1040, 0, 600, 637, 643, 666, 0, 5, 0),
    m(108000, 1152, 1216, 1344, 1600, 0, 864, 865, 868, 900, 0, 5, 0),
];

const EST3: [(u16, u16, u16, bool); 44] = [(640, 350, 85, false), (640, 400, 85, false), (720, 400, 85, false), (640, 480, 85, false), (848, 480, 60, false), (800, 600, 85, false), (1024, 768, 85, false), (1152, 864, 75, false), (1280, 768, 60, true), (1280, 768, 60, false), (1280, 768, 75, false), (1280, 768, 85, false), (1280, 960, 60, false), (1280, 960, 85, false), (1280, 1024, 60, false), (1280, 1024, 85, false), (1360, 768, 60, false), (1440, 900, 60, true), (1440, 900, 60, false), (1440, 900, 75, false), (1440, 900, 85, false), (1400, 1050, 60, true), (1400, 1050, 60, false), (1400, 1050, 75, false), (1400, 1050, 85, false), (1680, 1050, 60, true), (1680, 1050, 60, false), (1680, 1050, 75, false), (1680, 1050, 85, false), (1600, 1200, 60, false), (1600, 1200, 65, false), (1600, 1200, 70, false), (1600, 1200, 75, false), (1600, 1200, 85, false), (1792, 1344, 60, false), (1792, 1344, 75, false), (1856, 1392, 60, false), (1856, 1392, 75, false), (1920, 1200, 60, true), (1920, 1200, 60, false), (1920, 1200, 75, false), (1920, 1200, 85, false), (1920, 1440, 60, false), (1920, 1440, 75, false)];

const CEA_1: [M; 127] = [
    m(25175, 640, 656, 752, 800, 0, 480, 490, 492, 525, 0, 10, 1),
    m(27000, 720, 736, 798, 858, 0, 480, 489, 495, 525, 0, 10, 1),
    m(27000, 720, 736, 798, 858, 0, 480, 489, 495, 525, 0, 10, 2),
    m(74250, 1280, 1390, 1430, 1650, 0, 720, 725, 730, 750, 0, 5, 2),
    m(74250, 1920, 2008, 2052, 2200, 0, 1080, 1084, 1094, 1125, 0, 21, 2),
    m(13500, 720, 739, 801, 858, 0, 480, 488, 494, 525, 0, 4122, 1),
    m(13500, 720, 739, 801, 858, 0, 480, 488, 494, 525, 0, 4122, 2),
    m(13500, 720, 739, 801, 858, 0, 240, 244, 247, 262, 0, 4106, 1),
    m(13500, 720, 739, 801, 858, 0, 240, 244, 247, 262, 0, 4106, 2),
    m(54000, 2880, 2956, 3204, 3432, 0, 480, 488, 494, 525, 0, 26, 1),
    m(54000, 2880, 2956, 3204, 3432, 0, 480, 488, 494, 525, 0, 26, 2),
    m(54000, 2880, 2956, 3204, 3432, 0, 240, 244, 247, 262, 0, 10, 1),
    m(54000, 2880, 2956, 3204, 3432, 0, 240, 244, 247, 262, 0, 10, 2),
    m(54000, 1440, 1472, 1596, 1716, 0, 480, 489, 495, 525, 0, 10, 1),
    m(54000, 1440, 1472, 1596, 1716, 0, 480, 489, 495, 525, 0, 10, 2),
    m(148500, 1920, 2008, 2052, 2200, 0, 1080, 1084, 1089, 1125, 0, 5, 2),
    m(27000, 720, 732, 796, 864, 0, 576, 581, 586, 625, 0, 10, 1),
    m(27000, 720, 732, 796, 864, 0, 576, 581, 586, 625, 0, 10, 2),
    m(74250, 1280, 1720, 1760, 1980, 0, 720, 725, 730, 750, 0, 5, 2),
    m(74250, 1920, 2448, 2492, 2640, 0, 1080, 1084, 1094, 1125, 0, 21, 2),
    m(13500, 720, 732, 795, 864, 0, 576, 580, 586, 625, 0, 4122, 1),
    m(13500, 720, 732, 795, 864, 0, 576, 580, 586, 625, 0, 4122, 2),
    m(13500, 720, 732, 795, 864, 0, 288, 290, 293, 312, 0, 4106, 1),
    m(13500, 720, 732, 795, 864, 0, 288, 290, 293, 312, 0, 4106, 2),
    m(54000, 2880, 2928, 3180, 3456, 0, 576, 580, 586, 625, 0, 26, 1),
    m(54000, 2880, 2928, 3180, 3456, 0, 576, 580, 586, 625, 0, 26, 2),
    m(54000, 2880, 2928, 3180, 3456, 0, 288, 290, 293, 312, 0, 10, 1),
    m(54000, 2880, 2928, 3180, 3456, 0, 288, 290, 293, 312, 0, 10, 2),
    m(54000, 1440, 1464, 1592, 1728, 0, 576, 581, 586, 625, 0, 10, 1),
    m(54000, 1440, 1464, 1592, 1728, 0, 576, 581, 586, 625, 0, 10, 2),
    m(148500, 1920, 2448, 2492, 2640, 0, 1080, 1084, 1089, 1125, 0, 5, 2),
    m(74250, 1920, 2558, 2602, 2750, 0, 1080, 1084, 1089, 1125, 0, 5, 2),
    m(74250, 1920, 2448, 2492, 2640, 0, 1080, 1084, 1089, 1125, 0, 5, 2),
    m(74250, 1920, 2008, 2052, 2200, 0, 1080, 1084, 1089, 1125, 0, 5, 2),
    m(108000, 2880, 2944, 3192, 3432, 0, 480, 489, 495, 525, 0, 10, 1),
    m(108000, 2880, 2944, 3192, 3432, 0, 480, 489, 495, 525, 0, 10, 2),
    m(108000, 2880, 2928, 3184, 3456, 0, 576, 581, 586, 625, 0, 10, 1),
    m(108000, 2880, 2928, 3184, 3456, 0, 576, 581, 586, 625, 0, 10, 2),
    m(72000, 1920, 1952, 2120, 2304, 0, 1080, 1126, 1136, 1250, 0, 25, 2),
    m(148500, 1920, 2448, 2492, 2640, 0, 1080, 1084, 1094, 1125, 0, 21, 2),
    m(148500, 1280, 1720, 1760, 1980, 0, 720, 725, 730, 750, 0, 5, 2),
    m(54000, 720, 732, 796, 864, 0, 576, 581, 586, 625, 0, 10, 1),
    m(54000, 720, 732, 796, 864, 0, 576, 581, 586, 625, 0, 10, 2),
    m(27000, 720, 732, 795, 864, 0, 576, 580, 586, 625, 0, 4122, 1),
    m(27000, 720, 732, 795, 864, 0, 576, 580, 586, 625, 0, 4122, 2),
    m(148500, 1920, 2008, 2052, 2200, 0, 1080, 1084, 1094, 1125, 0, 21, 2),
    m(148500, 1280, 1390, 1430, 1650, 0, 720, 725, 730, 750, 0, 5, 2),
    m(54000, 720, 736, 798, 858, 0, 480, 489, 495, 525, 0, 10, 1),
    m(54000, 720, 736, 798, 858, 0, 480, 489, 495, 525, 0, 10, 2),
    m(27000, 720, 739, 801, 858, 0, 480, 488, 494, 525, 0, 4122, 1),
    m(27000, 720, 739, 801, 858, 0, 480, 488, 494, 525, 0, 4122, 2),
    m(108000, 720, 732, 796, 864, 0, 576, 581, 586, 625, 0, 10, 1),
    m(108000, 720, 732, 796, 864, 0, 576, 581, 586, 625, 0, 10, 2),
    m(54000, 720, 732, 795, 864, 0, 576, 580, 586, 625, 0, 4122, 1),
    m(54000, 720, 732, 795, 864, 0, 576, 580, 586, 625, 0, 4122, 2),
    m(108000, 720, 736, 798, 858, 0, 480, 489, 495, 525, 0, 10, 1),
    m(108000, 720, 736, 798, 858, 0, 480, 489, 495, 525, 0, 10, 2),
    m(54000, 720, 739, 801, 858, 0, 480, 488, 494, 525, 0, 4122, 1),
    m(54000, 720, 739, 801, 858, 0, 480, 488, 494, 525, 0, 4122, 2),
    m(59400, 1280, 3040, 3080, 3300, 0, 720, 725, 730, 750, 0, 5, 2),
    m(74250, 1280, 3700, 3740, 3960, 0, 720, 725, 730, 750, 0, 5, 2),
    m(74250, 1280, 3040, 3080, 3300, 0, 720, 725, 730, 750, 0, 5, 2),
    m(297000, 1920, 2008, 2052, 2200, 0, 1080, 1084, 1089, 1125, 0, 5, 2),
    m(297000, 1920, 2448, 2492, 2640, 0, 1080, 1084, 1089, 1125, 0, 5, 2),
    m(59400, 1280, 3040, 3080, 3300, 0, 720, 725, 730, 750, 0, 5, 3),
    m(74250, 1280, 3700, 3740, 3960, 0, 720, 725, 730, 750, 0, 5, 3),
    m(74250, 1280, 3040, 3080, 3300, 0, 720, 725, 730, 750, 0, 5, 3),
    m(74250, 1280, 1720, 1760, 1980, 0, 720, 725, 730, 750, 0, 5, 3),
    m(74250, 1280, 1390, 1430, 1650, 0, 720, 725, 730, 750, 0, 5, 3),
    m(148500, 1280, 1720, 1760, 1980, 0, 720, 725, 730, 750, 0, 5, 3),
    m(148500, 1280, 1390, 1430, 1650, 0, 720, 725, 730, 750, 0, 5, 3),
    m(74250, 1920, 2558, 2602, 2750, 0, 1080, 1084, 1089, 1125, 0, 5, 3),
    m(74250, 1920, 2448, 2492, 2640, 0, 1080, 1084, 1089, 1125, 0, 5, 3),
    m(74250, 1920, 2008, 2052, 2200, 0, 1080, 1084, 1089, 1125, 0, 5, 3),
    m(148500, 1920, 2448, 2492, 2640, 0, 1080, 1084, 1089, 1125, 0, 5, 3),
    m(148500, 1920, 2008, 2052, 2200, 0, 1080, 1084, 1089, 1125, 0, 5, 3),
    m(297000, 1920, 2448, 2492, 2640, 0, 1080, 1084, 1089, 1125, 0, 5, 3),
    m(297000, 1920, 2008, 2052, 2200, 0, 1080, 1084, 1089, 1125, 0, 5, 3),
    m(59400, 1680, 3040, 3080, 3300, 0, 720, 725, 730, 750, 0, 5, 3),
    m(59400, 1680, 2908, 2948, 3168, 0, 720, 725, 730, 750, 0, 5, 3),
    m(59400, 1680, 2380, 2420, 2640, 0, 720, 725, 730, 750, 0, 5, 3),
    m(82500, 1680, 1940, 1980, 2200, 0, 720, 725, 730, 750, 0, 5, 3),
    m(99000, 1680, 1940, 1980, 2200, 0, 720, 725, 730, 750, 0, 5, 3),
    m(165000, 1680, 1740, 1780, 2000, 0, 720, 725, 730, 825, 0, 5, 3),
    m(198000, 1680, 1740, 1780, 2000, 0, 720, 725, 730, 825, 0, 5, 3),
    m(99000, 2560, 3558, 3602, 3750, 0, 1080, 1084, 1089, 1100, 0, 5, 3),
    m(90000, 2560, 3008, 3052, 3200, 0, 1080, 1084, 1089, 1125, 0, 5, 3),
    m(118800, 2560, 3328, 3372, 3520, 0, 1080, 1084, 1089, 1125, 0, 5, 3),
    m(185625, 2560, 3108, 3152, 3300, 0, 1080, 1084, 1089, 1125, 0, 5, 3),
    m(198000, 2560, 2808, 2852, 3000, 0, 1080, 1084, 1089, 1100, 0, 5, 3),
    m(371250, 2560, 2778, 2822, 2970, 0, 1080, 1084, 1089, 1250, 0, 5, 3),
    m(495000, 2560, 3108, 3152, 3300, 0, 1080, 1084, 1089, 1250, 0, 5, 3),
    m(297000, 3840, 5116, 5204, 5500, 0, 2160, 2168, 2178, 2250, 0, 5, 2),
    m(297000, 3840, 4896, 4984, 5280, 0, 2160, 2168, 2178, 2250, 0, 5, 2),
    m(297000, 3840, 4016, 4104, 4400, 0, 2160, 2168, 2178, 2250, 0, 5, 2),
    m(594000, 3840, 4896, 4984, 5280, 0, 2160, 2168, 2178, 2250, 0, 5, 2),
    m(594000, 3840, 4016, 4104, 4400, 0, 2160, 2168, 2178, 2250, 0, 5, 2),
    m(297000, 4096, 5116, 5204, 5500, 0, 2160, 2168, 2178, 2250, 0, 5, 4),
    m(297000, 4096, 5064, 5152, 5280, 0, 2160, 2168, 2178, 2250, 0, 5, 4),
    m(297000, 4096, 4184, 4272, 4400, 0, 2160, 2168, 2178, 2250, 0, 5, 4),
    m(594000, 4096, 5064, 5152, 5280, 0, 2160, 2168, 2178, 2250, 0, 5, 4),
    m(594000, 4096, 4184, 4272, 4400, 0, 2160, 2168, 2178, 2250, 0, 5, 4),
    m(297000, 3840, 5116, 5204, 5500, 0, 2160, 2168, 2178, 2250, 0, 5, 3),
    m(297000, 3840, 4896, 4984, 5280, 0, 2160, 2168, 2178, 2250, 0, 5, 3),
    m(297000, 3840, 4016, 4104, 4400, 0, 2160, 2168, 2178, 2250, 0, 5, 3),
    m(594000, 3840, 4896, 4984, 5280, 0, 2160, 2168, 2178, 2250, 0, 5, 3),
    m(594000, 3840, 4016, 4104, 4400, 0, 2160, 2168, 2178, 2250, 0, 5, 3),
    m(90000, 1280, 2240, 2280, 2500, 0, 720, 725, 730, 750, 0, 5, 2),
    m(90000, 1280, 2240, 2280, 2500, 0, 720, 725, 730, 750, 0, 5, 3),
    m(99000, 1680, 2490, 2530, 2750, 0, 720, 725, 730, 750, 0, 5, 3),
    m(148500, 1920, 2558, 2602, 2750, 0, 1080, 1084, 1089, 1125, 0, 5, 2),
    m(148500, 1920, 2558, 2602, 2750, 0, 1080, 1084, 1089, 1125, 0, 5, 3),
    m(198000, 2560, 3558, 3602, 3750, 0, 1080, 1084, 1089, 1100, 0, 5, 3),
    m(594000, 3840, 5116, 5204, 5500, 0, 2160, 2168, 2178, 2250, 0, 5, 2),
    m(594000, 4096, 5116, 5204, 5500, 0, 2160, 2168, 2178, 2250, 0, 5, 4),
    m(594000, 3840, 5116, 5204, 5500, 0, 2160, 2168, 2178, 2250, 0, 5, 3),
    m(1188000, 3840, 4896, 4984, 5280, 0, 2160, 2168, 2178, 2250, 0, 5, 2),
    m(1188000, 3840, 4016, 4104, 4400, 0, 2160, 2168, 2178, 2250, 0, 5, 2),
    m(1188000, 3840, 4896, 4984, 5280, 0, 2160, 2168, 2178, 2250, 0, 5, 3),
    m(1188000, 3840, 4016, 4104, 4400, 0, 2160, 2168, 2178, 2250, 0, 5, 3),
    m(396000, 5120, 7116, 7204, 7500, 0, 2160, 2168, 2178, 2200, 0, 5, 3),
    m(396000, 5120, 6816, 6904, 7200, 0, 2160, 2168, 2178, 2200, 0, 5, 3),
    m(396000, 5120, 5784, 5872, 6000, 0, 2160, 2168, 2178, 2200, 0, 5, 3),
    m(742500, 5120, 5866, 5954, 6250, 0, 2160, 2168, 2178, 2475, 0, 5, 3),
    m(742500, 5120, 6216, 6304, 6600, 0, 2160, 2168, 2178, 2250, 0, 5, 3),
    m(742500, 5120, 5284, 5372, 5500, 0, 2160, 2168, 2178, 2250, 0, 5, 3),
    m(1485000, 5120, 6216, 6304, 6600, 0, 2160, 2168, 2178, 2250, 0, 5, 3),
];

const CEA_193: [M; 27] = [
    m(1485000, 5120, 5284, 5372, 5500, 0, 2160, 2168, 2178, 2250, 0, 5, 3),
    m(1188000, 7680, 10232, 10408, 11000, 0, 4320, 4336, 4356, 4500, 0, 5, 2),
    m(1188000, 7680, 10032, 10208, 10800, 0, 4320, 4336, 4356, 4400, 0, 5, 2),
    m(1188000, 7680, 8232, 8408, 9000, 0, 4320, 4336, 4356, 4400, 0, 5, 2),
    m(2376000, 7680, 10232, 10408, 11000, 0, 4320, 4336, 4356, 4500, 0, 5, 2),
    m(2376000, 7680, 10032, 10208, 10800, 0, 4320, 4336, 4356, 4400, 0, 5, 2),
    m(2376000, 7680, 8232, 8408, 9000, 0, 4320, 4336, 4356, 4400, 0, 5, 2),
    m(4752000, 7680, 9792, 9968, 10560, 0, 4320, 4336, 4356, 4500, 0, 5, 2),
    m(4752000, 7680, 8032, 8208, 8800, 0, 4320, 4336, 4356, 4500, 0, 5, 2),
    m(1188000, 7680, 10232, 10408, 11000, 0, 4320, 4336, 4356, 4500, 0, 5, 3),
    m(1188000, 7680, 10032, 10208, 10800, 0, 4320, 4336, 4356, 4400, 0, 5, 3),
    m(1188000, 7680, 8232, 8408, 9000, 0, 4320, 4336, 4356, 4400, 0, 5, 3),
    m(2376000, 7680, 10232, 10408, 11000, 0, 4320, 4336, 4356, 4500, 0, 5, 3),
    m(2376000, 7680, 10032, 10208, 10800, 0, 4320, 4336, 4356, 4400, 0, 5, 3),
    m(2376000, 7680, 8232, 8408, 9000, 0, 4320, 4336, 4356, 4400, 0, 5, 3),
    m(4752000, 7680, 9792, 9968, 10560, 0, 4320, 4336, 4356, 4500, 0, 5, 3),
    m(4752000, 7680, 8032, 8208, 8800, 0, 4320, 4336, 4356, 4500, 0, 5, 3),
    m(1485000, 10240, 11732, 11908, 12500, 0, 4320, 4336, 4356, 4950, 0, 5, 3),
    m(1485000, 10240, 12732, 12908, 13500, 0, 4320, 4336, 4356, 4400, 0, 5, 3),
    m(1485000, 10240, 10528, 10704, 11000, 0, 4320, 4336, 4356, 4500, 0, 5, 3),
    m(2970000, 10240, 11732, 11908, 12500, 0, 4320, 4336, 4356, 4950, 0, 5, 3),
    m(2970000, 10240, 12732, 12908, 13500, 0, 4320, 4336, 4356, 4400, 0, 5, 3),
    m(2970000, 10240, 10528, 10704, 11000, 0, 4320, 4336, 4356, 4500, 0, 5, 3),
    m(5940000, 10240, 12432, 12608, 13200, 0, 4320, 4336, 4356, 4500, 0, 5, 3),
    m(5940000, 10240, 10528, 10704, 11000, 0, 4320, 4336, 4356, 4500, 0, 5, 3),
    m(1188000, 4096, 4896, 4984, 5280, 0, 2160, 2168, 2178, 2250, 0, 5, 4),
    m(1188000, 4096, 4184, 4272, 4400, 0, 2160, 2168, 2178, 2250, 0, 5, 4),
];


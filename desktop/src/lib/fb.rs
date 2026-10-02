// 画面 (/dev/fb0、Linux の fbdev と同じ形) に描く
//   Fb::open() で開いて mmap、px() で描き、present() で画面へ (FBIOPAN_DISPLAY)
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
}

impl Fb {
    pub fn open() -> io::Result<Fb> {
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
        Ok(Fb { file, mem: mem as *mut u32, len, width: var[0] as usize, height: var[1] as usize, stride: line / 4 })
    }

    pub fn pixels(&mut self) -> &mut [u32] {
        unsafe { std::slice::from_raw_parts_mut(self.mem, self.len / 4) }
    }

    /// (x, y) に色 (0xRRGGBB) を a (0..=255) の濃さで重ねる
    pub fn blend(&mut self, x: i32, y: i32, rgb: u32, a: u32) {
        if x < 0 || y < 0 || x as usize >= self.width || y as usize >= self.height || a == 0 {
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
        let mut var = [0u32; 40];
        unsafe { libc::ioctl(self.file.as_raw_fd(), FBIOPAN_DISPLAY, var.as_mut_ptr()) };
    }
}

impl Fb {
    #[allow(dead_code)]
    /// [y0, y1) の行だけ画面へ (その行を自分自身へ書くと、カーネルがその行だけ送る)
    pub fn present_rows(&self, y0: usize, y1: usize) {
        let (y0, y1) = (y0.min(self.height), y1.min(self.height));
        if y0 >= y1 {
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

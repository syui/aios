// PNG (アイコン) を読んで描く
use crate::fb::Fb;

pub struct Image {
    pub width: usize,
    pub height: usize,
    /// RGBA
    pub rgba: Vec<u8>,
}

impl Image {
    pub fn load(path: &str) -> Result<Image, String> {
        let f = std::fs::File::open(path).map_err(|e| format!("{}: {}", path, e))?;
        let mut dec = png::Decoder::new(std::io::BufReader::new(f));
        dec.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
        let mut r = dec.read_info().map_err(|e| format!("{}: {}", path, e))?;
        let mut buf = vec![0; r.output_buffer_size().ok_or("png: too big")?];
        let info = r.next_frame(&mut buf).map_err(|e| format!("{}: {}", path, e))?;
        let (w, h) = (info.width as usize, info.height as usize);
        let rgba = match info.color_type {
            png::ColorType::Rgba => buf[..w * h * 4].to_vec(),
            png::ColorType::Rgb => buf[..w * h * 3].chunks(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
            png::ColorType::GrayscaleAlpha => buf[..w * h * 2].chunks(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
            _ => buf[..w * h].iter().flat_map(|&g| [g, g, g, 255]).collect(),
        };
        Ok(Image { width: w, height: h, rgba })
    }

    /// (x, y) を左上に、size × size に縮めて (平均して) 描く
    pub fn draw(&self, fb: &mut Fb, x: i32, y: i32, size: usize) {
        for dy in 0..size {
            for dx in 0..size {
                // 元の画像のこの画素にあたる範囲を平均する
                let (x0, x1) = (dx * self.width / size, ((dx + 1) * self.width / size).max(dx * self.width / size + 1));
                let (y0, y1) = (dy * self.height / size, ((dy + 1) * self.height / size).max(dy * self.height / size + 1));
                let (mut r, mut g, mut b, mut a, mut n) = (0u32, 0u32, 0u32, 0u32, 0u32);
                for sy in y0..y1.min(self.height) {
                    for sx in x0..x1.min(self.width) {
                        let p = &self.rgba[(sy * self.width + sx) * 4..];
                        let pa = p[3] as u32;
                        r += p[0] as u32 * pa;
                        g += p[1] as u32 * pa;
                        b += p[2] as u32 * pa;
                        a += pa;
                        n += 1;
                    }
                }
                if a == 0 {
                    continue;
                }
                let rgb = (r / a) << 16 | (g / a) << 8 | (b / a);
                fb.blend(x + dx as i32, y + dy as i32, rgb, a / n);
            }
        }
    }
}

// 文字を描く (aifont を fontdue で点にする)
use crate::fb::Fb;

pub const FONT: &str = "/usr/share/fonts/aifont/aifont.ttf";

pub struct Text {
    font: fontdue::Font,
}

impl Text {
    pub fn load(path: &str) -> Result<Text, String> {
        let data = std::fs::read(path).map_err(|e| format!("{}: {}", path, e))?;
        let font = fontdue::Font::from_bytes(data, fontdue::FontSettings::default()).map_err(|e| format!("{}: {}", path, e))?;
        Ok(Text { font })
    }

    /// 幅 (画素)
    pub fn width(&self, s: &str, size: f32) -> i32 {
        s.chars().map(|c| self.font.metrics(c, size).advance_width).sum::<f32>().round() as i32
    }

    /// (x, y) を左、ベースラインにして描く
    pub fn draw(&self, fb: &mut Fb, s: &str, x: i32, y: i32, size: f32, rgb: u32) {
        let mut pen = x as f32;
        for c in s.chars() {
            let (m, bitmap) = self.font.rasterize(c, size);
            let gx = pen.round() as i32 + m.xmin;
            let gy = y - m.height as i32 - m.ymin;
            for row in 0..m.height {
                for col in 0..m.width {
                    let a = bitmap[row * m.width + col] as u32;
                    fb.blend(gx + col as i32, gy + row as i32, rgb, a);
                }
            }
            pen += m.advance_width;
        }
    }

    /// 真ん中にそろえて描く
    pub fn center(&self, fb: &mut Fb, s: &str, y: i32, size: f32, rgb: u32) {
        let x = (fb.width as i32 - self.width(s, size)) / 2;
        self.draw(fb, s, x, y, size, rgb);
    }
}

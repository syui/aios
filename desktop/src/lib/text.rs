// 文字を描く (aifont を glyph.rs (ab_glyph) で点にする)
use crate::fb::Fb;
use crate::glyph;

pub const FONT: &str = "/usr/share/fonts/aifont/aifont.ttf";
/// aifont にない字 (日本語など) を描くフォント (aifont-ja パッケージ。なくてもよい)
pub const FONT_JA: &str = "/usr/share/fonts/aifont/aifont-ja.ttf";

pub struct Text {
    font: glyph::Font,
    /// aifont-ja は、aifont にない字がはじめて出たときに読む
    ja: std::cell::OnceCell<Option<glyph::Font>>,
    /// 点にした字 ((字, 大きさ) → 形と濃さ)。バーやタブは同じ字を何度も描く
    cache: std::cell::RefCell<std::collections::HashMap<(char, u32), (glyph::Metrics, Vec<u8>)>>,
}

impl Text {
    pub fn load(path: &str) -> Result<Text, String> {
        let data = std::fs::read(path).map_err(|e| format!("{}: {}", path, e))?;
        let font = glyph::Font::from_bytes(data).map_err(|e| format!("{}: {}", path, e))?;
        Ok(Text { font, ja: std::cell::OnceCell::new(), cache: Default::default() })
    }

    /// c を描くフォント (aifont になければ aifont-ja)
    fn font_of(&self, c: char) -> &glyph::Font {
        if self.font.has(c) {
            return &self.font;
        }
        match self.ja.get_or_init(|| std::fs::read(FONT_JA).ok().and_then(|d| glyph::Font::from_bytes(d).ok())) {
            Some(ja) if ja.has(c) => ja,
            _ => &self.font,
        }
    }

    /// 幅 (画素)
    pub fn width(&self, s: &str, size: f32) -> i32 {
        s.chars().map(|c| self.font_of(c).metrics(c, size).advance_width).sum::<f32>().round() as i32
    }

    /// (x, y) を左、ベースラインにして描く
    pub fn draw(&self, fb: &mut Fb, s: &str, x: i32, y: i32, size: f32, rgb: u32) {
        let mut pen = x as f32;
        for c in s.chars() {
            let key = (c, size.to_bits());
            if !self.cache.borrow().contains_key(&key) {
                let g = self.font_of(c).rasterize(c, size);
                self.cache.borrow_mut().insert(key, g);
            }
            let cache = self.cache.borrow();
            let (m, bitmap) = &cache[&key];
            let m = *m;
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

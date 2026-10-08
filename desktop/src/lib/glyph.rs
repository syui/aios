// フォントの字を点にする (ab_glyph)。使う字だけをそのときに読む
//   fontdue は読むときに全部の字を下ごしらえする。aifont (2 万 5 千字) では
//   それだけで aiwm と aiterm の起動が数秒かかっていた。形 (Metrics) は fontdue と同じ意味にしてある
#![allow(dead_code)]
use ab_glyph::{Font as _, FontVec, PxScale, ScaleFont};

pub struct Font {
    f: FontVec,
    /// em の大きさ (fontdue の size) を ab_glyph の PxScale (ascent - descent の高さ) に直す倍率
    em: f32,
}

/// fontdue::Metrics と同じ: 絵の左 (xmin) と下 (ymin、ベースラインから上へ)、大きさ、送り幅
#[derive(Clone, Copy, Debug)]
pub struct Metrics {
    pub xmin: i32,
    pub ymin: i32,
    pub width: usize,
    pub height: usize,
    pub advance_width: f32,
}

/// fontdue::LineMetrics と同じ (descent は負)
pub struct LineMetrics {
    pub ascent: f32,
    pub descent: f32,
    pub line_gap: f32,
}

impl Font {
    pub fn from_bytes(data: Vec<u8>) -> Result<Font, String> {
        let f = FontVec::try_from_vec(data).map_err(|e| e.to_string())?;
        let upem = f.units_per_em().unwrap_or(1000.0);
        let em = f.height_unscaled() / upem;
        Ok(Font { f, em })
    }

    fn scale(&self, size: f32) -> PxScale {
        PxScale::from(size * self.em)
    }

    /// その字があるか
    pub fn has(&self, c: char) -> bool {
        self.f.glyph_id(c).0 != 0
    }

    pub fn line_metrics(&self, size: f32) -> LineMetrics {
        let s = self.f.as_scaled(self.scale(size));
        LineMetrics { ascent: s.ascent(), descent: s.descent(), line_gap: s.line_gap() }
    }

    /// 形だけ (点にしない)
    pub fn metrics(&self, c: char, size: f32) -> Metrics {
        let s = self.f.as_scaled(self.scale(size));
        let id = self.f.glyph_id(c);
        let adv = s.h_advance(id);
        match self.f.outline_glyph(id.with_scale(self.scale(size))) {
            Some(g) => {
                let b = g.px_bounds();
                Metrics { xmin: b.min.x as i32, ymin: -(b.max.y as i32), width: b.width() as usize, height: b.height() as usize, advance_width: adv }
            }
            None => Metrics { xmin: 0, ymin: 0, width: 0, height: 0, advance_width: adv },
        }
    }

    /// 点にする: 形と濃さ (0..=255、上の行から)
    pub fn rasterize(&self, c: char, size: f32) -> (Metrics, Vec<u8>) {
        let s = self.f.as_scaled(self.scale(size));
        let id = self.f.glyph_id(c);
        let adv = s.h_advance(id);
        let Some(g) = self.f.outline_glyph(id.with_scale(self.scale(size))) else {
            return (Metrics { xmin: 0, ymin: 0, width: 0, height: 0, advance_width: adv }, Vec::new());
        };
        let b = g.px_bounds();
        let (w, h) = (b.width() as usize, b.height() as usize);
        let mut a = vec![0u8; w * h];
        g.draw(|x, y, v| {
            let (x, y) = (x as usize, y as usize);
            if x < w && y < h {
                a[y * w + x] = (v.clamp(0.0, 1.0) * 255.0).round() as u8;
            }
        });
        (Metrics { xmin: b.min.x as i32, ymin: -(b.max.y as i32), width: w, height: h, advance_width: adv }, a)
    }
}

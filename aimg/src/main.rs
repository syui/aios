// aimg: 画像を変える (aios の、ImageMagick の代わり。Rust だけ)
//   aimg info FILE... [--json]          形式と大きさ
//   aimg IN OUT [OPTIONS]               変換 (形式は OUT の拡張子: png webp jpg gif bmp ico)
//     --resize 50% | 800x | x600 | 800x600 (中に収める) | 800x600! (そのまま)
//     --width N / --height N            svg を描く大きさ (ほかの画像では --resize Nx / xN と同じ)
//     --crop WxH+X+Y   --rotate 90|180|270   --flip h|v   --quality N (jpg、既定 90)
//     --background COLOR                透明なところの色 (jpg は既定 white。#rrggbb か white black)
// 読めるもの: png jpg gif webp bmp ico svg (svg は resvg で描く。字は /usr/share/fonts などから)
// 順番は crop → resize → rotate → flip。webp は可逆 (lossless) で書く
use image::{DynamicImage, GenericImageView, ImageFormat, Rgba, RgbaImage, imageops::FilterType};
use std::path::Path;
use std::process::exit;

fn die(msg: impl std::fmt::Display) -> ! {
    eprintln!("aimg: {}", msg);
    exit(1)
}

fn usage() -> ! {
    eprintln!("usage: aimg info FILE... [--json]");
    eprintln!("       aimg IN OUT [--resize 50%|800x|x600|800x600|800x600!] [--width N] [--height N]");
    eprintln!("                   [--crop WxH+X+Y] [--rotate 90|180|270] [--flip h|v] [--quality N] [--background COLOR]");
    eprintln!("  formats: png jpg gif webp bmp ico (read and write), svg (read)");
    exit(2)
}

fn ext(path: &str) -> String {
    Path::new(path).extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default()
}

fn is_svg(path: &str) -> bool {
    matches!(ext(path).as_str(), "svg" | "svgz")
}

/// svg を読む (字のために、システムのフォントも)
fn svg_tree(path: &str) -> resvg::usvg::Tree {
    let data = std::fs::read(path).unwrap_or_else(|e| die(format!("{}: {}", path, e)));
    let mut opt = resvg::usvg::Options { resources_dir: Path::new(path).parent().map(Path::to_path_buf), ..Default::default() };
    opt.fontdb_mut().load_system_fonts();
    resvg::usvg::Tree::from_data(&data, &opt).unwrap_or_else(|e| die(format!("{}: {}", path, e)))
}

fn bad_resize(spec: &str) -> ! {
    die(format!("--resize {}: give 50%, 800x, x600, 800x600 or 800x600!", spec))
}

fn bad_crop(c: &str) -> ! {
    die(format!("--crop {}: give WxH+X+Y", c))
}

/// 大きさの指定 (w, h) を、いまの大きさ (w0, h0) から決める
fn fit(spec: &str, w0: u32, h0: u32) -> (u32, u32) {
    if let Some(p) = spec.strip_suffix('%') {
        let p: f64 = p.parse().unwrap_or_else(|_| bad_resize(spec));
        return (((w0 as f64 * p / 100.0).round() as u32).max(1), ((h0 as f64 * p / 100.0).round() as u32).max(1));
    }
    let exact = spec.ends_with('!');
    let (a, b) = spec.trim_end_matches('!').split_once('x').unwrap_or_else(|| bad_resize(spec));
    let num = |s: &str| if s.is_empty() { None } else { Some(s.parse::<u32>().unwrap_or_else(|_| bad_resize(spec))) };
    match (num(a), num(b)) {
        (Some(w), None) => (w, ((h0 as f64 * w as f64 / w0 as f64).round() as u32).max(1)),
        (None, Some(h)) => (((w0 as f64 * h as f64 / h0 as f64).round() as u32).max(1), h),
        (Some(w), Some(h)) if exact => (w, h),
        // 中に収める (縦横の比はそのまま)
        (Some(w), Some(h)) => {
            let r = (w as f64 / w0 as f64).min(h as f64 / h0 as f64);
            (((w0 as f64 * r).round() as u32).max(1), ((h0 as f64 * r).round() as u32).max(1))
        }
        (None, None) => bad_resize(spec),
    }
}

/// svg を (w, h) で描く
fn render_svg(tree: &resvg::usvg::Tree, w: u32, h: u32) -> DynamicImage {
    let mut pm = resvg::tiny_skia::Pixmap::new(w, h).unwrap_or_else(|| die("svg: bad size"));
    let s = tree.size();
    let tf = resvg::tiny_skia::Transform::from_scale(w as f32 / s.width(), h as f32 / s.height());
    resvg::render(tree, tf, &mut pm.as_mut());
    let mut img = RgbaImage::new(w, h);
    for (i, px) in pm.pixels().iter().enumerate() {
        let c = px.demultiply();
        img.put_pixel(i as u32 % w, i as u32 / w, Rgba([c.red(), c.green(), c.blue(), c.alpha()]));
    }
    DynamicImage::ImageRgba8(img)
}

fn color(s: &str) -> [u8; 3] {
    match s {
        "white" => [255, 255, 255],
        "black" => [0, 0, 0],
        _ => {
            let h = s.trim_start_matches('#');
            let v = u32::from_str_radix(h, 16).ok().filter(|_| h.len() == 6).unwrap_or_else(|| die(format!("--background {}: give #rrggbb, white or black", s)));
            [(v >> 16) as u8, (v >> 8) as u8, v as u8]
        }
    }
}

/// 透明なところを bg の色にする
fn flatten(img: &DynamicImage, bg: [u8; 3]) -> DynamicImage {
    let rgba = img.to_rgba8();
    let mut out = image::RgbImage::new(rgba.width(), rgba.height());
    for (x, y, p) in rgba.enumerate_pixels() {
        let a = p[3] as u32;
        let mix = |c: u8, b: u8| ((c as u32 * a + b as u32 * (255 - a)) / 255) as u8;
        out.put_pixel(x, y, image::Rgb([mix(p[0], bg[0]), mix(p[1], bg[1]), mix(p[2], bg[2])]));
    }
    DynamicImage::ImageRgb8(out)
}

fn info(files: &[String], json: bool) {
    let mut all = Vec::new();
    for f in files {
        let (format, w, h) = if is_svg(f) {
            let s = svg_tree(f).size();
            ("svg".to_string(), s.width().round() as u32, s.height().round() as u32)
        } else {
            let r = image::ImageReader::open(f).and_then(|r| r.with_guessed_format()).unwrap_or_else(|e| die(format!("{}: {}", f, e)));
            let fmt = r.format().map(|x| format!("{:?}", x).to_lowercase()).unwrap_or_else(|| "?".into());
            let (w, h) = r.into_dimensions().unwrap_or_else(|e| die(format!("{}: {}", f, e)));
            (fmt, w, h)
        };
        let bytes = std::fs::metadata(f).map(|m| m.len()).unwrap_or(0);
        if json {
            all.push(serde_json::json!({ "file": f, "format": format, "width": w, "height": h, "bytes": bytes }));
        } else {
            println!("{} {} {}x{} {}", f, format, w, h, bytes);
        }
    }
    if json {
        println!("{}", serde_json::Value::Array(all));
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "-h" || a == "--help") {
        usage();
    }
    if args[0] == "--version" || args[0] == "version" {
        println!("aimg {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if args[0] == "info" {
        let files: Vec<String> = args[1..].iter().filter(|a| !a.starts_with("--")).cloned().collect();
        if files.is_empty() {
            usage();
        }
        return info(&files, args.iter().any(|a| a == "--json"));
    }
    // aimg IN OUT [OPTIONS]
    let mut pos = Vec::new();
    let (mut resize, mut width, mut height, mut crop, mut rotate, mut flip, mut quality, mut background) = (None, None, None, None, None, None, 90u8, None);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().cloned().unwrap_or_else(|| die(format!("{}: needs a value", a)));
        match a.as_str() {
            "--resize" => resize = Some(val()),
            "--width" => width = Some(val().parse::<u32>().unwrap_or_else(|_| die("--width: give a number"))),
            "--height" => height = Some(val().parse::<u32>().unwrap_or_else(|_| die("--height: give a number"))),
            "--crop" => crop = Some(val()),
            "--rotate" => rotate = Some(val()),
            "--flip" => flip = Some(val()),
            "--quality" => quality = val().parse().unwrap_or_else(|_| die("--quality: give 1-100")),
            "--background" => background = Some(color(&val())),
            s if s.starts_with("--") => die(format!("{}: unknown option", s)),
            _ => pos.push(a.clone()),
        }
    }
    let [input, output] = pos.as_slice() else { usage() };
    // --width / --height は --resize Nx / xN と同じ (両方なら NxM!)
    let resize = resize.or(match (width, height) {
        (Some(w), Some(h)) => Some(format!("{}x{}!", w, h)),
        (Some(w), None) => Some(format!("{}x", w)),
        (None, Some(h)) => Some(format!("x{}", h)),
        _ => None,
    });
    let mut img = if is_svg(input) {
        // svg はその大きさで描く (crop がなければ、resize の大きさで描いてにじませない)
        let tree = svg_tree(input);
        let (w0, h0) = (tree.size().width().round().max(1.0) as u32, tree.size().height().round().max(1.0) as u32);
        let (w, h) = match (&resize, &crop) {
            (Some(r), None) => fit(r, w0, h0),
            _ => (w0, h0),
        };
        render_svg(&tree, w, h)
    } else {
        image::ImageReader::open(input).and_then(|r| r.with_guessed_format()).unwrap_or_else(|e| die(format!("{}: {}", input, e))).decode().unwrap_or_else(|e| die(format!("{}: {}", input, e)))
    };
    let svg_sized = is_svg(input) && crop.is_none();
    if let Some(c) = &crop {
            let (wh, rest) = c.split_once('+').unwrap_or_else(|| bad_crop(c));
        let (w, h) = wh.split_once('x').unwrap_or_else(|| bad_crop(c));
        let (x, y) = rest.split_once('+').unwrap_or_else(|| bad_crop(c));
        let n = |s: &str| s.parse::<u32>().unwrap_or_else(|_| bad_crop(c));
        let (x, y, w, h) = (n(x), n(y), n(w), n(h));
        let (iw, ih) = img.dimensions();
        if x >= iw || y >= ih {
            die(format!("--crop {}: outside the {}x{} image", c, iw, ih));
        }
        img = img.crop_imm(x, y, w.min(iw - x), h.min(ih - y));
    }
    if let Some(r) = &resize
        && !svg_sized
    {
        let (w, h) = fit(r, img.width(), img.height());
        img = img.resize_exact(w, h, FilterType::Lanczos3);
    }
    match rotate.as_deref() {
        None | Some("0") => {}
        Some("90") => img = img.rotate90(),
        Some("180") => img = img.rotate180(),
        Some("270") => img = img.rotate270(),
        Some(r) => die(format!("--rotate {}: give 90, 180 or 270", r)),
    }
    match flip.as_deref() {
        None => {}
        Some("h") => img = img.fliph(),
        Some("v") => img = img.flipv(),
        Some(f) => die(format!("--flip {}: give h or v", f)),
    }
    let format = match ext(output).as_str() {
        "png" => ImageFormat::Png,
        "jpg" | "jpeg" => ImageFormat::Jpeg,
        "webp" => ImageFormat::WebP,
        "gif" => ImageFormat::Gif,
        "bmp" => ImageFormat::Bmp,
        "ico" => ImageFormat::Ico,
        "svg" => die("cannot write svg"),
        e => die(format!("{}: unknown format .{} (png jpg webp gif bmp ico)", output, e)),
    };
    if let Some(bg) = background {
        img = flatten(&img, bg);
    }
    let file = std::fs::File::create(output).unwrap_or_else(|e| die(format!("{}: {}", output, e)));
    let mut w = std::io::BufWriter::new(file);
    let r = match format {
        ImageFormat::Jpeg => {
            let rgb = if img.color().has_alpha() { flatten(&img, background.unwrap_or([255, 255, 255])) } else { img.clone() };
            rgb.write_with_encoder(image::codecs::jpeg::JpegEncoder::new_with_quality(&mut w, quality))
        }
        f => img.write_to(&mut w, f),
    };
    r.unwrap_or_else(|e| die(format!("{}: {}", output, e)));
}

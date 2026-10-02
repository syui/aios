// aisplash: 画面 (/dev/fb0) に aios のロゴと文字を描く。キーを押すか、時間がたったら消して終わる
//   aisplash [秒]   (既定 30 秒。0 ならずっと)
// デスクトップ (段階 1) の確かめ: 画面、フォント (aifont)、アイコン (ai.png)、キーボード
#[path = "../lib/fb.rs"]
mod fb;
#[path = "../lib/image.rs"]
mod image;
#[path = "../lib/input.rs"]
mod input;
#[path = "../lib/text.rs"]
mod text;

use std::process::exit;

const ICON: &str = "/usr/share/icons/ai/ai.png";
const YELLOW: u32 = 0xf5c518;

fn uname() -> String {
    let mut u: libc::utsname = unsafe { std::mem::zeroed() };
    unsafe { libc::uname(&mut u) };
    let s = |f: &[libc::c_char]| unsafe { std::ffi::CStr::from_ptr(f.as_ptr()) }.to_string_lossy().into_owned();
    format!("{} {} {}", s(&u.sysname), s(&u.release), s(&u.machine))
}

fn main() {
    let secs: i32 = std::env::args().nth(1).and_then(|a| a.parse().ok()).unwrap_or(30);
    let mut fb = fb::Fb::open().unwrap_or_else(|e| {
        eprintln!("aisplash: /dev/fb0: {} (sudo modprobe virtio_gpu、窓は AIOS_DISPLAY=1)", e);
        exit(1)
    });
    let txt = text::Text::load(text::FONT).unwrap_or_else(|e| {
        eprintln!("aisplash: {}", e);
        exit(1)
    });
    let (w, h) = (fb.width as i32, fb.height as i32);

    // 背景: 上から下へ、紺のグラデーション
    let (stride, fw) = (fb.stride, fb.width);
    for y in 0..fb.height {
        let t = y as u32 * 255 / fb.height as u32;
        let c = (10 + t * 16 / 255) << 16 | (12 + t * 24 / 255) << 8 | (28 + t * 48 / 255);
        fb.pixels()[y * stride..y * stride + fw].fill(c);
    }

    // ロゴ
    let size = (h / 4).max(64) as usize;
    let logo_y = h / 2 - size as i32;
    match image::Image::load(ICON) {
        Ok(img) => img.draw(&mut fb, (w - size as i32) / 2, logo_y, size),
        Err(e) => eprintln!("aisplash: {}", e),
    }

    // 文字
    let big = (h as f32 / 10.0).max(24.0);
    let small = (h as f32 / 40.0).max(12.0);
    let base = logo_y + size as i32 + big as i32 + 10;
    txt.center(&mut fb, "aios", base, big, YELLOW);
    txt.center(&mut fb, &uname(), base + (small * 2.0) as i32, small, 0xc8d0e0);
    txt.center(&mut fb, &format!("{}x{}  aifont \u{e0a0} \u{f179} \u{f31a}  — press any key", w, h), base + (small * 3.6) as i32, small, 0x8090b0);
    fb.present();

    // キーを待つ
    let mut inputs = input::Inputs::open();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs.max(0) as u64);
    loop {
        if secs > 0 && std::time::Instant::now() >= deadline {
            break;
        }
        if inputs.is_empty() {
            std::thread::sleep(std::time::Duration::from_millis(200));
            continue;
        }
        // キーボードのキーだけ (マウスのボタン BTN_* は 0x100 から。窓をクリックしただけでは消さない)
        if let Some(e) = inputs.wait(500).iter().find(|e| e.typ == input::EV_KEY && e.value == 1 && e.code < 0x100) {
            eprintln!("aisplash: key {}", e.code);
            break;
        }
    }
    fb.fill(0);
    fb.present();
}

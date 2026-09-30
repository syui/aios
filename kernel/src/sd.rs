// SD カード (SDHCI, PIO で待つだけの簡単なもの)
//
// ラズパイ 3 の Arasan SDHCI (QEMU raspi3b ではここに SD カードがつながる) と、
// ラズパイ 4 の emmc2 はどちらも SDHCI。BCM2835 の SDHCI は 8/16 ビットの書き込みを
// 受けつけないので、レジスタはいつも 32 ビットで読み書きする。
use crate::memlayout::p2v;
use core::ptr::{read_volatile, write_volatile};

const ARG2: usize = 0x00;
const BLKSIZECNT: usize = 0x04;
const ARG1: usize = 0x08;
const CMDTM: usize = 0x0c;
const RESP0: usize = 0x10;
const DATA: usize = 0x20;
const STATUS: usize = 0x24;
const CONTROL0: usize = 0x28;
const CONTROL1: usize = 0x2c;
const INTERRUPT: usize = 0x30;
const IRPT_MASK: usize = 0x34;
const IRPT_EN: usize = 0x38;

// STATUS
const CMD_INHIBIT: u32 = 1 << 0;
const DAT_INHIBIT: u32 = 1 << 1;
// CONTROL1
const CLK_INTLEN: u32 = 1 << 0;
const CLK_STABLE: u32 = 1 << 1;
const CLK_EN: u32 = 1 << 2;
const SRST_HC: u32 = 1 << 24;
// INTERRUPT
const INT_CMD_DONE: u32 = 1 << 0;
const INT_DATA_DONE: u32 = 1 << 1;
const INT_WRITE_RDY: u32 = 1 << 4;
const INT_READ_RDY: u32 = 1 << 5;
const INT_ERR: u32 = 1 << 15;

// CMDTM の上位 16 ビット (コマンド) と下位 16 ビット (転送モード)
const RSP_NONE: u32 = 0;
const RSP_136: u32 = 1 << 16;
const RSP_48: u32 = 2 << 16;
const RSP_48_BUSY: u32 = 3 << 16;
const CRC_CHK: u32 = 1 << 19;
const IDX_CHK: u32 = 1 << 20;
const DATA_PRESENT: u32 = 1 << 21;
const TM_BLKCNT_EN: u32 = 1 << 1;
const TM_AUTO_CMD12: u32 = 1 << 2;
const TM_READ: u32 = 1 << 4;
const TM_MULTI: u32 = 1 << 5;

const R1: u32 = RSP_48 | CRC_CHK | IDX_CHK;
const R1B: u32 = RSP_48_BUSY | CRC_CHK | IDX_CHK;
const R2: u32 = RSP_136 | CRC_CHK;
const R3: u32 = RSP_48;
const R6: u32 = RSP_48 | CRC_CHK | IDX_CHK;
const R7: u32 = RSP_48 | CRC_CHK | IDX_CHK;

pub const SECTOR: usize = 512;

struct Sd {
    base: usize,
    /// SDHC / SDXC はブロック番号、SDSC はバイトの位置で指す
    block_addr: bool,
}

static mut SD: Option<Sd> = None;

fn rd(s: &Sd, r: usize) -> u32 {
    unsafe { read_volatile((s.base + r) as *const u32) }
}

fn wr(s: &Sd, r: usize, v: u32) {
    unsafe { write_volatile((s.base + r) as *mut u32, v) }
}

/// 条件が立つまで待つ (だいたいの回数で打ち切る)
fn wait(s: &Sd, r: usize, mask: u32, set: bool) -> Result<(), i64> {
    for _ in 0..1_000_000 {
        if (rd(s, r) & mask != 0) == set {
            return Ok(());
        }
        core::hint::spin_loop();
    }
    Err(-5)
}

/// 割り込みの状態に mask が立つまで待って消す。エラーなら EIO
fn wait_int(s: &Sd, mask: u32) -> Result<(), i64> {
    for _ in 0..1_000_000 {
        let v = rd(s, INTERRUPT);
        if v & INT_ERR != 0 {
            wr(s, INTERRUPT, v);
            return Err(-5);
        }
        if v & mask != 0 {
            wr(s, INTERRUPT, mask);
            return Ok(());
        }
        core::hint::spin_loop();
    }
    Err(-5)
}

fn cmd(s: &Sd, idx: u32, arg: u32, flags: u32) -> Result<u32, i64> {
    wait(s, STATUS, CMD_INHIBIT, false)?;
    if flags & RSP_48_BUSY == RSP_48_BUSY || flags & DATA_PRESENT != 0 {
        wait(s, STATUS, DAT_INHIBIT, false)?;
    }
    wr(s, INTERRUPT, 0xffff_ffff);
    wr(s, ARG1, arg);
    wr(s, CMDTM, (idx << 24) | flags);
    wait_int(s, INT_CMD_DONE)?;
    Ok(rd(s, RESP0))
}

/// アプリケーション用のコマンド (CMD55 を先に)
fn acmd(s: &Sd, rca: u32, idx: u32, arg: u32, flags: u32) -> Result<u32, i64> {
    cmd(s, 55, rca << 16, R1)?;
    cmd(s, idx, arg, flags)
}

/// SD の割り込みを使わずに、DTB の SDHCI を初期化してカードを使えるようにする
pub fn init() -> bool {
    let Some((pa, _)) = crate::dtb::reg_of("brcm,bcm2835-sdhci", 0).or_else(|| crate::dtb::reg_of("brcm,bcm2711-emmc2", 0)) else {
        return false;
    };
    let mut s = Sd { base: p2v(pa as usize), block_addr: false };
    match card_init(&mut s) {
        Ok(()) => {
            println!("sd: card at {:#x}{}", pa, if s.block_addr { " (SDHC)" } else { "" });
            unsafe { SD = Some(s) };
            true
        }
        Err(_) => false,
    }
}

fn card_init(s: &mut Sd) -> Result<(), i64> {
    // リセットして、電源 (3.3V) と 400 kHz くらいのクロック
    wr(s, CONTROL0, 0);
    wr(s, CONTROL1, SRST_HC);
    wait(s, CONTROL1, SRST_HC, false)?;
    wr(s, CONTROL0, 0x0f << 8);
    wr(s, CONTROL1, CLK_INTLEN | (0x80 << 8) | (0xe << 16));
    wait(s, CONTROL1, CLK_STABLE, true)?;
    wr(s, CONTROL1, rd(s, CONTROL1) | CLK_EN);
    wr(s, IRPT_EN, 0);
    wr(s, IRPT_MASK, 0xffff_ffff);
    wr(s, INTERRUPT, 0xffff_ffff);

    cmd(s, 0, 0, RSP_NONE)?;
    // CMD8: 2.0 以降のカードか (3.3V, 確かめの 0xaa が返る)
    let v2 = cmd(s, 8, 0x1aa, R7).is_ok_and(|r| r & 0xfff == 0x1aa);
    // ACMD41: 電源が入り終わるまで (HCS = 大容量を受けつける)
    let mut ocr = 0;
    for _ in 0..1000 {
        ocr = acmd(s, 0, 41, 0x00ff_8000 | if v2 { 1 << 30 } else { 0 }, R3)?;
        if ocr & (1 << 31) != 0 {
            break;
        }
    }
    if ocr & (1 << 31) == 0 {
        return Err(-5);
    }
    s.block_addr = ocr & (1 << 30) != 0;
    cmd(s, 2, 0, R2)?;
    let rca = cmd(s, 3, 0, R6)? >> 16;
    cmd(s, 7, rca << 16, R1B)?;
    if !s.block_addr {
        cmd(s, 16, SECTOR as u32, R1)?;
    }
    // データは 4 ビット幅 (ACMD6) で、クロックを上げる
    if acmd(s, rca, 6, 2, R1).is_ok() {
        wr(s, CONTROL0, rd(s, CONTROL0) | (1 << 1));
    }
    wr(s, CONTROL1, rd(s, CONTROL1) & !CLK_EN);
    wr(s, CONTROL1, (rd(s, CONTROL1) & !0xffc0) | (0x02 << 8));
    wait(s, CONTROL1, CLK_STABLE, true)?;
    wr(s, CONTROL1, rd(s, CONTROL1) | CLK_EN);
    let _ = ARG2;
    Ok(())
}

fn sd() -> Result<&'static Sd, i64> {
    unsafe { (*(&raw const SD)).as_ref().ok_or(-6) }
}

/// sector から buf (SECTOR の倍数) に読む
pub fn read(sector: u64, buf: &mut [u8]) -> Result<(), i64> {
    let s = sd()?;
    let n = buf.len() / SECTOR;
    if n == 0 || buf.len() % SECTOR != 0 {
        return Err(-22);
    }
    let addr = if s.block_addr { sector } else { sector * SECTOR as u64 } as u32;
    wr(s, BLKSIZECNT, (SECTOR as u32) | ((n as u32) << 16));
    let (idx, tm) = if n > 1 { (18, TM_READ | TM_MULTI | TM_BLKCNT_EN | TM_AUTO_CMD12) } else { (17, TM_READ) };
    cmd(s, idx, addr, R1 | DATA_PRESENT | tm)?;
    for blk in buf.chunks_mut(SECTOR) {
        wait_int(s, INT_READ_RDY)?;
        for w in blk.chunks_mut(4) {
            w.copy_from_slice(&rd(s, DATA).to_le_bytes());
        }
    }
    wait_int(s, INT_DATA_DONE)
}

/// buf (SECTOR の倍数) を sector に書く
pub fn write(sector: u64, buf: &[u8]) -> Result<(), i64> {
    let s = sd()?;
    let n = buf.len() / SECTOR;
    if n == 0 || buf.len() % SECTOR != 0 {
        return Err(-22);
    }
    let addr = if s.block_addr { sector } else { sector * SECTOR as u64 } as u32;
    wr(s, BLKSIZECNT, (SECTOR as u32) | ((n as u32) << 16));
    let (idx, tm) = if n > 1 { (25, TM_MULTI | TM_BLKCNT_EN | TM_AUTO_CMD12) } else { (24, 0) };
    cmd(s, idx, addr, R1 | DATA_PRESENT | tm)?;
    for blk in buf.chunks(SECTOR) {
        wait_int(s, INT_WRITE_RDY)?;
        for w in blk.chunks(4) {
            wr(s, DATA, u32::from_le_bytes(w.try_into().unwrap()));
        }
    }
    wait_int(s, INT_DATA_DONE)
}

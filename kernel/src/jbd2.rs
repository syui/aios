// ext4 のジャーナル (jbd2)。extfs の子モジュール
//
// flush のたびに、書きかえたメタデータのブロックを 1 つのトランザクションとして
// ジャーナルに書き (記述ブロック + ブロックの写し + コミットブロック)、それから
// 本当の場所へ書き、ジャーナルを空に戻す (data=ordered: ファイルの中身は先に直接書く)。
// マウントのときに書きかけのトランザクションが残っていれば、それを再生する。
//
// ジャーナルの中はビッグエンディアン。metadata_csum のファイルシステムでは Linux と同じく
// CSUM_V3 (crc32c) を使う。
use super::{crc32c, ExtFs, INCOMPAT_RECOVER};
use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;

const MAGIC: u32 = 0xc03b_3998;
const DESCRIPTOR: u32 = 1;
const COMMIT: u32 = 2;
const SB_V2: u32 = 4;
const REVOKE: u32 = 5;

const INCOMPAT_REVOKE: u32 = 0x1;
const INCOMPAT_64BIT: u32 = 0x2;
const INCOMPAT_CSUM_V3: u32 = 0x10;
const KNOWN_INCOMPAT: u32 = INCOMPAT_REVOKE | INCOMPAT_64BIT | INCOMPAT_CSUM_V3;
const CRC32C_CHKSUM: u8 = 4;

const FLAG_ESCAPE: u32 = 1;
const FLAG_SAME_UUID: u32 = 2;
const FLAG_LAST_TAG: u32 = 8;

fn be32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes(b[o..o + 4].try_into().unwrap())
}

fn put_be32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_be_bytes());
}

pub struct Journal {
    /// ジャーナルの i 番目のブロックがディスクのどこか
    blocks: Vec<u64>,
    first: u32,
    maxlen: u32,
    /// 次のトランザクションの番号
    seq: u32,
    uuid: [u8; 16],
    v3: bool,
    bit64: bool,
    seed: u32,
    /// ジャーナルのスーパーブロックの入ったブロック (先頭 1024 バイトがそれ)
    sb: Vec<u8>,
}

impl Journal {
    fn tag_bytes(&self) -> usize {
        if self.v3 { 16 } else if self.bit64 { 12 } else { 8 }
    }

    fn header(&self, b: &mut [u8], kind: u32, seq: u32) {
        put_be32(b, 0, MAGIC);
        put_be32(b, 4, kind);
        put_be32(b, 8, seq);
    }
}

impl ExtFs {
    fn jwrite(&self, j: &Journal, lb: u32, data: &[u8]) -> Result<(), i64> {
        let spb = (self.bsize / crate::block::SECTOR) as u64;
        crate::block::write(j.blocks[lb as usize] * spb, data)
    }

    fn jread(&self, j: &Journal, lb: u32) -> Result<Vec<u8>, i64> {
        let spb = (self.bsize / crate::block::SECTOR) as u64;
        let mut b = vec![0u8; self.bsize];
        crate::block::read(j.blocks[lb as usize] * spb, &mut b)?;
        Ok(b)
    }

    /// ジャーナルのスーパーブロックを (チェックサムを直して) 書く
    fn write_jsb(&self, j: &mut Journal) -> Result<(), i64> {
        if j.v3 {
            put_be32(&mut j.sb, 0xfc, 0);
            let c = crc32c(!0, &j.sb[..1024]);
            put_be32(&mut j.sb, 0xfc, c);
        }
        let data = j.sb.clone();
        self.jwrite(j, 0, &data)
    }

    /// マウントのとき: ジャーナルを開き、残っていれば再生する。
    /// 使えないジャーナル (知らない機能など) なら None (書くときはジャーナルなし)
    pub(super) fn journal_open(&self) -> Result<Option<Journal>, &'static str> {
        const COMPAT_HAS_JOURNAL: u32 = 0x4;
        let (has, inum) = {
            let sb = self.sb.borrow();
            (super::u32_at(&sb, 92) & COMPAT_HAS_JOURNAL != 0, super::u32_at(&sb, 0xe0))
        };
        if !has || inum == 0 {
            return Ok(None);
        }
        let mut raw = self.read_inode(inum).map_err(|_| "journal inode")?;
        let nblocks = (raw.size() / self.bsize as u64) as u32;
        let mut blocks = Vec::with_capacity(nblocks as usize);
        for lb in 0..nblocks {
            let b = self.map(inum, &mut raw, lb as u64, false).map_err(|_| "journal map")?;
            if b == 0 {
                return Err("journal has holes");
            }
            blocks.push(b);
        }
        let spb = (self.bsize / crate::block::SECTOR) as u64;
        let mut sb = vec![0u8; self.bsize];
        crate::block::read(blocks[0] * spb, &mut sb).map_err(|_| "read error")?;
        if be32(&sb, 0) != MAGIC || be32(&sb, 4) != SB_V2 && be32(&sb, 4) != 3 {
            return Err("bad journal superblock");
        }
        if be32(&sb, 0xc) as usize != self.bsize {
            return Err("journal block size differs");
        }
        let incompat = be32(&sb, 0x28);
        if incompat & !KNOWN_INCOMPAT != 0 {
            println!("extfs: journal features {:#x} not supported, writing without journal", incompat & !KNOWN_INCOMPAT);
            return Ok(None);
        }
        let mut uuid = [0u8; 16];
        uuid.copy_from_slice(&sb[0x30..0x40]);
        let mut j = Journal {
            first: be32(&sb, 0x14),
            maxlen: be32(&sb, 0x10).min(nblocks),
            seq: be32(&sb, 0x18),
            v3: incompat & INCOMPAT_CSUM_V3 != 0,
            bit64: incompat & INCOMPAT_64BIT != 0,
            seed: crc32c(!0, &uuid),
            uuid,
            blocks,
            sb,
        };
        if be32(&j.sb, 0x1c) != 0 {
            self.journal_replay(&mut j).map_err(|_| "journal replay failed")?;
        }
        // Linux と同じく、metadata_csum なら CSUM_V3 (と 64bit) のジャーナルにする
        if self.csum && !j.v3 {
            let bit64 = super::u32_at(&self.sb.borrow(), 96) & super::INCOMPAT_64BIT != 0;
            let mut inc = incompat | INCOMPAT_CSUM_V3;
            if bit64 {
                inc |= INCOMPAT_64BIT;
            }
            put_be32(&mut j.sb, 0x28, inc);
            put_be32(&mut j.sb, 4, SB_V2);
            j.sb[0x50] = CRC32C_CHKSUM;
            j.v3 = true;
            j.bit64 = bit64;
            self.write_jsb(&mut j).map_err(|_| "write error")?;
        }
        // 回復の印が残っていれば消す
        let recover = super::u32_at(&self.sb.borrow(), 96) & INCOMPAT_RECOVER != 0;
        if recover {
            self.set_recover(false).map_err(|_| "write error")?;
        }
        Ok(Some(j))
    }

    /// ext4 のスーパーブロックの needs_recovery を立てる / 消す (直接ディスクへ)
    fn set_recover(&self, on: bool) -> Result<(), i64> {
        let data = {
            let mut sb = self.sb.borrow_mut();
            let v = super::u32_at(&sb, 96);
            super::put32(&mut sb, 96, if on { v | INCOMPAT_RECOVER } else { v & !INCOMPAT_RECOVER });
            if self.csum {
                let c = crc32c(!0, &sb[..0x3fc]);
                super::put32(&mut sb, 0x3fc, c);
            }
            sb.clone()
        };
        crate::block::write(2, &data)?;
        // キャッシュにあるスーパーブロックの入ったブロックにも映す
        self.sync_sb_cache(&data);
        Ok(())
    }

    /// 残っているトランザクションを本当の場所へ書き、ジャーナルを空にする
    fn journal_replay(&self, j: &mut Journal) -> Result<(), i64> {
        let mut pos = be32(&j.sb, 0x1c);
        let mut seq = be32(&j.sb, 0x18);
        // (トランザクション番号, 書き先, ジャーナルの位置, エスケープ)
        let mut writes: Vec<(u32, u64, u32, bool)> = Vec::new();
        let mut revoked: BTreeMap<u64, u32> = BTreeMap::new();
        let mut pending: Vec<(u32, u64, u32, bool)> = Vec::new();
        let next = |p: u32| if p + 1 >= j.maxlen { j.first } else { p + 1 };
        let mut n = 0;
        loop {
            n += 1;
            if n > j.maxlen {
                break;
            }
            let b = self.jread(j, pos)?;
            if be32(&b, 0) != MAGIC || be32(&b, 8) != seq {
                break;
            }
            match be32(&b, 4) {
                DESCRIPTOR => {
                    let tb = j.tag_bytes();
                    let end = self.bsize - if j.v3 { 4 } else { 0 };
                    let mut o = 12;
                    let mut dpos = next(pos);
                    while o + tb <= end {
                        let lo = be32(&b, o) as u64;
                        let (flags, hi) = if j.v3 {
                            (be32(&b, o + 4), be32(&b, o + 8) as u64)
                        } else {
                            (u16::from_be_bytes([b[o + 6], b[o + 7]]) as u32, if j.bit64 { be32(&b, o + 8) as u64 } else { 0 })
                        };
                        let target = lo | if j.bit64 { hi << 32 } else { 0 };
                        pending.push((seq, target, dpos, flags & FLAG_ESCAPE != 0));
                        dpos = next(dpos);
                        o += tb;
                        if flags & FLAG_SAME_UUID == 0 {
                            o += 16;
                        }
                        if flags & FLAG_LAST_TAG != 0 {
                            break;
                        }
                    }
                    pos = dpos;
                }
                COMMIT => {
                    writes.append(&mut pending);
                    seq = seq.wrapping_add(1);
                    pos = next(pos);
                }
                REVOKE => {
                    let count = be32(&b, 12) as usize;
                    let rsize = if j.bit64 { 8 } else { 4 };
                    let mut o = 16;
                    while o + rsize <= count.min(self.bsize) {
                        let blk = if j.bit64 { u64::from_be_bytes(b[o..o + 8].try_into().unwrap()) } else { be32(&b, o) as u64 };
                        let e = revoked.entry(blk).or_insert(seq);
                        *e = (*e).max(seq);
                        o += rsize;
                    }
                    pos = next(pos);
                }
                _ => break,
            }
        }
        let spb = (self.bsize / crate::block::SECTOR) as u64;
        let mut replayed = 0;
        for (s, target, jpos, escaped) in writes {
            if revoked.get(&target).is_some_and(|&r| r >= s) {
                continue;
            }
            let mut d = self.jread(j, jpos)?;
            if escaped {
                put_be32(&mut d, 0, MAGIC);
            }
            crate::block::write(target * spb, &d)?;
            replayed += 1;
        }
        println!("extfs: journal replayed {} blocks (up to transaction {})", replayed, seq.wrapping_sub(1));
        put_be32(&mut j.sb, 0x1c, 0);
        put_be32(&mut j.sb, 0x18, seq);
        j.seq = seq;
        self.write_jsb(j)?;
        // スーパーブロックとグループディスクリプタは再生で変わったかもしれないので読みなおす
        let mut sb = vec![0u8; 1024];
        crate::block::read(2, &mut sb)?;
        *self.sb.borrow_mut() = sb;
        Ok(())
    }

    /// meta のブロックを 1 つのトランザクションとしてジャーナルに書く。
    /// 入りきらなければ Ok(false) (呼ぶ側がジャーナルなしで書く)
    pub(super) fn journal_commit(&self, j: &mut Journal, meta: &[u64]) -> Result<bool, i64> {
        let tb = j.tag_bytes();
        let tail = if j.v3 { 4 } else { 0 };
        // 記述ブロック 1 つに入るタグの数 (先頭のタグの後ろには UUID が付く)
        let per_desc = (self.bsize - 12 - tail - 16) / tb;
        let ndesc = meta.len().div_ceil(per_desc);
        let total = ndesc + meta.len() + 1;
        if j.first as usize + total > j.maxlen as usize {
            return Ok(false);
        }
        let seq = j.seq;
        let mut pos = j.first;
        for chunk in meta.chunks(per_desc) {
            let mut desc = vec![0u8; self.bsize];
            j.header(&mut desc, DESCRIPTOR, seq);
            let mut datas = Vec::with_capacity(chunk.len());
            let mut o = 12;
            for (i, &blk) in chunk.iter().enumerate() {
                let mut d = self.read_block(blk)?;
                let mut flags = 0;
                if be32(&d, 0) == MAGIC {
                    put_be32(&mut d, 0, 0);
                    flags |= FLAG_ESCAPE;
                }
                if i > 0 {
                    flags |= FLAG_SAME_UUID;
                }
                if i + 1 == chunk.len() {
                    flags |= FLAG_LAST_TAG;
                }
                put_be32(&mut desc, o, blk as u32);
                if j.v3 {
                    put_be32(&mut desc, o + 4, flags);
                    put_be32(&mut desc, o + 8, (blk >> 32) as u32);
                    let c = crc32c(crc32c(j.seed, &seq.to_be_bytes()), &d);
                    put_be32(&mut desc, o + 12, c);
                } else {
                    desc[o + 6..o + 8].copy_from_slice(&(flags as u16).to_be_bytes());
                    if j.bit64 {
                        put_be32(&mut desc, o + 8, (blk >> 32) as u32);
                    }
                }
                o += tb;
                if i == 0 {
                    desc[o..o + 16].copy_from_slice(&j.uuid);
                    o += 16;
                }
                datas.push(d);
            }
            if j.v3 {
                let c = crc32c(j.seed, &desc);
                put_be32(&mut desc, self.bsize - 4, c);
            }
            self.jwrite(j, pos, &desc)?;
            pos += 1;
            for d in datas {
                self.jwrite(j, pos, &d)?;
                pos += 1;
            }
        }
        let mut commit = vec![0u8; self.bsize];
        j.header(&mut commit, COMMIT, seq);
        let now = super::now_secs() as u64;
        commit[48..56].copy_from_slice(&now.to_be_bytes());
        if j.v3 {
            let c = crc32c(j.seed, &commit);
            put_be32(&mut commit, 16, c);
        }
        self.jwrite(j, pos, &commit)?;
        // ここでトランザクションは確かになる: ジャーナルの頭を指し、回復の印を立てる
        put_be32(&mut j.sb, 0x1c, j.first);
        put_be32(&mut j.sb, 0x18, seq);
        self.write_jsb(j)?;
        self.set_recover(true)?;
        Ok(true)
    }

    /// 本当の場所へ書き終えたら、ジャーナルを空に戻す
    pub(super) fn journal_done(&self, j: &mut Journal) -> Result<(), i64> {
        j.seq = j.seq.wrapping_add(1);
        put_be32(&mut j.sb, 0x1c, 0);
        put_be32(&mut j.sb, 0x18, j.seq);
        self.write_jsb(j)?;
        self.set_recover(false)
    }
}

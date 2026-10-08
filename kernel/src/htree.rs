// ext4 の htree (dir_index): ディレクトリの名前のハッシュで、どの葉のブロックに入れるかを引く索引
//
// ブロック 0 (dx_root): "." と "..", 索引の情報 (hash_version, indirect_levels)、(hash, 論理ブロック) の並び
// indirect_levels が 1 なら、間に dx_node のブロックがもう 1 段ある。葉はふつうのディレクトリブロック。
// 名前を探すときも足すときも索引をたどって、1 つの葉だけを見る。葉がいっぱいならハッシュで半分に分けて、索引に入口を足す。
// 索引のブロックもいっぱいなら分け、根がいっぱいなら 1 段増やす (2 段まで。largedir の 3 段は使わない)。
// 名前を消すときは葉から消すだけ (Linux と同じく、索引は縮めない)。
use super::*;

const DX_HASH_LEGACY: u8 = 0;
const DX_HASH_HALF_MD4: u8 = 1;
const DX_HASH_TEA: u8 = 2;
/// superblock の s_flags: 名前のハッシュで char を unsigned として扱う
const FLAGS_UNSIGNED_HASH: u32 = 0x2;
/// 索引では決められない (全部を読んで探す)
const EAGAIN: i64 = 11;

/// このディレクトリのハッシュの求め方
#[derive(Clone, Copy)]
struct Hasher {
    version: u8,
    unsigned: bool,
    seed: [u32; 4],
}

impl Hasher {
    fn hash(&self, n: &[u8]) -> u32 {
        dirhash(n, self.version, self.unsigned, &self.seed)
    }
}

// ---- ハッシュ (Linux の fs/ext4/hash.c と同じ) ----

fn dx_hack_hash(name: &[u8], signed: bool) -> u32 {
    let (mut hash0, mut hash1) = (0x12a3fe2du32, 0x37abe8f9u32);
    for &c in name {
        let v = if signed { c as i8 as i32 } else { c as i32 };
        let mut hash = hash1.wrapping_add(hash0 ^ (v.wrapping_mul(7152373) as u32));
        if hash & 0x8000_0000 != 0 {
            hash = hash.wrapping_sub(0x7fff_ffff);
        }
        hash1 = hash0;
        hash0 = hash;
    }
    hash0 << 1
}

/// 名前を num 個の u32 に詰める (足りないところは長さから作った値)
fn str2hashbuf(msg: &[u8], num: usize, signed: bool) -> [u32; 8] {
    let len = msg.len();
    let mut pad = (len as u32) | ((len as u32) << 8);
    pad |= pad << 16;
    let mut out = [0u32; 8];
    let mut k = 0;
    let mut val = pad;
    let mut num = num as isize;
    for (i, &c) in msg.iter().take(num as usize * 4).enumerate() {
        let v = if signed { c as i8 as i32 as u32 } else { c as u32 };
        val = v.wrapping_add(val << 8);
        if i % 4 == 3 {
            out[k] = val;
            k += 1;
            val = pad;
            num -= 1;
        }
    }
    num -= 1;
    if num >= 0 {
        out[k] = val;
        k += 1;
    }
    while num > 0 {
        out[k] = pad;
        k += 1;
        num -= 1;
    }
    out
}

fn half_md4(buf: &mut [u32; 4], inp: &[u32; 8]) {
    let f = |x: u32, y: u32, z: u32| z ^ (x & (y ^ z));
    let g = |x: u32, y: u32, z: u32| (x & y).wrapping_add((x ^ y) & z);
    let h = |x: u32, y: u32, z: u32| x ^ y ^ z;
    let (mut a, mut b, mut c, mut d) = (buf[0], buf[1], buf[2], buf[3]);
    const K2: u32 = 0x5a82_7999;
    const K3: u32 = 0x6ed9_eba1;
    macro_rules! round {
        ($f:expr, $a:ident, $b:ident, $c:ident, $d:ident, $x:expr, $s:expr) => {
            $a = $a.wrapping_add($f($b, $c, $d)).wrapping_add($x).rotate_left($s)
        };
    }
    round!(f, a, b, c, d, inp[0], 3);
    round!(f, d, a, b, c, inp[1], 7);
    round!(f, c, d, a, b, inp[2], 11);
    round!(f, b, c, d, a, inp[3], 19);
    round!(f, a, b, c, d, inp[4], 3);
    round!(f, d, a, b, c, inp[5], 7);
    round!(f, c, d, a, b, inp[6], 11);
    round!(f, b, c, d, a, inp[7], 19);
    round!(g, a, b, c, d, inp[1].wrapping_add(K2), 3);
    round!(g, d, a, b, c, inp[3].wrapping_add(K2), 5);
    round!(g, c, d, a, b, inp[5].wrapping_add(K2), 9);
    round!(g, b, c, d, a, inp[7].wrapping_add(K2), 13);
    round!(g, a, b, c, d, inp[0].wrapping_add(K2), 3);
    round!(g, d, a, b, c, inp[2].wrapping_add(K2), 5);
    round!(g, c, d, a, b, inp[4].wrapping_add(K2), 9);
    round!(g, b, c, d, a, inp[6].wrapping_add(K2), 13);
    round!(h, a, b, c, d, inp[3].wrapping_add(K3), 3);
    round!(h, d, a, b, c, inp[7].wrapping_add(K3), 9);
    round!(h, c, d, a, b, inp[2].wrapping_add(K3), 11);
    round!(h, b, c, d, a, inp[6].wrapping_add(K3), 15);
    round!(h, a, b, c, d, inp[1].wrapping_add(K3), 3);
    round!(h, d, a, b, c, inp[5].wrapping_add(K3), 9);
    round!(h, c, d, a, b, inp[0].wrapping_add(K3), 11);
    round!(h, b, c, d, a, inp[4].wrapping_add(K3), 15);
    buf[0] = buf[0].wrapping_add(a);
    buf[1] = buf[1].wrapping_add(b);
    buf[2] = buf[2].wrapping_add(c);
    buf[3] = buf[3].wrapping_add(d);
}

fn tea(buf: &mut [u32; 4], inp: &[u32; 8]) {
    let (mut sum, mut b0, mut b1) = (0u32, buf[0], buf[1]);
    let (a, b, c, d) = (inp[0], inp[1], inp[2], inp[3]);
    for _ in 0..16 {
        sum = sum.wrapping_add(0x9e37_79b9);
        b0 = b0.wrapping_add(((b1 << 4).wrapping_add(a)) ^ (b1.wrapping_add(sum)) ^ ((b1 >> 5).wrapping_add(b)));
        b1 = b1.wrapping_add(((b0 << 4).wrapping_add(c)) ^ (b0.wrapping_add(sum)) ^ ((b0 >> 5).wrapping_add(d)));
    }
    buf[0] = buf[0].wrapping_add(b0);
    buf[1] = buf[1].wrapping_add(b1);
}

/// 名前のハッシュ (下の 1 ビットは 0。索引では「同じハッシュの続き」の印に使う)
pub fn dirhash(name: &[u8], version: u8, unsigned: bool, seed: &[u32; 4]) -> u32 {
    let mut buf = if seed.iter().any(|&s| s != 0) { *seed } else { [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476] };
    let signed = !unsigned;
    let hash = match version {
        DX_HASH_LEGACY => dx_hack_hash(name, signed),
        DX_HASH_HALF_MD4 => {
            let mut p = name;
            loop {
                half_md4(&mut buf, &str2hashbuf(p, 8, signed));
                if p.len() <= 32 {
                    break;
                }
                p = &p[32..];
            }
            buf[1]
        }
        _ => {
            let mut p = name;
            loop {
                tea(&mut buf, &str2hashbuf(p, 4, signed));
                if p.len() <= 16 {
                    break;
                }
                p = &p[16..];
            }
            buf[0]
        }
    };
    let hash = hash & !1;
    if hash == 0x7fff_ffff << 1 { (0x7fff_ffff - 1) << 1 } else { hash }
}

// ---- 索引のブロック ----

/// 索引のブロックの中の並び (根なら 0x20、dx_node なら 8 から)
struct Dx {
    /// 物理ブロック
    pb: u64,
    data: Vec<u8>,
    /// count / limit の位置
    off: usize,
}

impl Dx {
    fn limit(&self) -> usize {
        u16_at(&self.data, self.off) as usize
    }
    fn count(&self) -> usize {
        u16_at(&self.data, self.off + 2) as usize
    }
    fn set_count(&mut self, n: usize) {
        put16(&mut self.data, self.off + 2, n as u16);
    }
    /// i 番目の (hash, 論理ブロック)。0 番目の hash は 0 (その場所は count / limit)
    fn get(&self, i: usize) -> (u32, u32) {
        let o = self.off + i * 8;
        (if i == 0 { 0 } else { u32_at(&self.data, o) }, u32_at(&self.data, o + 4))
    }
    fn set(&mut self, i: usize, hash: u32, blk: u32) {
        let o = self.off + i * 8;
        if i > 0 {
            put32(&mut self.data, o, hash);
        }
        put32(&mut self.data, o + 4, blk);
    }
    /// hash が入るところ (hash 以下で最後のもの)
    fn find(&self, hash: u32) -> usize {
        let mut at = 0;
        for i in 1..self.count() {
            if self.get(i).0 > hash {
                break;
            }
            at = i;
        }
        at
    }
    /// at の次に入れる (空きがあること)
    fn insert_after(&mut self, at: usize, hash: u32, blk: u32) {
        let n = self.count();
        for i in (at + 1..n).rev() {
            let (h, b) = self.get(i);
            self.set(i + 1, h, b);
        }
        self.set(at + 1, hash, blk);
        self.set_count(n + 1);
    }
}

impl ExtFs {
    fn dx_limit(&self, off: usize) -> usize {
        (self.bsize - off) / 8 - if self.csum { 1 } else { 0 }
    }

    /// 索引のブロックのチェックサム (count / limit の後ろの dx_tail)
    fn dx_write(&self, dir: u32, generation: u32, dx: &mut Dx) -> Result<(), i64> {
        if self.csum {
            let t = dx.off + dx.limit() * 8;
            if t + 8 <= self.bsize {
                let size = dx.off + dx.count() * 8;
                let mut c = crc32c(self.inode_seed(dir, generation), &dx.data[..size]);
                c = crc32c(c, &dx.data[t..t + 4]);
                c = crc32c(c, &[0u8; 4]);
                put32(&mut dx.data, t + 4, c);
            }
        }
        let data = dx.data.clone();
        self.modify_block(dx.pb, |d| d.copy_from_slice(&data))
    }

    /// dx_node の空のブロック (偽のエントリ 1 つで、ディレクトリとして読むと空)
    fn dx_new_node(&self, dir: u32, r: &mut Raw) -> Result<(u32, Dx), i64> {
        let lb = (r.size() / self.bsize as u64) as u32;
        let pb = self.map(dir, r, lb as u64, true)?;
        r.set_size(r.size() + self.bsize as u64);
        let mut data = vec![0u8; self.bsize];
        put16(&mut data, 4, self.bsize as u16);
        let mut dx = Dx { pb, data, off: 8 };
        put16(&mut dx.data, 8, self.dx_limit(8) as u16);
        dx.set_count(0);
        Ok((lb, dx))
    }

    /// 根を読んで、名前のハッシュを求める関数と、根から葉への道を返す
    fn dx_descend(&self, dir: u32, r: &mut Raw, name: &str) -> Result<(Hasher, u32, Vec<(Dx, usize)>), i64> {
        let root_pb = self.map(dir, r, 0, false)?;
        let rdata = self.read_block(root_pb)?;
        let info_len = rdata[0x1d] as usize;
        let levels = rdata[0x1e] as usize;
        let off = 0x18 + info_len;
        if levels > 1 || off + 4 > self.bsize {
            return Err(-EROFS);
        }
        let mut version = rdata[0x1c];
        let (seed, sflags) = {
            let sb = self.sb.borrow();
            ([u32_at(&sb, 0xec), u32_at(&sb, 0xf0), u32_at(&sb, 0xf4), u32_at(&sb, 0xf8)], u32_at(&sb, 0x160))
        };
        let unsigned = sflags & FLAGS_UNSIGNED_HASH != 0;
        // 3 より上は unsigned の版 (ディスクに書かれていることもある)
        if version > DX_HASH_TEA {
            version -= 3;
        }
        if version > DX_HASH_TEA {
            // siphash (casefold) などは知らない
            return Err(-EROFS);
        }
        let hasher = Hasher { version, unsigned, seed };
        let hash = hasher.hash(name.as_bytes());

        // 根から葉へ
        let mut path: Vec<(Dx, usize)> = vec![];
        let mut dx = Dx { pb: root_pb, data: rdata, off };
        for level in 0..=levels {
            let at = dx.find(hash);
            let blk = dx.get(at).1;
            path.push((dx, at));
            if level == levels {
                break;
            }
            let pb = self.map(dir, r, blk as u64, false)?;
            dx = Dx { pb, data: self.read_block(pb)?, off: 8 };
        }
        Ok((hasher, hash, path))
    }

    /// 索引で名前を探す。Ok(None) はない、Err(EAGAIN) は索引では決められない (全部を読んで探す)
    pub(super) fn dx_find(&self, dir: u32, name: &str) -> Result<Option<u32>, i64> {
        Ok(self.dx_locate(dir, name)?.map(|(_, ino)| ino))
    }

    /// 名前を消す (葉の中で前のエントリに吸収させる)。索引で見つからなければ Err(EAGAIN)
    pub(super) fn dx_remove(&self, dir: u32, generation: u32, name: &str) -> Result<(), i64> {
        let Some((pb, _)) = self.dx_locate(dir, name)? else { return Err(-ENOENT) };
        let end = if self.csum { self.bsize - DIR_TAIL } else { self.bsize };
        let at = self.with_block(pb, |d| {
            let (mut o, mut prev) = (0, None);
            while o < end {
                let (i, rl, nl) = (u32_at(d, o), u16_at(d, o + 4) as usize, d[o + 6] as usize);
                if rl < 8 || o + rl > end {
                    break;
                }
                if i != 0 && &d[o + 8..o + 8 + nl] == name.as_bytes() {
                    return Some((o, rl, prev));
                }
                prev = Some((o, rl));
                o += rl;
            }
            None
        })?;
        let Some((o, rl, prev)) = at else { return Err(-ENOENT) };
        self.modify_dir_block(dir, generation, pb, |d| match prev {
            Some((p, prl)) => put16(d, p + 4, (prl + rl) as u16),
            None => put32(d, o, 0),
        })
    }

    /// 名前の入っている葉 (物理ブロック) と inode
    fn dx_locate(&self, dir: u32, name: &str) -> Result<Option<(u64, u32)>, i64> {
        let mut r = self.read_inode(dir)?;
        let Ok((_, hash, path)) = self.dx_descend(dir, &mut r, name) else { return Err(-EAGAIN) };
        let (node, at) = path.last().unwrap();
        let mut at = *at;
        loop {
            let pb = self.map(dir, &mut r, node.get(at).1 as u64, false)?;
            if let Some(ino) = self.block_find(pb, name.as_bytes())? {
                return Ok(Some((pb, ino)));
            }
            // 同じハッシュが次の葉に続いているか
            if at + 1 < node.count() {
                let h = node.get(at + 1).0;
                if h & 1 != 0 && h & !1 == hash {
                    at += 1;
                    continue;
                }
                return Ok(None);
            }
            // 次の dx_node に続いているかもしれない
            return Err(-EAGAIN);
        }
    }

    /// 1 ブロックのディレクトリがいっぱいになったら、索引つきに変える (Linux の make_indexed_dir と同じ)。
    /// ブロック 0 を根にし、名前はぜんぶ新しいブロック 1 (葉) へ移す。変えたら true
    pub(super) fn make_indexed(&self, dir: u32, r: &mut Raw) -> Result<bool, i64> {
        const COMPAT_DIR_INDEX: u32 = 0x20;
        let (compat, def_hash) = {
            let sb = self.sb.borrow();
            (u32_at(&sb, 0x5c), sb[0xfc])
        };
        if compat & COMPAT_DIR_INDEX == 0 || r.size() != self.bsize as u64 || r.flags() & INDEX_FL != 0 || def_hash > DX_HASH_TEA {
            return Ok(false);
        }
        let generation = r.gen_no();
        let root_pb = self.map(dir, r, 0, false)?;
        let all = self.leaf_entries(root_pb)?;
        // 先頭の 2 つが . と .. でなければ変えない
        if all.len() < 2 || all[0].name != b"." || all[1].name != b".." {
            return Ok(false);
        }
        let (dot, dotdot) = (all[0].ino, all[1].ino);
        let ents = &all[2..];
        let leaf_pb = self.map(dir, r, 1, true)?;
        r.set_size(2 * self.bsize as u64);
        self.leaf_write(dir, generation, leaf_pb, ents)?;
        // 根: "." (12) と ".." (残りぜんぶ) の後ろに索引
        let mut data = vec![0u8; self.bsize];
        put32(&mut data, 0, dot);
        put16(&mut data, 4, 12);
        data[6] = 1;
        data[7] = 2;
        data[8] = b'.';
        put32(&mut data, 12, dotdot);
        put16(&mut data, 16, (self.bsize - 12) as u16);
        data[18] = 2;
        data[19] = 2;
        data[20] = b'.';
        data[21] = b'.';
        data[0x1c] = def_hash;
        data[0x1d] = 8;
        data[0x1e] = 0;
        let mut root = Dx { pb: root_pb, data, off: 0x20 };
        put16(&mut root.data, 0x20, self.dx_limit(0x20) as u16);
        root.set_count(1);
        root.set(0, 0, 1);
        self.dx_write(dir, generation, &mut root)?;
        r.set_flags(r.flags() | INDEX_FL);
        self.write_inode(dir, r)?;
        Ok(true)
    }

    /// 索引つきのディレクトリに名前を足す
    pub(super) fn dx_add_entry(&self, dir: u32, r: &mut Raw, name: &str, ino: u32, mode: u32) -> Result<(), i64> {
        let generation = r.gen_no();
        let (hasher, hash, path) = self.dx_descend(dir, r, name)?;
        let hash_of = |n: &[u8]| hasher.hash(n);
        let (last, at) = path.last().unwrap();
        let leaf_lb = last.get(*at).1;
        let leaf_pb = self.map(dir, r, leaf_lb as u64, false)?;

        let entry = DirEnt { ino, name: name.as_bytes().to_vec(), ft: ftype(mode) };
        if self.leaf_insert(dir, generation, leaf_pb, &entry)? {
            return Ok(());
        }

        // 葉を分ける: ハッシュの順に並べて、後ろ半分を新しいブロックへ
        let mut ents = self.leaf_entries(leaf_pb)?;
        ents.sort_by_key(|e| hash_of(&e.name));
        let hashes: Vec<u32> = ents.iter().map(|e| hash_of(&e.name)).collect();
        let mut split = ents.len() / 2;
        // 同じハッシュは同じ側に (ずらせなければ「続き」の印をつける)
        while split > 0 && hashes[split] == hashes[split - 1] {
            split -= 1;
        }
        if split == 0 {
            split = ents.len() / 2;
        }
        let hash2 = hashes.get(split).copied().unwrap_or(hash);
        let continued = split > 0 && hashes[split - 1] == hash2;
        let (new_lb, new_pb) = {
            let lb = r.size() / self.bsize as u64;
            let pb = self.map(dir, r, lb, true)?;
            r.set_size(r.size() + self.bsize as u64);
            (lb as u32, pb)
        };
        let upper = ents.split_off(split);
        self.leaf_write(dir, generation, leaf_pb, &ents)?;
        self.leaf_write(dir, generation, new_pb, &upper)?;
        let target = if hash >= hash2 { new_pb } else { leaf_pb };
        if !self.leaf_insert(dir, generation, target, &entry)? {
            return Err(-ENOSPC);
        }
        self.write_inode(dir, r)?;
        self.dx_insert(dir, r, generation, path, hash2 | continued as u32, new_lb)
    }

    /// 索引に (hash, blk) を足す。path は根から、(ブロック, 入口の位置)
    fn dx_insert(&self, dir: u32, r: &mut Raw, generation: u32, mut path: Vec<(Dx, usize)>, hash: u32, blk: u32) -> Result<(), i64> {
        let depth = path.len();
        let (mut node, at) = path.pop().unwrap();
        if node.count() < node.limit() {
            node.insert_after(at, hash, blk);
            return self.dx_write(dir, generation, &mut node);
        }
        if depth == 1 {
            // 根がいっぱいで段がない: 根の中身をまるごと新しい dx_node へ移し、1 段増やす
            let (nlb, mut child) = self.dx_new_node(dir, r)?;
            let n = node.count();
            for i in 0..n {
                let (h, b) = node.get(i);
                child.set(i, h, b);
            }
            child.set_count(n);
            child.insert_after(at, hash, blk);
            node.set_count(1);
            node.set(0, 0, nlb);
            node.data[0x1e] = 1;
            self.write_inode(dir, r)?;
            self.dx_write(dir, generation, &mut child)?;
            return self.dx_write(dir, generation, &mut node);
        }
        // dx_node がいっぱい: 後ろ半分を新しい dx_node へ移し、根に入口を足す
        let (mut root, rat) = path.pop().unwrap();
        if root.count() >= root.limit() {
            // 3 段目 (largedir) は作らない
            return Err(-ENOSPC);
        }
        let (nlb, mut sib) = self.dx_new_node(dir, r)?;
        let n = node.count();
        let half = n / 2;
        let split_hash = node.get(half).0;
        for i in half..n {
            let (h, b) = node.get(i);
            sib.set(i - half, h, b);
        }
        sib.set_count(n - half);
        node.set_count(half);
        if at >= half {
            sib.insert_after(at - half, hash, blk);
        } else {
            node.insert_after(at, hash, blk);
        }
        // sib の 0 番目の hash は書かれない (根の入口の hash がそれ)
        root.insert_after(rat, split_hash, nlb);
        self.write_inode(dir, r)?;
        self.dx_write(dir, generation, &mut node)?;
        self.dx_write(dir, generation, &mut sib)?;
        self.dx_write(dir, generation, &mut root)
    }

    /// 葉のブロックの名前 (inode 0 と末尾の偽エントリは除く)
    fn leaf_entries(&self, pb: u64) -> Result<Vec<DirEnt>, i64> {
        self.with_block(pb, |d| {
            let mut out = vec![];
            let mut o = 0;
            while o + 8 <= self.bsize {
                let (i, rl, nl, ft) = (u32_at(d, o), u16_at(d, o + 4) as usize, d[o + 6] as usize, d[o + 7]);
                if rl < 8 || o + rl > self.bsize {
                    break;
                }
                if i != 0 {
                    out.push(DirEnt { ino: i, name: d[o + 8..o + 8 + nl.min(rl - 8)].to_vec(), ft });
                }
                o += rl;
            }
            out
        })
    }

    /// 葉のブロックを ents で書きなおす
    fn leaf_write(&self, dir: u32, generation: u32, pb: u64, ents: &[DirEnt]) -> Result<(), i64> {
        let end = if self.csum { self.bsize - DIR_TAIL } else { self.bsize };
        self.modify_dir_block(dir, generation, pb, |d| {
            self.init_dir_block(d);
            let mut o = 0;
            for (k, e) in ents.iter().enumerate() {
                let rl = if k + 1 == ents.len() { end - o } else { rec_len_for(e.name.len()) };
                put32(d, o, e.ino);
                put16(d, o + 4, rl as u16);
                d[o + 6] = e.name.len() as u8;
                d[o + 7] = e.ft;
                d[o + 8..o + 8 + e.name.len()].copy_from_slice(&e.name);
                o += rl;
            }
        })
    }

    /// 葉のすき間に入れる。入らなければ false
    fn leaf_insert(&self, dir: u32, generation: u32, pb: u64, e: &DirEnt) -> Result<bool, i64> {
        let need = rec_len_for(e.name.len());
        let end = if self.csum { self.bsize - DIR_TAIL } else { self.bsize };
        let slot = self.with_block(pb, |d| {
            let mut o = 0;
            while o < end {
                let (i, rl, nl) = (u32_at(d, o), u16_at(d, o + 4) as usize, d[o + 6] as usize);
                if rl < 8 || o + rl > end {
                    break;
                }
                let used = if i == 0 { 0 } else { rec_len_for(nl) };
                if rl - used >= need {
                    return Some((o, rl, used));
                }
                o += rl;
            }
            None
        })?;
        let Some((o, rl, used)) = slot else { return Ok(false) };
        self.modify_dir_block(dir, generation, pb, |d| {
            let at = if used == 0 {
                o
            } else {
                put16(d, o + 4, used as u16);
                o + used
            };
            put32(d, at, e.ino);
            put16(d, at + 4, (rl - used) as u16);
            d[at + 6] = e.name.len() as u8;
            d[at + 7] = e.ft;
            d[at + 8..at + 8 + e.name.len()].copy_from_slice(&e.name);
        })?;
        Ok(true)
    }
}

pub(super) struct DirEnt {
    ino: u32,
    name: Vec<u8>,
    ft: u8,
}

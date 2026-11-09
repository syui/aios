// LSM のシステムコール (Linux 6.8 から: security/lsm_syscalls.c と security.c の lsm_list_modules、lsm_get_self_attr、
// lsm_set_self_attr。libapparmor は /proc/self/attr より先に使う)。なくても動く (Linux の COND_SYSCALL): main.rs が
// syscall::register でつなぐ。答えるのは security.rs につないだ LSM
use crate::security::{lsms, LSM_CTX, LSM_FLAG_SINGLE, LSM_ID_CAPABILITY};

const E2BIG: i64 = 7;
const EINVAL: i64 = 22;
const EOPNOTSUPP: i64 = 95;

pub const SYSCALLS: &[u64] = {
    use crate::syscall::nr::*;
    &[LSM_GET_SELF_ATTR, LSM_SET_SELF_ATTR, LSM_LIST_MODULES]
};

pub fn sys(n: u64, a: &[u64; 6]) -> Result<i64, i64> {
    use crate::syscall::nr::*;
    match n {
        LSM_GET_SELF_ATTR => lsm_get_self_attr(a[0] as u32, a[1] as usize, a[2] as usize, a[3] as u32),
        LSM_SET_SELF_ATTR => lsm_set_self_attr(a[0] as u32, a[1] as usize, a[2] as u32, a[3] as u32),
        LSM_LIST_MODULES => lsm_list_modules(a[0] as usize, a[1] as usize, a[2] as u32),
        _ => Err(-38),
    }
}

use crate::proc::{get_user, put_user};

fn get_u32(va: usize) -> Result<u32, i64> {
    let mut b = [0u8; 4];
    get_user(va, &mut b)?;
    Ok(u32::from_le_bytes(b))
}

/// lsm_list_modules(ids, size, flags): 動いている LSM の番号 (/sys/kernel/security/lsm と同じ順)
fn lsm_list_modules(ids: usize, size: usize, flags: u32) -> Result<i64, i64> {
    if flags != 0 {
        return Err(-EINVAL);
    }
    let have = get_u32(size)?;
    let mut v = alloc::vec![LSM_ID_CAPABILITY];
    for l in lsms() {
        if l.enabled() {
            v.push(l.id().1);
        }
    }
    let total = (v.len() * 8) as u32;
    put_user(size, &total.to_le_bytes())?;
    if have < total {
        return Err(-E2BIG);
    }
    for (i, id) in v.iter().enumerate() {
        put_user(ids + i * 8, &id.to_le_bytes())?;
    }
    Ok(v.len() as i64)
}

/// lsm_get_self_attr(attr, ctx, size, flags): いまのプロセスの current / exec / prev を struct lsm_ctx で (答えるのは
/// ラベルを持つ LSM: 中身は /proc/self/attr と同じ「名前 (モード)」の改行なし)。ctx が 0 か size が足りなければ、要る
/// 大きさを size に書く (足りなければ E2BIG)。flags の LSM_FLAG_SINGLE なら ctx の id の LSM だけ。答えた LSM の数を返す
fn lsm_get_self_attr(attr: u32, ctx: usize, size: usize, flags: u32) -> Result<i64, i64> {
    if attr == 0 || size == 0 {
        return Err(-EINVAL);
    }
    let left = get_u32(size)?;
    let mut single = None;
    if flags != 0 {
        if flags != LSM_FLAG_SINGLE || ctx == 0 {
            return Err(-EINVAL);
        }
        let mut b = [0u8; 8];
        get_user(ctx, &mut b)?;
        let id = u64::from_le_bytes(b);
        if id == 0 {
            return Err(-EINVAL);
        }
        single = Some(id);
    }
    let (mut total, mut count, mut toobig) = (0u32, 0i64, false);
    for l in lsms() {
        let id = l.id().1;
        if !l.enabled() || single.is_some_and(|s| s != id) {
            continue;
        }
        let Some(text) = l.getselfattr(attr)? else { continue };
        let v = text.as_bytes();
        // lsm_fill_user_ctx: 頭と中身を 8 バイトにそろえて
        let len = (LSM_CTX + v.len()).div_ceil(8) * 8;
        if len as u32 > left {
            toobig = true;
        } else {
            if ctx != 0 {
                let mut b = alloc::vec![0u8; len];
                for (i, x) in [id, 0, len as u64, v.len() as u64].iter().enumerate() {
                    b[i * 8..i * 8 + 8].copy_from_slice(&x.to_le_bytes());
                }
                b[LSM_CTX..LSM_CTX + v.len()].copy_from_slice(v);
                put_user(ctx, &b)?;
            }
            count += 1;
        }
        total += len as u32;
    }
    put_user(size, &total.to_le_bytes())?;
    if toobig {
        return Err(-E2BIG);
    }
    if count == 0 {
        return Err(-EOPNOTSUPP);
    }
    Ok(count)
}

/// lsm_set_self_attr(attr, ctx, size, flags): ctx の id の LSM に current か exec を書く (AppArmor なら
/// /proc/self/attr に書くのと同じ "changeprofile NAME" など)
fn lsm_set_self_attr(attr: u32, ctx: usize, size: u32, flags: u32) -> Result<i64, i64> {
    if flags != 0 || (size as usize) < LSM_CTX {
        return Err(-EINVAL);
    }
    if size > 4096 {
        return Err(-E2BIG);
    }
    let mut b = alloc::vec![0u8; size as usize];
    get_user(ctx, &mut b)?;
    let field = |i: usize| u64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap());
    let (id, len, ctx_len) = (field(0), field(2), field(3));
    let need = (LSM_CTX as u64).checked_add(ctx_len);
    if (size as u64) < len || need.is_none_or(|n| len < n) {
        return Err(-EINVAL);
    }
    let Some(l) = lsms().into_iter().find(|l| l.enabled() && l.id().1 == id) else { return Err(-EOPNOTSUPP) };
    l.setselfattr(attr, &b[LSM_CTX..LSM_CTX + ctx_len as usize])?;
    Ok(0)
}

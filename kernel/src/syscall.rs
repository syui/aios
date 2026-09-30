// aarch64 Linux 互換のシステムコール
// 番号は x8、引数は x0..x5、戻り値は x0 (エラーは -errno)
use crate::proc;
use crate::trap::TrapFrame;

const SYS_WRITE: u64 = 64;
const SYS_EXIT: u64 = 93;
const SYS_EXIT_GROUP: u64 = 94;

const EBADF: i64 = 9;
const EFAULT: i64 = 14;
const ENOSYS: i64 = 38;

pub fn dispatch(tf: &mut TrapFrame) {
    let a = tf.x;
    let ret = match tf.x[8] {
        SYS_WRITE => sys_write(a[0], a[1] as usize, a[2] as usize),
        SYS_EXIT | SYS_EXIT_GROUP => proc::exit(a[0] as i32),
        nr => {
            println!("syscall: unknown {}", nr);
            -ENOSYS
        }
    };
    tf.x[0] = ret as u64;
}

fn sys_write(fd: u64, buf: usize, len: usize) -> i64 {
    if fd != 1 && fd != 2 {
        return -EBADF;
    }
    let p = proc::current();
    let mut chunk = [0u8; 128];
    let mut done = 0;
    while done < len {
        let n = chunk.len().min(len - done);
        if p.pagetable.copy_in(&mut chunk[..n], buf + done).is_none() {
            return if done == 0 { -EFAULT } else { done as i64 };
        }
        let _g = crate::uart::LOCK.lock();
        for &c in &chunk[..n] {
            if c == b'\n' {
                crate::uart::putc(b'\r');
            }
            crate::uart::putc(c);
        }
        done += n;
    }
    done as i64
}

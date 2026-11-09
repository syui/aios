// SysRq (Linux の CONFIG_MAGIC_SYSRQ: drivers/tty/sysrq.c の handle_sysrq)。シリアルの Ctrl-P (p: プロセスの一覧) と
// Ctrl-] (t: すべてのスレッドの状態。シェルが動かなくても、カーネルが生きていれば出る) から。main.rs が
// debug::register_sysrq でつなぐ。/proc/threads も同じもの (threads_text)
use crate::proc;

pub fn handle(key: u8) {
    match key {
        b'p' => dump(),
        b't' => crate::println!("\n{}", threads_text()),
        _ => {}
    }
}

/// Ctrl-P: プロセスの一覧 (デバッグ用)
fn dump() {
    crate::println!();
    for (i, p) in proc::procs().iter().enumerate() {
        if p.state == proc::State::Unused {
            continue;
        }
        let st = match p.state {
            proc::State::Runnable => "runnable",
            proc::State::Running => "running",
            proc::State::Sleeping => "sleeping",
            proc::State::Zombie => "zombie",
            proc::State::Unused => "unused",
        };
        let tf = p.tf_ref();
        crate::println!("[{}] pid {} tgid {} ppid {} {} chan {:#x} wake_at {} pc {:#x} x8 {} x0 {:#x}", i, p.pid, p.tgid, p.ppid, st, p.chan, p.wake_at, tf.elr, tf.x[8], tf.x[0]);
    }
    crate::println!("ticks {} current {:?}", crate::timer::ticks(), proc::cur());
}

pub fn threads_text() -> alloc::string::String {
    let mut out = alloc::format!("ticks {}\n  PID  TGID ST CHAN             SYSCALL ARG0             ARG1                 WAKE     RUNS NAME\n", crate::timer::ticks());
    for p in proc::live() {
        if p.state == proc::State::Unused {
            continue;
        }
        let st = match p.state {
            proc::State::Running | proc::State::Runnable => 'R',
            proc::State::Sleeping if p.nointr => 'D',
            proc::State::Sleeping => 'S',
            proc::State::Zombie => 'Z',
            proc::State::Unused => 'X',
        };
        let n = p.comm.iter().position(|&c| c == 0).unwrap_or(16);
        out.push_str(&alloc::format!(
            "{:5} {:5} {}  {:16x} {:7} {:16x} {:16x} {:8} {:8} {}\n",
            p.pid,
            p.tgid,
            st,
            p.chan,
            p.last_sys.0,
            p.last_sys.1,
            p.last_sys.2,
            p.wake_at,
            p.nrun,
            core::str::from_utf8(&p.comm[..n]).unwrap_or("?")
        ));
        // poll / select で眠っているなら、見張っている印 (パイプは pipe:[N] の N と同じ)。* は何でも
        if p.state == proc::State::Sleeping && p.chan == proc::poll_chan() {
            match &p.poll_keys {
                None => out.push_str("      poll *\n"),
                Some(k) => {
                    let ks: alloc::vec::Vec<alloc::string::String> = k.iter().take(16).map(|k| alloc::format!("{}", k & 0xffffff)).collect();
                    out.push_str(&alloc::format!("      poll {}\n", ks.join(" ")));
                }
            }
        }
    }
    out
}

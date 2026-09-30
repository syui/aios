// コンソール (PL011) の入力。行編集は tty の行規律がする
use crate::proc;

const CTRL_P: u8 = 0x10;

/// 割り込みから 1 文字ずつ
pub fn intr(c: u8) {
    match c {
        CTRL_P => proc::dump(),
        _ => crate::tty::console_input(c),
    }
}

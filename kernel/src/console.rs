// コンソール (PL011) の入力。行編集は tty の行規律がする
// Ctrl-P はプロセスの一覧 (デバッグ用: SysRq の p)。ただし端末が 1 文字ずつのモードなら、ふつうの文字として渡す

const CTRL_P: u8 = 0x10;

/// 割り込みから 1 文字ずつ
pub fn intr(c: u8) {
    match c {
        // 1 文字ずつ読んでいるプログラム (シェルの C-p など) には、そのまま渡す
        CTRL_P if crate::tty::console_canonical() => crate::debug::sysrq(b'p'),
        _ => crate::tty::console_input(c),
    }
}

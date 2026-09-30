// aios の最初のプロセス: シェルを起こし、終わったら起こしなおす
use std::ffi::CString;

fn main() {
    println!("aios init (pid {})", std::process::id());
    let sh = CString::new("/bin/sh").unwrap();
    let argv = [sh.as_ptr(), std::ptr::null()];
    let envp = [c"PATH=/usr/bin:/bin".as_ptr(), c"HOME=/".as_ptr(), c"TERM=vt100".as_ptr(), std::ptr::null()];
    loop {
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            unsafe {
                libc::execve(sh.as_ptr(), argv.as_ptr(), envp.as_ptr());
                libc::_exit(127);
            }
        }
        // シェルも、親をなくした子も、ここで刈り取る
        loop {
            let mut st = 0;
            let w = unsafe { libc::waitpid(-1, &mut st, 0) };
            if w == pid || w < 0 {
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

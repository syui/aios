// aios の最初のプロセス
use std::ffi::CString;

fn spawn(path: &str, args: &[&str]) -> i32 {
    let path = CString::new(path).unwrap();
    let args: Vec<CString> = args.iter().map(|a| CString::new(*a).unwrap()).collect();
    let mut argv: Vec<*const libc::c_char> = args.iter().map(|a| a.as_ptr()).collect();
    argv.push(std::ptr::null());
    let envp = [c"PATH=/bin".as_ptr(), std::ptr::null()];
    unsafe {
        let pid = libc::fork();
        if pid == 0 {
            libc::execve(path.as_ptr(), argv.as_ptr(), envp.as_ptr());
            libc::_exit(127);
        }
        pid
    }
}

fn wait() -> (i32, i32) {
    let mut status = 0;
    let pid = unsafe { libc::waitpid(-1, &mut status, 0) };
    (pid, libc::WEXITSTATUS(status))
}

fn main() {
    println!("aios init (pid {})", std::process::id());

    let a = spawn("/bin/hello", &["hello", "one"]);
    let b = spawn("/bin/hello", &["hello", "two", "three"]);
    println!("init: spawned {} and {}", a, b);
    for _ in 0..2 {
        let (pid, code) = wait();
        println!("init: pid {} exited with {}", pid, code);
    }

    let c = spawn("/bin/nothing", &["nothing"]);
    let (pid, code) = wait();
    println!("init: pid {} (spawned {}) exited with {}", pid, c, code);

    println!("init: done, sleeping");
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}

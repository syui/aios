// ジョブ: パイプラインごとのプロセスグループ、fg / bg / jobs / wait / kill %N
use std::io;

#[derive(Clone, Copy, PartialEq)]
pub enum JobState {
    Running,
    Stopped,
    Done(i32),
}

pub struct Job {
    pub id: usize,
    /// プロセスグループ (ジョブ制御がなければ 0)
    pub pgid: i32,
    /// (pid, 終わっていれば終了ステータス)
    pub pids: Vec<(i32, Option<i32>)>,
    pub cmd: String,
    pub state: JobState,
    /// 状態の変化をもう知らせたか
    pub notified: bool,
    /// 止まったときの端末の設定 (fg で戻す)
    pub tmodes: Option<libc::termios>,
}

static mut JOBS: Vec<Job> = Vec::new();
pub static mut INTERACTIVE: bool = false;
pub static mut LAST_BG: i32 = 0;
/// シェル自身の端末の設定
pub static mut SHELL_TMODES: Option<libc::termios> = None;

pub fn jobs() -> &'static mut Vec<Job> {
    unsafe { &mut *(&raw mut JOBS) }
}

pub fn interactive() -> bool {
    unsafe { INTERACTIVE }
}

pub fn exit_code(st: i32) -> i32 {
    if libc::WIFSIGNALED(st) { 128 + libc::WTERMSIG(st) } else { libc::WEXITSTATUS(st) }
}

/// waitpid で分かったことをジョブに書く
/// set -o pipefail
pub static mut PIPEFAIL: bool = false;

pub fn record(pid: i32, st: i32) {
    for j in jobs().iter_mut() {
        let Some(k) = j.pids.iter().position(|(p, _)| *p == pid) else { continue };
        if libc::WIFSTOPPED(st) {
            j.state = JobState::Stopped;
            j.notified = false;
        } else if libc::WIFCONTINUED(st) {
            j.state = JobState::Running;
        } else {
            j.pids[k].1 = Some(exit_code(st));
            if j.pids.iter().all(|(_, s)| s.is_some()) {
                // パイプラインのステータスは最後のコマンドのもの (set -o pipefail なら、しくじった最後のもの)
                let last = j.pids.last().unwrap().1.unwrap();
                let st = if unsafe { PIPEFAIL } { j.pids.iter().rev().filter_map(|(_, s)| *s).find(|&s| s != 0).unwrap_or(0) } else { last };
                j.state = JobState::Done(st);
                j.notified = false;
            }
        }
        return;
    }
}

pub fn set_tmodes(t: &Option<libc::termios>) {
    if let Some(t) = t {
        unsafe { libc::tcsetattr(0, libc::TCSADRAIN, t) };
    }
}

/// 前のジョブが終わるか止まるまで待つ
pub fn foreground(idx: usize) -> i32 {
    let status = loop {
        let j = &mut jobs()[idx];
        match j.state {
            JobState::Done(s) => {
                jobs().remove(idx);
                break s;
            }
            JobState::Stopped => {
                j.notified = true;
                if interactive() {
                    let mut t: libc::termios = unsafe { std::mem::zeroed() };
                    if unsafe { libc::tcgetattr(0, &mut t) } == 0 {
                        j.tmodes = Some(t);
                    }
                    eprintln!("\n[{}]+  {:<24}{}", j.id, state_text(j.state), j.cmd);
                }
                break 128 + libc::SIGTSTP;
            }
            JobState::Running => {}
        }
        let Some(&(pid, _)) = j.pids.iter().find(|(_, s)| s.is_none()) else {
            j.state = JobState::Done(0);
            continue;
        };
        let mut st = 0;
        let r = unsafe { libc::waitpid(pid, &mut st, libc::WUNTRACED) };
        if r > 0 {
            record(r, st);
        } else if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            // もう待てない子 (誰かが先に回収した)
            if let Some(e) = jobs()[idx].pids.iter_mut().find(|(p, _)| *p == pid) {
                e.1 = Some(0);
            }
            let j = &mut jobs()[idx];
            if j.pids.iter().all(|(_, s)| s.is_some()) {
                j.state = JobState::Done(j.pids.last().unwrap().1.unwrap());
            }
        }
        // Ctrl-C でシェルのハンドラが動くと EINTR で戻るので、待ちなおす
    };
    if interactive() {
        // 端末をシェルに戻し、設定も戻す
        unsafe { libc::tcsetpgrp(0, libc::getpgrp()) };
        set_tmodes(unsafe { &*(&raw const SHELL_TMODES) });
    }
    status
}

/// うしろのジョブの様子を集めて、変わったものを知らせる (プロンプトの前に)
pub fn report_jobs() {
    loop {
        let mut st = 0;
        let r = unsafe { libc::waitpid(-1, &mut st, libc::WNOHANG | libc::WUNTRACED | libc::WCONTINUED) };
        if r <= 0 {
            break;
        }
        record(r, st);
    }
    jobs().retain_mut(|j| {
        if j.notified {
            return true;
        }
        j.notified = true;
        if j.state != JobState::Running {
            eprintln!("[{}]+  {:<24}{}", j.id, state_text(j.state), j.cmd);
        }
        !matches!(j.state, JobState::Done(_))
    });
}

/// jobs などに出す状態
pub fn state_text(s: JobState) -> String {
    match s {
        JobState::Running => "Running".into(),
        JobState::Stopped => "Stopped".into(),
        JobState::Done(0) => "Done".into(),
        JobState::Done(s) if s > 128 => match s - 128 {
            libc::SIGHUP => "Hangup".into(),
            libc::SIGINT => "Interrupt".into(),
            libc::SIGQUIT => "Quit".into(),
            libc::SIGKILL => "Killed".into(),
            libc::SIGSEGV => "Segmentation fault".into(),
            libc::SIGPIPE => "Broken pipe".into(),
            libc::SIGTERM => "Terminated".into(),
            n => format!("Signal {}", n),
        },
        JobState::Done(s) => format!("Exit {}", s),
    }
}

/// %N / %% / 省略 (いちばん新しいジョブ) からジョブの添字
pub fn find_job(arg: Option<&String>) -> Result<usize, String> {
    let js = jobs();
    let spec = arg.map(|s| s.as_str()).unwrap_or("%%");
    let idx = match spec {
        "%%" | "%+" | "%" => js.len().checked_sub(1),
        s => s.strip_prefix('%').and_then(|n| n.parse::<usize>().ok()).and_then(|n| js.iter().position(|j| j.id == n)),
    };
    idx.ok_or_else(|| format!("{}: no such job", spec))
}

pub fn job_builtin(args: &[String]) -> Option<i32> {
    let name = args[0].as_str();
    let res = match name {
        "jobs" => {
            report_jobs_quiet();
            for j in jobs().iter() {
                println!("[{}]   {:<24}{}", j.id, state_text(j.state), j.cmd);
            }
            // 終わったものは一度見せたら消す
            jobs().retain(|j| !matches!(j.state, JobState::Done(_)));
            Ok(0)
        }
        "fg" | "bg" => find_job(args.get(1)).map(|idx| {
            let j = &mut jobs()[idx];
            let target = if j.pgid > 0 { -j.pgid } else { j.pids[0].0 };
            if name == "fg" {
                println!("{}", j.cmd);
                if interactive() && j.pgid > 0 {
                    unsafe { libc::tcsetpgrp(0, j.pgid) };
                    set_tmodes(&j.tmodes.take());
                }
                j.state = JobState::Running;
                unsafe { libc::kill(target, libc::SIGCONT) };
                foreground(idx)
            } else {
                println!("[{}]+ {} &", j.id, j.cmd);
                j.state = JobState::Running;
                unsafe { libc::kill(target, libc::SIGCONT) };
                0
            }
        }),
        "wait" => {
            let mut s = 0;
            while let Some(idx) = jobs().iter().position(|j| j.state == JobState::Running) {
                s = foreground_quiet(idx);
            }
            Ok(s)
        }
        // kill %N はジョブのグループへ。それ以外は /bin/kill に任せる
        "kill" if args.iter().any(|a| a.starts_with('%')) => {
            let sig = args.get(1).filter(|a| a.starts_with('-')).map(|a| signal_number(&a[1..]));
            let sig = match sig {
                Some(Some(s)) => s,
                Some(None) => {
                    eprintln!("kill: {}: invalid signal", args[1]);
                    return Some(1);
                }
                None => libc::SIGTERM,
            };
            let mut st = 0;
            for a in args[1..].iter().filter(|a| a.starts_with('%')) {
                match find_job(Some(a)) {
                    Ok(idx) => {
                        let j = &jobs()[idx];
                        let target = if j.pgid > 0 { -j.pgid } else { j.pids[0].0 };
                        unsafe { libc::kill(target, sig) };
                        if sig != libc::SIGCONT && j.state == JobState::Stopped {
                            unsafe { libc::kill(target, libc::SIGCONT) };
                        }
                    }
                    Err(e) => {
                        eprintln!("kill: {}", e);
                        st = 1;
                    }
                }
            }
            Ok(st)
        }
        _ => return None,
    };
    Some(res.unwrap_or_else(|e| {
        eprintln!("{}: {}", name, e);
        1
    }))
}

pub fn signal_number(s: &str) -> Option<i32> {
    if let Ok(n) = s.parse() {
        return Some(n);
    }
    let s = s.strip_prefix("SIG").unwrap_or(s);
    Some(match s {
        "HUP" => libc::SIGHUP,
        "INT" => libc::SIGINT,
        "QUIT" => libc::SIGQUIT,
        "KILL" => libc::SIGKILL,
        "TERM" => libc::SIGTERM,
        "STOP" => libc::SIGSTOP,
        "TSTP" => libc::SIGTSTP,
        "CONT" => libc::SIGCONT,
        "USR1" => libc::SIGUSR1,
        "USR2" => libc::SIGUSR2,
        _ => return None,
    })
}

/// jobs の前に、終わったものの状態だけ集める (知らせは jobs の表示で)
pub fn report_jobs_quiet() {
    loop {
        let mut st = 0;
        let r = unsafe { libc::waitpid(-1, &mut st, libc::WNOHANG | libc::WUNTRACED | libc::WCONTINUED) };
        if r <= 0 {
            break;
        }
        record(r, st);
    }
    for j in jobs().iter_mut() {
        j.notified = true;
    }
}

/// wait 用: 端末は動かさずに待つ
pub fn foreground_quiet(idx: usize) -> i32 {
    let was = unsafe { INTERACTIVE };
    unsafe { INTERACTIVE = false };
    let s = foreground(idx);
    unsafe { INTERACTIVE = was };
    s
}

pub fn last_bg() -> i32 {
    unsafe { LAST_BG }
}

/// 作ったプロセスをジョブにする。前で動くなら終わるか止まるまで待つ
pub fn add_job(pgid: i32, pids: Vec<i32>, text: &str, bg: bool) -> i32 {
    if pids.is_empty() {
        return 1;
    }
    let id = jobs().iter().map(|j| j.id).max().unwrap_or(0) + 1;
    let last_pid = *pids.last().unwrap();
    jobs().push(Job { id, pgid, pids: pids.into_iter().map(|p| (p, None)).collect(), cmd: text.to_string(), state: JobState::Running, notified: true, tmodes: None });
    if bg {
        unsafe { LAST_BG = last_pid };
        if interactive() {
            eprintln!("[{}] {}", id, last_pid);
        }
        return 0;
    }
    foreground(jobs().len() - 1)
}

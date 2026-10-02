// キーボードやマウス (/dev/input/eventN、Linux の evdev と同じ形)
#![allow(dead_code)]
use std::fs::File;
use std::io::Read;
use std::os::fd::AsRawFd;

pub const EV_KEY: u16 = 1;

pub struct Event {
    pub typ: u16,
    pub code: u16,
    pub value: i32,
}

pub struct Inputs {
    files: Vec<File>,
}

impl Inputs {
    /// /dev/input/event* をぜんぶ開く
    pub fn open() -> Inputs {
        let mut files = vec![];
        for n in 0..16 {
            match File::open(format!("/dev/input/event{}", n)) {
                Ok(f) => files.push(f),
                Err(_) => break,
            }
        }
        Inputs { files }
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// イベントが来るまで (timeout ミリ秒、-1 でずっと) 待って、来たものを返す
    pub fn wait(&mut self, timeout: i32) -> Vec<Event> {
        let mut pfd: Vec<libc::pollfd> = self.files.iter().map(|f| libc::pollfd { fd: f.as_raw_fd(), events: libc::POLLIN, revents: 0 }).collect();
        let mut out = vec![];
        if unsafe { libc::poll(pfd.as_mut_ptr(), pfd.len() as _, timeout) } <= 0 {
            return out;
        }
        for (i, p) in pfd.iter().enumerate() {
            if p.revents & libc::POLLIN == 0 {
                continue;
            }
            let mut b = [0u8; 24 * 32];
            let n = self.files[i].read(&mut b).unwrap_or(0);
            for e in b[..n - n % 24].chunks(24) {
                out.push(Event {
                    typ: u16::from_le_bytes([e[16], e[17]]),
                    code: u16::from_le_bytes([e[18], e[19]]),
                    value: i32::from_le_bytes(e[20..24].try_into().unwrap()),
                });
            }
        }
        out
    }
}

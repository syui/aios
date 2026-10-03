// プラグイン: aish の機能を足す別のプログラム (shell/README.md)
//   plugin NAME [ARGS...]    起こしてつなぐ (~/.aishrc)。NAME は $AISH_PLUGIN_PATH
//                            (なければ ~/.local/lib/aish/plugins:/usr/lib/aish/plugins) か PATH から探す
//   plugin                   つないでいるものの一覧
//   bindkey KEY PLUGIN:WIDGET / bindkey -r KEY / bindkey   キーとプラグインの機能を結ぶ / 外す / 一覧
// プラグインの標準入力と標準出力はパイプでつなぎっぱなしにして、1 行 1 つの JSON で話す。
// aish が { "ev": ..., ... } を送り、プラグインは 1 行の JSON で答える。最初は hello で、答えに
// 受けとるフック (hooks) と既定のキー (keys) が入っている。そのあとはフックのあるものだけに送る。
// 決まった時間に答えなければ (key と not_found は待ちつづける)、そのプラグインは止めて外す
// 版 2: hello の答えに tools (端末なしで呼べる機能)。aish --mcp はそれを MCP のツールとして見せ、
// { "ev": "tool", "name": ..., "args": {...} } で呼ぶ
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;

/// 答えを待つ長さ (ms)。key と not_found は端末で人と話すので待ちつづける
const TIMEOUT_MS: i32 = 3000;

struct Plugin {
    name: String,
    prog: String,
    child: std::process::Child,
    w: std::process::ChildStdin,
    r: std::process::ChildStdout,
    /// 読んだが、まだ行になっていないもの
    buf: Vec<u8>,
    hooks: Vec<String>,
    /// 端末なしで呼べる機能 ({ name, description, input })
    tools: Vec<Value>,
    alive: bool,
}

/// キーの並び ("C-p C-p" なら 2 つ) と、プラグインの機能
pub struct Binding {
    pub keys: Vec<String>,
    plugin: usize,
    widget: String,
}

#[derive(Default)]
pub struct Plugins {
    list: Vec<Plugin>,
    pub binds: Vec<Binding>,
    /// プラグインとつないだシェルのプロセス。fork した子 (パイプラインや $(...)) からは話さない
    owner: i32,
}

impl Plugin {
    /// 1 行送って 1 行の答え。答えがなければ (閉じた、時間切れ、JSON でない) None
    fn ask(&mut self, ev: &Value, wait: bool) -> Option<Value> {
        if !self.alive {
            return None;
        }
        let mut line = ev.to_string();
        line.push('\n');
        if self.w.write_all(line.as_bytes()).and_then(|_| self.w.flush()).is_err() {
            self.die("closed");
            return None;
        }
        loop {
            if let Some(i) = self.buf.iter().position(|&b| b == b'\n') {
                let l: Vec<u8> = self.buf.drain(..=i).collect();
                return match serde_json::from_slice::<Value>(&l) {
                    Ok(v) if v.is_object() => Some(v),
                    _ => {
                        self.die("answered something that is not a JSON object");
                        None
                    }
                };
            }
            let mut p = libc::pollfd { fd: self.r.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            let n = unsafe { libc::poll(&mut p, 1, if wait { -1 } else { TIMEOUT_MS }) };
            if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            if n == 0 {
                self.die("did not answer in time");
                return None;
            }
            let mut b = [0u8; 4096];
            match self.r.read(&mut b) {
                Ok(0) | Err(_) => {
                    self.die("closed");
                    return None;
                }
                Ok(k) => self.buf.extend_from_slice(&b[..k]),
            }
        }
    }

    fn die(&mut self, why: &str) {
        if self.alive {
            eprintln!("{}: plugin {}: {} (stopped)", super::shell_name(), self.name, why);
            self.alive = false;
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// プラグインのプログラムを探す
fn find(name: &str, home: &str, path_var: Option<String>, path: &str) -> Option<String> {
    if name.contains('/') {
        return Some(name.to_string());
    }
    let dirs = path_var.unwrap_or_else(|| format!("{}/.local/lib/aish/plugins:/usr/lib/aish/plugins", home));
    dirs.split(':').chain(path.split(':')).filter(|d| !d.is_empty()).map(|d| format!("{}/{}", d, name)).find(|p| super::is_exec(p))
}

/// キーの名前を、そろえた形に ("ctrl-r" "^R" "C-R" → "C-r"、"alt-f" → "M-f")
pub fn norm_key(k: &str) -> String {
    k.split_whitespace()
        .map(|one| {
            let low = one.to_lowercase();
            if let Some(c) = low.strip_prefix("c-").or(low.strip_prefix("ctrl-")).or(low.strip_prefix('^')) {
                format!("C-{}", c)
            } else if let Some(c) = one.strip_prefix("M-").or(one.strip_prefix("m-")).or(one.strip_prefix("alt-")) {
                format!("M-{}", c)
            } else {
                one.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

impl Plugins {
    fn mine(&self) -> bool {
        self.owner != 0 && unsafe { libc::getpid() } == self.owner
    }

    /// プラグインを起こして hello を送る。hello に足すもの (histfile など) は extra
    pub fn load(&mut self, name: &str, args: &[String], home: &str, path_var: Option<String>, path: &str, extra: Value) -> Result<(), String> {
        let prog = find(name, home, path_var, path).ok_or_else(|| format!("{}: not found", name))?;
        if self.owner == 0 {
            self.owner = unsafe { libc::getpid() };
        }
        if !self.mine() {
            return Err("plugins can be loaded only by the interactive shell".into());
        }
        let mut cmd = std::process::Command::new(&prog);
        cmd.args(args).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped());
        // Ctrl-C は打っている人がシェルに送るもの。プラグインは受けない
        unsafe {
            cmd.pre_exec(|| {
                libc::signal(libc::SIGINT, libc::SIG_IGN);
                libc::signal(libc::SIGPIPE, libc::SIG_DFL);
                Ok(())
            });
        }
        let mut child = cmd.spawn().map_err(|e| format!("{}: {}", prog, e))?;
        let (w, r) = (child.stdin.take().unwrap(), child.stdout.take().unwrap());
        let mut p = Plugin { name: name.to_string(), prog, child, w, r, buf: Vec::new(), hooks: Vec::new(), tools: Vec::new(), alive: true };
        let mut hello = json!({ "ev": "hello", "version": 2, "shell": "aish", "args": args, "home": home });
        if let (Some(h), Some(e)) = (hello.as_object_mut(), extra.as_object()) {
            h.extend(e.clone());
        }
        let Some(reply) = p.ask(&hello, false) else { return Err(format!("{}: no answer to hello", name)) };
        if let Some(n) = reply["name"].as_str() {
            p.name = n.to_string();
        }
        p.hooks = reply["hooks"].as_array().map(|a| a.iter().filter_map(|h| h.as_str().map(String::from)).collect()).unwrap_or_default();
        p.tools = reply["tools"].as_array().cloned().unwrap_or_default().into_iter().filter(|t| t["name"].is_string()).collect();
        let idx = self.list.len();
        // 既定のキー (あとから bindkey で変えられる)
        if let Some(keys) = reply["keys"].as_object() {
            for (k, w) in keys {
                if let Some(w) = w.as_str() {
                    self.bind(&norm_key(k), idx, w);
                }
            }
        }
        self.list.push(p);
        Ok(())
    }

    fn bind(&mut self, keys: &str, plugin: usize, widget: &str) {
        let keys: Vec<String> = keys.split_whitespace().map(String::from).collect();
        self.binds.retain(|b| b.keys != keys);
        self.binds.push(Binding { keys, plugin, widget: widget.to_string() });
    }

    /// bindkey KEY PLUGIN:WIDGET
    pub fn bindkey(&mut self, key: &str, target: &str) -> Result<(), String> {
        let (pname, widget) = target.split_once(':').ok_or_else(|| format!("{}: give PLUGIN:WIDGET", target))?;
        let idx = self.list.iter().position(|p| p.name == pname || p.prog.rsplit('/').next() == Some(pname)).ok_or_else(|| format!("{}: no such plugin", pname))?;
        self.bind(&norm_key(key), idx, widget);
        Ok(())
    }

    pub fn unbind(&mut self, key: &str) {
        let keys: Vec<String> = norm_key(key).split_whitespace().map(String::from).collect();
        self.binds.retain(|b| b.keys != keys);
    }

    /// 一覧 (plugin)
    pub fn describe(&self) -> String {
        let mut s = String::new();
        for p in &self.list {
            s.push_str(&format!("{} {} [{}]{}\n", p.name, p.prog, p.hooks.join(" "), if p.alive { "" } else { " (stopped)" }));
        }
        s
    }

    /// 一覧 (bindkey)
    pub fn describe_keys(&self) -> String {
        let mut s = String::new();
        for b in &self.binds {
            let name = self.list.get(b.plugin).map_or("?", |p| p.name.as_str());
            s.push_str(&format!("bindkey '{}' {}:{}\n", b.keys.join(" "), name, b.widget));
        }
        s
    }

    /// フックを受けとるプラグインがあるか
    pub fn wants(&self, hook: &str) -> bool {
        self.mine() && self.list.iter().any(|p| p.alive && p.hooks.iter().any(|h| h == hook))
    }

    /// フックを受けとるものに順に聞き、空でない答えを返した最初のもの
    pub fn ask(&mut self, hook: &str, mut ev: Value) -> Option<Value> {
        if !self.mine() {
            return None;
        }
        ev["ev"] = json!(hook);
        let wait = hook == "not_found";
        for p in self.list.iter_mut().filter(|p| p.alive && p.hooks.iter().any(|h| h == hook)) {
            if let Some(r) = p.ask(&ev, wait)
                && r.as_object().is_some_and(|o| !o.is_empty())
            {
                return Some(r);
            }
        }
        None
    }

    /// フックを受けとるものみなに知らせる (答えは見ない)
    pub fn tell(&mut self, hook: &str, mut ev: Value) {
        if !self.mine() {
            return;
        }
        ev["ev"] = json!(hook);
        for p in self.list.iter_mut().filter(|p| p.alive && p.hooks.iter().any(|h| h == hook)) {
            p.ask(&ev, false);
        }
    }

    /// 端末なしで呼べる機能の一覧: (プラグインの名前, { name, description, input })
    pub fn tools(&self) -> Vec<(String, Value)> {
        self.list.iter().filter(|p| p.alive).flat_map(|p| p.tools.iter().map(move |t| (p.name.clone(), t.clone()))).collect()
    }

    /// プラグインのプロセス (aish --mcp が時間切れで子を止めるとき、これは残す)
    pub fn pids(&self) -> Vec<i32> {
        self.list.iter().filter(|p| p.alive).map(|p| p.child.id() as i32).collect()
    }

    /// 端末なしの機能を呼ぶ (名前が同じなら先に読んだもの)。答えを待ちつづける
    pub fn tool(&mut self, name: &str, mut ev: Value) -> Option<Value> {
        if !self.mine() {
            return None;
        }
        ev["ev"] = json!("tool");
        ev["name"] = json!(name);
        let p = self.list.iter_mut().find(|p| p.alive && p.tools.iter().any(|t| t["name"] == name))?;
        p.ask(&ev, true)
    }

    /// キーに結んだ機能を呼ぶ (端末で人と話すかもしれないので待ちつづける)
    pub fn key(&mut self, b: usize, mut ev: Value) -> Option<Value> {
        if !self.mine() {
            return None;
        }
        let (plugin, widget) = (self.binds[b].plugin, self.binds[b].widget.clone());
        ev["ev"] = json!("key");
        ev["widget"] = json!(widget);
        ev["key"] = json!(self.binds[b].keys.join(" "));
        self.list.get_mut(plugin).and_then(|p| p.ask(&ev, true))
    }
}

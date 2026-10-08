// printf の組み込み (bash と同じ): printf [-v VAR] FORMAT [ARG...]
//   %s %b %q %c %d %i %u %o %x %X %e %E %f %F %g %G %a %A %% と、幅・精度・フラグ (- + 空白 # 0、* も)
//   %b は引数の \ をほどき (\c でそこまで)、%q はシェルで読める形にクォートする。%(FMT)T は時刻 (strftime。
//   引数は秒、-1 か空はいま、-2 はシェルが起きたとき)
//   FORMAT を使い切っても引数が残れば、FORMAT をくり返す。数でない引数は 0 にしてエラー (終わりのステータス 1)
use std::ffi::CString;

/// FORMAT と引数から出力を作る。(出力, ステータス)。started はシェルが起きた時刻 (秒)
pub fn format(fmt: &str, args: &[String], started: i64) -> (String, i32) {
    let f: Vec<char> = fmt.chars().collect();
    let mut out = String::new();
    let mut st = 0;
    let mut ai = 0;
    loop {
        let used_before = ai;
        let mut i = 0;
        while i < f.len() {
            match f[i] {
                '\\' => {
                    let (s, n, stop) = escape(&f[i + 1..], false);
                    out.push_str(&s);
                    i += 1 + n;
                    if stop {
                        return (out, st);
                    }
                }
                '%' if f.get(i + 1) == Some(&'%') => {
                    out.push('%');
                    i += 2;
                }
                '%' => {
                    let mut next = || {
                        let a = args.get(ai).cloned();
                        ai += 1;
                        a
                    };
                    match spec(&f, i + 1, &mut next, &mut out, &mut st, started) {
                        Some((n, stop)) => {
                            i = n;
                            if stop {
                                return (out, st);
                            }
                        }
                        None => {
                            eprintln!("printf: {}: invalid format character", f[i..].iter().take(2).collect::<String>());
                            return (out, 1);
                        }
                    }
                }
                c => {
                    out.push(c);
                    i += 1;
                }
            }
        }
        // 引数が残っていて、この回で 1 つでも使ったら、もう 1 回
        if ai >= args.len() || ai == used_before {
            break;
        }
    }
    (out, st)
}

/// % のあと (from から): 1 つの変換をして、(次の位置, \c で止まるか)。分からない字なら None
fn spec(f: &[char], from: usize, next: &mut dyn FnMut() -> Option<String>, out: &mut String, st: &mut i32, started: i64) -> Option<(usize, bool)> {
    let mut i = from;
    let mut flags = String::new();
    while i < f.len() && "-+ #0".contains(f[i]) {
        flags.push(f[i]);
        i += 1;
    }
    // %(FMT)T
    if f.get(i) == Some(&'(') {
        let end = i + f[i..].iter().position(|&c| c == ')')?;
        if f.get(end + 1) != Some(&'T') {
            return None;
        }
        let tf: String = f[i + 1..end].iter().collect();
        let a = next().unwrap_or_default();
        let t = match a.trim() {
            "" | "-1" => now(),
            "-2" => started,
            s => num(s, st),
        };
        out.push_str(&strftime(&tf, t));
        return Some((end + 2, false));
    }
    let mut width = String::new();
    if f.get(i) == Some(&'*') {
        let w = num(&next().unwrap_or_default(), st);
        if w < 0 {
            flags.push('-');
        }
        width = w.abs().to_string();
        i += 1;
    } else {
        while i < f.len() && f[i].is_ascii_digit() {
            width.push(f[i]);
            i += 1;
        }
    }
    let mut prec: Option<String> = None;
    if f.get(i) == Some(&'.') {
        i += 1;
        let mut p = String::new();
        if f.get(i) == Some(&'*') {
            p = num(&next().unwrap_or_default(), st).max(0).to_string();
            i += 1;
        } else {
            while i < f.len() && f[i].is_ascii_digit() {
                p.push(f[i]);
                i += 1;
            }
        }
        prec = Some(if p.is_empty() { "0".into() } else { p });
    }
    // 長さの修飾 (l h j z t L) は飛ばす
    while i < f.len() && "lhjztL".contains(f[i]) {
        i += 1;
    }
    let conv = *f.get(i)?;
    let left = flags.contains('-');
    let pad = |s: String| -> String {
        let w: usize = width.parse().unwrap_or(0);
        let n = s.chars().count();
        if n >= w {
            s
        } else if left {
            s + &" ".repeat(w - n)
        } else {
            " ".repeat(w - n) + &s
        }
    };
    let cut = |s: String| match &prec {
        Some(p) => s.chars().take(p.parse().unwrap_or(0)).collect(),
        None => s,
    };
    match conv {
        's' => out.push_str(&pad(cut(next().unwrap_or_default()))),
        'q' => out.push_str(&pad(quote(&next().unwrap_or_default()))),
        'b' => {
            let a: Vec<char> = next().unwrap_or_default().chars().collect();
            let mut s = String::new();
            let mut k = 0;
            let mut stop = false;
            while k < a.len() {
                if a[k] == '\\' {
                    let (e, n, c) = escape(&a[k + 1..], true);
                    s.push_str(&e);
                    k += 1 + n;
                    if c {
                        stop = true;
                        break;
                    }
                } else {
                    s.push(a[k]);
                    k += 1;
                }
            }
            out.push_str(&pad(cut(s)));
            if stop {
                return Some((i + 1, true));
            }
        }
        'c' => out.push_str(&pad(next().unwrap_or_default().chars().next().map(String::from).unwrap_or_default())),
        'd' | 'i' | 'o' | 'u' | 'x' | 'X' => {
            let n = num(&next().unwrap_or_default(), st);
            let p = prec.as_ref().map_or(String::new(), |p| format!(".{}", p));
            out.push_str(&c_format(&format!("%{}{}{}ll{}", flags, width, p, conv), Arg::Int(n)));
        }
        'e' | 'E' | 'f' | 'F' | 'g' | 'G' | 'a' | 'A' => {
            let n = float(&next().unwrap_or_default(), st);
            let p = prec.as_ref().map_or(String::new(), |p| format!(".{}", p));
            out.push_str(&c_format(&format!("%{}{}{}{}", flags, width, p, conv), Arg::Float(n)));
        }
        _ => return None,
    }
    Some((i + 1, false))
}

enum Arg {
    Int(i64),
    Float(f64),
}

/// C の snprintf で 1 つの変換 (数の形は C と同じにしたいので)
fn c_format(spec: &str, a: Arg) -> String {
    let Ok(cs) = CString::new(spec) else { return String::new() };
    let mut buf = vec![0u8; 512];
    let n = unsafe {
        match a {
            Arg::Int(v) => libc::snprintf(buf.as_mut_ptr() as *mut libc::c_char, buf.len(), cs.as_ptr(), v as libc::c_longlong),
            Arg::Float(v) => libc::snprintf(buf.as_mut_ptr() as *mut libc::c_char, buf.len(), cs.as_ptr(), v),
        }
    };
    if n < 0 {
        return String::new();
    }
    let n = n as usize;
    if n >= buf.len() {
        buf = vec![0u8; n + 1];
        unsafe {
            match a {
                Arg::Int(v) => libc::snprintf(buf.as_mut_ptr() as *mut libc::c_char, buf.len(), cs.as_ptr(), v as libc::c_longlong),
                Arg::Float(v) => libc::snprintf(buf.as_mut_ptr() as *mut libc::c_char, buf.len(), cs.as_ptr(), v),
            };
        }
    }
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

/// 数の引数: 'A や "A は字の番号、0x は 16 進、0 で始まれば 8 進。数でなければ 0 にしてエラー
fn num(s: &str, st: &mut i32) -> i64 {
    let t = s.trim_start();
    if let Some(c) = t.strip_prefix(['\'', '"']) {
        return c.chars().next().map_or(0, |c| c as i64);
    }
    if t.is_empty() {
        return 0;
    }
    let (neg, body) = match t.strip_prefix('-') {
        Some(b) => (true, b),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let r = if let Some(h) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
        i64::from_str_radix(h, 16)
    } else if body.len() > 1 && body.starts_with('0') {
        i64::from_str_radix(&body[1..], 8)
    } else {
        body.parse::<i64>()
    };
    match r {
        Ok(v) => if neg { -v } else { v },
        Err(_) => {
            eprintln!("printf: {}: invalid number", s);
            *st = 1;
            0
        }
    }
}

fn float(s: &str, st: &mut i32) -> f64 {
    let t = s.trim();
    if let Some(c) = t.strip_prefix(['\'', '"']) {
        return c.chars().next().map_or(0.0, |c| c as u32 as f64);
    }
    if t.is_empty() {
        return 0.0;
    }
    match t.parse::<f64>() {
        Ok(v) => v,
        Err(_) => match t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")).and_then(|h| i64::from_str_radix(h, 16).ok()) {
            Some(v) => v as f64,
            None => {
                eprintln!("printf: {}: invalid number", s);
                *st = 1;
                0.0
            }
        },
    }
}

/// \ のあと (s): (ほどいた字, 使った字の数, \c か)。in_b は %b の中 (8 進は \0NNN、\c が効く)
fn escape(s: &[char], in_b: bool) -> (String, usize, bool) {
    let Some(&c) = s.first() else { return ("\\".into(), 0, false) };
    let hex = |s: &[char], max: usize| -> (Option<char>, usize) {
        let n = s.iter().take(max).take_while(|c| c.is_ascii_hexdigit()).count();
        if n == 0 {
            return (None, 0);
        }
        let v = u32::from_str_radix(&s[..n].iter().collect::<String>(), 16).unwrap_or(0);
        (char::from_u32(v), n)
    };
    let one = |ch: char| (ch.to_string(), 1, false);
    match c {
        'n' => one('\n'),
        't' => one('\t'),
        'r' => one('\r'),
        'a' => one('\x07'),
        'b' => one('\x08'),
        'f' => one('\x0c'),
        'v' => one('\x0b'),
        'e' | 'E' => one('\x1b'),
        '\\' => one('\\'),
        '"' if !in_b => one('"'),
        '\'' if !in_b => one('\''),
        'c' if in_b => (String::new(), 1, true),
        'x' => match hex(&s[1..], 2) {
            (Some(ch), n) => (ch.to_string(), 1 + n, false),
            _ => ("\\x".into(), 1, false),
        },
        'u' | 'U' => match hex(&s[1..], if c == 'u' { 4 } else { 8 }) {
            (Some(ch), n) => (ch.to_string(), 1 + n, false),
            _ => (format!("\\{}", c), 1, false),
        },
        '0'..='7' => {
            // FORMAT では \NNN、%b では \0NNN (と \NNN)
            let (skip, max) = if in_b && c == '0' { (1, 3) } else { (0, 3) };
            let digits: String = s[skip..].iter().take(max).take_while(|c| ('0'..='7').contains(c)).collect();
            let v = u32::from_str_radix(&digits, 8).unwrap_or(0);
            ((v as u8 as char).to_string(), skip + digits.len(), false)
        }
        _ => (format!("\\{}", c), 1, false),
    }
}

/// %q: bash と同じように、シェルでそのまま読める形にする。空なら ''、制御文字があれば $'...'、
/// ほかは特別な字の前に \ をつける
pub fn quote(s: &str) -> String {
    if s.is_empty() {
        return "''".into();
    }
    if s.chars().any(|c| c.is_control()) {
        let mut o = String::from("$'");
        for c in s.chars() {
            match c {
                '\n' => o.push_str("\\n"),
                '\t' => o.push_str("\\t"),
                '\r' => o.push_str("\\r"),
                '\x1b' => o.push_str("\\E"),
                '\x07' => o.push_str("\\a"),
                '\x08' => o.push_str("\\b"),
                '\x0c' => o.push_str("\\f"),
                '\x0b' => o.push_str("\\v"),
                '\\' => o.push_str("\\\\"),
                '\'' => o.push_str("\\'"),
                c if c.is_control() => o.push_str(&format!("\\{:03o}", c as u32)),
                c => o.push(c),
            }
        }
        o.push('\'');
        return o;
    }
    let mut o = String::new();
    for (k, c) in s.chars().enumerate() {
        let special = matches!(c, ' ' | '!' | '"' | '#' | '$' | '&' | '\'' | '(' | ')' | '*' | ',' | ';' | '<' | '>' | '?' | '[' | '\\' | ']' | '^' | '`' | '{' | '|' | '}')
            || (c == '~' && k == 0);
        if special {
            o.push('\\');
        }
        o.push(c);
    }
    o
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// strftime (ローカル時刻)
fn strftime(fmt: &str, t: i64) -> String {
    let fmt = if fmt.is_empty() { "%X" } else { fmt };
    let Ok(cf) = CString::new(fmt) else { return String::new() };
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let tt = t as libc::time_t;
    unsafe { libc::localtime_r(&tt, &mut tm) };
    let mut buf = vec![0u8; 256 + fmt.len() * 8];
    let n = unsafe { libc::strftime(buf.as_mut_ptr() as *mut libc::c_char, buf.len(), cf.as_ptr(), &tm) };
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

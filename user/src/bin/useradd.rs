// useradd / usermod / userdel / groupadd / groupdel: ユーザーとグループを足す・変える・消す
//   Linux の shadow-utils (Arch の shadow パッケージ) と同じ名前と、よく使うオプション。
//   呼ばれた名前で動きを変える (usermod などは useradd へのリンク)。root だけ
//
//   useradd [-m] [-d HOME] [-s SHELL] [-u UID] [-g GROUP] [-G G1,G2] [-c COMMENT] [-r] [-N] NAME
//     ユーザーと同じ名前のグループも作る (Arch と同じ)。-m でホームを作り /etc/skel を写す。
//     パスワードはまだない (passwd NAME で付ける)
//   usermod [-a] [-G G1,G2] [-g GROUP] [-s SHELL] [-d HOME [-m]] [-c COMMENT] [-L] [-U] NAME
//   userdel [-r] NAME            (-r でホームも消す)
//   groupadd [-g GID] [-r] NAME
//   groupdel NAME
//
// 既定は /etc/default/useradd (SHELL=, HOME=, CREATE_HOME=yes なら -m がなくてもホームを作る)
#[path = "../lib/users.rs"]
mod users;

use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs;
use std::io::Write;
use std::process::exit;

const PASSWD: &str = "/etc/passwd";
const GROUP: &str = "/etc/group";
const SHADOW: &str = "/etc/shadow";
const UID_MIN: u32 = 1000;
const UID_MAX: u32 = 60000;
const SYS_UID_MAX: u32 = 999;

fn die(prog: &str, msg: impl std::fmt::Display) -> ! {
    eprintln!("{}: {}", prog, msg);
    exit(1)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let prog = args[0].rsplit('/').next().unwrap_or("useradd").to_string();
    let rest = &args[1..];
    if rest.iter().any(|a| a == "-h" || a == "--help") {
        usage(&prog, 0);
    }
    if unsafe { libc::geteuid() } != 0 {
        die(&prog, "Permission denied. (sudo を付けて)");
    }
    match prog.as_str() {
        "usermod" => usermod(&prog, rest),
        "userdel" => userdel(&prog, rest),
        "groupadd" => groupadd(&prog, rest),
        "groupdel" => groupdel(&prog, rest),
        _ => useradd(&prog, rest),
    }
}

fn usage(prog: &str, code: i32) -> ! {
    let u = match prog {
        "usermod" => "usermod [-a] [-G G1,G2] [-g GROUP] [-s SHELL] [-d HOME [-m]] [-c COMMENT] [-L|-U] NAME",
        "userdel" => "userdel [-r] NAME",
        "groupadd" => "groupadd [-g GID] [-r] NAME",
        "groupdel" => "groupdel NAME",
        _ => "useradd [-m] [-d HOME] [-s SHELL] [-u UID] [-g GROUP] [-G G1,G2] [-c COMMENT] [-r] [-N] NAME",
    };
    eprintln!("usage: {}", u);
    exit(code)
}

/// オプションを読む: (値のいらない印, 値のある印 → 値, 残り)
fn parse(prog: &str, args: &[String], with_value: &str) -> (Vec<char>, BTreeMap<char, String>, Vec<String>) {
    let (mut flags, mut vals, mut rest) = (vec![], BTreeMap::new(), vec![]);
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a.len() > 1 && a.starts_with('-') && !a.starts_with("--") {
            let cs: Vec<char> = a[1..].chars().collect();
            for (k, c) in cs.iter().enumerate() {
                if with_value.contains(*c) {
                    // -G wheel か -Gwheel
                    let v = if k + 1 < cs.len() {
                        cs[k + 1..].iter().collect()
                    } else {
                        i += 1;
                        args.get(i).cloned().unwrap_or_else(|| usage(prog, 2))
                    };
                    vals.insert(*c, v);
                    break;
                }
                flags.push(*c);
            }
        } else {
            rest.push(a.clone());
        }
        i += 1;
    }
    (flags, vals, rest)
}

fn one_name(prog: &str, rest: &[String]) -> String {
    match rest {
        [n] => n.clone(),
        _ => usage(prog, 2),
    }
}

/// shadow-utils と同じ形の名前か (小文字か _ で始まり、小文字・数字・_ -、最後に $ も可、32 文字まで)
fn valid_name(n: &str) -> bool {
    let b = n.as_bytes();
    !b.is_empty()
        && b.len() <= 32
        && (b[0].is_ascii_lowercase() || b[0] == b'_')
        && b.iter().enumerate().all(|(i, &c)| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'-' || (c == b'$' && i == b.len() - 1))
}

// ---- ファイル ----

fn lines(path: &str) -> Vec<Vec<String>> {
    fs::read_to_string(path).unwrap_or_default().lines().filter(|l| !l.is_empty()).map(|l| l.split(':').map(String::from).collect()).collect()
}

/// 一時ファイルに書いて rename (途中で止まっても元のファイルはこわれない)
fn save(prog: &str, path: &str, rows: &[Vec<String>], mode: u32) {
    let mut text = String::new();
    for r in rows {
        text.push_str(&r.join(":"));
        text.push('\n');
    }
    let tmp = format!("{}.new", path);
    let r = (|| {
        let mut f = fs::OpenOptions::new().write(true).create(true).truncate(true).open(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        let c = CString::new(tmp.clone()).unwrap();
        unsafe {
            libc::chmod(c.as_ptr(), mode);
            libc::chown(c.as_ptr(), 0, 0);
        }
        fs::rename(&tmp, path)
    })();
    if let Err(e) = r {
        die(prog, format!("{}: {}", path, e));
    }
}

fn days() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() / 86400)
}

/// /etc/default/useradd の KEY=VALUE
fn defaults() -> BTreeMap<String, String> {
    fs::read_to_string("/etc/default/useradd")
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .filter_map(|l| l.split_once('=').map(|(k, v)| (k.trim().to_string(), v.trim().trim_matches('"').to_string())))
        .collect()
}

fn gid_of(group: &str) -> Option<u32> {
    if let Ok(n) = group.parse::<u32>() {
        return lines(GROUP).iter().any(|g| g.get(2).is_some_and(|x| *x == n.to_string())).then_some(n);
    }
    lines(GROUP).iter().find(|g| g[0] == group).and_then(|g| g.get(2)?.parse().ok())
}

/// まだ使われていない番号 (ふつうは UID_MIN から上へ、システム用は 999 から下へ)
fn free_id(used: &[u32], system: bool, also_free_in: &[u32]) -> Option<u32> {
    let ok = |n: &u32| !used.contains(n) && !also_free_in.contains(n);
    if system {
        (1..=SYS_UID_MAX).rev().find(ok)
    } else {
        (UID_MIN..=UID_MAX).find(ok)
    }
}

fn used_uids() -> Vec<u32> {
    lines(PASSWD).iter().filter_map(|r| r.get(2)?.parse().ok()).collect()
}

fn used_gids() -> Vec<u32> {
    lines(GROUP).iter().filter_map(|r| r.get(2)?.parse().ok()).collect()
}

/// -G の並びのグループに name を足す (exclusive なら、ほかのグループからは抜く)
fn set_supplementary(prog: &str, name: &str, list: &str, exclusive: bool) {
    let want: Vec<&str> = list.split(',').filter(|g| !g.is_empty()).collect();
    let mut groups = lines(GROUP);
    for g in &want {
        if !groups.iter().any(|r| r[0] == *g) {
            die(prog, format!("group '{}' does not exist", g));
        }
    }
    for r in groups.iter_mut() {
        r.resize(4.max(r.len()), String::new());
        let mut members: Vec<String> = r[3].split(',').filter(|m| !m.is_empty()).map(String::from).collect();
        let inside = members.iter().any(|m| m == name);
        if want.contains(&r[0].as_str()) {
            if !inside {
                members.push(name.to_string());
            }
        } else if exclusive && inside {
            members.retain(|m| m != name);
        }
        r[3] = members.join(",");
    }
    save(prog, GROUP, &groups, 0o644);
}

// ---- useradd ----

fn useradd(prog: &str, args: &[String]) {
    let (flags, vals, rest) = parse(prog, args, "dsugGcek");
    let name = one_name(prog, &rest);
    if !valid_name(&name) {
        die(prog, format!("invalid user name '{}'", name));
    }
    if users::by_name(&name).is_some() {
        die(prog, format!("user '{}' already exists", name));
    }
    let def = defaults();
    let system = flags.contains(&'r');
    let home = vals.get(&'d').cloned().unwrap_or_else(|| format!("{}/{}", def.get("HOME").map_or("/home", |h| h.as_str()), name));
    let shell = vals.get(&'s').cloned().or_else(|| def.get("SHELL").cloned()).unwrap_or_else(|| "/bin/aish".into());
    let comment = vals.get(&'c').cloned().unwrap_or_default();
    if comment.contains(':') || home.contains(':') || shell.contains(':') {
        die(prog, "':' is not allowed");
    }
    let uids = used_uids();
    let uid = match vals.get(&'u') {
        Some(u) => {
            let u: u32 = u.parse().unwrap_or_else(|_| die(prog, format!("invalid user ID '{}'", u)));
            if uids.contains(&u) {
                die(prog, format!("UID {} is not unique", u));
            }
            u
        }
        None => {
            // 同じ名前のグループを作るなら、グループでも空いている番号にする (uid と gid をそろえる)
            let avoid = if vals.contains_key(&'g') || flags.contains(&'N') { vec![] } else { used_gids() };
            free_id(&uids, system, &avoid).or_else(|| free_id(&uids, system, &[])).unwrap_or_else(|| die(prog, "no free UID"))
        }
    };
    // 主グループ: -g か、-N なら users、なければ同じ名前のグループを作る
    let (gid, new_group) = match vals.get(&'g') {
        Some(g) => (gid_of(g).unwrap_or_else(|| die(prog, format!("group '{}' does not exist", g))), false),
        None if flags.contains(&'N') => (gid_of("users").unwrap_or(100), false),
        None => {
            if gid_of(&name).is_some() {
                die(prog, format!("group {} exists - if you want to add this user to that group, use -g.", name));
            }
            let gids = used_gids();
            let gid = if gids.contains(&uid) { free_id(&gids, system, &[]).unwrap_or_else(|| die(prog, "no free GID")) } else { uid };
            (gid, true)
        }
    };
    if let Some(list) = vals.get(&'G') {
        for g in list.split(',').filter(|g| !g.is_empty()) {
            if gid_of(g).is_none() {
                die(prog, format!("group '{}' does not exist", g));
            }
        }
    }

    if new_group {
        let mut groups = lines(GROUP);
        groups.push(vec![name.clone(), "x".into(), gid.to_string(), String::new()]);
        save(prog, GROUP, &groups, 0o644);
    }
    let mut pw = lines(PASSWD);
    pw.push(vec![name.clone(), "x".into(), uid.to_string(), gid.to_string(), comment, home.clone(), shell]);
    save(prog, PASSWD, &pw, 0o644);
    // パスワードはまだない ("!": passwd で付けるまでパスワードではログインできない)
    let mut sh = lines(SHADOW);
    sh.retain(|r| r[0] != name);
    sh.push(vec![name.clone(), "!".into(), days().to_string(), "0".into(), "99999".into(), "7".into(), String::new(), String::new(), String::new()]);
    save(prog, SHADOW, &sh, 0o600);
    if let Some(list) = vals.get(&'G') {
        set_supplementary(prog, &name, list, false);
    }
    let create = flags.contains(&'m') || (!flags.contains(&'M') && def.get("CREATE_HOME").is_some_and(|v| v == "yes"));
    if create {
        if fs::metadata(&home).is_ok() {
            eprintln!("{}: warning: the home directory {} already exists. Not copying any file from skel directory into it.", prog, home);
        } else if let Some(u) = users::by_name(&name) {
            users::make_home(&u);
        }
    }
}

// ---- usermod ----

fn usermod(prog: &str, args: &[String]) {
    let (flags, vals, rest) = parse(prog, args, "dsgGcl");
    let name = one_name(prog, &rest);
    let Some(old) = users::by_name(&name) else { die(prog, format!("user '{}' does not exist", name)) };
    if flags.is_empty() && vals.is_empty() {
        eprintln!("{}: no options", prog);
        usage(prog, 2);
    }
    let mut pw = lines(PASSWD);
    let row = pw.iter_mut().find(|r| r[0] == name).unwrap();
    row.resize(7.max(row.len()), String::new());
    if let Some(s) = vals.get(&'s') {
        row[6] = s.clone();
    }
    if let Some(c) = vals.get(&'c') {
        row[4] = c.clone();
    }
    if let Some(g) = vals.get(&'g') {
        row[3] = gid_of(g).unwrap_or_else(|| die(prog, format!("group '{}' does not exist", g))).to_string();
    }
    let mut moved = None;
    if let Some(d) = vals.get(&'d') {
        if flags.contains(&'m') && fs::metadata(&old.home).is_ok() && old.home != *d {
            moved = Some((old.home.clone(), d.clone()));
        }
        row[5] = d.clone();
    }
    if [vals.get(&'s'), vals.get(&'c'), vals.get(&'d')].iter().flatten().any(|v| v.contains(':')) {
        die(prog, "':' is not allowed");
    }
    save(prog, PASSWD, &pw, 0o644);
    if let Some((from, to)) = moved {
        if let Err(e) = fs::rename(&from, &to) {
            die(prog, format!("cannot move {} to {}: {}", from, to, e));
        }
    }
    if let Some(list) = vals.get(&'G') {
        // -a なら足すだけ、なければ並べたグループだけにする (shadow-utils と同じ)
        set_supplementary(prog, &name, list, !flags.contains(&'a'));
    }
    // -L / -U: パスワードの前の "!" で鍵をかける / 外す
    if flags.contains(&'L') || flags.contains(&'U') {
        let mut sh = lines(SHADOW);
        if let Some(r) = sh.iter_mut().find(|r| r[0] == name) {
            r.resize(2.max(r.len()), String::new());
            if flags.contains(&'L') && !r[1].starts_with('!') {
                r[1].insert(0, '!');
            } else if flags.contains(&'U') {
                if r[1] == "!" {
                    die(prog, format!("unlocking the user's password would result in a passwordless account (passwd {})", name));
                }
                r[1] = r[1].trim_start_matches('!').to_string();
            }
        }
        save(prog, SHADOW, &sh, 0o600);
    }
}

// ---- userdel ----

fn userdel(prog: &str, args: &[String]) {
    let (flags, _, rest) = parse(prog, args, "");
    let name = one_name(prog, &rest);
    let Some(u) = users::by_name(&name) else { die(prog, format!("user '{}' does not exist", name)) };
    if u.uid == 0 {
        die(prog, "cannot remove root");
    }
    let mut pw = lines(PASSWD);
    pw.retain(|r| r[0] != name);
    save(prog, PASSWD, &pw, 0o644);
    let mut sh = lines(SHADOW);
    sh.retain(|r| r[0] != name);
    save(prog, SHADOW, &sh, 0o600);
    // グループのメンバーから抜き、同じ名前の自分のグループは (ほかに使う人がいなければ) 消す
    let still_used = pw.iter().any(|r| r.get(3).is_some_and(|g| *g == u.gid.to_string()));
    let mut groups = lines(GROUP);
    groups.retain(|r| !(r[0] == name && r.get(2).is_some_and(|g| *g == u.gid.to_string()) && r.get(3).is_none_or(|m| m.is_empty()) && !still_used));
    for r in groups.iter_mut() {
        if let Some(m) = r.get_mut(3) {
            *m = m.split(',').filter(|x| !x.is_empty() && *x != name).collect::<Vec<_>>().join(",");
        }
    }
    save(prog, GROUP, &groups, 0o644);
    if flags.contains(&'r') && u.home.starts_with('/') && u.home != "/" {
        if let Err(e) = fs::remove_dir_all(&u.home) {
            eprintln!("{}: {}: {}", prog, u.home, e);
        }
    }
}

// ---- groupadd / groupdel ----

fn groupadd(prog: &str, args: &[String]) {
    let (flags, vals, rest) = parse(prog, args, "g");
    let name = one_name(prog, &rest);
    if !valid_name(&name) {
        die(prog, format!("'{}' is not a valid group name", name));
    }
    if gid_of(&name).is_some() {
        die(prog, format!("group '{}' already exists", name));
    }
    let gids = used_gids();
    let gid = match vals.get(&'g') {
        Some(g) => {
            let g: u32 = g.parse().unwrap_or_else(|_| die(prog, format!("invalid group ID '{}'", g)));
            if gids.contains(&g) {
                die(prog, format!("GID '{}' already exists", g));
            }
            g
        }
        None => free_id(&gids, flags.contains(&'r'), &[]).unwrap_or_else(|| die(prog, "no free GID")),
    };
    let mut groups = lines(GROUP);
    groups.push(vec![name, "x".into(), gid.to_string(), String::new()]);
    save(prog, GROUP, &groups, 0o644);
}

fn groupdel(prog: &str, args: &[String]) {
    let (_, _, rest) = parse(prog, args, "");
    let name = one_name(prog, &rest);
    let Some(gid) = gid_of(&name) else { die(prog, format!("group '{}' does not exist", name)) };
    if let Some(u) = lines(PASSWD).iter().find(|r| r.get(3).is_some_and(|g| *g == gid.to_string())) {
        die(prog, format!("cannot remove the primary group of user '{}'", u[0]));
    }
    let mut groups = lines(GROUP);
    groups.retain(|r| r[0] != name);
    save(prog, GROUP, &groups, 0o644);
}

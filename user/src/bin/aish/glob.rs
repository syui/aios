// ワイルドカード: クォートの外の * ? [ は展開のときに印 (私用領域の文字) にしておき、ここで広げる

/// クォートの外にあった * ? [ の印 (Unicode の私用領域の文字)
pub const GLOB_STAR: char = '\u{f0000}';
pub const GLOB_ONE: char = '\u{f0001}';
pub const GLOB_SET: char = '\u{f0002}';

fn is_mark(c: char) -> bool {
    matches!(c, GLOB_STAR | GLOB_ONE | GLOB_SET)
}

/// 印をもとの文字に戻す
pub fn unmark(w: &str) -> String {
    w.chars()
        .map(|c| match c {
            GLOB_STAR => '*',
            GLOB_ONE => '?',
            GLOB_SET => '[',
            c => c,
        })
        .collect()
}

/// 語をファイル名に広げる。印がないか、何にも当たらなければ、もとの語 1 つ
pub fn glob(w: &str) -> Vec<String> {
    if !w.chars().any(is_mark) {
        return vec![w.to_string()];
    }
    let (mut found, parts): (Vec<String>, Vec<&str>) = if let Some(rest) = w.strip_prefix('/') {
        (vec!["/".into()], rest.split('/').collect())
    } else {
        (vec![String::new()], w.split('/').collect())
    };
    for (i, part) in parts.iter().enumerate() {
        let last = i + 1 == parts.len();
        let mut next = vec![];
        for base in &found {
            if part.is_empty() {
                // "a//b" や末尾の "/"
                if !last || !base.is_empty() {
                    next.push(format!("{}/", base.trim_end_matches('/')));
                }
                continue;
            }
            let join = |name: &str| if base.is_empty() || base.ends_with('/') { format!("{}{}", base, name) } else { format!("{}/{}", base, name) };
            // ** (それだけの要素): 0 個以上のディレクトリ (zsh と bash の globstar)。. で始まるものとリンクの先は見ない
            if !last && *part == "\u{f0000}\u{f0000}" {
                let mut stack = vec![base.clone()];
                while let Some(d) = stack.pop() {
                    next.push(d.clone());
                    let Ok(rd) = std::fs::read_dir(if d.is_empty() { "." } else { d.as_str() }) else { continue };
                    let mut subs: Vec<String> = rd
                        .flatten()
                        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .filter(|n| !n.starts_with('.'))
                        .map(|n| if d.is_empty() || d.ends_with('/') { format!("{}{}", d, n) } else { format!("{}/{}", d, n) })
                        .collect();
                    subs.sort();
                    subs.reverse();
                    stack.extend(subs);
                }
                continue;
            }
            if !part.chars().any(is_mark) {
                let p = join(part);
                if last || std::fs::metadata(&p).is_ok_and(|m| m.is_dir()) {
                    next.push(p);
                }
                continue;
            }
            let dir = if base.is_empty() { "." } else { base.as_str() };
            let Ok(rd) = std::fs::read_dir(dir) else { continue };
            let pat: Vec<char> = part.chars().collect();
            let mut names: Vec<String> = rd
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                // . で始まる名前は、パターンも . で始まるときだけ
                .filter(|n| !n.starts_with('.') || pat[0] == '.')
                .filter(|n| glob_match(&pat, &n.chars().collect::<Vec<_>>()))
                .collect();
            names.sort();
            for n in names {
                let p = join(&n);
                if last || std::fs::metadata(&p).is_ok_and(|m| m.is_dir()) {
                    next.push(p);
                }
            }
        }
        found = next;
    }
    // 存在しない普通の部分だけの候補は、最後の要素にしか印がないときに出うるので、確かめる
    found.retain(|p| std::fs::symlink_metadata(p).is_ok());
    // ** を使ったら、zsh と同じく全部を名前の順に
    if w.contains("\u{f0000}\u{f0000}") {
        found.sort();
        found.dedup();
    }
    if found.is_empty() { vec![unmark(w)] } else { found }
}

/// glob と同じ。ただし何にも当たらなければ 0 個 (setopt nullglob と *(N))
pub fn glob_or_none(w: &str) -> Vec<String> {
    let v = glob(w);
    if w.chars().any(is_mark) && v.len() == 1 && v[0] == unmark(w) && std::fs::symlink_metadata(&v[0]).is_err() {
        return vec![];
    }
    v
}

/// extglob の始まり (?( *( +( @( !( ) か
fn ext_at(p: &[char], i: usize) -> bool {
    matches!(p[i], GLOB_STAR | GLOB_ONE | '+' | '@' | '!') && p.get(i + 1) == Some(&'(')
}

/// extglob の ( の対になる ) と、| で分けた中身
fn ext_group(p: &[char], open: usize) -> Option<(usize, Vec<&[char]>)> {
    let (mut depth, mut start, mut alts) = (0, open + 1, vec![]);
    for (i, &c) in p.iter().enumerate().skip(open) {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    alts.push(&p[start..i]);
                    return Some((i, alts));
                }
            }
            '|' if depth == 1 => {
                alts.push(&p[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    None
}

/// extglob のあるパターン: 前から 1 つずつ、分かれ道は全部ためす
fn ext_match(p: &[char], s: &[char]) -> bool {
    let Some(&c) = p.first() else { return s.is_empty() };
    if ext_at(p, 0)
        && let Some((close, alts)) = ext_group(p, 1)
    {
        let rest = &p[close + 1..];
        let one = |a: &[char], k: usize| ext_match(a, &s[..k]);
        return match c {
            '@' => (0..=s.len()).any(|k| alts.iter().any(|a| one(a, k)) && ext_match(rest, &s[k..])),
            GLOB_ONE => ext_match(rest, s) || (0..=s.len()).any(|k| alts.iter().any(|a| one(a, k)) && ext_match(rest, &s[k..])),
            // *( ) は 0 回から、+( ) は 1 回から。くり返しは同じグループを残りに当てる
            GLOB_STAR | '+' => {
                (c == GLOB_STAR && ext_match(rest, s))
                    || (1..=s.len()).any(|k| alts.iter().any(|a| one(a, k)) && (ext_match(rest, &s[k..]) || ext_match(p, &s[k..])))
            }
            // !( ): どれにも当たらない部分
            _ => (0..=s.len()).any(|k| !alts.iter().any(|a| one(a, k)) && ext_match(rest, &s[k..])),
        };
    }
    match c {
        GLOB_STAR => ext_match(&p[1..], s) || (!s.is_empty() && ext_match(p, &s[1..])),
        _ if s.is_empty() => false,
        GLOB_ONE => ext_match(&p[1..], &s[1..]),
        GLOB_SET => match match_set(&p[1..], s[0]) {
            Some((ok, len)) => ok && ext_match(&p[1 + len..], &s[1..]),
            None => s[0] == '[' && ext_match(&p[1..], &s[1..]),
        },
        c => c == s[0] && ext_match(&p[1..], &s[1..]),
    }
}

/// パターン (印つき) が名前全体に当たるか
pub fn glob_match(p: &[char], s: &[char]) -> bool {
    if (0..p.len()).any(|i| ext_at(p, i)) {
        return ext_match(p, s);
    }
    let (mut pi, mut si) = (0, 0);
    // 最後に見た * の場所 (そこからやり直す)
    let mut star: Option<(usize, usize)> = None;
    while si < s.len() {
        if pi < p.len() {
            match p[pi] {
                GLOB_STAR => {
                    star = Some((pi, si));
                    pi += 1;
                    continue;
                }
                GLOB_ONE => {
                    pi += 1;
                    si += 1;
                    continue;
                }
                GLOB_SET => {
                    if let Some((ok, len)) = match_set(&p[pi + 1..], s[si]) {
                        if ok {
                            pi += 1 + len;
                            si += 1;
                            continue;
                        }
                    } else if s[si] == '[' {
                        // 閉じていない [ はただの文字
                        pi += 1;
                        si += 1;
                        continue;
                    }
                }
                c if c == s[si] => {
                    pi += 1;
                    si += 1;
                    continue;
                }
                _ => {}
            }
        }
        match star {
            Some((sp, ss)) => {
                pi = sp + 1;
                si = ss + 1;
                star = Some((sp, ss + 1));
            }
            None => return false,
        }
    }
    p[pi..].iter().all(|&c| c == GLOB_STAR)
}

/// [ の後ろ (set) を c に当てる。(当たったか, ] までの長さ)。] がなければ None
fn match_set(set: &[char], c: char) -> Option<(bool, usize)> {
    let lit = |x: char| unmark(&x.to_string()).chars().next().unwrap();
    let mut i = 0;
    let neg = matches!(set.first(), Some('!' | '^'));
    if neg {
        i += 1;
    }
    let mut hit = false;
    let mut first = true;
    while i < set.len() {
        let x = lit(set[i]);
        if x == ']' && !first {
            return Some((hit != neg, i + 1));
        }
        first = false;
        // [:alpha:] などの文字の種類 (POSIX)
        if x == '[' && set.get(i + 1) == Some(&':') {
            let rest: String = set[i + 2..].iter().map(|&c| lit(c)).collect();
            if let Some(end) = rest.find(":]") {
                let name = &rest[..end];
                hit |= match name {
                    "alpha" => c.is_alphabetic(),
                    "digit" => c.is_ascii_digit(),
                    "alnum" => c.is_alphanumeric(),
                    "upper" => c.is_uppercase(),
                    "lower" => c.is_lowercase(),
                    "space" => c.is_whitespace(),
                    "blank" => c == ' ' || c == '\t',
                    "punct" => c.is_ascii_punctuation(),
                    "xdigit" => c.is_ascii_hexdigit(),
                    "cntrl" => c.is_control(),
                    "print" => !c.is_control(),
                    "graph" => !c.is_control() && !c.is_whitespace(),
                    "word" => c.is_alphanumeric() || c == '_',
                    _ => false,
                };
                i += 2 + name.chars().count() + 2;
                continue;
            }
        }
        if i + 2 < set.len() && set[i + 1] == '-' && lit(set[i + 2]) != ']' {
            if (x..=lit(set[i + 2])).contains(&c) {
                hit = true;
            }
            i += 3;
        } else {
            if x == c {
                hit = true;
            }
            i += 1;
        }
    }
    None
}

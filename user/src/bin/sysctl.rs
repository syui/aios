// sysctl: カーネルの値 (/proc/sys) を読み書きする (procps の sysctl の一部)
//   sysctl NAME...           読む (NAME = 値)
//   sysctl [-w] NAME=VAL...  書く (root)
//   sysctl -a                ぜんぶ
//   sysctl -p [FILE]         ファイルの中身を入れる (なければ /etc/sysctl.conf)
//   sysctl --system          起動のときと同じ: /etc/sysctl.conf と /etc/sysctl.d/*.conf
//   -n で値だけ、-q で書いたものを出さない
#[path = "../lib/sysctl.rs"]
mod sysctl;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |f: &str| args.iter().any(|a| a == f);
    let (values_only, quiet) = (flag("-n"), flag("-q"));
    let show = |k: &str, v: &str| {
        if values_only {
            println!("{}", v);
        } else {
            println!("{} = {}", k, v);
        }
    };
    let mut rc = 0;
    let mut fail = |e: String| {
        eprintln!("sysctl: {}", e);
        rc = 1;
    };
    let rest: Vec<&String> = args.iter().filter(|a| !a.starts_with('-')).collect();
    if flag("-a") || flag("-A") || (args.is_empty()) {
        for (k, v) in sysctl::all() {
            show(&k, &v);
        }
    } else if flag("--system") || flag("-p") {
        let files: Vec<String> = if flag("--system") {
            sysctl::boot_files()
        } else if rest.is_empty() {
            vec![sysctl::CONF.to_string()]
        } else {
            rest.iter().map(|s| s.to_string()).collect()
        };
        for f in files {
            for e in sysctl::apply_file(&f) {
                fail(e);
            }
            if !quiet {
                for (k, _, _) in sysctl::parse(&std::fs::read_to_string(&f).unwrap_or_default()) {
                    if let Ok(v) = sysctl::get(&k) {
                        show(&k, &v);
                    }
                }
            }
        }
    } else {
        for a in rest {
            match a.split_once('=') {
                Some((k, v)) => match sysctl::set(k.trim(), v.trim()) {
                    Ok(()) if !quiet => show(k.trim(), v.trim()),
                    Ok(()) => {}
                    Err(e) => fail(e),
                },
                None => match sysctl::get(a) {
                    Ok(v) => show(a, &v),
                    Err(e) => fail(e),
                },
            }
        }
    }
    std::process::exit(rc);
}

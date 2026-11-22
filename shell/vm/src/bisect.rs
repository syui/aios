// bisect: test が通る good と通らない bad のあいだを git bisect で探す (コミットごとにカーネルを作り、VM で test)
use super::{note, secs, sub, vmpy};
use crate::kernel::{boot, git, kernel_for, worktree};
use aish_plugin::{Value, json, s};
use std::path::Path;
use std::time::{Duration, Instant};

pub(crate) fn job_bisect(root: &Path, a: &Value) -> Result<Value, String> {
    let good = s(a, "good");
    let bad = a["bad"].as_str().filter(|x| !x.is_empty()).unwrap_or("HEAD");
    let test = s(a, "test");
    let t = Duration::from_secs_f64(secs(a, "timeout", 300.0, 3600.0));
    let (good, bad) = (git(root, &["rev-parse", "--verify", &format!("{}^{{commit}}", good)])?, git(root, &["rev-parse", "--verify", &format!("{}^{{commit}}", bad)])?);
    let wt = worktree(root)?;
    let _ = git(&wt, &["bisect", "reset"]);
    git(&wt, &["checkout", "--detach", "-f", &bad])?;
    let mut out = git(&wt, &["bisect", "start", &bad, &good])?;
    let mut steps = Vec::new();
    let result = loop {
        if let Some(first) = out.lines().find(|l| l.contains("is the first bad commit")) {
            let sha = first.split_whitespace().next().unwrap_or("").to_string();
            let subject = git(root, &["log", "-1", "--format=%s", &sha]).unwrap_or_default();
            break Ok(json!({ "first_bad": sha, "subject": subject }));
        }
        if steps.len() >= 40 {
            break Err("too many steps".to_string());
        }
        let sha = git(&wt, &["rev-parse", "HEAD"])?;
        let t0 = Instant::now();
        // カーネルを作る (作れなければ skip)、起こす (起きなければ bad)、test (0 good、125 skip、ほか bad)
        let verdict = match kernel_for(root, &sha) {
            Err(e) => ("skip", e),
            Ok(k) => match boot(root, Some(&k), true, "", &[]) {
                Err(e) => ("bad", e),
                Ok(_) => {
                    let (st, o, _, _, to) = vmpy(root, &["run", &sub(test), "-t", &format!("{}", t.as_secs())], &[], t + Duration::from_secs(10));
                    let last = o.lines().rev().take(3).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join(" / ");
                    match (st, to) {
                        (_, true) => ("bad", format!("timeout: {}", last)),
                        (0, _) => ("good", last),
                        (125, _) => ("skip", last),
                        _ => ("bad", format!("exit {}: {}", st, last)),
                    }
                }
            },
        };
        let subject = git(root, &["log", "-1", "--format=%s", &sha]).unwrap_or_default();
        let step = json!({ "sha": &sha[..12.min(sha.len())], "subject": subject, "result": verdict.0, "why": verdict.1, "ms": t0.elapsed().as_millis() as u64 });
        note(step.clone());
        steps.push(step);
        out = match git(&wt, &["bisect", verdict.0]) {
            Ok(o) => o,
            Err(e) => break Err(e),
        };
    };
    let _ = git(&wt, &["bisect", "reset"]);
    vmpy(root, &["stop"], &[], Duration::from_secs(90));
    let mut r = result?;
    r["steps"] = json!(steps);
    r["note"] = json!("only the kernel changes between steps; the disk (userland) is the current aios.img");
    Ok(r)
}

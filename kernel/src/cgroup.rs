// cgroup v2 の核 (Linux の kernel/cgroup/cgroup.c) と pids controller (kernel/cgroup/pids.c): 手本の test/linux.config の
// CGROUPS と CGROUP_PIDS
//
//   木         ディレクトリの木 1 つ (根の番号は 1)。プロセスのいるところは Proc の cgroup (番号: ディレクトリの ino と
//              同じ。Linux の cgroup_id)。fork とスレッドは親のところ、clone3 の CLONE_INTO_CGROUP ならその fd のところ
//   cgroup2    mount -t cgroup2 (分けあう 1 つのファイルシステム: 同じところにもう一度は EBUSY)。根は cgroup の namespace
//              の根。はじめてかぶせるまで /proc/PID/cgroup は空 (Linux の cgrp_dfl_visible)
//   ファイル   cgroup.procs と cgroup.threads (書いて移す)、cgroup.controllers と cgroup.subtree_control (controller は
//              pids だけ)、cgroup.max.depth と cgroup.max.descendants (mkdir の上限: EAGAIN)、cgroup.stat、cgroup.type
//              (domain、domain threaded、domain invalid)、cgroup.events (populated と frozen)、cgroup.freeze (1 で中と
//              子孫を凍らせる: シグナルの道でとけるか SIGKILL まで眠る。proc.rs の freeze_while_frozen)、cgroup.kill
//              (中のみなに SIGKILL)、cgroup.stat.local、cpu.stat (走った時間) と cpu.stat.local。根には type、events、
//              freeze、kill、stat.local、pids.* がない
//   pids       親の cgroup.subtree_control に +pids で、子に pids.max / current / peak / events / events.local。数は
//              タスク (スレッドも 1 つ。片づけていないゾンビの代表もまだ数える) で、その cgroup と子孫のもの。fork と
//              スレッドを作るときに、いる (CLONE_INTO_CGROUP なら行き先の) cgroup から根の手前まで、どこかで pids.max を
//              こえれば EAGAIN (proc.rs の clone が can_fork を呼ぶ)。移すときは上限を見ない。pids は threaded の
//              controller なので、中にプロセスがいても +pids できる (子に中のものがいなければ)。そのときその cgroup は
//              domain threaded になり、子は domain invalid (移せない: EOPNOTSUPP)
//   mkdir      子の cgroup (名前に改行は EINVAL)。持ち主は作った人 (fsuid)、許しは mode のまま。ファイルは 0644 か 0444
//              (kill は 0200)。chmod と chown はできる (任せる: delegate)。ファイルは作れない (EACCES)、名前は変えられ
//              ない (EPERM)。rmdir は中にプロセスか子の cgroup があれば EBUSY
//   移す       cgroup.procs に pid (0 は自分、スレッドの番号ならそのプロセス) を書く: もとと行き先の共通の祖先の
//              cgroup.procs に書けなければ EACCES (Linux の cgroup_procs_write_permission)。cgroup.threads は domain の
//              cgroup どうしでは移せない (EOPNOTSUPP)
//   namespace  /proc/PID/cgroup のパスは、読む人の cgroup の namespace の根から (外のものは /../NAME)
//   /proc/PID/cgroup、/proc/cgroups (controller がないので見出しだけ)、/sys/kernel/cgroup の delegate と features は
//   procfs と sysfs が、ここのものを見せる
// まだ: threaded (cgroup.type に threaded を書くと EOPNOTSUPP)、cgroup.events の知らせ (poll と inotify)、v1 の名前つきの木、
// pids のほかの controller
use crate::cell::Bkl;
use crate::proc::{self, State};
use crate::vfs::*;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

const ESRCH: i64 = 3;
const EBADF: i64 = 9;
const EAGAIN: i64 = 11;
const EACCES: i64 = 13;
const EBUSY: i64 = 16;
const ERANGE: i64 = 34;
const CGROUP2_SUPER_MAGIC: u64 = 0x6367_7270;

/// 根の cgroup の番号 (ディレクトリの ino)
pub const ROOT: u64 = crate::ns::CGROUP_ROOT;

/// pids.max の "max" (Linux の PIDS_MAX: PID_MAX_LIMIT + 1。これから上は書けない)
const PIDS_MAX: i64 = 4 * 1024 * 1024 + 1;

/// ファイル (名前、許し、根にもあるか)。Linux の cgroup_base_files の順と、pids の pids_files (pids を持つ cgroup だけ)
const FILES: [(&str, u32, bool); 19] = [
    ("cgroup.type", 0o644, false),
    ("cgroup.procs", 0o644, true),
    ("cgroup.threads", 0o644, true),
    ("cgroup.controllers", 0o444, true),
    ("cgroup.subtree_control", 0o644, true),
    ("cgroup.events", 0o444, false),
    ("cgroup.max.descendants", 0o644, true),
    ("cgroup.max.depth", 0o644, true),
    ("cgroup.stat", 0o444, true),
    ("cgroup.stat.local", 0o444, false),
    ("cgroup.freeze", 0o644, false),
    ("cgroup.kill", 0o200, false),
    ("cpu.stat", 0o444, true),
    ("cpu.stat.local", 0o444, true),
    ("pids.max", 0o644, false),
    ("pids.current", 0o444, false),
    ("pids.peak", 0o444, false),
    ("pids.events", 0o444, false),
    ("pids.events.local", 0o444, false),
];
const F_TYPE: u8 = 0;
const F_PROCS: u8 = 1;
const F_THREADS: u8 = 2;
const F_CONTROLLERS: u8 = 3;
const F_SUBTREE: u8 = 4;
const F_EVENTS: u8 = 5;
const F_MAX_DESC: u8 = 6;
const F_MAX_DEPTH: u8 = 7;
const F_STAT: u8 = 8;
const F_STAT_LOCAL: u8 = 9;
const F_FREEZE: u8 = 10;
const F_KILL: u8 = 11;
const F_CPU_STAT: u8 = 12;
const F_CPU_STAT_LOCAL: u8 = 13;
const F_PIDS_MAX: u8 = 14;
const F_PIDS_CURRENT: u8 = 15;
const F_PIDS_PEAK: u8 = 16;
const F_PIDS_EVENTS: u8 = 17;
const F_PIDS_EVENTS_LOCAL: u8 = 18;
/// cgroup ごとの番号の幅 (ディレクトリと、そのあとにファイルの ino)
const STRIDE: u64 = 32;

/// 名前つきの許しと持ち主と時刻 (Linux の kernfs の iattr)
#[derive(Clone, Copy)]
struct Attr {
    mode: u32,
    uid: u32,
    gid: u32,
    mtime: u64,
    ctime: u64,
}

struct Cg {
    /// 親 (根は 0)
    parent: u64,
    name: String,
    /// 子の cgroup (作った順。消したものは外す)
    kids: Vec<u64>,
    /// cgroup.max.depth と cgroup.max.descendants (i32::MAX は max)
    max_depth: i32,
    max_desc: i32,
    /// 生きている子孫の数 (Linux の nr_descendants)
    nr_desc: u32,
    /// cgroup.freeze に書いた値と、凍らせた時刻 (ns)、それまでに凍っていた時間 (ns: cgroup.stat.local の frozen_usec)
    freeze: bool,
    freeze_at: u64,
    frozen_ns: u64,
    /// rmdir したもの (まだプロセス (ゾンビ) が指しているあいだだけ残す: /proc/PID/cgroup の " (deleted)")
    dead: bool,
    /// cgroup.subtree_control の pids (子が pids を持つ)
    subtree_pids: bool,
    /// pids の入れもの (Linux の pids_cgroup の css: 親が +pids しているあいだ。根は持たない (数えない))
    pids: Option<Pids>,
    dir: Attr,
    files: [Attr; FILES.len()],
}

/// pids の入れもの (Linux の pids_cgroup)。数 (pids.current) はとっておかず、そのつど数える (count)
#[derive(Clone, Copy)]
struct Pids {
    /// pids.max (PIDS_MAX は max)
    max: i64,
    /// pids.peak (Linux の watermark)
    peak: i64,
    /// events[PIDCG_MAX]: ここか子孫で上限に当たった数 (pids.events)
    ev_max: u64,
    /// events_local[PIDCG_MAX]: ここの上限に当たった数 (pids.events.local)
    local_max: u64,
    /// events_local[PIDCG_FORKFAIL]: ここで fork にしくじった数 (pids_localevents のときの pids.events と
    /// pids.events.local。はじめの 1 回は kmsg に)
    forkfail: u64,
}

const NEW_PIDS: Pids = Pids { max: PIDS_MAX, peak: 0, ev_max: 0, local_max: 0, forkfail: 0 };

static TREE: Bkl<BTreeMap<u64, Cg>> = Bkl::new(BTreeMap::new());
static NEXT: Bkl<u64> = Bkl::new(ROOT + STRIDE);
/// cgroup2 をかぶせたことがある (Linux の cgrp_dfl_visible: それまで /proc/PID/cgroup に出さない)
static VISIBLE: AtomicBool = AtomicBool::new(false);
/// nsdelegate (マウントのオプション: namespace の根を任せる境目にする。Linux の CGRP_ROOT_NS_DELEGATE)
static NS_DELEGATE: AtomicBool = AtomicBool::new(false);
/// pids_localevents (マウントのオプション: pids.events を v1 のように、ここで fork にしくじった数に。Linux の
/// CGRP_ROOT_PIDS_LOCAL_EVENTS)
static PIDS_LOCAL: AtomicBool = AtomicBool::new(false);
static FS: AtomicUsize = AtomicUsize::new(0);

fn fs_id() -> usize {
    match FS.load(Ordering::Relaxed) {
        0 => {
            let id = new_fs_id();
            FS.store(id, Ordering::Relaxed);
            id
        }
        id => id,
    }
}

fn attr(mode: u32, uid: u32, gid: u32) -> Attr {
    let now = crate::timer::epoch_ns();
    Attr { mode, uid, gid, mtime: now, ctime: now }
}

fn new_cg(parent: u64, name: String, uid: u32, gid: u32, mode: u32) -> Cg {
    Cg {
        parent,
        name,
        kids: Vec::new(),
        max_depth: i32::MAX,
        max_desc: i32::MAX,
        nr_desc: 0,
        freeze: false,
        freeze_at: 0,
        frozen_ns: 0,
        dead: false,
        subtree_pids: false,
        pids: None,
        dir: attr(S_IFDIR | mode, uid, gid),
        files: FILES.map(|(_, m, _)| attr(S_IFREG | m, uid, gid)),
    }
}

/// 木 (はじめて触るときに根を作る: 根のディレクトリは 0555、持ち主は root)
fn tree() -> &'static mut BTreeMap<u64, Cg> {
    let t = TREE.get();
    if t.is_empty() {
        t.insert(ROOT, new_cg(0, String::new(), 0, 0, 0o555));
    }
    t
}

fn get(id: u64) -> Option<&'static mut Cg> {
    tree().get_mut(&id)
}

fn live_cg(id: u64) -> Result<&'static mut Cg, i64> {
    get(id).filter(|c| !c.dead).ok_or(-ENODEV)
}

/// 根から id までの番号 (根をふくむ)
fn chain(id: u64) -> Vec<u64> {
    let mut v = Vec::new();
    let mut c = id;
    while c != 0 {
        v.push(c);
        c = get(c).map_or(0, |g| g.parent);
    }
    v.reverse();
    v
}

/// a は b か b の子孫か (Linux の cgroup_is_descendant)
fn descends(a: u64, b: u64) -> bool {
    let mut c = a;
    while c != 0 {
        if c == b {
            return true;
        }
        c = get(c).map_or(0, |g| g.parent);
    }
    false
}

/// 共通の祖先
fn common(a: u64, b: u64) -> u64 {
    let mut c = a;
    while !descends(b, c) {
        c = get(c).map_or(ROOT, |g| g.parent);
    }
    c
}

/// from (namespace の根) から見た id のパス (Linux の kernfs_path_from_node: 同じなら /、外は /.. を重ねる)
fn path_from(id: u64, from: u64) -> String {
    if id == from {
        return String::from("/");
    }
    let (ct, cf) = (chain(id), chain(from));
    let n = ct.iter().zip(cf.iter()).take_while(|(a, b)| a == b).count();
    let mut s = String::new();
    for _ in n..cf.len() {
        s.push_str("/..");
    }
    for &c in &ct[n..] {
        s.push('/');
        s.push_str(get(c).map_or("", |g| g.name.as_str()));
    }
    s
}

/// id とその子孫
fn subtree(id: u64) -> Vec<u64> {
    let mut v = alloc::vec![id];
    let mut i = 0;
    while i < v.len() {
        if let Some(g) = get(v[i]) {
            v.extend(g.kids.iter().copied());
        }
        i += 1;
    }
    v
}

/// 消した cgroup で、もうだれも指していないものを片づける (時間は親へ)
fn sweep() {
    let used: Vec<u64> = proc::live().filter(|p| p.state != State::Unused).map(|p| p.cgroup).collect();
    let gone: Vec<(u64, u64)> = tree().iter().filter(|(id, g)| g.dead && !used.contains(id) && !tree().values().any(|k| k.parent == **id)).map(|(&id, g)| (id, g.parent)).collect();
    for (id, parent) in gone {
        tree().remove(&id);
        proc::cgroup_forget(id, parent);
    }
}

/// id か、その祖先が凍らせている (Linux の e_freeze)
fn freezing(id: u64) -> bool {
    chain(id).iter().any(|&c| get(c).is_some_and(|g| g.freeze))
}

/// 凍らせていて、中と子孫のスレッドがみな凍って眠っている (Linux の CGRP_FROZEN。中が空でも)
fn frozen(id: u64) -> bool {
    let ids = subtree(id);
    freezing(id) && proc::live().filter(|p| p.state != State::Unused && p.state != State::Zombie && !p.kthread && ids.contains(&p.cgroup)).all(|p| p.frozen)
}

/// id の中と子孫のスレッドを、それぞれのいる cgroup のとおりに凍らせる・とかす
fn apply_freeze(id: u64) {
    let ids = subtree(id);
    for p in proc::live().filter(|p| p.state != State::Unused && p.state != State::Zombie && !p.kthread && ids.contains(&p.cgroup)) {
        let on = freezing(p.cgroup);
        proc::set_freezing(p, on);
    }
}

/// 中のプロセス (ゾンビでないスレッドがいる) があるか
fn populated(id: u64) -> bool {
    let ids = subtree(id);
    proc::live().any(|p| p.state != State::Unused && p.state != State::Zombie && !p.kthread && ids.contains(&p.cgroup))
}

// ---- threaded のかたち (Linux の cgroup_is_thread_root など。aios に threaded の cgroup はないので、threaded の
//      controller (pids) を +pids したところだけが thread root になる) ----

/// id そのものに (ゾンビでない) タスクがいる (Linux の cgroup_has_tasks)
fn has_tasks(id: u64) -> bool {
    proc::live().any(|p| p.state != State::Unused && p.state != State::Zombie && !p.kthread && p.cgroup == id)
}

/// 中のものがいる子の数 (Linux の nr_populated_domain_children)
fn populated_kids(id: u64) -> usize {
    get(id).map_or(0, |g| g.kids.iter().filter(|&&k| populated(k)).count())
}

/// タスクがいて threaded の controller を子に渡している (Linux の cgroup_is_thread_root: domain threaded)
fn thread_root(id: u64) -> bool {
    get(id).is_some_and(|g| g.subtree_pids) && has_tasks(id)
}

/// 祖先 (根はのぞく: mixable) に thread root がない (Linux の cgroup_is_valid_domain。あれば domain invalid)
fn valid_domain(id: u64) -> bool {
    let mut c = get(id).map_or(0, |g| g.parent);
    while c != 0 && c != ROOT {
        if thread_root(c) {
            return false;
        }
        c = get(c).map_or(0, |g| g.parent);
    }
    true
}

/// thread root になれる (Linux の cgroup_can_be_thread_root: 根か、中のものがいる子がない)
fn can_be_thread_root(id: u64) -> bool {
    id == ROOT || populated_kids(id) == 0
}

/// 移す・CLONE_INTO_CGROUP の行き先 (Linux の cgroup_migrate_vet_dst): domain invalid は EOPNOTSUPP、子に中のものが
/// いて controller を子に渡しているところは EBUSY (no internal process)
fn vet_dst(dst: u64) -> Result<(), i64> {
    if !valid_domain(dst) {
        return Err(-EOPNOTSUPP);
    }
    if can_be_thread_root(dst) {
        return Ok(());
    }
    if get(dst).is_some_and(|g| g.subtree_pids) {
        return Err(-EBUSY);
    }
    Ok(())
}

// ---- pids (Linux の kernel/cgroup/pids.c) ----

/// id にいるタスクを数える pids の入れもの (Linux の task_css(pids): id から上で pids を持つはじめのもの。なければ根)
fn eff(id: u64) -> u64 {
    let mut c = id;
    while c != ROOT {
        match get(c) {
            Some(g) if g.pids.is_some() => return c,
            Some(g) => c = g.parent,
            None => return ROOT,
        }
    }
    ROOT
}

/// e から根の手前までの pids の入れもの (Linux の parent_pids をたどるもの: 下から)
fn pids_chain(e: u64) -> Vec<u64> {
    let mut v = Vec::new();
    let mut c = e;
    while c != ROOT {
        let Some(g) = get(c) else { break };
        if g.pids.is_none() {
            break;
        }
        v.push(c);
        c = g.parent;
    }
    v
}

/// q と子孫 (消したものもふくむ: ゾンビが残っていれば数える) のタスクの数 (pids.current)
fn count(q: u64) -> i64 {
    proc::live().filter(|p| proc::pids_counted(p) && descends(p.cgroup, q)).count() as i64
}

fn pids_of(id: u64) -> Option<&'static mut Pids> {
    get(id).and_then(|g| g.pids.as_mut())
}

/// 上限に当たった (Linux の pids_event): forking は fork した (子の入る) ところの入れもの、over は上限をこえたところ
fn pids_event(forking: u64, over: u64) {
    if let Some(f) = pids_of(forking) {
        f.forkfail += 1;
        if f.forkfail == 1 {
            crate::println!("cgroup: fork rejected by pids controller in {}", path_from(forking, ROOT));
        }
    }
    if PIDS_LOCAL.load(Ordering::Relaxed) {
        return;
    }
    if let Some(o) = pids_of(over) {
        o.local_max += 1;
    }
    for c in pids_chain(over) {
        if let Some(p) = pids_of(c) {
            p.ev_max += 1;
        }
    }
}

/// fork とスレッドを作る前 (Linux の pids_can_fork と pids_try_charge): 子の入る cgroup から上へ、1 つ足して
/// pids.max をこえるところがあれば EAGAIN。こえなかったところの peak は上げたまま (しくじっても戻さない)
pub fn can_fork(target: u64) -> Result<(), i64> {
    let e = eff(target);
    for q in pids_chain(e) {
        let new = count(q) + 1;
        let Some(p) = pids_of(q) else { break };
        if new > p.max {
            pids_event(e, q);
            return Err(-EAGAIN);
        }
        p.peak = p.peak.max(new);
    }
    Ok(())
}

/// n 個のタスクを src の入れものから dst の入れものへ移す前の peak (Linux の pids_can_attach: 1 つずつ行き先から上に
/// 足し、もとから上を引く。共通の祖先は 1 つ多い時がある)
fn migrate_peaks(src: u64, dst: u64, n: i64) {
    if src == dst || n == 0 {
        return;
    }
    let old = pids_chain(src);
    for q in pids_chain(dst) {
        let c = count(q);
        if let Some(p) = pids_of(q) {
            p.peak = p.peak.max(if old.contains(&q) { c + 1 } else { c + n });
        }
    }
}

/// id に pids の入れものを作る (親が +pids した・+pids したところに mkdir した。Linux の css_create と、中のタスクを
/// 移す cgroup_update_dfl_csses)。ファイルの持ち主は作った人
fn pids_create(id: u64) {
    let Some(g) = get(id) else { return };
    if g.pids.is_some() {
        return;
    }
    let parent = g.parent;
    let n = count(id);
    // 中のタスクは親の入れものから移る: 親から上は共通の祖先 (1 つ足してから引く)
    if n > 0 {
        for q in pids_chain(eff(parent)) {
            let c = count(q);
            if let Some(p) = pids_of(q) {
                p.peak = p.peak.max(c + 1);
            }
        }
    }
    let c = crate::cred::current();
    let g = get(id).unwrap();
    for f in F_PIDS_MAX..=F_PIDS_EVENTS_LOCAL {
        g.files[f as usize] = attr(S_IFREG | FILES[f as usize].1, c.fsuid, c.fsgid);
    }
    g.pids = Some(Pids { peak: n, ..NEW_PIDS });
}

/// id の pids の入れものを捨てる (親が -pids した。中のタスクは親の入れものへ)
fn pids_destroy(id: u64) {
    let Some(g) = get(id) else { return };
    if g.pids.is_none() {
        return;
    }
    let parent = g.parent;
    migrate_peaks(id, eff(parent), count(id));
    get(id).unwrap().pids = None;
}

/// cgroup.subtree_control に書く (Linux の cgroup_subtree_control_write): "+pids" と "-pids" を空白で並べる (あとの
/// ものが勝つ)。ほかの名前や +/- のないものは EINVAL、使えない (親が渡していない) ものを + は ENOENT、子が +pids
/// しているのに - は EBUSY。+ は cgroup_vet_subtree_control_enable: domain invalid は EOPNOTSUPP、根でなく、子に中の
/// ものがいて、自分にもタスクがいれば EBUSY
fn subtree_write(id: u64, text: &str) -> Result<(), i64> {
    let mut want: Option<bool> = None;
    for tok in text.trim().split(' ').filter(|t| !t.is_empty()) {
        let (sign, name) = tok.split_at(tok.chars().next().map_or(0, |c| c.len_utf8()));
        if name != "pids" {
            return Err(crate::why::inval());
        }
        want = Some(match sign {
            "+" => true,
            "-" => false,
            _ => return Err(crate::why::inval()),
        });
    }
    let g = live_cg(id)?;
    let Some(on) = want else { return Ok(()) };
    if on == g.subtree_pids {
        return Ok(());
    }
    if on {
        let parent = g.parent;
        if id != ROOT && !get(parent).is_some_and(|p| p.subtree_pids) {
            return Err(-ENOENT);
        }
        if !valid_domain(id) {
            return Err(-EOPNOTSUPP);
        }
        if id != ROOT && !can_be_thread_root(id) && has_tasks(id) {
            return Err(-EBUSY);
        }
    } else if g.kids.iter().any(|&k| get(k).is_some_and(|c| !c.dead && c.subtree_pids)) {
        return Err(-EBUSY);
    }
    let kids: Vec<u64> = g.kids.iter().copied().filter(|&k| get(k).is_some_and(|c| !c.dead)).collect();
    g.subtree_pids = on;
    for k in kids {
        if on {
            pids_create(k);
        } else {
            pids_destroy(k);
        }
    }
    Ok(())
}

/// Linux の kstrtoll (base 0。終わりの改行 1 つはよい): まちがいは EINVAL、あふれは ERANGE
fn kstrtoll(s: &str) -> Result<i64, i64> {
    let s = s.strip_suffix('\n').unwrap_or(s);
    let (neg, s) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let (radix, d) = if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        (16, h)
    } else if s.len() > 1 && s.starts_with('0') {
        (8, &s[1..])
    } else {
        (10, s)
    };
    if d.is_empty() || !d.chars().all(|c| c.is_digit(radix)) {
        return Err(crate::why::inval());
    }
    let v = u64::from_str_radix(d, radix).map_err(|_| -ERANGE)?;
    if neg {
        if v > i64::MAX as u64 + 1 {
            return Err(-ERANGE);
        }
        Ok((v as i64).wrapping_neg())
    } else {
        i64::try_from(v).map_err(|_| -ERANGE)
    }
}

/// pids.max に書く (Linux の pids_max_write): "max" か 0 から PIDS_MAX の手前まで (ほかは EINVAL)
fn pids_max_write(id: u64, text: &str) -> Result<(), i64> {
    let t = text.trim();
    let v = if t == "max" {
        PIDS_MAX
    } else {
        let v = kstrtoll(t)?;
        if !(0..PIDS_MAX).contains(&v) {
            return Err(crate::why::inval());
        }
        v
    };
    pids_of(id).ok_or(-ENODEV)?.max = v;
    Ok(())
}

// ---- /proc と /sys に見せるもの ----

/// /proc/PID/cgroup (Linux の proc_cgroup_show): "0::パス"。パスは読む人の cgroup の namespace の根から。消した cgroup に
/// いるゾンビは " (deleted)"。cgroup2 をかぶせたことがなければ空
pub fn proc_text(p: &proc::Proc) -> String {
    if !VISIBLE.load(Ordering::Relaxed) {
        return String::new();
    }
    let from = proc::current().cred.ns.cgroup_root();
    let dead = get(p.cgroup).is_some_and(|g| g.dead);
    format!("0::{}{}\n", path_from(p.cgroup, from), if dead { " (deleted)" } else { "" })
}

/// /proc/cgroups (Linux の proc_cgroupstats_show): controller (pids) ごとに、木の番号 (v2 の木は 0)、cgroup の数 (根と、
/// 消してもまだ残っているものをふくむ)、使えるか
pub fn proc_cgroups() -> String {
    format!("#subsys_name\thierarchy\tnum_cgroups\tenabled\npids\t0\t{}\t1\n", tree().len())
}

/// /sys/kernel/cgroup/delegate (任せてよいファイル: Linux の CFTYPE_NS_DELEGATABLE のもの)
pub fn delegate_text() -> &'static str {
    "cgroup.procs\ncgroup.threads\ncgroup.subtree_control\n"
}

/// /sys/kernel/cgroup/features (マウントのオプションの名前)
pub fn features_text() -> &'static str {
    "nsdelegate\nfavordynmods\nmemory_localevents\nmemory_recursiveprot\nmemory_hugetlb_accounting\npids_localevents\n"
}

// ---- マウント ----

/// mount -t cgroup2 (Linux の cgroup2 の get_tree と parse_param): 根はいまの cgroup の namespace の根。オプションは
/// nsdelegate と pids_localevents (と、ほかの controller のための名前: 受けるだけ)。知らないものは EINVAL
/// ("cgroup2: Unknown parameter")
pub fn mount(data: &str) -> Result<InodeRef, i64> {
    let mut deleg = None;
    let mut local = false;
    for o in data.split(',').filter(|o| !o.is_empty()) {
        match o {
            "nsdelegate" => deleg = Some(true),
            "pids_localevents" => local = true,
            "favordynmods" | "memory_localevents" | "memory_recursiveprot" | "memory_hugetlb_accounting" => {}
            _ => {
                crate::println!("cgroup2: Unknown parameter '{}'", o.split('=').next().unwrap_or(o));
                return Err(crate::why::inval());
            }
        }
    }
    // はじめの namespace でかぶせたときだけ変えられる (Linux の apply_cgroup_root_flags と同じく、中からは見ない)。
    // nsdelegate は書いたときだけ立て、pids_localevents はかぶせるたびにそのとおりに (Linux はどちらもかぶせるたび)
    if proc::current().cred.ns.cgroup.is_none() {
        if let Some(d) = deleg {
            NS_DELEGATE.store(d, Ordering::Relaxed);
        }
        PIDS_LOCAL.store(local, Ordering::Relaxed);
        let mut o = String::new();
        if NS_DELEGATE.load(Ordering::Relaxed) {
            o.push_str(",nsdelegate");
        }
        if local {
            o.push_str(",pids_localevents");
        }
        set_fs_opts(fs_id(), o);
    }
    VISIBLE.store(true, Ordering::Relaxed);
    let root = proc::current().cred.ns.cgroup_root();
    Ok(Rc::new(CgInode { node: Node::Dir(root) }))
}

/// mount -t cgroup (v1): controller も名前つきの木もない (Linux も、controller がなく name= のないものは EINVAL)
pub fn mount_v1(_data: &str) -> Result<InodeRef, i64> {
    Err(crate::why::inval())
}

// ---- 移す ----

/// いまの人が src から dst へ移してよいか (Linux の cgroup_attach_permissions): 共通の祖先の cgroup.procs に書けるか
/// (EACCES)、nsdelegate なら両方が自分の namespace の中か (ENOENT)、行き先がタスクを持てるか (vet_dst: EOPNOTSUPP か
/// EBUSY)、スレッドだけなら同じ domain か (EOPNOTSUPP)
fn may_attach(src: u64, dst: u64, threadgroup: bool) -> Result<(), i64> {
    let com = common(src, dst);
    let a = get(com).ok_or(-ENOENT)?.files[F_PROCS as usize];
    crate::cred::current().check(&Meta { ino: 0, mode: a.mode, nlink: 1, uid: a.uid, gid: a.gid, size: 0, rdev: 0, blocks: 0, mtime: 0, ctime: 0, atime: 0 }, crate::cred::W)?;
    if NS_DELEGATE.load(Ordering::Relaxed) {
        let r = proc::current().cred.ns.cgroup_root();
        if !descends(src, r) || !descends(dst, r) {
            return Err(-ENOENT);
        }
    }
    vet_dst(dst)?;
    if !threadgroup && src != dst {
        return Err(-EOPNOTSUPP);
    }
    Ok(())
}

/// clone3 の CLONE_INTO_CGROUP (Linux の cgroup_css_set_fork): fd が cgroup2 のディレクトリでなければ EBADF、消したもの
/// なら ENODEV、移してよいか (may_attach)。答えはその番号と、そこが凍っているか (子ははじめから凍る)
pub fn into_cgroup(fd: i64, threadgroup: bool) -> Result<(u64, bool), i64> {
    let f = proc::current().files().get(fd as u64).cloned().ok_or(-EBADF)?;
    let b = f.borrow();
    let crate::file::Kind::Inode(ino, _) = &b.kind else { return Err(-EBADF) };
    let Some(Node::Dir(id)) = ino.as_any().downcast_ref::<CgInode>().map(|c| c.node) else { return Err(-EBADF) };
    live_cg(id)?;
    may_attach(proc::current().cgroup, id, threadgroup)?;
    Ok((id, freezing(id)))
}

/// Linux の kstrtoint (base 0: 0x は 16 進、0 から始まれば 8 進。終わりの改行 1 つはよい)
fn kstrtoint(s: &str) -> Option<i32> {
    let s = s.strip_suffix('\n').unwrap_or(s);
    let (neg, s) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let (radix, d) = if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        (16, h)
    } else if s.len() > 1 && s.starts_with('0') {
        (8, &s[1..])
    } else {
        (10, s)
    };
    if d.is_empty() {
        return None;
    }
    let v = i64::from_str_radix(d, radix).ok()?;
    let v = if neg { -v } else { v };
    i32::try_from(v).ok()
}

/// cgroup.procs と cgroup.threads に書く (Linux の __cgroup_procs_write)
fn procs_write(dst: u64, text: &str, threadgroup: bool) -> Result<(), i64> {
    live_cg(dst)?;
    let pid = kstrtoint(text.trim()).filter(|&p| p >= 0).ok_or_else(crate::why::inval)?;
    let t: &proc::Proc = if pid == 0 {
        proc::current()
    } else {
        let g = crate::ns::to_global(pid as u32).ok_or(-ESRCH)?;
        proc::find_any(g).ok_or(-ESRCH)?
    };
    let t: &proc::Proc = if threadgroup && !t.is_leader() { proc::find_leader_any(t.tgid).map_or(t, |l| &*l) } else { t };
    if t.kthread {
        return Err(crate::why::inval());
    }
    let src = t.cgroup;
    let (tgid, tid) = (t.tgid, t.pid);
    may_attach(src, dst, threadgroup)?;
    // 終わりかけ (ゾンビ) のものは移さない (Linux の cgroup_migrate_add_task は PF_EXITING を外す)。行き先が凍って
    // いれば凍らせ、そうでなければとかす (Linux の cgroup_freezer_migrate_task)
    let on = freezing(dst);
    let pick = |p: &proc::Proc| (if threadgroup { p.tgid == tgid } else { p.pid == tid }) && p.state != State::Unused && p.state != State::Zombie;
    // pids の数は移るものについていく (上限は見ない。peak は Linux の pids_can_attach のとおり)
    let n = proc::live().filter(|p| pick(p)).count() as i64;
    migrate_peaks(eff(src), eff(dst), n);
    for p in proc::live() {
        if pick(p) {
            p.cgroup = dst;
            proc::set_freezing(p, on);
        }
    }
    sweep();
    Ok(())
}

/// cgroup.kill に 1 (Linux の cgroup_kill): 中と子孫のプロセスみなに SIGKILL
fn kill(id: u64) {
    let ids = subtree(id);
    let tgids: Vec<u32> = proc::live().filter(|p| p.state != State::Unused && p.state != State::Zombie && !p.kthread && p.is_leader() && ids.contains(&p.cgroup)).map(|p| p.tgid).collect();
    for t in tgids {
        let _ = crate::signal::send_group(t, crate::signal::SIGKILL, crate::signal::SigInfo::from(crate::signal::SI_KERNEL));
    }
}

/// mkdir の上限 (Linux の cgroup_check_hierarchy_limits): 親から根まで、子孫の数と深さ
fn limits_ok(parent: u64) -> bool {
    let mut c = parent;
    let mut level = 0;
    while let Some(g) = get(c) {
        if g.nr_desc as i64 >= g.max_desc as i64 || level >= g.max_depth {
            return false;
        }
        level += 1;
        c = g.parent;
    }
    true
}

// ---- 中身 ----

/// (ユーザー, カーネル, nice) の ns: いまいるプロセスと、片づけたもの (根はみなの CPU の時間: Linux の root_cgroup_cputime)
fn cputime(id: u64) -> (u64, u64, u64) {
    if id == ROOT {
        let (mut u, mut s) = (0, 0);
        for c in 0..crate::smp::online() {
            let (run, sys) = proc::cpu_times(c);
            u += run.saturating_sub(sys);
            s += sys;
        }
        let nice: u64 = proc::live().filter(|p| p.state != State::Unused && p.is_leader() && p.nice > 0).map(|p| proc::user_sys_ns(p, false).0).sum();
        return (u, s, nice);
    }
    let ids = subtree(id);
    let (mut u, mut s, mut n) = (0, 0, 0);
    for &c in &ids {
        let d = proc::cgroup_dead(c);
        u += d[0];
        s += d[1];
        n += d[2];
    }
    for p in proc::live().filter(|p| p.state != State::Unused && p.is_leader() && !p.kthread && ids.contains(&p.cgroup)) {
        let (pu, ps) = proc::user_sys_ns(p, false);
        u += pu;
        s += ps;
        if p.nice > 0 {
            n += pu;
        }
    }
    (u, s, n)
}

fn max_text(v: i32) -> String {
    if v == i32::MAX { String::from("max\n") } else { format!("{}\n", v) }
}

fn events_text(v: u64) -> String {
    format!("max {}\n", v)
}

fn content(id: u64, f: u8) -> Result<String, i64> {
    let g = get(id).ok_or(-ENODEV)?;
    // pids.*: -pids したあとに開いたままの fd は ENODEV (Linux の kernfs の消えたファイル)
    let pids = || g.pids.ok_or(-ENODEV);
    let local = PIDS_LOCAL.load(Ordering::Relaxed);
    Ok(match f {
        // Linux の cgroup_type_show (threaded の cgroup はまだないので、そのほかの 3 つ)
        F_TYPE => String::from(if !valid_domain(id) {
            "domain invalid\n"
        } else if thread_root(id) {
            "domain threaded\n"
        } else {
            "domain\n"
        }),
        F_CONTROLLERS => String::from(if id == ROOT || get(g.parent).is_some_and(|p| p.subtree_pids) { "pids\n" } else { "" }),
        F_SUBTREE => String::from(if g.subtree_pids { "pids\n" } else { "" }),
        F_PIDS_MAX => {
            let m = pids()?.max;
            if m >= PIDS_MAX { String::from("max\n") } else { format!("{}\n", m) }
        }
        F_PIDS_CURRENT => {
            pids()?;
            format!("{}\n", count(id))
        }
        F_PIDS_PEAK => format!("{}\n", pids()?.peak),
        // pids_localevents なら、どちらもここで fork にしくじった数 (Linux の __pids_events_show)
        F_PIDS_EVENTS => {
            let p = pids()?;
            events_text(if local { p.forkfail } else { p.ev_max })
        }
        F_PIDS_EVENTS_LOCAL => {
            let p = pids()?;
            events_text(if local { p.forkfail } else { p.local_max })
        }
        F_PROCS | F_THREADS => {
            let mut v: Vec<u32> = proc::live()
                .filter(|p| p.state != State::Unused && !p.kthread && p.cgroup == id)
                .filter(|p| if f == F_PROCS { p.is_leader() && (p.state != State::Zombie || proc::threads_of(p.tgid).iter().any(|t| t.state != State::Zombie)) } else { p.state != State::Zombie })
                .map(|p| crate::ns::local_or_0(p.pid))
                .collect();
            v.sort_unstable();
            v.iter().map(|p| format!("{}\n", p)).collect()
        }
        F_CPU_STAT_LOCAL => String::new(),
        F_EVENTS => format!("populated {}\nfrozen {}\n", populated(id) as u8, frozen(id) as u8),
        F_MAX_DESC => max_text(g.max_desc),
        F_MAX_DEPTH => max_text(g.max_depth),
        // controller ごとの入れものの数 (Linux の nr_subsys_*: 自分のものと、子孫の生きているもの。持たなければ 0)
        F_STAT => {
            let sub = subtree(id);
            let dying = sub.iter().skip(1).filter(|&&c| get(c).is_some_and(|k| k.dead)).count();
            let has = |c: u64| c == ROOT || get(c).is_some_and(|k| k.pids.is_some());
            let n = if has(id) { 1 + sub.iter().skip(1).filter(|&&c| get(c).is_some_and(|k| !k.dead && k.pids.is_some())).count() } else { 0 };
            let dying_pids = sub.iter().skip(1).filter(|&&c| get(c).is_some_and(|k| k.dead && k.pids.is_some())).count();
            format!("nr_descendants {}\nnr_subsys_pids {}\nnr_dying_descendants {}\nnr_dying_subsys_pids {}\n", g.nr_desc, n, dying, dying_pids)
        }
        // 凍らせていたあいだ (cgroup.freeze に 1 を書いてから 0 を書くまで)
        F_STAT_LOCAL => {
            let now = if g.freeze { crate::timer::uptime_ns().saturating_sub(g.freeze_at) } else { 0 };
            format!("frozen_usec {}\n", (g.frozen_ns + now) / 1000)
        }
        F_FREEZE => format!("{}\n", g.freeze as u8),
        F_CPU_STAT => {
            let (u, s, n) = cputime(id);
            format!("usage_usec {}\nuser_usec {}\nsystem_usec {}\nnice_usec {}\n", (u + s) / 1000, u / 1000, s / 1000, n / 1000)
        }
        // 書くだけのもの (Linux の cgroup_seqfile_show: 読み方がなければ EINVAL)
        _ => return Err(crate::why::inval()),
    })
}

fn write(id: u64, f: u8, b: &[u8]) -> Result<(), i64> {
    // 0 バイトはなにもしない (Linux も 0 を返す: pids.max に "" でも EINVAL にならない)
    if b.is_empty() {
        return Ok(());
    }
    let text = core::str::from_utf8(b).map_err(|_| crate::why::inval())?;
    // nsdelegate のとき、自分の namespace の根のファイルは、任せてよいもの (procs、threads、subtree_control) のほかは EPERM
    if NS_DELEGATE.load(Ordering::Relaxed) && !matches!(f, F_PROCS | F_THREADS | F_SUBTREE) {
        let ns = &proc::current().cred.ns;
        if ns.cgroup.is_some() && ns.cgroup_root() == id {
            return Err(-EPERM);
        }
    }
    let g = live_cg(id)?;
    match f {
        F_PROCS => procs_write(id, text, true),
        F_THREADS => procs_write(id, text, false),
        F_SUBTREE => subtree_write(id, text),
        F_PIDS_MAX => pids_max_write(id, text),
        F_MAX_DEPTH | F_MAX_DESC => {
            let t = text.trim();
            let v = if t == "max" { i32::MAX } else { kstrtoint(t).ok_or_else(crate::why::inval)? };
            if v < 0 {
                return Err(-ERANGE);
            }
            if f == F_MAX_DEPTH {
                g.max_depth = v;
            } else {
                g.max_desc = v;
            }
            Ok(())
        }
        // threaded だけを受ける (まだない)
        F_TYPE => {
            if text.trim() != "threaded" {
                return Err(crate::why::inval());
            }
            Err(-EOPNOTSUPP)
        }
        F_FREEZE => {
            let v = kstrtoint(text.trim()).ok_or_else(crate::why::inval)?;
            if !(0..=1).contains(&v) {
                return Err(-ERANGE);
            }
            let on = v == 1;
            if g.freeze != on {
                let now = crate::timer::uptime_ns();
                if on {
                    g.freeze_at = now;
                } else {
                    g.frozen_ns += now.saturating_sub(g.freeze_at);
                }
                g.freeze = on;
                apply_freeze(id);
            }
            Ok(())
        }
        F_KILL => {
            let v = kstrtoint(text.trim()).ok_or_else(crate::why::inval)?;
            if v != 1 {
                return Err(-ERANGE);
            }
            kill(id);
            Ok(())
        }
        _ => Err(crate::why::inval()),
    }
}

// ---- ファイルシステム ----

#[derive(Clone, Copy, PartialEq)]
enum Node {
    Dir(u64),
    File(u64, u8),
}

pub struct CgInode {
    node: Node,
}

impl CgInode {
    fn ino(&self) -> u64 {
        match self.node {
            Node::Dir(id) => id,
            Node::File(id, f) => id + 1 + f as u64,
        }
    }

    fn attr(&self) -> Option<Attr> {
        match self.node {
            Node::Dir(id) => get(id).map(|g| g.dir),
            Node::File(id, f) => get(id).map(|g| g.files[f as usize]),
        }
    }

    fn set_attr(&self, f: impl FnOnce(&mut Attr)) -> Result<(), i64> {
        let a = match self.node {
            Node::Dir(id) => &mut get(id).ok_or(-ENOENT)?.dir,
            Node::File(id, i) => &mut get(id).ok_or(-ENOENT)?.files[i as usize],
        };
        f(a);
        a.ctime = crate::timer::epoch_ns();
        Ok(())
    }
}

/// id にそのファイルがあるか (pids.* は pids の入れものを持つあいだだけ)
fn file_on(id: u64, f: usize) -> bool {
    if f >= F_PIDS_MAX as usize {
        return id != ROOT && get(id).is_some_and(|g| g.pids.is_some());
    }
    id != ROOT || FILES[f].2
}

impl Inode for CgInode {
    fn id(&self) -> (usize, u64) {
        (fs_id(), self.ino())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn meta(&self) -> Meta {
        let a = self.attr().unwrap_or(Attr { mode: S_IFREG, uid: 0, gid: 0, mtime: 0, ctime: 0 });
        let nlink = match self.node {
            Node::Dir(id) => 2 + get(id).map_or(0, |g| g.kids.len() as u32),
            Node::File(..) => 1,
        };
        Meta { ino: self.ino(), mode: a.mode, nlink, uid: a.uid, gid: a.gid, size: 0, rdev: 0, blocks: 0, mtime: a.mtime, ctime: a.ctime, atime: a.mtime }
    }

    fn read_at(&self, off: usize, buf: &mut [u8]) -> Result<usize, i64> {
        let Node::File(id, f) = self.node else { return Err(-EISDIR) };
        let s = content(id, f)?;
        let b = s.as_bytes();
        if off >= b.len() {
            return Ok(0);
        }
        let n = buf.len().min(b.len() - off);
        buf[..n].copy_from_slice(&b[off..off + n]);
        Ok(n)
    }

    fn write_at(&self, _off: usize, b: &[u8]) -> Result<usize, i64> {
        let Node::File(id, f) = self.node else { return Err(-EISDIR) };
        write(id, f, b)?;
        Ok(b.len())
    }

    fn whole_write(&self) -> bool {
        true
    }

    fn truncate(&self, _: usize) -> Result<(), i64> {
        // O_TRUNC で開くのはかまわない (echo PID > cgroup.procs)
        Ok(())
    }

    fn readlink(&self) -> Result<String, i64> {
        Err(crate::why::inval())
    }

    fn lookup(&self, name: &str) -> Result<InodeRef, i64> {
        let Node::Dir(id) = self.node else { return Err(-ENOTDIR) };
        let g = get(id).ok_or(-ENOENT)?;
        if g.dead {
            return Err(-ENOENT);
        }
        if let Some(f) = FILES.iter().position(|(n, _, _)| *n == name).filter(|&f| file_on(id, f)) {
            return Ok(Rc::new(CgInode { node: Node::File(id, f as u8) }));
        }
        let k = g.kids.iter().copied().find(|&k| get(k).is_some_and(|c| c.name == name)).ok_or(-ENOENT)?;
        Ok(Rc::new(CgInode { node: Node::Dir(k) }))
    }

    fn readdir(&self) -> Result<Vec<DirEntry>, i64> {
        let Node::Dir(id) = self.node else { return Err(-ENOTDIR) };
        let g = get(id).ok_or(-ENOENT)?;
        let mut v = Vec::new();
        for (f, (name, _, _)) in FILES.iter().enumerate().filter(|&(f, _)| file_on(id, f)) {
            v.push(DirEntry { name: (*name).into(), ino: id + 1 + f as u64, mode: g.files[f].mode });
        }
        for &k in &g.kids {
            if let Some(c) = get(k) {
                v.push(DirEntry { name: c.name.clone(), ino: k, mode: c.dir.mode });
            }
        }
        Ok(v)
    }

    /// mkdir (Linux の cgroup_mkdir)。ファイルは作れない (kernfs に create がない: EACCES)、ほか (シンボリックリンク、
    /// 装置) は EPERM
    fn create(&self, name: &str, mode: u32, node: NewNode) -> Result<InodeRef, i64> {
        let Node::Dir(parent) = self.node else { return Err(-ENOTDIR) };
        match node {
            NewNode::Dir => {}
            NewNode::File => return Err(-EACCES),
            _ => return Err(-EPERM),
        }
        if name.contains('\n') {
            return Err(crate::why::inval());
        }
        let p = live_cg(parent)?;
        if FILES.iter().any(|(n, _, _)| *n == name) || p.kids.iter().any(|&k| get(k).is_some_and(|c| c.name == name)) {
            return Err(-EEXIST);
        }
        if !limits_ok(parent) {
            return Err(-EAGAIN);
        }
        let id = *NEXT.get();
        *NEXT.get() += STRIDE;
        let c = crate::cred::current();
        tree().insert(id, new_cg(parent, name.into(), c.fsuid, c.fsgid, mode & 0o7777));
        let now = crate::timer::epoch_ns();
        if let Some(p) = get(parent) {
            p.kids.push(id);
            p.dir.mtime = now;
            p.dir.ctime = now;
        }
        let mut a = parent;
        while let Some(g) = get(a) {
            g.nr_desc += 1;
            a = g.parent;
        }
        // 親が +pids していれば、はじめから pids を持つ
        if get(parent).is_some_and(|g| g.subtree_pids) {
            pids_create(id);
        }
        Ok(Rc::new(CgInode { node: Node::Dir(id) }))
    }

    fn link(&self, _: &str, _: &InodeRef) -> Result<(), i64> {
        Err(-EPERM)
    }

    /// rmdir (Linux の cgroup_rmdir: 中にプロセスか子の cgroup があれば EBUSY)。ファイルは消せない (EPERM)
    fn unlink(&self, name: &str, rmdir: bool) -> Result<(), i64> {
        let Node::Dir(parent) = self.node else { return Err(-ENOTDIR) };
        let p = get(parent).ok_or(-ENOENT)?;
        let k = p.kids.iter().copied().find(|&k| get(k).is_some_and(|c| c.name == name));
        let Some(k) = k else {
            return Err(if FILES.iter().any(|(n, _, _)| *n == name) { if rmdir { -ENOTDIR } else { -EPERM } } else { -ENOENT });
        };
        if !rmdir {
            return Err(-EISDIR);
        }
        if populated(k) || get(k).is_some_and(|c| c.kids.iter().any(|&kk| get(kk).is_some_and(|x| !x.dead))) {
            return Err(-EBUSY);
        }
        if let Some(c) = get(k) {
            c.dead = true;
        }
        let now = crate::timer::epoch_ns();
        if let Some(p) = get(parent) {
            p.kids.retain(|&x| x != k);
            p.dir.mtime = now;
            p.dir.ctime = now;
        }
        let mut a = parent;
        while let Some(g) = get(a) {
            g.nr_desc = g.nr_desc.saturating_sub(1);
            a = g.parent;
        }
        sweep();
        Ok(())
    }

    fn rename(&self, _: &str, _: &InodeRef, _: &str) -> Result<(), i64> {
        Err(-EPERM)
    }

    fn set_mode(&self, mode: u32) -> Result<(), i64> {
        self.set_attr(|a| a.mode = (a.mode & S_IFMT) | (mode & 0o7777))
    }

    fn set_owner(&self, uid: Option<u32>, gid: Option<u32>) -> Result<(), i64> {
        self.set_attr(|a| {
            if let Some(u) = uid {
                a.uid = u;
            }
            if let Some(g) = gid {
                a.gid = g;
            }
        })
    }

    fn set_mtime(&self, ns: u64) -> Result<(), i64> {
        self.set_attr(|a| a.mtime = ns)
    }

    fn statfs(&self) -> [u8; 120] {
        crate::tmpfs::statfs_bytes(CGROUP2_SUPER_MAGIC, 4096, 0, 0, 0, 0)
    }
}

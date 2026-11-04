// nftables の決まりの表 (Linux 7.3 の net/netfilter/nfnetlink.c と nf_tables_api.c と同じ形と答え)。
// doc/aios.md の「ファイアウォール (nftables)」の段階 1: NETLINK_NETFILTER の nfnetlink で、表 (table)、鎖 (chain)、
// 決まり (rule)、集まり (set) とその要素を作る・消す・見る。段階 2: 式を動かす VM (このファイルの終わりの「評価」:
// nft_do_chain と式ごとの eval) と、1 つのフックの基の鎖を順に通す hook。パケットがどこでフックを通るかは nfhook.rs。
//
// - nfnetlink: メッセージの type は (subsys << 8) | msg、頭のうしろに nfgenmsg (family、version、res_id (BE))。
//   nf_tables は subsys 10。変えるもの (NEW* / DEL* / DESTROY*) は batch の中だけ (外は EINVAL)、見るもの (GET*) は外だけ
// - batch: NFNL_MSG_BATCH_BEGIN (res_id 10、NFNL_BATCH_GENID があれば生成番号と同じこと: ちがえば ERESTART) から
//   NFNL_MSG_BATCH_END まで。いまの表を写したものに順に当て、みな通れば入れかえて生成番号を進める (commit)。
//   ひとつでもしくじれば (END がなくても) 捨てる (abort)。しくじりと NLM_F_ACK の答えは、batch を終えてから順に
//   (Linux と同じく、しくじっても残りもみな見る)。commit のたしかめ (jump の深さ、フックに置けない式) でしくじれば、batch を
//   はじめからやりなおして、しくじった決まりがエラーを返す (Linux の NFNL_BATCH_REPLAY と NFT_VALIDATE_DO)
// - 番号 (handle): 表は全体で 1 から、鎖・決まり・集まりは表ごとに 1 から。batch を捨てても進んだまま (Linux と同じ)
// - しくじりの答えには、Linux と同じところで NLMSGERR_ATTR_OFFS (どの属性か: nft がそれで「^^^」の場所を決める)
// - 式 (expr): immediate cmp payload meta bitwise byteorder lookup range counter log limit reject quota を読んで、
//   Linux の dump と同じ形で返す (レジスタは 32 ビットの番号にしてから: NFT_REG32_00 は NFT_REG_1 で返る)。
//   ほかの名前は ENOENT (手本の Linux に入れていないもの: ct など)
// - family は ip、ip6、inet (手本の Linux の test/linux.config と同じ。arp、bridge、netdev は EOPNOTSUPP)
use crate::cell::Bkl;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

const EPERM: i64 = 1;
const ENOENT: i64 = 2;
const E2BIG: i64 = 7;
const EBUSY: i64 = 16;
const EEXIST: i64 = 17;
const EINVAL: i64 = 22;
const ERANGE: i64 = 34;
const EMLINK: i64 = 31;
const EOPNOTSUPP: i64 = 95;
const ERESTART: i64 = 85;

pub const NETLINK_NETFILTER: u32 = 12;
const NLMSG_HDRLEN: usize = 16;
const NLMSG_DONE: u16 = 3;
const NLMSG_MIN_TYPE: u16 = 16;
const NLM_F_REQUEST: u16 = 0x1;
const NLM_F_MULTI: u16 = 0x2;
const NLM_F_ACK: u16 = 0x4;
const NLM_F_DUMP: u16 = 0x300;
// NEW のときのフラグ
const NLM_F_REPLACE: u16 = 0x100;
const NLM_F_EXCL: u16 = 0x200;
const NLM_F_CREATE: u16 = 0x400;
const NLM_F_APPEND: u16 = 0x800;
// DEL のときのフラグ
const NLM_F_NONREC: u16 = 0x100;

const NFNL_SUBSYS_NFTABLES: u16 = 10;
const NFNL_SUBSYS_COUNT: u16 = 13;
const NFNL_MSG_BATCH_BEGIN: u16 = 16;
const NFNL_MSG_BATCH_END: u16 = 17;
const NFNL_BATCH_GENID: u16 = 1;

// nf_tables_msg_types
const NEWTABLE: u16 = 0;
const GETTABLE: u16 = 1;
const DELTABLE: u16 = 2;
const NEWCHAIN: u16 = 3;
const GETCHAIN: u16 = 4;
const DELCHAIN: u16 = 5;
const NEWRULE: u16 = 6;
const GETRULE: u16 = 7;
const DELRULE: u16 = 8;
const NEWSET: u16 = 9;
const GETSET: u16 = 10;
const DELSET: u16 = 11;
const NEWSETELEM: u16 = 12;
const GETSETELEM: u16 = 13;
const DELSETELEM: u16 = 14;
const NEWGEN: u16 = 15;
const GETGEN: u16 = 16;
const NEWOBJ: u16 = 18;
const GETOBJ: u16 = 19;
const DELOBJ: u16 = 20;
const GETOBJ_RESET: u16 = 21;
const NEWFLOWTABLE: u16 = 22;
const GETFLOWTABLE: u16 = 23;
const DELFLOWTABLE: u16 = 24;
const GETRULE_RESET: u16 = 25;
const DESTROYTABLE: u16 = 26;
const DESTROYCHAIN: u16 = 27;
const DESTROYRULE: u16 = 28;
const DESTROYSET: u16 = 29;
const DESTROYSETELEM: u16 = 30;
const DESTROYOBJ: u16 = 31;
const DESTROYFLOWTABLE: u16 = 32;
const GETSETELEM_RESET: u16 = 33;
const MSG_MAX: u16 = 34;

// family (NFPROTO_*)
const NFPROTO_UNSPEC: u8 = 0;
const NFPROTO_INET: u8 = 1;
const NFPROTO_IPV4: u8 = 2;
const NFPROTO_IPV6: u8 = 10;

// 属性
const NLA_TYPE_MASK: u16 = 0x3fff;
const NFTA_LIST_ELEM: u16 = 1;
const NFTA_TABLE_NAME: u16 = 1;
const NFTA_TABLE_FLAGS: u16 = 2;
const NFTA_TABLE_USE: u16 = 3;
const NFTA_TABLE_HANDLE: u16 = 4;
const NFTA_TABLE_USERDATA: u16 = 6;
const NFTA_TABLE_OWNER: u16 = 7;
const NFT_TABLE_F_DORMANT: u32 = 1;
const NFT_TABLE_F_OWNER: u32 = 2;
const NFT_TABLE_F_PERSIST: u32 = 4;
const NFT_TABLE_F_MASK: u32 = 7;
const NFTA_CHAIN_TABLE: u16 = 1;
const NFTA_CHAIN_HANDLE: u16 = 2;
const NFTA_CHAIN_NAME: u16 = 3;
const NFTA_CHAIN_HOOK: u16 = 4;
const NFTA_CHAIN_POLICY: u16 = 5;
const NFTA_CHAIN_USE: u16 = 6;
const NFTA_CHAIN_TYPE: u16 = 7;
const NFTA_CHAIN_COUNTERS: u16 = 8;
const NFTA_CHAIN_FLAGS: u16 = 10;
const NFTA_CHAIN_ID: u16 = 11;
const NFTA_CHAIN_USERDATA: u16 = 12;
const NFT_CHAIN_BASE: u32 = 1;
const NFT_CHAIN_HW_OFFLOAD: u32 = 2;
const NFT_CHAIN_BINDING: u32 = 4;
const NFTA_HOOK_HOOKNUM: u16 = 1;
const NFTA_HOOK_PRIORITY: u16 = 2;
#[allow(dead_code)]
const NFTA_HOOK_DEV: u16 = 3;
const NFTA_HOOK_DEVS: u16 = 4;
const NFTA_RULE_TABLE: u16 = 1;
const NFTA_RULE_CHAIN: u16 = 2;
const NFTA_RULE_HANDLE: u16 = 3;
const NFTA_RULE_EXPRESSIONS: u16 = 4;
const NFTA_RULE_COMPAT: u16 = 5;
const NFTA_RULE_POSITION: u16 = 6;
const NFTA_RULE_USERDATA: u16 = 7;
const NFTA_RULE_ID: u16 = 9;
const NFTA_RULE_POSITION_ID: u16 = 10;
const NFTA_RULE_CHAIN_ID: u16 = 11;
const NFTA_EXPR_NAME: u16 = 1;
const NFTA_EXPR_DATA: u16 = 2;
const NFTA_SET_TABLE: u16 = 1;
const NFTA_SET_NAME: u16 = 2;
const NFTA_SET_FLAGS: u16 = 3;
const NFTA_SET_KEY_TYPE: u16 = 4;
const NFTA_SET_KEY_LEN: u16 = 5;
const NFTA_SET_DATA_TYPE: u16 = 6;
const NFTA_SET_DATA_LEN: u16 = 7;
const NFTA_SET_POLICY: u16 = 8;
const NFTA_SET_DESC: u16 = 9;
const NFTA_SET_ID: u16 = 10;
const NFTA_SET_TIMEOUT: u16 = 11;
const NFTA_SET_GC_INTERVAL: u16 = 12;
const NFTA_SET_USERDATA: u16 = 13;
const NFTA_SET_OBJ_TYPE: u16 = 15;
const NFTA_SET_HANDLE: u16 = 16;
const NFTA_SET_EXPR: u16 = 17;
const NFTA_SET_EXPRESSIONS: u16 = 18;
const NFTA_SET_TYPE: u16 = 19;
const NFTA_SET_COUNT: u16 = 20;
const NFTA_SET_DESC_SIZE: u16 = 1;
const NFTA_SET_DESC_CONCAT: u16 = 2;
const NFTA_SET_FIELD_LEN: u16 = 1;
const NFT_SET_ANONYMOUS: u32 = 1;
const NFT_SET_CONSTANT: u32 = 2;
const NFT_SET_INTERVAL: u32 = 4;
const NFT_SET_MAP: u32 = 8;
const NFT_SET_TIMEOUT: u32 = 16;
const NFT_SET_EVAL: u32 = 32;
const NFT_SET_OBJECT: u32 = 64;
const NFT_SET_CONCAT: u32 = 128;
const NFT_SET_EXPR: u32 = 256;
const NFTA_SET_ELEM_KEY: u16 = 1;
const NFTA_SET_ELEM_DATA: u16 = 2;
const NFTA_SET_ELEM_FLAGS: u16 = 3;
const NFTA_SET_ELEM_TIMEOUT: u16 = 4;
const NFTA_SET_ELEM_EXPIRATION: u16 = 5;
const NFTA_SET_ELEM_USERDATA: u16 = 6;
const NFTA_SET_ELEM_EXPR: u16 = 7;
const NFTA_SET_ELEM_OBJREF: u16 = 9;
const NFTA_SET_ELEM_KEY_END: u16 = 10;
const NFTA_SET_ELEM_EXPRESSIONS: u16 = 11;
const NFT_SET_ELEM_INTERVAL_END: u32 = 1;
const NFT_SET_ELEM_CATCHALL: u32 = 2;
const NFTA_SET_ELEM_LIST_TABLE: u16 = 1;
const NFTA_SET_ELEM_LIST_SET: u16 = 2;
const NFTA_SET_ELEM_LIST_ELEMENTS: u16 = 3;
const NFTA_SET_ELEM_LIST_SET_ID: u16 = 4;
const NFTA_DATA_VALUE: u16 = 1;
const NFTA_DATA_VERDICT: u16 = 2;
const NFTA_VERDICT_CODE: u16 = 1;
const NFTA_VERDICT_CHAIN: u16 = 2;
const NFTA_VERDICT_CHAIN_ID: u16 = 3;
const NFTA_GEN_ID: u16 = 1;
const NFTA_GEN_PROC_PID: u16 = 2;
const NFTA_GEN_PROC_NAME: u16 = 3;

/// NFT_DATA_VERDICT (集まりの中身の型)
const NFT_DATA_VERDICT: u32 = 0xffff_ff00;
const NFT_DATA_RESERVED_MASK: u32 = 0xffff_ff00;
const NFT_DATA_VALUE_MAXLEN: usize = 64;
const NAME_MAX: usize = 255;
const USERDATA_MAX: usize = 256;
const NFT_RULE_MAXEXPRS: usize = 128;
/// 鎖の深さ (jump / goto をたどる上限: Linux の NFT_JUMP_STACK_SIZE)
const NFT_JUMP_STACK_SIZE: usize = 16;

// 判定 (verdict)
const NF_DROP: i32 = 0;
const NF_ACCEPT: i32 = 1;
const NF_QUEUE: i32 = 3;
const NFT_CONTINUE: i32 = -1;
const NFT_BREAK: i32 = -2;
const NFT_JUMP: i32 = -3;
const NFT_GOTO: i32 = -4;
const NFT_RETURN: i32 = -5;
const NF_VERDICT_MASK: u32 = 0xff;

// ---- 属性を読む ----

/// 属性 1 つ: 中身と、メッセージの頭からの属性 (頭の 4 バイト) の場所 (しくじりの NLMSGERR_ATTR_OFFS)
#[derive(Clone, Copy)]
struct A<'a> {
    d: &'a [u8],
    off: usize,
}

impl<'a> A<'a> {
    fn be32(&self) -> u32 {
        u32::from_be_bytes(self.d[..4].try_into().unwrap())
    }
    fn be64(&self) -> u64 {
        u64::from_be_bytes(self.d[..8].try_into().unwrap())
    }
    /// NLA_STRING: NUL まで (なくてもよい)
    fn s(&self) -> &'a [u8] {
        let n = self.d.iter().position(|&c| c == 0).unwrap_or(self.d.len());
        &self.d[..n]
    }
    fn string(&self) -> String {
        String::from_utf8_lossy(self.s()).into_owned()
    }
    /// 中の属性 (この属性の中身から)
    fn nested(&self, max: u16) -> Vec<Option<A<'a>>> {
        parse(self.d, self.off + 4, max)
    }
    /// 中の属性を順に
    fn list(&self) -> Vec<(u16, A<'a>)> {
        each(self.d, self.off + 4)
    }
}

/// nla_parse_deprecated: type ごとに (max より大きいものは読まない。同じものがいくつもあれば最後のもの)
fn parse(buf: &[u8], base: usize, max: u16) -> Vec<Option<A<'_>>> {
    let mut t = vec![None; max as usize + 1];
    for (ty, a) in each(buf, base) {
        if ty <= max {
            t[ty as usize] = Some(a);
        }
    }
    t
}

/// 属性を順に (長さのおかしいところで止まる: nla_for_each_attr)
fn each(mut buf: &[u8], mut base: usize) -> Vec<(u16, A<'_>)> {
    let mut v = Vec::new();
    while buf.len() >= 4 {
        let len = u16::from_le_bytes([buf[0], buf[1]]) as usize;
        if len < 4 || len > buf.len() {
            break;
        }
        let ty = u16::from_le_bytes([buf[2], buf[3]]) & NLA_TYPE_MASK;
        v.push((ty, A { d: &buf[4..len], off: base }));
        let step = align4(len).min(buf.len());
        buf = &buf[step..];
        base += step;
    }
    v
}

fn align4(n: usize) -> usize {
    (n + 3) & !3
}

/// 属性の決まり (nla_policy の要るところだけ): 短い数は ERANGE、長い名前も ERANGE
#[derive(Clone, Copy)]
enum P {
    U32,
    U64,
    Str(usize),
    Bin(usize),
    Nest,
}

/// しくじり: errno と、Linux が NL_SET_BAD_ATTR で指す属性
#[derive(Clone, Copy, Debug)]
pub struct Fail {
    pub err: i64,
    pub off: Option<usize>,
    pub msg: Option<&'static str>,
}

type Res<T> = Result<T, Fail>;

fn fail(err: i64) -> Fail {
    Fail { err: -err, off: None, msg: None }
}

fn bad(err: i64, a: Option<A>) -> Fail {
    Fail { err: -err, off: a.map(|a| a.off), msg: None }
}

/// policy で長さを確かめる (validate_nla): ちがえば ERANGE と「Attribute failed policy validation」
fn check(t: &[Option<A>], pol: &[(u16, P)]) -> Res<()> {
    for &(ty, p) in pol {
        let Some(a) = t.get(ty as usize).copied().flatten() else { continue };
        let ok = match p {
            P::U32 => a.d.len() >= 4,
            P::U64 => a.d.len() >= 8,
            P::Str(max) => a.s().len() <= max,
            P::Bin(max) => a.d.len() <= max,
            P::Nest => a.d.is_empty() || a.d.len() >= 4,
        };
        if !ok {
            return Err(Fail { err: -ERANGE, off: Some(a.off), msg: Some("Attribute failed policy validation") });
        }
    }
    Ok(())
}

// ---- 書く ----

/// 属性を積む (入れ子は start / end)
struct W(Vec<u8>);

impl W {
    fn attr(&mut self, ty: u16, d: &[u8]) {
        let len = 4 + d.len();
        self.0.extend_from_slice(&(len as u16).to_le_bytes());
        self.0.extend_from_slice(&ty.to_le_bytes());
        self.0.extend_from_slice(d);
        self.0.resize(align4(self.0.len()), 0);
    }
    fn be32(&mut self, ty: u16, v: u32) {
        self.attr(ty, &v.to_be_bytes());
    }
    fn be64(&mut self, ty: u16, v: u64) {
        self.attr(ty, &v.to_be_bytes());
    }
    fn str(&mut self, ty: u16, s: &str) {
        let mut b = s.as_bytes().to_vec();
        b.push(0);
        self.attr(ty, &b);
    }
    fn start(&mut self, ty: u16) -> usize {
        let at = self.0.len();
        self.0.extend_from_slice(&[0, 0]);
        self.0.extend_from_slice(&ty.to_le_bytes());
        at
    }
    fn end(&mut self, at: usize) {
        let len = (self.0.len() - at) as u16;
        self.0[at..at + 2].copy_from_slice(&len.to_le_bytes());
    }
}

/// nfnl_msg_put: 頭と nfgenmsg (res_id は生成番号の下 16 ビット)
fn msg(typ: u16, flags: u16, seq: u32, pid: u32, family: u8, res_id: u16, attrs: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(NLMSG_HDRLEN + 4 + attrs.len());
    let len = NLMSG_HDRLEN + 4 + attrs.len();
    m.extend_from_slice(&(len as u32).to_le_bytes());
    m.extend_from_slice(&((NFNL_SUBSYS_NFTABLES << 8) | typ).to_le_bytes());
    m.extend_from_slice(&flags.to_le_bytes());
    m.extend_from_slice(&seq.to_le_bytes());
    m.extend_from_slice(&pid.to_le_bytes());
    m.extend_from_slice(&[family, 0]);
    m.extend_from_slice(&res_id.to_be_bytes());
    m.extend_from_slice(attrs);
    m
}

// ---- 表 ----

#[derive(Clone)]
struct Table {
    family: u8,
    name: String,
    handle: u64,
    flags: u32,
    udata: Option<Vec<u8>>,
    owner: u32,
    chains: Vec<Chain>,
    sets: Vec<Set>,
}

#[derive(Clone)]
struct Base {
    hooknum: u32,
    prio: i32,
    policy: u32,
    typ: &'static str,
    /// フックに入れた順 (同じ priority なら先に入れたものから: nf_hook_entries_grow)
    seq: u64,
}

#[derive(Clone)]
struct Chain {
    name: String,
    handle: u64,
    base: Option<Base>,
    flags: u32,
    udata: Option<Vec<u8>>,
    rules: Vec<Rule>,
    /// この batch で作ったときの NFTA_CHAIN_ID (0 はなし)
    id: u32,
    /// 基の鎖: 通ったパケットの数と、policy まで来た数 (/proc/ai/fw)
    seen: u64,
    to_policy: u64,
}

#[derive(Clone)]
struct Rule {
    handle: u64,
    exprs: Vec<Expr>,
    udata: Option<Vec<u8>>,
    /// この batch の NFTA_RULE_ID
    id: u32,
    /// 当たった数 (式がみな通った: counter がなくても数える。/proc/ai/fw) と、さいごに当たった時刻
    hits: u64,
    last: u64,
}

/// 式: 名前と、Linux の dump と同じ形の NFTA_EXPR_DATA の中身。たどるもの (集まり、jump / goto の先の鎖) も
#[derive(Clone)]
struct Expr {
    name: &'static str,
    data: Vec<u8>,
    set: Option<String>,
    chain: Option<String>,
    /// 評価するもの (段階 2)。counter などは数もここに
    op: Op,
}

#[derive(Clone, PartialEq)]
enum Data {
    Value(Vec<u8>),
    Verdict(i32, Option<String>),
}

#[derive(Clone)]
struct Set {
    name: String,
    handle: u64,
    flags: u32,
    ktype: u32,
    klen: u32,
    dtype: u32,
    dlen: u32,
    objtype: u32,
    policy: u32,
    size: u32,
    fields: Vec<u32>,
    timeout: u64,
    gc: u32,
    udata: Option<Vec<u8>>,
    elems: Vec<Elem>,
    /// この batch の NFTA_SET_ID
    id: u32,
    /// lookup などで使っている決まりの数 (名前のない集まりは、使うものがなくなると消える)
    bound: u32,
}

#[derive(Clone)]
struct Elem {
    key: Vec<u8>,
    key_end: Option<Vec<u8>>,
    data: Option<Data>,
    flags: Option<u32>,
    timeout: Option<u64>,
    udata: Option<Vec<u8>>,
    /// 消える時刻 (起きてからの ns。0 は消えない)
    expires: u64,
}

#[derive(Clone, Default)]
struct State {
    tables: Vec<Table>,
}

struct Glob {
    cur: State,
    /// 生成番号 (commit で進む。0 にはならない)
    base_seq: u32,
    /// 表の番号 (全体で)
    table_handle: u64,
    /// 表ごとの番号 (鎖・決まり・集まり。表の番号で引く。batch を捨てても残る)
    hgen: BTreeMap<u64, u64>,
    /// 基の鎖をフックに入れた順
    hook_seq: u64,
}

static G: Bkl<Option<Glob>> = Bkl::new(None);

fn g() -> &'static mut Glob {
    G.get().get_or_insert_with(|| Glob { cur: State::default(), base_seq: 1, table_handle: 0, hgen: BTreeMap::new(), hook_seq: 0 })
}

fn supported(family: u8) -> bool {
    matches!(family, NFPROTO_INET | NFPROTO_IPV4 | NFPROTO_IPV6)
}

/// 1 つのメッセージを見るときのもの
struct Cx<'a> {
    st: &'a mut State,
    family: u8,
    flags: u16,
    typ: u16,
    portid: u32,
}

/// フックに入れる順の番号
fn next_seq() -> u64 {
    let gl = g();
    gl.hook_seq += 1;
    gl.hook_seq
}

/// nf_tables_alloc_handle
fn alloc_handle(table_handle: u64) -> u64 {
    let h = g().hgen.entry(table_handle).or_insert(0);
    *h += 1;
    *h
}

// ---- 受け口 ----

/// NETLINK_NETFILTER のソケットに送られたデータグラム (netlink.rs から)。答えのデータグラムを返す (ACK とエラーは
/// 1 つずつ別のもの、dump は中身と NLMSG_DONE)
pub fn rcv(data: &[u8], me: u32, cap_ack: bool, ext_ack: bool) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    if data.len() < NLMSG_HDRLEN {
        return out;
    }
    let len = u32::from_le_bytes(data[0..4].try_into().unwrap()) as usize;
    if len < NLMSG_HDRLEN || data.len() < len {
        return out;
    }
    let ack = |out: &mut Vec<Vec<u8>>, m: &[u8], f: Option<Fail>| out.push(crate::netlink::ack(m, f.map_or(0, |f| f.err), cap_ack, ext_ack, f.and_then(|f| f.off), f.and_then(|f| f.msg), me));
    // nfnetlink_rcv: ぜんぶ CAP_NET_ADMIN
    if !crate::proc::current().cred.capable(crate::cred::CAP_NET_ADMIN) {
        ack(&mut out, &data[..len], Some(fail(EPERM)));
        return out;
    }
    let typ = u16::from_le_bytes([data[4], data[5]]);
    if typ == NFNL_MSG_BATCH_BEGIN {
        batch(data, me, &mut out, &ack);
        return out;
    }
    // netlink_rcv_skb と nfnetlink_rcv_msg: 1 つずつ (GET だけ)
    let mut at = 0;
    while data.len() - at >= NLMSG_HDRLEN {
        let h = &data[at..];
        let len = u32::from_le_bytes(h[0..4].try_into().unwrap()) as usize;
        if len < NLMSG_HDRLEN || h.len() < len {
            break;
        }
        let m = &h[..len];
        let typ = u16::from_le_bytes([m[4], m[5]]);
        let flags = u16::from_le_bytes([m[6], m[7]]);
        let mut r: Result<bool, Fail> = Ok(false);
        if flags & NLM_F_REQUEST != 0 && typ >= NLMSG_MIN_TYPE {
            r = single(m, me, &mut out);
        }
        match r {
            // dump は NLMSG_DONE で終わるので ACK はない
            Ok(true) => {}
            Ok(false) => {
                if flags & NLM_F_ACK != 0 {
                    ack(&mut out, m, None);
                }
            }
            Err(f) => ack(&mut out, m, Some(f)),
        }
        at += align4(len).min(data.len() - at);
    }
    out
}

/// batch の外の 1 つ。Ok(true) は dump した
fn single(m: &[u8], me: u32, out: &mut Vec<Vec<u8>>) -> Result<bool, Fail> {
    // nfgenmsg より短いものは答えない
    if m.len() < NLMSG_HDRLEN + 4 {
        return Ok(false);
    }
    let typ = u16::from_le_bytes([m[4], m[5]]);
    if typ >> 8 != NFNL_SUBSYS_NFTABLES {
        return Err(fail(EINVAL));
    }
    let t = typ & 0xff;
    let flags = u16::from_le_bytes([m[6], m[7]]);
    let dump = flags & NLM_F_DUMP == NLM_F_DUMP;
    match t {
        GETTABLE | GETCHAIN | GETRULE | GETRULE_RESET | GETSET | GETSETELEM | GETSETELEM_RESET | GETGEN | GETOBJ | GETOBJ_RESET | GETFLOWTABLE => {
            get(m, t, dump, me, out)?;
            Ok(dump && t != GETGEN)
        }
        // 変えるものは batch の中だけ (nc->type が NFNL_CB_BATCH)
        _ if t < MSG_MAX => Err(fail(EINVAL)),
        _ => Err(fail(EINVAL)),
    }
}

type AckFn<'a> = dyn Fn(&mut Vec<Vec<u8>>, &[u8], Option<Fail>) + 'a;

/// nfnetlink_rcv_skb_batch と nfnetlink_rcv_batch
fn batch(data: &[u8], me: u32, out: &mut Vec<Vec<u8>>, ack: &AckFn) {
    let blen = u32::from_le_bytes(data[0..4].try_into().unwrap()) as usize;
    let begin = &data[..blen];
    if data.len() < NLMSG_HDRLEN + 4 {
        return;
    }
    let ba = parse(&begin[(NLMSG_HDRLEN + 4).min(begin.len())..], NLMSG_HDRLEN + 4, NFNL_BATCH_GENID);
    let genid = ba[NFNL_BATCH_GENID as usize].filter(|a| a.d.len() >= 4).map_or(0, |a| a.be32());
    let raw = u16::from_le_bytes([begin[NLMSG_HDRLEN + 2], begin[NLMSG_HDRLEN + 3]]);
    // 古い nft は res_id を CPU の順で書く
    let res_id = if raw == NFNL_SUBSYS_NFTABLES { raw } else { u16::from_be(raw) };
    if res_id >= NFNL_SUBSYS_COUNT {
        ack(out, begin, Some(fail(EINVAL)));
        return;
    }
    if res_id != NFNL_SUBSYS_NFTABLES {
        ack(out, begin, Some(fail(EOPNOTSUPP)));
        return;
    }
    if genid != 0 && genid != g().base_seq {
        ack(out, begin, Some(fail(ERESTART)));
        return;
    }
    let bflags = u16::from_le_bytes([begin[6], begin[7]]);
    // commit のたしかめ (validate) でしくじれば、Linux と同じく batch をはじめからやりなおす (NFNL_BATCH_REPLAY):
    // こんどは決まりと要素を足すたびにその表をたしかめ、しくじったものがそのエラーを返す (足したものは残るので、
    // 同じ表のあとのものもしくじる)。番号 (handle) は 1 度目のぶんも進んだまま
    let mut replay = false;
    loop {
        let mut errs: Vec<(usize, usize, Option<Fail>)> = Vec::new();
        if bflags & NLM_F_ACK != 0 {
            errs.push((0, blen, None));
        }
        let mut st = g().cur.clone();
        let (mut failed, mut done) = (false, false);
        let mut at = align4(blen).min(data.len());
        let mut last = (0, blen);
        while data.len() - at >= NLMSG_HDRLEN {
            let h = &data[at..];
            let len = u32::from_le_bytes(h[0..4].try_into().unwrap()) as usize;
            if len < NLMSG_HDRLEN || h.len() < len || len < NLMSG_HDRLEN + 4 {
                errs.clear();
                failed = true;
                break;
            }
            let m = &h[..len];
            last = (at, len);
            let typ = u16::from_le_bytes([m[4], m[5]]);
            let flags = u16::from_le_bytes([m[6], m[7]]);
            let r: Res<()> = if flags & NLM_F_REQUEST == 0 {
                Err(fail(EINVAL))
            } else if typ == NFNL_MSG_BATCH_BEGIN {
                errs.clear();
                failed = true;
                break;
            } else if typ == NFNL_MSG_BATCH_END {
                done = true;
                break;
            } else if typ < NLMSG_MIN_TYPE || typ >> 8 != NFNL_SUBSYS_NFTABLES {
                Err(fail(EINVAL))
            } else {
                let mut cx = Cx { st: &mut st, family: m[NLMSG_HDRLEN], flags, typ: typ & 0xff, portid: me };
                change(&mut cx, m).and_then(|()| {
                    if replay && matches!(typ & 0xff, NEWRULE | NEWSETELEM) {
                        // nf_tables_newrule / nf_tables_newsetelem の NFT_VALIDATE_DO
                        let a = parse(&m[NLMSG_HDRLEN + 4..], NLMSG_HDRLEN + 4, 1);
                        let fam = m[NLMSG_HDRLEN];
                        if let Some(n) = a[1]
                            && let Some(t) = st.tables.iter().find(|t| t.family == fam && t.name.as_bytes() == n.s())
                        {
                            table_validate(t).map_err(fail)?;
                        }
                    }
                    Ok(())
                })
            };
            if flags & NLM_F_ACK != 0 || r.is_err() {
                if r.is_err() {
                    failed = true;
                }
                errs.push((at, len, r.err()));
            }
            at += align4(len).min(data.len() - at);
        }
        if done && !failed {
            // commit
            match validate(&st) {
                Err(_) if !replay => {
                    replay = true;
                    continue;
                }
                Err(e) => errs.push((0, blen, Some(fail(e)))),
                Ok(()) => {
                    g().cur = st;
                    let gl = g();
                    gl.base_seq = gl.base_seq.wrapping_add(1).max(1);
                    refresh_active();
                    let (lat, llen) = last;
                    if u16::from_le_bytes([data[lat + 6], data[lat + 7]]) & NLM_F_ACK != 0 {
                        errs.push((lat, llen, None));
                    }
                }
            }
        }
        for (at, len, f) in errs {
            ack(out, &data[at..at + len], f);
        }
        return;
    }
}

/// commit の前のたしかめ (nf_tables_validate → nft_table_validate)
fn validate(st: &State) -> Result<(), i64> {
    st.tables.iter().try_for_each(table_validate)
}

/// 表を基の鎖からたどってたしかめる (nft_chain_validate): 16 段より深い jump は EMLINK、置けないフックの式は
/// EOPNOTSUPP (reject、rt、meta の pkttype を変えるもの、sdif)。基の鎖から届かない鎖は見ない (輪があってもよい)
fn table_validate(t: &Table) -> Result<(), i64> {
    for c in &t.chains {
        if let Some(b) = &c.base {
            chain_validate(t, c, b.hooknum, 0)?;
        }
    }
    Ok(())
}

fn chain_validate(t: &Table, c: &Chain, hook: u32, level: usize) -> Result<(), i64> {
    if level == NFT_JUMP_STACK_SIZE {
        return Err(EMLINK);
    }
    let hooks = |mask: u32| if mask & (1 << hook) != 0 { Ok(()) } else { Err(EOPNOTSUPP) };
    for r in &c.rules {
        for e in &r.exprs {
            match &e.op {
                // nft_reject_validate (ip と ip6) と nft_reject_inet_validate
                Op::Reject { .. } => hooks(if t.family == NFPROTO_INET { 0b10_1111 } else { 0b1110 })?,
                // nft_rt_validate: FORWARD、LOCAL_OUT、POST_ROUTING
                Op::Rt { .. } => hooks(0b1_1100)?,
                // nft_meta_set_validate: pkttype は PRE_ROUTING だけ
                Op::MetaSet(_, 19) => hooks(0b1)?,
                // nft_meta_get_validate_sdif: LOCAL_IN と FORWARD
                Op::Meta(_, 33 | 34) => hooks(0b110)?,
                Op::Verdict(NFT_JUMP | NFT_GOTO, Some(n)) => {
                    if let Some(x) = t.chains.iter().find(|x| &x.name == n) {
                        chain_validate(t, x, hook, level + 1)?;
                    }
                }
                Op::Lookup { dreg: Some(0), .. } => {
                    if let Some(s) = e.set.as_ref().and_then(|s| t.sets.iter().find(|x| &x.name == s)) {
                        for el in &s.elems {
                            if let Some(Data::Verdict(NFT_JUMP | NFT_GOTO, Some(n))) = &el.data
                                && let Some(x) = t.chains.iter().find(|x| &x.name == n)
                            {
                                chain_validate(t, x, hook, level + 1)?;
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}

// ---- 変える (batch の中) ----

fn change(cx: &mut Cx, m: &[u8]) -> Res<()> {
    let body = &m[NLMSG_HDRLEN + 4..];
    let base = NLMSG_HDRLEN + 4;
    match cx.typ {
        NEWTABLE => newtable(cx, &parse(body, base, 7)),
        DELTABLE | DESTROYTABLE => deltable(cx, &parse(body, base, 7)),
        NEWCHAIN => newchain(cx, &parse(body, base, 12)),
        DELCHAIN | DESTROYCHAIN => delchain(cx, &parse(body, base, 12)),
        NEWRULE => newrule(cx, &parse(body, base, 11)),
        DELRULE | DESTROYRULE => delrule(cx, &parse(body, base, 11)),
        NEWSET => newset(cx, &parse(body, base, 20)),
        DELSET | DESTROYSET => delset(cx, &parse(body, base, 20)),
        NEWSETELEM => newsetelem(cx, &parse(body, base, 4)),
        DELSETELEM | DESTROYSETELEM => delsetelem(cx, &parse(body, base, 4)),
        // 名前のついたもの (counter、quota などの object) と flowtable はまだ
        NEWOBJ | DELOBJ | DESTROYOBJ | NEWFLOWTABLE | DELFLOWTABLE | DESTROYFLOWTABLE => Err(fail(EOPNOTSUPP)),
        // 見るものは batch の中では使えない
        _ => Err(fail(EINVAL)),
    }
}

const TABLE_POL: &[(u16, P)] = &[(NFTA_TABLE_NAME, P::Str(NAME_MAX)), (NFTA_TABLE_FLAGS, P::U32), (NFTA_TABLE_HANDLE, P::U64), (NFTA_TABLE_USERDATA, P::Bin(USERDATA_MAX))];

/// 名前のもの (nla_strcmp)
fn is(a: A, name: &str) -> bool {
    a.s() == name.as_bytes()
}

/// nft_table_lookup: (表の場所)。名前がなければ EINVAL、持ち主のものでなければ EPERM
fn table_of(st: &State, a: Option<A>, family: u8, portid: u32) -> Result<usize, i64> {
    let a = a.ok_or(EINVAL)?;
    let i = st.tables.iter().position(|t| t.family == family && is(a, &t.name)).ok_or(ENOENT)?;
    let t = &st.tables[i];
    if t.flags & NFT_TABLE_F_OWNER != 0 && portid != 0 && t.owner != portid {
        return Err(EPERM);
    }
    Ok(i)
}

fn newtable(cx: &mut Cx, t: &[Option<A>]) -> Res<()> {
    if !supported(cx.family) {
        return Err(fail(EOPNOTSUPP));
    }
    check(t, TABLE_POL)?;
    let attr = t[NFTA_TABLE_NAME as usize];
    match table_of(cx.st, attr, cx.family, cx.portid) {
        Ok(i) => {
            if cx.flags & NLM_F_EXCL != 0 {
                return Err(bad(EEXIST, attr));
            }
            if cx.flags & NLM_F_REPLACE != 0 {
                return Err(fail(EOPNOTSUPP));
            }
            // nf_tables_updtable
            let Some(f) = t[NFTA_TABLE_FLAGS as usize] else { return Ok(()) };
            let flags = f.be32();
            if flags & !NFT_TABLE_F_MASK != 0 {
                return Err(fail(EOPNOTSUPP));
            }
            let tb = &mut cx.st.tables[i];
            if flags == tb.flags & NFT_TABLE_F_MASK {
                return Ok(());
            }
            let owned = tb.flags & NFT_TABLE_F_OWNER != 0;
            if (owned && flags & NFT_TABLE_F_OWNER == 0) || (flags & NFT_TABLE_F_OWNER != 0 && owned && tb.owner != 0) {
                return Err(fail(EOPNOTSUPP));
            }
            if (flags ^ tb.flags) & NFT_TABLE_F_PERSIST != 0 {
                return Err(fail(EOPNOTSUPP));
            }
            // 起こした表の基の鎖は、フックに入れなおす (同じ priority のものの後ろへ)
            if tb.flags & NFT_TABLE_F_DORMANT != 0 && flags & NFT_TABLE_F_DORMANT == 0 {
                let n = tb.chains.iter().filter(|c| c.base.is_some()).count() as u64;
                let gl = g();
                let first = gl.hook_seq + 1;
                gl.hook_seq += n;
                let tb = &mut cx.st.tables[i];
                for (k, b) in tb.chains.iter_mut().filter_map(|c| c.base.as_mut()).enumerate() {
                    b.seq = first + k as u64;
                }
            }
            let tb = &mut cx.st.tables[i];
            tb.flags = (tb.flags & !NFT_TABLE_F_DORMANT) | (flags & NFT_TABLE_F_DORMANT);
            if flags & NFT_TABLE_F_OWNER != 0 && !owned {
                tb.owner = cx.portid;
                tb.flags |= NFT_TABLE_F_OWNER;
            }
            Ok(())
        }
        Err(ENOENT) => {
            let attr = attr.ok_or(fail(EINVAL))?;
            let flags = t[NFTA_TABLE_FLAGS as usize].map_or(0, |f| f.be32());
            if flags & !NFT_TABLE_F_MASK != 0 {
                return Err(fail(EOPNOTSUPP));
            }
            let gl = g();
            gl.table_handle += 1;
            let handle = gl.table_handle;
            cx.st.tables.push(Table {
                family: cx.family,
                name: attr.string(),
                handle,
                flags,
                udata: t[NFTA_TABLE_USERDATA as usize].map(|a| a.d.to_vec()),
                owner: if flags & NFT_TABLE_F_OWNER != 0 { cx.portid } else { 0 },
                chains: Vec::new(),
                sets: Vec::new(),
            });
            Ok(())
        }
        Err(e) => Err(fail(e)),
    }
}

/// nf_tables_deltable と nft_flush: family が AF_UNSPEC か名前も番号もなければ、当たる表をみな
fn deltable(cx: &mut Cx, t: &[Option<A>]) -> Res<()> {
    check(t, TABLE_POL)?;
    let name = t[NFTA_TABLE_NAME as usize];
    if cx.family == NFPROTO_UNSPEC || (name.is_none() && t[NFTA_TABLE_HANDLE as usize].is_none()) {
        let fam = cx.family;
        let portid = cx.portid;
        cx.st.tables.retain(|tb| {
            let hit = (fam == NFPROTO_UNSPEC || tb.family == fam)
                && !(tb.flags & NFT_TABLE_F_OWNER != 0 && tb.owner != portid)
                && name.is_none_or(|n| is(n, &tb.name));
            !hit
        });
        return Ok(());
    }
    let (attr, idx) = if let Some(h) = t[NFTA_TABLE_HANDLE as usize] {
        let hd = h.be64();
        let r = cx.st.tables.iter().position(|tb| tb.family == cx.family && tb.handle == hd).ok_or(ENOENT).and_then(|i| {
            let tb = &cx.st.tables[i];
            if tb.flags & NFT_TABLE_F_OWNER != 0 && tb.owner != cx.portid { Err(EPERM) } else { Ok(i) }
        });
        (Some(h), r)
    } else {
        (name, table_of(cx.st, name, cx.family, cx.portid))
    };
    let i = match idx {
        Ok(i) => i,
        Err(ENOENT) if cx.typ == DESTROYTABLE => return Ok(()),
        Err(e) => return Err(bad(e, attr)),
    };
    let tb = &cx.st.tables[i];
    if cx.flags & NLM_F_NONREC != 0 && tb.chains.len() + tb.sets.len() > 0 {
        return Err(fail(EBUSY));
    }
    cx.st.tables.remove(i);
    Ok(())
}

const CHAIN_POL: &[(u16, P)] = &[
    (NFTA_CHAIN_TABLE, P::Str(NAME_MAX)),
    (NFTA_CHAIN_HANDLE, P::U64),
    (NFTA_CHAIN_NAME, P::Str(NAME_MAX)),
    (NFTA_CHAIN_HOOK, P::Nest),
    (NFTA_CHAIN_POLICY, P::U32),
    (NFTA_CHAIN_TYPE, P::Str(16)),
    (NFTA_CHAIN_COUNTERS, P::Nest),
    (NFTA_CHAIN_FLAGS, P::U32),
    (NFTA_CHAIN_ID, P::U32),
    (NFTA_CHAIN_USERDATA, P::Bin(USERDATA_MAX)),
];

/// 鎖の型が使えるフック (nft_chain_filter.c と nft_chain_route.c。nat は手本の Linux に入れていない)
fn chain_type(name: &[u8], family: u8) -> Option<(&'static str, u32)> {
    // PRE_ROUTING 0, LOCAL_IN 1, FORWARD 2, LOCAL_OUT 3, POST_ROUTING 4, INGRESS 5
    let all = 0b1_1111 | if family == NFPROTO_INET { 1 << 5 } else { 0 };
    match name {
        b"filter" => Some(("filter", all)),
        b"route" => Some(("route", 1 << 3)),
        _ => None,
    }
}

fn chain_of(tb: &Table, a: A) -> Option<usize> {
    tb.chains.iter().position(|c| is(a, &c.name))
}

fn newchain(cx: &mut Cx, t: &[Option<A>]) -> Res<()> {
    check(t, CHAIN_POL)?;
    let ti = table_of(cx.st, t[NFTA_CHAIN_TABLE as usize], cx.family, cx.portid).map_err(|e| bad(e, t[NFTA_CHAIN_TABLE as usize]))?;
    let mut attr = t[NFTA_CHAIN_NAME as usize];
    let mut ci = None;
    if let Some(h) = t[NFTA_CHAIN_HANDLE as usize] {
        let hd = h.be64();
        ci = Some(cx.st.tables[ti].chains.iter().position(|c| c.handle == hd).ok_or(bad(ENOENT, Some(h)))?);
        attr = Some(h);
    } else if let Some(n) = attr {
        ci = chain_of(&cx.st.tables[ti], n);
    } else if t[NFTA_CHAIN_ID as usize].is_none() {
        return Err(fail(EINVAL));
    }
    let mut policy = NF_ACCEPT as u32;
    if let Some(p) = t[NFTA_CHAIN_POLICY as usize] {
        if ci.is_some_and(|i| cx.st.tables[ti].chains[i].base.is_none()) || (ci.is_none() && t[NFTA_CHAIN_HOOK as usize].is_none()) {
            return Err(bad(EOPNOTSUPP, Some(p)));
        }
        policy = p.be32();
        if policy != NF_DROP as u32 && policy != NF_ACCEPT as u32 {
            return Err(fail(EINVAL));
        }
    }
    let mut flags = match (t[NFTA_CHAIN_FLAGS as usize], ci) {
        (Some(f), _) => f.be32(),
        (None, Some(i)) => cx.st.tables[ti].chains[i].flags,
        _ => 0,
    };
    if flags & !(NFT_CHAIN_BASE | NFT_CHAIN_HW_OFFLOAD | NFT_CHAIN_BINDING) != 0 {
        return Err(fail(EOPNOTSUPP));
    }
    if let Some(i) = ci {
        // nf_tables_updchain: 基の鎖の policy だけ (フックは変えられない)
        let c = &cx.st.tables[ti].chains[i];
        if c.flags & NFT_CHAIN_BINDING != 0 {
            return Err(fail(EINVAL));
        }
        if cx.flags & NLM_F_EXCL != 0 {
            return Err(bad(EEXIST, attr));
        }
        if cx.flags & NLM_F_REPLACE != 0 {
            return Err(fail(EOPNOTSUPP));
        }
        flags |= c.flags & NFT_CHAIN_BASE;
        if c.flags ^ flags != 0 {
            return Err(fail(EOPNOTSUPP));
        }
        if let Some(h) = t[NFTA_CHAIN_HOOK as usize] {
            let Some(b) = &c.base else { return Err(bad(EEXIST, attr)) };
            let ha = h.nested(NFTA_HOOK_DEVS);
            if ha[NFTA_HOOK_HOOKNUM as usize].is_some_and(|x| x.be32() != b.hooknum) || ha[NFTA_HOOK_PRIORITY as usize].is_some_and(|x| x.be32() as i32 != b.prio) {
                return Err(fail(EOPNOTSUPP));
            }
            if let Some(ty) = t[NFTA_CHAIN_TYPE as usize]
                && chain_type(ty.s(), cx.family).is_none()
            {
                return Err(bad(ENOENT, Some(ty)));
            }
        }
        let c = &mut cx.st.tables[ti].chains[i];
        if let (Some(b), Some(_)) = (c.base.as_mut(), t[NFTA_CHAIN_POLICY as usize]) {
            b.policy = policy;
        }
        if let Some(u) = t[NFTA_CHAIN_USERDATA as usize] {
            c.udata = Some(u.d.to_vec());
        }
        return Ok(());
    }
    // nf_tables_addchain
    let mut base = None;
    if let Some(h) = t[NFTA_CHAIN_HOOK as usize] {
        if flags & NFT_CHAIN_BINDING != 0 {
            return Err(fail(EOPNOTSUPP));
        }
        let ha = h.nested(NFTA_HOOK_DEVS);
        let (Some(num), Some(prio)) = (ha[NFTA_HOOK_HOOKNUM as usize], ha[NFTA_HOOK_PRIORITY as usize]) else {
            return Err(bad(ENOENT, t[NFTA_CHAIN_NAME as usize]));
        };
        let (typ, mask) = match t[NFTA_CHAIN_TYPE as usize] {
            Some(ty) => chain_type(ty.s(), cx.family).ok_or(bad(ENOENT, Some(ty)))?,
            None => chain_type(b"filter", cx.family).ok_or(fail(EOPNOTSUPP))?,
        };
        let num = num.be32();
        if num >= 6 || mask & (1 << num) == 0 {
            return Err(fail(EOPNOTSUPP));
        }
        // inet の ingress は口 (dev) が要る: まだ口のフックはない
        if num == 5 {
            return Err(fail(EOPNOTSUPP));
        }
        flags |= NFT_CHAIN_BASE;
        base = Some(Base { hooknum: num, prio: prio.be32() as i32, policy, typ, seq: next_seq() });
    } else {
        if flags & NFT_CHAIN_BASE != 0 {
            return Err(fail(EINVAL));
        }
        if flags & NFT_CHAIN_HW_OFFLOAD != 0 {
            return Err(fail(EOPNOTSUPP));
        }
    }
    let th = cx.st.tables[ti].handle;
    let handle = alloc_handle(th);
    let name = match t[NFTA_CHAIN_NAME as usize] {
        Some(n) => n.string(),
        None if flags & NFT_CHAIN_BINDING != 0 => alloc::format!("__chain{}", handle),
        None => return Err(fail(EINVAL)),
    };
    let id = t[NFTA_CHAIN_ID as usize].map_or(0, |a| a.be32());
    cx.st.tables[ti].chains.push(Chain { name, handle, base, flags, udata: t[NFTA_CHAIN_USERDATA as usize].map(|a| a.d.to_vec()), rules: Vec::new(), id, seen: 0, to_policy: 0 });
    Ok(())
}

/// 鎖を指すもの (決まりの jump / goto と、集まりの判定) の数
fn chain_refs(tb: &Table, name: &str) -> u32 {
    let mut n = 0;
    for c in &tb.chains {
        for r in &c.rules {
            n += r.exprs.iter().filter(|e| e.chain.as_deref() == Some(name)).count() as u32;
        }
    }
    for s in &tb.sets {
        n += s.elems.iter().filter(|e| matches!(&e.data, Some(Data::Verdict(_, Some(c))) if c == name)).count() as u32;
    }
    n
}

fn delchain(cx: &mut Cx, t: &[Option<A>]) -> Res<()> {
    check(t, CHAIN_POL)?;
    let ti = table_of(cx.st, t[NFTA_CHAIN_TABLE as usize], cx.family, cx.portid).map_err(|e| bad(e, t[NFTA_CHAIN_TABLE as usize]))?;
    let (attr, ci) = if let Some(h) = t[NFTA_CHAIN_HANDLE as usize] {
        let hd = h.be64();
        (Some(h), cx.st.tables[ti].chains.iter().position(|c| c.handle == hd))
    } else {
        let a = t[NFTA_CHAIN_NAME as usize];
        (a, a.and_then(|a| chain_of(&cx.st.tables[ti], a)))
    };
    let Some(ci) = ci else {
        if cx.typ == DESTROYCHAIN {
            return Ok(());
        }
        return Err(bad(if attr.is_none() { EINVAL } else { ENOENT }, attr));
    };
    let tb = &cx.st.tables[ti];
    let c = &tb.chains[ci];
    if c.flags & NFT_CHAIN_BINDING != 0 {
        return Err(fail(EOPNOTSUPP));
    }
    if t[NFTA_CHAIN_HOOK as usize].is_some() && cx.typ == DESTROYCHAIN {
        return Err(fail(EOPNOTSUPP));
    }
    let refs = chain_refs(tb, &c.name);
    // use は決まりの数 + 指すもの
    if cx.flags & NLM_F_NONREC != 0 && c.rules.len() as u32 + refs > 0 {
        return Err(fail(EBUSY));
    }
    // 決まりを消してもまだ指すものがあれば消せない (自分の決まりから自分を指すものは消える)
    let own = c.rules.iter().map(|r| r.exprs.iter().filter(|e| e.chain.as_deref() == Some(c.name.as_str())).count() as u32).sum::<u32>();
    if refs - own > 0 {
        return Err(bad(EBUSY, attr));
    }
    let rules: Vec<Rule> = cx.st.tables[ti].chains[ci].rules.drain(..).collect();
    for r in &rules {
        unbind_sets(&mut cx.st.tables[ti], r);
    }
    cx.st.tables[ti].chains.remove(ci);
    Ok(())
}

// ---- 決まり (rule) と式 ----

const RULE_POL: &[(u16, P)] = &[
    (NFTA_RULE_TABLE, P::Str(NAME_MAX)),
    (NFTA_RULE_CHAIN, P::Str(NAME_MAX)),
    (NFTA_RULE_HANDLE, P::U64),
    (NFTA_RULE_EXPRESSIONS, P::Nest),
    (NFTA_RULE_COMPAT, P::Nest),
    (NFTA_RULE_POSITION, P::U64),
    (NFTA_RULE_USERDATA, P::Bin(USERDATA_MAX)),
    (NFTA_RULE_ID, P::U32),
    (NFTA_RULE_POSITION_ID, P::U32),
    (NFTA_RULE_CHAIN_ID, P::U32),
];

/// 鎖の場所 (名前か、この batch の NFTA_CHAIN_ID)
fn chain_lookup(tb: &Table, name: Option<A>, id: Option<A>) -> Result<usize, (i64, Option<usize>)> {
    if let Some(n) = name {
        return chain_of(tb, n).ok_or((ENOENT, Some(n.off)));
    }
    if let Some(i) = id {
        let v = i.be32();
        return tb.chains.iter().position(|c| c.id != 0 && c.id == v).ok_or((ENOENT, Some(i.off)));
    }
    Err((EINVAL, None))
}

fn newrule(cx: &mut Cx, t: &[Option<A>]) -> Res<()> {
    check(t, RULE_POL)?;
    let ti = table_of(cx.st, t[NFTA_RULE_TABLE as usize], cx.family, cx.portid).map_err(|e| bad(e, t[NFTA_RULE_TABLE as usize]))?;
    let ci = chain_lookup(&cx.st.tables[ti], t[NFTA_RULE_CHAIN as usize], t[NFTA_RULE_CHAIN_ID as usize]).map_err(|(e, off)| Fail { err: -e, off, msg: None })?;
    let th = cx.st.tables[ti].handle;
    // 置きかえるもの、前か後ろに入れる相手
    let mut old: Option<usize> = None;
    let handle;
    {
        let c = &cx.st.tables[ti].chains[ci];
        if let Some(h) = t[NFTA_RULE_HANDLE as usize] {
            let hd = h.be64();
            let i = c.rules.iter().position(|r| r.handle == hd).ok_or(bad(ENOENT, Some(h)))?;
            if cx.flags & NLM_F_EXCL != 0 {
                return Err(bad(EEXIST, Some(h)));
            }
            if cx.flags & NLM_F_REPLACE == 0 {
                return Err(fail(EOPNOTSUPP));
            }
            old = Some(i);
            handle = hd;
        } else {
            if cx.flags & NLM_F_CREATE == 0 || cx.flags & NLM_F_REPLACE != 0 {
                return Err(fail(EINVAL));
            }
            handle = alloc_handle(th);
            if let Some(p) = t[NFTA_RULE_POSITION as usize] {
                let ph = p.be64();
                old = Some(c.rules.iter().position(|r| r.handle == ph).ok_or(bad(ENOENT, Some(p)))?);
            } else if let Some(p) = t[NFTA_RULE_POSITION_ID as usize] {
                let id = p.be32();
                old = Some(c.rules.iter().position(|r| r.id != 0 && r.id == id).ok_or(bad(ENOENT, Some(p)))?);
            }
        }
    }
    // 式: まず名前をみな見て (nf_tables_expr_parse)、それから 1 つずつ作る (nf_tables_newexpr)。しくじりはその式を指す
    let mut list = Vec::new();
    if let Some(ex) = t[NFTA_RULE_EXPRESSIONS as usize] {
        for (ty, e) in ex.list() {
            if ty != NFTA_LIST_ELEM || list.len() == NFT_RULE_MAXEXPRS {
                return Err(fail(EINVAL));
            }
            let ea = e.nested(NFTA_EXPR_DATA);
            let Some(name) = ea[NFTA_EXPR_NAME as usize].and_then(|n| expr_name(n.s())) else {
                return Err(bad(if ea[NFTA_EXPR_NAME as usize].is_none() { EINVAL } else { ENOENT }, Some(e)));
            };
            list.push((name, e, ea[NFTA_EXPR_DATA as usize]));
        }
    }
    let mut exprs = Vec::new();
    for (name, e, data) in list {
        let x = expr_new(&mut cx.st.tables[ti], cx.family, name, data).map_err(|err| bad(err, Some(e)))?;
        exprs.push(x);
    }
    let rule = Rule { handle, exprs, udata: t[NFTA_RULE_USERDATA as usize].map(|a| a.d.to_vec()), id: t[NFTA_RULE_ID as usize].map_or(0, |a| a.be32()), hits: 0, last: 0 };
    let c = &mut cx.st.tables[ti].chains[ci];
    if cx.flags & NLM_F_REPLACE != 0 {
        let i = old.unwrap_or(0);
        let gone = core::mem::replace(&mut c.rules[i], rule);
        unbind_sets(&mut cx.st.tables[ti], &gone);
    } else if cx.flags & NLM_F_APPEND != 0 {
        // うしろに (相手があればそのうしろ)
        match old {
            Some(i) => c.rules.insert(i + 1, rule),
            None => c.rules.push(rule),
        }
    } else {
        // 前に (相手があればその前)
        c.rules.insert(old.unwrap_or(0), rule);
    }
    Ok(())
}

/// 決まりを消すとき: lookup で使っていた集まりの数を減らし、名前のないものは使うものがなくなれば消す
fn unbind_sets(tb: &mut Table, r: &Rule) {
    for e in &r.exprs {
        let Some(name) = &e.set else { continue };
        if let Some(i) = tb.sets.iter().position(|s| &s.name == name) {
            let s = &mut tb.sets[i];
            s.bound = s.bound.saturating_sub(1);
            if s.bound == 0 && s.flags & NFT_SET_ANONYMOUS != 0 {
                tb.sets.remove(i);
            }
        }
    }
}

fn delrule(cx: &mut Cx, t: &[Option<A>]) -> Res<()> {
    check(t, RULE_POL)?;
    let ti = table_of(cx.st, t[NFTA_RULE_TABLE as usize], cx.family, cx.portid).map_err(|e| bad(e, t[NFTA_RULE_TABLE as usize]))?;
    let chains: Vec<usize> = if let Some(n) = t[NFTA_RULE_CHAIN as usize] {
        match chain_of(&cx.st.tables[ti], n) {
            Some(ci) => {
                if cx.st.tables[ti].chains[ci].flags & NFT_CHAIN_BINDING != 0 {
                    return Err(fail(EOPNOTSUPP));
                }
                vec![ci]
            }
            None if cx.typ == DESTROYRULE => return Ok(()),
            None => return Err(bad(ENOENT, Some(n))),
        }
    } else {
        (0..cx.st.tables[ti].chains.len()).filter(|&i| cx.st.tables[ti].chains[i].flags & NFT_CHAIN_BINDING == 0).collect()
    };
    if t[NFTA_RULE_CHAIN as usize].is_some() {
        let ci = chains[0];
        let c = &cx.st.tables[ti].chains[ci];
        let one = if let Some(h) = t[NFTA_RULE_HANDLE as usize] {
            let hd = h.be64();
            match c.rules.iter().position(|r| r.handle == hd) {
                Some(i) => Some(i),
                None if cx.typ == DESTROYRULE => return Ok(()),
                None => return Err(bad(ENOENT, Some(h))),
            }
        } else if let Some(id) = t[NFTA_RULE_ID as usize] {
            let v = id.be32();
            Some(c.rules.iter().position(|r| r.id != 0 && r.id == v).ok_or(bad(ENOENT, Some(id)))?)
        } else {
            None
        };
        if let Some(i) = one {
            let r = cx.st.tables[ti].chains[ci].rules.remove(i);
            unbind_sets(&mut cx.st.tables[ti], &r);
            return Ok(());
        }
    }
    for ci in chains {
        let rules: Vec<Rule> = cx.st.tables[ti].chains[ci].rules.drain(..).collect();
        for r in &rules {
            unbind_sets(&mut cx.st.tables[ti], r);
        }
    }
    Ok(())
}

/// 使える式の名前 (手本の Linux の核のものと、入れたもの: log limit reject quota)
fn expr_name(n: &[u8]) -> Option<&'static str> {
    const NAMES: [&str; 18] = ["immediate", "cmp", "payload", "meta", "bitwise", "byteorder", "lookup", "range", "counter", "log", "limit", "reject", "quota", "exthdr", "rt", "last", "ct", "notrack"];
    NAMES.iter().copied().find(|x| x.as_bytes() == n)
}

/// レジスタ: 32 ビットの番号に (nft_parse_register: NFT_REG_1..4 は 4 倍、NFT_REG32_00.. は 4 から)
fn reg(a: Option<A>) -> Result<u32, i64> {
    let a = a.ok_or(EINVAL)?;
    if a.d.len() < 4 {
        return Err(ERANGE);
    }
    match a.be32() {
        r @ 0..=4 => Ok(r * 4),
        r @ 8..=23 => Ok(r - 8 + 4),
        _ => Err(ERANGE),
    }
}

/// nft_dump_register: 4 の倍数は NFT_REG_1..4 の番号で
fn dump_reg(r: u32) -> u32 {
    if r.is_multiple_of(4) { r / 4 } else { r - 4 + 8 }
}

/// 読むレジスタ (nft_parse_register_load): 中身があること (判定のレジスタでない)、はみ出さないこと
fn load(a: Option<A>, len: usize) -> Result<u32, i64> {
    let r = reg(a)?;
    if r < 4 || len == 0 || r as usize * 4 + len > 20 * 4 {
        return Err(ERANGE);
    }
    Ok(r)
}

/// 書くレジスタ (nft_parse_register_store)。判定のレジスタは判定だけ
fn store(a: Option<A>, len: usize, verdict: bool) -> Result<u32, i64> {
    let r = reg(a)?;
    if r == 0 {
        return if verdict { Ok(0) } else { Err(EINVAL) };
    }
    if r < 4 || len == 0 || r as usize * 4 + len > 20 * 4 {
        return Err(ERANGE);
    }
    Ok(r)
}

/// nft_data_init: NFTA_DATA_VALUE (1..64 バイト) か NFTA_DATA_VERDICT
fn data_init(tb: &Table, a: Option<A>, want_verdict: Option<bool>) -> Result<Data, i64> {
    let a = a.ok_or(EINVAL)?;
    let d = a.nested(NFTA_DATA_VERDICT);
    if let Some(v) = d[NFTA_DATA_VALUE as usize] {
        if want_verdict == Some(true) {
            return Err(EINVAL);
        }
        if v.d.is_empty() || v.d.len() > NFT_DATA_VALUE_MAXLEN {
            return Err(ERANGE);
        }
        return Ok(Data::Value(v.d.to_vec()));
    }
    if let Some(v) = d[NFTA_DATA_VERDICT as usize] {
        if want_verdict == Some(false) {
            return Err(EINVAL);
        }
        return verdict_init(tb, v);
    }
    Err(EINVAL)
}

/// nft_verdict_init
fn verdict_init(tb: &Table, v: A) -> Result<Data, i64> {
    let va = v.nested(NFTA_VERDICT_CHAIN_ID);
    let code = va[NFTA_VERDICT_CODE as usize].ok_or(EINVAL)?.be32() as i32;
    match code {
        NF_ACCEPT | NF_DROP | NF_QUEUE | NFT_CONTINUE | NFT_BREAK | NFT_RETURN => Ok(Data::Verdict(code, None)),
        NFT_JUMP | NFT_GOTO => {
            let ci = chain_lookup(tb, va[NFTA_VERDICT_CHAIN as usize], va[NFTA_VERDICT_CHAIN_ID as usize]).map_err(|(e, _)| e)?;
            let c = &tb.chains[ci];
            if c.base.is_some() {
                return Err(EOPNOTSUPP);
            }
            Ok(Data::Verdict(code, Some(c.name.clone())))
        }
        _ => match code as u32 & NF_VERDICT_MASK {
            0 | 1 | 3 => Ok(Data::Verdict(code, None)),
            _ => Err(EINVAL),
        },
    }
}

/// nft_data_dump
fn data_dump(w: &mut W, ty: u16, d: &Data) {
    let n = w.start(ty);
    match d {
        Data::Value(v) => w.attr(NFTA_DATA_VALUE, v),
        Data::Verdict(code, chain) => {
            let m = w.start(NFTA_DATA_VERDICT);
            w.be32(NFTA_VERDICT_CODE, *code as u32);
            if let Some(c) = chain {
                w.str(NFTA_VERDICT_CHAIN, c);
            }
            w.end(m);
        }
    }
    w.end(n);
}

fn value(tb: &Table, a: Option<A>) -> Result<Vec<u8>, i64> {
    match data_init(tb, a, Some(false))? {
        Data::Value(v) => Ok(v),
        _ => Err(EINVAL),
    }
}

/// 式を 1 つ作る (式ごとの init と、Linux の dump と同じ形の中身)
fn expr_new(tb: &mut Table, family: u8, name: &'static str, data: Option<A>) -> Result<Expr, i64> {
    let ea = data.map(|d| d.nested(16)).unwrap_or_else(|| vec![None; 17]);
    let at = |i: u16| ea[i as usize];
    let need = |i: u16| at(i).ok_or(EINVAL);
    let u32of = |i: u16| need(i).map(|a| a.be32());
    let mut w = W(Vec::new());
    let mut set = None;
    let mut chain = None;
    let op;
    match name {
        "immediate" => {
            // NFTA_IMMEDIATE_DREG 1、DATA 2
            need(1)?;
            let d = data_init(tb, need(2).ok(), None)?;
            let dreg = match &d {
                Data::Verdict(_, c) => {
                    chain = c.clone();
                    store(at(1), 16, true)?
                }
                Data::Value(v) => store(at(1), v.len(), false)?,
            };
            if matches!(d, Data::Verdict(..)) && dreg != 0 {
                return Err(EINVAL);
            }
            w.be32(1, dump_reg(dreg));
            data_dump(&mut w, 2, &d);
            op = match d {
                Data::Verdict(code, c) => Op::Verdict(code, c),
                Data::Value(v) => Op::Imm(dreg as u8, v),
            };
        }
        "cmp" => {
            // SREG 1、OP 2、DATA 3
            need(1)?;
            need(2)?;
            let v = value(tb, need(3).ok())?;
            let sreg = load(at(1), v.len())?;
            let cop = u32of(2)?;
            if cop > 5 {
                return Err(EINVAL);
            }
            w.be32(1, dump_reg(sreg));
            w.be32(2, cop);
            data_dump(&mut w, 3, &Data::Value(v.clone()));
            op = Op::Cmp(sreg as u8, cop, v);
        }
        "payload" => {
            // DREG 1、BASE 2、OFFSET 3、LEN 4、SREG 5、CSUM_TYPE 6、CSUM_OFFSET 7、CSUM_FLAGS 8
            let (base, off, len) = (u32of(2)?, u32of(3)?, u32of(4)?);
            if base > 3 {
                return Err(EOPNOTSUPP);
            }
            if at(5).is_some() {
                if at(1).is_some() {
                    return Err(EINVAL);
                }
                let sreg = load(at(5), len as usize)?;
                let ct = at(6).map_or(0, |a| a.be32());
                if ct > 2 {
                    return Err(EOPNOTSUPP);
                }
                let (coff, cflags) = (at(7).map_or(0, |a| a.be32()), at(8).map_or(0, |a| a.be32()));
                w.be32(5, dump_reg(sreg));
                w.be32(2, base);
                w.be32(3, off);
                w.be32(4, len);
                w.be32(6, ct);
                w.be32(7, coff);
                w.be32(8, cflags);
                op = Op::PayloadSet { sreg: sreg as u8, base, off, len, csum: ct, csum_off: coff, l4: cflags & 1 != 0 };
            } else {
                need(1)?;
                if len == 0 || len > 64 {
                    return Err(ERANGE);
                }
                let dreg = store(at(1), len as usize, false)?;
                w.be32(1, dump_reg(dreg));
                w.be32(2, base);
                w.be32(3, off);
                w.be32(4, len);
                op = Op::Payload { dreg: dreg as u8, base, off, len };
            }
        }
        "meta" => {
            // DREG 1、KEY 2、SREG 3
            let key = u32of(2)?;
            if let Some(d) = at(1) {
                if at(3).is_some() {
                    return Err(EINVAL);
                }
                let len = meta_len(key).ok_or(EOPNOTSUPP)?;
                let dreg = store(Some(d), len, false)?;
                w.be32(2, key);
                w.be32(1, dump_reg(dreg));
                op = Op::Meta(dreg as u8, key);
            } else if let Some(s) = at(3) {
                // nft_meta_set_init: mark と priority (4 バイト)、nftrace と pkttype (1 バイト)
                let len = match key {
                    3 | 2 => 4,
                    12 | 19 => 1,
                    _ => return Err(EOPNOTSUPP),
                };
                let sreg = load(Some(s), len)?;
                w.be32(2, key);
                w.be32(3, dump_reg(sreg));
                op = Op::MetaSet(sreg as u8, key);
            } else {
                return Err(EINVAL);
            }
        }
        "bitwise" => {
            // SREG 1、DREG 2、LEN 3、MASK 4、XOR 5、OP 6、DATA 7、SREG2 8
            need(1)?;
            need(2)?;
            let len = u32of(3)?;
            if len == 0 || len > 64 {
                return Err(ERANGE);
            }
            let sreg = load(at(1), len as usize)?;
            let dreg = store(at(2), len as usize, false)?;
            let bop = at(6).map_or(0, |a| a.be32());
            w.be32(1, dump_reg(sreg));
            w.be32(2, dump_reg(dreg));
            w.be32(3, len);
            w.be32(6, bop);
            let (mut mask, mut xor, mut shift, mut sreg2) = (Vec::new(), Vec::new(), 0, 0);
            match bop {
                0 => {
                    let (m, x) = (value(tb, need(4).ok())?, value(tb, need(5).ok())?);
                    if m.len() != len as usize || x.len() != len as usize {
                        return Err(EINVAL);
                    }
                    data_dump(&mut w, 4, &Data::Value(m.clone()));
                    data_dump(&mut w, 5, &Data::Value(x.clone()));
                    (mask, xor) = (m, x);
                }
                1 | 2 => {
                    let d = value(tb, need(7).ok())?;
                    if d.len() != 4 || u32::from_ne_bytes(d[..4].try_into().unwrap()) >= 32 {
                        return Err(EINVAL);
                    }
                    shift = u32::from_ne_bytes(d[..4].try_into().unwrap());
                    data_dump(&mut w, 7, &Data::Value(d));
                }
                3..=5 => {
                    if let Some(s2) = at(8) {
                        let r2 = load(Some(s2), len as usize)?;
                        w.be32(8, dump_reg(r2));
                        sreg2 = r2 as u8;
                    } else {
                        return Err(EINVAL);
                    }
                }
                _ => return Err(EOPNOTSUPP),
            }
            op = Op::Bitwise { sreg: sreg as u8, dreg: dreg as u8, len, op: bop, mask, xor, shift, sreg2 };
        }
        "byteorder" => {
            // SREG 1、DREG 2、OP 3、LEN 4、SIZE 5
            need(1)?;
            need(2)?;
            let (bo, len, size) = (u32of(3)?, u32of(4)?, u32of(5)?);
            if bo > 1 {
                return Err(EINVAL);
            }
            if !matches!(size, 2 | 4 | 8) || len % size != 0 {
                return Err(EINVAL);
            }
            let sreg = load(at(1), len as usize)?;
            let dreg = store(at(2), len as usize, false)?;
            w.be32(1, dump_reg(sreg));
            w.be32(2, dump_reg(dreg));
            w.be32(3, bo);
            w.be32(5, size);
            w.be32(4, len);
            op = Op::Byteorder { sreg: sreg as u8, dreg: dreg as u8, len, size };
        }
        "lookup" => {
            // SET 1、SREG 2、DREG 3、SET_ID 4、FLAGS 5
            need(2)?;
            let si = set_lookup(tb, at(1), at(4)).map_err(|(e, _)| e)?;
            let flags = at(5).map_or(0, |a| a.be32());
            if flags & !1 != 0 {
                return Err(EINVAL);
            }
            let s = &tb.sets[si];
            let sreg = load(at(2), s.klen as usize)?;
            w.str(1, &s.name);
            w.be32(2, dump_reg(sreg));
            let mut ldreg = None;
            if let Some(d) = at(3) {
                if flags & 1 != 0 || s.flags & NFT_SET_MAP == 0 {
                    return Err(EINVAL);
                }
                let dreg = store(Some(d), s.dlen as usize, s.dtype == NFT_DATA_VERDICT)?;
                w.be32(3, dump_reg(dreg));
                ldreg = Some(dreg as u8);
            }
            op = Op::Lookup { sreg: sreg as u8, dreg: ldreg, inv: flags & 1 != 0 };
            w.be32(5, flags);
            set = Some(s.name.clone());
            tb.sets[si].bound += 1;
        }
        "range" => {
            // SREG 1、OP 2、FROM_DATA 3、TO_DATA 4
            need(1)?;
            need(2)?;
            let from = value(tb, need(3).ok())?;
            let to = value(tb, need(4).ok())?;
            if from.len() != to.len() {
                return Err(EINVAL);
            }
            let sreg = load(at(1), from.len())?;
            let rop = u32of(2)?;
            if rop > 1 {
                return Err(EINVAL);
            }
            w.be32(1, dump_reg(sreg));
            w.be32(2, rop);
            data_dump(&mut w, 3, &Data::Value(from.clone()));
            data_dump(&mut w, 4, &Data::Value(to.clone()));
            op = Op::Range { sreg: sreg as u8, op: rop, from, to };
        }
        "counter" => {
            // BYTES 1、PACKETS 2
            let (bytes, packets) = (at(1).map_or(0, |a| a.be64()), at(2).map_or(0, |a| a.be64()));
            w.be64(1, bytes);
            w.be64(2, packets);
            op = Op::Counter { bytes, packets };
        }
        "log" => {
            // GROUP 1、PREFIX 2、SNAPLEN 3、QTHRESHOLD 4、LEVEL 5、FLAGS 6。group (nfnetlink_log) は手本の Linux にない
            if at(5).is_some() && at(1).is_some() {
                return Err(EINVAL);
            }
            if at(1).is_some() {
                return Err(ENOENT);
            }
            let level = at(5).map_or(4, |a| a.be32());
            if level > 8 {
                return Err(EINVAL);
            }
            let lflags = at(6).map_or(0, |a| a.be32());
            if lflags & !0x2f != 0 {
                return Err(EINVAL);
            }
            let mut prefix = String::new();
            if let Some(p) = at(2) {
                if p.s().len() > 127 {
                    return Err(ERANGE);
                }
                prefix = p.string();
                w.str(2, &prefix);
            }
            w.be32(5, level);
            if lflags != 0 {
                w.be32(6, lflags);
            }
            op = Op::Log { prefix, level, flags: lflags };
        }
        "limit" => {
            // RATE 1、UNIT 2、BURST 3、TYPE 4、FLAGS 5
            let (rate, unit) = (need(1)?.be64(), need(2)?.be64());
            if unit == 0 || unit > u64::MAX / 1_000_000_000 {
                return Err(75); // EOVERFLOW
            }
            let typ = at(4).map_or(0, |a| a.be32());
            if typ > 1 {
                return Err(EOPNOTSUPP);
            }
            let mut burst = at(3).map_or(0, |a| a.be32());
            if typ == 0 && burst == 0 {
                burst = 5;
            }
            let flags = at(5).map_or(0, |a| a.be32());
            if flags & !1 != 0 {
                return Err(EOPNOTSUPP);
            }
            if rate == 0 || rate.checked_add(burst as u64).is_none() {
                return Err(75);
            }
            // nft_limit_init: 秒の ns と、ためておける量 (tokens_max)
            let nsecs = unit * 1_000_000_000;
            let max = if typ == 0 {
                (nsecs / rate).checked_mul(burst as u64).ok_or(75)?
            } else {
                ((rate + burst as u64).checked_mul(nsecs).ok_or(75)?) / rate
            };
            w.be64(1, rate);
            w.be64(2, unit);
            w.be32(3, burst);
            w.be32(4, typ);
            w.be32(5, flags);
            op = Op::Limit(Limit { bytes: typ == 1, inv: flags & 1 != 0, nsecs, rate, cost: nsecs / rate, tokens: max, max, last: crate::timer::uptime_ns() });
        }
        "reject" => {
            // TYPE 1、ICMP_CODE 2
            let typ = u32of(1)?;
            w.be32(1, typ);
            let mut rcode = 0;
            match typ {
                0 | 2 => {
                    let code = need(2)?.d.first().copied().ok_or(EINVAL)?;
                    if typ == 2 && code > 3 {
                        return Err(EINVAL);
                    }
                    if typ == 2 && family != NFPROTO_INET {
                        return Err(EINVAL);
                    }
                    w.attr(2, &[code]);
                    rcode = code;
                }
                1 => {}
                _ => return Err(EINVAL),
            }
            op = Op::Reject { typ, code: rcode };
        }
        "quota" => {
            // BYTES 1、FLAGS 2、CONSUMED 4
            let bytes = need(1)?.be64();
            let flags = at(2).map_or(0, |a| a.be32());
            if flags & !1 != 0 {
                return Err(EINVAL);
            }
            let used = at(4).map_or(0, |a| a.be64());
            if used > bytes {
                return Err(EINVAL);
            }
            w.be64(1, bytes);
            w.be64(4, used);
            w.be32(2, flags | if used >= bytes { 2 } else { 0 });
            op = Op::Quota { quota: bytes, flags, consumed: used };
        }
        "last" => {
            // SET 1、MSECS 2 (nft_last_init: いつ当たったか)
            let lset = at(1).map_or(0, |a| a.be32());
            let mut at_ns = 0;
            if lset != 0
                && let Some(ms) = at(2)
            {
                at_ns = crate::timer::uptime_ns().saturating_sub(ms.be64().saturating_mul(1_000_000));
            }
            op = Op::Last { set: lset, at: at_ns };
            w.0 = last_dump(lset, at_ns);
        }
        "ct" => {
            // DREG 1、KEY 2、DIRECTION 3、SREG 4 (nft_ct_select_ops、nft_ct_get_init / nft_ct_set_init。手本の Linux に
            // ないもの (secmark labels zone eventmask) は EOPNOTSUPP)
            let key = u32of(2)?;
            let dir = at(3).and_then(|a| a.d.first().copied());
            if at(1).is_some() && at(4).is_some() {
                return Err(EINVAL);
            }
            if let Some(d) = at(1) {
                let need_dir = matches!(key, 8 | 9 | 11 | 12 | 19..=22);
                let no_dir = matches!(key, 1 | 0 | 2 | 3 | 5 | 6 | 23);
                if (need_dir && dir.is_none()) || (no_dir && dir.is_some()) {
                    return Err(EINVAL);
                }
                let len = match key {
                    1 | 7 | 10 => 1,
                    0 | 2 | 3 | 5 | 23 | 19 | 20 => 4,
                    6 => 16,
                    8 | 9 => {
                        if family == NFPROTO_IPV4 {
                            4
                        } else {
                            16
                        }
                    }
                    21 | 22 => 16,
                    11 | 12 => 2,
                    14..=16 => 8,
                    _ => return Err(EOPNOTSUPP),
                };
                if let Some(x) = dir
                    && x > 1
                {
                    return Err(EINVAL);
                }
                let dreg = store(Some(d), len, false)?;
                w.be32(1, dump_reg(dreg));
                w.be32(2, key);
                if matches!(key, 8 | 9 | 11 | 12 | 19..=22) || (matches!(key, 14..=16) && dir.is_some()) {
                    w.attr(3, &[dir.unwrap_or(0)]);
                }
                op = Op::Ct { dreg: dreg as u8, key, dir: dir.unwrap_or(2) };
            } else if let Some(sr) = at(4) {
                // 書けるのは mark だけ (CONFIG_NF_CONNTRACK_MARK)
                if key != 3 {
                    return Err(EOPNOTSUPP);
                }
                if dir.is_some() {
                    return Err(EINVAL);
                }
                let sreg = load(Some(sr), 4)?;
                w.be32(4, dump_reg(sreg));
                w.be32(2, key);
                op = Op::CtSet(sreg as u8);
            } else {
                return Err(EINVAL);
            }
        }
        "notrack" => op = Op::Notrack,
        "rt" => {
            // DREG 1、KEY 2 (nft_rt_get_init: classid と xfrm は手本の Linux にない)
            let key = u32of(2)?;
            let len = match key {
                1 => 4,
                2 => 16,
                3 => 2,
                _ => return Err(EOPNOTSUPP),
            };
            let dreg = store(at(1), len, false)?;
            w.be32(2, key);
            w.be32(1, dump_reg(dreg));
            op = Op::Rt { dreg: dreg as u8, key };
        }
        "exthdr" => {
            // DREG 1、TYPE 2、OFFSET 3、LEN 4、FLAGS 5、OP 6、SREG 7 (読むものだけ評価する: TCP のオプション)
            if let Some(d) = data {
                w.0.extend_from_slice(d.d);
            }
            let xop = at(6).map_or(0, |a| a.be32());
            let (typ, off, len, flags) = (at(2).and_then(|a| a.d.first().copied()).unwrap_or(0), at(3).map_or(0, |a| a.be32()), at(4).map_or(0, |a| a.be32()), at(5).map_or(0, |a| a.be32()));
            op = match (at(1), xop) {
                (Some(d), 1) => {
                    let dreg = store(Some(d), if flags & 1 != 0 { 1 } else { len.max(1) as usize }, false)?;
                    Op::Exthdr { dreg: dreg as u8, typ, off, len, flags }
                }
                _ => Op::Break,
            };
        }
        _ => {
            if let Some(d) = data {
                w.0.extend_from_slice(d.d);
            }
            op = Op::Break;
        }
    }
    let _ = family;
    Ok(Expr { name, data: w.0, set, chain, op })
}

/// meta の key の長さ (nft_meta_get_init。手本の Linux にないもの (rtclassid secmark cgroup secpath と bridge の
/// もの) と、読めないもの (nftrace) は None で EOPNOTSUPP)
fn meta_len(key: u32) -> Option<usize> {
    Some(match key {
        // PROTOCOL 1、IIFTYPE 8、OIFTYPE 9
        1 | 8 | 9 => 2,
        // LEN 0、PRIORITY 2、MARK 3、IIF 4、OIF 5、SKUID 10、SKGID 11、NFPROTO 15、L4PROTO 16、PKTTYPE 19、CPU 20、
        // IIFGROUP 21、OIFGROUP 22、PRANDOM 24、TIME_HOUR 32、SDIF 33
        0 | 2..=5 | 10 | 11 | 15 | 16 | 19..=22 | 24 | 32 | 33 => 4,
        // IIFNAME 6、OIFNAME 7、IIFKIND 26、OIFKIND 27、SDIFNAME 34
        6 | 7 | 26 | 27 | 34 => 16,
        // TIME_NS 30
        30 => 8,
        // TIME_DAY 31
        31 => 1,
        _ => return None,
    })
}

// ---- 集まり (set) と要素 ----

const SET_POL: &[(u16, P)] = &[
    (NFTA_SET_TABLE, P::Str(NAME_MAX)),
    (NFTA_SET_NAME, P::Str(NAME_MAX)),
    (NFTA_SET_FLAGS, P::U32),
    (NFTA_SET_KEY_TYPE, P::U32),
    (NFTA_SET_KEY_LEN, P::U32),
    (NFTA_SET_DATA_TYPE, P::U32),
    (NFTA_SET_DATA_LEN, P::U32),
    (NFTA_SET_POLICY, P::U32),
    (NFTA_SET_DESC, P::Nest),
    (NFTA_SET_ID, P::U32),
    (NFTA_SET_TIMEOUT, P::U64),
    (NFTA_SET_GC_INTERVAL, P::U32),
    (NFTA_SET_USERDATA, P::Bin(USERDATA_MAX)),
    (NFTA_SET_OBJ_TYPE, P::U32),
    (NFTA_SET_HANDLE, P::U64),
    (NFTA_SET_EXPR, P::Nest),
    (NFTA_SET_EXPRESSIONS, P::Nest),
];

/// nft_set_lookup_global: 名前で、なければこの batch の NFTA_SET_ID で
fn set_lookup(tb: &Table, name: Option<A>, id: Option<A>) -> Result<usize, (i64, Option<usize>)> {
    if let Some(n) = name {
        if let Some(i) = tb.sets.iter().position(|s| is(n, &s.name)) {
            return Ok(i);
        }
        if id.is_none() {
            return Err((ENOENT, Some(n.off)));
        }
    }
    if let Some(i) = id {
        let v = i.be32();
        return tb.sets.iter().position(|s| s.id != 0 && s.id == v).ok_or((ENOENT, Some(i.off)));
    }
    Err((EINVAL, None))
}

fn newset(cx: &mut Cx, t: &[Option<A>]) -> Res<()> {
    check(t, SET_POL)?;
    let (Some(_), Some(name), Some(klen), Some(_)) = (t[NFTA_SET_TABLE as usize], t[NFTA_SET_NAME as usize], t[NFTA_SET_KEY_LEN as usize], t[NFTA_SET_ID as usize]) else {
        return Err(fail(EINVAL));
    };
    let ktype = t[NFTA_SET_KEY_TYPE as usize].map_or(0, |a| a.be32());
    if ktype & NFT_DATA_RESERVED_MASK == NFT_DATA_RESERVED_MASK {
        return Err(fail(EINVAL));
    }
    let klen = klen.be32();
    if klen == 0 || klen as usize > NFT_DATA_VALUE_MAXLEN {
        return Err(fail(EINVAL));
    }
    let flags = t[NFTA_SET_FLAGS as usize].map_or(0, |a| a.be32());
    if flags & !(NFT_SET_ANONYMOUS | NFT_SET_CONSTANT | NFT_SET_INTERVAL | NFT_SET_TIMEOUT | NFT_SET_MAP | NFT_SET_EVAL | NFT_SET_OBJECT | NFT_SET_CONCAT | NFT_SET_EXPR) != 0
        || flags & (NFT_SET_MAP | NFT_SET_OBJECT) == NFT_SET_MAP | NFT_SET_OBJECT
        || flags & (NFT_SET_EVAL | NFT_SET_OBJECT) == NFT_SET_EVAL | NFT_SET_OBJECT
        || flags & (NFT_SET_ANONYMOUS | NFT_SET_TIMEOUT | NFT_SET_EVAL) == NFT_SET_ANONYMOUS | NFT_SET_TIMEOUT
        || flags & (NFT_SET_CONSTANT | NFT_SET_TIMEOUT) == NFT_SET_CONSTANT | NFT_SET_TIMEOUT
    {
        return Err(fail(EOPNOTSUPP));
    }
    let (mut dtype, mut dlen) = (0, 0);
    if let Some(d) = t[NFTA_SET_DATA_TYPE as usize] {
        if flags & NFT_SET_MAP == 0 {
            return Err(fail(EINVAL));
        }
        dtype = d.be32();
        if dtype & NFT_DATA_RESERVED_MASK == NFT_DATA_RESERVED_MASK && dtype != NFT_DATA_VERDICT {
            return Err(fail(EINVAL));
        }
        if dtype != NFT_DATA_VERDICT {
            dlen = t[NFTA_SET_DATA_LEN as usize].ok_or(fail(EINVAL))?.be32();
            if dlen == 0 || dlen as usize > NFT_DATA_VALUE_MAXLEN {
                return Err(fail(EINVAL));
            }
        } else {
            // sizeof(struct nft_verdict)
            dlen = 16;
        }
    } else if flags & NFT_SET_MAP != 0 {
        return Err(fail(EINVAL));
    }
    let objtype = match t[NFTA_SET_OBJ_TYPE as usize] {
        Some(_) if flags & NFT_SET_OBJECT == 0 => return Err(fail(EINVAL)),
        Some(o) => {
            let v = o.be32();
            if v == 0 || v > 10 {
                return Err(fail(EOPNOTSUPP));
            }
            v
        }
        None if flags & NFT_SET_OBJECT != 0 => return Err(fail(EINVAL)),
        None => 0,
    };
    let mut timeout = 0;
    if let Some(to) = t[NFTA_SET_TIMEOUT as usize] {
        if flags & NFT_SET_TIMEOUT == 0 {
            return Err(fail(EINVAL));
        }
        if flags & NFT_SET_ANONYMOUS != 0 {
            return Err(fail(EOPNOTSUPP));
        }
        timeout = to.be64();
    }
    let mut gc = 0;
    if let Some(gi) = t[NFTA_SET_GC_INTERVAL as usize] {
        if flags & NFT_SET_TIMEOUT == 0 {
            return Err(fail(EINVAL));
        }
        if flags & NFT_SET_ANONYMOUS != 0 {
            return Err(fail(EOPNOTSUPP));
        }
        gc = gi.be32();
    }
    let policy = t[NFTA_SET_POLICY as usize].map_or(0, |a| a.be32());
    if policy > 1 {
        return Err(fail(EOPNOTSUPP));
    }
    let (mut size, mut fields) = (0, Vec::new());
    if let Some(d) = t[NFTA_SET_DESC as usize] {
        let da = d.nested(NFTA_SET_DESC_CONCAT);
        size = da[NFTA_SET_DESC_SIZE as usize].map_or(0, |a| a.be32());
        if let Some(c) = da[NFTA_SET_DESC_CONCAT as usize] {
            for (ty, f) in c.list() {
                if ty != NFTA_LIST_ELEM || fields.len() >= 16 {
                    return Err(fail(if ty != NFTA_LIST_ELEM { EINVAL } else { E2BIG }));
                }
                let fl = f.nested(NFTA_SET_FIELD_LEN)[NFTA_SET_FIELD_LEN as usize].ok_or(fail(EINVAL))?.be32();
                if fl == 0 || fl > 255 {
                    return Err(fail(EINVAL));
                }
                fields.push(fl);
            }
            if fields.iter().map(|&l| (l + 3) & !3).sum::<u32>() != klen {
                return Err(fail(EINVAL));
            }
        }
        if fields.len() > 1 {
            if flags & NFT_SET_CONCAT == 0 {
                return Err(fail(EINVAL));
            }
        } else if flags & NFT_SET_CONCAT != 0 {
            return Err(fail(EINVAL));
        }
    } else if flags & NFT_SET_CONCAT != 0 {
        return Err(fail(EINVAL));
    }
    if t[NFTA_SET_EXPR as usize].is_some() || t[NFTA_SET_EXPRESSIONS as usize].is_some() {
        // 要素ごとの式 (counter など) はまだ
        return Err(fail(EOPNOTSUPP));
    }
    let ti = table_of(cx.st, t[NFTA_SET_TABLE as usize], cx.family, cx.portid).map_err(|e| bad(e, t[NFTA_SET_TABLE as usize]))?;
    if let Some(si) = cx.st.tables[ti].sets.iter().position(|s| is(name, &s.name)) {
        if cx.flags & NLM_F_EXCL != 0 {
            return Err(bad(EEXIST, Some(name)));
        }
        if cx.flags & NLM_F_REPLACE != 0 {
            return Err(fail(EOPNOTSUPP));
        }
        let s = &cx.st.tables[ti].sets[si];
        if s.flags & NFT_SET_ANONYMOUS != 0 {
            return Err(fail(EOPNOTSUPP));
        }
        // nft_set_is_same
        if s.ktype != ktype || s.klen != klen || s.dtype != dtype || s.dlen != dlen || s.flags != flags || s.policy != policy || s.timeout != timeout || s.gc != gc || (size != 0 && s.size != size) || s.fields != fields {
            return Err(bad(EEXIST, Some(name)));
        }
        return Ok(());
    }
    if cx.flags & NLM_F_CREATE == 0 {
        return Err(fail(ENOENT));
    }
    // nf_tables_set_alloc_name: %d は空いているいちばん小さい番号
    let mut nm = name.string();
    if let Some(p) = nm.find('%') {
        if !nm[p..].starts_with("%d") || nm[p + 2..].contains('%') || nm.len() >= 16 {
            return Err(fail(EINVAL));
        }
        let (pre, post) = (String::from(&nm[..p]), String::from(&nm[p + 2..]));
        let used: Vec<u32> = cx.st.tables[ti].sets.iter().filter_map(|s| s.name.strip_prefix(pre.as_str())).filter_map(|r| {
            let d: String = r.chars().take_while(|c| c.is_ascii_digit()).collect();
            d.parse().ok()
        }).collect();
        let n = (0..32768).find(|i| !used.contains(i)).ok_or(fail(23))?;
        nm = alloc::format!("{}{}{}", pre, n, post);
        if cx.st.tables[ti].sets.iter().any(|s| s.name == nm) {
            return Err(fail(23)); // ENFILE
        }
    }
    let handle = alloc_handle(cx.st.tables[ti].handle);
    cx.st.tables[ti].sets.push(Set {
        name: nm,
        handle,
        flags,
        ktype,
        klen,
        dtype,
        dlen,
        objtype,
        policy,
        size,
        fields,
        timeout,
        gc,
        udata: t[NFTA_SET_USERDATA as usize].map(|a| a.d.to_vec()),
        elems: Vec::new(),
        id: t[NFTA_SET_ID as usize].map_or(0, |a| a.be32()),
        bound: 0,
    });
    Ok(())
}

fn delset(cx: &mut Cx, t: &[Option<A>]) -> Res<()> {
    check(t, SET_POL)?;
    let ti = table_of(cx.st, t[NFTA_SET_TABLE as usize], cx.family, cx.portid).map_err(|e| bad(e, t[NFTA_SET_TABLE as usize]))?;
    let tb = &cx.st.tables[ti];
    let (attr, si) = if let Some(h) = t[NFTA_SET_HANDLE as usize] {
        let hd = h.be64();
        (Some(h), tb.sets.iter().position(|s| s.handle == hd))
    } else {
        let a = t[NFTA_SET_NAME as usize];
        (a, a.and_then(|a| tb.sets.iter().position(|s| is(a, &s.name))))
    };
    let Some(si) = si else {
        if cx.typ == DESTROYSET {
            return Ok(());
        }
        return Err(bad(if attr.is_none() { EINVAL } else { ENOENT }, attr));
    };
    let s = &tb.sets[si];
    if s.bound > 0 || (cx.flags & NLM_F_NONREC != 0 && !s.elems.is_empty()) {
        return Err(bad(EBUSY, attr));
    }
    cx.st.tables[ti].sets.remove(si);
    Ok(())
}

const ELEM_LIST_POL: &[(u16, P)] = &[(NFTA_SET_ELEM_LIST_TABLE, P::Str(NAME_MAX)), (NFTA_SET_ELEM_LIST_SET, P::Str(NAME_MAX)), (NFTA_SET_ELEM_LIST_ELEMENTS, P::Nest), (NFTA_SET_ELEM_LIST_SET_ID, P::U32)];

/// 要素 1 つを読む (nft_add_set_elem / nft_del_setelem の前半)
fn elem_parse(tb: &Table, s: &Set, e: A, adding: bool) -> Result<Elem, i64> {
    let ea = e.nested(NFTA_SET_ELEM_EXPRESSIONS);
    let flags = ea[NFTA_SET_ELEM_FLAGS as usize].map(|a| a.be32());
    let f = flags.unwrap_or(0);
    if f & !(NFT_SET_ELEM_INTERVAL_END | NFT_SET_ELEM_CATCHALL) != 0 {
        return Err(EOPNOTSUPP);
    }
    if f & NFT_SET_ELEM_INTERVAL_END != 0 && s.flags & NFT_SET_INTERVAL == 0 {
        return Err(EINVAL);
    }
    let key = match ea[NFTA_SET_ELEM_KEY as usize] {
        Some(k) => {
            let v = value(tb, Some(k))?;
            if v.len() != s.klen as usize {
                return Err(EINVAL);
            }
            v
        }
        None if f & NFT_SET_ELEM_CATCHALL != 0 => Vec::new(),
        None => return Err(EINVAL),
    };
    let key_end = match ea[NFTA_SET_ELEM_KEY_END as usize] {
        Some(k) => {
            if s.flags & NFT_SET_INTERVAL == 0 {
                return Err(EOPNOTSUPP);
            }
            let v = value(tb, Some(k))?;
            if v.len() != s.klen as usize {
                return Err(EINVAL);
            }
            Some(v)
        }
        None => None,
    };
    let mut timeout = None;
    if let Some(to) = ea[NFTA_SET_ELEM_TIMEOUT as usize] {
        if s.flags & NFT_SET_TIMEOUT == 0 {
            return Err(EINVAL);
        }
        timeout = Some(to.be64());
    }
    if ea[NFTA_SET_ELEM_EXPIRATION as usize].is_some() && s.flags & NFT_SET_TIMEOUT == 0 {
        return Err(EINVAL);
    }
    if ea[NFTA_SET_ELEM_OBJREF as usize].is_some() || ea[NFTA_SET_ELEM_EXPR as usize].is_some() || ea[NFTA_SET_ELEM_EXPRESSIONS as usize].is_some() {
        return Err(EOPNOTSUPP);
    }
    let mut data = None;
    if adding {
        match ea[NFTA_SET_ELEM_DATA as usize] {
            Some(d) => {
                if s.flags & NFT_SET_MAP == 0 {
                    return Err(EINVAL);
                }
                let v = data_init(tb, Some(d), Some(s.dtype == NFT_DATA_VERDICT))?;
                if let Data::Value(b) = &v
                    && b.len() != s.dlen as usize
                {
                    return Err(EINVAL);
                }
                data = Some(v);
            }
            None if s.flags & NFT_SET_MAP != 0 && f & NFT_SET_ELEM_INTERVAL_END == 0 => return Err(EINVAL),
            None => {}
        }
    }
    // 消える時刻 (要素の timeout か集まりの既定。NFTA_SET_ELEM_EXPIRATION があれば残りはそれ)
    let mut expires = 0;
    if adding && s.flags & NFT_SET_TIMEOUT != 0 {
        let to = timeout.unwrap_or(s.timeout);
        let left = ea[NFTA_SET_ELEM_EXPIRATION as usize].map_or(to, |a| a.be64());
        if to > 0 {
            expires = crate::timer::uptime_ns() + left.saturating_mul(1_000_000);
        }
    }
    Ok(Elem { key, key_end, data, flags, timeout, udata: ea[NFTA_SET_ELEM_USERDATA as usize].map(|a| a.d.to_vec()), expires })
}

fn same_key(a: &Elem, b: &Elem) -> bool {
    a.key == b.key && a.key_end == b.key_end && a.flags.unwrap_or(0) & 3 == b.flags.unwrap_or(0) & 3
}

fn newsetelem(cx: &mut Cx, t: &[Option<A>]) -> Res<()> {
    check(t, ELEM_LIST_POL)?;
    let Some(list) = t[NFTA_SET_ELEM_LIST_ELEMENTS as usize] else { return Err(fail(EINVAL)) };
    let ti = table_of(cx.st, t[NFTA_SET_ELEM_LIST_TABLE as usize], cx.family, cx.portid).map_err(|e| bad(e, t[NFTA_SET_ELEM_LIST_TABLE as usize]))?;
    let si = set_lookup(&cx.st.tables[ti], t[NFTA_SET_ELEM_LIST_SET as usize], t[NFTA_SET_ELEM_LIST_SET_ID as usize]).map_err(|(e, _)| bad(e, t[NFTA_SET_ELEM_LIST_SET as usize]))?;
    {
        let s = &cx.st.tables[ti].sets[si];
        if s.bound > 0 && s.flags & (NFT_SET_CONSTANT | NFT_SET_ANONYMOUS) != 0 {
            return Err(fail(EBUSY));
        }
    }
    for (_, e) in list.list() {
        let el = {
            let tb = &cx.st.tables[ti];
            elem_parse(tb, &tb.sets[si], e, true).map_err(|err| bad(err, Some(e)))?
        };
        let s = &mut cx.st.tables[ti].sets[si];
        if let Some(old) = s.elems.iter().find(|x| same_key(x, &el)) {
            if old.data != el.data || cx.flags & NLM_F_EXCL != 0 {
                return Err(bad(EEXIST, Some(e)));
            }
            continue;
        }
        let count = s.elems.iter().filter(|x| x.flags.unwrap_or(0) & NFT_SET_ELEM_INTERVAL_END == 0).count() as u32;
        if s.size != 0 && el.flags.unwrap_or(0) & NFT_SET_ELEM_INTERVAL_END == 0 && count >= s.size {
            return Err(bad(23, Some(e))); // ENFILE
        }
        s.elems.push(el);
    }
    Ok(())
}

fn delsetelem(cx: &mut Cx, t: &[Option<A>]) -> Res<()> {
    check(t, ELEM_LIST_POL)?;
    let ti = table_of(cx.st, t[NFTA_SET_ELEM_LIST_TABLE as usize], cx.family, cx.portid).map_err(|e| bad(e, t[NFTA_SET_ELEM_LIST_TABLE as usize]))?;
    let si = set_lookup(&cx.st.tables[ti], t[NFTA_SET_ELEM_LIST_SET as usize], t[NFTA_SET_ELEM_LIST_SET_ID as usize]).map_err(|(e, _)| bad(e, t[NFTA_SET_ELEM_LIST_SET as usize]))?;
    {
        let s = &cx.st.tables[ti].sets[si];
        if s.bound > 0 && s.flags & (NFT_SET_CONSTANT | NFT_SET_ANONYMOUS) != 0 {
            return Err(fail(EBUSY));
        }
    }
    let Some(list) = t[NFTA_SET_ELEM_LIST_ELEMENTS as usize] else {
        cx.st.tables[ti].sets[si].elems.clear();
        return Ok(());
    };
    for (_, e) in list.list() {
        let el = {
            let tb = &cx.st.tables[ti];
            elem_parse(tb, &tb.sets[si], e, false).map_err(|err| bad(err, Some(e)))?
        };
        let s = &mut cx.st.tables[ti].sets[si];
        match s.elems.iter().position(|x| same_key(x, &el)) {
            Some(i) => {
                s.elems.remove(i);
            }
            None if cx.typ == DESTROYSETELEM => {}
            None => return Err(bad(ENOENT, Some(e))),
        }
    }
    Ok(())
}

/// ソケットが閉じた (NETLINK_URELEASE): そのポートが持ち主の表を消す (NFT_TABLE_F_PERSIST なら持ち主をなくすだけ)
pub fn release(portid: u32) {
    let Some(gl) = G.get().as_mut() else { return };
    if !gl.cur.tables.iter().any(|t| t.flags & NFT_TABLE_F_OWNER != 0 && t.owner == portid) {
        return;
    }
    gl.cur.tables.retain_mut(|t| {
        if t.flags & NFT_TABLE_F_OWNER == 0 || t.owner != portid {
            return true;
        }
        if t.flags & NFT_TABLE_F_PERSIST != 0 {
            t.owner = 0;
            return true;
        }
        false
    });
    gl.base_seq = gl.base_seq.wrapping_add(1).max(1);
    refresh_active();
}

// ---- 見る (GET と dump) ----

/// Linux の dump の 1 つのデータグラムの大きさ (NLMSG_GOODSIZE: 4 KiB のページから skb_shared_info をひいたもの)
const GOODSIZE: usize = 3776;

/// メッセージを GOODSIZE までデータグラムに詰め、おしまいに NLMSG_DONE (別のデータグラム)
fn dump_out(msgs: Vec<Vec<u8>>, seq: u32, me: u32, out: &mut Vec<Vec<u8>>) {
    let mut cur: Vec<u8> = Vec::new();
    for m in msgs {
        if !cur.is_empty() && cur.len() + m.len() > GOODSIZE {
            out.push(core::mem::take(&mut cur));
        }
        cur.extend_from_slice(&m);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out.push(done_msg(seq, me));
}

fn done_msg(seq: u32, me: u32) -> Vec<u8> {
    let mut e = Vec::new();
    e.extend_from_slice(&20u32.to_le_bytes());
    e.extend_from_slice(&NLMSG_DONE.to_le_bytes());
    e.extend_from_slice(&NLM_F_MULTI.to_le_bytes());
    e.extend_from_slice(&seq.to_le_bytes());
    e.extend_from_slice(&me.to_le_bytes());
    e.extend_from_slice(&0i32.to_le_bytes());
    e
}

fn res_id() -> u16 {
    (g().base_seq & 0xffff) as u16
}

fn fill_table(tb: &Table, typ: u16, flags: u16, seq: u32, me: u32) -> Vec<u8> {
    let mut w = W(Vec::new());
    w.str(NFTA_TABLE_NAME, &tb.name);
    w.be32(NFTA_TABLE_USE, (tb.chains.len() + tb.sets.len()) as u32);
    w.be64(NFTA_TABLE_HANDLE, tb.handle);
    w.be32(NFTA_TABLE_FLAGS, tb.flags & NFT_TABLE_F_MASK);
    if tb.flags & NFT_TABLE_F_OWNER != 0 {
        w.be32(NFTA_TABLE_OWNER, tb.owner);
    }
    if let Some(u) = &tb.udata {
        w.attr(NFTA_TABLE_USERDATA, u);
    }
    msg(typ, flags, seq, me, tb.family, res_id(), &w.0)
}

fn fill_chain(tb: &Table, c: &Chain, typ: u16, flags: u16, seq: u32, me: u32) -> Vec<u8> {
    let mut w = W(Vec::new());
    w.str(NFTA_CHAIN_TABLE, &tb.name);
    w.str(NFTA_CHAIN_NAME, &c.name);
    w.be64(NFTA_CHAIN_HANDLE, c.handle);
    if let Some(b) = &c.base {
        let h = w.start(NFTA_CHAIN_HOOK);
        w.be32(NFTA_HOOK_HOOKNUM, b.hooknum);
        w.be32(NFTA_HOOK_PRIORITY, b.prio as u32);
        w.end(h);
        w.be32(NFTA_CHAIN_POLICY, b.policy);
        w.str(NFTA_CHAIN_TYPE, b.typ);
    }
    if c.flags != 0 {
        w.be32(NFTA_CHAIN_FLAGS, c.flags);
    }
    w.be32(NFTA_CHAIN_USE, c.rules.len() as u32 + chain_refs(tb, &c.name));
    if let Some(u) = &c.udata {
        w.attr(NFTA_CHAIN_USERDATA, u);
    }
    msg(typ, flags, seq, me, tb.family, res_id(), &w.0)
}

fn fill_rule(tb: &Table, c: &Chain, i: usize, typ: u16, flags: u16, seq: u32, me: u32) -> Vec<u8> {
    let r = &c.rules[i];
    let mut w = W(Vec::new());
    w.str(NFTA_RULE_TABLE, &tb.name);
    w.str(NFTA_RULE_CHAIN, &c.name);
    w.be64(NFTA_RULE_HANDLE, r.handle);
    if i > 0 {
        w.be64(NFTA_RULE_POSITION, c.rules[i - 1].handle);
    }
    let l = w.start(NFTA_RULE_EXPRESSIONS);
    for e in &r.exprs {
        let n = w.start(NFTA_LIST_ELEM);
        w.str(NFTA_EXPR_NAME, e.name);
        w.attr(NFTA_EXPR_DATA, &expr_data(e));
        w.end(n);
    }
    w.end(l);
    if let Some(u) = &r.udata {
        w.attr(NFTA_RULE_USERDATA, u);
    }
    msg(typ, flags, seq, me, tb.family, res_id(), &w.0)
}

/// 集まりの作り (nft_select_set_ops が選ぶもの。NFTA_SET_TYPE に出す名前)
fn set_backend(s: &Set) -> &'static str {
    if s.flags & NFT_SET_INTERVAL != 0 {
        if s.fields.len() > 1 { "nft_set_pipapo_type" } else { "nft_set_rbtree_type" }
    } else if (s.fields.len() > 1 && s.flags & NFT_SET_CONCAT != 0 && s.size == 0) || s.flags & (NFT_SET_TIMEOUT | NFT_SET_EVAL) != 0 {
        "nft_set_rhash_type"
    } else if s.klen <= 2 {
        "nft_set_bitmap_type"
    } else if s.size != 0 {
        if s.klen == 4 { "nft_set_hash_fast_type" } else { "nft_set_hash_type" }
    } else {
        "nft_set_rhash_type"
    }
}

fn fill_set(tb: &Table, s: &Set, typ: u16, flags: u16, seq: u32, me: u32) -> Vec<u8> {
    let mut w = W(Vec::new());
    w.str(NFTA_SET_TABLE, &tb.name);
    w.str(NFTA_SET_NAME, &s.name);
    w.be64(NFTA_SET_HANDLE, s.handle);
    if s.flags != 0 {
        w.be32(NFTA_SET_FLAGS, s.flags);
    }
    w.be32(NFTA_SET_KEY_TYPE, s.ktype);
    w.be32(NFTA_SET_KEY_LEN, s.klen);
    if s.flags & NFT_SET_MAP != 0 {
        w.be32(NFTA_SET_DATA_TYPE, s.dtype);
        w.be32(NFTA_SET_DATA_LEN, s.dlen);
    }
    if s.flags & NFT_SET_OBJECT != 0 {
        w.be32(NFTA_SET_OBJ_TYPE, s.objtype);
    }
    if s.timeout != 0 {
        w.be64(NFTA_SET_TIMEOUT, s.timeout);
    }
    if s.gc != 0 {
        w.be32(NFTA_SET_GC_INTERVAL, s.gc);
    }
    if s.policy != 0 {
        w.be32(NFTA_SET_POLICY, s.policy);
    }
    if let Some(u) = &s.udata {
        w.attr(NFTA_SET_USERDATA, u);
    }
    let d = w.start(NFTA_SET_DESC);
    if s.size != 0 {
        w.be32(NFTA_SET_DESC_SIZE, s.size);
    }
    if s.fields.len() > 1 {
        let c = w.start(NFTA_SET_DESC_CONCAT);
        for &f in &s.fields {
            let e = w.start(NFTA_LIST_ELEM);
            w.be32(NFTA_SET_FIELD_LEN, f);
            w.end(e);
        }
        w.end(c);
    }
    w.end(d);
    w.str(NFTA_SET_TYPE, set_backend(s));
    let n = s.elems.len() as u32;
    w.be32(NFTA_SET_COUNT, if s.flags & NFT_SET_INTERVAL != 0 && s.fields.len() <= 1 { n / 2 } else { n });
    msg(typ, flags, seq, me, tb.family, res_id(), &w.0)
}

fn fill_elem(w: &mut W, s: &Set, e: &Elem) {
    let n = w.start(NFTA_LIST_ELEM);
    if !e.key.is_empty() {
        data_dump(w, NFTA_SET_ELEM_KEY, &Data::Value(e.key.clone()));
    }
    if let Some(k) = &e.key_end {
        data_dump(w, NFTA_SET_ELEM_KEY_END, &Data::Value(k.clone()));
    }
    if let Some(d) = &e.data {
        data_dump(w, NFTA_SET_ELEM_DATA, d);
    }
    if let Some(f) = e.flags {
        w.be32(NFTA_SET_ELEM_FLAGS, f);
    }
    if s.flags & NFT_SET_TIMEOUT != 0 {
        let to = e.timeout.unwrap_or(s.timeout);
        if to != s.timeout {
            w.be64(NFTA_SET_ELEM_TIMEOUT, to);
        }
        if to > 0 {
            // まだ時を数えない (段階 2): 残りはいつも timeout
            w.be64(NFTA_SET_ELEM_EXPIRATION, to);
        }
    }
    if let Some(u) = &e.udata {
        w.attr(NFTA_SET_ELEM_USERDATA, u);
    }
    w.end(n);
}

fn elems_msg(tb: &Table, s: &Set, elems: &[Elem], seq: u32, me: u32) -> Vec<u8> {
    let mut w = W(Vec::new());
    w.str(NFTA_SET_ELEM_LIST_TABLE, &tb.name);
    w.str(NFTA_SET_ELEM_LIST_SET, &s.name);
    let n = w.start(NFTA_SET_ELEM_LIST_ELEMENTS);
    for e in elems {
        fill_elem(&mut w, s, e);
    }
    w.end(n);
    msg(NEWSETELEM, NLM_F_MULTI, seq, me, tb.family, res_id(), &w.0)
}

fn get(m: &[u8], t: u16, dump: bool, me: u32, out: &mut Vec<Vec<u8>>) -> Res<()> {
    let seq = u32::from_le_bytes(m[8..12].try_into().unwrap());
    let family = m[NLMSG_HDRLEN];
    let body = &m[NLMSG_HDRLEN + 4..];
    let base = NLMSG_HDRLEN + 4;
    let st = &g().cur;
    let fam_ok = |f: u8| family == NFPROTO_UNSPEC || family == f;
    match t {
        GETGEN => {
            let mut w = W(Vec::new());
            w.be32(NFTA_GEN_ID, g().base_seq);
            let p = crate::proc::current();
            w.be32(NFTA_GEN_PROC_PID, p.pid);
            w.str(NFTA_GEN_PROC_NAME, core::str::from_utf8(&p.comm[..p.comm.iter().position(|&c| c == 0).unwrap_or(16)]).unwrap_or(""));
            out.push(msg(NEWGEN, 0, seq, me, NFPROTO_UNSPEC, res_id(), &w.0));
            Ok(())
        }
        GETTABLE if dump => {
            let msgs = st.tables.iter().filter(|tb| fam_ok(tb.family)).map(|tb| fill_table(tb, NEWTABLE, NLM_F_MULTI, seq, me)).collect();
            dump_out(msgs, seq, me, out);
            Ok(())
        }
        GETTABLE => {
            let a = parse(body, base, 7);
            let ti = table_of(st, a[NFTA_TABLE_NAME as usize], family, 0).map_err(|e| bad(e, a[NFTA_TABLE_NAME as usize]))?;
            out.push(fill_table(&st.tables[ti], NEWTABLE, 0, seq, me));
            Ok(())
        }
        GETCHAIN if dump => {
            let mut msgs = Vec::new();
            for tb in st.tables.iter().filter(|tb| fam_ok(tb.family)) {
                for c in &tb.chains {
                    msgs.push(fill_chain(tb, c, NEWCHAIN, NLM_F_MULTI, seq, me));
                }
            }
            dump_out(msgs, seq, me, out);
            Ok(())
        }
        GETCHAIN => {
            let a = parse(body, base, 12);
            let ti = table_of(st, a[NFTA_CHAIN_TABLE as usize], family, 0).map_err(|e| bad(e, a[NFTA_CHAIN_TABLE as usize]))?;
            let tb = &st.tables[ti];
            let n = a[NFTA_CHAIN_NAME as usize];
            let ci = n.and_then(|n| chain_of(tb, n)).ok_or(bad(ENOENT, n))?;
            out.push(fill_chain(tb, &tb.chains[ci], NEWCHAIN, 0, seq, me));
            Ok(())
        }
        GETRULE | GETRULE_RESET if dump => {
            let a = parse(body, base, 11);
            let (tf, cf) = (a[NFTA_RULE_TABLE as usize], a[NFTA_RULE_CHAIN as usize]);
            let mut msgs = Vec::new();
            for tb in st.tables.iter().filter(|tb| fam_ok(tb.family) && tf.is_none_or(|x| is(x, &tb.name))) {
                for c in tb.chains.iter().filter(|c| cf.is_none_or(|x| is(x, &c.name))) {
                    for i in 0..c.rules.len() {
                        msgs.push(fill_rule(tb, c, i, NEWRULE, NLM_F_MULTI, seq, me));
                    }
                }
            }
            dump_out(msgs, seq, me, out);
            Ok(())
        }
        GETRULE | GETRULE_RESET => {
            let a = parse(body, base, 11);
            let ti = table_of(st, a[NFTA_RULE_TABLE as usize], family, 0).map_err(|e| bad(e, a[NFTA_RULE_TABLE as usize]))?;
            let tb = &st.tables[ti];
            let cn = a[NFTA_RULE_CHAIN as usize];
            let ci = cn.and_then(|n| chain_of(tb, n)).ok_or(bad(ENOENT, cn))?;
            let c = &tb.chains[ci];
            let h = a[NFTA_RULE_HANDLE as usize];
            let hd = h.ok_or(fail(EINVAL))?.be64();
            let i = c.rules.iter().position(|r| r.handle == hd).ok_or(bad(ENOENT, h))?;
            out.push(fill_rule(tb, c, i, NEWRULE, 0, seq, me));
            Ok(())
        }
        GETSET if dump => {
            let a = parse(body, base, 20);
            let tf = a[NFTA_SET_TABLE as usize];
            let mut msgs = Vec::new();
            for tb in st.tables.iter().filter(|tb| fam_ok(tb.family) && tf.is_none_or(|x| is(x, &tb.name))) {
                for s in &tb.sets {
                    msgs.push(fill_set(tb, s, NEWSET, NLM_F_MULTI, seq, me));
                }
            }
            dump_out(msgs, seq, me, out);
            Ok(())
        }
        GETSET => {
            let a = parse(body, base, 20);
            let ti = table_of(st, a[NFTA_SET_TABLE as usize], family, 0).map_err(|e| bad(e, a[NFTA_SET_TABLE as usize]))?;
            let tb = &st.tables[ti];
            let si = set_lookup(tb, a[NFTA_SET_NAME as usize], None).map_err(|(e, _)| bad(e, a[NFTA_SET_NAME as usize]))?;
            out.push(fill_set(tb, &tb.sets[si], NEWSET, 0, seq, me));
            Ok(())
        }
        GETSETELEM | GETSETELEM_RESET => {
            let a = parse(body, base, 4);
            let ti = table_of(st, a[NFTA_SET_ELEM_LIST_TABLE as usize], family, 0).map_err(|e| bad(e, a[NFTA_SET_ELEM_LIST_TABLE as usize]))?;
            let tb = &st.tables[ti];
            let si = set_lookup(tb, a[NFTA_SET_ELEM_LIST_SET as usize], None).map_err(|(e, _)| bad(e, a[NFTA_SET_ELEM_LIST_SET as usize]))?;
            let s = &tb.sets[si];
            if !dump {
                // 決まった要素を聞くもの (nft get element) はまだ
                return Err(fail(EOPNOTSUPP));
            }
            // 要素の入ったもの (GOODSIZE ずつ) と、おしまいに要素のないものと NLMSG_DONE を 1 つのデータグラムに
            let mut start = 0;
            while start < s.elems.len() {
                let mut end = start;
                let mut len = 0;
                while end < s.elems.len() {
                    let mut w = W(Vec::new());
                    fill_elem(&mut w, s, &s.elems[end]);
                    if end > start && len + w.0.len() > GOODSIZE - 256 {
                        break;
                    }
                    len += w.0.len();
                    end += 1;
                }
                out.push(elems_msg(tb, s, &s.elems[start..end], seq, me));
                start = end;
            }
            let mut last = elems_msg(tb, s, &[], seq, me);
            last.extend_from_slice(&done_msg(seq, me));
            out.push(last);
            Ok(())
        }
        // object と flowtable はまだない
        GETOBJ | GETOBJ_RESET | GETFLOWTABLE if dump => {
            dump_out(Vec::new(), seq, me, out);
            Ok(())
        }
        _ => Err(fail(ENOENT)),
    }
}

// ---- 評価 (段階 2: nf_tables_core.c の nft_do_chain と、式ごとの eval) ----

/// 評価する式 (expr_new が作る)。レジスタは 32 ビットの番号 (0 は判定、4 から NFT_REG_1 / NFT_REG32_00)
#[derive(Clone)]
enum Op {
    /// 評価しないもの (exthdr の TCP のオプションでないものなど): 当たらない
    Break,
    Imm(u8, Vec<u8>),
    Verdict(i32, Option<String>),
    Cmp(u8, u32, Vec<u8>),
    Payload { dreg: u8, base: u32, off: u32, len: u32 },
    PayloadSet { sreg: u8, base: u32, off: u32, len: u32, csum: u32, csum_off: u32, l4: bool },
    Meta(u8, u32),
    MetaSet(u8, u32),
    Bitwise { sreg: u8, dreg: u8, len: u32, op: u32, mask: Vec<u8>, xor: Vec<u8>, shift: u32, sreg2: u8 },
    /// ntoh も hton も、この CPU (little endian) ではバイトの入れかえ
    Byteorder { sreg: u8, dreg: u8, len: u32, size: u32 },
    Lookup { sreg: u8, dreg: Option<u8>, inv: bool },
    Range { sreg: u8, op: u32, from: Vec<u8>, to: Vec<u8> },
    Counter { bytes: u64, packets: u64 },
    Log { prefix: String, level: u32, flags: u32 },
    Limit(Limit),
    Reject { typ: u32, code: u8 },
    Quota { quota: u64, flags: u32, consumed: u64 },
    Last { set: u32, at: u64 },
    Exthdr { dreg: u8, typ: u8, off: u32, len: u32, flags: u32 },
    Rt { dreg: u8, key: u32 },
    /// ct の式 (dir 2 は向きなし)
    Ct { dreg: u8, key: u32, dir: u8 },
    /// ct mark set (書けるのは mark だけ)
    CtSet(u8),
    Notrack,
}

/// nft_limit: ns の token bucket (bytes ならパケットの長さぶん)
#[derive(Clone)]
struct Limit {
    bytes: bool,
    inv: bool,
    nsecs: u64,
    rate: u64,
    cost: u64,
    tokens: u64,
    max: u64,
    last: u64,
}

pub use crate::nfhook::{Pkt, Reject, Verdict, Why};

/// 基の鎖があるか (ip と inet の表で、dormant でないもの)。パケットを作るかどうかを決める (commit と release で)
static ACTIVE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

pub fn active() -> bool {
    ACTIVE.load(core::sync::atomic::Ordering::Relaxed)
}

/// conntrack を動かすか (IPv4 の ip と inet の表に ct の式がある: nf_ct_netns_get。dormant の表でも)
static CT_ON: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

pub fn ct_on() -> bool {
    CT_ON.load(core::sync::atomic::Ordering::Relaxed)
}

fn refresh_active() {
    let tables = &g().cur.tables;
    let on = tables.iter().any(|t| matches!(t.family, NFPROTO_IPV4 | NFPROTO_IPV6 | NFPROTO_INET) && t.flags & NFT_TABLE_F_DORMANT == 0 && t.chains.iter().any(|c| c.base.is_some()));
    ACTIVE.store(on, core::sync::atomic::Ordering::Relaxed);
    let exprs = || tables.iter().filter(|t| matches!(t.family, NFPROTO_IPV4 | NFPROTO_IPV6 | NFPROTO_INET)).flat_map(|t| t.chains.iter()).flat_map(|c| c.rules.iter()).flat_map(|r| r.exprs.iter());
    let ct = exprs().any(|e| matches!(e.op, Op::Ct { .. } | Op::CtSet(_)));
    CT_ON.store(ct, core::sync::atomic::Ordering::Relaxed);
    // ct bytes / packets / avgpkt は数えることを始める (nf_ct_set_acct: もどさない)
    if exprs().any(|e| matches!(e.op, Op::Ct { key: 14..=16, .. })) {
        crate::conntrack::KNOBS.get().acct = true;
    }
}

/// 1 つのフック: IPv4 のパケットは ip と inet の表、IPv6 のものは ip6 と inet の表の基の鎖を priority の順 (同じなら
/// 先に入れたもの) に。drop したらそこで
pub fn hook(h: u32, p: &mut Pkt) -> Verdict {
    if !active() && !ct_on() {
        return Verdict::Accept;
    }
    let st = &mut g().cur;
    let mut list: Vec<(i32, u64, usize, usize)> = Vec::new();
    // conntrack: PRE_ROUTING と LOCAL_OUT の -200 (NF_IP_PRI_CONNTRACK) で見る。表は usize::MAX で印をつける
    let v6 = p.v6();
    let ct = ct_on();
    if ct && matches!(h, 0 | 3) {
        list.push((-200, 0, usize::MAX, 0));
    }
    let l3 = if v6 { NFPROTO_IPV6 } else { NFPROTO_IPV4 };
    for (ti, t) in st.tables.iter().enumerate() {
        if !(t.family == l3 || t.family == NFPROTO_INET) || t.flags & NFT_TABLE_F_DORMANT != 0 {
            continue;
        }
        for (ci, c) in t.chains.iter().enumerate() {
            if let Some(b) = &c.base
                && b.hooknum == h
            {
                list.push((b.prio, b.seq, ti, ci));
            }
        }
    }
    list.sort_unstable_by_key(|x| (x.0, x.1));
    for (_, _, ti, ci) in list {
        if ti == usize::MAX {
            if crate::conntrack::conntrack_in(p.ip, h, &mut p.ct) == NF_DROP {
                return Verdict::Drop { reject: None, why: Why { family: "ip", table: String::from("conntrack"), chain: String::from("table full"), handle: 0 } };
            }
            continue;
        }
        if let v @ Verdict::Drop { .. } = do_chain(&mut st.tables[ti], ci, p, h) {
            return v;
        }
    }
    // LOCAL_IN と POST_ROUTING のいちばん後 (NF_IP_PRI_CONNTRACK_CONFIRM): 表に入れる
    if ct && matches!(h, 1 | 4) {
        crate::conntrack::confirm(&mut p.ct);
    }
    Verdict::Accept
}

struct Regs {
    d: [u8; 80],
    code: i32,
    chain: Option<String>,
}

/// nft_do_chain
fn do_chain(tb: &mut Table, base: usize, p: &mut Pkt, h: u32) -> Verdict {
    let Table { chains, sets, name, family, .. } = tb;
    let fam = match *family {
        NFPROTO_INET => "inet",
        NFPROTO_IPV6 => "ip6",
        _ => "ip",
    };
    let mut r = Regs { d: [0; 80], code: NFT_CONTINUE, chain: None };
    let mut stack: Vec<(usize, usize)> = Vec::new();
    let (mut c, mut ri) = (base, 0usize);
    let mut rj: Option<Reject> = None;
    let l4 = p.valid();
    let now = crate::timer::uptime_ns();
    let why = |chains: &[Chain], c: usize, handle: u64| Why { family: fam, table: name.clone(), chain: chains[c].name.clone(), handle };
    let tracing = TRACE.get().is_some();
    if tracing {
        let b = chains[base].base.as_ref();
        trace(alloc::format!("chain {} {} {} (hook {}, priority {}, policy {})", fam, name, chains[base].name, crate::nfhook::hook_name(h), b.map_or(0, |b| b.prio), if b.is_some_and(|b| b.policy == NF_DROP as u32) { "drop" } else { "accept" }));
    } else {
        chains[base].seen += 1;
    }
    loop {
        if ri < chains[c].rules.len() {
            r.code = NFT_CONTINUE;
            let mut stopped = None;
            for (k, e) in chains[c].rules[ri].exprs.iter_mut().enumerate() {
                eval(&mut e.op, e.set.as_deref(), &mut r, p, sets, h, l4, now, &mut rj, *family);
                if r.code != NFT_CONTINUE {
                    if r.code == NFT_BREAK {
                        stopped = Some((k, e.name));
                    }
                    break;
                }
            }
            if tracing {
                let rule = &chains[c].rules[ri];
                let v = match stopped {
                    Some((k, n)) => alloc::format!("no match at expr {} ({})", k + 1, n),
                    None => verdict_text(r.code, r.chain.as_deref()),
                };
                trace(alloc::format!("  {} rule {}: {} -> {}", chains[c].name, rule.handle, rule_text(rule), v));
            } else if stopped.is_none() {
                let rule = &mut chains[c].rules[ri];
                rule.hits += 1;
                rule.last = now;
            }
            match r.code {
                NFT_CONTINUE | NFT_BREAK => {
                    ri += 1;
                    continue;
                }
                code if code >= 0 => {
                    let handle = chains[c].rules[ri].handle;
                    return match code as u32 & NF_VERDICT_MASK {
                        1 => Verdict::Accept,
                        // NF_QUEUE: 受ける人 (nfnetlink_queue) はいないので、bypass でなければ落とす
                        3 if code as u32 & 0x8000 != 0 => Verdict::Accept,
                        4 => Verdict::Accept,
                        _ => Verdict::Drop { reject: rj, why: why(chains, c, handle) },
                    };
                }
                NFT_JUMP | NFT_GOTO => {
                    let target = r.chain.take().and_then(|n| chains.iter().position(|x| x.name == n));
                    let Some(t) = target else { return Verdict::Drop { reject: None, why: why(chains, c, chains[c].rules[ri].handle) } };
                    if r.code == NFT_JUMP {
                        if stack.len() >= NFT_JUMP_STACK_SIZE {
                            return Verdict::Drop { reject: None, why: why(chains, c, chains[c].rules[ri].handle) };
                        }
                        stack.push((c, ri + 1));
                    }
                    c = t;
                    ri = 0;
                    continue;
                }
                _ => {}
            }
        }
        // 鎖の終わりか return: もどるか、基の鎖の policy
        if let Some((pc, pr)) = stack.pop() {
            c = pc;
            ri = pr;
            continue;
        }
        let pol = chains[base].base.as_ref().map_or(NF_ACCEPT as u32, |b| b.policy);
        if tracing {
            trace(alloc::format!("  policy {}", if pol == NF_DROP as u32 { "drop" } else { "accept" }));
        } else {
            chains[base].to_policy += 1;
        }
        return if pol == NF_DROP as u32 { Verdict::Drop { reject: None, why: why(chains, base, 0) } } else { Verdict::Accept };
    }
}

fn reg_at(d: &mut [u8; 80], r: u8, len: usize) -> &mut [u8] {
    let o = r as usize * 4;
    &mut d[o..o + len]
}

/// nft_data_copy などの書きかた: 終わりの 32 ビットの半端は 0 にしてから
fn reg_put(d: &mut [u8; 80], r: u8, v: &[u8]) {
    let o = r as usize * 4;
    let w = v.len().div_ceil(4) * 4;
    d[o..o + w].fill(0);
    d[o..o + v.len()].copy_from_slice(v);
}

fn put32(d: &mut [u8; 80], r: u8, v: u32) {
    reg_at(d, r, 4).copy_from_slice(&v.to_ne_bytes());
}

fn put16(d: &mut [u8; 80], r: u8, b: [u8; 2]) {
    let x = reg_at(d, r, 4);
    x.fill(0);
    x[..2].copy_from_slice(&b);
}

fn put8(d: &mut [u8; 80], r: u8, v: u8) {
    let x = reg_at(d, r, 4);
    x.fill(0);
    x[0] = v;
}

/// パケットのバイトを読む (nft_payload_eval の base と offset)。はみ出せば None
fn pkt_off(p: &Pkt, base: u32, l4: bool) -> Option<isize> {
    match base {
        // LL: 受けたものだけ (出すものに MAC の頭はない)
        0 => p.ll.map(|l| -(l.len() as isize)),
        1 => Some(0),
        2 => (l4 && p.fragoff() == 0).then(|| p.thoff() as isize),
        // inner: TCP と UDP の頭のうしろ (__nft_payload_inner_offset)
        3 => {
            if !l4 || p.fragoff() != 0 {
                return None;
            }
            let th = p.thoff();
            match p.tprot() {
                17 => Some((th + 8) as isize),
                6 => {
                    let doff = *p.ip.get(th + 12)? as usize >> 4;
                    Some((th + doff * 4) as isize)
                }
                _ => None,
            }
        }
        _ => None,
    }
}

fn pkt_read(p: &Pkt, at: isize, out: &mut [u8]) -> bool {
    for (i, b) in out.iter_mut().enumerate() {
        let k = at + i as isize;
        *b = if k < 0 {
            match p.ll {
                Some(l) => l[(l.len() as isize + k) as usize],
                None => return false,
            }
        } else {
            match p.ip.get(k as usize) {
                Some(&x) => x,
                None => return false,
            }
        };
    }
    true
}

/// 集まりで key を引く (nft_set_do_lookup: hash は同じもの、rbtree は区間の始まりと終わり、pipapo は区間ごと。
/// 消える時刻をすぎたものは見ない。なければ catchall)
fn set_find<'a>(s: &'a Set, key: &[u8], now: u64) -> Option<&'a Elem> {
    let live = |e: &&Elem| e.expires == 0 || now < e.expires;
    let catchall = |e: &&Elem| e.flags.unwrap_or(0) & NFT_SET_ELEM_CATCHALL != 0;
    let is_end = |e: &Elem| e.flags.unwrap_or(0) & NFT_SET_ELEM_INTERVAL_END != 0;
    let mut it = s.elems.iter().filter(live).filter(|e| !catchall(e));
    if s.flags & NFT_SET_INTERVAL == 0 {
        return it.find(|e| e.key == key);
    }
    if s.fields.len() > 1 || s.elems.iter().any(|e| e.key_end.is_some()) {
        // pipapo: フィールドごと (4 バイトにそろえた長さ) に key <= x <= key_end
        let lens: Vec<usize> = if s.fields.len() > 1 { s.fields.iter().map(|&f| (f as usize).div_ceil(4) * 4).collect() } else { vec![s.klen as usize] };
        return it.find(|e| {
            let end = e.key_end.as_deref().unwrap_or(&e.key);
            let mut o = 0;
            lens.iter().all(|&l| {
                let ok = o + l <= key.len() && e.key[o..o + l] <= key[o..o + l] && key[o..o + l] <= end[o..o + l];
                o += l;
                ok
            })
        });
    }
    // rbtree: 始まり (かつ同じもの) を先に。なければ x より小さい中でいちばん大きいものが始まりなら
    let v: Vec<&Elem> = it.collect();
    if let Some(e) = v.iter().find(|e| !is_end(e) && e.key == key) {
        return Some(e);
    }
    let best = v.iter().filter(|e| e.key.as_slice() < key).max_by(|a, b| a.key.cmp(&b.key).then_with(|| is_end(b).cmp(&is_end(a))));
    best.filter(|e| !is_end(e)).copied()
}

#[allow(clippy::too_many_arguments)]
fn eval(op: &mut Op, set: Option<&str>, r: &mut Regs, p: &mut Pkt, sets: &[Set], h: u32, l4: bool, now: u64, rj: &mut Option<Reject>, family: u8) {
    match op {
        Op::Break => r.code = NFT_BREAK,
        Op::Imm(dreg, v) => reg_put(&mut r.d, *dreg, v),
        Op::Verdict(code, chain) => {
            r.code = *code;
            r.chain = chain.clone();
        }
        Op::Cmp(sreg, cop, v) => {
            let o = *sreg as usize * 4;
            let d = r.d[o..o + v.len()].cmp(v.as_slice());
            let ok = match cop {
                0 => d.is_eq(),
                1 => d.is_ne(),
                2 => d.is_lt(),
                3 => d.is_le(),
                4 => d.is_gt(),
                _ => d.is_ge(),
            };
            if !ok {
                r.code = NFT_BREAK;
            }
        }
        Op::Payload { dreg, base, off, len } => {
            let mut b = [0u8; 64];
            let n = *len as usize;
            let ok = pkt_off(p, *base, l4).is_some_and(|at| pkt_read(p, at + *off as isize, &mut b[..n]));
            if ok {
                reg_put(&mut r.d, *dreg, &b[..n]);
            } else {
                r.code = NFT_BREAK;
            }
        }
        Op::PayloadSet { sreg, base, off, len, csum, csum_off, l4: pseudo } => {
            let n = *len as usize;
            let src: Vec<u8> = r.d[*sreg as usize * 4..*sreg as usize * 4 + n].to_vec();
            let Some(at) = pkt_off(p, *base, l4).filter(|&a| a >= 0) else {
                r.code = NFT_BREAK;
                return;
            };
            let at = at as usize + *off as usize;
            if at + n > p.ip.len() {
                r.code = NFT_BREAK;
                return;
            }
            // nft_payload_set_eval: チェックサムを差分で直す (nft_csum_replace)
            let fsum = crate::nfhook::csum_add(0, &p.ip[at..at + n]);
            let tsum = crate::nfhook::csum_add(0, &src);
            let fix = |ip: &mut [u8], c: usize| {
                if c + 2 <= ip.len() {
                    // ~(~old + ~F + T) (F と T は前と後のバイトの 1 の補数の和)
                    let old = u16::from_be_bytes([ip[c], ip[c + 1]]);
                    let mut x = (!old) as u32 + crate::nfhook::csum_fold(fsum) as u32 + (!crate::nfhook::csum_fold(tsum)) as u32;
                    x = (x & 0xffff) + (x >> 16);
                    x = (x & 0xffff) + (x >> 16);
                    let mut v = !(x as u16);
                    if v == 0 {
                        v = 0xffff;
                    }
                    ip[c..c + 2].copy_from_slice(&v.to_be_bytes());
                }
            };
            if *csum == 1 {
                let b = pkt_off(p, *base, l4).unwrap_or(0).max(0) as usize;
                fix(p.ip, b + *csum_off as usize);
            }
            if *pseudo && l4 && p.fragoff() == 0 {
                let th = p.thoff();
                match p.tprot() {
                    6 => fix(p.ip, th + 16),
                    17 if p.ip.len() >= th + 8 && (p.ip[th + 6] != 0 || p.ip[th + 7] != 0) => fix(p.ip, th + 6),
                    _ => {}
                }
            }
            p.ip[at..at + n].copy_from_slice(&src);
        }
        Op::Meta(dreg, key) => {
            let d = &mut r.d;
            let dr = *dreg;
            let ifn = |d: &mut [u8; 80], i: u32| {
                let n = match i {
                    1 => "lo",
                    2 => crate::net::IFNAME,
                    _ => "",
                };
                let x = reg_at(d, dr, 16);
                x.fill(0);
                x[..n.len()].copy_from_slice(n.as_bytes());
            };
            let iftype = |i: u32| match i {
                1 => Some(772u16),
                2 => Some(1u16),
                _ => None,
            };
            let ok = match *key {
                0 => {
                    put32(d, dr, p.len() as u32);
                    true
                }
                // skb->protocol (ETH_P_IP、ネットワークの順のまま)
                1 => {
                    put16(d, dr, [8, 0]);
                    true
                }
                2 => {
                    put32(d, dr, p.priority);
                    true
                }
                3 => {
                    put32(d, dr, p.mark);
                    true
                }
                4 => {
                    put32(d, dr, p.iif);
                    true
                }
                5 => {
                    put32(d, dr, p.oif);
                    true
                }
                6 => {
                    ifn(d, p.iif);
                    true
                }
                7 => {
                    ifn(d, p.oif);
                    true
                }
                8 => iftype(p.iif).map(|t| put16(d, dr, t.to_ne_bytes())).is_some(),
                9 => iftype(p.oif).map(|t| put16(d, dr, t.to_ne_bytes())).is_some(),
                10 => p.sk.map(|s| put32(d, dr, s.0)).is_some(),
                11 => p.sk.map(|s| put32(d, dr, s.1)).is_some(),
                15 => {
                    put8(d, dr, if p.v6() { NFPROTO_IPV6 } else { NFPROTO_IPV4 });
                    true
                }
                16 => {
                    put8(d, dr, if l4 { p.tprot() } else { 0 });
                    true
                }
                19 => {
                    put8(d, dr, p.pkttype);
                    true
                }
                20 => {
                    put32(d, dr, crate::smp::id() as u32);
                    true
                }
                // 口のまとまり (dev->group): 0
                21 => (p.iif != 0).then(|| put32(d, dr, 0)).is_some(),
                22 => (p.oif != 0).then(|| put32(d, dr, 0)).is_some(),
                24 => {
                    put32(d, dr, crate::rand::u32());
                    true
                }
                30 => {
                    reg_at(d, dr, 8).copy_from_slice(&crate::timer::epoch_ns().to_ne_bytes());
                    true
                }
                31 => {
                    let days = crate::timer::epoch_ns() / 1_000_000_000 / 86400;
                    put8(d, dr, ((4 + days) % 7) as u8);
                    true
                }
                32 => {
                    put32(d, dr, (crate::timer::epoch_ns() / 1_000_000_000 % 86400) as u32);
                    true
                }
                // sdif (l3mdev はない): 0 と空の名前
                33 => {
                    put32(d, dr, 0);
                    true
                }
                34 => {
                    reg_at(d, dr, 16).fill(0);
                    true
                }
                // iifkind / oifkind: lo と eth0 は rtnl_link_ops を持たない
                _ => false,
            };
            if !ok {
                r.code = NFT_BREAK;
            }
        }
        Op::MetaSet(sreg, key) => {
            let o = *sreg as usize * 4;
            let v = u32::from_ne_bytes(r.d[o..o + 4].try_into().unwrap());
            match *key {
                3 => p.mark = v,
                2 => p.priority = v,
                19 => {
                    let b = r.d[o];
                    if b <= 3 {
                        p.pkttype = b;
                    }
                }
                _ => {}
            }
        }
        Op::Bitwise { sreg, dreg, len, op, mask, xor, shift, sreg2 } => {
            let words = (*len as usize).div_ceil(4);
            let (s, s2, d) = (*sreg as usize * 4, *sreg2 as usize * 4, *dreg as usize * 4);
            let src: Vec<u32> = (0..words).map(|i| u32::from_ne_bytes(r.d[s + i * 4..s + i * 4 + 4].try_into().unwrap())).collect();
            let mut out = vec![0u32; words];
            match op {
                0 => {
                    let w = |v: &[u8], i: usize| {
                        let mut b = [0u8; 4];
                        for (k, x) in b.iter_mut().enumerate() {
                            *x = v.get(i * 4 + k).copied().unwrap_or(0);
                        }
                        u32::from_ne_bytes(b)
                    };
                    for i in 0..words {
                        out[i] = (src[i] & w(mask, i)) ^ w(xor, i);
                    }
                }
                1 => {
                    let mut carry = 0u32;
                    for i in (0..words).rev() {
                        out[i] = (src[i] << *shift) | carry;
                        carry = src[i].checked_shr(32 - *shift).unwrap_or(0);
                    }
                }
                2 => {
                    let mut carry = 0u32;
                    for i in 0..words {
                        out[i] = carry | (src[i] >> *shift);
                        carry = src[i].checked_shl(32 - *shift).unwrap_or(0);
                    }
                }
                _ => {
                    for i in 0..words {
                        let b = u32::from_ne_bytes(r.d[s2 + i * 4..s2 + i * 4 + 4].try_into().unwrap());
                        out[i] = match op {
                            3 => src[i] & b,
                            4 => src[i] | b,
                            _ => src[i] ^ b,
                        };
                    }
                }
            }
            for (i, v) in out.iter().enumerate() {
                r.d[d + i * 4..d + i * 4 + 4].copy_from_slice(&v.to_ne_bytes());
            }
        }
        Op::Byteorder { sreg, dreg, len, size } => {
            let (s, d, n, z) = (*sreg as usize * 4, *dreg as usize * 4, *len as usize, *size as usize);
            let src: Vec<u8> = r.d[s..s + n].to_vec();
            for k in 0..n / z {
                for j in 0..z {
                    r.d[d + k * z + j] = src[k * z + z - 1 - j];
                }
            }
        }
        Op::Lookup { sreg, dreg, inv } => {
            let Some(s) = set.and_then(|n| sets.iter().find(|x| x.name == n)) else {
                r.code = NFT_BREAK;
                return;
            };
            let o = *sreg as usize * 4;
            let key: Vec<u8> = r.d[o..o + s.klen as usize].to_vec();
            let found = set_find(s, &key, now);
            let mut e = found.filter(|_| !*inv);
            if found.is_some() == *inv {
                // 当たらない: catchall があればそれ
                e = s.elems.iter().find(|x| x.flags.unwrap_or(0) & NFT_SET_ELEM_CATCHALL != 0 && (x.expires == 0 || now < x.expires));
                if e.is_none() {
                    r.code = NFT_BREAK;
                    return;
                }
            }
            if let (Some(e), Some(dr)) = (e, dreg) {
                match &e.data {
                    Some(Data::Verdict(code, chain)) => {
                        r.code = *code;
                        r.chain = chain.clone();
                    }
                    Some(Data::Value(v)) => reg_put(&mut r.d, *dr, v),
                    None => {}
                }
            }
        }
        Op::Range { sreg, op, from, to } => {
            let o = *sreg as usize * 4;
            let x = &r.d[o..o + from.len()];
            let inside = x >= from.as_slice() && x <= to.as_slice();
            if inside == (*op == 1) {
                r.code = NFT_BREAK;
            }
        }
        Op::Counter { bytes, packets } => {
            *packets += 1;
            *bytes += p.len() as u64;
        }
        Op::Log { prefix, level, flags } => crate::nfhook::log(p, prefix, *level, *flags),
        Op::Limit(l) => {
            let cost = if l.bytes { l.nsecs.saturating_mul(p.len() as u64) / l.rate } else { l.cost };
            let tokens = (l.tokens + now.saturating_sub(l.last)).min(l.max);
            l.last = now;
            let over = if tokens >= cost {
                l.tokens = tokens - cost;
                l.inv
            } else {
                l.tokens = tokens;
                !l.inv
            };
            if over {
                r.code = NFT_BREAK;
            }
        }
        Op::Reject { typ, code } => {
            // nft_reject_ipv4_eval / nft_reject_ipv6_eval / nft_reject_inet_eval (icmpx の番号は ICMP か ICMPv6 の
            // 番号に: nft_reject_icmp_code と nft_reject_icmpv6_code)
            let icmpx: [u8; 4] = if p.v6() { [0, 4, 3, 1] } else { [0, 3, 1, 13] };
            *rj = match *typ {
                0 => Some(Reject { rst: false, code: *code }),
                1 => Some(Reject { rst: true, code: 0 }),
                2 if family == NFPROTO_INET => Some(Reject { rst: false, code: icmpx[(*code as usize).min(3)] }),
                _ => None,
            };
            r.code = NF_DROP;
        }
        Op::Quota { quota, flags, consumed } => {
            *consumed += p.len() as u64;
            let over = *consumed > *quota;
            if over ^ (*flags & 1 != 0) {
                r.code = NFT_BREAK;
            }
        }
        Op::Last { set, at } => {
            *set = 1;
            *at = now;
        }
        Op::Exthdr { dreg, typ, off, len, flags } => {
            // nft_exthdr_tcp_eval
            let present = *flags & 1 != 0;
            let found = (|| {
                if !l4 || p.tprot() != 6 || p.fragoff() != 0 {
                    return None;
                }
                let th = p.thoff();
                let tcp = p.ip.get(th..)?;
                let hl = (*tcp.get(12)? as usize >> 4) * 4;
                if hl < 20 || tcp.len() < hl {
                    return None;
                }
                let opt = &tcp[..hl];
                let mut i = 20;
                while i + 1 < hl {
                    let ol = if opt[i] <= 1 || opt[i + 1] == 0 { 1 } else { opt[i + 1] as usize };
                    if opt[i] == *typ {
                        if i + ol > hl || (*len + *off) as usize > ol {
                            return None;
                        }
                        return Some(opt[i + *off as usize..i + (*off + *len) as usize].to_vec());
                    }
                    i += ol;
                }
                None
            })();
            match (found, present) {
                (Some(_), true) => put32(&mut r.d, *dreg, 1),
                (None, true) => put32(&mut r.d, *dreg, 0),
                (Some(v), false) => reg_put(&mut r.d, *dreg, &v),
                (None, false) => r.code = NFT_BREAK,
            }
        }
        Op::Ct { dreg, key, dir } => {
            use crate::conntrack as ctk;
            // nft_ct_get_eval: state はつながりがなくても (invalid か untracked)
            if *key == 0 {
                let st = match (&p.ct, p.ct.info()) {
                    (_, Some(i)) => 1u32 << (i % 3 + 1),
                    (ctk::Ct::Untracked, None) => 64,
                    _ => 1,
                };
                put32(&mut r.d, *dreg, st);
                return;
            }
            let info = p.ct.info().unwrap_or(0);
            let Some(c) = ctk::conn_of(&p.ct) else {
                r.code = NFT_BREAK;
                return;
            };
            let count = |k: u32, d: u8| -> u64 {
                let Some(a) = ctk::acct(c) else { return 0 };
                let i = if k == 15 { 1 } else { 0 };
                if d < 2 { a[d as usize][i] } else { a[0][i] + a[1][i] }
            };
            let t = &c.tuple[if *dir < 2 { *dir as usize } else { 0 }];
            match *key {
                1 => put8(&mut r.d, *dreg, (info >= ctk::CT_IS_REPLY) as u8),
                2 => put32(&mut r.d, *dreg, c.status),
                3 => put32(&mut r.d, *dreg, c.mark),
                5 => put32(&mut r.d, *dreg, (ctk::expires_ns(c) / 1_000_000) as u32),
                7 => put8(&mut r.d, *dreg, if t.l3 == 6 { NFPROTO_IPV6 } else { NFPROTO_IPV4 }),
                10 => put8(&mut r.d, *dreg, t.proto),
                14 | 15 => reg_at(&mut r.d, *dreg, 8).copy_from_slice(&count(*key, *dir).to_ne_bytes()),
                16 => {
                    let (pk, by) = (count(14, *dir), count(15, *dir));
                    reg_at(&mut r.d, *dreg, 8).copy_from_slice(&by.checked_div(pk).unwrap_or(0).to_ne_bytes());
                }
                23 => put32(&mut r.d, *dreg, c.id),
                // 送り元とあて先: NFT_CT_SRC / DST は家族の長さ (IPv4 は 4 バイト。inet の表でもレジスタの残りはそのまま)、
                // SRC_IP / DST_IP は IPv4 だけ、SRC_IP6 / DST_IP6 は IPv6 だけ (ちがえば NFT_BREAK)
                8 | 9 => {
                    let a = if *key == 8 { t.src } else { t.dst };
                    if t.l3 == 6 {
                        reg_at(&mut r.d, *dreg, 16).copy_from_slice(&a.to_be_bytes());
                    } else {
                        reg_at(&mut r.d, *dreg, 4).copy_from_slice(&(a as u32).to_be_bytes());
                    }
                }
                19 | 20 if t.l3 == 4 => reg_at(&mut r.d, *dreg, 4).copy_from_slice(&((if *key == 19 { t.src } else { t.dst }) as u32).to_be_bytes()),
                21 | 22 if t.l3 == 6 => reg_at(&mut r.d, *dreg, 16).copy_from_slice(&(if *key == 21 { t.src } else { t.dst }).to_be_bytes()),
                // ポート (ICMP は src が id、dst が type と code)
                11 => put16(&mut r.d, *dreg, t.sport.to_be_bytes()),
                12 => put16(&mut r.d, *dreg, t.dport.to_be_bytes()),
                _ => r.code = NFT_BREAK,
            }
        }
        Op::CtSet(sreg) => {
            let o = *sreg as usize * 4;
            let v = u32::from_ne_bytes(r.d[o..o + 4].try_into().unwrap());
            if let Some(c) = crate::conntrack::conn_of_mut(&mut p.ct) {
                c.mark = v;
            }
        }
        Op::Notrack => {
            if matches!(p.ct, crate::conntrack::Ct::None) {
                p.ct = crate::conntrack::Ct::Untracked;
            }
        }
        Op::Rt { dreg, key } => {
            // みち (dst) は PRE_ROUTING ではまだない
            let oif = if p.oif != 0 { p.oif } else { p.iif };
            match *key {
                // NFT_RT_NEXTHOP4 は IPv4 のパケットだけ、NFT_RT_NEXTHOP6 は IPv6 (aios の IPv6 は ::1 だけ: あて先そのもの)
                1 if h != crate::nfhook::PRE_ROUTING && !p.v6() => {
                    let gw = crate::nfhook::nexthop(p.daddr(), oif);
                    reg_at(&mut r.d, *dreg, 4).copy_from_slice(&gw.to_be_bytes());
                }
                2 if h != crate::nfhook::PRE_ROUTING && p.v6() && p.ip.len() >= 40 => {
                    let d: [u8; 16] = p.ip[24..40].try_into().unwrap();
                    reg_at(&mut r.d, *dreg, 16).copy_from_slice(&d);
                }
                // TCPMSS: 口の MTU から IP の頭 (20 か 40) と TCP の 20
                3 if h != crate::nfhook::PRE_ROUTING => {
                    let mtu: u32 = if oif == crate::nfhook::IF_LO { 65536 } else { 1500 };
                    let iph: u32 = if p.v6() { 60 } else { 40 };
                    put16(&mut r.d, *dreg, ((mtu - iph) as u16).to_ne_bytes());
                }
                _ => r.code = NFT_BREAK,
            }
        }
    }
}

/// last の dump (NFTA_LAST_SET と、当たってからの ms)
fn last_dump(set: u32, at: u64) -> Vec<u8> {
    let mut w = W(Vec::new());
    w.be32(1, set);
    w.be64(2, if set != 0 { crate::timer::uptime_ns().saturating_sub(at) / 1_000_000 } else { 0 });
    w.0
}

/// 数を持つ式の dump は、いまの数で (counter、quota、last)
fn expr_data(e: &Expr) -> Vec<u8> {
    match &e.op {
        Op::Counter { bytes, packets } => {
            let mut w = W(Vec::new());
            w.be64(1, *bytes);
            w.be64(2, *packets);
            w.0
        }
        Op::Quota { quota, flags, consumed } => {
            let mut w = W(Vec::new());
            let (used, f) = if consumed >= quota { (*quota, flags | 2) } else { (*consumed, *flags) };
            w.be64(1, *quota);
            w.be64(4, used);
            w.be32(2, f);
            w.0
        }
        Op::Last { set, at } => last_dump(*set, *at),
        _ => e.data.clone(),
    }
}

// ---- AI が見る口 (/proc/ai/fw と ctl の fw trace: aish-sys の fw ツール) ----

/// trace のあいだの記録 (None なら記録しない)
static TRACE: Bkl<Option<Vec<String>>> = Bkl::new(None);
/// さいごの trace (/proc/ai/fw に)
static LAST_TRACE: Bkl<Vec<String>> = Bkl::new(Vec::new());

fn trace(line: String) {
    if let Some(v) = TRACE.get().as_mut() {
        v.push(line);
    }
}

pub fn tracing() -> bool {
    TRACE.get().is_some()
}

fn verdict_text(code: i32, chain: Option<&str>) -> String {
    match code {
        NFT_CONTINUE => String::from("continue"),
        NFT_JUMP => alloc::format!("jump {}", chain.unwrap_or("?")),
        NFT_GOTO => alloc::format!("goto {}", chain.unwrap_or("?")),
        NFT_RETURN => String::from("return"),
        c if c as u32 & NF_VERDICT_MASK == 0 => String::from("drop"),
        c if c as u32 & NF_VERDICT_MASK == 1 => String::from("accept"),
        c if c as u32 & NF_VERDICT_MASK == 3 => String::from("queue"),
        c => alloc::format!("verdict {}", c),
    }
}

const META_NAMES: [&str; 37] = [
    "len", "protocol", "priority", "mark", "iif", "oif", "iifname", "oifname", "iiftype", "oiftype", "skuid", "skgid", "nftrace", "rtclassid", "secmark", "nfproto", "l4proto", "bri_iifname", "bri_oifname",
    "pkttype", "cpu", "iifgroup", "oifgroup", "cgroup", "random", "secpath", "iifkind", "oifkind", "bri_iifpvid", "bri_iifvproto", "time", "day", "hour", "sdif", "sdifname", "bri_broute", "bri_iifhwaddr",
];
const CT_NAMES: [&str; 24] = [
    "state", "direction", "status", "mark", "secmark", "expiration", "helper", "l3protocol", "src", "dst", "protocol", "proto_src", "proto_dst", "label", "packets", "bytes", "avgpkt", "zone", "eventmask",
    "src_ip", "dst_ip", "src_ip6", "dst_ip6", "id",
];

/// libnftnl の data の見せかた: 32 ビットずつ (この CPU の順で) 0x%08x
fn hex(b: &[u8]) -> String {
    let mut v: Vec<String> = Vec::new();
    for c in b.chunks(4) {
        let mut w = [0u8; 4];
        w[..c.len()].copy_from_slice(c);
        v.push(alloc::format!("0x{:08x}", u32::from_le_bytes(w)));
    }
    v.join(" ")
}

/// 決まりの式を、nft --debug=netlink (libnftnl の snprintf) と同じ形で: [ payload load 2b @ transport header + 2 => reg 1 ]
fn rule_text(r: &Rule) -> String {
    let reg = |x: u8| dump_reg(x as u32);
    let base = |b: u32| ["link", "network", "transport", "inner"][b.min(3) as usize];
    let mut out: Vec<String> = Vec::new();
    for e in &r.exprs {
        let t = match &e.op {
            Op::Imm(d, v) => alloc::format!("immediate reg {} {}", reg(*d), hex(v)),
            Op::Verdict(c, ch) => match *c {
                NFT_JUMP | NFT_GOTO => alloc::format!("immediate reg 0 {} -> {}", if *c == NFT_JUMP { "jump" } else { "goto" }, ch.as_deref().unwrap_or("?")),
                c => alloc::format!("immediate reg 0 {}", verdict_text(c, None)),
            },
            Op::Cmp(sr, op, v) => alloc::format!("cmp {} reg {} {}", ["eq", "neq", "lt", "lte", "gt", "gte"][(*op).min(5) as usize], reg(*sr), hex(v)),
            Op::Payload { dreg, base: b, off, len } => alloc::format!("payload load {}b @ {} header + {} => reg {}", len, base(*b), off, reg(*dreg)),
            Op::PayloadSet { sreg, base: b, off, len, csum, csum_off, l4 } => alloc::format!("payload write reg {} => {}b @ {} header + {} csum_type {} csum_off {} csum_flags 0x{:x}", reg(*sreg), len, base(*b), off, csum, csum_off, *l4 as u8),
            Op::Meta(d, k) => alloc::format!("meta load {} => reg {}", META_NAMES.get(*k as usize).unwrap_or(&"?"), reg(*d)),
            Op::MetaSet(sr, k) => alloc::format!("meta set {} with reg {}", META_NAMES.get(*k as usize).unwrap_or(&"?"), reg(*sr)),
            Op::Bitwise { sreg, dreg, op, mask, xor, shift, sreg2, .. } => match op {
                0 => alloc::format!("bitwise reg {} = ( reg {} & {} ) ^ {}", reg(*dreg), reg(*sreg), hex(mask), hex(xor)),
                1 | 2 => alloc::format!("bitwise reg {} = ( reg {} {} 0x{:08x} )", reg(*dreg), reg(*sreg), if *op == 1 { "<<" } else { ">>" }, shift),
                _ => alloc::format!("bitwise reg {} = ( reg {} {} reg {} )", reg(*dreg), reg(*sreg), ["&", "|", "^"][(*op as usize).saturating_sub(3).min(2)], reg(*sreg2)),
            },
            Op::Byteorder { sreg, dreg, len, size } => alloc::format!("byteorder reg {} = ntoh(reg {}, {}, {})", reg(*dreg), reg(*sreg), size, len),
            Op::Lookup { sreg, dreg, inv } => alloc::format!("lookup reg {} set {}{}{}", reg(*sreg), e.set.as_deref().unwrap_or("?"), dreg.map_or(String::new(), |d| alloc::format!(" dreg {}", reg(d))), if *inv { " 0x1" } else { "" }),
            Op::Range { sreg, op, from, to } => alloc::format!("range {} reg {} {} {}", if *op == 1 { "neq" } else { "eq" }, reg(*sreg), hex(from), hex(to)),
            Op::Counter { bytes, packets } => alloc::format!("counter pkts {} bytes {}", packets, bytes),
            Op::Log { prefix, level, flags } => alloc::format!("log prefix {} level {} flags {}", prefix, level, flags),
            Op::Limit(l) => alloc::format!("limit rate {}/{}s{} flags 0x{:x}", l.rate, l.nsecs / 1_000_000_000, if l.bytes { " type bytes" } else { "" }, l.inv as u8),
            Op::Reject { typ, code } => alloc::format!("reject type {} code {}", typ, code),
            Op::Quota { quota, consumed, flags } => alloc::format!("quota bytes {} consumed {} flags {}", quota, consumed, flags),
            Op::Last { .. } => String::from("last"),
            Op::Exthdr { dreg, typ, off, len, .. } => alloc::format!("exthdr load tcpopt {}b @ {} + {} => reg {}", len, typ, off, reg(*dreg)),
            Op::Rt { dreg, key } => alloc::format!("rt load {} => reg {}", if *key == 1 { "nexthop4" } else { "tcpmss" }, reg(*dreg)),
            Op::Ct { dreg, key, dir } => alloc::format!("ct load {} => reg {}{}", CT_NAMES.get(*key as usize).unwrap_or(&"?"), reg(*dreg), if *dir < 2 { [" , dir original", " , dir reply"][*dir as usize] } else { "" }),
            Op::CtSet(sr) => alloc::format!("ct set mark with reg {}", reg(*sr)),
            Op::Notrack => String::from("notrack"),
            Op::Break => alloc::format!("{} (not evaluated)", e.name),
        };
        out.push(alloc::format!("[ {} ]", t));
    }
    out.join(" ")
}

fn esc(s: &str) -> String {
    crate::aiproc::esc(s)
}

/// /proc/ai/fw: まとめ、基の鎖、決まり (当たった数と短い式)、落としたもの、conntrack、さいごの trace (1 行 1 つの JSON)
pub fn ai_json() -> String {
    let now = crate::timer::uptime_ns();
    let st = &g().cur;
    let mut s = String::new();
    let nrules: usize = st.tables.iter().flat_map(|t| t.chains.iter()).map(|c| c.rules.len()).sum();
    let _ = core::fmt::Write::write_fmt(&mut s, format_args!("{{\"fw\":\"nftables\",\"active\":{},\"conntrack\":{},\"tables\":{},\"rules\":{},\"generation\":{}}}\n", active(), ct_on(), st.tables.len(), nrules, g().base_seq));
    for t in &st.tables {
        let fam = match t.family {
            NFPROTO_INET => "inet",
            NFPROTO_IPV4 => "ip",
            NFPROTO_IPV6 => "ip6",
            _ => "?",
        };
        for c in &t.chains {
            if let Some(b) = &c.base {
                let _ = core::fmt::Write::write_fmt(
                    &mut s,
                    format_args!(
                        "{{\"chain\":\"{} {} {}\",\"hook\":\"{}\",\"priority\":{},\"policy\":\"{}\",\"type\":\"{}\",\"dormant\":{},\"seen\":{},\"to_policy\":{},\"rules\":{}}}\n",
                        fam,
                        esc(&t.name),
                        esc(&c.name),
                        crate::nfhook::hook_name(b.hooknum),
                        b.prio,
                        if b.policy == NF_DROP as u32 { "drop" } else { "accept" },
                        b.typ,
                        t.flags & NFT_TABLE_F_DORMANT != 0,
                        c.seen,
                        c.to_policy,
                        c.rules.len()
                    ),
                );
            } else {
                let _ = core::fmt::Write::write_fmt(&mut s, format_args!("{{\"chain\":\"{} {} {}\",\"rules\":{}}}\n", fam, esc(&t.name), esc(&c.name), c.rules.len()));
            }
            for r in &c.rules {
                let ago = if r.last == 0 { String::from("null") } else { alloc::format!("{}", (now - r.last) / 1_000_000) };
                let _ = core::fmt::Write::write_fmt(&mut s, format_args!("{{\"rule\":\"{} {} {}\",\"handle\":{},\"hits\":{},\"ago_ms\":{},\"expr\":\"{}\"}}\n", fam, esc(&t.name), esc(&c.name), r.handle, r.hits, ago, esc(&rule_text(r))));
            }
        }
        for set in &t.sets {
            let live = set.elems.iter().filter(|e| e.expires == 0 || now < e.expires).count();
            let _ = core::fmt::Write::write_fmt(&mut s, format_args!("{{\"set\":\"{} {} {}\",\"elements\":{},\"interval\":{},\"map\":{}}}\n", fam, esc(&t.name), esc(&set.name), live, set.flags & NFT_SET_INTERVAL != 0, set.flags & NFT_SET_MAP != 0));
        }
    }
    s.push_str(&crate::nfhook::drops_json());
    s.push_str(&crate::conntrack::summary());
    s.push_str(&crate::conntrack::json_lines(64));
    for l in LAST_TRACE.get().iter() {
        let _ = core::fmt::Write::write_fmt(&mut s, format_args!("{{\"trace\":\"{}\"}}\n", esc(l)));
    }
    s
}

/// ctl の fw trace: 決まりをいまのまま写して、作ったパケットを通してみる (counter も conntrack も変えない)。
/// 行は LAST_TRACE に
pub fn run_trace(what: &str, f: impl FnOnce()) {
    let saved = core::mem::replace(&mut g().cur, g().cur.clone());
    *TRACE.get() = Some(alloc::vec![alloc::format!("trace {}", what)]);
    crate::conntrack::set_dry(true);
    f();
    crate::conntrack::set_dry(false);
    *LAST_TRACE.get() = TRACE.get().take().unwrap_or_default();
    g().cur = saved;
}

/// trace の終わりの行 (道の答え)
pub fn trace_note(line: String) {
    trace(line);
}

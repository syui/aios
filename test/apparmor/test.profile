# test/apparmor.c が読みこむプロファイル (gen.sh が apparmor_parser でバイナリにして blob.h に)。
# どれも /tmp/aatest の下だけ。aatest は /tmp/aatest/t (テストのプログラムの写し) にパスでつく
abi <abi/4.0>,

profile aatest /tmp/aatest/t {
  /tmp/aatest/r r,
  /tmp/aatest/w w,
  /tmp/aatest/rw rw,
  /tmp/aatest/a a,
  deny /tmp/aatest/deny r,
  /tmp/aatest/d/ r,
  /tmp/aatest/d/** rw,
  owner /tmp/aatest/own r,
  /tmp/aatest/k rk,
  link /tmp/aatest/ln -> /tmp/aatest/r,
  /tmp/aatest/ix ix,
  /tmp/aatest/px Px -> aachild,
  /tmp/aatest/cx Cx -> sub,
  /tmp/aatest/pnomap Px -> aanomap,
  /tmp/aatest/ux Ux,
  /tmp/aatest/nox r,
  change_profile -> aachild,
  /proc/*/attr/current rw,
  /proc/*/attr/exec rw,
  /proc/*/attr/prev r,

  profile sub {
    /tmp/aatest/cx mr,
    /proc/*/attr/current r,
  }

  # change_hat で入る hat (aatest//hat1)。中では w が読めて r は読めない
  ^hat1 {
    /tmp/aatest/w r,
    /proc/*/attr/current rw,
    /proc/*/attr/prev r,
  }

  ^hat2 {
    /tmp/aatest/rw r,
    /proc/*/attr/current rw,
  }
}

# 移った先は、そのプログラムを写す mr が要る (Linux は exec のあとの mmap で見なおす。なければ SIGSEGV: aanomap)
profile aachild {
  /tmp/aatest/px mr,
  /proc/*/attr/current rw,
}

profile aanomap {
  /proc/*/attr/current r,
}

profile aacomplain flags=(complain) {
  /tmp/aatest/r r,
}

# capability は chown だけ (root でもほかの読む・書く・setuid・mknod・chmod はだめ)
profile aacap {
  capability chown,
  /tmp/aatest/** rw,
  /proc/*/attr/current r,
}

# ネットワーク (network_v8): inet の stream はみな、inet の dgram は作る・bind・名前だけ、unix は dgram だけ
profile aanet {
  /proc/*/attr/current r,
  # 開いたソケットを持ったまま aacap (ネットワークの決まりがない) に移る (aa_sock_file_perm をたしかめる)
  change_profile -> aacap,
  network inet stream,
  network (create, bind, getattr) inet dgram,
  network unix dgram,
}

# unix の stream を作って listen するだけ (受けられない: つないでくる相手もこの決まりで止まる)。
# network unix ... と書くと、v9 のパーサは unix のすべてを許す決まりも足すので、unix の決まりで書く
profile aanetl {
  /proc/*/attr/current r,
  unix (create, bind, listen) type=stream,
}

# unix の dgram を作って bind するだけ (受けられない: 送ってくる相手もこの決まりで止まる)
profile aanetd {
  /proc/*/attr/current r,
  unix (create, bind) type=dgram,
}

# シグナルと ptrace: usr1 だけ送れる (だれにでも)、だれからでも受けられる。/proc で見られる (ptrace の read) のは
# 決まりのないものだけ
profile aasig {
  /proc/*/attr/current r,
  /proc/*/environ r,
  /proc/*/exe r,
  signal (send) set=(usr1),
  signal (receive),
  ptrace (read) peer=unconfined,
}

# mount: m1 に tmpfs、m3 に nosuid の tmpfs。外すのは m1 だけ (bind や proc はだめ)。読むだけのマウントは
# ためさない (aios は一度でも読むだけのマウントがあると、それからロックなしの作る・消すを使わないので、あとのテストが変わる)
profile aamnt {
  /proc/*/attr/current r,
  capability sys_admin,
  mount fstype=tmpfs -> /tmp/aatest/m1/,
  mount options=(nosuid) fstype=tmpfs -> /tmp/aatest/m3/,
  umount /tmp/aatest/m1/,
}

# AF_UNIX の細かい決まり (network_v9 の af_unix): 抽象名 @aaunix-s で listen して受けられる、そこへつなげる、
# aaunix どうしなら送り受けできる。dgram は @aaunix-d で受けられ、そこへ送れる。パスのソケットはファイルの決まりで
profile aaunix {
  /proc/*/attr/current r,
  /tmp/aatest/sock rw,
  unix (create) type=stream,
  unix (bind, listen, accept, getattr) type=stream addr=@aaunix-s,
  unix (connect, send, receive) type=stream peer=(addr=@aaunix-s),
  unix (send, receive, accept) type=stream addr=@aaunix-s peer=(label=aaunix),
  # bind は @aaunix-? ならよいが、@aaunix-x だけは deny で止める
  unix (bind) type=stream addr=@aaunix-?,
  deny unix (bind) type=stream addr=@aaunix-x,
  # @aaunix-y は番地はあうがラベルがちがう (相手は決まりのないもの)
  unix (connect, send, receive) type=stream peer=(addr=@aaunix-y, label=aaunix),
  unix (create) type=dgram,
  unix (bind, send, receive) type=dgram addr=@aaunix-d,
  unix (send) type=dgram peer=(addr=@aaunix-d),
  # 決まりのないもの (@aaunix-z) へは送れる
  unix (send) type=dgram peer=(label=unconfined),
}

# AF_INET の細かい決まり (network_v9 の af_inet: aios がえらんだちがい apparmor-inet)。127.0.0.1:7801 で listen して
# 受けられる、そこと 80 番 (特権の port) と 7820〜7829 (7825 は deny) へつなげる、つないだソケットは名前なしで送り受け
# できる (peer=(ip=none))。dgram は 127.0.0.1:7811 に bind して、そこへだけ送れる。Linux 7.3 は af_inet を名のらないので、
# Linux 用の blob (gen.sh) では ip= と port= が落ちて (inet, type) の決まりに広がる
profile aainet {
  /proc/*/attr/current r,
  capability net_bind_service,
  network (create) inet stream,
  network (bind, listen, accept, getattr, setopt) inet stream ip=127.0.0.1 port=7801,
  # bind を許す決まりの特権の port は、parser が ADDR_LOCAL_PRIV で書く (listen や accept も)
  network inet stream ip=127.0.0.1 port=80,
  network (connect) inet stream peer=(ip=127.0.0.1 port=7801),
  network (connect) inet stream peer=(ip=127.0.0.1 port=80),
  network (connect) inet stream peer=(ip=127.0.0.1 port=7820-7829),
  deny network (connect) inet stream peer=(port=7825),
  network (send, receive) inet stream peer=(ip=none),
  network (create, bind) inet dgram ip=127.0.0.1 port=7811,
  network (send) inet dgram peer=(ip=127.0.0.1 port=7811),
  network (receive) inet dgram peer=(ip=none),
}

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
  network inet stream,
  network (create, bind, getattr) inet dgram,
  network unix dgram,
}

# unix の stream を作って listen するだけ (受けられない: つないでくる相手もこの決まりで止まる)
profile aanetl {
  /proc/*/attr/current r,
  network (create, bind, listen) unix stream,
}

# unix の dgram を作って bind するだけ (受けられない: 送ってくる相手もこの決まりで止まる)
profile aanetd {
  /proc/*/attr/current r,
  network (create, bind) unix dgram,
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

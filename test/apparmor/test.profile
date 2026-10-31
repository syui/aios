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

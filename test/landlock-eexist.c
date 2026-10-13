// 砂場 (aibox、landlock) の中で、もうあるものを作ろうとしたら EEXIST になるか (カーネル: sysfile.rs の mkdirat、
// symlinkat、mknodat)。Linux と同じく、親に書けなくても「ある」が先。前は landlock が先で EACCES になり、
// create_dir_all (Rust) が書けるディレクトリに書けなかった (aibox の中の aish の write ツール)。
// aios の中で: aibox -- ./landlock-eexist (書けない / と /home に、もうある /tmp と /home を作ろうとする)
// aarch64-linux-gnu-gcc -O2 -static -o landlock-eexist test/landlock-eexist.c
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

static int fail;

static void want_eexist(const char *what, int r) {
  if (r == 0 || errno != EEXIST) {
    printf("FAIL %s: %s (want EEXIST)\n", what, r == 0 ? "made" : strerror(errno));
    fail = 1;
  }
}

int main(void) {
  want_eexist("mkdir /tmp", mkdir("/tmp", 0755));
  want_eexist("mkdir /home", mkdir("/home", 0755));
  want_eexist("symlink /tmp", symlink("x", "/tmp"));
  want_eexist("mknod /tmp", mknod("/tmp", S_IFIFO | 0644, 0));
  // ないものは、書けなければ作れない (砂場の中なら EACCES)
  if (mkdir("/aibox-eexist-nope", 0755) == 0) {
    rmdir("/aibox-eexist-nope");
    puts("note: / is writable here (not in aibox?)");
  }
  puts(fail ? "landlock-eexist: FAIL" : "landlock-eexist: ok");
  return fail;
}

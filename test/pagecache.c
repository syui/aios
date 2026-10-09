// ページキャッシュ (vm.rs) のテスト: 同じ秒のうちに同じ大きさで書きかえたファイルを mmap して、新しい中身が見えるか
// (ext4 の更新時刻は秒までなので、版だけでは見分けられない)。aios の中で: zig cc -target aarch64-linux-musl -O2 -static
// -o pagecache-test test/pagecache.c; ./pagecache-test (/home/ai の ext4)、./pagecache-test /tmp/x (tmpfs)
#include <stdio.h>
#include <fcntl.h>
#include <unistd.h>
#include <string.h>
#include <sys/mman.h>
static int fill(const char *f, char c) { char b[8192]; memset(b, c, sizeof b); int fd = open(f, O_CREAT|O_WRONLY|O_TRUNC, 0644); write(fd, b, sizeof b); close(fd); return 0; }
static char peek(const char *f) { int fd = open(f, O_RDONLY); char *p = mmap(0, 8192, PROT_READ, MAP_PRIVATE, fd, 0); char c = p[4096]; munmap(p, 8192); close(fd); return c; }
int main(int argc, char **argv) {
  const char *f = argc > 1 ? argv[1] : "/home/ai/samesec.bin"; int bad = 0;
  for (int i = 0; i < 20; i++) { char c = 'A' + (i % 26); fill(f, c); if (peek(f) != c) bad++; }
  /* 書くだけ (O_TRUNC なし) の上書き */
  fill(f, 'x'); peek(f); int fd = open(f, O_WRONLY); char y[8192]; memset(y, 'y', sizeof y); write(fd, y, sizeof y); close(fd);
  if (peek(f) != 'y') bad++;
  unlink(f); printf("samesec: %d stale\n", bad); return bad;
}

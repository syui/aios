// /dev/null と /dev/zero の read / write のテスト (カーネル): aios の中で動かす。大きなロックなしの道 (file::fast_dev)
// と同じふるまいか (aarch64-linux-gnu-gcc -O2 -static -o dev test/dev.c。aish-vm の try で file: test/dev.c でも)。
// /dev/null の write は長さを返す (ユーザーのメモリは読まない)、読むだけで開いたものへの write は EBADF、
// /dev/null の read は 0、/dev/zero の read は 0 で埋めて、はみ出さない、へんな番地なら EFAULT
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

int main(void) {
  int fail = 0;
  static char buf[70000];
  int n = open("/dev/null", O_WRONLY);
  if (write(n, buf, sizeof buf) != sizeof buf) {
    puts("FAIL write /dev/null");
    fail = 1;
  }
  if (write(n, (void *)8, 100) != 100) {
    puts("FAIL write a bad pointer to /dev/null (Linux: 100)");
    fail = 1;
  }
  int r = open("/dev/null", O_RDONLY);
  errno = 0;
  if (write(r, buf, 10) != -1 || errno != EBADF) {
    printf("FAIL write to /dev/null opened O_RDONLY: errno %d (want EBADF)\n", errno);
    fail = 1;
  }
  if (read(r, buf, 10) != 0) {
    puts("FAIL read /dev/null");
    fail = 1;
  }
  int z = open("/dev/zero", O_RDONLY);
  memset(buf, 7, sizeof buf);
  if (read(z, buf, 5000) != 5000) {
    puts("FAIL read /dev/zero");
    fail = 1;
  }
  for (int i = 0; i < 5000; i++)
    if (buf[i]) {
      puts("FAIL /dev/zero is not zero");
      fail = 1;
      break;
    }
  if (buf[5000] != 7) {
    puts("FAIL /dev/zero wrote past the length");
    fail = 1;
  }
  if (read(z, buf, sizeof buf) != sizeof buf) {
    puts("FAIL a big read of /dev/zero");
    fail = 1;
  }
  errno = 0;
  if (read(z, (void *)8, 10) != -1 || errno != EFAULT) {
    printf("FAIL read /dev/zero into a bad pointer: errno %d (want EFAULT)\n", errno);
    fail = 1;
  }
  puts(fail ? "dev: FAIL" : "dev: ok");
  return fail;
}

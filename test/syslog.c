// syslog (klogctl) が Linux と同じか: root でなければ READ_ALL (3) と SIZE_BUFFER (10) だけ (dmesg_restrict が 0 のとき)、
// ほかは EPERM (知らない番号も)。READ_ALL は "<レベル>[秒.µs] 本文\n" の行を、len に入る分だけ (終わりの行から)。
// buf がないか len が負なら EINVAL、len 0 は 0、読めないところは EFAULT。root なら OPEN と CLOSE は 0、CONSOLE_LEVEL は
// 1〜8、READ (2) は読んだところから先を、READ_CLEAR (4) と CLEAR (5) は READ_ALL を空に、知らない番号は EINVAL
#define _GNU_SOURCE
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/klog.h>
#include <unistd.h>

static int kl(int type, char *buf, int len) {
  int r = klogctl(type, buf, len);
  return r < 0 ? -errno : r;
}

// 行の形: <数> (と、CONFIG_PRINTK_TIME なら [ 秒.6 けた]) 本文、終わりは改行
static int shaped(const char *b, int n) {
  if (n <= 0 || b[0] != '<' || b[n - 1] != '\n')
    return 0;
  const char *p = b + 1;
  while (*p >= '0' && *p <= '9')
    p++;
  return p > b + 1 && *p == '>';
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  static char buf[1 << 20];
  int root = geteuid() == 0;
  char dr[16] = "?";
  FILE *f = fopen("/proc/sys/kernel/dmesg_restrict", "r");
  if (f) {
    if (!fgets(dr, sizeof dr, f))
      dr[0] = '?';
    fclose(f);
  }
  dr[strcspn(dr, "\n")] = 0;
  int size = kl(10, NULL, 0);
  char pk[64] = "?";
  f = fopen("/proc/sys/kernel/printk", "r");
  if (f) {
    if (!fgets(pk, sizeof pk, f))
      pk[0] = '?';
    fclose(f);
  }
  pk[strcspn(pk, "\n")] = 0;
  // 1 つ目 (端末のレベル) は起こしかた (quiet など) で変わるので見ない
  char *rest = strchr(pk, '\t');
  printf("root %d, dmesg_restrict %s, printk after the first %s, SIZE_BUFFER ok %d\n", root, dr, rest ? rest + 1 : pk, size >= 4096);
  int n = kl(3, buf, 200);
  printf("READ_ALL 200 (fits %d, shaped %d), len 0 %d, NULL %d, len -1 %d, bad pointer %d\n", n > 0 && n <= 200, shaped(buf, n), kl(3, buf, 0), kl(3, NULL, 10), kl(3, buf, -1),
         kl(3, (char *)8, sizeof buf));
  n = kl(3, buf, sizeof buf);
  printf("READ_ALL all (shaped %d, under size %d)\n", shaped(buf, n), n <= size);
  if (!root) {
    printf("not root (CLOSE %d, OPEN %d, READ %d, READ_CLEAR %d, CLEAR %d, CONSOLE_OFF %d, CONSOLE_ON %d, CONSOLE_LEVEL %d, SIZE_UNREAD %d, 99 %d, -1 %d)\n", kl(0, NULL, 0),
           kl(1, NULL, 0), kl(2, buf, 10), kl(4, buf, 10), kl(5, NULL, 0), kl(6, NULL, 0), kl(7, NULL, 0), kl(8, NULL, 4), kl(9, NULL, 0), kl(99, NULL, 0), kl(-1, NULL, 0));
    return 0;
  }
  printf("root (CLOSE %d, OPEN %d, CONSOLE_LEVEL 0 %d, 9 %d, 7 %d, CONSOLE_OFF %d, CONSOLE_ON %d, 99 %d, SIZE_UNREAD ok %d)\n", kl(0, NULL, 0), kl(1, NULL, 0), kl(8, NULL, 0),
         kl(8, NULL, 9), kl(8, NULL, 7), kl(6, NULL, 0), kl(7, NULL, 0), kl(99, NULL, 0), kl(9, NULL, 0) >= 0);
  n = kl(2, buf, 100);
  printf("READ (%d, shaped start %d), NULL %d, len 0 %d\n", n > 0, n > 0 && buf[0] == '<', kl(2, NULL, 10), kl(2, buf, 0));
  int rc = kl(4, buf, sizeof buf);
  int after = kl(3, buf, sizeof buf);
  printf("READ_CLEAR (%d), READ_ALL after %d, CLEAR %d\n", rc > 0, after, kl(5, NULL, 0));
  return 0;
}

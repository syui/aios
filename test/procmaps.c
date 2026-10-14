// /proc/self/maps のパスが / から始まるか (カーネル: vm.rs の maps_text)。前は usr/bin/... のように先頭の / が
// なく、VLC (config_GetLibDir) は自分の行の最初の / から先を自分のディレクトリと見るので、/opt/c/lib ではなく
// /c/lib/vlc にプラグインを探して 1 つも読めなかった。自分 (/proc/self/exe) の行があることも見る。aios の中で動かす
// (aarch64-linux-gnu-gcc -O2 -static -o procmaps test/procmaps.c)
#include <limits.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

int main(void) {
  char exe[PATH_MAX];
  ssize_t n = readlink("/proc/self/exe", exe, sizeof exe - 1);
  if (n <= 0) {
    perror("readlink /proc/self/exe");
    return 1;
  }
  exe[n] = 0;
  FILE *f = fopen("/proc/self/maps", "r");
  if (!f) {
    perror("/proc/self/maps");
    return 1;
  }
  char line[1024];
  int fail = 0, self = 0, paths = 0;
  while (fgets(line, sizeof line, f)) {
    line[strcspn(line, "\n")] = 0;
    // 開始-終わり 権限 オフセット デバイス inode パス (パスの前の空白は 1 つとはかぎらない)
    char *p = line;
    for (int i = 0; i < 5 && p; i++) {
      p = strchr(p, ' ');
      if (p) {
        while (*p == ' ') {
          p++;
        }
      }
    }
    if (!p || !*p) {
      continue;
    }
    paths++;
    if (*p != '/' && *p != '[') {
      printf("FAIL not absolute: %s\n", line);
      fail = 1;
    }
    if (!strcmp(p, exe)) {
      self = 1;
    }
  }
  fclose(f);
  if (!paths) {
    puts("FAIL no paths in /proc/self/maps");
    fail = 1;
  }
  if (!self) {
    printf("FAIL no line for %s\n", exe);
    fail = 1;
  }
  puts(fail ? "procmaps: FAIL" : "procmaps: ok");
  return fail;
}

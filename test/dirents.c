// 大きなディレクトリの getdents のテスト (カーネル: tmpfs の readdir_after、file.rs の getdents_stream): aios の中で動かす。
// /tmp (tmpfs) に 30000 のファイルを作り、ぜんぶ 1 回ずつ読めるか、読みながら消しても飛ばさないか (rm -r と同じ)、
// telldir / seekdir で戻れるか。前は getdents のたびにディレクトリの中身をまるごとヒープにコピーしていて、
// 大きなビルドでカーネルのヒープが足りなくなった (memory allocation of 880640 bytes failed)。
// aarch64-linux-gnu-gcc -O2 -static -o dirents test/dirents.c。aish-vm の try で file: test/dirents.c でも
#include <dirent.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

#define N 30000

int main(void) {
  char dir[] = "/tmp/dirents.XXXXXX";
  if (!mkdtemp(dir)) {
    puts("dirents: FAIL mkdtemp");
    return 1;
  }
  char p[256];
  for (int i = 0; i < N; i++) {
    snprintf(p, sizeof p, "%s/f%05d", dir, i);
    int fd = open(p, O_CREAT | O_WRONLY, 0644);
    if (fd < 0) {
      printf("dirents: FAIL create %s\n", p);
      return 1;
    }
    close(fd);
  }
  int fail = 0;
  // 1 回ずつ読めるか
  static char seen[N];
  DIR *d = opendir(dir);
  struct dirent *e;
  int n = 0, dots = 0;
  long pos = -1;
  char at[32] = "";
  while ((e = readdir(d))) {
    if (!strcmp(e->d_name, ".") || !strcmp(e->d_name, "..")) {
      dots++;
      continue;
    }
    int i = atoi(e->d_name + 1);
    if (i < 0 || i >= N || seen[i]) {
      printf("dirents: FAIL %s twice or unknown\n", e->d_name);
      fail = 1;
      break;
    }
    seen[i] = 1;
    n++;
    if (n == 12345) {
      pos = telldir(d);
    }
    if (n == 12346) {
      snprintf(at, sizeof at, "%s", e->d_name);
    }
  }
  if (n != N || dots != 2) {
    printf("dirents: FAIL read %d of %d (and %d of . ..)\n", n, N, dots);
    fail = 1;
  }
  // seekdir で 12345 番目のあとへ戻る
  seekdir(d, pos);
  e = readdir(d);
  if (!e || strcmp(e->d_name, at)) {
    printf("dirents: FAIL seekdir: got %s, want %s\n", e ? e->d_name : "(end)", at);
    fail = 1;
  }
  closedir(d);
  // 読みながら消す (rm -r): 残らないか
  d = opendir(dir);
  int removed = 0;
  while ((e = readdir(d))) {
    if (e->d_name[0] == '.') {
      continue;
    }
    snprintf(p, sizeof p, "%s/%s", dir, e->d_name);
    if (unlink(p) == 0) {
      removed++;
    }
  }
  closedir(d);
  if (removed != N) {
    printf("dirents: FAIL removed %d of %d while reading\n", removed, N);
    fail = 1;
  }
  if (rmdir(dir) != 0) {
    puts("dirents: FAIL rmdir (not empty)");
    fail = 1;
  }
  puts(fail ? "dirents: FAIL" : "dirents: ok");
  return fail;
}

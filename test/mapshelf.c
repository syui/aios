// 読むだけで開いたファイルを写す・閉じる・外す子 3 つと、そのあいだにファイルを作りなおして rename で置きかえる親
// (ロックなしの open と close の棚 (file::Shelf)、ロックなしの munmap の UNMAPPED、棚を捨てる shelf_forget がすれちがう)。
// 子は写しの中身がいつもどれかの版のページのままかを見る。カーネルがこわれていないかは、このあと ctests を動かして見る
// (todo.json の mem-corrupt-stress: 1e077bc で、d と b を 3 回ずつのあとの ctests が 2 回に 1 回ほど落ちた)
//   mode a: 子だけ (open RDONLY、mmap、close、さわる、munmap)。b: a に親の置きかえを足す。c: 子は O_RDWR (棚にしまわない)
//   d: b の close を munmap のあとに
// 出すだけのテスト: aios の中で (ext4 のホームで) ./mapshelf d 2000; ./mapshelf b 2000
//   aarch64-linux-gnu-gcc -O2 -static -o mapshelf test/mapshelf.c
#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>
#define PG 4096

int main(int argc, char **argv) {
  char mode = argc > 1 ? argv[1][0] : 'a';
  int loops = argc > 2 ? atoi(argv[2]) : 2000;
  const char *p = "ms-file";
  char page[PG];
  memset(page, 'a', PG);
  int f = open(p, O_WRONLY | O_CREAT | O_TRUNC, 0644);
  if (f < 0 || write(f, page, PG) != PG)
    return 1;
  close(f);
  pid_t k[3];
  for (int i = 0; i < 3; i++) {
    k[i] = fork();
    if (k[i] == 0) {
      int bad = 0;
      for (int r = 0; r < loops; r++) {
        int fd = open(p, mode == 'c' ? O_RDWR : O_RDONLY);
        if (fd < 0)
          continue;
        char *q = mmap(NULL, PG, PROT_READ, MAP_PRIVATE, fd, 0);
        if (mode != 'd')
          close(fd);
        if (q == MAP_FAILED) {
          if (mode == 'd')
            close(fd);
          continue;
        }
        // 中身はいつもどれかの版のページ (みな同じ英小文字)。返したページを写していれば、ほかのものが見える
        if (q[0] != q[PG - 1] || q[0] < 'a' || q[0] > 'z' || q[PG / 2] != q[0])
          bad++;
        munmap(q, PG);
        if (mode == 'd')
          close(fd);
      }
      _exit(bad != 0);
    }
  }
  if (mode != 'a')
    for (int r = 0; r < 300; r++) {
      int nf = open("ms-file.n", O_WRONLY | O_CREAT | O_TRUNC, 0644);
      memset(page, 'a' + r % 26, PG);
      if (nf < 0 || write(nf, page, PG) != PG)
        return 1;
      close(nf);
      rename("ms-file.n", p);
    }
  int fails = 0;
  for (int i = 0; i < 3; i++) {
    int st;
    waitpid(k[i], &st, 0);
    fails += !(WIFEXITED(st) && WEXITSTATUS(st) == 0);
  }
  unlink(p);
  printf("mapshelf %c: %d failed\n", mode, fails);
  return 0;
}

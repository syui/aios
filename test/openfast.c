// 棚にないファイルの大きなロックなしの open (カーネルの fastdir::fast_open: statc の表で名前を引き、ext4 の名前と inode
// から新しく開く。書く口も) があっても、Linux と同じ答えになるか: aios の中で動かす
// (aarch64-linux-gnu-gcc -O2 -static -o openfast test/openfast.c)。どれも 2 回ずつ (1 回目で表に入り、2 回目は
// ロックなしの道):
//   link      リンクのディレクトリを通って開いたものの /proc/self/fd は、たどった先のパス
//   perm      (root でなければ) 0000 は読めない、0444 は書けない、0200 は書けるが読めない、通れないディレクトリの下も
//   txtbsy    動かしているプログラムは書く口で開けない (ETXTBSY)。止まれば開ける
//   replace   rename で置きかえたら新しい中身、消したら ENOENT
//   dirfd     開いたディレクトリからの fstatat と openat。ディレクトリの名前を変えて、前の名前に別のディレクトリを
//             作っても、開いたほうのもの
//   fresh     書いたあと (inode の版が進んだあと) の stat は、いまの大きさと時刻 (fstat と同じ)
// openfast DIR
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>
static int fails;
static void expect(const char *what, int got, int want) {
  if (got != want) { printf("FAIL %s: got %d (%s) want %d\n", what, got, got < 0 ? strerror(-got) : "", want); fails++; }
  else printf("ok   %s\n", what);
}
static int op(const char *p, int flags) { int fd = open(p, flags); if (fd < 0) return -errno; close(fd); return 0; }
int main(int argc, char **argv) {
  if (argc > 1 && !strcmp(argv[1], "--wait")) { char c; while (read(0, &c, 1) > 0) {} return 0; }
  char d[PATH_MAX - 64];
  snprintf(d, sizeof d, "%s/openfast.%d", argv[1], (int)getpid());
  mkdir(d, 0755);
  if (chdir(d) < 0 || !getcwd(d, sizeof d)) return 1;
  // 1. リンクのディレクトリを通ったファイルの /proc/self/fd は、たどった先のパス
  mkdir("d", 0755);
  close(open("d/f", O_CREAT | O_WRONLY, 0644));
  if (symlink("d", "l") < 0) perror("symlink");
  char want[PATH_MAX], got[PATH_MAX], fdp[64];
  snprintf(want, sizeof want, "%s/d/f", d);
  for (int k = 0; k < 3; k++) {
    struct stat st;
    stat("l/f", &st);
    int fd = open("l/f", O_RDONLY);
    snprintf(fdp, sizeof fdp, "/proc/self/fd/%d", fd);
    ssize_t n = readlink(fdp, got, sizeof got - 1);
    got[n > 0 ? n : 0] = 0;
    close(fd);
    printf("%s fd path via link (%d): %s\n", strcmp(got, want) ? "FAIL" : "ok  ", k, got);
    if (strcmp(got, want)) fails++;
    fd = open("d/f", O_RDONLY);
    n = readlink(fdp, got, sizeof got - 1);
    got[n > 0 ? n : 0] = 0;
    close(fd);
    printf("%s fd path plain (%d): %s\n", strcmp(got, want) ? "FAIL" : "ok  ", k, got);
    if (strcmp(got, want)) fails++;
  }
  // 2. 許し (root でなければ): 0000 は読めない、0444 は書けない、0200 は書けるが読めない
  if (geteuid() != 0) {
    close(open("p", O_CREAT | O_WRONLY, 0644));
    for (int k = 0; k < 2; k++) expect("read 0644", op("p", O_RDONLY), 0);
    chmod("p", 0);
    for (int k = 0; k < 2; k++) expect("read 0000", op("p", O_RDONLY), -EACCES);
    chmod("p", 0444);
    for (int k = 0; k < 2; k++) expect("read 0444", op("p", O_RDONLY), 0);
    for (int k = 0; k < 2; k++) expect("write 0444", op("p", O_WRONLY), -EACCES);
    for (int k = 0; k < 2; k++) expect("rdwr 0444", op("p", O_RDWR), -EACCES);
    chmod("p", 0200);
    for (int k = 0; k < 2; k++) expect("write 0200", op("p", O_WRONLY), 0);
    for (int k = 0; k < 2; k++) expect("read 0200", op("p", O_RDONLY), -EACCES);
    chmod("p", 0644);
    // 通れないディレクトリの下
    mkdir("nx", 0755);
    close(open("nx/f", O_CREAT | O_WRONLY, 0644));
    for (int k = 0; k < 2; k++) expect("read under 0755", op("nx/f", O_RDONLY), 0);
    chmod("nx", 0644);
    for (int k = 0; k < 2; k++) expect("read under 0644 dir", op("nx/f", O_RDONLY), -EACCES);
    for (int k = 0; k < 2; k++) expect("write under 0644 dir", op("nx/f", O_WRONLY), -EACCES);
    chmod("nx", 0755);
  } else printf("skip permission checks (root)\n");
  // 3. 動かしているプログラムは書く口で開けない (ETXTBSY)。止まれば開ける
  {
    int in = open("/proc/self/exe", O_RDONLY), out = open("tb", O_CREAT | O_WRONLY | O_TRUNC, 0755);
    char buf[65536];
    ssize_t n;
    while ((n = read(in, buf, sizeof buf)) > 0) if (write(out, buf, n) != n) break;
    close(in); close(out);
    for (int k = 0; k < 2; k++) expect("write tb (not running)", op("tb", O_WRONLY), 0);
    int pipefd[2];
    if (pipe(pipefd) < 0) return 1;
    pid_t c = fork();
    if (c == 0) { close(pipefd[1]); dup2(pipefd[0], 0); execl("./tb", "tb", "--wait", (char *)0); _exit(127); }
    usleep(300000);
    for (int k = 0; k < 2; k++) expect("write tb (running)", op("tb", O_WRONLY), -ETXTBSY);
    for (int k = 0; k < 2; k++) expect("rdwr tb (running)", op("tb", O_RDWR), -ETXTBSY);
    for (int k = 0; k < 2; k++) expect("read tb (running)", op("tb", O_RDONLY), 0);
    close(pipefd[0]); close(pipefd[1]);
    waitpid(c, 0, 0);
    for (int k = 0; k < 2; k++) expect("write tb (stopped)", op("tb", O_WRONLY), 0);
  }
  // 4. 消したもの、置きかえたもの
  {
    int fd = open("r", O_CREAT | O_WRONLY, 0644); if (write(fd, "old", 3) != 3) {} close(fd);
    for (int k = 0; k < 2; k++) expect("open r", op("r", O_RDONLY), 0);
    fd = open("r2", O_CREAT | O_WRONLY, 0644); if (write(fd, "new", 3) != 3) {} close(fd);
    rename("r2", "r");
    char b[8] = {0};
    fd = open("r", O_RDONLY); if (read(fd, b, 3) != 3) {} close(fd);
    printf("%s after rename-over: %s\n", strcmp(b, "new") ? "FAIL" : "ok  ", b);
    if (strcmp(b, "new")) fails++;
    unlink("r");
    for (int k = 0; k < 2; k++) expect("open unlinked", op("r", O_RDONLY), -ENOENT);
    for (int k = 0; k < 2; k++) expect("write unlinked", op("r", O_WRONLY), -ENOENT);
  }
  // 5. 開いたディレクトリから (fstatat と openat)
  {
    mkdir("D", 0755);
    int fd = open("D/f", O_CREAT | O_WRONLY, 0644); if (write(fd, "D", 1) != 1) {} close(fd);
    struct stat a, b;
    stat("D/f", &a);
    int dfd = open("D", O_RDONLY | O_DIRECTORY);
    for (int k = 0; k < 2; k++) {
      int r = fstatat(dfd, "f", &b, 0);
      printf("%s fstatat dirfd (%d)\n", r == 0 && b.st_ino == a.st_ino ? "ok  " : "FAIL", k);
      if (r != 0 || b.st_ino != a.st_ino) fails++;
    }
    for (int k = 0; k < 2; k++) expect("fstatat dirfd missing", fstatat(dfd, "nope", &b, 0) < 0 ? -errno : 0, -ENOENT);
    // 名前を変えて、前の名前に別のディレクトリと f を作る: dirfd は前のもの (E) をさしたまま
    rename("D", "E");
    mkdir("D", 0755);
    fd = open("D/f", O_CREAT | O_WRONLY, 0644); if (write(fd, "new", 3) != 3) {} close(fd);
    struct stat c;
    stat("D/f", &c);
    for (int k = 0; k < 2; k++) {
      int r = fstatat(dfd, "f", &b, 0);
      printf("%s fstatat dirfd after rename (%d)\n", r == 0 && b.st_ino == a.st_ino && b.st_ino != c.st_ino ? "ok  " : "FAIL", k);
      if (r != 0 || b.st_ino != a.st_ino || b.st_ino == c.st_ino) fails++;
      char got1[4] = {0};
      int ofd = openat(dfd, "f", O_RDONLY);
      if (ofd < 0 || read(ofd, got1, 3) != 1 || got1[0] != 'D') { printf("FAIL openat dirfd after rename (%d): '%s'\n", k, got1); fails++; }
      else printf("ok   openat dirfd after rename (%d)\n", k);
      if (ofd >= 0) close(ofd);
    }
    close(dfd);
  }
  // 6. 書いたあとの stat (inode の版が進んだあと) は、いまの大きさ
  {
    int fd = open("w", O_CREAT | O_WRONLY, 0644);
    close(fd);
    struct stat a, b;
    for (int k = 0; k < 3; k++) {
      stat("w", &a);
      fd = open("w", O_WRONLY | O_APPEND);
      if (write(fd, "0123456789", 10) != 10) {}
      fstat(fd, &b);
      close(fd);
      stat("w", &a);
      int ok = a.st_size == 10 * (k + 1) && a.st_size == b.st_size && a.st_mtim.tv_sec == b.st_mtim.tv_sec && a.st_mtim.tv_nsec == b.st_mtim.tv_nsec && a.st_blocks == b.st_blocks;
      printf("%s stat after write (%d): size %lld\n", ok ? "ok  " : "FAIL", k, (long long)a.st_size);
      if (!ok) fails++;
      int dfd = open(".", O_RDONLY | O_DIRECTORY);
      struct stat c;
      fstatat(dfd, "w", &c, 0);
      close(dfd);
      if (c.st_size != b.st_size) { printf("FAIL fstatat after write (%d): size %lld\n", k, (long long)c.st_size); fails++; }
    }
  }
  printf("openfast: %s (%d failed)\n", fails ? "BAD" : "ok", fails);
  return fails != 0;
}

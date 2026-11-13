// sync_file_range が Linux と同じ答えか (カーネル: sysfile.rs の sync_file_range)。ふつうのファイルとディレクトリは 0、
// パイプ・ソケット・/dev/null は ESPIPE、まちがった flags と負の範囲とあふれは EINVAL、閉じた fd と O_PATH は EBADF。
// 書き出し (WRITE) のあとも中身はそのまま。PostgreSQL の initdb と checkpointer が使う (前は ENOSYS)
// (aarch64-linux-gnu-gcc -O2 -static -o syncrange test/syncrange.c。test/ldiff.sh syncrange @home で Linux とくらべる)
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

static const char *err(int r) {
  if (r == 0) return "ok";
  switch (errno) {
  case EINVAL: return "EINVAL";
  case EBADF: return "EBADF";
  case ESPIPE: return "ESPIPE";
  case ENOSYS: return "ENOSYS";
  default: return strerror(errno);
  }
}

#define W SYNC_FILE_RANGE_WRITE
#define WB SYNC_FILE_RANGE_WAIT_BEFORE
#define WA SYNC_FILE_RANGE_WAIT_AFTER

int main(int argc, char **argv) {
  const char *dir = argc > 1 ? argv[1] : "/tmp";
  char path[512];
  snprintf(path, sizeof path, "%s/syncrange.%d", dir, getpid());
  int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0600);
  if (fd < 0) {
    perror(path);
    return 2;
  }
  char buf[8192];
  memset(buf, 'a', sizeof buf);
  if (write(fd, buf, sizeof buf) != (ssize_t)sizeof buf) return 2;

  printf("write: %s\n", err(sync_file_range(fd, 0, 4096, W)));
  printf("write+wait: %s\n", err(sync_file_range(fd, 0, 0, WB | W | WA)));
  printf("wait only: %s\n", err(sync_file_range(fd, 4096, 4096, WA)));
  printf("no flags: %s\n", err(sync_file_range(fd, 0, 0, 0)));
  printf("past end: %s\n", err(sync_file_range(fd, 1 << 20, 4096, W)));
  printf("bad flags: %s\n", err(sync_file_range(fd, 0, 0, 8)));
  printf("negative offset: %s\n", err(sync_file_range(fd, -1, 0, W)));
  printf("negative nbytes: %s\n", err(sync_file_range(fd, 4096, -100, W)));
  printf("overflow: %s\n", err(sync_file_range(fd, INT64_MAX - 10, 100, W)));
  // 順: flags がまちがっていれば、fd の種類より先に EINVAL
  int p[2];
  if (pipe(p) < 0) return 2;
  printf("pipe: %s\n", err(sync_file_range(p[0], 0, 0, W)));
  printf("pipe bad flags: %s\n", err(sync_file_range(p[0], 0, 0, 8)));
  int sv[2];
  socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
  printf("socket: %s\n", err(sync_file_range(sv[0], 0, 0, W)));
  int null = open("/dev/null", O_RDWR);
  printf("dev null: %s\n", err(sync_file_range(null, 0, 0, W)));
  int d = open(dir, O_RDONLY | O_DIRECTORY);
  printf("directory: %s\n", err(sync_file_range(d, 0, 0, W)));
  int op = open(path, O_PATH);
  printf("o_path: %s\n", err(sync_file_range(op, 0, 0, W)));
  printf("closed fd: %s\n", err(sync_file_range(1000, 0, 0, W)));
  printf("bad fd bad flags: %s\n", err(sync_file_range(1000, 0, 0, 8)));
  // 読むだけで開いたものも書き戻せる (Linux は fd の口を見ない)
  int ro = open(path, O_RDONLY);
  printf("read-only fd: %s\n", err(sync_file_range(ro, 0, 0, W)));

  char back[8192];
  ssize_t n = pread(fd, back, sizeof back, 0);
  printf("content: %zd same %d\n", n, n == (ssize_t)sizeof buf && !memcmp(back, buf, sizeof buf));
  unlink(path);
  printf("done\n");
  return 0;
}

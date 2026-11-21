// cwd からの相対パスは、cwd から先だけを通れればよい (上のディレクトリが通れなくても): Linux は cwd の dentry から
// たどる。root で、0700 のディレクトリの中の 0755 のディレクトリに入ってから nobody になり、相対パスで stat、open、
// 作る、消す、..、getcwd。引数のディレクトリごとに (ext4 と tmpfs)
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

static void say(const char *k, long v) { printf(", %s %ld", k, v); }
static long E(long r) { return r < 0 ? -errno : r; }
static long st(const char *p) {
  struct stat s;
  return stat(p, &s) ? -errno : 0;
}
static long op(const char *p, int fl) {
  int fd = open(p, fl, 0644);
  if (fd < 0)
    return -errno;
  close(fd);
  return 0;
}

static int one(const char *base) {
  char top[256], in[300], f[320], sub[320];
  snprintf(top, sizeof top, "%s/cwdperm-t", base);
  snprintf(in, sizeof in, "%s/in", top);
  snprintf(f, sizeof f, "%s/f", in);
  snprintf(sub, sizeof sub, "%s/sub", in);
  mkdir(top, 0700);
  mkdir(in, 0777);
  mkdir(sub, 0755);
  close(open(f, O_CREAT | O_WRONLY, 0644));
  pid_t c = fork();
  if (c == 0) {
    if (chdir(in) || setresgid(65534, 65534, 65534) || setresuid(65534, 65534, 65534))
      _exit(1);
    printf("%s", base[1] == 't' ? "tmpfs" : "ext4");
    say("stat f", st("f"));
    say("./f", st("./f"));
    say("sub/../f", st("sub/../f"));
    say("open f", op("f", O_RDONLY));
    say("create", op("mine", O_CREAT | O_WRONLY));
    say("unlink", E(unlink("mine")));
    say("mkdir", E(mkdir("d", 0755)));
    say("rmdir", E(rmdir("d")));
    say("stat .", st("."));
    say("stat ..", st(".."));
    say("../in/f", st("../in/f"));
    say("absolute", st(f));
    char b[256];
    say("getcwd", getcwd(b, sizeof b) ? 0 : -errno);
    say("chdir sub", E(chdir("sub")));
    say("stat ../f", st("../f"));
    printf("\n");
    _exit(0);
  }
  int s;
  waitpid(c, &s, 0);
  unlink(f);
  rmdir(sub);
  rmdir(in);
  rmdir(top);
  return WIFEXITED(s) ? WEXITSTATUS(s) : 100;
}

int main(int argc, char **argv) {
  setvbuf(stdout, NULL, _IONBF, 0);
  if (getuid() != 0) {
    printf("cwdperm: not root, skipped\n");
    return 0;
  }
  int r = 0;
  for (int i = 1; i < argc; i++)
    r |= one(argv[i]);
  return argc > 1 ? r : one("/tmp");
}

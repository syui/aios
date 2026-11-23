// capability (inheritable、permitted、effective、bounding、ambient の 5 つの組) が Linux と同じか (root で):
//   status   /proc/self/status の CapInh〜CapAmb
//   capget   版 1・2・3、ほかの pid、まちがった版と pid、data が NULL
//   capset   自分だけ、effective は permitted の中、permitted は広げられない、inheritable は bounding の中、版 1 は下の 32
//   effect   CAP_CHOWN と CAP_DAC_OVERRIDE / CAP_DAC_READ_SEARCH を落とすと chown と open が EPERM / EACCES
//   setuid   setresuid でみな落ちる、KEEPCAPS なら permitted は残る、seteuid と setfsuid で effective が出入りする
//   bounding PR_CAPBSET_READ / DROP、root の exec の permitted は bounding と inheritable から
//   ambient  PR_CAP_AMBIENT (RAISE は permitted と inheritable にあるときだけ)、root でない人の exec にわたる
//   secbits  PR_GET/SET_SECUREBITS、NOROOT なら root の exec も何も持たない、ロック、KEEPCAPS は exec で外れる
//   file     security.capability (版 2、版 3) をつけたファイルを root でない人が exec すると permitted と effective に。
//            書きかえや chown で消える、root でない人はつけられない、no_new_privs ではわたらない、bounding になければ EPERM
// 出すだけのテスト: test/ldiff.sh --root caps @home (ファイルの capability は ext4 のところで)。終わりの番号は 0
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/capability.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/fsuid.h>
#include <sys/prctl.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <sys/xattr.h>
#include <unistd.h>

static uint64_t full;
static char base[256], self[256];

static const char *err(long r) {
  static char b[32];
  if (r >= 0) {
    snprintf(b, sizeof b, "%ld", r);
    return b;
  }
  switch (errno) {
  case EPERM: return "EPERM";
  case EINVAL: return "EINVAL";
  case EACCES: return "EACCES";
  case ESRCH: return "ESRCH";
  case EFAULT: return "EFAULT";
  case ENODATA: return "ENODATA";
  case ERANGE: return "ERANGE";
  case EOPNOTSUPP: return "EOPNOTSUPP";
  default: return strerror(errno);
  }
}

static void res(const char *what, long r) { printf("%-48s %s\n", what, err(r)); }

// 組を名前で: all、0、all-cN (少し欠けた all)、cN,cM
static const char *desc(uint64_t v) {
  static char b[4][160];
  static int k;
  char *s = b[k++ & 3];
  if (full && v == full)
    return "all";
  if (v == 0)
    return "0";
  uint64_t miss = full & ~v;
  s[0] = 0;
  if (full && (v & ~full) == 0 && __builtin_popcountll(miss) <= 4) {
    strcpy(s, "all");
    for (int i = 0; i < 64; i++)
      if (miss >> i & 1)
        sprintf(s + strlen(s), "-c%d", i);
    return s;
  }
  for (int i = 0; i < 64; i++)
    if (v >> i & 1)
      sprintf(s + strlen(s), "%sc%d", s[0] ? "," : "", i);
  return s;
}

static uint64_t field(const char *name) {
  FILE *f = fopen("/proc/self/status", "r");
  char line[256];
  uint64_t v = ~0ULL;
  size_t n = strlen(name);
  while (f && fgets(line, sizeof line, f))
    if (!strncmp(line, name, n) && line[n] == ':')
      v = strtoull(line + n + 1, NULL, 16);
  if (f)
    fclose(f);
  return v;
}

static void status(const char *label) {
  printf("%-14s inh %s prm %s eff %s bnd %s amb %s\n", label, desc(field("CapInh")), desc(field("CapPrm")), desc(field("CapEff")),
         desc(field("CapBnd")), desc(field("CapAmb")));
}

static long capget_(uint32_t ver, int pid, struct __user_cap_data_struct *d, uint32_t *ver_out) {
  struct __user_cap_header_struct h = {ver, pid};
  long r = syscall(SYS_capget, &h, d);
  if (ver_out)
    *ver_out = h.version;
  return r;
}

static long capset_(uint32_t ver, int pid, uint64_t eff, uint64_t prm, uint64_t inh) {
  struct __user_cap_header_struct h = {ver, pid};
  struct __user_cap_data_struct d[2] = {{(uint32_t)eff, (uint32_t)prm, (uint32_t)inh}, {(uint32_t)(eff >> 32), (uint32_t)(prm >> 32), (uint32_t)(inh >> 32)}};
  return syscall(SYS_capset, &h, d);
}

static void child(void (*f)(void)) {
  fflush(stdout);
  pid_t p = fork();
  if (p == 0) {
    f();
    fflush(stdout);
    _exit(0);
  }
  waitpid(p, NULL, 0);
}

// 自分 (か path) を exec して、そこで status を出す
static void exec_show(const char *path, const char *label) {
  fflush(stdout);
  pid_t p = fork();
  if (p == 0) {
    execl(path, "caps", "--show", label, (char *)NULL);
    printf("%-48s %s\n", label, err(-1));
    fflush(stdout);
    _exit(0);
  }
  waitpid(p, NULL, 0);
}

static char file[300];

static void t_root(void) {
  status("root");
  struct __user_cap_data_struct d[2];
  uint32_t v;
  memset(d, 0, sizeof d);
  long r = capget_(_LINUX_CAPABILITY_VERSION_3, 0, d, NULL);
  printf("%-48s %s eff %s prm %s inh %s\n", "capget v3", err(r), desc(d[0].effective | (uint64_t)d[1].effective << 32),
         desc(d[0].permitted | (uint64_t)d[1].permitted << 32), desc(d[0].inheritable | (uint64_t)d[1].inheritable << 32));
  memset(d, 0, sizeof d);
  r = capget_(_LINUX_CAPABILITY_VERSION_2, 0, d, NULL);
  printf("%-48s %s eff %s\n", "capget v2", err(r), desc(d[0].effective | (uint64_t)d[1].effective << 32));
  memset(d, 0xee, sizeof d);
  r = capget_(_LINUX_CAPABILITY_VERSION_1, 0, d, NULL);
  printf("%-48s %s eff %08x, second word untouched %d\n", "capget v1", err(r), d[0].effective, d[1].effective == 0xeeeeeeee);
  memset(d, 0, sizeof d);
  r = capget_(_LINUX_CAPABILITY_VERSION_3, getppid(), d, NULL);
  printf("%-48s %s eff %s\n", "capget the parent", err(r), desc(d[0].effective | (uint64_t)d[1].effective << 32));
  res("capget pid -1", capget_(_LINUX_CAPABILITY_VERSION_3, -1, d, NULL));
  res("capget pid 4000000", capget_(_LINUX_CAPABILITY_VERSION_3, 4000000, d, NULL));
  r = capget_(0x12345678, 0, d, &v);
  printf("%-48s %s version now %08x\n", "capget bad version", err(r), v);
  r = capget_(0x12345678, 0, NULL, &v);
  printf("%-48s %s version now %08x\n", "capget bad version, data NULL", err(r), v);
  res("capget data NULL", capget_(_LINUX_CAPABILITY_VERSION_3, 0, NULL, NULL));
}

static void t_capset(void) {
  res("capset the parent", capset_(_LINUX_CAPABILITY_VERSION_3, getppid(), full, full, 0));
  res("capset bad version", capset_(0x12345678, 0, full, full, 0));
  res("capset eff without CHOWN", capset_(_LINUX_CAPABILITY_VERSION_3, 0, full & ~1ULL, full, 0));
  status("eff -CHOWN");
  res("  chown", chown(file, 1234, -1));
  res("capset eff back", capset_(_LINUX_CAPABILITY_VERSION_3, 0, full, full, 0));
  res("  chown", chown(file, 1234, -1));
  chown(file, 0, -1);
  res("capset eff not in prm", capset_(_LINUX_CAPABILITY_VERSION_3, 0, full, full & ~1ULL, 0));
  res("capset prm without CHOWN", capset_(_LINUX_CAPABILITY_VERSION_3, 0, full & ~1ULL, full & ~1ULL, 0));
  res("capset prm grows back", capset_(_LINUX_CAPABILITY_VERSION_3, 0, full & ~1ULL, full, 0));
  res("capset inh NET_BIND_SERVICE", capset_(_LINUX_CAPABILITY_VERSION_3, 0, full & ~1ULL, full & ~1ULL, 1ULL << 10));
  status("after capset");
  res("capset v1 (lower 32 only)", capset_(_LINUX_CAPABILITY_VERSION_1, 0, 0xfffffffe, 0xfffffffe, 0));
  status("after v1");
}

static void t_inh(void) {
  // bounding の外へは広げられない。CAP_SETPCAP がなければ、inheritable は inheritable と permitted の中だけ
  res("PR_CAPBSET_DROP NET_BROADCAST", prctl(PR_CAPBSET_DROP, 11, 0, 0, 0));
  res("  inh NET_BROADCAST (not in bset)", capset_(_LINUX_CAPABILITY_VERSION_3, 0, full, full, 1ULL << 11));
  uint64_t e = full & ~(1ULL << 8) & ~(1ULL << 13);
  res("capset eff without SETPCAP", capset_(_LINUX_CAPABILITY_VERSION_3, 0, e, full & ~(1ULL << 13), 0));
  res("  inh NET_RAW (not in prm)", capset_(_LINUX_CAPABILITY_VERSION_3, 0, e, full & ~(1ULL << 13), 1ULL << 13));
  res("  inh NET_ADMIN (in prm)", capset_(_LINUX_CAPABILITY_VERSION_3, 0, e, full & ~(1ULL << 13), 1ULL << 12));
  res("  inh NET_ADMIN kept, prm drops it", capset_(_LINUX_CAPABILITY_VERSION_3, 0, e & ~(1ULL << 12), full & ~(1ULL << 13) & ~(1ULL << 12), 1ULL << 12));
  status("inh");
}

static void t_dac(void) {
  int fd = open(file, O_RDWR);
  printf("%-48s %s\n", "open 0000 file as root", fd >= 0 ? "ok" : err(fd));
  if (fd >= 0)
    close(fd);
  capset_(_LINUX_CAPABILITY_VERSION_3, 0, full & ~2ULL, full, 0);
  fd = open(file, O_RDONLY);
  printf("%-48s %s\n", "  without DAC_OVERRIDE: read", fd >= 0 ? "ok" : err(fd));
  if (fd >= 0)
    close(fd);
  fd = open(file, O_WRONLY);
  printf("%-48s %s\n", "  without DAC_OVERRIDE: write", fd >= 0 ? "ok" : err(fd));
  if (fd >= 0)
    close(fd);
  capset_(_LINUX_CAPABILITY_VERSION_3, 0, full & ~6ULL, full, 0);
  fd = open(file, O_RDONLY);
  printf("%-48s %s\n", "  without DAC_READ_SEARCH too: read", fd >= 0 ? "ok" : err(fd));
  if (fd >= 0)
    close(fd);
}

static void t_setuid(void) {
  res("setresuid 1000", setresuid(1000, 1000, 1000));
  status("uid 1000");
  res("  setresuid 0", setresuid(0, 0, 0));
  res("  PR_CAPBSET_DROP NET_RAW", prctl(PR_CAPBSET_DROP, 13, 0, 0, 0));
  res("  capset eff CHOWN", capset_(_LINUX_CAPABILITY_VERSION_3, 0, 1, 0, 0));
}

static void t_keepcaps(void) {
  res("PR_SET_KEEPCAPS 1", prctl(PR_SET_KEEPCAPS, 1, 0, 0, 0));
  res("PR_GET_KEEPCAPS", prctl(PR_GET_KEEPCAPS, 0, 0, 0, 0));
  res("PR_SET_KEEPCAPS 2", prctl(PR_SET_KEEPCAPS, 2, 0, 0, 0));
  res("setresuid 1000 with KEEPCAPS", setresuid(1000, 1000, 1000));
  status("keepcaps");
  res("  capset eff CHOWN", capset_(_LINUX_CAPABILITY_VERSION_3, 0, 1, full, 0));
  res("  chown", chown(file, 1234, -1));
  res("  PR_GET_SECUREBITS", prctl(PR_GET_SECUREBITS, 0, 0, 0, 0));
  exec_show(self, "keepcaps exec");
}

static void t_seteuid(void) {
  res("seteuid 1000", seteuid(1000));
  status("euid 1000");
  res("  seteuid 0", seteuid(0));
  status("euid 0");
  res("setfsuid 1000 (old fsuid)", setfsuid(1000));
  status("fsuid 1000");
  res("  setfsuid 0 (old fsuid)", setfsuid(0));
  status("fsuid 0");
  res("setresuid 0 1000 0", setresuid(0, 1000, 0));
  status("ruid 0 euid 1000");
}

static void t_bset(void) {
  res("PR_CAPBSET_READ NET_RAW", prctl(PR_CAPBSET_READ, 13, 0, 0, 0));
  res("PR_CAPBSET_READ 99", prctl(PR_CAPBSET_READ, 99, 0, 0, 0));
  res("PR_CAPBSET_DROP NET_RAW", prctl(PR_CAPBSET_DROP, 13, 0, 0, 0));
  res("PR_CAPBSET_READ NET_RAW", prctl(PR_CAPBSET_READ, 13, 0, 0, 0));
  res("PR_CAPBSET_DROP 99", prctl(PR_CAPBSET_DROP, 99, 0, 0, 0));
  status("bset -NET_RAW");
  exec_show(self, "bset exec");
  capset_(_LINUX_CAPABILITY_VERSION_3, 0, full, full, 1ULL << 13);
  exec_show(self, "bset+inh exec");
}

static void t_ambient(void) {
  res("AMBIENT_IS_SET NBS", prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_IS_SET, 10, 0, 0));
  res("AMBIENT_RAISE NBS (not in inh)", prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_RAISE, 10, 0, 0));
  res("capset inh NBS", capset_(_LINUX_CAPABILITY_VERSION_3, 0, full, full, 1ULL << 10));
  res("AMBIENT_RAISE NBS", prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_RAISE, 10, 0, 0));
  res("AMBIENT_IS_SET NBS", prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_IS_SET, 10, 0, 0));
  status("ambient");
  res("AMBIENT_LOWER NBS", prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_LOWER, 10, 0, 0));
  res("AMBIENT_IS_SET NBS", prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_IS_SET, 10, 0, 0));
  res("AMBIENT_RAISE NBS", prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_RAISE, 10, 0, 0));
  res("capset inh 0 (drops ambient)", capset_(_LINUX_CAPABILITY_VERSION_3, 0, full, full, 0));
  res("AMBIENT_IS_SET NBS", prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_IS_SET, 10, 0, 0));
  capset_(_LINUX_CAPABILITY_VERSION_3, 0, full, full, 1ULL << 10);
  prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_RAISE, 10, 0, 0);
  res("AMBIENT_CLEAR_ALL", prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_CLEAR_ALL, 0, 0, 0));
  res("AMBIENT_IS_SET NBS", prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_IS_SET, 10, 0, 0));
  res("AMBIENT_CLEAR_ALL arg3 1", prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_CLEAR_ALL, 1, 0, 0));
  res("AMBIENT_RAISE 99", prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_RAISE, 99, 0, 0));
  res("AMBIENT op 9", prctl(PR_CAP_AMBIENT, 9, 10, 0, 0));
}

static void t_ambient_exec(void) {
  prctl(PR_SET_KEEPCAPS, 1, 0, 0, 0);
  capset_(_LINUX_CAPABILITY_VERSION_3, 0, full, full, 1ULL << 10);
  prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_RAISE, 10, 0, 0);
  res("setresuid 1000 (ambient)", setresuid(1000, 1000, 1000));
  res("  AMBIENT_IS_SET NBS", prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_IS_SET, 10, 0, 0));
  res("  AMBIENT_RAISE NBS", prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_RAISE, 10, 0, 0));
  status("ambient 1000");
  exec_show(self, "ambient exec");
  res("  PR_SET_SECUREBITS NO_CAP_AMBIENT_RAISE", prctl(PR_SET_SECUREBITS, 1 << 6, 0, 0, 0));
}

static void t_ambient_noraise(void) {
  capset_(_LINUX_CAPABILITY_VERSION_3, 0, full, full, 1ULL << 10);
  res("PR_SET_SECUREBITS NO_CAP_AMBIENT_RAISE", prctl(PR_SET_SECUREBITS, 1 << 6, 0, 0, 0));
  res("  AMBIENT_RAISE NBS", prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_RAISE, 10, 0, 0));
}

static void t_secbits(void) {
  res("PR_GET_SECUREBITS", prctl(PR_GET_SECUREBITS, 0, 0, 0, 0));
  res("PR_SET_SECUREBITS 1<<20", prctl(PR_SET_SECUREBITS, 1 << 20, 0, 0, 0));
  res("PR_SET_SECUREBITS NOROOT", prctl(PR_SET_SECUREBITS, 1, 0, 0, 0));
  res("PR_GET_SECUREBITS", prctl(PR_GET_SECUREBITS, 0, 0, 0, 0));
  exec_show(self, "noroot exec");
  res("PR_SET_SECUREBITS NOROOT|NOROOT_LOCKED", prctl(PR_SET_SECUREBITS, 3, 0, 0, 0));
  res("PR_SET_SECUREBITS 0 (locked)", prctl(PR_SET_SECUREBITS, 0, 0, 0, 0));
  res("PR_SET_SECUREBITS 3 (same)", prctl(PR_SET_SECUREBITS, 3, 0, 0, 0));
  res("PR_SET_KEEPCAPS 1", prctl(PR_SET_KEEPCAPS, 1, 0, 0, 0));
  res("PR_GET_SECUREBITS", prctl(PR_GET_SECUREBITS, 0, 0, 0, 0));
}

// ---- ファイルの capability ----

struct vfs2 {
  uint32_t magic;
  uint32_t d[4];
};
struct vfs3 {
  uint32_t magic;
  uint32_t d[4];
  uint32_t rootid;
};
static char fc[300], fu[300];

static void show_xattr(const char *what, const char *path) {
  unsigned char b[64];
  long r = getxattr(path, "security.capability", b, sizeof b);
  printf("%-48s %s", what, err(r));
  for (long i = 0; i < r; i++)
    printf("%s%02x", i % 4 ? "" : " ", b[i]);
  printf("\n");
}

static void t_file_nonroot(void) {
  setresuid(1000, 1000, 1000);
  exec_show(fc, "file caps exec");
}

static void t_file_nnp(void) {
  setresuid(1000, 1000, 1000);
  prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
  exec_show(fc, "file caps nnp exec");
}

static void t_file_bset(void) {
  prctl(PR_CAPBSET_DROP, 10, 0, 0, 0);
  setresuid(1000, 1000, 1000);
  exec_show(fc, "file caps, not in bset");
}

static void t_file_root(void) { exec_show(fc, "file caps root exec"); }

static void t_file_set_nonroot(void) {
  setresuid(1000, 1000, 1000);
  struct vfs2 v = {VFS_CAP_REVISION_2 | VFS_CAP_FLAGS_EFFECTIVE, {1 << 10, 0, 0, 0}};
  res("setxattr capability by uid 1000 (own file)", setxattr(fu, "security.capability", &v, sizeof v, 0));
  res("setxattr user.x by uid 1000 (own file)", setxattr(fu, "user.x", "1", 1, 0));
}

static void t_file(void) {
  int in = open(self, O_RDONLY), out = open(fc, O_CREAT | O_TRUNC | O_WRONLY, 0755);
  char b[65536];
  ssize_t n;
  while (in >= 0 && out >= 0 && (n = read(in, b, sizeof b)) > 0)
    write(out, b, n);
  close(in);
  close(out);
  chmod(fc, 0755);
  show_xattr("no capability", fc);
  struct vfs2 v = {VFS_CAP_REVISION_2 | VFS_CAP_FLAGS_EFFECTIVE, {1 << 10, 0, 0, 0}};
  res("setxattr v2 eff NBS", setxattr(fc, "security.capability", &v, sizeof v, 0));
  show_xattr("  getxattr", fc);
  res("  size 0 getxattr", getxattr(fc, "security.capability", NULL, 0));
  res("  getxattr size 4", getxattr(fc, "security.capability", b, 4));
  child(t_file_nonroot);
  child(t_file_nnp);
  child(t_file_bset);
  child(t_file_root);
  v.magic = VFS_CAP_REVISION_2;
  res("setxattr v2 NBS, no effective bit", setxattr(fc, "security.capability", &v, sizeof v, 0));
  child(t_file_nonroot);
  res("setxattr 19 bytes", setxattr(fc, "security.capability", &v, 19, 0));
  struct vfs2 bad = {0x05000000, {1 << 10, 0, 0, 0}};
  res("setxattr revision 5", setxattr(fc, "security.capability", &bad, sizeof bad, 0));
  struct vfs3 v3 = {VFS_CAP_REVISION_3 | VFS_CAP_FLAGS_EFFECTIVE, {1 << 13, 0, 0, 0}, 0};
  res("setxattr v3 eff NET_RAW rootid 0", setxattr(fc, "security.capability", &v3, sizeof v3, 0));
  show_xattr("  getxattr", fc);
  child(t_file_nonroot);
  v3.rootid = 1000;
  res("setxattr v3 rootid 1000", setxattr(fc, "security.capability", &v3, sizeof v3, 0));
  show_xattr("  getxattr", fc);
  child(t_file_nonroot);
  v.magic = VFS_CAP_REVISION_2 | VFS_CAP_FLAGS_EFFECTIVE;
  setxattr(fc, "security.capability", &v, sizeof v, 0);
  int fd = open(fc, O_WRONLY | O_APPEND);
  res("write one byte", write(fd, "", 1));
  close(fd);
  show_xattr("  after write", fc);
  setxattr(fc, "security.capability", &v, sizeof v, 0);
  res("chown to 0 0", chown(fc, 0, 0));
  show_xattr("  after chown", fc);
  setxattr(fc, "security.capability", &v, sizeof v, 0);
  res("chmod 0700", chmod(fc, 0700));
  show_xattr("  after chmod", fc);
  res("removexattr", removexattr(fc, "security.capability"));
  show_xattr("  after removexattr", fc);
  close(open(fu, O_CREAT | O_WRONLY, 0644));
  chown(fu, 1000, 1000);
  child(t_file_set_nonroot);
  unlink(fu);
  unlink(fc);
}

int main(int argc, char **argv) {
  setvbuf(stdout, NULL, _IONBF, 0);
  struct __user_cap_data_struct d[2];
  if (argc > 2 && !strcmp(argv[1], "--show")) {
    full = (1ULL << (CAP_LAST_CAP + 1)) - 1;
    status(argv[2]);
    printf("%-14s securebits %d keepcaps %d\n", argv[2], prctl(PR_GET_SECUREBITS, 0, 0, 0, 0), prctl(PR_GET_KEEPCAPS, 0, 0, 0, 0));
    return 0;
  }
  if (geteuid() != 0) {
    printf("caps: needs root\n");
    return 0;
  }
  memset(d, 0, sizeof d);
  capget_(_LINUX_CAPABILITY_VERSION_3, 0, d, NULL);
  full = d[0].permitted | (uint64_t)d[1].permitted << 32;
  printf("full set is CAP_LAST_CAP %d\n", 63 - __builtin_clzll(full));
  ssize_t n = readlink("/proc/self/exe", self, sizeof self - 1);
  if (n <= 0)
    return 1;
  self[n] = 0;
  snprintf(base, sizeof base, "%s/capsXXXXXX", argc > 1 ? argv[1] : "/tmp");
  if (!mkdtemp(base))
    return 1;
  chmod(base, 0755);
  snprintf(file, sizeof file, "%s/f", base);
  close(open(file, O_CREAT | O_WRONLY, 0));
  chmod(file, 0);
  snprintf(fc, sizeof fc, "%s/fc", base);
  snprintf(fu, sizeof fu, "%s/fu", base);
  child(t_root);
  child(t_capset);
  child(t_inh);
  child(t_dac);
  child(t_setuid);
  child(t_keepcaps);
  child(t_seteuid);
  child(t_bset);
  child(t_ambient);
  child(t_ambient_exec);
  child(t_ambient_noraise);
  child(t_secbits);
  t_file();
  unlink(file);
  rmdir(base);
  return 0;
}

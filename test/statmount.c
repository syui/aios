// statmount と listmount (Linux 6.8、7.4 の形) が Linux と同じか (root で)。自分で作ったマウント (tmpfs の base、その下の
// a (nosuid,nodev)、a の下の b、a/d を bind した c、ro の r) を見る。番号は statx の STATX_MNT_ID_UNIQUE、mountinfo の番号、
// stat の st_dev とくらべて同じかだけを出す。statmount: たしかめる順 (flags、mnt_id_req の大きさ E2BIG / EINVAL / EFAULT、
// 番号 EINVAL / ENOENT、入れ物が小さい EOVERFLOW)、各マスクの答え (ファイルシステム、マウントの印、根、マウント先、種類、
// オプション、もと)、STATMOUNT_BY_FD (はなれたマウントはマウント先と namespace なし)、mnt_ns_fd / mnt_ns_id。listmount:
// 下にあるマウントを番号の順に (LISTMOUNT_REVERSE は逆)、param から続き、たしかめる順
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/sysmacros.h>
#include <sys/wait.h>
#include <unistd.h>

struct sm {
  uint32_t size, mnt_opts;
  uint64_t mask;
  uint32_t sb_dev_major, sb_dev_minor;
  uint64_t sb_magic;
  uint32_t sb_flags, fs_type;
  uint64_t mnt_id, mnt_parent_id;
  uint32_t mnt_id_old, mnt_parent_id_old;
  uint64_t mnt_attr, mnt_propagation, mnt_peer_group, mnt_master, propagate_from;
  uint32_t mnt_root, mnt_point;
  uint64_t mnt_ns_id;
  uint32_t fs_subtype, sb_source, opt_num, opt_array, opt_sec_num, opt_sec_array;
  uint64_t supported_mask;
  uint32_t mnt_uidmap_num, mnt_uidmap, mnt_gidmap_num, mnt_gidmap;
  uint64_t spare[43];
  char str[];
};
struct req {
  uint32_t size, fd;
  uint64_t mnt_id, param, mnt_ns_id;
};

#define ALL 0x1fff // SB_BASIC から SUPPORTED_MASK まで (uidmap / gidmap なし)
#define OFF (1ull << 31)
#define LSMT_ROOT 0xffffffffffffffffull
#define BY_FD 1
#define BASE "/tmp/smt"

static char buf[16384];
static struct sm *s = (struct sm *)buf;

static long stm(uint64_t id, uint64_t mask, size_t size, unsigned fl) {
  struct req r = {32, 0, id, mask, 0};
  long x = syscall(457, &r, s, size, fl);
  return x < 0 ? -errno : x;
}
static long stm_req(struct req *r, size_t size, unsigned fl) {
  long x = syscall(457, r, s, size, fl);
  return x < 0 ? -errno : x;
}
static long lsm(uint64_t id, uint64_t last, uint64_t *ids, size_t n, unsigned fl, uint64_t ns) {
  struct req r = {32, 0, id, last, ns};
  long x = syscall(458, &r, ids, n, fl);
  return x < 0 ? -errno : x;
}
static uint64_t uid_of(const char *p) {
  struct statx x;
  return statx(AT_FDCWD, p, 0, 0x4000, &x) ? 0 : x.stx_mnt_id;
}
// mountinfo のマウント先 p の行の (番号, 親の番号)
static void old_of(const char *p, unsigned *id, unsigned *parent) {
  FILE *f = fopen("/proc/self/mountinfo", "r");
  char l[1024], pat[256];
  snprintf(pat, sizeof pat, " %s ", p);
  *id = *parent = 0;
  while (f && fgets(l, sizeof l, f))
    if (strstr(l, pat))
      sscanf(l, "%u %u", id, parent);
  if (f)
    fclose(f);
}
static const char *str(uint64_t bit, uint32_t off) { return s->mask & bit ? s->str + off : "-"; }

static const char *names[8];
static uint64_t ids[8];
static int nn;
static const char *name_of(uint64_t id) {
  for (int i = 0; i < nn; i++)
    if (ids[i] == id)
      return names[i];
  return "?";
}

static void show(const char *p, const char *parent) {
  uint64_t id = uid_of(p);
  long r = stm(id, ALL, sizeof buf, 0);
  if (r) {
    printf("%s: statmount %ld\n", p, r);
    return;
  }
  struct stat st;
  stat(p, &st);
  unsigned old, oldp;
  old_of(p, &old, &oldp);
  printf("%s: mask %#llx, magic %#llx, sb_flags %u, dev %d, id %d, parent %d, old ids %d %d, attr %#llx, propagation %#llx, peer %llu, master %llu, from %llu\n", p,
         (unsigned long long)s->mask, (unsigned long long)s->sb_magic, s->sb_flags, makedev(s->sb_dev_major, s->sb_dev_minor) == st.st_dev, s->mnt_id == id,
         s->mnt_parent_id == uid_of(parent), s->mnt_id_old == old, s->mnt_parent_id_old == oldp, (unsigned long long)s->mnt_attr, (unsigned long long)s->mnt_propagation,
         (unsigned long long)s->mnt_peer_group, (unsigned long long)s->mnt_master, (unsigned long long)s->propagate_from);
  printf("  type %s, root %s, point %s, opts %s, subtype %s, source %s, opt_num %u [", str(0x20, s->fs_type), str(0x8, s->mnt_root), str(0x10, s->mnt_point),
         str(0x80, s->mnt_opts), str(0x100, s->fs_subtype), str(0x200, s->sb_source), s->opt_num);
  const char *o = s->mask & 0x400 ? s->str + s->opt_array : "";
  for (unsigned i = 0; i < s->opt_num; i++, o += strlen(o) + 1)
    printf("%s%s", i ? " " : "", o);
  printf("], sec %u, supported %#llx, size %u\n", s->opt_sec_num, (unsigned long long)s->supported_mask, s->size);
}

static void list(const char *what, uint64_t id, uint64_t last, size_t n, unsigned fl) {
  uint64_t got[16];
  long r = lsm(id, last, got, n, fl, 0);
  printf("%s: %ld [", what, r);
  for (long i = 0; i < r; i++)
    printf("%s%s", i ? " " : "", name_of(got[i]));
  printf("]\n");
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  if (getuid() != 0) {
    printf("statmount: not root, skipped\n");
    return 0;
  }
  mkdir(BASE, 0755);
  if (mount("tmpfs", BASE, "tmpfs", 0, "size=1m,mode=755"))
    return 1;
  mkdir(BASE "/a", 0755);
  mkdir(BASE "/c", 0755);
  mkdir(BASE "/r", 0755);
  mount("tmpfs", BASE "/a", "tmpfs", MS_NOSUID | MS_NODEV, NULL);
  mkdir(BASE "/a/b", 0755);
  mkdir(BASE "/a/d", 0755);
  mount("tmpfs", BASE "/a/b", "tmpfs", 0, "nr_inodes=100");
  mount(BASE "/a/d", BASE "/c", NULL, MS_BIND, NULL);
  mount("tmpfs", BASE "/r", "tmpfs", MS_RDONLY, NULL);
  const char *ps[] = {BASE, BASE "/a", BASE "/a/b", BASE "/c", BASE "/r"};
  const char *ns_[] = {"base", "a", "b", "c", "r"};
  for (nn = 0; nn < 5; nn++) {
    ids[nn] = uid_of(ps[nn]);
    names[nn] = ns_[nn];
  }

  show(BASE, "/tmp");
  show(BASE "/a", BASE);
  show(BASE "/a/b", BASE "/a");
  show(BASE "/c", BASE);
  show(BASE "/r", BASE);

  // たしかめる順
  uint64_t a = ids[1];
  struct req big = {8192, 0, a, ALL, 0}, small = {16, 0, a, ALL, 0}, v0 = {24, 0, a, 0x2, 0};
  printf("statmount checks: flags %ld, req too big %ld, too small %ld, bad req %ld, ver0 %ld, id 0 %ld, id offset %ld, no such %ld\n", stm(a, ALL, sizeof buf, 2),
         stm_req(&big, sizeof buf, 0), stm_req(&small, sizeof buf, 0), (long)(syscall(457, (void *)8, s, sizeof buf, 0) < 0 ? -errno : 0), stm_req(&v0, sizeof buf, 0),
         stm(0, ALL, sizeof buf, 0), stm(OFF, ALL, sizeof buf, 0), stm(OFF + 999999, ALL, sizeof buf, 0));
  long r1 = stm(a, 0x2, 64, 0);
  printf("  basic in 64 bytes %ld (size %u), strings in 512 %ld, in 520 %ld, bad buf %ld, nothing asked %ld (mask %#llx)\n", r1, s->size, stm(a, ALL, 512, 0),
         stm(a, ALL, 520, 0), (long)(syscall(457, &(struct req){32, 0, a, ALL, 0}, (void *)8, sizeof buf, 0) < 0 ? -errno : 0), stm(a, 0, sizeof buf, 0),
         (unsigned long long)s->mask);
  // fd から
  int fd = open(BASE "/a/b", O_PATH);
  struct req byfd = {32, (uint32_t)fd, 0, 0x12, 0}, byfd_id = {32, (uint32_t)fd, a, 0x12, 0}, byfd_bad = {32, 999, 0, 0x12, 0};
  long r2 = stm_req(&byfd, sizeof buf, BY_FD);
  printf("by fd: %ld, it is b %d, point %s, with an id %ld, bad fd %ld\n", r2, s->mnt_id == ids[2], str(0x10, s->mnt_point), stm_req(&byfd_id, sizeof buf, BY_FD),
         stm_req(&byfd_bad, sizeof buf, BY_FD));
  close(fd);
  fd = syscall(SYS_open_tree, AT_FDCWD, BASE "/a", 1 /* OPEN_TREE_CLONE */);
  byfd.fd = fd;
  long r3 = stm_req(&byfd, sizeof buf, BY_FD);
  printf("  detached: %ld, mask %#llx, new id %d\n", r3, (unsigned long long)s->mask, s->mnt_id != ids[1] && s->mnt_id > OFF);
  struct req bf2 = {32, (uint32_t)fd, 0, 0x40, 0};
  printf("  detached ns id: %ld, mask %#llx\n", stm_req(&bf2, sizeof buf, BY_FD), (unsigned long long)s->mask);
  close(fd);

  // namespace
  stm(a, 0x40, sizeof buf, 0);
  uint64_t myns = s->mnt_ns_id;
  stm(ids[0], 0x40, sizeof buf, 0);
  int nsfd = open("/proc/self/ns/mnt", O_RDONLY);
  struct req withfd = {32, (uint32_t)nsfd, a, 0x2, 0}, both = {32, (uint32_t)nsfd, a, 0x2, myns}, notns = {32, 0, a, 0x2, 0}, withid = {32, 0, a, 0x2, myns},
             badid = {32, 0, a, 0x2, 12345};
  int nf = open("/proc/self/stat", O_RDONLY);
  notns.fd = nf;
  printf("ns: same for base and a %d, ns fd %ld, fd and id %ld, not an ns fd %ld, ns id %ld, unknown ns id %ld\n", s->mnt_ns_id == myns && myns != 0,
         stm_req(&withfd, sizeof buf, 0), stm_req(&both, sizeof buf, 0), stm_req(&notns, sizeof buf, 0), stm_req(&withid, sizeof buf, 0), stm_req(&badid, sizeof buf, 0));
  pid_t c = fork();
  if (c == 0) {
    if (unshare(CLONE_NEWNS))
      _exit(1);
    long x = stm(a, 0x42, sizeof buf, 0);
    struct req old_ns = {32, 0, a, 0x42, myns};
    long y = stm_req(&old_ns, sizeof buf, 0);
    uint64_t got_ns = s->mnt_ns_id;
    long z = stm(uid_of(BASE "/a"), 0x42, sizeof buf, 0);
    printf("new mount ns: old id %ld, with the old ns id %ld (that ns %d), the copy %ld (new ns %d, new id %d)\n", x, y, got_ns == myns, z, s->mnt_ns_id != myns,
           s->mnt_id != a);
    struct req nsw = {32, (uint32_t)nsfd, LSMT_ROOT, 0, 0};
    uint64_t got[256];
    long l1 = syscall(458, &nsw, got, 256, 0);
    int has_a = 0;
    for (long i = 0; i < l1; i++)
      has_a += got[i] == a;
    printf("  listmount of the old ns by fd: %d, has a %d\n", l1 > 5, has_a);
    _exit(0);
  }
  waitpid(c, NULL, 0);
  close(nsfd);
  close(nf);

  // listmount
  list("list base", ids[0], 0, 16, 0);
  list("list base reverse", ids[0], 0, 16, 1);
  list("list a", ids[1], 0, 16, 0);
  list("list base first 2", ids[0], 0, 2, 0);
  list("list base after a", ids[0], ids[1], 16, 0);
  list("list base reverse before c", ids[0], ids[3], 16, 1);
  list("list b", ids[2], 0, 16, 0);
  uint64_t got[4];
  printf("listmount checks: flags %ld, too many %ld, bad last %ld, no such %ld, unknown ns %ld, zero %ld, bad ids %ld\n", lsm(ids[0], 0, got, 4, 2, 0),
         lsm(ids[0], 0, got, 2000000, 0, 0), lsm(ids[0], 5, got, 4, 0, 0), lsm(OFF + 999999, 0, got, 4, 0, 0), lsm(ids[0], 0, got, 4, 0, 12345),
         lsm(ids[0], 0, got, 0, 0, 0), lsm(ids[0], 0, (uint64_t *)8, 4, 0, 0));
  uint64_t all[256];
  long n = lsm(LSMT_ROOT, 0, all, 256, 0, 0);
  int has[5] = {0}, sorted = 1;
  for (long i = 0; i < n; i++) {
    for (int k = 0; k < 5; k++)
      has[k] += all[i] == ids[k];
    if (i && all[i] <= all[i - 1])
      sorted = 0;
  }
  printf("list root: some %d, sorted %d, ours %d%d%d%d%d, root itself %d\n", n > 5, sorted, has[0], has[1], has[2], has[3], has[4], n > 0 && all[0] == uid_of("/"));

  umount2(BASE, MNT_DETACH);
  rmdir(BASE);
  return 0;
}

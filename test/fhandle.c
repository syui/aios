// name_to_handle_at と open_by_handle_at が Linux と同じか (引数のディレクトリで: ext4 と tmpfs)。ハンドルの大きさと種類
// (ext4 は 8 バイトの FILEID_INO32_GEN、tmpfs は 12 バイト)、小さければ EOVERFLOW で要る大きさ、mount_id は statx の
// stx_mnt_id、同じファイルは同じハンドル、flags と handle_bytes のたしかめ、/proc は EOPNOTSUPP。open_by_handle_at は
// CAP_DAC_READ_SEARCH がなければ EPERM、root なら開けて同じ inode、消したものは ESTALE
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

struct fh {
  unsigned int bytes;
  int type;
  unsigned char f[128];
};

static long n2h(int dfd, const char *p, struct fh *h, int *mid, int fl) {
  return name_to_handle_at(dfd, p, (struct file_handle *)h, mid, fl) < 0 ? -errno : 0;
}
static long obh(int mfd, struct fh *h, int fl) {
  int r = open_by_handle_at(mfd, (struct file_handle *)h, fl);
  return r < 0 ? -errno : r;
}
// 開けたら 0 (fd の番号は見ない)
static long ok(long r) {
  if (r >= 0)
    close(r);
  return r < 0 ? r : 0;
}
static unsigned long long mnt_of(const char *p) {
  struct statx s;
  return statx(AT_FDCWD, p, 0, STATX_MNT_ID, &s) ? 0 : s.stx_mnt_id;
}

static void one(const char *dir) {
  char a[256], b[256], d[256], l[256];
  snprintf(a, sizeof a, "%s/fh-a", dir);
  snprintf(b, sizeof b, "%s/fh-b", dir);
  snprintf(d, sizeof d, "%s/fh-d", dir);
  snprintf(l, sizeof l, "%s/fh-l", dir);
  unlink(a), unlink(b), rmdir(d), unlink(l);
  int fd = open(a, O_CREAT | O_WRONLY, 0644);
  if (fd < 0 || write(fd, "hello", 5) != 5)
    return;
  close(fd);
  close(open(b, O_CREAT | O_WRONLY, 0644));
  mkdir(d, 0755);
  if (symlink("fh-a", l))
    return;
  struct fh h = {0}, h2 = {0}, hb = {0};
  int mid = -1, mid2 = -1;
  long r = n2h(AT_FDCWD, a, &h, &mid, 0);
  printf("%s: small (%ld, needs %u, type %d)\n", strncmp(dir, "/tmp", 4) ? "ext4" : "tmpfs", r, h.bytes, h.type);
  h.bytes = 128;
  r = n2h(AT_FDCWD, a, &h, &mid, 0);
  h2.bytes = 128;
  long r2 = n2h(AT_FDCWD, a, &h2, &mid2, 0);
  hb.bytes = 128;
  long rb = n2h(AT_FDCWD, b, &hb, &mid2, 0);
  printf("  file (%ld, bytes %u, type %d, mount id %d, same twice %d, other differs %d)\n", r, h.bytes, h.type, (unsigned long long)mid == mnt_of(a), r2 == 0 && !memcmp(&h, &h2, 8 + h.bytes),
         rb == 0 && memcmp(h.f, hb.f, h.bytes) != 0);
  struct fh hd = {128}, hl = {128}, hf = {128}, hfid = {128}, he = {128};
  long rd = n2h(AT_FDCWD, d, &hd, &mid2, 0);
  long rl = n2h(AT_FDCWD, l, &hl, &mid2, 0);
  long rf = n2h(AT_FDCWD, l, &hf, &mid2, AT_SYMLINK_FOLLOW);
  printf("  dir (%ld, bytes %u), link (%ld, not the file %d), followed (%ld, the file %d)\n", rd, hd.bytes, rl, memcmp(hl.f, h.f, h.bytes) != 0, rf, !memcmp(hf.f, h.f, h.bytes));
  long rfid = n2h(AT_FDCWD, a, &hfid, &mid2, 0x200);
  fd = open(a, O_RDONLY);
  long re = n2h(fd, "", &he, &mid2, AT_EMPTY_PATH);
  close(fd);
  printf("  fid (%ld, bytes %u, type %d), empty path (%ld, the file %d)\n", rfid, hfid.bytes, hfid.type, re, !memcmp(he.f, h.f, h.bytes));
  struct fh hc = {128}, hcd = {128};
  long rc = n2h(AT_FDCWD, a, &hc, &mid2, 0x2);
  long rcd = n2h(AT_FDCWD, d, &hcd, &mid2, 0x2);
  printf("  connectable (file %ld, bytes %u, type %#x, dir %ld, bytes %u, type %#x, with fid %ld)\n", rc, hc.bytes, hc.type, rcd, hcd.bytes, hcd.type, n2h(AT_FDCWD, a, &hc, &mid2, 0x202));
  struct fh hx = {129};
  long rx = n2h(AT_FDCWD, a, &hx, &mid2, 0);
  struct fh hy = {128};
  long ry = n2h(AT_FDCWD, a, &hy, &mid2, 0x4);
  struct fh hz = {128};
  long rz = n2h(AT_FDCWD, "/nonexistent", &hz, &mid2, 0);
  printf("  checks (129 bytes %ld, bad flag %ld, no such %ld, bad handle ptr %ld)\n", rx, ry, rz, n2h(AT_FDCWD, a, (struct fh *)8, &mid2, 0));
  int m = open(dir, O_RDONLY | O_DIRECTORY);
  long o = obh(m, &h, O_RDONLY);
  struct stat s1, s2;
  stat(a, &s1);
  if (o >= 0) {
    char buf[8] = {0};
    fstat(o, &s2);
    printf("  open by handle (ok, same inode %d, reads %ld %s)\n", s1.st_ino == s2.st_ino, (long)read(o, buf, 5), buf);
    close(o);
  } else {
    printf("  open by handle %ld\n", o);
  }
  char in[300];
  snprintf(in, sizeof in, "%s/inner", d);
  close(open(in, O_CREAT | O_WRONLY, 0644));
  long od = obh(m, &hd, O_RDONLY | O_DIRECTORY);
  if (od >= 0) {
    struct stat sd;
    fstat(od, &sd);
    char fl[64], lk[4096] = {0}, real[4096] = {0};
    snprintf(fl, sizeof fl, "/proc/self/fd/%ld", od);
    ssize_t n = readlink(fl, lk, sizeof lk - 1);
    if (n > 0)
      lk[n] = 0;
    if (!realpath(d, real))
      real[0] = 0;
    int inner = openat(od, "inner", O_RDONLY);
    printf("  open dir by handle (ok, dir %d, link is the dir %d, openat inside %d)\n", S_ISDIR(sd.st_mode), !strcmp(lk, real), inner >= 0);
    if (inner >= 0)
      close(inner);
    close(od);
  } else {
    printf("  open dir by handle %ld\n", od);
  }
  // 名前を変えたあとのディレクトリ: パスは新しいもの (覚えのパスは古い)
  char n1[300], n2[300], n3[300], n1b[300], n3b[300];
  snprintf(n1, sizeof n1, "%s/fh-n", dir);
  snprintf(n2, sizeof n2, "%s/fh-n/x", dir);
  snprintf(n3, sizeof n3, "%s/fh-n/x/y", dir);
  snprintf(n1b, sizeof n1b, "%s/fh-m", dir);
  snprintf(n3b, sizeof n3b, "%s/fh-m/x/y", dir);
  mkdir(n1, 0755), mkdir(n2, 0755), mkdir(n3, 0755);
  struct fh hn = {128};
  long rn = n2h(AT_FDCWD, n3, &hn, &mid2, 0);
  long rr = rename(n1, n1b) ? -errno : 0;
  long on = obh(m, &hn, O_RDONLY | O_DIRECTORY);
  if (on >= 0) {
    char fl[64], lk[4096] = {0}, real[4096] = {0};
    snprintf(fl, sizeof fl, "/proc/self/fd/%ld", on);
    ssize_t n = readlink(fl, lk, sizeof lk - 1);
    if (n > 0)
      lk[n] = 0;
    if (!realpath(n3b, real))
      real[0] = 0;
    printf("  renamed dir (%ld, %ld, link is the new path %d)\n", rn, rr, !strcmp(lk, real));
    close(on);
  } else {
    printf("  renamed dir (%ld, %ld, open %ld)\n", rn, rr, on);
  }
  rmdir(n3b);
  snprintf(n3b, sizeof n3b, "%s/fh-m/x", dir);
  rmdir(n3b), rmdir(n1b);
  struct fh bad = h;
  bad.bytes = 0;
  struct fh badt = h;
  badt.type = 0x77;
  printf("  open checks (zero bytes %ld, bad type %ld, not a fd %ld)\n", ok(obh(m, &bad, O_RDONLY)), ok(obh(m, &badt, O_RDONLY)), ok(obh(-1, &h, O_RDONLY)));
  unlink(a);
  // tmpfs の消したものは、Linux では dcache に残っているかしだい (開けることがある) なので ext4 だけ
  if (strncmp(dir, "/tmp", 4))
    printf("  open removed %ld\n", ok(obh(m, &h, O_RDONLY)));
  close(m);
  unlink(in), unlink(b), rmdir(d), unlink(l);
}

int main(int argc, char **argv) {
  setvbuf(stdout, NULL, _IONBF, 0);
  for (int i = 1; i < argc; i++)
    one(argv[i]);
  struct fh h = {128};
  int mid;
  printf("proc %ld, proc fid %ld\n", n2h(AT_FDCWD, "/proc/self/stat", &h, &mid, 0), n2h(AT_FDCWD, "/proc/self/stat", &h, &mid, 0x200));
  return 0;
}

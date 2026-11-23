// perf の MMAP2 の build id (attr.build_id: PERF_RECORD_MISC_MMAP_BUILD_ID) が Linux と同じか:
//   elf     実行できるように写した ELF (このテストのプログラム) は、maj / min / ino のかわりに build id の大きさと 20 バイト
//           (ファイルのはじめから読む: 写したところがはじめでなくても)
//   other   無名のもの、ELF でないファイルは、ふつうの maj / min / ino (印なし)
//   nomap2  build_id だけ (mmap2 なし) は、ふつうの MMAP
// 1 行ずつ出す (aios と Linux で同じか、test/ldiff.sh でくらべる)。終わりの番号は思ったものでなかった数
#define _GNU_SOURCE
#include <elf.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/perf_event.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

static int bad;
static long PG;

static long perf_open(struct perf_event_attr *a) {
  long r = syscall(SYS_perf_event_open, a, 0, -1, -1, PERF_FLAG_FD_CLOEXEC);
  return r < 0 ? -errno : r;
}

struct rec {
  uint32_t type;
  uint16_t misc, size;
  unsigned char body[512];
};
static int records(struct perf_event_mmap_page *h, struct rec *out, int max) {
  uint64_t head = __atomic_load_n(&h->data_head, __ATOMIC_ACQUIRE);
  uint64_t tail = h->data_tail;
  unsigned char *data = (unsigned char *)h + h->data_offset;
  uint64_t size = h->data_size;
  int n = 0;
  while (tail < head && n < max) {
    struct perf_event_header eh;
    for (unsigned i = 0; i < sizeof eh; i++)
      ((unsigned char *)&eh)[i] = data[(tail + i) % size];
    if (eh.size < sizeof eh || eh.size > sizeof(struct rec))
      break;
    out[n].type = eh.type;
    out[n].misc = eh.misc;
    out[n].size = eh.size;
    for (unsigned i = 0; i < eh.size - sizeof eh; i++)
      out[n].body[i] = data[(tail + sizeof eh + i) % size];
    n++;
    tail += eh.size;
  }
  __atomic_store_n(&h->data_tail, tail, __ATOMIC_RELEASE);
  return n;
}
static uint64_t u64at(const unsigned char *b, int o) {
  uint64_t v;
  memcpy(&v, b + o, 8);
  return v;
}
static uint32_t u32at(const unsigned char *b, int o) {
  uint32_t v;
  memcpy(&v, b + o, 4);
  return v;
}
// 名前: このプログラムなら self (VM ごとに置き場所と名前がちがう)、ほかはそのまま
static char self_path[256];
static char tmp_path[64];
static const char *base(const char *p) {
  return !strcmp(p, self_path) ? "self" : !strcmp(p, tmp_path) ? "tmpfile" : p;
}

static int open_ev2(int data, int mmap2, int build_id, struct perf_event_mmap_page **h) {
  struct perf_event_attr a;
  memset(&a, 0, sizeof a);
  a.type = PERF_TYPE_SOFTWARE;
  a.size = sizeof a;
  a.config = PERF_COUNT_SW_DUMMY;
  a.sample_type = PERF_SAMPLE_TID;
  a.sample_id_all = 1;
  a.mmap = 1;
  a.mmap_data = data;
  a.mmap2 = mmap2;
  a.build_id = build_id;
  a.exclude_kernel = 1;
  a.exclude_hv = 1;
  int fd = perf_open(&a);
  *h = mmap(0, 9 * PG, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
  return fd;
}


// このプログラムの NT_GNU_BUILD_ID (ファイルのプログラムヘッダの PT_NOTE から)
static unsigned char want[20];
static int want_len;
static void own_build_id(int f) {
  Elf64_Ehdr eh;
  pread(f, &eh, sizeof eh, 0);
  for (int i = 0; i < eh.e_phnum; i++) {
    Elf64_Phdr ph;
    pread(f, &ph, sizeof ph, eh.e_phoff + i * eh.e_phentsize);
    if (ph.p_type != PT_NOTE)
      continue;
    unsigned char buf[4096];
    long n = pread(f, buf, ph.p_filesz < sizeof buf ? ph.p_filesz : sizeof buf, ph.p_offset);
    long o = 0;
    while (o + 12 <= n) {
      uint32_t nsz, dsz, typ;
      memcpy(&nsz, buf + o, 4);
      memcpy(&dsz, buf + o + 4, 4);
      memcpy(&typ, buf + o + 8, 4);
      long d = o + 12 + ((nsz + 3) & ~3);
      if (typ == NT_GNU_BUILD_ID && nsz == 4 && !memcmp(buf + o + 12, "GNU", 4) && dsz <= 20) {
        memcpy(want, buf + d, dsz);
        want_len = dsz;
        return;
      }
      o = d + ((dsz + 3) & ~3);
    }
  }
}

static void show(const char *what, struct rec *r, int n) {
  printf("%s: %d records", what, n);
  for (int i = 0; i < n; i++) {
    const unsigned char *b = r[i].body;
    if (r[i].type == PERF_RECORD_MMAP2) {
      int bid = (r[i].misc & PERF_RECORD_MISC_MMAP_BUILD_ID) != 0;
      printf(" [mmap2 misc %#x", r[i].misc);
      if (bid)
        printf(" build id size %u same %d reserved %u", b[32], b[32] == want_len && !memcmp(b + 36, want, want_len), b[33] | b[34] | b[35]);
      else
        printf(" ino %s", u64at(b, 40) ? "set" : "0");
      printf(" %s]", base((const char *)b + 64));
    } else if (r[i].type == PERF_RECORD_MMAP) {
      printf(" [mmap misc %#x %s]", r[i].misc, base((const char *)b + 32));
    } else {
      printf(" [type %u]", r[i].type);
    }
  }
  printf("\n");
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  PG = sysconf(_SC_PAGESIZE);
  int f = open("/proc/self/exe", O_RDONLY);
  long sl = readlink("/proc/self/exe", self_path, sizeof self_path - 1);
  self_path[sl > 0 ? sl : 0] = 0;
  own_build_id(f);
  printf("self: build id %d bytes\n", want_len);
  static struct rec r[32];
  struct perf_event_mmap_page *h;
  int fd = open_ev2(0, 1, 1, &h);
  printf("open: %s\n", fd >= 0 ? "ok" : strerror(errno));
  records(h, r, 32);
  void *x = mmap(0, PG, PROT_READ | PROT_EXEC, MAP_PRIVATE, f, 0);
  show("elf at 0", r, records(h, r, 32));
  munmap(x, PG);
  x = mmap(0, PG, PROT_READ | PROT_EXEC, MAP_PRIVATE, f, PG);
  show("elf at a page", r, records(h, r, 32));
  munmap(x, PG);
  void *ax = mmap(0, PG, PROT_READ | PROT_EXEC, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  show("anon", r, records(h, r, 32));
  munmap(ax, PG);
  char tp[] = "/tmp/perf6XXXXXX";
  int t = mkstemp(tp);
  strcpy(tmp_path, tp);
  write(t, "not an elf file\n", 16);
  void *tx = mmap(0, PG, PROT_READ | PROT_EXEC, MAP_PRIVATE, t, 0);
  show("not elf", r, records(h, r, 32));
  munmap(tx, PG);
  close(t);
  unlink(tp);
  munmap(h, 9 * PG);
  close(fd);
  // build_id だけ (mmap2 なし)
  fd = open_ev2(0, 0, 1, &h);
  printf("open without mmap2: %s\n", fd >= 0 ? "ok" : strerror(errno));
  x = mmap(0, PG, PROT_READ | PROT_EXEC, MAP_PRIVATE, f, 0);
  show("no mmap2", r, records(h, r, 32));
  munmap(x, PG);
  munmap(h, 9 * PG);
  close(fd);
  // mmap2 だけ (build_id なし)
  fd = open_ev2(0, 1, 0, &h);
  x = mmap(0, PG, PROT_READ | PROT_EXEC, MAP_PRIVATE, f, 0);
  show("mmap2 without build_id", r, records(h, r, 32));
  munmap(x, PG);
  munmap(h, 9 * PG);
  close(fd);
  close(f);
  printf("perf6: %d bad\n", bad);
  return bad;
}

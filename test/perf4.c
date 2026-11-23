// perf の PERF_RECORD_MMAP / MMAP2 (attr.mmap、mmap_data、mmap2: perf record が写したライブラリを知る) が Linux と同じか:
//   exec    実行できるように写したファイル (番地、長さ、pgoff、名前) と無名のもの (//anon)。読むだけのものは出さない
//   mprotect あとから実行できるようにしたもの
//   data    mmap_data なら読み書きのものも (MMAP_DATA の印)
//   mmap2   MMAP2: inode の番号、prot、flags
// pgoff はバイト (Linux の vm_pgoff << PAGE_SHIFT: 無名なら番地)。mmap_data なら perf の輪の写し
// (anon_inode:[perf_event]) も
// 1 行ずつ出す (aios と Linux で同じか、test/ldiff.sh でくらべる)。終わりの番号は思ったものでなかった数
#define _GNU_SOURCE
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
static const char *base(const char *p) {
  return strcmp(p, self_path) ? p : "self";
}

static int open_ev(int data, int mmap2, struct perf_event_mmap_page **h) {
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
  a.exclude_kernel = 1;
  a.exclude_hv = 1;
  int fd = perf_open(&a);
  *h = mmap(0, 9 * PG, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
  return fd;
}

// 記録を 1 行に: 種類、番地と長さが合うか、pgoff、名前 (最後のところ)、印
static void show(const char *what, struct rec *r, int n, void *addr, long len, long pgoff) {
  printf("%s: %d records", what, n);
  for (int i = 0; i < n; i++) {
    const unsigned char *b = r[i].body;
    if (r[i].type == PERF_RECORD_MMAP) {
      const char *name = (const char *)b + 32;
      printf(" [mmap pid %d addr %d len %d pgoff %s %s misc %#x]", u32at(b, 0) == (uint32_t)getpid(), u64at(b, 8) == (uint64_t)addr, u64at(b, 16) == (uint64_t)len,
             u64at(b, 24) == (uint64_t)pgoff ? "ok" : "other", base(name), r[i].misc);
    } else if (r[i].type == PERF_RECORD_MMAP2) {
      const char *name = (const char *)b + 64;
      printf(" [mmap2 addr %d len %d pgoff %s prot %u flags %#x %s misc %#x]", u64at(b, 8) == (uint64_t)addr, u64at(b, 16) == (uint64_t)len, u64at(b, 24) == (uint64_t)pgoff ? "ok" : "other",
             u32at(b, 56), u32at(b, 60) & 0xff, base(name), r[i].misc);
    } else {
      printf(" [type %u]", r[i].type);
    }
  }
  printf("\n");
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  PG = sysconf(_SC_PAGESIZE);
  // 写すファイル (このテストのプログラム: 実行できる)
  int f = open("/proc/self/exe", O_RDONLY);
  long sl = readlink("/proc/self/exe", self_path, sizeof self_path - 1);
  self_path[sl > 0 ? sl : 0] = 0;
  struct stat sb;
  fstat(f, &sb);
  static struct rec r[32];
  struct perf_event_mmap_page *h;
  int fd = open_ev(0, 0, &h);
  records(h, r, 32);
  void *x = mmap(0, 2 * PG, PROT_READ | PROT_EXEC, MAP_PRIVATE, f, PG);
  show("exec file", r, records(h, r, 32), x, 2 * PG, PG);
  void *ax = mmap(0, PG, PROT_READ | PROT_EXEC, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  show("exec anon", r, records(h, r, 32), ax, PG, (long)ax);
  void *d = mmap(0, PG, PROT_READ, MAP_PRIVATE, f, 0);
  show("read only file", r, records(h, r, 32), d, PG, 0);
  void *rw = mmap(0, 3 * PG, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  show("anon rw", r, records(h, r, 32), rw, 3 * PG, (long)rw);
  // あとから実行できるように (まん中の 1 ページ)
  mprotect((char *)rw + PG, PG, PROT_READ | PROT_EXEC);
  show("mprotect exec", r, records(h, r, 32), (char *)rw + PG, PG, (long)rw + PG);
  munmap(x, 2 * PG);
  munmap(ax, PG);
  munmap(d, PG);
  munmap(rw, 3 * PG);
  show("munmap", r, records(h, r, 32), 0, 0, 0);
  munmap(h, 9 * PG);
  close(fd);
  // mmap_data: 読み書きのものも
  fd = open_ev(1, 0, &h);
  d = mmap(0, PG, PROT_READ, MAP_PRIVATE, f, 0);
  show("data file", r, records(h, r, 32), d, PG, 0);
  rw = mmap(0, PG, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  show("data anon", r, records(h, r, 32), rw, PG, (long)rw);
  munmap(d, PG);
  munmap(rw, PG);
  munmap(h, 9 * PG);
  close(fd);
  // MMAP2: maj、min、ino、ino_generation、prot、flags
  fd = open_ev(0, 1, &h);
  x = mmap(0, PG, PROT_READ | PROT_EXEC, MAP_PRIVATE, f, 0);
  int n = records(h, r, 32);
  show("mmap2", r, n, x, PG, 0);
  for (int i = 0; i < n; i++)
    if (r[i].type == PERF_RECORD_MMAP2)
      printf("mmap2: ino %d, dev %d\n", u64at(r[i].body, 40) == (uint64_t)sb.st_ino, ((uint64_t)u32at(r[i].body, 32) << 8 | u32at(r[i].body, 36)) != 0);
  munmap(x, PG);
  munmap(h, 9 * PG);
  close(fd);
  close(f);
  printf("perf4: %d bad\n", bad);
  return bad;
}

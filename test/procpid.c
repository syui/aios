// /proc/self と /proc/self/task/TID のならび (名前、種類、許可) が Linux と同じか。手本の Linux (test/linux.config) に
// 合わせたもの: cgroup、io、wchan などはどちらにもない。stack は aios にはあるが (0400)、手本は CONFIG_STACKTRACE が
// ないのでないため見ない。中身は test/procfiles.c、procknobs.c、procmem.c、rlimit.c
#include <dirent.h>
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

static int cmp(const void *a, const void *b) { return strcmp(*(char *const *)a, *(char *const *)b); }

static void list(const char *dir, const char *tag) {
  DIR *d = opendir(dir);
  if (!d) {
    printf("%s: %s\n", tag, strerror(errno));
    return;
  }
  char *names[256];
  int n = 0;
  struct dirent *e;
  while ((e = readdir(d)) && n < 256)
    if (strcmp(e->d_name, ".") && strcmp(e->d_name, "..") && strcmp(e->d_name, "stack"))
      names[n++] = strdup(e->d_name);
  closedir(d);
  qsort(names, n, sizeof(char *), cmp);
  printf("%s (%d):", tag, n);
  for (int i = 0; i < n; i++) {
    char p[512];
    snprintf(p, sizeof p, "%s/%s", dir, names[i]);
    struct stat st;
    if (lstat(p, &st) < 0)
      printf(" %s:%s", names[i], strerror(errno));
    else
      printf(" %s:%c%o", names[i], S_ISDIR(st.st_mode) ? 'd' : S_ISREG(st.st_mode) ? 'f' : S_ISLNK(st.st_mode) ? 'l' : '?', st.st_mode & 07777);
    free(names[i]);
  }
  printf("\n");
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  list("/proc/self", "self");
  char t[64];
  snprintf(t, sizeof t, "/proc/self/task/%d", getpid());
  list(t, "task");
  struct stat st;
  lstat("/proc/net", &st);
  printf("/proc/net is a link %d\n", S_ISLNK(st.st_mode));
  return 0;
}

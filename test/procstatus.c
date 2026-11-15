// /proc/self/status と /proc/self/stat が Linux と同じ形か: status の行の名前のならび (aios が足した Landlock はのぞく) (と、決まった値のもの: Umask、
// State、TracerPid、FDSize の大きさ、Groups、NStgid と NSpid、Kthread、CoreDumping、THP_enabled、untag_mask、Cap*、
// NoNewPrivs、Seccomp、Speculation_Store_Bypass、Cpus_allowed_list、Mems_allowed_list)、stat の項目の数と決まった値
// (priority、nice、num_threads、rsslim、exit_signal、rt_priority、policy)
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  FILE *f = fopen("/proc/self/status", "r");
  char l[512];
  const char *show[] = {"Umask", "State", "TracerPid", "Kthread", "CoreDumping", "THP_enabled", "untag_mask", "CapInh", "CapPrm", "CapEff", "CapBnd", "CapAmb",
                        "NoNewPrivs", "Seccomp", "Seccomp_filters", "SpeculationIndirectBranch", "Mems_allowed_list", "Threads", 0};
  while (f && fgets(l, sizeof l, f)) {
    char *c = strchr(l, ':');
    if (!c)
      continue;
    *c = 0;
    char *v = c + 1;
    while (*v == ' ' || *v == '\t')
      v++;
    v[strcspn(v, "\n")] = 0;
    // Landlock は aios が足したもの (aish が読む)。Speculation_Store_Bypass は CPU で変わる
    if (!strcmp(l, "Landlock"))
      continue;
    int shown = 0;
    for (int i = 0; show[i]; i++)
      if (!strcmp(l, show[i])) {
        printf("%s: %s\n", l, v);
        shown = 1;
      }
    if (!shown) {
      // 数の行は kB などの単位だけ
      const char *unit = strstr(v, "kB") ? " kB" : "";
      printf("%s:%s\n", l, unit);
    }
  }
  if (f)
    fclose(f);
  f = fopen("/proc/self/stat", "r");
  char b[2048];
  if (f && fgets(b, sizeof b, f)) {
    char *p = strrchr(b, ')') + 2;
    char *t[64];
    int n = 0;
    for (char *s = strtok(p, " \n"); s && n < 64; s = strtok(NULL, " \n"))
      t[n++] = s;
    // ) のあとの n 番目は stat の n + 3 番目
    printf("stat fields %d, state %s, priority %s, nice %s, threads %s, itrealvalue %s, rsslim %s, exit_signal %s, rt_priority %s, policy %s\n", n + 2, t[0], t[15], t[16],
           t[17], t[18], t[22], t[35], t[37], t[38]);
  }
  return 0;
}

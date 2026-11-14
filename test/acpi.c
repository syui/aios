// ACPI で起こしたときの /sys/firmware/acpi (Linux の drivers/acpi/sysfs.c): ならび (名前の順)、種類と許可、表の頭
// (名前、長さ、版、OEM。root で)、pm_profile と hotplug/force_remove、force_remove に 1 を書くと EINVAL、0 はよい。
// root でなければ表は EACCES。aios は test/acpi-boot.sh (UEFI と QEMU の acpi=on)、Linux は
// LINUX_EFI=1 LINUX_ACPI=1 test/linux-vm.sh rootexec で (aarch64-linux-gnu-gcc -O2 -static -o acpi test/acpi.c)
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

static int cmp(const void *a, const void *b) { return strcmp(*(char *const *)a, *(char *const *)b); }

static void walk(const char *dir, int depth) {
  DIR *d = opendir(dir);
  if (!d) {
    printf("%s: %s\n", dir, strerror(errno));
    return;
  }
  char *names[256];
  int n = 0;
  struct dirent *e;
  while ((e = readdir(d)) && n < 256)
    if (strcmp(e->d_name, ".") && strcmp(e->d_name, ".."))
      names[n++] = strdup(e->d_name);
  closedir(d);
  qsort(names, n, sizeof(char *), cmp);
  for (int i = 0; i < n; i++) {
    char p[512];
    snprintf(p, sizeof p, "%s/%s", dir, names[i]);
    struct stat st;
    if (lstat(p, &st) < 0) {
      printf("%*s%s: %s\n", depth * 2, "", names[i], strerror(errno));
      continue;
    }
    printf("%*s%s %s %o\n", depth * 2, "", names[i], S_ISDIR(st.st_mode) ? "dir" : S_ISREG(st.st_mode) ? "file" : "other",
           st.st_mode & 07777);
    if (S_ISDIR(st.st_mode))
      walk(p, depth + 1);
    free(names[i]);
  }
}

// 表の頭 (36 バイト): 名前、長さ (読んだバイトの数と同じか)、版、OEM ID
static void table(const char *name) {
  char p[256];
  snprintf(p, sizeof p, "/sys/firmware/acpi/tables/%s", name);
  int fd = open(p, O_RDONLY);
  if (fd < 0) {
    printf("table %s: %s\n", name, strerror(errno));
    return;
  }
  static unsigned char b[1 << 20];
  ssize_t n = 0, r;
  while ((r = read(fd, b + n, sizeof b - n)) > 0)
    n += r;
  close(fd);
  if (n < 36) {
    printf("table %s: short %zd\n", name, n);
    return;
  }
  uint32_t len = b[4] | b[5] << 8 | b[6] << 16 | (uint32_t)b[7] << 24;
  unsigned sum = 0;
  for (ssize_t i = 0; i < n; i++)
    sum += b[i];
  printf("table %s: sig %.4s len %s rev %u oem %.6s sum %s\n", name, b, len == (uint32_t)n ? "same" : "differs", b[8], b + 10,
         (sum & 0xff) == 0 ? "ok" : "bad");
}

static void show(const char *p) {
  char b[64] = {0};
  int fd = open(p, O_RDONLY);
  if (fd < 0) {
    printf("%s: %s\n", p, strerror(errno));
    return;
  }
  ssize_t n = read(fd, b, sizeof b - 1);
  close(fd);
  printf("%s: %s", p, n > 0 ? b : "(empty)\n");
}

static void put(const char *p, const char *v) {
  int fd = open(p, O_WRONLY);
  if (fd < 0) {
    printf("write %s to %s: open %s\n", v, p, strerror(errno));
    return;
  }
  ssize_t n = write(fd, v, strlen(v));
  printf("write %s to %s: %s\n", v, p, n < 0 ? strerror(errno) : "ok");
  close(fd);
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  printf("uid %s\n", getuid() == 0 ? "root" : "user");
  walk("/sys/firmware/acpi", 0);
  show("/sys/firmware/acpi/pm_profile");
  show("/sys/firmware/acpi/hotplug/force_remove");
  const char *names[] = {"APIC", "DSDT", "FACP", "GTDT", "MCFG", "SPCR"};
  for (unsigned i = 0; i < sizeof names / sizeof *names; i++)
    table(names[i]);
  if (getuid() == 0) {
    put("/sys/firmware/acpi/hotplug/force_remove", "1");
    put("/sys/firmware/acpi/hotplug/force_remove", "0");
  }
  return 0;
}

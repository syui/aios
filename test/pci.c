// PCI の装置の /sys (Linux の drivers/pci/pci-sysfs.c と probe.c): /sys/bus/pci と /sys/devices/pci0000:00 の
// ならび (名前の順、種類と許可、リンクの先)、読めるファイルの中身 (config は頭の 64 バイトと読めた長さ)、
// root でないと config は 64 バイトだけ、/sys/class/pci_bus。aios は test/ldiff.sh --root pci、ACPI は
// test/acpi-boot.sh、Linux は test/linux-vm.sh rootexec (LINUX_EFI=1 LINUX_ACPI=1 で ACPI)。
// QEMU に装置を足して (AIOS_QEMU_ARGS と LINUX_QEMU_ARGS) ブリッジの下もくらべる
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

static int cmp(const void *a, const void *b) { return strcmp(*(char *const *)a, *(char *const *)b); }

static void hex(const unsigned char *b, int n) {
  for (int i = 0; i < n; i++)
    printf("%s%02x", i % 16 ? " " : i ? "\n      " : "", b[i]);
  printf("\n");
}

static void content(const char *p, const char *name, int depth) {
  int fd = open(p, O_RDONLY);
  if (fd < 0) {
    printf("%*s  = %s\n", depth * 2, "", strerror(errno));
    return;
  }
  static unsigned char b[8192];
  ssize_t n = 0, r;
  while (n < (ssize_t)sizeof b && (r = read(fd, b + n, sizeof b - n)) > 0)
    n += r;
  r = n < 0 ? -errno : 0;
  close(fd);
  if (r < 0) {
    printf("%*s  = %s\n", depth * 2, "", strerror(-r));
    return;
  }
  if (!strcmp(name, "config")) {
    printf("%*s  = %zd bytes:", depth * 2, "", n);
    hex(b, n < 64 ? n : 64);
    return;
  }
  printf("%*s  = \"", depth * 2, "");
  for (ssize_t i = 0; i < n && i < 4096; i++)
    if (b[i] == '\n')
      printf("\\n");
    else if (b[i] >= 0x20 && b[i] < 0x7f)
      putchar(b[i]);
    else
      printf("\\x%02x", b[i]);
  printf("\"\n");
}

static void walk(const char *dir, int depth, int deep) {
  DIR *d = opendir(dir);
  if (!d) {
    printf("%*s%s: %s\n", depth * 2, "", dir, strerror(errno));
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
    char p[1024];
    snprintf(p, sizeof p, "%s/%s", dir, names[i]);
    struct stat st;
    if (lstat(p, &st) < 0) {
      printf("%*s%s: %s\n", depth * 2, "", names[i], strerror(errno));
      continue;
    }
    if (S_ISLNK(st.st_mode)) {
      char t[512] = {0};
      readlink(p, t, sizeof t - 1);
      printf("%*s%s -> %s\n", depth * 2, "", names[i], t);
    } else if (S_ISDIR(st.st_mode)) {
      printf("%*s%s/ %o\n", depth * 2, "", names[i], st.st_mode & 07777);
      if (deep)
        walk(p, depth + 1, deep);
    } else {
      printf("%*s%s %o size %lld\n", depth * 2, "", names[i], st.st_mode & 07777, (long long)st.st_size);
      if (st.st_mode & 0444)
        content(p, names[i], depth);
    }
    free(names[i]);
  }
}

// root でなければ config は頭の 64 バイトだけ (Linux の pci_read_config: CAP_SYS_ADMIN がなければ)
static void as_user(const char *p) {
  pid_t c = fork();
  if (c == 0) {
    if (setgid(65534) < 0 || setuid(65534) < 0)
      _exit(1);
    int fd = open(p, O_RDONLY);
    if (fd < 0) {
      printf("user %s: %s\n", p, strerror(errno));
      _exit(0);
    }
    static unsigned char b[8192];
    ssize_t n = 0, r;
    while ((r = read(fd, b + n, sizeof b - n)) > 0)
      n += r;
    ssize_t at = pread(fd, b, 16, 60);
    ssize_t past = pread(fd, b, 16, 64);
    printf("user %s: %zd bytes, pread 60: %zd, pread 64: %zd\n", p, n, at, past);
    _exit(0);
  }
  waitpid(c, NULL, 0);
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  printf("uid %s\n", getuid() == 0 ? "root" : "user");
  walk("/sys/bus/pci", 0, 1);
  // ルートのバスの装置 (DTB はホストブリッジの platform の装置の下: /sys/devices/platform/4010000000.pcie/pci0000:00。
  // ACPI は /sys/devices/pci0000:00)
  char root[4096];
  if (realpath("/sys/class/pci_bus/0000:00/device", root)) {
    printf("root %s\n", root);
    walk(root, 0, 1);
    // DTB: その上の platform の装置 (中の pci0000:00 はたどらない)
    char *up = strrchr(root, '/');
    if (up && up != root && strcmp(root, "/sys/devices/pci0000:00")) {
      *up = 0;
      printf("up %s\n", root);
      walk(root, 0, 0);
    }
  } else {
    printf("root: %s\n", strerror(errno));
  }
  walk("/sys/class/pci_bus", 0, 1);
  if (getuid() == 0)
    as_user("/sys/bus/pci/devices/0000:00:00.0/config");
  return 0;
}

// efivarfs (kernel/src/efivarfs.rs と efirt.rs): UEFI の変数のファイル。Linux 7.3.1 (fs/efivarfs) と同じ答え。
// root で (書く、消す、chattr)。UEFI から起動していなければ (/sys/firmware/efi がない) 何もせずに 0 で終わる。
//   /sys/firmware/efi の fw_platform_size と systab、efivars の statfs (f_type、f_bsize 1)
//   ファームウェアの変数 (PlatformLang) を読む: 属性 4 バイト + 中身、pread、大きさ
//   作る (O_CREAT) → 書く (属性 + 中身) → 読む、変えられない印 (知らない GUID)、chattr -i、APPEND_WRITE で足す、
//   中身 0 バイトで消す (閉じるまで 0 バイトで残り、閉じると消える)、書かずに閉じると消える、unlink
//   まちがい: 短い write、知らない属性のビット、名前の形、GUID の大文字、mkdir / symlink / link / rename、
//   乱数の種の GUID、変えられないものを書く・切りつめる・消す
// 書く変数はこのテストだけの GUID (QEMU の VM の中で。-snapshot なら残らない)
// aios の中で: zig cc -target aarch64-linux-musl -O2 -static -o efivarfs test/efivarfs.c; sudo ./efivarfs
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/fs.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/vfs.h>
#include <unistd.h>

#define DIR "/sys/firmware/efi/efivars"
#define GLOBAL "8be4df61-93ca-11d2-aa0d-00e098032b8c"
#define TEST_GUID "a1b2c3d4-0e0f-4a5b-8c9d-aabbccddeeff"
#define NV_BS_RT 7
#define APPEND 0x40

static int fails, checks;

static void check(const char *what, long got, long want) {
    checks++;
    if (got != want) {
        fails++;
        printf("FAIL %s: got %ld, want %ld\n", what, got, want);
    }
}

// 0 か -errno
static long r(long v) {
    return v < 0 ? -errno : v;
}

static long put(int fd, unsigned attr, const char *data, size_t n) {
    char b[256];
    memcpy(b, &attr, 4);
    memcpy(b + 4, data, n);
    return r(write(fd, b, 4 + n));
}

static long size_of(const char *p) {
    struct stat st;
    return stat(p, &st) == 0 ? (long)st.st_size : -errno;
}

static long getflags(const char *p) {
    int fd = open(p, O_RDONLY);
    if (fd < 0)
        return -errno;
    int f = 0;
    long v = ioctl(fd, FS_IOC_GETFLAGS, &f) == 0 ? f : -errno;
    close(fd);
    return v;
}

static long setflags(const char *p, int f) {
    int fd = open(p, O_RDONLY);
    if (fd < 0)
        return -errno;
    long v = r(ioctl(fd, FS_IOC_SETFLAGS, &f));
    close(fd);
    return v;
}

static void sysfs(void) {
    char b[64] = {0};
    int fd = open("/sys/firmware/efi/fw_platform_size", O_RDONLY);
    check("fw_platform_size open", fd >= 0, 1);
    check("fw_platform_size", read(fd, b, sizeof b - 1), 3);
    check("fw_platform_size 64", strcmp(b, "64\n"), 0);
    close(fd);
    struct stat st;
    check("systab", stat("/sys/firmware/efi/systab", &st), 0);
    check("systab mode", st.st_mode & 07777, 0400);
    check("efivars dir", stat(DIR, &st) == 0 && S_ISDIR(st.st_mode), 1);
    struct statfs sf;
    check("statfs", statfs(DIR, &sf), 0);
    check("statfs f_type", (unsigned long)sf.f_type, 0xde5e81e4);
    check("statfs f_bsize", sf.f_bsize, 1);
    check("statfs f_blocks", sf.f_blocks > 0, 1);
    check("statfs f_bfree", sf.f_bfree <= sf.f_blocks, 1);
}

// ファームウェアのもの: PlatformLang (EDK II は "en-US"。NV BS RT)
static void firmware(void) {
    const char *p = DIR "/PlatformLang-" GLOBAL;
    char b[64];
    int fd = open(p, O_RDONLY);
    check("PlatformLang open", fd >= 0, 1);
    long n = read(fd, b, sizeof b);
    check("PlatformLang read", n > 4, 1);
    unsigned attr;
    memcpy(&attr, b, 4);
    check("PlatformLang attr", attr, NV_BS_RT);
    check("PlatformLang nul", n > 4 && b[n - 1] == 0, 1);
    check("PlatformLang size", size_of(p), n);
    char c[8];
    check("PlatformLang pread", pread(fd, c, 2, 4), 2);
    check("PlatformLang pread data", memcmp(c, b + 4, 2), 0);
    check("PlatformLang pread end", pread(fd, c, 8, n), 0);
    close(fd);
    // Linux が知っているもの (消せる) には変えられない印がない
    check("PlatformLang flags", getflags(p), 0);
    // GUID の大文字でも同じもの (名前は大文字小文字をくべつする)
    check("upper guid", size_of(DIR "/PlatformLang-8BE4DF61-93CA-11D2-AA0D-00E098032B8C"), n);
    check("lower name", size_of(DIR "/platformlang-" GLOBAL), -ENOENT);
}

static void lifecycle(void) {
    const char *p = DIR "/AiosTest-" TEST_GUID;
    unlink(p);
    int fd = open(p, O_CREAT | O_WRONLY | O_EXCL, 0644);
    check("create", fd >= 0, 1);
    check("create size", size_of(p), 0);
    // 作っただけ (まだない) を読むと 0 バイト
    int rd = open(p, O_RDONLY);
    char b[64];
    check("read uncommitted", read(rd, b, sizeof b), 0);
    close(rd);
    // 0 バイトの中身は「消す」: まだないものを消すと UEFI は NOT_FOUND、Linux はそれを EIO に
    check("write empty uncommitted", put(fd, NV_BS_RT, "", 0), -EIO);
    check("size after empty", size_of(p), 0);
    check("write 3 bytes", r(write(fd, "abc", 3)), -EINVAL);
    check("write bad attr", put(fd, 0x80 | NV_BS_RT, "x", 1), -EINVAL);
    check("write", put(fd, NV_BS_RT, "hello", 5), 9);
    check("size after write", size_of(p), 9);
    // 書く位置は見ない (どの write も変数ぜんぶ)
    check("write again", put(fd, NV_BS_RT, "hi", 2), 6);
    check("size after rewrite", size_of(p), 6);
    check("append", put(fd, NV_BS_RT | APPEND, "!!", 2), 6);
    check("size after append", size_of(p), 8);
    close(fd);
    fd = open(p, O_RDONLY);
    long n = read(fd, b, sizeof b);
    check("read back", n, 8);
    unsigned attr;
    memcpy(&attr, b, 4);
    check("read attr", attr, NV_BS_RT);
    check("read data", memcmp(b + 4, "hi!!", 4), 0);
    close(fd);
    // 知らない GUID: 閉じて開きなおすと変えられない印
    check("immutable", getflags(p), FS_IMMUTABLE_FL);
    check("open write immutable", r(open(p, O_WRONLY)), -EPERM);
    check("open trunc immutable", r(open(p, O_RDONLY | O_TRUNC)), -EPERM);
    check("unlink immutable", r(unlink(p)), -EPERM);
    check("chmod immutable", r(chmod(p, 0600)), -EPERM);
    check("setflags other", setflags(p, FS_IMMUTABLE_FL | FS_APPEND_FL), -EOPNOTSUPP);
    check("chattr -i", setflags(p, 0), 0);
    check("flags after chattr", getflags(p), 0);
    check("chmod", r(chmod(p, 0600)), 0);
    struct stat st;
    check("mode", stat(p, &st) == 0 ? (long)(st.st_mode & 07777) : -errno, 0600);
    // O_TRUNC は大きさを変えない
    fd = open(p, O_WRONLY | O_TRUNC);
    check("open trunc", fd >= 0, 1);
    check("size after trunc", size_of(p), 8);
    // 中身 0 バイトで消す: 閉じるまで 0 バイトで残る
    check("delete write", put(fd, NV_BS_RT, "", 0), 4);
    check("size after delete", size_of(p), 0);
    rd = open(p, O_RDONLY);
    check("read deleted", read(rd, b, sizeof b), 0);
    close(rd);
    close(fd);
    check("gone after close", size_of(p), -ENOENT);
    // 書かずに閉じると消える
    fd = open(p, O_CREAT | O_WRONLY, 0644);
    check("create again", fd >= 0, 1);
    close(fd);
    check("touch gone", size_of(p), -ENOENT);
    // unlink (chattr -i のあと)
    fd = open(p, O_CREAT | O_WRONLY, 0644);
    check("write for unlink", put(fd, NV_BS_RT, "z", 1), 5);
    close(fd);
    check("unlink immutable 2", r(unlink(p)), -EPERM);
    check("chattr -i 2", setflags(p, 0), 0);
    check("unlink", r(unlink(p)), 0);
    check("unlink gone", size_of(p), -ENOENT);
    check("unlink again", r(unlink(p)), -ENOENT);
}

static void errors(void) {
    check("create no guid", r(open(DIR "/Foo", O_CREAT | O_WRONLY, 0644)), -EINVAL);
    check("create bad guid", r(open(DIR "/Foo-a1b2c3d4-0e0f-4a5b-8c9d-aabbccddeefg", O_CREAT | O_WRONLY, 0644)), -EINVAL);
    check("create no name", r(open(DIR "/-" TEST_GUID, O_CREAT | O_WRONLY, 0644)), -EINVAL);
    check("create seed guid", r(open(DIR "/Seed-1ce1e5bc-7ceb-42f2-81e5-8aadf180f57b", O_CREAT | O_WRONLY, 0644)), -EPERM);
    check("mkdir", r(mkdir(DIR "/D-" TEST_GUID, 0755)), -EPERM);
    check("symlink", r(symlink("x", DIR "/L-" TEST_GUID)), -EPERM);
    const char *p = DIR "/AiosTest2-" TEST_GUID;
    int fd = open(p, O_CREAT | O_WRONLY, 0644);
    check("write t2", put(fd, NV_BS_RT, "q", 1), 5);
    check("rename", r(rename(p, DIR "/AiosTest3-" TEST_GUID)), -EPERM);
    check("link", r(link(p, DIR "/AiosTest3-" TEST_GUID)), -EPERM);
    close(fd);
    setflags(p, 0);
    check("unlink t2", r(unlink(p)), 0);
    check("lookup missing", size_of(DIR "/Nope-" TEST_GUID), -ENOENT);
}

int main(void) {
    struct stat st;
    if (stat("/sys/firmware/efi", &st) != 0) {
        printf("efivarfs: no UEFI runtime (skipped)\n");
        return 0;
    }
    // 手本の Linux (test/linux-vm.sh の /init) はかぶせていないので、かぶせる (aios は起動のときにかぶせてある)
    struct statfs sf;
    if (statfs(DIR, &sf) == 0 && (unsigned long)sf.f_type != 0xde5e81e4)
        check("mount", mount("efivarfs", DIR, "efivarfs", 0, NULL), 0);
    sysfs();
    firmware();
    lifecycle();
    errors();
    printf("efivarfs: %d checks, %d failed\n", checks, fails);
    return fails ? 1 : 0;
}

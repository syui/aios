// 近所さがし (ARP と IPv6 の NS) があて先ごとか: 近くにいない 2 つのあて先へ、別々の UDP ソケットで続けて送り、近くの
// 番地の表 (RTM_GETNEIGH) に 2 つとも INCOMPLETE で出るかを見る (Linux はあて先ごとに問い合わせる。smoltcp 0.12 は
// 1 つ問い合わせると、どのあて先にも 1 秒は問い合わせず、1 つ目を問い合わせなおすたびにまただまるので、ほかはいつまでも
// 問い合わせてもらえなかった: todo の smoltcp-nd-silence)。IPv4 は 10.0.2.200 と .201、IPv6 は SLAAC の fec0::/64 の
// fec0::200 と ::201 (QEMU のユーザーネットワークが答えるのは自分の番地だけ)。3 度問い合わせて答えがなければ FAILED に
// して待っているパケットを捨てる (Linux) のは、aios ではまだ (smoltcp は問い合わせつづける: todo の neigh-failed) なので
// 見ない
// 出すだけのテスト: 手本の Linux は ip=dhcp の virtio-net (test/ldiff.sh --net ndsilence)
//   aios の中で: ./ndsilence      aarch64-linux-gnu-gcc -O2 -static -o ndsilence test/ndsilence.c
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <ifaddrs.h>
#include <linux/neighbour.h>
#include <linux/netlink.h>
#include <linux/rtnetlink.h>
#include <netinet/in.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <time.h>
#include <unistd.h>

static char buf[65536];

static const char *state_name(int s) {
    switch (s) {
    case NUD_INCOMPLETE: return "INCOMPLETE";
    case NUD_REACHABLE: return "REACHABLE";
    case NUD_STALE: return "STALE";
    case NUD_DELAY: return "DELAY";
    case NUD_PROBE: return "PROBE";
    case NUD_FAILED: return "FAILED";
    case NUD_NOARP: return "NOARP";
    case NUD_PERMANENT: return "PERMANENT";
    default: return "?";
    }
}

static void msleep(int ms) {
    struct timespec t = {ms / 1000, (long)(ms % 1000) * 1000000};
    nanosleep(&t, NULL);
}

// family の近くの番地の表から、want の 2 つ (len バイト) の状態を出す。INCOMPLETE の数を返す
static int show(const char *what, int family, const unsigned char want[2][16], int len) {
    int s = socket(AF_NETLINK, SOCK_RAW | SOCK_CLOEXEC, NETLINK_ROUTE);
    if (s < 0) {
        printf("%s: socket: %s\n", what, strerror(errno));
        return 0;
    }
    struct {
        struct nlmsghdr h;
        struct ndmsg n;
    } req = {{sizeof req, RTM_GETNEIGH, NLM_F_REQUEST | NLM_F_DUMP, 1, 0}, {.ndm_family = family}};
    if (send(s, &req, sizeof req, 0) < 0) {
        printf("%s: send: %s\n", what, strerror(errno));
        close(s);
        return 0;
    }
    const char *st[2] = {"(none)", "(none)"};
    for (int done = 0; !done;) {
        int n = recv(s, buf, sizeof buf, 0);
        if (n <= 0) break;
        for (struct nlmsghdr *h = (struct nlmsghdr *)buf; NLMSG_OK(h, (unsigned)n); h = NLMSG_NEXT(h, n)) {
            if (h->nlmsg_type == NLMSG_DONE || h->nlmsg_type == NLMSG_ERROR) {
                done = 1;
                break;
            }
            if (h->nlmsg_type != RTM_NEWNEIGH) continue;
            struct ndmsg *nd = NLMSG_DATA(h);
            int alen = h->nlmsg_len - NLMSG_LENGTH(sizeof *nd);
            for (struct rtattr *a = (struct rtattr *)((char *)nd + NLMSG_ALIGN(sizeof *nd)); RTA_OK(a, alen); a = RTA_NEXT(a, alen)) {
                if (a->rta_type != NDA_DST || RTA_PAYLOAD(a) != (unsigned)len) continue;
                for (int i = 0; i < 2; i++)
                    if (!memcmp(RTA_DATA(a), want[i], len)) st[i] = state_name(nd->ndm_state);
            }
        }
    }
    close(s);
    char a0[64], a1[64];
    inet_ntop(family, want[0], a0, sizeof a0);
    inet_ntop(family, want[1], a1, sizeof a1);
    printf("%s: %s %s, %s %s\n", what, a0, st[0], a1, st[1]);
    return !strcmp(st[0], "INCOMPLETE") + !strcmp(st[1], "INCOMPLETE");
}

// 2 つのあて先へ、別々の UDP ソケットで 1 つずつ (同じソケットだと、smoltcp では前のものが答えを待つあいだ後ろも待つ)
static void send_two(int family, const unsigned char to[2][16]) {
    for (int i = 0; i < 2; i++) {
        int s = socket(family, SOCK_DGRAM, 0);
        if (s < 0) {
            printf("socket: %s\n", strerror(errno));
            continue;
        }
        struct sockaddr_storage ss = {0};
        socklen_t sl;
        if (family == AF_INET) {
            struct sockaddr_in *a = (struct sockaddr_in *)&ss;
            a->sin_family = AF_INET;
            a->sin_port = htons(9);
            memcpy(&a->sin_addr, to[i], 4);
            sl = sizeof *a;
        } else {
            struct sockaddr_in6 *a = (struct sockaddr_in6 *)&ss;
            a->sin6_family = AF_INET6;
            a->sin6_port = htons(9);
            memcpy(&a->sin6_addr, to[i], 16);
            sl = sizeof *a;
        }
        if (sendto(s, "x", 1, 0, (struct sockaddr *)&ss, sl) != 1) printf("sendto %d: %s\n", i, strerror(errno));
        // 閉じない (送るものが答えを待っているあいだ)
    }
}

// eth0 に fec0::/64 の番地ができるまで (15 秒まで)
static int wait_slaac(void) {
    for (int i = 0; i < 150; i++) {
        struct ifaddrs *ifs;
        if (getifaddrs(&ifs) == 0) {
            int ok = 0;
            for (struct ifaddrs *p = ifs; p; p = p->ifa_next) {
                if (!p->ifa_addr || p->ifa_addr->sa_family != AF_INET6) continue;
                const unsigned char *b = ((struct sockaddr_in6 *)p->ifa_addr)->sin6_addr.s6_addr;
                if (b[0] == 0xfe && b[1] == 0xc0) ok = 1;
            }
            freeifaddrs(ifs);
            if (ok) return 1;
        }
        msleep(100);
    }
    return 0;
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    unsigned char v4[2][16] = {{10, 0, 2, 200}, {10, 0, 2, 201}};
    unsigned char v6[2][16];
    inet_pton(AF_INET6, "fec0::200", v6[0]);
    inet_pton(AF_INET6, "fec0::201", v6[1]);
    printf("slaac: %s\n", wait_slaac() ? "ok" : "no fec0:: address");
    send_two(AF_INET, v4);
    send_two(AF_INET6, v6);
    msleep(300);
    int n = show("ipv4 after 0.3s", AF_INET, v4, 4);
    n += show("ipv6 after 0.3s", AF_INET6, v6, 16);
    // 4 つとも問い合わせていなければしくじり (ctests)
    return n == 4 ? 0 : 1;
}

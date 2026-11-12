// 新しい NET の namespace の中から見えるもの (todo の netns-view): lo だけで、はじめは DOWN (番地も道もない)、
// SIOCSIFFLAGS か RTM_NEWLINK で UP にすると 127.0.0.1/8 と ::1 と local の道ができる。netlink (RTM_GETLINK・GETADDR・
// GETROUTE・GETNEIGH)、getifaddrs と if_nameindex、ioctl (SIOCGIFCONF、FLAGS、INDEX、TXQLEN)、/proc/net、127.0.0.1 への
// TCP と UDP。root でない人で (ユーザーの namespace で中の root になってから)。test/ldiff.sh netnsview で Linux とくらべる。
// 手本の Linux の sit0 (CONFIG_IPV6_SIT のトンネル) は見ない (ほかのテストと同じ)。DOWN にしたあとの 127.0.0.1 の connect は
// Linux では SYN を送りつづけて待つので、しない
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <ifaddrs.h>
#include <linux/netlink.h>
#include <linux/rtnetlink.h>
#include <linux/sockios.h>
#include <net/if.h>
#include <netinet/in.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <unistd.h>

static int put(const char *file, const char *s) {
  int fd = open(file, O_WRONLY);
  if (fd < 0) return -1;
  int r = write(fd, s, strlen(s)) == (ssize_t)strlen(s) ? 0 : -1;
  close(fd);
  return r;
}

/* netlink に 1 つ送って、答えを NLMSG_DONE か ERROR まで読む。each(msg) をよぶ。返すのは ERROR の値 (なければ 0) */
static int nl_talk(int type, int flags, void *body, int blen, void (*each)(struct nlmsghdr *)) {
  int s = socket(AF_NETLINK, SOCK_RAW | SOCK_CLOEXEC, NETLINK_ROUTE);
  if (s < 0) return -errno;
  char req[256] = {0};
  struct nlmsghdr *h = (void *)req;
  h->nlmsg_len = NLMSG_LENGTH(blen);
  h->nlmsg_type = type;
  h->nlmsg_flags = NLM_F_REQUEST | flags;
  h->nlmsg_seq = 1;
  memcpy(NLMSG_DATA(h), body, blen);
  if (send(s, req, h->nlmsg_len, 0) < 0) { close(s); return -errno; }
  int ret = 0;
  for (;;) {
    char buf[32768];
    ssize_t n = recv(s, buf, sizeof buf, 0);
    if (n <= 0) break;
    int end = 0;
    for (struct nlmsghdr *m = (void *)buf; NLMSG_OK(m, n); m = NLMSG_NEXT(m, n)) {
      if (m->nlmsg_type == NLMSG_DONE) { end = 1; break; }
      if (m->nlmsg_type == NLMSG_ERROR) { ret = ((struct nlmsgerr *)NLMSG_DATA(m))->error; end = 1; break; }
      if (each) each(m);
    }
    if (end || !(flags & NLM_F_DUMP)) break;
  }
  close(s);
  return ret;
}

static void on_link(struct nlmsghdr *m) {
  struct ifinfomsg *i = NLMSG_DATA(m);
  const char *name = "?";
  int mtu = -1, txq = -1, oper = -1;
  int len = IFLA_PAYLOAD(m);
  for (struct rtattr *a = IFLA_RTA(i); RTA_OK(a, len); a = RTA_NEXT(a, len)) {
    if (a->rta_type == IFLA_IFNAME) name = RTA_DATA(a);
    if (a->rta_type == IFLA_MTU) mtu = *(int *)RTA_DATA(a);
    if (a->rta_type == IFLA_TXQLEN) txq = *(int *)RTA_DATA(a);
    if (a->rta_type == IFLA_OPERSTATE) oper = *(unsigned char *)RTA_DATA(a);
  }
  if (!strncmp(name, "sit", 3)) return;
  printf("  link %d %s type %d flags 0x%x mtu %d txqlen %d operstate %d\n", i->ifi_index, name, i->ifi_type, i->ifi_flags, mtu, txq, oper);
}
static void on_addr(struct nlmsghdr *m) {
  struct ifaddrmsg *i = NLMSG_DATA(m);
  char s[64] = "?";
  int len = IFA_PAYLOAD(m);
  for (struct rtattr *a = IFA_RTA(i); RTA_OK(a, len); a = RTA_NEXT(a, len))
    if (a->rta_type == IFA_ADDRESS) inet_ntop(i->ifa_family, RTA_DATA(a), s, sizeof s);
  printf("  addr %s %s/%d dev %d scope %d\n", i->ifa_family == AF_INET ? "inet" : "inet6", s, i->ifa_prefixlen, i->ifa_index, i->ifa_scope);
}
static void on_route(struct nlmsghdr *m) {
  struct rtmsg *r = NLMSG_DATA(m);
  char s[64] = "default";
  int oif = 0, table = r->rtm_table;
  int len = RTM_PAYLOAD(m);
  for (struct rtattr *a = RTM_RTA(r); RTA_OK(a, len); a = RTA_NEXT(a, len)) {
    if (a->rta_type == RTA_DST) inet_ntop(r->rtm_family, RTA_DATA(a), s, sizeof s);
    if (a->rta_type == RTA_OIF) oif = *(int *)RTA_DATA(a);
    if (a->rta_type == RTA_TABLE) table = *(int *)RTA_DATA(a);
  }
  printf("  route %s table %d type %d %s/%d oif %d\n", r->rtm_family == AF_INET ? "inet" : "inet6", table, r->rtm_type, s, r->rtm_dst_len, oif);
}
static int neighs;
static void on_neigh(struct nlmsghdr *m) { (void)m; neighs++; }

static void netlink_view(void) {
  struct ifinfomsg li = {.ifi_family = AF_UNSPEC};
  printf("RTM_GETLINK dump %d\n", nl_talk(RTM_GETLINK, NLM_F_DUMP, &li, sizeof li, on_link));
  struct ifaddrmsg ai = {.ifa_family = AF_UNSPEC};
  printf("RTM_GETADDR dump %d\n", nl_talk(RTM_GETADDR, NLM_F_DUMP, &ai, sizeof ai, on_addr));
  struct rtmsg r4 = {.rtm_family = AF_INET};
  printf("RTM_GETROUTE inet dump %d\n", nl_talk(RTM_GETROUTE, NLM_F_DUMP, &r4, sizeof r4, on_route));
  struct rtmsg r6 = {.rtm_family = AF_INET6};
  printf("RTM_GETROUTE inet6 dump %d\n", nl_talk(RTM_GETROUTE, NLM_F_DUMP, &r6, sizeof r6, on_route));
  struct ndmsg nd = {.ndm_family = AF_UNSPEC};
  neighs = 0;
  int r = nl_talk(RTM_GETNEIGH, NLM_F_DUMP, &nd, sizeof nd, on_neigh);
  printf("RTM_GETNEIGH dump %d: %d\n", r, neighs);
  struct ifinfomsg eth = {.ifi_family = AF_UNSPEC, .ifi_index = 99};
  printf("RTM_GETLINK index 99 %d\n", nl_talk(RTM_GETLINK, 0, &eth, sizeof eth, on_link));
  struct ifinfomsg byname = {.ifi_family = AF_UNSPEC};
  printf("RTM_GETLINK index 1 %d\n", nl_talk(RTM_GETLINK, 0, (byname.ifi_index = 1, &byname), sizeof byname, on_link));
}

static void lib_view(void) {
  struct if_nameindex *ni = if_nameindex();
  printf("if_nameindex:");
  for (struct if_nameindex *p = ni; p && p->if_index; p++)
    if (strncmp(p->if_name, "sit", 3)) printf(" %u:%s", p->if_index, p->if_name);
  printf("\n");
  if (ni) if_freenameindex(ni);
  struct ifaddrs *ia;
  printf("getifaddrs:");
  if (getifaddrs(&ia) == 0) {
    for (struct ifaddrs *p = ia; p; p = p->ifa_next) {
      char s[64] = "-";
      if (p->ifa_addr && p->ifa_addr->sa_family == AF_INET) inet_ntop(AF_INET, &((struct sockaddr_in *)p->ifa_addr)->sin_addr, s, sizeof s);
      if (p->ifa_addr && p->ifa_addr->sa_family == AF_INET6) inet_ntop(AF_INET6, &((struct sockaddr_in6 *)p->ifa_addr)->sin6_addr, s, sizeof s);
      if (strncmp(p->ifa_name, "sit", 3)) printf(" %s:%d:%s", p->ifa_name, p->ifa_addr ? p->ifa_addr->sa_family : -1, s);
    }
    freeifaddrs(ia);
  }
  printf("\n");
  printf("if_nametoindex lo %u eth0 %u\n", if_nametoindex("lo"), if_nametoindex("eth0"));
}

static void ioctl_view(int s) {
  char buf[1024];
  struct ifconf ic = {.ifc_len = sizeof buf, .ifc_buf = buf};
  int r = ioctl(s, SIOCGIFCONF, &ic);
  printf("SIOCGIFCONF %s:", r ? strerror(errno) : "0");
  for (int i = 0; !r && i < ic.ifc_len / (int)sizeof(struct ifreq); i++) {
    struct ifreq *q = &((struct ifreq *)buf)[i];
    printf(" %s=%s", q->ifr_name, inet_ntoa(((struct sockaddr_in *)&q->ifr_addr)->sin_addr));
  }
  printf("\n");
  const char *names[] = {"lo", "eth0"};
  for (int i = 0; i < 2; i++) {
    struct ifreq q = {0};
    strcpy(q.ifr_name, names[i]);
    r = ioctl(s, SIOCGIFFLAGS, &q);
    printf("SIOCGIFFLAGS %s %s 0x%x\n", names[i], r ? strerror(errno) : "0", r ? 0 : q.ifr_flags & 0xffff);
    memset(&q, 0, sizeof q); strcpy(q.ifr_name, names[i]);
    r = ioctl(s, SIOCGIFINDEX, &q);
    printf("SIOCGIFINDEX %s %s %d\n", names[i], r ? strerror(errno) : "0", r ? 0 : q.ifr_ifindex);
    memset(&q, 0, sizeof q); strcpy(q.ifr_name, names[i]);
    r = ioctl(s, SIOCGIFTXQLEN, &q);
    printf("SIOCGIFTXQLEN %s %s %d\n", names[i], r ? strerror(errno) : "0", r ? 0 : q.ifr_qlen);
    memset(&q, 0, sizeof q); strcpy(q.ifr_name, names[i]);
    r = ioctl(s, SIOCGIFADDR, &q);
    printf("SIOCGIFADDR %s %s %s\n", names[i], r ? strerror(errno) : "0", r ? "-" : inet_ntoa(((struct sockaddr_in *)&q.ifr_addr)->sin_addr));
  }
  struct ifreq q = {0};
  q.ifr_ifindex = 1;
  r = ioctl(s, SIOCGIFNAME, &q);
  printf("SIOCGIFNAME 1 %s %s\n", r ? strerror(errno) : "0", r ? "-" : q.ifr_name);
  q.ifr_ifindex = 99;
  r = ioctl(s, SIOCGIFNAME, &q);
  printf("SIOCGIFNAME 99 %s\n", r ? strerror(errno) : "0");
}

/* /proc/net のファイル: 1 列めだけ (数字はちがってよい) */
static void proc_view(void) {
  const char *files[] = {"/proc/net/dev", "/proc/net/route", "/proc/net/if_inet6", "/proc/net/arp", "/proc/net/ipv6_route", NULL};
  for (int i = 0; files[i]; i++) {
    FILE *f = fopen(files[i], "r");
    printf("%s:", files[i]);
    if (!f) { printf(" %s\n", strerror(errno)); continue; }
    char line[512];
    int n = 0;
    while (fgets(line, sizeof line, f)) {
      n++;
      char first[64] = "";
      sscanf(line, "%63s", first);
      if (i == 0 && n <= 2) continue; /* /proc/net/dev の見出し 2 行 */
      if ((i == 1 || i == 3) && n == 1) continue;
      if (i == 2) { char a[64], dev[32]; unsigned ix, pl, sc, fl; if (sscanf(line, "%63s %x %x %x %x %31s", a, &ix, &pl, &sc, &fl, dev) == 6) { printf(" %s/%u:%s", a, pl, dev); continue; } }
      if (i == 4) { char d[64], dev[32]; unsigned dl; if (sscanf(line, "%63s %x %*s %*s %*s %*s %*s %*s %*s %31s", d, &dl, dev) == 3) { printf(" %s/%u:%s", d, dl, dev); continue; } }
      if (strncmp(first, "sit", 3)) printf(" %s", first);
    }
    fclose(f);
    printf("\n");
  }
}

static void reach(void) {
  int t = socket(AF_INET, SOCK_STREAM, 0);
  struct sockaddr_in a = {.sin_family = AF_INET, .sin_port = htons(9), .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
  printf("tcp connect 127.0.0.1:9 %s\n", connect(t, (void *)&a, sizeof a) ? strerror(errno) : "0");
  close(t);
  t = socket(AF_INET, SOCK_STREAM, 0);
  a.sin_port = htons(7401);
  printf("tcp bind 127.0.0.1:7401 %s\n", bind(t, (void *)&a, sizeof a) ? strerror(errno) : "0");
  close(t);
  int u = socket(AF_INET, SOCK_DGRAM, 0);
  a.sin_port = htons(9);
  printf("udp sendto 127.0.0.1:9 %s\n", sendto(u, "x", 1, 0, (void *)&a, sizeof a) < 0 ? strerror(errno) : "1");
  close(u);
  int t6 = socket(AF_INET6, SOCK_STREAM, 0);
  struct sockaddr_in6 b = {.sin6_family = AF_INET6, .sin6_port = htons(9), .sin6_addr = IN6ADDR_LOOPBACK_INIT};
  printf("tcp6 connect [::1]:9 %s\n", connect(t6, (void *)&b, sizeof b) ? strerror(errno) : "0");
  close(t6);
}

static void view(const char *when) {
  printf("== %s\n", when);
  netlink_view();
  lib_view();
  int s = socket(AF_INET, SOCK_DGRAM, 0);
  ioctl_view(s);
  close(s);
  proc_view();
  reach();
}

/* lo を ioctl で UP / DOWN */
static void lo_ioctl(int up) {
  int s = socket(AF_INET, SOCK_DGRAM, 0);
  struct ifreq q = {0};
  strcpy(q.ifr_name, "lo");
  ioctl(s, SIOCGIFFLAGS, &q);
  if (up) q.ifr_flags |= IFF_UP; else q.ifr_flags &= ~IFF_UP;
  printf("SIOCSIFFLAGS lo %s %s\n", up ? "up" : "down", ioctl(s, SIOCSIFFLAGS, &q) ? strerror(errno) : "0");
  close(s);
}
/* lo を RTM_NEWLINK で (ip link set lo up / down) */
static void lo_netlink(int up) {
  struct ifinfomsg i = {.ifi_family = AF_UNSPEC, .ifi_index = 1, .ifi_flags = up ? IFF_UP : 0, .ifi_change = IFF_UP};
  printf("RTM_NEWLINK lo %s %d\n", up ? "up" : "down", nl_talk(RTM_NEWLINK, NLM_F_ACK, &i, sizeof i, NULL));
}

int main(void) {
  char m[64];
  uid_t me = getuid();
  gid_t g = getgid();
  printf("%-30s %s\n", "unshare NEWUSER", unshare(CLONE_NEWUSER) ? strerror(errno) : "0");
  snprintf(m, sizeof m, "0 %d 1\n", (int)me);
  put("/proc/self/uid_map", m);
  put("/proc/self/setgroups", "deny");
  snprintf(m, sizeof m, "0 %d 1\n", (int)g);
  put("/proc/self/gid_map", m);
  printf("%-30s %s\n", "unshare NEWNET", unshare(CLONE_NEWNET) ? strerror(errno) : "0");
  view("new netns (lo down)");
  lo_ioctl(1);
  view("lo up (ioctl)");
  lo_netlink(0);
  printf("after RTM_NEWLINK down:\n");
  netlink_view();
  lo_netlink(1);
  printf("after RTM_NEWLINK up:\n");
  netlink_view();
  reach();
  /* 中どうしの 127.0.0.1 の TCP */
  int l = socket(AF_INET, SOCK_STREAM, 0);
  struct sockaddr_in a = {.sin_family = AF_INET, .sin_port = htons(7402), .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
  int ok = !bind(l, (void *)&a, sizeof a) && !listen(l, 1);
  int c = socket(AF_INET, SOCK_STREAM, 0);
  ok = ok && !connect(c, (void *)&a, sizeof a);
  int x = accept(l, NULL, NULL);
  ok = ok && x >= 0 && write(c, "hi", 2) == 2;
  char b[4] = {0};
  ok = ok && read(x, b, 2) == 2 && !strcmp(b, "hi");
  printf("tcp inside 127.0.0.1 %s\n", ok ? "ok" : strerror(errno));
  return 0;
}

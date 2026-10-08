// inotify のテスト (カーネル): aios の中で動かす。作りかた:
//   zig cc -target aarch64-linux-musl -O2 -static -o inotify-test test/inotify.c
// ディレクトリを見張ると中のファイルの IN_ACCESS / IN_MODIFY が名前つきで届くこと、ファイルそのものの見張り。
// 読む速さも出す: 見張りなし、ほかのもの (IN_ACCESS を待たない) の見張りがあるとき、IN_ACCESS を待つとき
// (はじめの 2 つは同じくらいのはず。読むたびに親ディレクトリを探さない)
#include <stdio.h>
#include <fcntl.h>
#include <unistd.h>
#include <time.h>
#include <string.h>
#include <sys/inotify.h>
#include <sys/stat.h>
static double now(){struct timespec t;clock_gettime(CLOCK_MONOTONIC,&t);return t.tv_sec*1e6+t.tv_nsec/1e3;}
static double bench(const char*f){char b[1024];double best=1e18;for(int r=0;r<3;r++){int fd=open(f,O_RDONLY);long c=0;double t=now();while(read(fd,b,1024)>0)c++;double d=(now()-t)/c;close(fd);if(d<best)best=d;}return best;}
int main(){ int fail=0; mkdir("/tmp/iw",0755); int w=open("/tmp/iw/f",O_CREAT|O_WRONLY|O_TRUNC,0644); char z[200000]={0}; write(w,z,sizeof z); close(w);
  printf("no watch      %.1f us/read\n",bench("/tmp/iw/f"));
  int n=inotify_init1(IN_NONBLOCK); inotify_add_watch(n,"/etc",IN_CREATE|IN_DELETE);
  printf("other watch   %.1f us/read\n",bench("/tmp/iw/f"));
  int wd=inotify_add_watch(n,"/tmp/iw",IN_ACCESS|IN_MODIFY);
  printf("access watch  %.1f us/read\n",bench("/tmp/iw/f"));
  char buf[65536]; while(read(n,buf,sizeof buf)>0); // drain
  int fd=open("/tmp/iw/f",O_RDONLY); read(fd,buf,10); close(fd);
  fd=open("/tmp/iw/f",O_WRONLY); write(fd,"x",1); close(fd);
  int got=0,acc=0,mod=0; long k=read(n,buf,sizeof buf); for(char*p=buf;p<buf+k;){struct inotify_event*e=(void*)p; if(e->wd==wd&&!strcmp(e->name,"f")){if(e->mask&IN_ACCESS)acc=1;if(e->mask&IN_MODIFY)mod=1;} got++; p+=sizeof(*e)+e->len;}
  printf("%s access-event\n%s modify-event\n",acc?"ok":"FAIL",mod?"ok":"FAIL"); fail+=!acc+!mod;
  // self watch on file
  int n2=inotify_init1(IN_NONBLOCK); inotify_add_watch(n2,"/tmp/iw/f",IN_ACCESS); fd=open("/tmp/iw/f",O_RDONLY); read(fd,buf,10); close(fd);
  k=read(n2,buf,sizeof buf); printf("%s self-access\n",k>0?"ok":"FAIL"); fail+=k<=0;
  printf("inotify: %d failed\n",fail); return fail; }

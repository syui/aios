// System V の msg と sem が Linux と同じか (ipc/msg.c と ipc/sem.c):
//   msg  msgget (IPC_PRIVATE、IPC_CREAT と IPC_EXCL、ENOENT)、msgsnd (mtype が 0 以下は EINVAL、大きさ、IPC_NOWAIT で
//        いっぱいなら EAGAIN)、msgrcv (mtype 0 ははじめ、正はその型、負はそれ以下でいちばん小さい型、MSG_NOEXCEPT と E2BIG、
//        MSG_EXCEPT、IPC_NOWAIT で ENOMSG、待っていて別のプロセスが送る)、msgctl (IPC_STAT の数、IPC_SET、IPC_RMID で待って
//        いたものは EIDRM、IPC_INFO と MSG_INFO)
//   sem  semget (nsems の範囲、あるものより多い nsems は EINVAL)、semctl (SETVAL と GETVAL、SETALL と GETALL、GETPID、
//        GETNCNT と GETZCNT、IPC_STAT の sem_nsems、32767 をこえる値は ERANGE、IPC_RMID)、semop (足す・引く・0 を待つ、
//        IPC_NOWAIT で EAGAIN、まとめてするか何もしないか、sem_num が範囲の外は EFBIG、待っていて別のプロセスが上げる、
//        SEM_UNDO は終わったときにもどす)、semtimedop の時間切れ (EAGAIN)
#define _GNU_SOURCE
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/ipc.h>
#include <sys/msg.h>
#include <sys/sem.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static long e(long r) { return r < 0 ? -errno : r; }

struct m {
  long type;
  char text[64];
};

union semun {
  int val;
  struct semid_ds *buf;
  unsigned short *array;
  struct seminfo *info;
};

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  // ---- msg ----
  key_t k = 0x5a5a0000 + getpid() % 1000;
  int q = msgget(IPC_PRIVATE, 0600);
  printf("msgget (private ok %d, key missing %ld, create %d, excl %ld)\n", q >= 0, e(msgget(k, 0600)), e(msgget(k, IPC_CREAT | 0600)) >= 0, e(msgget(k, IPC_CREAT | IPC_EXCL | 0600)));
  msgctl(msgget(k, 0), IPC_RMID, NULL);
  struct m a = {3, "three"}, b = {1, "one"}, c = {2, "two"}, r;
  printf("msgsnd (type 0 %ld, negative size %ld, ok %ld %ld %ld)\n", e(msgsnd(q, &(struct m){0, "x"}, 2, 0)), e(msgsnd(q, &a, -1, 0)), e(msgsnd(q, &a, 6, 0)), e(msgsnd(q, &b, 4, 0)),
         e(msgsnd(q, &c, 4, 0)));
  struct msqid_ds ds;
  msgctl(q, IPC_STAT, &ds);
  printf("IPC_STAT (qnum %lu, cbytes %lu, qbytes %lu, mode %o)\n", (unsigned long)ds.msg_qnum, (unsigned long)ds.__msg_cbytes, (unsigned long)ds.msg_qbytes, ds.msg_perm.mode & 0777);
  memset(&r, 0, sizeof r);
  long n1 = e(msgrcv(q, &r, 64, -2, 0));
  printf("msgrcv -2 (%ld, type %ld, %s)\n", n1, r.type, r.text);
  memset(&r, 0, sizeof r);
  long n2 = e(msgrcv(q, &r, 2, 0, 0));
  printf("msgrcv 0 small (%ld) ", n2);
  long n3 = e(msgrcv(q, &r, 2, 0, MSG_NOERROR));
  printf("NOERROR (%ld, type %ld, %.2s) ", n3, r.type, r.text);
  long n4 = e(msgrcv(q, &r, 64, 2, MSG_EXCEPT | IPC_NOWAIT));
  printf("EXCEPT 2 (%ld) ", n4);
  long n5 = e(msgrcv(q, &r, 64, 9, IPC_NOWAIT));
  printf("missing type (%ld)\n", n5);
  // 待っているところへ子が送る
  pid_t k1 = fork();
  if (k1 == 0) {
    usleep(200000);
    struct m z = {7, "late"};
    msgsnd(q, &z, 5, 0);
    _exit(0);
  }
  memset(&r, 0, sizeof r);
  long n6 = e(msgrcv(q, &r, 64, 7, 0));
  waitpid(k1, NULL, 0);
  printf("blocking msgrcv (%ld, type %ld, %s)\n", n6, r.type, r.text);
  // いっぱい
  ds.msg_qbytes = 16;
  long setr = e(msgctl(q, IPC_SET, &ds));
  long f1 = e(msgsnd(q, &a, 10, IPC_NOWAIT)), f2 = e(msgsnd(q, &a, 10, IPC_NOWAIT));
  printf("IPC_SET qbytes 16 (%ld), full (%ld, %ld), too big %ld\n", setr, f1, f2, e(msgsnd(q, &a, 17, IPC_NOWAIT)));
  // 待っているものに IPC_RMID
  int q2 = msgget(IPC_PRIVATE, 0600);
  pid_t k2 = fork();
  if (k2 == 0) {
    struct m z;
    long rr = msgrcv(q2, &z, 64, 0, 0);
    _exit(rr < 0 && errno == EIDRM ? 0 : 1);
  }
  usleep(200000);
  msgctl(q2, IPC_RMID, NULL);
  int st;
  waitpid(k2, &st, 0);
  printf("IPC_RMID wakes the waiter with EIDRM %d, after rmid %ld\n", WIFEXITED(st) && WEXITSTATUS(st) == 0, e(msgsnd(q2, &a, 2, 0)));
  struct msginfo mi;
  long inf = e(msgctl(0, IPC_INFO, (struct msqid_ds *)&mi));
  printf("IPC_INFO (%d ok, msgmax %d, msgmnb %d, msgmni %d)\n", inf >= 0, mi.msgmax, mi.msgmnb, mi.msgmni);
  msgctl(q, IPC_RMID, NULL);
  // ---- sem ----
  printf("semget (nsems -1 %ld, 32001 %ld, 0 private %ld)\n", e(semget(IPC_PRIVATE, -1, 0600)), e(semget(IPC_PRIVATE, 32001, 0600)), e(semget(IPC_PRIVATE, 0, 0600)));
  int s = semget(IPC_PRIVATE, 3, 0600);
  key_t sk = k + 7;
  int s2 = semget(sk, 2, IPC_CREAT | 0600);
  printf("semget key (more nsems %ld, fewer %d)\n", e(semget(sk, 3, 0)), semget(sk, 1, 0) == s2);
  semctl(s2, 0, IPC_RMID);
  printf("SETVAL (%ld, 32768 %ld, -1 %ld), GETVAL %ld, sem_num 5 %ld\n", e(semctl(s, 0, SETVAL, (union semun){.val = 2})), e(semctl(s, 1, SETVAL, (union semun){.val = 32768})),
         e(semctl(s, 1, SETVAL, (union semun){.val = -1})), e(semctl(s, 0, GETVAL)), e(semctl(s, 5, GETVAL)));
  unsigned short all[3] = {5, 6, 7}, got[3] = {0};
  printf("SETALL %ld, GETALL %ld (%d %d %d), GETPID is me %d\n", e(semctl(s, 0, SETALL, (union semun){.array = all})), e(semctl(s, 0, GETALL, (union semun){.array = got})), got[0],
         got[1], got[2], semctl(s, 0, GETPID) == getpid());
  struct semid_ds sd;
  semctl(s, 0, IPC_STAT, (union semun){.buf = &sd});
  printf("IPC_STAT (nsems %lu, mode %o)\n", (unsigned long)sd.sem_nsems, sd.sem_perm.mode & 0777);
  struct sembuf ops[2] = {{0, -5, 0}, {1, -7, IPC_NOWAIT}};
  printf("semop all or nothing (%ld, sem0 still %ld)", e(semop(s, ops, 2)), e(semctl(s, 0, GETVAL)));
  struct sembuf one = {0, -2, 0};
  printf(", take 2 (%ld, now %ld), sem_num 9 %ld, nops 0 %ld\n", e(semop(s, &one, 1)), e(semctl(s, 0, GETVAL)), e(semop(s, &(struct sembuf){9, 1, 0}, 1)), e(semop(s, &one, 0)));
  // 0 を待つ / 待っていて子が上げる
  semctl(s, 2, SETVAL, (union semun){.val = 0});
  pid_t k3 = fork();
  if (k3 == 0) {
    usleep(200000);
    semop(s, &(struct sembuf){2, 3, 0}, 1);
    _exit(0);
  }
  struct sembuf w = {2, -3, 0};
  long wr = e(semop(s, &w, 1));
  waitpid(k3, NULL, 0);
  printf("blocking semop (%ld, now %ld)\n", wr, e(semctl(s, 2, GETVAL)));
  // GETNCNT: 子が待っているあいだ
  pid_t k4 = fork();
  if (k4 == 0) {
    semop(s, &(struct sembuf){2, -1, 0}, 1);
    _exit(0);
  }
  usleep(200000);
  long nc = e(semctl(s, 2, GETNCNT)), zc = e(semctl(s, 2, GETZCNT));
  semop(s, &(struct sembuf){2, 1, 0}, 1);
  waitpid(k4, NULL, 0);
  printf("GETNCNT %ld GETZCNT %ld\n", nc, zc);
  // SEM_UNDO
  semctl(s, 1, SETVAL, (union semun){.val = 10});
  pid_t k5 = fork();
  if (k5 == 0) {
    semop(s, &(struct sembuf){1, -4, SEM_UNDO}, 1);
    _exit(0);
  }
  waitpid(k5, NULL, 0);
  printf("SEM_UNDO back to %ld\n", e(semctl(s, 1, GETVAL)));
  struct timespec to = {0, 100000000};
  printf("semtimedop timeout %ld, NOWAIT %ld\n", e(semtimedop(s, &(struct sembuf){2, -5, 0}, 1, &to)), e(semop(s, &(struct sembuf){2, -5, IPC_NOWAIT}, 1)));
  struct seminfo si;
  long sinf = e(semctl(0, 0, IPC_INFO, (union semun){.info = &si}));
  printf("IPC_INFO (%d ok, semmsl %d, semmns %d, semopm %d, semmni %d, semvmx %d)\n", sinf >= 0, si.semmsl, si.semmns, si.semopm, si.semmni, si.semvmx);
  printf("IPC_RMID %ld, after %ld\n", e(semctl(s, 0, IPC_RMID)), e(semctl(s, 0, GETVAL)));
  return 0;
}

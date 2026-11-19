// cgshim: lets a real `systemd --user` run where the kernel gives it no cgroup filesystem.
//
// systemd refuses to start without a cgroup2 hierarchy it can write. Under proot on Android there is
// none, so /sys/fs/cgroup is a plain directory (droidc binds one there) and this LD_PRELOAD library
// makes systemd believe it is cgroup2:
//   - statfs/fstatfs on anything under /sys/fs/cgroup report CGROUP2_SUPER_MAGIC;
//   - /proc/<pid>/cgroup reads "0::<cgroup dir listing that pid>" (an in-memory file built from
//     the tree's cgroup.procs, so GetUnitByPID and sd_pid_get_unit work);
//   - mkdir under /sys/fs/cgroup also creates the cgroup.* files real cgroupfs makes on its own;
//   - rmdir removes those files first (real cgroupfs lets a "populated" tree directory go);
//   - readdir() reports DT_UNKNOWN for DT_LNK (proot's link2symlink files are symlinks to getdents only);
//   - prctl(PR_SET_SECUREBITS) that an app uid is refused reports success (the executor sets it for
//     every service);
//   - posix_spawn("/proc/self/fd/N"), how systemd 257 starts the executor, runs the file the fd names
//     instead (proot cannot execve through /proc/self/fd);
//   - posix_spawn(POSIX_SPAWN_SETCGROUP), which systemd 257 uses for every unit, is turned into a
//     plain spawn plus a write of the new pid to the unit's cgroup.procs;
//   - cgroup.procs is opened for append, so every pid systemd writes survives;
//   - cgroup.events starts as "populated 1" so a waiter sees a change when droidc's helper flips it.
// Membership and "populated" accounting are NOT done here: see crates/droidc/src/cgroups.rs.
// Build for the GUEST's libc (glibc), not the host's: see .github/workflows/ci.yml.
#define _GNU_SOURCE
#include <dirent.h>
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <stdarg.h>
#include <spawn.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/vfs.h>
#include <unistd.h>

#define CG2_MAGIC 0x63677270
#define ROOT "/sys/fs/cgroup"
#define NEXT(ret, name, args) \
  static ret(*real)args; \
  if (!real) real = dlsym(RTLD_NEXT, name)

static int under(const char *p) {
  return p && !strncmp(p, ROOT, sizeof(ROOT) - 1) &&
         (p[sizeof(ROOT) - 1] == 0 || p[sizeof(ROOT) - 1] == '/');
}

static int fd_under(int fd) {
  char link[64], buf[4096];
  snprintf(link, sizeof link, "/proc/self/fd/%d", fd);
  ssize_t n = readlink(link, buf, sizeof buf - 1);
  if (n < 0) return 0;
  buf[n] = 0;
  return under(buf);
}

static int all_digits(const char *s, size_t n) {
  if (!n) return 0;
  for (size_t i = 0; i < n; i++)
    if (s[i] < '0' || s[i] > '9') return 0;
  return 1;
}

// /proc/self/cgroup, /proc/thread-self/cgroup, /proc/<pid>/cgroup, /proc/<pid>/task/<tid>/cgroup.
// Returns 1 and the pid the file is about (0 = the caller) for those paths.
static int proc_cgroup_pid(const char *p, int *pid) {
  if (!p || strncmp(p, "/proc/", 6)) return 0;
  size_t n = strlen(p);
  if (n < 14 || strcmp(p + n - 7, "/cgroup")) return 0;
  const char *mid = p + 6;
  size_t len = n - 6 - 7;
  *pid = 0;
  if ((len == 4 && !strncmp(mid, "self", 4)) || (len == 11 && !strncmp(mid, "thread-self", 11)))
    return 1;
  const char *slash = memchr(mid, '/', len);
  size_t a = slash ? (size_t)(slash - mid) : len;
  if (!all_digits(mid, a)) return 0;
  if (slash && (strncmp(slash, "/task/", 6) || !all_digits(slash + 6, len - a - 6))) return 0;
  *pid = atoi(mid);
  return 1;
}

// systemd writes each pid to cgroup.procs at offset 0 of a plain file: successive writes would
// overwrite each other. Append instead so droidc's helper sees every pid (see cgroups.rs).
static int fix_flags(const char *p, int f) {
  size_t n = p ? strlen(p) : 0;
  if (under(p) && n > 13 && !strcmp(p + n - 13, "/cgroup.procs") && (f & O_ACCMODE) != O_RDONLY)
    f |= O_APPEND;
  return f;
}

// The cgroup directory (relative to ROOT) whose cgroup.procs lists `pid`, deepest first; the file
// is kept current by droidc's helper. Real cgroupfs answers this from the kernel.
static int find_cgroup(const char *dir, int pid, char *out, size_t n, int depth) {
  DIR *d = opendir(dir);
  if (!d) return 0;
  struct dirent *e;
  while (depth < 8 && (e = readdir(d))) {
    if (e->d_name[0] == '.') continue;
    char sub[4096];
    snprintf(sub, sizeof sub, "%s/%s", dir, e->d_name);
    struct stat st;
    if (stat(sub, &st) == 0 && S_ISDIR(st.st_mode) && find_cgroup(sub, pid, out, n, depth + 1)) {
      closedir(d);
      return 1;
    }
  }
  closedir(d);
  char f[4096];
  snprintf(f, sizeof f, "%s/cgroup.procs", dir);
  FILE *fp = fopen(f, "r");
  if (!fp) return 0;
  int v, hit = 0;
  while (!hit && fscanf(fp, "%d", &v) == 1) hit = v == pid;
  fclose(fp);
  if (hit) snprintf(out, n, "%s", dir + sizeof(ROOT) - 1);
  return hit;
}

static int fake_cgroup_fd(int pid) {
  char path[4096] = "";
  if (!pid) pid = getpid();
  if (!find_cgroup(ROOT, pid, path, sizeof path, 0) || !path[0]) strcpy(path, "/");
  char body[4200];
  int len = snprintf(body, sizeof body, "0::%s\n", path);
  int fd = syscall(SYS_memfd_create, "cgroup", 0);
  if (fd < 0) return -1;
  if (write(fd, body, len) != len) { close(fd); return -1; }
  lseek(fd, 0, SEEK_SET);
  return fd;
}

#define WRAP_STATFS(name, T) \
  int name(const char *p, T *b) { \
    NEXT(int, #name, (const char *, T *)); \
    int r = real(p, b); \
    if (!r && under(p)) b->f_type = CG2_MAGIC; \
    return r; \
  }
#define WRAP_FSTATFS(name, T) \
  int name(int fd, T *b) { \
    NEXT(int, #name, (int, T *)); \
    int r = real(fd, b); \
    if (!r && fd_under(fd)) b->f_type = CG2_MAGIC; \
    return r; \
  }
WRAP_STATFS(statfs, struct statfs)
WRAP_STATFS(statfs64, struct statfs64)
WRAP_FSTATFS(fstatfs, struct statfs)
WRAP_FSTATFS(fstatfs64, struct statfs64)

int open(const char *p, int f, ...) {
  NEXT(int, "open", (const char *, int, ...));
  va_list a; va_start(a, f); mode_t m = va_arg(a, mode_t); va_end(a);
  int cp; if (proc_cgroup_pid(p, &cp)) { int fd = fake_cgroup_fd(cp); if (fd >= 0) return fd; }
  return real(p, fix_flags(p, f), m);
}
int open64(const char *p, int f, ...) {
  NEXT(int, "open64", (const char *, int, ...));
  va_list a; va_start(a, f); mode_t m = va_arg(a, mode_t); va_end(a);
  int cp; if (proc_cgroup_pid(p, &cp)) { int fd = fake_cgroup_fd(cp); if (fd >= 0) return fd; }
  return real(p, fix_flags(p, f), m);
}
int openat(int d, const char *p, int f, ...) {
  NEXT(int, "openat", (int, const char *, int, ...));
  va_list a; va_start(a, f); mode_t m = va_arg(a, mode_t); va_end(a);
  int cp; if (proc_cgroup_pid(p, &cp)) { int fd = fake_cgroup_fd(cp); if (fd >= 0) return fd; }
  return real(d, p, fix_flags(p, f), m);
}
int openat64(int d, const char *p, int f, ...) {
  NEXT(int, "openat64", (int, const char *, int, ...));
  va_list a; va_start(a, f); mode_t m = va_arg(a, mode_t); va_end(a);
  int cp; if (proc_cgroup_pid(p, &cp)) { int fd = fake_cgroup_fd(cp); if (fd >= 0) return fd; }
  return real(d, p, fix_flags(p, f), m);
}
FILE *fopen(const char *p, const char *m) {
  NEXT(FILE *, "fopen", (const char *, const char *));
  int cp; if (proc_cgroup_pid(p, &cp)) { int fd = fake_cgroup_fd(cp); if (fd >= 0) return fdopen(fd, "r"); }
  return real(p, m);
}
FILE *fopen64(const char *p, const char *m) {
  NEXT(FILE *, "fopen64", (const char *, const char *));
  int cp; if (proc_cgroup_pid(p, &cp)) { int fd = fake_cgroup_fd(cp); if (fd >= 0) return fdopen(fd, "r"); }
  return real(p, m);
}

// systemd 257 starts every service through posix_spawn(POSIX_SPAWN_SETCGROUP) on an fd of the
// unit's cgroup (glibc 2.39: clone3 + CLONE_INTO_CGROUP). The kernel rejects a plain directory with
// EBADF, so every unit fails with "Failed to spawn executor". Do it the old way: drop the flag,
// remember the cgroup directory, spawn, and then write the new pid into its cgroup.procs, which is
// where this shim's accounting (droidc's cgroups.rs) learns about it.
#ifndef POSIX_SPAWN_SETCGROUP
#define POSIX_SPAWN_SETCGROUP 0x100
#endif
#define MAX_ATTRS 32
static struct { const void *attr; char path[1024]; } spawn_cg[MAX_ATTRS];  // main thread only

static void remember_cgroup(const void *attr, const char *path) {
  int free_slot = -1;
  for (int i = 0; i < MAX_ATTRS; i++) {
    if (spawn_cg[i].attr == attr) { snprintf(spawn_cg[i].path, sizeof spawn_cg[i].path, "%s", path); return; }
    if (!spawn_cg[i].attr && free_slot < 0) free_slot = i;
  }
  if (free_slot >= 0) {
    spawn_cg[free_slot].attr = attr;
    snprintf(spawn_cg[free_slot].path, sizeof spawn_cg[free_slot].path, "%s", path);
  }
}
static const char *recall_cgroup(const void *attr) {
  for (int i = 0; attr && i < MAX_ATTRS; i++)
    if (spawn_cg[i].attr == attr) return spawn_cg[i].path;
  return NULL;
}
static void forget_cgroup(const void *attr) {
  for (int i = 0; i < MAX_ATTRS; i++)
    if (spawn_cg[i].attr == attr) spawn_cg[i].attr = NULL;
}
static void attach(const char *cgroup, pid_t pid) {
  char f[1100], line[32];
  snprintf(f, sizeof f, "%s/cgroup.procs", cgroup);
  int fd = open(f, O_WRONLY | O_APPEND);  // our open(): append is forced anyway
  if (fd < 0) return;
  int n = snprintf(line, sizeof line, "%d\n", (int)pid);
  (void)!write(fd, line, n);
  close(fd);
}

// systemd 257 starts every service with posix_spawn("/proc/self/fd/N") on an fd of its executor
// (a plain file, or a sealed memfd copy). proot cannot execve through /proc/self/fd: its loader dies
// with status 182 before the program runs, so every unit fails. Run the file the fd names instead;
// for a memfd, the executor's usual path.
static const char *exec_path(const char *path, char *buf, size_t n) {
  if (!path || strncmp(path, "/proc/self/fd/", 14)) return path;
  char target[1024];
  ssize_t k = readlink(path, target, sizeof target - 1);
  if (k <= 0) return path;
  target[k] = 0;
  if (getenv("CGSHIM_TRACE")) fprintf(stderr, "cgshim: spawn %s -> %s\n", path, target);
  if (!strncmp(target, "/memfd:", 7)) {
    if (!strstr(target, "executor")) return path;
    const char *real = getenv("CGSHIM_EXECUTOR");
    snprintf(buf, n, "%s", real && *real ? real : "/usr/lib/systemd/systemd-executor");
  } else if (target[0] == '/') {
    char *deleted = strstr(target, " (deleted)");
    if (deleted) return path;
    snprintf(buf, n, "%s", target);
  } else {
    return path;
  }
  return access(buf, X_OK) == 0 ? buf : path;
}

int posix_spawnattr_setcgroup_np(posix_spawnattr_t *attr, int cgroup) {
  char link[64], buf[1024];
  snprintf(link, sizeof link, "/proc/self/fd/%d", cgroup);
  ssize_t n = readlink(link, buf, sizeof buf - 1);
  if (n > 0) { buf[n] = 0; remember_cgroup(attr, buf); }
  return 0;
}
int posix_spawnattr_setflags(posix_spawnattr_t *attr, short flags) {
  NEXT(int, "posix_spawnattr_setflags", (posix_spawnattr_t *, short));
  return real(attr, flags & ~POSIX_SPAWN_SETCGROUP);
}
int posix_spawnattr_destroy(posix_spawnattr_t *attr) {
  NEXT(int, "posix_spawnattr_destroy", (posix_spawnattr_t *));
  forget_cgroup(attr);
  return real(attr);
}
int posix_spawn(pid_t *pid, const char *path, const posix_spawn_file_actions_t *fa,
                const posix_spawnattr_t *attr, char *const argv[], char *const envp[]) {
  NEXT(int, "posix_spawn", (pid_t *, const char *, const posix_spawn_file_actions_t *,
                            const posix_spawnattr_t *, char *const[], char *const[]));
  char fixed[512];
  int r = real(pid, exec_path(path, fixed, sizeof fixed), fa, attr, argv, envp);
  const char *cg = recall_cgroup(attr);
  if (!r && cg && pid) attach(cg, *pid);
  return r;
}
int pidfd_spawn(int *pidfd, const char *path, const posix_spawn_file_actions_t *fa,
                const posix_spawnattr_t *attr, char *const argv[], char *const envp[]) {
  NEXT(int, "pidfd_spawn", (int *, const char *, const posix_spawn_file_actions_t *,
                            const posix_spawnattr_t *, char *const[], char *const[]));
  char fixed[512];
  int r = real(pidfd, exec_path(path, fixed, sizeof fixed), fa, attr, argv, envp);
  const char *cg = recall_cgroup(attr);
  if (!r && cg && pidfd) {
    char f[64], line[256];
    snprintf(f, sizeof f, "/proc/self/fdinfo/%d", *pidfd);
    FILE *fp = fopen(f, "r");
    while (fp && fgets(line, sizeof line, fp)) {
      int p;
      if (sscanf(line, "Pid: %d", &p) == 1) { attach(cg, p); break; }
    }
    if (fp) fclose(fp);
  }
  return r;
}

// proot's link2symlink (how hard links are emulated where Android forbids them) makes lstat() and
// readlink() treat such a file as regular, but getdents still reports the underlying symlink as
// DT_LNK. systemd takes DT_LNK at its word, readlink()s every unit file, gets EINVAL and loads no
// units at all. DT_UNKNOWN makes callers stat() instead, which gives the consistent answer.
struct dirent *readdir(DIR *d) {
  NEXT(struct dirent *, "readdir", (DIR *));
  struct dirent *e = real(d);
  if (e && e->d_type == DT_LNK) e->d_type = DT_UNKNOWN;
  return e;
}
struct dirent64 *readdir64(DIR *d) {
  NEXT(struct dirent64 *, "readdir64", (DIR *));
  struct dirent64 *e = real(d);
  if (e && e->d_type == DT_LNK) e->d_type = DT_UNKNOWN;
  return e;
}

// The executor calls prctl(PR_SET_SECUREBITS) for every service, which an app uid (no CAP_SETPCAP)
// is refused with EPERM: the unit then fails with status 213/SECUREBITS before it starts. There are
// no capabilities here to lock down, so say it worked.
#ifndef PR_SET_SECUREBITS
#define PR_SET_SECUREBITS 28
#endif
int prctl(int option, ...) {
  NEXT(int, "prctl", (int, ...));
  va_list a;
  va_start(a, option);
  unsigned long a2 = va_arg(a, unsigned long), a3 = va_arg(a, unsigned long),
                a4 = va_arg(a, unsigned long), a5 = va_arg(a, unsigned long);
  va_end(a);
  int r = real(option, a2, a3, a4, a5);
  if (r < 0 && errno == EPERM && option == PR_SET_SECUREBITS) {
    if (getenv("CGSHIM_TRACE")) fprintf(stderr, "cgshim: PR_SET_SECUREBITS %#lx refused, pretending\n", a2);
    return 0;
  }
  return r;
}

// Debugging aid, silent unless CGSHIM_TRACE is set: say which unix socket a failed connect() wanted.
int connect(int fd, const struct sockaddr *addr, socklen_t len) {
  NEXT(int, "connect", (int, const struct sockaddr *, socklen_t));
  int r = real(fd, addr, len);
  if (r < 0 && addr && addr->sa_family == AF_UNIX && getenv("CGSHIM_TRACE")) {
    int e = errno;
    const struct sockaddr_un *u = (const struct sockaddr_un *)addr;
    fprintf(stderr, "cgshim: connect(%.*s) failed: %s\n", (int)(len > 2 ? len - 2 : 0), u->sun_path[0] ? u->sun_path : u->sun_path + 1, strerror(e));
    errno = e;
  }
  return r;
}

static const char *const FILES[] = {"cgroup.procs", "cgroup.controllers", "cgroup.subtree_control",
                                    "cgroup.events", "cgroup.type", "cgroup.freeze", "cgroup.kill",
                                    "cgroup.max.depth", "cgroup.max.descendants"};
#define NFILES (sizeof FILES / sizeof *FILES)

static void populate(const char *dir) {
  char b[4096];
  for (size_t i = 0; i < NFILES; i++) {
    snprintf(b, sizeof b, "%s/%s", dir, FILES[i]);
    int fd = creat(b, 0644);
    if (fd < 0) continue;
    if (!strcmp(FILES[i], "cgroup.events")) (void)!write(fd, "populated 1\nfrozen 0\n", 21);
    close(fd);
  }
}

static void unpopulate(const char *dir) {
  char b[4096];
  for (size_t i = 0; i < NFILES; i++) {
    snprintf(b, sizeof b, "%s/%s", dir, FILES[i]);
    unlink(b);
  }
}

static int abs_under(int d, const char *p, char *out, size_t n) {
  if (!p) return 0;
  if (p[0] == '/') { snprintf(out, n, "%s", p); return under(out); }
  if (d == AT_FDCWD || !fd_under(d)) return 0;
  char link[64], base[4096];
  snprintf(link, sizeof link, "/proc/self/fd/%d", d);
  ssize_t k = readlink(link, base, sizeof base - 1);
  if (k < 0) return 0;
  base[k] = 0;
  snprintf(out, n, "%s/%s", base, p);
  return 1;
}

int mkdir(const char *p, mode_t m) {
  NEXT(int, "mkdir", (const char *, mode_t));
  int r = real(p, m);
  if (!r && under(p)) populate(p);
  return r;
}
int mkdirat(int d, const char *p, mode_t m) {
  NEXT(int, "mkdirat", (int, const char *, mode_t));
  int r = real(d, p, m);
  char full[8192];
  if (!r && abs_under(d, p, full, sizeof full)) populate(full);
  return r;
}
int rmdir(const char *p) {
  NEXT(int, "rmdir", (const char *));
  if (under(p)) unpopulate(p);
  return real(p);
}
int unlinkat(int d, const char *p, int flags) {
  NEXT(int, "unlinkat", (int, const char *, int));
  char full[8192];
  if ((flags & AT_REMOVEDIR) && abs_under(d, p, full, sizeof full)) unpopulate(full);
  return real(d, p, flags);
}

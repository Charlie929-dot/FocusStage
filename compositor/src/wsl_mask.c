#define _GNU_SOURCE
#define _LARGEFILE64_SOURCE

#include <ctype.h>
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/utsname.h>
#include <unistd.h>

static int starts_with(const char *s, const char *prefix) {
    size_t len = strlen(prefix);
    return strncmp(s, prefix, len) == 0;
}

static int is_hidden_wsl_path(const char *path) {
    if (!path || path[0] != '/') {
        return 0;
    }
    if (strcmp(path, "/run/WSL") == 0 || starts_with(path, "/run/WSL/")) {
        return 1;
    }
    if (strcmp(path, "/proc/sys/fs/binfmt_misc/WSLInterop") == 0 ||
        strcmp(path, "/proc/sys/fs/binfmt_misc/WSLInterop-late") == 0) {
        return 1;
    }
    return 0;
}

static int is_sanitized_proc_path(const char *path) {
    if (!path || path[0] != '/') {
        return 0;
    }
    return strcmp(path, "/proc/version") == 0 ||
           strcmp(path, "/proc/sys/kernel/osrelease") == 0;
}

static int should_sanitize_open(const char *path, int flags) {
    return (flags & (O_PATH | O_ACCMODE)) == O_RDONLY && is_sanitized_proc_path(path);
}

static int strncase_eq(const char *a, const char *b, size_t n) {
    for (size_t i = 0; i < n; i++) {
        if (tolower((unsigned char)a[i]) != tolower((unsigned char)b[i])) {
            return 0;
        }
    }
    return 1;
}

static void replace_ci(char *buf, size_t cap, const char *pat, const char *repl) {
    char tmp[1024];
    size_t pat_len = strlen(pat);
    size_t repl_len = strlen(repl);
    size_t src_i = 0;
    size_t dst_i = 0;

    if (cap == 0 || pat_len == 0) {
        return;
    }

    while (buf[src_i] != '\0' && dst_i + 1 < sizeof(tmp)) {
        if (strncase_eq(&buf[src_i], pat, pat_len)) {
            size_t copy_len = repl_len;
            if (dst_i + copy_len >= sizeof(tmp)) {
                copy_len = sizeof(tmp) - 1 - dst_i;
            }
            memcpy(&tmp[dst_i], repl, copy_len);
            dst_i += copy_len;
            src_i += pat_len;
        } else {
            tmp[dst_i++] = buf[src_i++];
        }
    }
    tmp[dst_i] = '\0';

    size_t final_len = dst_i < cap - 1 ? dst_i : cap - 1;
    memcpy(buf, tmp, final_len);
    buf[final_len] = '\0';
}

static void sanitize_kernel_text(char *buf, size_t cap) {
    replace_ci(buf, cap, "-microsoft-standard-WSL2", "-generic");
    replace_ci(buf, cap, "-microsoft-standard", "-generic");
    replace_ci(buf, cap, "-microsoft-WSL2", "-generic");
    replace_ci(buf, cap, "-WSL2", "");
    replace_ci(buf, cap, "microsoft", "linux");
    replace_ci(buf, cap, "WSL", "LNX");
}

static _Atomic(int (*)(struct utsname *)) next_uname;
static _Atomic(int (*)(const char *, int, ...)) next_open;
static _Atomic(int (*)(const char *, int, ...)) next_open64;
static _Atomic(int (*)(int, const char *, int, ...)) next_openat;
static _Atomic(int (*)(int, const char *, int, ...)) next_openat64;
static _Atomic(FILE *(*)(const char *, const char *)) next_fopen;
static _Atomic(FILE *(*)(const char *, const char *)) next_fopen64;
static _Atomic(int (*)(const char *, int)) next_access;
static _Atomic(int (*)(int, const char *, int, int)) next_faccessat;
static _Atomic(int (*)(const char *, struct stat *)) next_stat;
static _Atomic(int (*)(const char *, struct stat *)) next_lstat;
static _Atomic(int (*)(const char *, struct stat64 *)) next_stat64;
static _Atomic(int (*)(const char *, struct stat64 *)) next_lstat64;
static _Atomic(int (*)(int, const char *, int, unsigned int, struct statx *)) next_statx;
static _Atomic(int (*)(int, const char *, struct stat *)) next___xstat;
static _Atomic(int (*)(int, const char *, struct stat *)) next___lxstat;
static _Atomic(int (*)(int, const char *, struct stat64 *)) next___xstat64;
static _Atomic(int (*)(int, const char *, struct stat64 *)) next___lxstat64;

static int open_sanitized_proc(int real_fd, int flags) {
    if (real_fd < 0) {
        return real_fd;
    }

    char original[1024];
    ssize_t total;
    do {
        total = pread(real_fd, original, sizeof(original) - 1, 0);
    } while (total < 0 && errno == EINTR);
    if (total <= 0 || total == sizeof(original) - 1) {
        return real_fd;
    }
    original[total] = '\0';
    char buf[sizeof(original)];
    memcpy(buf, original, (size_t)total + 1);
    sanitize_kernel_text(buf, sizeof(buf));
    if (strcmp(buf, original) == 0) {
        return real_fd;
    }
    size_t out_len = strlen(buf);

    int mfd = memfd_create("fs_proc", MFD_CLOEXEC);
    if (mfd < 0) {
        return real_fd;
    }
    size_t offset = 0;
    while (offset < out_len) {
        ssize_t written = write(mfd, buf + offset, out_len - offset);
        if (written < 0 && errno == EINTR) {
            continue;
        }
        if (written <= 0) {
            close(mfd);
            return real_fd;
        }
        offset += (size_t)written;
    }

    char fd_path[64];
    snprintf(fd_path, sizeof(fd_path), "/proc/self/fd/%d", mfd);
    int masked_fd = open(fd_path, O_RDONLY | (flags & (O_CLOEXEC | O_NONBLOCK)));
    close(mfd);
    if (masked_fd < 0) {
        return real_fd;
    }
    close(real_fd);
    return masked_fd;
}

int uname(struct utsname *buf) {
    if (!next_uname) {
        next_uname = dlsym(RTLD_NEXT, "uname");
    }
    if (!next_uname) {
        errno = ENOSYS;
        return -1;
    }
    int rc = next_uname(buf);
    if (rc == 0 && buf) {
        sanitize_kernel_text(buf->release, sizeof(buf->release));
        sanitize_kernel_text(buf->version, sizeof(buf->version));
    }
    return rc;
}

int open(const char *pathname, int flags, ...) {
    if (!next_open) {
        next_open = dlsym(RTLD_NEXT, "open");
    }
    mode_t mode = 0;
    if ((flags & O_CREAT) != 0
#ifdef O_TMPFILE
        || (flags & O_TMPFILE) == O_TMPFILE
#endif
    ) {
        va_list ap;
        va_start(ap, flags);
        mode = (mode_t)va_arg(ap, int);
        va_end(ap);
    }
    if (is_hidden_wsl_path(pathname)) {
        errno = ENOENT;
        return -1;
    }
    int fd = next_open(pathname, flags, mode);
    if (fd >= 0 && should_sanitize_open(pathname, flags)) {
        return open_sanitized_proc(fd, flags);
    }
    return fd;
}

int open64(const char *pathname, int flags, ...) {
    if (!next_open64) {
        next_open64 = dlsym(RTLD_NEXT, "open64");
    }
    mode_t mode = 0;
    if ((flags & O_CREAT) != 0
#ifdef O_TMPFILE
        || (flags & O_TMPFILE) == O_TMPFILE
#endif
    ) {
        va_list ap;
        va_start(ap, flags);
        mode = (mode_t)va_arg(ap, int);
        va_end(ap);
    }
    if (is_hidden_wsl_path(pathname)) {
        errno = ENOENT;
        return -1;
    }
    int fd = next_open64(pathname, flags, mode);
    if (fd >= 0 && should_sanitize_open(pathname, flags)) {
        return open_sanitized_proc(fd, flags);
    }
    return fd;
}

int openat(int dirfd, const char *pathname, int flags, ...) {
    if (!next_openat) {
        next_openat = dlsym(RTLD_NEXT, "openat");
    }
    mode_t mode = 0;
    if ((flags & O_CREAT) != 0
#ifdef O_TMPFILE
        || (flags & O_TMPFILE) == O_TMPFILE
#endif
    ) {
        va_list ap;
        va_start(ap, flags);
        mode = (mode_t)va_arg(ap, int);
        va_end(ap);
    }
    if (is_hidden_wsl_path(pathname)) {
        errno = ENOENT;
        return -1;
    }
    int fd = next_openat(dirfd, pathname, flags, mode);
    if (fd >= 0 && should_sanitize_open(pathname, flags)) {
        return open_sanitized_proc(fd, flags);
    }
    return fd;
}

int openat64(int dirfd, const char *pathname, int flags, ...) {
    if (!next_openat64) {
        next_openat64 = dlsym(RTLD_NEXT, "openat64");
    }
    mode_t mode = 0;
    if ((flags & O_CREAT) != 0
#ifdef O_TMPFILE
        || (flags & O_TMPFILE) == O_TMPFILE
#endif
    ) {
        va_list ap;
        va_start(ap, flags);
        mode = (mode_t)va_arg(ap, int);
        va_end(ap);
    }
    if (is_hidden_wsl_path(pathname)) {
        errno = ENOENT;
        return -1;
    }
    int fd = next_openat64(dirfd, pathname, flags, mode);
    if (fd >= 0 && should_sanitize_open(pathname, flags)) {
        return open_sanitized_proc(fd, flags);
    }
    return fd;
}

FILE *fopen(const char *pathname, const char *mode) {
    if (!next_fopen) {
        next_fopen = dlsym(RTLD_NEXT, "fopen");
    }
    if (is_hidden_wsl_path(pathname)) {
        errno = ENOENT;
        return NULL;
    }
    if (mode && mode[0] == 'r' && !strchr(mode, '+') && is_sanitized_proc_path(pathname)) {
        if (!next_open) {
            next_open = dlsym(RTLD_NEXT, "open");
        }
        int flags = O_RDONLY | (strchr(mode, 'e') ? O_CLOEXEC : 0);
        int real_fd = next_open(pathname, flags);
        int masked_fd = open_sanitized_proc(real_fd, flags);
        if (masked_fd < 0) {
            return NULL;
        }
        FILE *fp = fdopen(masked_fd, mode);
        if (!fp) {
            close(masked_fd);
        }
        return fp;
    }
    return next_fopen(pathname, mode);
}

FILE *fopen64(const char *pathname, const char *mode) {
    if (!next_fopen64) {
        next_fopen64 = dlsym(RTLD_NEXT, "fopen64");
    }
    if (is_hidden_wsl_path(pathname)) {
        errno = ENOENT;
        return NULL;
    }
    if (mode && mode[0] == 'r' && !strchr(mode, '+') && is_sanitized_proc_path(pathname)) {
        if (!next_open64) {
            next_open64 = dlsym(RTLD_NEXT, "open64");
        }
        int flags = O_RDONLY | (strchr(mode, 'e') ? O_CLOEXEC : 0);
        int real_fd = next_open64(pathname, flags);
        int masked_fd = open_sanitized_proc(real_fd, flags);
        if (masked_fd < 0) {
            return NULL;
        }
        FILE *fp = fdopen(masked_fd, mode);
        if (!fp) {
            close(masked_fd);
        }
        return fp;
    }
    return next_fopen64(pathname, mode);
}

int access(const char *pathname, int mode) {
    if (!next_access) {
        next_access = dlsym(RTLD_NEXT, "access");
    }
    if (is_hidden_wsl_path(pathname)) {
        errno = ENOENT;
        return -1;
    }
    return next_access(pathname, mode);
}

int faccessat(int dirfd, const char *pathname, int mode, int flags) {
    if (!next_faccessat) {
        next_faccessat = dlsym(RTLD_NEXT, "faccessat");
    }
    if (is_hidden_wsl_path(pathname)) {
        errno = ENOENT;
        return -1;
    }
    return next_faccessat(dirfd, pathname, mode, flags);
}

int stat(const char *pathname, struct stat *statbuf) {
    if (!next_stat) {
        next_stat = dlsym(RTLD_NEXT, "stat");
    }
    if (is_hidden_wsl_path(pathname)) {
        errno = ENOENT;
        return -1;
    }
    return next_stat(pathname, statbuf);
}

int lstat(const char *pathname, struct stat *statbuf) {
    if (!next_lstat) {
        next_lstat = dlsym(RTLD_NEXT, "lstat");
    }
    if (is_hidden_wsl_path(pathname)) {
        errno = ENOENT;
        return -1;
    }
    return next_lstat(pathname, statbuf);
}

int stat64(const char *pathname, struct stat64 *statbuf) {
    if (!next_stat64) {
        next_stat64 = dlsym(RTLD_NEXT, "stat64");
    }
    if (is_hidden_wsl_path(pathname)) {
        errno = ENOENT;
        return -1;
    }
    return next_stat64(pathname, statbuf);
}

int lstat64(const char *pathname, struct stat64 *statbuf) {
    if (!next_lstat64) {
        next_lstat64 = dlsym(RTLD_NEXT, "lstat64");
    }
    if (is_hidden_wsl_path(pathname)) {
        errno = ENOENT;
        return -1;
    }
    return next_lstat64(pathname, statbuf);
}

int statx(int dirfd, const char *pathname, int flags, unsigned int mask, struct statx *statxbuf) {
    if (!next_statx) {
        next_statx = dlsym(RTLD_NEXT, "statx");
    }
    if (is_hidden_wsl_path(pathname)) {
        errno = ENOENT;
        return -1;
    }
    return next_statx(dirfd, pathname, flags, mask, statxbuf);
}

int __xstat(int ver, const char *pathname, struct stat *statbuf) {
    if (!next___xstat) {
        next___xstat = dlsym(RTLD_NEXT, "__xstat");
    }
    if (is_hidden_wsl_path(pathname)) {
        errno = ENOENT;
        return -1;
    }
    return next___xstat(ver, pathname, statbuf);
}

int __lxstat(int ver, const char *pathname, struct stat *statbuf) {
    if (!next___lxstat) {
        next___lxstat = dlsym(RTLD_NEXT, "__lxstat");
    }
    if (is_hidden_wsl_path(pathname)) {
        errno = ENOENT;
        return -1;
    }
    return next___lxstat(ver, pathname, statbuf);
}

int __xstat64(int ver, const char *pathname, struct stat64 *statbuf) {
    if (!next___xstat64) {
        next___xstat64 = dlsym(RTLD_NEXT, "__xstat64");
    }
    if (is_hidden_wsl_path(pathname)) {
        errno = ENOENT;
        return -1;
    }
    return next___xstat64(ver, pathname, statbuf);
}

int __lxstat64(int ver, const char *pathname, struct stat64 *statbuf) {
    if (!next___lxstat64) {
        next___lxstat64 = dlsym(RTLD_NEXT, "__lxstat64");
    }
    if (is_hidden_wsl_path(pathname)) {
        errno = ENOENT;
        return -1;
    }
    return next___lxstat64(ver, pathname, statbuf);
}

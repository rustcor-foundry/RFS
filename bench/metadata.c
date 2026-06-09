/* Metadata micro-benchmark: create / stat / unlink N empty files in a dir,
 * via real syscalls. Mirrors the rfs-bench engine metadata phase so the FUSE
 * and kernel-fs numbers are comparable. Usage: metadata <dir> <n> */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <fcntl.h>
#include <unistd.h>
#include <sys/stat.h>
#include <time.h>

static double now(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (double)t.tv_sec + (double)t.tv_nsec / 1e9;
}

int main(int argc, char **argv) {
    if (argc < 3) { fprintf(stderr, "usage: %s <dir> <n>\n", argv[0]); return 2; }
    const char *dir = argv[1];
    long n = atol(argv[2]);
    char path[4096];

    double t0 = now();
    for (long i = 0; i < n; i++) {
        snprintf(path, sizeof path, "%s/f%08ld", dir, i);
        int fd = open(path, O_CREAT | O_WRONLY, 0644);
        if (fd < 0) { perror("open"); return 1; }
        close(fd);
    }
    sync();
    double t1 = now();

    struct stat st;
    for (long i = 0; i < n; i++) {
        snprintf(path, sizeof path, "%s/f%08ld", dir, i);
        if (stat(path, &st) < 0) { perror("stat"); return 1; }
    }
    double t2 = now();

    for (long i = 0; i < n; i++) {
        snprintf(path, sizeof path, "%s/f%08ld", dir, i);
        if (unlink(path) < 0) { perror("unlink"); return 1; }
    }
    sync();
    double t3 = now();

    printf("meta_create_ops_s %.1f\n", n / (t1 - t0));
    printf("meta_stat_ops_s %.1f\n", n / (t2 - t1));
    printf("meta_unlink_ops_s %.1f\n", n / (t3 - t2));
    return 0;
}

/* fsx-style filesystem exerciser: random write/read/truncate on one file,
 * checked against an in-memory oracle after every read. Catches data-integrity
 * bugs in the read/write/truncate paths. Usage: fsxlite <file> <numops> <seed> */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>

#define MAXLEN (256 * 1024)
#define MAXOP (64 * 1024)

static unsigned char good[MAXLEN];
static unsigned char buf[MAXLEN];
static long flen = 0;

static long rnd(long n) { return (long)((((unsigned long)rand() << 15) ^ rand()) % (unsigned long)n); }

int main(int argc, char **argv) {
    if (argc < 4) { fprintf(stderr, "usage: fsxlite file numops seed [verbose]\n"); return 2; }
    const char *path = argv[1];
    long nops = atol(argv[2]);
    int verbose = argc > 4;
    srand((unsigned)atol(argv[3]));

    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0644);
    if (fd < 0) { perror("open"); return 1; }

    for (long op = 1; op <= nops; op++) {
        int choice = rand() % 10;
        if (choice < 5) { /* write */
            long off = rnd(MAXLEN);
            long room = MAXLEN - off;
            long len = 1 + rnd(room > MAXOP ? MAXOP : room);
            if (off > flen) memset(good + flen, 0, (size_t)(off - flen)); /* write past EOF: gap is a hole (zeros) */
            for (long k = 0; k < len; k++) { unsigned char v = (unsigned char)(op + k); good[off + k] = v; buf[k] = v; }
            if (verbose) fprintf(stderr, "op %ld W off %ld len %ld\n", op, off, len);
            if (pwrite(fd, buf, len, off) != len) { perror("pwrite"); return 1; }
            if (off + len > flen) flen = off + len;
        } else if (choice < 7) { /* truncate */
            long nl = rnd(MAXLEN + 1);
            if (verbose) fprintf(stderr, "op %ld T %ld (was %ld)\n", op, nl, flen);
            if (ftruncate(fd, nl) != 0) { perror("ftruncate"); return 1; }
            if (nl > flen) memset(good + flen, 0, (size_t)(nl - flen));
            flen = nl;
        } else { /* read + verify */
            if (flen == 0) continue;
            long off = rnd(flen);
            long room = flen - off;
            long len = 1 + rnd(room > MAXOP ? MAXOP : room);
            if (verbose) fprintf(stderr, "op %ld R off %ld len %ld\n", op, off, len);
            long r = pread(fd, buf, len, off);
            if (r != len) { fprintf(stderr, "op %ld: short read %ld != %ld\n", op, r, len); return 1; }
            if (memcmp(buf, good + off, (size_t)len) != 0) {
                fprintf(stderr, "op %ld: DATA MISMATCH off %ld len %ld\n", op, off, len);
                for (long k = 0; k < len; k++)
                    if (buf[k] != good[off + k]) { fprintf(stderr, "  +%ld got %u want %u\n", k, buf[k], good[off + k]); break; }
                return 1;
            }
        }
    }

    /* final full verification */
    if (flen > 0) {
        long r = pread(fd, buf, flen, 0);
        if (r != flen) { fprintf(stderr, "final short read %ld != %ld\n", r, flen); return 1; }
        if (memcmp(buf, good, (size_t)flen) != 0) { fprintf(stderr, "final MISMATCH\n"); return 1; }
    }
    close(fd);
    printf("fsxlite OK: %ld ops, final size %ld\n", nops, flen);
    return 0;
}

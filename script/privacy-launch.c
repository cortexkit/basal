// Normal mode execs the tests in an explicitly self-responsible process.
// Orphan mode plain-spawns them from that process and then exits, to test
// responsibility queries after the original responsible ancestor is gone.
#include <dlfcn.h>
#include <spawn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

extern char **environ;

int main(int argc, char **argv) {
    if (argc < 2) return 2;
    if (strcmp(argv[1], "--originator") == 0) {
        if (argc < 3) return 2;
        char owner[32];
        snprintf(owner, sizeof(owner), "%d", getpid());
        if (setenv("BASAL_PRIVACY_ORIGINATOR", owner, 1) != 0) return 5;
        int (*responsible)(pid_t) = dlsym(RTLD_DEFAULT, "responsibility_get_pid_responsible_for_pid");
        fprintf(stderr, "ORIGINATOR_PROBE pid=%d responsible=%d\n", getpid(), responsible ? responsible(getpid()) : -1);
        pid_t child;
        int result = posix_spawn(&child, argv[2], NULL, NULL, argv + 2, environ);
        fprintf(stderr, "ORIGINATOR_PROBE child=%d result=%d\n", result ? -1 : child, result);
        return result;
    }
    const char *target = argv[1];
    char **args = argv + 1;
    int inherited = strcmp(argv[1], "--orphan-inherited") == 0;
    if (strcmp(argv[1], "--orphan") == 0 || inherited) {
        if (argc < 3) return 2;
        argv[1] = "--originator";
        target = argv[0];
        args = argv;
    }
    int (*disclaim)(posix_spawnattr_t *, int) = dlsym(RTLD_DEFAULT, "responsibility_spawnattrs_setdisclaim");
    if (!disclaim) return 3;
    posix_spawnattr_t attr;
    int result = posix_spawnattr_init(&attr);
    if (result) return result;
    result = posix_spawnattr_setflags(&attr, POSIX_SPAWN_SETEXEC);
    if (!result && !inherited) result = disclaim(&attr, 1);
    if (!result) result = posix_spawn(NULL, target, NULL, &attr, args, environ);
    fprintf(stderr, "self-responsible test launch failed: %d\n", result);
    posix_spawnattr_destroy(&attr);
    return result ? result : 4;
}

// Give the test observer an explicitly self-responsible identity, matching the
// reported failure instead of inheriting the GitHub runner agent's identity.
#include <dlfcn.h>
#include <spawn.h>
#include <stdio.h>
#include <unistd.h>

extern char **environ;

int main(int argc, char **argv) {
    if (argc < 2) return 2;
    int (*disclaim)(posix_spawnattr_t *, int) = dlsym(RTLD_DEFAULT, "responsibility_spawnattrs_setdisclaim");
    if (!disclaim) return 3;
    posix_spawnattr_t attr;
    int result = posix_spawnattr_init(&attr);
    if (result) return result;
    result = posix_spawnattr_setflags(&attr, POSIX_SPAWN_SETEXEC);
    if (!result) result = disclaim(&attr, 1);
    if (!result) result = posix_spawn(NULL, argv[1], NULL, &attr, argv + 1, environ);
    fprintf(stderr, "self-responsible test launch failed: %d\n", result);
    posix_spawnattr_destroy(&attr);
    return result ? result : 4;
}

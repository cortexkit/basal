// Temporary CI instrumentation: record the actual libc entry point used by
// Rust's Command without adding a pre_exec hook that would force fork itself.
#include <spawn.h>
#include <stdio.h>
#include <sys/time.h>
#include <unistd.h>

extern char **environ;

static int traced_posix_spawn(pid_t *pid, const char *path,
                              const posix_spawn_file_actions_t *actions,
                              const posix_spawnattr_t *attr,
                              char *const argv[], char *const envp[]) {
    int result = posix_spawn(pid, path, actions, attr, argv, envp);
    struct timeval now;
    gettimeofday(&now, NULL);
    fprintf(stderr, "SPAWN_PROBE observer=%d path=%s child=%d result=%d entry=posix_spawn time_us=%lld\n",
            getpid(), path, result == 0 && pid != NULL ? *pid : -1, result,
            (long long)now.tv_sec * 1000000 + now.tv_usec);
    return result;
}

static pid_t traced_fork(void) {
    pid_t result = fork();
    if (result != 0) {
        fprintf(stderr, "SPAWN_PROBE observer=%d child=%d entry=fork\n", getpid(), result);
    }
    return result;
}

#define INTERPOSE(replacement, original) \
    __attribute__((used)) static struct { const void *new_fn; const void *old_fn; } \
    interpose_##original __attribute__((section("__DATA,__interpose"))) = \
        { (const void *)&replacement, (const void *)&original }

INTERPOSE(traced_posix_spawn, posix_spawn);
INTERPOSE(traced_fork, fork);

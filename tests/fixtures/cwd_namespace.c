#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

static void fail(const char *operation) {
    perror(operation);
    exit(1);
}

static void require_directory(const char *path) {
    if (mkdir(path, 0700) < 0 && errno != EEXIST) {
        fail("mkdir");
    }
}

static void require_cwd(const char *expected) {
    char cwd[PATH_MAX];
    char proc_cwd[PATH_MAX];

    if (getcwd(cwd, sizeof(cwd)) == NULL) {
        fail("getcwd");
    }
    if (strcmp(cwd, expected) != 0) {
        fprintf(stderr, "getcwd: expected %s, got %s\n", expected, cwd);
        exit(1);
    }

    ssize_t length = readlink("/proc/self/cwd", proc_cwd, sizeof(proc_cwd) - 1);
    if (length < 0) {
        fail("readlink /proc/self/cwd");
    }
    proc_cwd[length] = '\0';
    if (strcmp(proc_cwd, expected) != 0) {
        fprintf(stderr, "/proc/self/cwd: expected %s, got %s\n", expected,
                proc_cwd);
        exit(1);
    }
}

static void *change_thread_cwd(void *unused) {
    (void)unused;
    require_directory("/workspace/thread");
    if (chdir("/workspace/thread") < 0) {
        fail("thread chdir");
    }
    require_cwd("/workspace/thread");
    return NULL;
}

int main(void) {
    require_cwd("/workspace/start");

    require_directory("/workspace/fd-target");
    int fd = open("/workspace/fd-target", O_RDONLY | O_DIRECTORY);
    if (fd < 0) {
        fail("open directory");
    }
    if (fchdir(fd) < 0) {
        fail("fchdir");
    }
    close(fd);
    require_cwd("/workspace/fd-target");

    pthread_t thread;
    if (pthread_create(&thread, NULL, change_thread_cwd, NULL) != 0) {
        fail("pthread_create");
    }
    if (pthread_join(thread, NULL) != 0) {
        fail("pthread_join");
    }
    require_cwd("/workspace/thread");

    pid_t child = fork();
    if (child < 0) {
        fail("fork");
    }
    if (child == 0) {
        require_directory("/workspace/child");
        if (chdir("/workspace/child") < 0) {
            fail("child chdir");
        }
        require_cwd("/workspace/child");
        _exit(0);
    }

    int status;
    if (waitpid(child, &status, 0) < 0) {
        fail("waitpid");
    }
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        fprintf(stderr, "child failed: status=%d\n", status);
        return 1;
    }
    require_cwd("/workspace/thread");

    int output = open("relative-output", O_WRONLY | O_CREAT | O_TRUNC, 0600);
    if (output < 0) {
        fail("open relative output");
    }
    const char contents[] = "thread cwd\n";
    if (write(output, contents, sizeof(contents) - 1) !=
        (ssize_t)(sizeof(contents) - 1)) {
        fail("write relative output");
    }
    close(output);
    return 0;
}

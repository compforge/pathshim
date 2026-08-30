#define _GNU_SOURCE

#include <fcntl.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <unistd.h>

extern char **environ;

int main(void) {
    char *const arguments[] = {
        "/workspace/app",
        "/workspace/execveat-output",
        NULL,
    };

    syscall(SYS_execveat, AT_FDCWD, "/workspace/app", arguments, environ, 0);
    perror("execveat /workspace/app");
    return 1;
}

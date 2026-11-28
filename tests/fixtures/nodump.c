/* A stand-in for a hardened agent CLI, for the test suites only: it marks itself non-dumpable, as
 * the release Codex CLI does, so its /proc environ and fd stay private; then it runs argv[1] in a
 * tool shell, waits for it, and sleeps argv[2] seconds. */
#include <stdlib.h>
#include <sys/prctl.h>
#include <sys/wait.h>
#include <unistd.h>

int main(int argc, char **argv) {
    if (argc < 3) return 2;
    prctl(PR_SET_DUMPABLE, 0, 0, 0, 0);
    pid_t tool = fork();
    if (tool == 0) {
        execl("/bin/bash", "bash", "-c", argv[1], (char *)0);
        _exit(127);
    }
    waitpid(tool, 0, 0);
    sleep(atoi(argv[2]));
    return 0;
}

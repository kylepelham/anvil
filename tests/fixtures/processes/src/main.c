#define _CRT_SECURE_NO_WARNINGS
#define _POSIX_C_SOURCE 200809L
/* A test helper: start a server that answers pings for a few seconds, ping it, leave a child holding the output */
#include <stdio.h>
#include <string.h>
#ifdef _WIN32
#include <windows.h>
static void sleep_ms(int ms) { Sleep(ms); }
#else
#include <fcntl.h>
#include <time.h>
#include <unistd.h>
static void sleep_ms(int ms)
{
    struct timespec delay = {ms / 1000, (ms % 1000) * 1000000L};
    nanosleep(&delay, NULL);
}
#endif

static int exists(const char* path)
{
    FILE* file = fopen(path, "r");
    if (file) fclose(file);
    return file != NULL;
}

/* Answers pings (deletes the ping file) for five seconds */
static int serve(const char* ping)
{
    int waited;
    for (waited = 0; waited < 5000; waited += 20) {
        if (exists(ping)) remove(ping);
        sleep_ms(20);
    }
    return 0;
}

#ifdef _WIN32
/* Starts this program again with `arguments`: detached with no handles (a server), or sharing the output */
static int start_self(const char* arguments, int share_output)
{
    char self[MAX_PATH];
    char command[2 * MAX_PATH];
    STARTUPINFOA startup = {sizeof startup};
    PROCESS_INFORMATION process;

    GetModuleFileNameA(NULL, self, MAX_PATH);
    snprintf(command, sizeof command, "\"%s\" %s", self, arguments);
    if (share_output) {
        startup.dwFlags = STARTF_USESTDHANDLES;
        startup.hStdOutput = GetStdHandle(STD_OUTPUT_HANDLE);
        startup.hStdError = GetStdHandle(STD_ERROR_HANDLE);
        SetHandleInformation(startup.hStdOutput, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT);
        SetHandleInformation(startup.hStdError, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT);
    }
    if (!CreateProcessA(NULL, command, NULL, NULL, share_output, share_output ? 0 : DETACHED_PROCESS, NULL, NULL, &startup, &process))
        return 0;
    CloseHandle(process.hProcess);
    CloseHandle(process.hThread);
    return 1;
}
#endif

int main(int argc, char** argv)
{
    if (argc >= 3 && strcmp(argv[1], "serve") == 0) return serve(argv[2]);
    if (argc >= 2 && strcmp(argv[1], "linger") == 0) {
        sleep_ms(5000);
        return 0;
    }

    if (argc >= 3 && strcmp(argv[1], "server") == 0) {
#ifdef _WIN32
        char arguments[MAX_PATH + 16];
        snprintf(arguments, sizeof arguments, "serve \"%s\"", argv[2]);
        return start_self(arguments, 0) ? 0 : 2;
#else
        if (fork() == 0) {
            int null = open("/dev/null", O_WRONLY);
            dup2(null, 1);
            dup2(null, 2);
            return serve(argv[2]);
        }
        return 0;
#endif
    }

    /* Ping: the server deletes the file within a moment, if it is still running */
    if (argc >= 3 && strcmp(argv[1], "check") == 0) {
        int waited;
        FILE* file = fopen(argv[2], "w");
        if (!file) return 3;
        fclose(file);
        for (waited = 0; waited < 3000; waited += 20) {
            if (!exists(argv[2])) {
                puts("the server answered");
                return 0;
            }
            sleep_ms(20);
        }
        puts("the server is gone");
        return 1;
    }

    if (argc >= 2 && strcmp(argv[1], "holder") == 0) {
#ifdef _WIN32
        if (!start_self("linger", 1)) return 2;
#else
        if (fork() == 0) {
            sleep_ms(5000);
            return 0;
        }
#endif
        puts("parent done");
        return 0;
    }

    if (argc >= 3 && strcmp(argv[1], "print") == 0) {
        puts(argv[2]);
        return 0;
    }
    return 2;
}

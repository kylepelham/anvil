#define _CRT_SECURE_NO_WARNINGS
/* A test helper: print, exit with a code, sleep, create or check files, hold a lock folder or check nobody holds it */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/types.h>
#include <sys/stat.h>
#ifdef _WIN32
#include <direct.h>
#include <windows.h>
#define make_dir(path) _mkdir(path)
#define remove_dir(path) _rmdir(path)
static void sleep_ms(int ms) { Sleep(ms); }
#else
#include <time.h>
#include <unistd.h>
#define make_dir(path) mkdir(path, 0777)
#define remove_dir(path) rmdir(path)
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

int main(int argc, char** argv)
{
    if (argc >= 3 && strcmp(argv[1], "print") == 0) {
        puts(argv[2]);
        return 0;
    }
    if (argc >= 4 && strcmp(argv[1], "exit") == 0) {
        puts(argv[3]);
        return atoi(argv[2]);
    }
    if (argc >= 3 && strcmp(argv[1], "sleep") == 0) {
        sleep_ms(atoi(argv[2]));
        return 0;
    }
    if (argc >= 4 && strcmp(argv[1], "touch") == 0) {
        FILE* file;
        sleep_ms(atoi(argv[3]));
        file = fopen(argv[2], "w");
        if (!file) return 5;
        fclose(file);
        return 0;
    }
    if (argc >= 3 && strcmp(argv[1], "exists") == 0) {
        return exists(argv[2]) ? 0 : 1;
    }
    if (argc >= 4 && strcmp(argv[1], "hold") == 0) {
        if (make_dir(argv[2]) != 0) return 3;
        sleep_ms(atoi(argv[3]));
        remove_dir(argv[2]);
        return 0;
    }
    if (argc >= 4 && strcmp(argv[1], "probe") == 0) {
        int waited;
        for (waited = 0; waited < atoi(argv[3]); waited += 10) {
            struct stat info;
            if (stat(argv[2], &info) == 0) {
                puts("the serial test was running");
                return 4;
            }
            sleep_ms(10);
        }
        return 0;
    }
    return 2;
}

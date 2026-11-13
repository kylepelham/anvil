#include <cstdio>
#include <cstring>

int main(int argc, char** argv) {
    int answer = 42;
    if (argc > 1 && std::strcmp(argv[1], "fail") == 0) {
        std::printf("failing on purpose\n");
        return 3;
    }
    std::printf("answer=%d args=%d\n", answer, argc); // the real suite's breakpoint
    return 0;
}

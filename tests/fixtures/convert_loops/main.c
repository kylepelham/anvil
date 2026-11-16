#include <stdio.h>

int main(void) {
#if defined(_CHAR_UNSIGNED) && !defined(PICKED_GNU)
    const char* picked = "msvc";
#elif defined(PICKED_GNU) && !defined(_CHAR_UNSIGNED)
    const char* picked = "gnu";
#else
    const char* picked = "both";
#endif
    printf("{\"pthread\": %d, \"picked\": \"%s\"}\n", HAS_PTHREAD, picked);
    return 0;
}

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "core/core.h"

int main(int argc, char** argv)
{
    const char* profile = getenv("UNIT_PROFILE");
    if (argc < 2 || strcmp(argv[1], "--fast") != 0 || !profile) return 1;
    printf("unit %s %d\n", profile, core_level());
    return core_level() == 2 ? 0 : 1;
}

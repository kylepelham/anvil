#include <stdio.h>

#ifndef LIB_C_ONLY
#error the program's C files get LIB_C_ONLY
#endif
#ifdef LIB_CXX_ONLY
#error LIB_CXX_ONLY is for C++ files
#endif

int lib_value(void);
const char *platform_name(void);

int main(void)
{
    printf("%s %d\n", platform_name(), lib_value());
    return 0;
}

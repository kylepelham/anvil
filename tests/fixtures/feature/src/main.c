#include <stdio.h>
int main(void)
{
#ifdef FAST_PATH
    puts("fast");
#else
    puts("slow");
#endif
    return 0;
}

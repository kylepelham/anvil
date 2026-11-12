#include <stdio.h>
int main(int argc, char** argv)
{
    int index;
    for (index = 1; index < argc; index++) printf(index > 1 ? " %s" : "%s", argv[index]);
    printf("\n");
    return 0;
}

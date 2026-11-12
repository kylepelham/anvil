#include <stdio.h>
#include <stdlib.h>
#include "calc.h"
int main(int argc, char** argv)
{
    if (argc < 3) return 1;
    printf("sum %d\n", calc_add(atoi(argv[1]), atoi(argv[2])));
    return 0;
}

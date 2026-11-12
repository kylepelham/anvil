#include <stdio.h>
#include "a.h"
int other(void);
int third(void);
int main(void) { printf("%d\n", A_VALUE + other() + third()); return 0; }

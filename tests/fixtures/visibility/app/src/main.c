#ifdef CORE_OWN
#error "a library's own defines do not reach its users"
#endif
#ifndef CORE_PUBLIC
#error "a library's public defines reach its users"
#endif
#ifndef CORE_INTERFACE
#error "a library's interface defines reach its users"
#endif
#include <stdio.h>
int core_value(void);
int main(void) { printf("core %d\n", core_value()); return 0; }

#include <stdio.h>
#include <wchar.h>
#include "config.h"

int main(void) {
#if SIZEOF_LONG == 4 || SIZEOF_LONG == 8
    int sizes = SIZEOF_LONG == sizeof(long) && SIZEOF_WCHAR_T == sizeof(wchar_t);
#endif
#ifdef HAVE_X11
    puts("x11");
#endif
    printf("sizes=%d foo=%s w4=%d\n", sizes, FOO_VALUE, HAVE_DASH_W4);
    return 0;
}

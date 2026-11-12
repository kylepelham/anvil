#include "calc.h"
int calc_add(int left, int right)
{
#ifdef CALC_FAST
    return right + left;
#else
    return left + right;
#endif
}

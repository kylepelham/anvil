#include "counter.h"

void Counter::increment()
{
    count++;
    emit valueChanged(count);
}

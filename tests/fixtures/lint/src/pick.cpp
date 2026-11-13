#include "lint/twice.h"

int pick(int value) {
  if (value = twice(value))
    return value;
  return 0;
}

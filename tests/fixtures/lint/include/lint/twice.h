#pragma once

inline int twice(int value) {
  int *unused = 0;
  return unused ? value : value * 2;
}

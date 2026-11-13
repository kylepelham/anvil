#include "geo/core.h"
#include "geo/util.h"
int geo_area(int width, int height) { return geo_twice(width * height) / 2; }

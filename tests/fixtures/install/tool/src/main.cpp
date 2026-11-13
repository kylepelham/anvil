#include "geo/core.h"
#include <cstdio>
#include <fstream>
#include <string>
int main(int, char** argv) {
    std::string self = argv[0];
    std::string folder = self.substr(0, self.find_last_of("/\\") + 1);
    std::ifstream map(folder + "assets/maps/one.txt");
    std::string line;
    std::getline(map, line);
    std::printf("area %d map %s\n", geo_area(3, 4), line.c_str());
}

#include <optport.h>
#include <cstdio>

extern "C" int cfile(void);

int main() {
    std::printf("c=%d", cfile());
#ifdef OPT_CXX_DEF
    std::printf(" OPT_CXX_DEF");
#endif
#ifdef OPT_C_FLAG
    std::printf(" OPT_C_FLAG");
#endif
#ifdef OPT_DEBUG
    std::printf(" OPT_DEBUG");
#endif
#ifdef _OPENMP
    std::printf(" openmp");
#endif
    std::printf("\n");
}

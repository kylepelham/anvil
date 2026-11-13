#ifndef LIB_CXX_ONLY
#error the library's C++ files get LIB_CXX_ONLY
#endif
#ifdef LIB_C_ONLY
#error LIB_C_ONLY is for C files
#endif

extern "C" int lib_value(void) { return 42; }

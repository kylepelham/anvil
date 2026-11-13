int cfile(void) {
    int found = 0;
#ifdef OPT_C_FLAG
    found |= 1;
#endif
#ifdef OPT_CXX_DEF
    found |= 2;
#endif
    return found;
}

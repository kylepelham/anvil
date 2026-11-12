#ifndef CORE_OWN
#error "a library's own defines reach its sources"
#endif
#ifndef CORE_PUBLIC
#error "a library's public defines reach its sources"
#endif
#ifdef CORE_INTERFACE
#error "a library's interface defines do not reach its own sources"
#endif
int core_value(void) { return 7; }

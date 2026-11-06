// The little of a C library's `stdlib.h` the bindings use: `src/rt.c` has them.
#include <stddef.h>
void *realloc(void *ptr, size_t size);
_Noreturn void abort(void);

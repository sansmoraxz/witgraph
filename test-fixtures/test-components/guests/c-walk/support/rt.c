// What the bindings need of a C library, for a guest built without one: memory that is
// never given back (a node's run is short), and a trap for `abort`. Built without debug
// info, so a debugger stops only in the guest's own code.

#include <stdint.h>
#include <stdlib.h>

extern unsigned char __heap_base;
static uintptr_t next;

void *realloc(void *ptr, size_t size) {
  if (!next) next = (uintptr_t)&__heap_base;
  uintptr_t at = (next + 15) & ~(uintptr_t)15;
  size_t have = __builtin_wasm_memory_size(0) * 65536;
  if (at + size > have &&
      __builtin_wasm_memory_grow(0, (at + size - have + 65535) / 65536) == (size_t)-1) {
    abort();
  }
  next = at + size;
  // What was had comes along: it is below, so `size` bytes of it can be read.
  if (ptr) __builtin_memmove((void *)at, ptr, size);
  return (void *)at;
}

void abort(void) { __builtin_trap(); }

// The bindings make sure that the object `wit-bindgen c` writes, with the guest's world in
// it, is linked in, by calling this, which it has. The build embeds the world from `wit/`
// instead, so it is here.
void __component_type_object_force_link_c_walk(void) {}

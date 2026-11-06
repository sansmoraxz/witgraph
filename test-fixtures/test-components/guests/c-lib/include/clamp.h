// A library a C guest uses that is not its own: built with it, with debug info, but from
// outside its directory, as a library kept elsewhere is. A debugger finds it is not the
// guest's own by that alone, as nothing knows its path.

#include <stdint.h>

static inline uint32_t clamp_u32(uint32_t value, uint32_t low, uint32_t high) {
  if (value < low) return low; // mark: clamp-low
  if (value > high) return high;
  return value;
}

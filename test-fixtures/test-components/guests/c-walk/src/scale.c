// A second file of the guest's own, for stepping from one file into another and back.

#include "scale.h"

/// `x` twice over, by way of `add` back in `walk.c`.
uint32_t scale(uint32_t x) {
  uint32_t doubled = add(x, x); // mark: scale-add
  return doubled; // mark: scale-end
}

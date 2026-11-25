---
name: security-memory-safety
description: "Memory safety vulnerabilities to watch for when reviewing C/C++ code. Load if reviewing C/C++ code (or Rust unsafe blocks)."
---
# Memory Safety Vulnerabilities

## Overview

When reviewing C/C++ code (or Rust unsafe blocks), focus on these memory safety issues that can lead to exploitable vulnerabilities.

## Vulnerability Classes

### Buffer Overflows

Watch for:
- Missing bounds checks on array/buffer access
- Off-by-one errors in loop bounds
- Incorrect size calculations (especially with `sizeof` on pointers vs arrays)
- Unchecked user-controlled lengths passed to `memcpy`, `strcpy`, `sprintf`
- Stack buffers sized based on assumptions about input

### Use-After-Free

Watch for:
- Pointers stored/cached and used after the underlying object is freed
- Destructor ordering issues in C++ (prevent accessing parent members from child destructors)
- Pointers to objects that may be moved or reallocated (e.g., storing pointers to `std::vector` elements)
- Capture-by-reference in closures/callbacks that outlive the referenced object
- Pointers to stack variables that escape the function

### Double Free

Watch for:
- Error handling paths that free memory already freed in the normal path
- Ownership confusion (multiple code paths think they own the same allocation)
- Missing null-assignment after free

### Uninitialized Memory Access

Watch for:
- Stack variables used before assignment (especially in error paths)
- Partially initialized structs
- `realloc` patterns where new portion is not initialized
- Conditional initialization where some branches skip initialization

### Integer Overflows

Watch for:
- Arithmetic on user-controlled sizes before allocation (`count * sizeof(element)`)
- Signed/unsigned confusion in size comparisons
- Truncation when casting between integer sizes
- Addition/multiplication that can wrap around

### Out-of-Bounds Access

Watch for:
- User-controlled indices without validation
- Negative index handling (signed vs unsigned)
- Off-by-one in string termination
- Assumptions about array sizes based on external data

## C++ Specific Issues

### Smart Pointer Pitfalls
- Raw pointer extracted from `unique_ptr`/`shared_ptr` and used after the smart pointer is destroyed
- Circular `shared_ptr` references preventing cleanup
- `weak_ptr::lock()` result used without null check

### Container Invalidation
- Iterators invalidated by container modification during iteration
- References/pointers to container elements invalidated by `push_back`, `insert`, `erase`

### RAII Violations
- Resources acquired but not tied to object lifetime
- Exception-unsafe code that leaks on throw

## When Reviewing Code

1. Trace all user-controlled input to memory operations
2. Check every array/buffer access for bounds validation
3. Map object lifetimes - when is memory allocated vs freed vs accessed?
4. Look for arithmetic on sizes/lengths before they're used
5. Check error paths separately - they often have different lifetime assumptions

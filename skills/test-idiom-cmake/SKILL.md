---
name: test-idiom-cmake
description: "How to establish a runnable reproducer against a CMake project: configuring once with compile_commands.json, reusing the exact per-file compiler invocation, and the focused ctest route to fall back to. Load this when the <build> section names CMake."
build_system:
  name: CMake
  priority: 60
  program:
    - run: cmake
  prewarm: "{program} -S . -B build -DCMAKE_EXPORT_COMPILE_COMMANDS=ON"
  any_of:
    - CMakeLists.txt
  lane_commands:
    - cmake
    - ctest
  test_reports:
    - "**/Testing/**/*.xml"
---

# Reproducer idiom: CMake

Establishing one proven way to build and run a small reproducer against this project.
The `<build>` section of your system prompt already carries what was probed at startup;
do not rediscover it.

CMake can tell you the exact command it uses to compile any file in the project. That export is
the analogue of a classpath elsewhere, and getting it is one configure step.

## Execute host builds. Cross-compiles are sketch-only.

**Only host-toolchain builds are ever run here.** A toolchain file targeting another platform will
configure and compile, and the binary cannot be executed — a failure that reads like your
reproducer being wrong rather than the container being unable.

Sanitisers are the opposite case and are worth reaching for. `-DCMAKE_BUILD_TYPE=ASAN`, or
`-fsanitize=address,undefined` added to the flags you copied from `compile_commands.json`, runs on
the host and turns a suspected memory-safety defect into an unambiguous diagnostic with a stack
trace. That is a real escalation with a real payoff, unlike a cross-compile. Record which build type
you used: a defect visible only under a sanitiser is a different claim from one that crashes a
release build, and both are worth stating precisely.

## Sketch what you cannot run

For a defect specific to another platform or toolchain, write the probe anyway and put it in the
report **clearly marked as never executed**, with the configure and run commands a human would use.
State the toolchain required. Never present an unexecuted sketch as evidence; it enriches a finding
and does not support one.

1. Configure once, out of tree, asking for the compilation database:

   ```
   cmake -S . -B /tmp/flawrence-build -DCMAKE_EXPORT_COMPILE_COMMANDS=ON
   ```

   Out of tree because the workspace is evidence: an in-source configure scatters generated files
   through the target and they will show up in the run's patch as changes an agent appears to have
   made.

2. Read `/tmp/flawrence-build/compile_commands.json`. Each entry is `{directory, command|arguments,
   file}` — the exact compiler, every `-I`, every `-D`, the standard level, for one translation
   unit. Find the entry for a file near the code under review and reuse its flags for your probe,
   changing only the input and output.

   This matters more here than anywhere else: C and C++ behaviour depends on flags. A probe built
   without the project's `-D` set can compile a different branch of the code entirely, and its
   result is then evidence about a program the project does not ship.

3. Build the library or target you need to link against:

   ```
   cmake --build /tmp/flawrence-build --target <target> -j
   ```

   `cmake --build` works whatever the underlying generator is (Make, Ninja); invoking the generator
   directly ties your recorded commands to one of them.

4. Compile and link the probe against the built artifact, then run it. Prefer linking the built
   library over recompiling its sources into your probe — the former is what the project ships.

5. Require a sentinel on stdout AND a zero exit status. For C and C++ also treat a non-zero signal
   exit as decisive evidence in its own right, and say which signal.

6. Record the configure command, the compile_commands.json entry you copied, the compiler version,
   the target built, and the probe path.

## Use the project's own tests instead when

The reproducer needs fixtures, generated headers, or a runtime environment the suite sets up:

```
ctest --test-dir /tmp/flawrence-build -R '<name>' --output-on-failure
```

`-R` is a regex. A pattern matching nothing reports "No tests were found" and still exits zero on
some versions — read the test count, not just the status.

## Notes

- Re-running `cmake -S . -B <dir>` on an existing build directory reconfigures incrementally; it is
  cheap and does not need deleting first unless the generator or compiler changed.
- Sanitisers are worth knowing about: if the project supports `-DCMAKE_BUILD_TYPE=ASAN` or similar,
  a memory-safety reproducer is far more convincing built that way. Record which build type you
  used, because a defect visible only under a sanitiser is a different claim from one that crashes
  a release build.

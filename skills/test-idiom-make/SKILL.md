---
name: test-idiom-make
description: "How to establish a runnable reproducer against a Makefile-driven project: extracting the real compiler invocation with make -n, reproducing it directly, and the focused make target route to fall back to. Load this when the <build> section names Make."
build_system:
  name: Make
  priority: 90
  program:
    - when_file: GNUmakefile
      run: make
    - run: make
  any_of:
    - Makefile
    - makefile
    - GNUmakefile
  lane_commands:
    - make
    - gmake
---

# Reproducer idiom: Make

Establishing one proven way to build and run a small reproducer against this project.
The `<build>` section of your system prompt already carries what was probed at startup;
do not rediscover it.

Make is a task runner, not a build system with a dependency model you can query. There is no
manifest to read: the only authority on how this project compiles is the Makefile itself, and the
fastest way to learn it is to make Make tell you.

## Execute what the host can run. Nothing else.

**Only host-executable targets are ever run here.** A Makefile may drive a cross-compiler, flash a
device, or launch an emulator; those targets configure and build and then fail at the point of
execution, in a way that reads like your reproducer being wrong.

`make -n` is your protection: read the commands a target *would* run before running it. If they
invoke a cross-compiler, a device tool or an emulator, do not run that target — extract what you
can and reproduce on the host instead.

## Sketch what you cannot run

For a defect reachable only through a target this container cannot execute, write the reproducer
anyway and put it in the report **clearly marked as never executed**, with the exact target and the
command a human would run, plus what output would confirm the defect. Never present an unexecuted
sketch as evidence; it enriches a finding and does not support one.

1. Find the targets that exist, rather than guessing `test` or `check`:

   ```
   make -np 2>/dev/null | grep -E '^[a-zA-Z][^ ]*:' | sort -u | head -50
   ```

   Also read the Makefile directly — target names are usually self-describing, and a `.PHONY` line
   lists the ones that are commands rather than files.

2. **Extract the real compile command without running it.** This is the whole trick, and the
   analogue of a classpath export elsewhere:

   ```
   make -n <target>
   ```

   `-n` prints the commands Make would run and executes none of them. That gives you the exact
   compiler, flags, include paths and link line the project uses — which you then reuse directly
   for your probe instead of going through Make at all.

3. Compile the probe with those flags, changing only the source file and the output path. Do not
   invent flags: a missing `-D` or `-I` can change which code path even compiles, and a reproducer
   built under different flags is evidence about a different program.

4. If the project builds a library, link the probe against the built artifact rather than
   recompiling the sources into it — that proves the behaviour of what the project actually ships.

5. Require a sentinel on stdout AND a zero exit status.

6. Record the target you inspected, the extracted command verbatim, the compiler version, and the
   probe path.

## Use the project's own target instead when

The reproducer needs generated sources, a configure step, or link ordering you cannot reconstruct.
Then run the narrowest target that covers it and read its output, remembering that Make reports the
exit status of the last recipe line — a test runner that prints failures and exits zero will look
like success.

## Notes

- `make -B <target>` forces a rebuild when Make thinks everything is up to date and you need to see
  the commands again.
- Parallel builds (`-j`) interleave output unreadably. Run single-threaded while establishing the
  idiom.
- A Makefile that shells out to another build system (cmake, cargo, gradle) is a wrapper: follow it
  through and use that system's idiom instead.

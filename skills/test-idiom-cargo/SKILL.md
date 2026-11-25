---
name: test-idiom-cargo
description: "How to establish a runnable reproducer against a Rust project: enumerating workspace members with cargo metadata, driving one through an example or an integration test, and the focused cargo test route to fall back to. Load this when the <build> section names Cargo."
build_system:
  name: Cargo
  priority: 30
  program:
    - run: cargo
  prewarm: "{program} test --no-run"
  any_of:
    - Cargo.toml
---

# Reproducer idiom: Rust / Cargo

Establishing one proven way to build and run a small reproducer against this project.
The `<build>` section of your system prompt already carries what was probed at startup;
do not rediscover it.

Rust has no classpath to export — Cargo resolves and links in one step, so the reproducer must be
built *inside* the project rather than beside it. Do not create a separate crate that depends on
the target by path: that re-resolves the whole dependency graph and can pick different feature
unification than the real build.

## Execute on the host target. Nothing else.

**Only the host target is ever run here.** Cargo will happily cross-compile for another triple, and
the result cannot be executed: `cargo test --target aarch64-apple-darwin` needs a runner this
container has no way to provide, and `wasm32-*` needs a JS host. Both fail in ways that read like
your reproducer being wrong rather than the environment being unable, so an agent that tries spends
its budget debugging the toolchain and proves nothing about the code.

Features are the exception, and are cheap: `--features x` changes which code compiles for the same
host target, so a defect behind a feature gate can be reproduced properly. Enable exactly the
features the defect needs and record them — `--all-features` can enable combinations no real
consumer builds, which makes the reproducer evidence about a configuration nobody ships.

Platform gates are not the same thing. Code behind `#[cfg(target_os = "windows")]` or
`#[cfg(target_arch = "aarch64")]` cannot be exercised here at all. Reproduce whatever the host
`cfg` selects, and state plainly which gates went unexercised.

## Sketch what you cannot run

For a defect behind a platform gate, write the test anyway and put it in the report **clearly
marked as never executed**, with the command a human would run:

```
cargo test -p <crate> --target x86_64-pc-windows-msvc <filter> -- --nocapture
```

That is a handover, not a shortfall: someone on that platform runs it in minutes, and the harness
cannot. Make the sketch complete enough to paste — the test source, the target, the command, and
what output would confirm the defect. Never present it as evidence; it enriches a finding and does
not support one.

1. Enumerate the workspace once, as machine-readable data rather than by reading Cargo.toml files:

   ```
   cargo metadata --no-deps --format-version 1
   ```

   `packages[].name` are the crate names for `-p`, and `packages[].targets[]` tells you which are
   libs and which are binaries. This is one invocation for the whole workspace.

2. Prefer an **example** for the proof. It compiles against the crate's public API exactly as a
   downstream consumer would, and needs no test harness:

   ```
   mkdir -p <crate>/examples
   # write <crate>/examples/flawrence_probe.rs with a main() that prints a sentinel
   cargo run -p <crate> --example flawrence_probe
   ```

   Use an **integration test** under `<crate>/tests/` instead when the reproducer needs
   `#[should_panic]`, `Result`-returning tests, or the test-only dev-dependencies:

   ```
   cargo test -p <crate> --test flawrence_probe -- --nocapture
   ```

   `--nocapture` is required or stdout is swallowed on success, which makes a sentinel useless.

3. Use a **unit test inside `src/`** only when the reproducer needs private items. Anything
   reachable from outside the crate should be proved from `examples/` or `tests/`, because that is
   the surface a consumer actually has.

4. Record whether the crate has non-default features and which you enabled. A defect that needs
   `--features x` is a different claim from one that does not, and `--all-features` can enable
   combinations no real consumer builds.

5. Require the sentinel in stdout AND a zero exit status. A panicking test still prints.

6. Record the exact commands, the crate selected, the feature flags, the toolchain
   (`rustc --version`), and the path of the file you added.

## Notes that save a rebuild

- `cargo build` and `cargo test` share a target directory but not their profiles; a warm
  `cargo build` does not make `cargo test` incremental. Expect the first test invocation to compile
  dev-dependencies.
- `--offline` fails outright if anything is unresolved rather than reaching the network. Try it
  first: if it succeeds the dependency graph is fully vendored and every later run is faster.
- `cargo test` caches results. Add `--no-run` to compile only, or change the input, if you are
  unsure whether something actually re-ran.

## Use the project test route instead when

The reproducer changes production source, needs private items, or must run under the crate's own
feature configuration. Then:

```
cargo test -p <crate> <filter> -- --nocapture --exact
```

and confirm the case you expect appears in the output; a mistyped filter runs zero tests and still
exits zero.

## Clean up

Delete any `examples/` or `tests/` file you added once the proof is recorded, or say plainly in the
artifact that it is still there.

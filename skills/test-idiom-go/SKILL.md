---
name: test-idiom-go
description: "How to establish a runnable reproducer against a Go project: driving a package test in place, defeating the test cache, and the focused go test route to fall back to. Load this when the <build> section names Go modules."
build_system:
  name: Go
  priority: 40
  program:
    - run: go
  prewarm: "{program} build ./..."
  any_of:
    - go.mod
---

# Reproducer idiom: Go

Establishing one proven way to build and run a small reproducer against this project.
The `<build>` section of your system prompt already carries what was probed at startup;
do not rediscover it.

Go resolves dependencies per module, so a reproducer written outside the module cannot see it
without a `replace` directive — which is fiddly and easy to get subtly wrong. Write the probe
**inside the module** instead.

## Execute on the host GOOS and GOARCH. Nothing else.

**Only the host platform is ever run here.** `GOOS=windows go build` succeeds and produces a binary
this container cannot execute; `go test` for a non-host GOOS does not run at all. Neither failure
says anything about the code, and both look like a broken reproducer.

Build tags are the thing to watch, because they fail *silently*. A file behind `//go:build linux`
is simply not compiled on another platform — and a test that was never compiled is indistinguishable
from a test that passed. Check the tags on the file under review, give your test file matching ones,
and confirm from `-v` output that your test actually ran rather than trusting the exit status.

cgo is worth checking before relying on it: `go env CGO_ENABLED` and whether a C toolchain is
present. A package that needs cgo behaves differently when it is disabled, and that difference is
easy to mistake for the defect you are chasing.

## Sketch what you cannot run

For a defect behind a build tag or a platform-specific implementation, write the test anyway and
put it in the report **clearly marked as never executed**, with the command a human would run:

```
GOOS=darwin GOARCH=arm64 go test ./<pkg> -run '^TestX$' -v -count=1
```

Someone on that platform runs it in minutes. Make it complete enough to paste, and never present an
unexecuted sketch as evidence — it enriches a finding, it does not support one.

1. Enumerate packages once:

   ```
   go list ./...
   ```

   These are the import paths you will pass to `go test`. `go list -m` gives the module path itself.

2. Prefer a **test file in the package under review**, because it is the only way to reach
   unexported identifiers and it needs no extra wiring:

   ```
   # write <pkg>/flawrence_probe_test.go with func TestHarnessProbe(t *testing.T)
   go test ./<pkg> -run '^TestHarnessProbe$' -v -count=1
   ```

   Use `_test` package suffix (`package foo_test`) when you want to prove the *external* API
   surface, which is what a consumer sees; use `package foo` when the reproducer needs internals.

3. `-count=1` is mandatory. Go caches successful test results keyed on inputs, so a re-run of an
   unchanged test prints `(cached)` and tells you nothing about the run you just asked for. Anchor
   `-run` with `^...$` too, or it is a regex that can match more tests than you meant.

4. Prefer `t.Log` plus `-v` over `fmt.Println` for the sentinel: it is attributed to the test and
   survives parallel runs interleaving output.

5. Check for a `vendor/` directory. If present, the build uses it by default (`-mod=vendor`) and a
   dependency you expect from the module cache may not be there. Record which mode you ran under.

6. Require the sentinel AND a `PASS`/zero exit. A `t.Skip` exits zero and prints nothing useful.

7. Record the exact command, the package, the Go version (`go version`), the vendor mode, and the
   file you added.

## Use `go run` instead when

The reproducer only exercises the exported API and you want it outside the test harness. It still
has to live inside the module:

```
# a main package under a scratch directory inside the module
go run ./internal/flawrence-probe
```

Delete it afterwards, or say in the artifact that it is still there.

## Notes

- `go build ./...` compiles without running and is the cheapest way to confirm the toolchain and
  module graph are healthy before writing anything.
- `GOFLAGS` may already pin `-mod=` in the environment; `go env GOFLAGS` shows it.
- A build tag (`//go:build ...`) on the file you are probing means your test file may need the same
  tag, or it will not be compiled at all — and a test that is not compiled looks exactly like a
  test that passed.

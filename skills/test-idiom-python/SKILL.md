---
name: test-idiom-python
description: "How to establish a runnable reproducer against a Python project: making the package importable without installing into the system interpreter, running a standalone probe, and the focused pytest route to fall back to. Load this when the <build> section names Python."
build_system:
  name: Python
  priority: 50
  program:
    - run: python
  any_of:
    - pyproject.toml
    - setup.py
    - setup.cfg
---

# Reproducer idiom: Python

Establishing one proven way to build and run a small reproducer against this project.
The `<build>` section of your system prompt already carries what was probed at startup;
do not rediscover it.

Python needs no compile step, so the whole problem is **importability**: getting the target
package on `sys.path` as the version under review rather than a copy installed elsewhere.

## Execute on the interpreter you have. Nothing else.

**Only the installed interpreter is ever run here.** Code behind `sys.version_info` branches for a
version this container does not have cannot be exercised, and neither can `sys.platform ==
"win32"` paths. Record the interpreter you used — `python --version` and its path — because a
defect that appears only on one version is a different claim from one that appears on all of them.

Missing third-party dependencies are a different case and often fixable: install the one package a
blocked step needs and record that you did. That is not the same as a missing *interpreter version*,
which you cannot conjure and should not pretend to.

## Sketch what you cannot run

For a defect on a version or platform you cannot reach, write the probe anyway and put it in the
report **clearly marked as never executed**, with what a human would run:

```
python3.11 -c "import sys; ..."     # not available here
```

State the version or platform required and what output would confirm the defect. Never present an
unexecuted sketch as evidence; it enriches a finding and does not support one.

1. Establish which layout the project uses, from files you can see:
   - `src/<pkg>/` — a src layout. The package is NOT importable from the repo root; you need
     `PYTHONPATH=src`.
   - `<pkg>/__init__.py` at the root — a flat layout, importable from the repo root as-is.
   - `pyproject.toml` / `setup.py` / `setup.cfg` name the distribution; the import name often
     differs from it (`pip install pillow`, `import PIL`).

2. Confirm you are importing the source under review, not an installed copy of the same name.
   This is the single most common way a Python reproducer lies:

   ```
   PYTHONPATH=src python -c "import <pkg>, inspect; print(inspect.getfile(<pkg>))"
   ```

   The path printed MUST be inside the workspace. If it points at site-packages, an older release
   is shadowing the source and every result you get is about that release instead.

3. Write one small probe outside the source tree, run it with the same `PYTHONPATH`, and print a
   sentinel:

   ```
   PYTHONPATH=src python /tmp/flawrence-probe/probe.py
   ```

4. Do not `pip install` the project to make imports work — it mutates the environment for every
   later agent and can pull dependencies from the network. `PYTHONPATH` is reversible and explicit.
   If a required third-party dependency is genuinely missing, install that one dependency and
   record that you did.

5. Require the sentinel in stdout AND a zero exit status. An uncaught exception prints a traceback
   to stderr and exits non-zero; a caught one prints nothing and exits zero.

6. Record the interpreter (`python --version` and its path), the PYTHONPATH used, the resolved
   package file from step 2, and the probe path.

## Use the project test route instead when

The reproducer needs fixtures, parametrisation, or the project's own conftest. Then run one test by
node id rather than by keyword:

```
python -m pytest "tests/test_x.py::TestClass::test_case" -q
```

A `-k` expression that matches nothing still exits zero with "no tests ran" — read the count, not
the exit status. `--collect-only` confirms your node id resolves before you spend a run on it.

## Environments

If the project ships a virtualenv or the interpreter lacks its dependencies, prefer the project's
own environment (`.venv/bin/python`) over the system one. Record which you used: a defect that only
appears under one interpreter version is a different claim.

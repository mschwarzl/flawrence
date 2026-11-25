---
name: test-idiom-maven
description: "How to establish a runnable reproducer against a Maven project: exporting a module classpath with dependency:build-classpath, compiling a standalone consumer with javac or kotlinc, and the focused surefire route to fall back to. Load this from the test-idiom bootstrap when the <build> section names Maven."
build_system:
  name: Maven
  priority: 20
  program:
    - when_file: mvnw
      run: "./mvnw"
    - run: mvn
  prewarm: "{program} -q -B test-compile"
  any_of:
    - pom.xml
  lane_commands:
    - mvn
    - mvnw
  test_reports:
    - "**/target/surefire-reports/*.xml"
    - "**/target/failsafe-reports/*.xml"
---

# Reproducer idiom: Maven

Establishing one proven way to compile and run a small reproducer against this project.
The `<build>` section of your system prompt already carries the module list, test-target hint
and source roots; do not rediscover those.

For a Maven project, first try a standalone JVM consumer backed by a classpath resolved by Maven.
Maven remains the authority for dependency resolution; never reconstruct a transitive classpath by
hand or by scanning `~/.m2`.

## Execute on an installed JDK. Nothing else.

**Only the JDKs this container actually has are ever run here.** That matters more for Maven than it
looks, because multi-release jars and `maven.compiler.release` let a project target a JDK that is
not installed — and the failure is explicit but easy to misread:

```
error: release version 25 not supported
```

That is the toolchain saying it cannot target that release, not a defect in the code. Check which
JDKs are available before concluding anything, and record which one you used: a defect that appears
only under one release is a different claim from one that appears under all of them.

Profiles are the other thing to state. A defect reachable only with `-P<profile>` active is a
narrower claim than one reachable by default; enable exactly what the defect needs and say which.

## Sketch what you cannot run

For a defect in a source set targeting a JDK release this container lacks, write the test anyway and
put it in the report **clearly marked as never executed**, with the command a human would run and
the JDK required. Never present an unexecuted sketch as evidence; it enriches a finding and does not
support one.

1. Take the module list and test-target hint from the `<build>` section. Pick one representative
   module from the attack surface.
2. Export the classpath ONCE with the plugin Maven ships for exactly this:

   ```
   <mvn> -q -B -pl <module> -am dependency:build-classpath \
       -Dmdep.outputFile=<dir>/<module>.classpath -Dmdep.includeScope=test
   ```

   `-Dmdep.includeScope=test` is what pulls in test-scope dependencies; without it a reproducer
   that touches the test framework fails to compile. Write `<dir>` under `/tmp`, not under
   `$WORKSPACE`: Maven commands execute in verifier worktrees, while later plain `javac`/`java`
   commands need the same absolute files.
3. The module's own compiled output is NOT on that classpath — it lists dependencies only. Append
   `<module>/target/classes` (and `<module>/target/test-classes` when the reproducer needs test
   fixtures) to what you pass to `javac`. Verify this by compiling against a symbol from the module
   itself, not only against a third-party class.
4. Write one small harmless consumer that calls a representative public symbol and prints a
   sentinel. Compile and run it directly:

   ```
   javac -cp "$(cat <dir>/<module>.classpath):<module>/target/classes" -d <dir>/out <file>.java
   java  -cp "$(cat <dir>/<module>.classpath):<module>/target/classes:<dir>/out" <MainClass>
   ```

   For a Kotlin module use `kotlinc` with the same classpath; the exported runtime classpath already
   contains the Kotlin runtime and all transitives.
5. Require the sentinel in stdout AND a zero exit status. A `java` invocation that throws still
   prints, so output alone is not proof.
6. Record the exact export, compile and run commands, the sentinel, the selected module, the
   classpath file path, the JDK version, and any path substitutions needed by verifier worktrees.

A standalone consumer can call code whose annotation-processor output was already built by Maven.
Reach for the focused Maven test route below when the reproducer needs a packaging step (shaded or
relocated jars), resources resolved from the classpath at runtime, an annotation processor to run
over new source, or when it changes production source. These are normal boundaries, not reasons to
abandon the verified fast path.

If the classpath export fails, the JDK is incompatible, or the standalone proof cannot be made
reliable after a small number of focused corrections, mark that backend unverified and establish the
project's focused test route instead. Do not loop through modules or guessed jars.

## Use the project test route instead when

The reproducer needs a packaging step, resources resolved from the classpath at runtime, an
annotation processor over new source, or it changes production source. Put the test under the module's `src/test/java` (or
`src/test/kotlin`), give the class a distinctive name, and run:

```
<mvn> -B -pl <module> -am test -Dtest=<DistinctiveClassName> -DfailIfNoSpecifiedTests=false
```

Then read `<module>/target/surefire-reports/TEST-<fqcn>.xml` and confirm the case you expect is
listed with the outcome you expect. `BUILD SUCCESS` with a mistyped `-Dtest` selector means zero
tests ran, which is indistinguishable from passing if you only read the console.

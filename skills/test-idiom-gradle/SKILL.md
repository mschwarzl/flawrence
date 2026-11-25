---
name: test-idiom-gradle
description: "How to establish a runnable reproducer against a Gradle project: exporting a module classpath through a temporary init script, compiling a standalone consumer with kotlinc or javac, and the focused project-test route to fall back to. Load this from the test-idiom bootstrap when the <build> section names Gradle."
build_system:
  name: Gradle
  priority: 10
  program:
    - when_file: gradlew
      run: "./gradlew"
    - run: gradle
  prewarm_derive:
    probe: "{program} --max-workers=1 tasks --all -q --console=plain"
    command: "{program} --max-workers=1 {tasks} --build-cache --console=plain"
    candidates:
      # Ordered, first match wins. KMP outranks classes/testClasses: a
      # multiplatform module only has those if it calls withJava(), so when they
      # appear at all they usually belong to some other JVM-only module, and
      # warming that compiles nothing the agents use while reporting success.
      - tasks: [compileKotlinJvm, compileTestKotlinJvm]
        flavour: "Kotlin Multiplatform with a JVM target"
      - tasks: [classes, testClasses]
        flavour: "plain JVM"
      - tasks: [compileJava, compileTestJava]
        flavour: "plain JVM"
      # Last resort, and never for multiplatform: assemble drags in every
      # declared target, so a project with Apple or Linux native targets burns
      # an hour on artifacts no agent will execute.
      - tasks: [assemble]
        flavour: "plain JVM"
        excluded_by: [compileKotlinJvm, compileTestKotlinJvm, jvmTest]
  any_of:
    - gradlew
    - settings.gradle.kts
    - settings.gradle
    - build.gradle.kts
    - build.gradle
  lane_commands:
    - gradle
    - gradlew
  test_reports:
    - "**/build/test-results/**/*.xml"
---

# Reproducer idiom: Gradle

Establishing one proven way to compile and run a small reproducer against this project.
The `<build>` section of your system prompt already carries the module list, test-target hint
and source roots; do not rediscover those.

For a Gradle Kotlin project, first try a standalone JVM consumer backed by a classpath resolved by
Gradle. Gradle remains the authority for dependency resolution; never reconstruct its transitive
graph from build files or Maven metadata.

## Execute on the JVM. Nothing else.

**Only JVM targets are ever run here, whatever the project declares.** This is a fact about the
container, not a preference:

- Android instrumented tests need an emulator, which needs KVM, which this container does not have.
- JS tests need a browser or a Node harness that is usually not installed and whose absence
  surfaces as a Gradle failure rather than a clear message.
- Native targets may cross-compile and still not run, and a linked binary for the wrong host is
  worse than no binary at all.

Every one of those fails in a way that reads like your reproducer being wrong. An agent that
escalates to a non-JVM target spends its budget debugging the environment, concludes nothing about
the code, and leaves a finding weaker than if it had never tried.

So: prove what the JVM can prove, and be explicit about the rest.

- **Code in `commonMain`** compiles into every target, so a JVM reproducer exercises exactly the
  source a native or JS consumer runs. There is no gap — say the code is common, and the JVM result
  stands for all targets.
- **Code behind `expect`/`actual`, or under a platform source set** (`nativeMain`, `iosMain`,
  `jsMain`, `androidMain`) genuinely differs per target. Run the JVM half if there is one, and
  state plainly which targets were **not** exercised and why. A finding that says "reproduced on
  JVM; the `iosMain` actual is unverified" is honest and useful. One that implies whole-project
  coverage from a JVM run is not.

## Sketch what you cannot run

For a suspected target-specific defect, write the reproducer for that target anyway and put it in
the report **clearly marked as never executed**, with the exact command a human would run:

```
<program> --max-workers=1 :<module>:iosSimulatorArm64Test --tests '<filter>'
```

This is a deliberate division of labour, not a shortfall. A human with a Mac, an emulator or a
browser runs it in minutes; the harness cannot, and pretending otherwise costs a run and produces
nothing. The sketch is what makes that handover possible, so make it complete enough to paste:
the test source, the target, the command, and what output would confirm the defect.

Never present an unexecuted sketch as evidence. It enriches a finding; it does not support one.
Read the project's `kotlin { }` block for the targets it actually declares, so the command you
sketch names a target that exists.


1. Prefer a module's exact `jvmTest` task when present. Otherwise use its `test` task. Decide per
   module: mixed builds may contain KMP and plain JVM modules, and custom task names may be valid.
2. Inject one temporary Groovy init script with `--init-script`; never edit the target's build files.
   In one task, enumerate projects and their `org.gradle.api.tasks.testing.Test` tasks. For each
   project choose `jvmTest` if present, otherwise `test`, and export the existing files from
   `test.testClassesDirs.files + test.classpath.files`, preserving order and removing duplicates.
   `Test.classpath` is the resolved runtime classpath, including transitives and project outputs.
   Start from this known-working generic shape. Put the init script, exported classpaths, and compiled
   probe output in a unique directory under `/tmp`, not under `$WORKSPACE`: Gradle commands execute
   in verifier worktrees, while later plain `kotlinc` commands need the same absolute files.

   ```groovy
   import org.gradle.api.tasks.testing.Test

   gradle.projectsEvaluated {
       def outputDirectory = new File(System.getProperty("harness.classpath.dir"))
       rootProject.tasks.register("_harnessExportJvmClasspaths") {
           doLast {
               outputDirectory.mkdirs()
               rootProject.allprojects.each { project ->
                   def tests = project.tasks.withType(Test)
                   def selected = tests.find { it.name == "jvmTest" } ?: tests.find { it.name == "test" }
                   if (selected != null) {
                       def files = new LinkedHashSet<File>()
                       files.addAll(selected.testClassesDirs.files)
                       files.addAll(selected.classpath.files)
                       def existing = files.findAll { it.exists() }
                       def module = project.path == ":" ? "root" : project.path.substring(1).replace(':', '-')
                       new File(outputDirectory, "${module}.classpath").text =
                           existing.collect { it.absolutePath }.join(File.pathSeparator)
                       println("FLAWRENCE_CLASSPATH\t${project.path}\t${selected.path}\t${existing.size()}")
                   }
               }
           }
       }
   }
   ```

   Run it once with the mandatory wrapper/resource flags from the system prompt, for example
   `./gradlew --max-workers=1 --init-script <script> -Dharness.classpath.dir=<dir>
   _harnessExportJvmClasspaths --console=plain`. If neither conventional task exists for a relevant
   module, inspect its available `Test` tasks and select the one that actually represents its JVM
   tests rather than forcing the template.
3. Use the startup pre-warm's outputs. If an expected project entry is absent, check only the
   relevant module and its declared project dependencies: common locations are
   `build/classes/kotlin/jvm/main`, `build/classes/kotlin/jvm/test`, and `build/libs/*-jvm*.jar`.
   A broader dependent module's `jvmTest` classpath may already contain everything needed. Add a
   specific verified output when necessary; do not scan the whole home directory for jars.
4. Write one small `.kt` consumer outside the target source tree. It must import and call a real
   public project symbol from an attack-surface module, exercise at least one nontrivial behavior,
   check the result, and print a unique success sentinel.
5. Compile it with the available `kotlinc`, the exported classpath, and the target JVM level when
   known. Run the result with plain `java -cp`, not only the `kotlin` launcher: plain Java proves the
   exported runtime classpath itself contains the Kotlin runtime and all transitives.
6. If ordinary existing tests declare source that needs a compiler plugin, cheaply test whether
   the installed compiler ships a matching plugin. For example, Kotlin distributions commonly place
   `kotlinx-serialization-compiler-plugin.jar` under their adjacent `lib` or `libexec/lib` directory;
   pass a verified jar as `-Xplugin=<jar>`. Prove it with one minimal plugin-dependent source and
   record the argument. Do not guess plugin compatibility or search unrelated caches indefinitely.
7. Record the exact compile command, run command, sentinel, selected module/task, classpath file,
   compiler version, and any path substitutions needed by persistent verifier worktrees.

A standalone consumer can call code whose serialization/KSP/other plugin-generated artifacts were
already built by Gradle. It can also compile new plugin-dependent source when the bootstrap has
explicitly verified and recorded a compatible compiler-plugin argument. Otherwise, or for
`internal`/friend-path access, Android resources/runtime, native or JS execution, or a reproducer
that changes production source, record and use the focused Gradle test route below. These are normal
boundaries, not reasons to abandon the verified fast path for public JVM behavior.

If `kotlinc` is absent, incompatible with the project's Kotlin metadata, or the standalone proof
cannot be made reliable after a small number of focused corrections, mark that backend unverified
and establish the project's focused test route instead. Do not loop through modules or guessed jars.

## Use the project test route instead when

Read one existing test beside the relevant attack-surface code and record its exact source set,
package, imports, framework style, assertion style, and focused command. Prefer `:<module>:jvmTest
--tests '<suite filter>'` for KMP and `:<module>:test --tests '<suite filter>'` for plain JVM, while
preserving a verified custom task when the project uses one. Never use `allTests` or `assemble` for
a focused reproducer.

You do not need to run a deliberately failing Gradle suite when the standalone consumer was
compiled and executed successfully. The final regression-test writer will run its real focused
test. If standalone execution is unavailable and a Gradle test is the only backend, verify discovery
with one uniquely named two-case suite: one case must pass and one must fail, and the XML report must
show exactly two executed cases and the expected failure. Delete that temporary suite afterward.

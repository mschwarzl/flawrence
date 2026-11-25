# Memory watchdog

Agents build and run code, and a single test JVM can use many gigabytes.
The memory watchdog keeps one agent from taking the whole machine down.

```dotenv
FLAWRENCE_MAX_PARALLEL_AGENTS=2
FLAWRENCE_AGENT_MEMORY_MB=1024
```

With these settings two agent trees run at the same time, and each tree may use about 1 GiB.

## What a tree is

A tree is one direct subagent of the root agent plus every agent it spawns.
A research agent and its validation agent therefore share one limit.
Each tree has its own limit. Unused memory in one tree does not go to another.
The root agent's own commands get a separate limit of the same size.

The limit covers the commands agents run and all their child processes.
It does not cover the runner itself, the dashboard or the build before the run.
Leave extra memory for those.

## What happens on a breach

About every 100 ms the watchdog adds up the resident memory of each tree's processes.
When a tree goes over its limit, the watchdog stops the commands in that tree.
The command returns an error that starts with `memory_limit_exceeded` and names the measured memory.
Every agent in the tree gets a notice on its next turn.
The agents keep running and can retry with a smaller input or less concurrency.
Other trees are not affected.

A breach is not a finding.
A proof of concept for memory exhaustion needs a control run and an attack run under the same limits.

## Limits of the watchdog

The watchdog is not a hard ceiling.
A fast allocation can fill the machine between two samples.
Shared pages can be counted twice.
A process that detaches before it is first sampled is not counted.
Set the limit well below the available memory and keep JVM heap limits in place.

The watchdog does not use cgroups and needs no extra privileges.
It needs Linux 5.3 or later with a readable `/proc`.
On macOS, use the Docker image.

`FLAWRENCE_AGENT_MEMORY_MB` takes a positive number of MiB.
An empty value turns the watchdog off.
Any other value stops the runner at startup.

While the watchdog is on, Gradle runs without a daemon and Kotlin compiles in process.
Otherwise those processes would escape the measurement.

## Tests

```sh
cargo test -p flawrence-tool memory::
```

The tests need `python3`.

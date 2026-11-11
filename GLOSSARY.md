# gbb

A build broker in front of cargo: it decides where a build runs, whether it may
run now, and whether an earlier build already answers it.

## Language

**Build dir**:
The directory cargo treats as its target directory for one invocation — the
shared `<checkout>/target`, or a private `<target>/parallel/<id>`.
_Avoid_: target dir, build cache (that is the snapshot store)

**Private build dir**:
A build dir owned by one invocation, taken so no command waits for another.
Named by `GBB_NAME` or by the checkout, with `-2`, `-3` suffixes for concurrent runs.
_Avoid_: isolated dir, agent dir, sandbox

**Slot**:
Admission to run, granted by class and capped by available memory. Holding a slot
is the right to consume a build's worth of RAM.
_Avoid_: permit, token, semaphore

**Claim**:
The lock that makes a private build dir exclusive to one invocation.
_Avoid_: reservation, lease

**Session**:
One invocation's lifecycle: join or isolate, claim, slot, seed, digest, run, finish.
Its decisions are the subject of the `Plan`.
_Avoid_: run, job (a job is the recorded result)

**Plan**:
The decisions a session would make, computed before any IO: which build dir,
whether to join or seed, which slot class.
_Avoid_: config, options

**Job**:
A recorded execution: key, args, build dir, exit, log. Jobs are how an invocation
is observed and joined by others.
_Avoid_: task, build (a build is the cargo work inside a job)

**Observation**:
What a waiting caller learns about a job: running, finished with an exit, or
vanished without a result.

**Snapshot**:
A copy of a finished build dir kept in the shared cache so a cold build dir can be
seeded warm.
_Avoid_: artifact cache, target cache

**Seed**:
Filling an empty build dir from a snapshot; cargo then rebuilds only what changed.
_Avoid_: restore, hydrate

**Host**:
Everything outside the process that gbb must ask: free disk, worktrees, repo
identity, memory, process liveness.
_Avoid_: system, environment, platform

**Intent**:
What an argv means to gbb: its class, whether it can be joined, reused, or
snapshotted.
_Avoid_: command, classification

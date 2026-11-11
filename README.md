# gbb — cargo build broker for parallel agents

Several agents on one machine, each in its own checkout, all running `cargo`. Today
that produces two distinct failures:

1. **Fake serialization.** A per-user build wrapper (one global lock file) makes agents
   in *unrelated* worktrees queue behind each other, so a 5-minute build stalls every
   other agent for 5 minutes.
2. **Real contention.** Two processes in the *same* build directory collide on cargo's
   own `<build-dir>/<triple>/<profile>/.cargo-build-lock` and print
   `Blocking waiting for file lock on build directory`, with no way to tell how long
   it will take or who holds it.

`gbb` sits in front of cargo and separates those two cases.

## What it does

| Layer | Behaviour |
|---|---|
| **Private build dirs (default)** | Each invocation gets `<target>/parallel/<id>`, so nothing waits for another command. Repeated runs in one checkout reuse the same warm dir; concurrent runs take `-2`, `-3`, … |
| **Shared mode** | `--shared` restores the classic behaviour: one build dir with joining and lock wait, for when disk matters more than latency. |
| **Memory-aware slots** | Global admission: `light` ×6, `heavy` ×3, further capped by `MemAvailable / GBB_JOB_MEM_GB`. RAM is machine-wide, so this cap is too. |
| **Disk-aware** | Private dirs and cache seeds are refused when free space drops below `GBB_RESERVE_GB` (default 25 GB); snapshots prune instead of filling the disk. |
| **Dedupe** | An identical command already running in the same build dir is *joined*, not duplicated. Its captured output is replayed to the joiner, including failures. |
| **Reuse** | A just-finished identical job (same argv + same input digest) is reused for 120 s. Never for `test`/`run`/`bench`: those produce observations, not artifacts. |
| **Fail fast** | No slot or foreign holder → exit `75` with the holder, its age, ETA from observed durations, and the key to wait on. Agents stop hanging on locks. |
| **Health** | While a job runs, a sampler compares it to a learned p90 for the same command *and* concurrency level, and surfaces `SLOW`, `STALLED` or `THRASHING` on stderr and in `gbb status`. `gbb kill <key>` stops it. |
| **Shared cache** | Cold build dirs are seeded from the newest snapshot of the same repository, across worktrees, so only changed units compile. Snapshots dedupe by artifact set and prune by byte budget. |
| **Isolation** | Jobs run under `systemd-run --scope --slice=dev.slice`, so a build can never starve the desktop. |
| **Disk** | `gbb gc` reports the `target/` size of every git worktree and deletes only clean, long-idle ones, and only with `--apply`. |

## Install

```sh
cargo install --path .            # or: cargo install gbb (once published)
install -m755 target/release/gbb ~/.local/bin/gbb
```

Requirements: Linux, Rust ≥ 1.89 (uses stable `std::fs::File::try_lock`).
Zero dependencies. `systemd-run` and `sccache` are optional; both are detected at
runtime and their absence only removes isolation/caching, never correctness.

## Usage

```sh
gbb check -p goldenbee            # light slot, private build dir
gbb test -p goldenbee filter      # heavy slot, private build dir
gbb --shared test -p goldenbee    # one shared dir: join or wait instead
gbb --wait 300 test -p goldenbee  # block up to 5 min for a slot
gbb status                        # slots, running jobs, free disk
gbb doctor                        # resolved build dir, digest time, tool presence
gbb cache                         # snapshots, size, budget
gbb cache --apply                 # dedupe + prune to budget
gbb gc                            # dry run: every worktree's target size
gbb gc --apply --days 7           # delete clean targets idle ≥ 7 days
gbb wait <key|prefix>             # join a job another agent started
gbb kill <key|prefix>             # stop one job, by systemd scope or child pid
gbb kill --all-slow               # stop every job currently reporting abnormality
```

`gbb` accepts the same arguments as cargo; anything it does not recognise is passed
through unchanged.

## Exit codes

| code | meaning |
|---|---|
| `0` | success |
| `1` | build/test failure (cargo's own failure) |
| `2` | usage error |
| `75` | busy — retry later or `gbb wait <key>`; nothing was started |

## Build cache

Target dirs are portable: a copied `target/` keeps its fingerprints, so cargo
recompiles only the units whose inputs changed. `gbb` uses that to make a new
checkout warm:

1. **Seed** — when a build dir has no profile directory yet (fresh worktree, new
   `CARGO_TARGET_DIR`, `--isolated`), gbb restores the newest snapshot for the same
   *repository* (`git rev-parse --git-common-dir`, so all worktrees share a key).
2. **Build** — cargo decides what is stale. Nothing is trusted blindly; the restored
   artifacts only save the work cargo would otherwise redo.
3. **Store** — after a successful artifact-producing command (`build`, `check`,
   `clippy`, `test --no-run`), the build dir is snapshotted. Skipped when the digest
   is unchanged, when the dir exceeds `GBB_SNAPSHOT_MAX_GB`, and never for
   `test`/`run`/`bench` without `--no-run`.
4. **Dedupe / prune** — snapshots are fingerprinted by artifact paths and sizes
   (dep-info and `rustc_info` excluded, because they embed absolute paths). Identical
   snapshots collapse to the newest; over-budget caches drop oldest-first while
   keeping the newest snapshot of each workspace/profile.

`incremental/` is deliberately excluded: it is machine-local, huge (11 GB in this
repo), and cargo rebuilds it without losing artifact reuse.

This machine is ext4, so there is no reflink and no copy-on-write: a snapshot is a
real copy. Measured here while fully loaded: **28 MB/s**, i.e. ~7 minutes for a
13 GB `deps/` — so snapshots run in a **detached background child**, never inline,
and they only start when no other gbb job is running (it takes the build-dir lock
first, so it can never copy a directory another build is rewriting). Seeding a cold
dir is inline, because it is still cheaper than the rebuild it replaces.


## Environment

| variable | default | purpose |
|---|---|---|
| `GBB_WAIT` | `0` | default `--wait` seconds |
| `GBB_LOCK_WAIT` | `240` | how long to queue behind the same build dir (shared mode) |
| `GBB_JOIN_WAIT` | `900` | how long to wait on a joined identical job |
| `GBB_LIGHT_SLOTS` | `6` | concurrent light jobs |
| `GBB_HEAVY_SLOTS` | `3` | concurrent heavy jobs (also capped by available RAM) |
| `GBB_JOB_MEM_GB` | `4` | memory assumed per heavy job for admission |
| `GBB_RESERVE_GB` | `25` | disk that must stay free before private dirs/cache seeds |
| `GBB_NAME` | unset | stable private-dir name (falls back to checkout hash) |
| `GBB_JOBS` | inherited | `CARGO_BUILD_JOBS` for launched jobs |
| `GBB_SLICE` | `dev.slice` | systemd slice |
| `GBB_NO_SYSTEMD` | unset | launch cargo directly |
| `GBB_NO_SCCACHE` | unset | do not inject `RUSTC_WRAPPER=sccache` |
| `GBB_REUSE_SECS` | `120` | reuse window for finished artifact jobs |
| `GBB_NO_CACHE` | unset | disable cache seeding and snapshots |
| `GBB_CACHE_GB` | `80` | snapshot cache budget |
| `GBB_SNAPSHOT_MAX_GB` | `30` | largest build dir worth snapshotting |
| `GBB_SNAPSHOT_MIN_SECS` | `300` | min age before re-snapshotting the same digest |
| `GBB_STORE_FORCE` | unset | snapshot even while other jobs are running |
| `GBB_SAMPLE_SECS` | `5` | health sampling interval |
| `GBB_NOTICE_SECS` | `30` | minimum gap between abnormal notices |
| `GBB_SLOW_FACTOR_PCT` | `200` | elapsed/p90 ratio that counts as slow |
| `GBB_STALL_CPU_PCT` | `2` | CPU below this with no `rustc` child counts as stalled |
| `GBB_SWAP_WARN_GB` | `4` | system swap that turns slow into thrashing |

## Design notes

- **No waiting is the default.** A private build dir per invocation is the only way two
  `cargo` commands can run at once in one checkout; sharing a build dir means cargo
  serializes, and no scheduler can change that. `--shared` is the opt-out for when disk
  is the scarcer resource.
- **Private dirs are disposable.** `gbb gc` lists `target/parallel/*` next to worktree
  targets and deletes them only with `--apply`, after `--days`.
- **Lock identity is the resolved build dir**, not the repo and not the user. A
  per-user lock is what breaks parallelism across worktrees; a per-repo lock is
  wrong when two worktrees deliberately share a target dir.
- **The digest is computed after the lock is taken.** The first build in a fresh
  checkout *creates* `Cargo.lock`, which changes the digest; computing it before the
  lock made simultaneous agents disagree and neither dedupe nor join. Joining
  therefore compares argv and build-dir owner, not the digest.
- **Digest is a stat walk**, not a content hash: `(path, mtime, size)` over `*.rs`,
  `*.toml`, `*.sql`, `*.proto` and cargo manifests, skipping `target/`,
  `node_modules/`, `.git/`, `dist/`, `build/`. ~0.6 s on a 186k-line workspace,
  ~0 ms on a small crate.
- **`cargo test` releases cargo's lock before running test binaries** (verified
  against cargo source and by experiment), so test *execution* never blocks another
  agent. Only compile/link is serialized.
- **A bare `cargo` cannot be serialized against.** `gbb` detects a foreign
  `.cargo-build-lock` and warns; it never blocks silently. Route every invocation
  through `gbb`.
- **`test`/`run`/`bench` are never reused**, even with an identical digest: a green
  result from two minutes ago is not evidence about the code now.
- **Health is a baseline, not a timeout.** A command has no single duration: four
  concurrent builds are slower than one. Samples are bucketed at 1, 2 and 3+ heavy
  jobs, and a run is only abnormal relative to its bucket (falling back to the
  all-bucket distribution until that bucket has two samples). A job with no
  baseline is never called slow. The sampler reads one `/proc/<pid>/stat` per
  process, writes health atomically, and only reports `stalled` after a 30 s grace
  period so dependency resolution is not mistaken for a hang.

## Prior art

Nothing off-the-shelf does this; the survey that led here is worth keeping.

| Approach | Removes cross-checkout serialization | Removes same-checkout contention |
|---|---|---|
| `cargo nextest archive` then run archives | yes (one builder) | yes for test *execution* |
| `sccache` + per-agent `CARGO_TARGET_DIR` | yes, at the cost of duplicate links | yes |
| `-Zfine-grain-locking` (nightly) | no | partially, `check` only |
| Bazel/buck2, BuildBuddy | different build system | n/a locally |
| `cargo-queue`, `heli`, `frust` | experimental, unused | — |
| **gbb** | **yes** | **yes, with join + failure replay** |

`sccache` is the only layer that reduces *total* work rather than rearranging it, so
`gbb` injects it when present (`RUSTC_WRAPPER=sccache`, `CARGO_INCREMENTAL=0`, since
sccache refuses incremental builds and cannot cache bins, dylibs, or proc-macros).

## License

MIT — see [LICENSE](LICENSE).

Copyright (c) 2026 Harunjeylan.

The problem this solves was mapped against cargo's own lock behaviour
(`src/compiler/layout.rs`), `cargo nextest archive`, `sccache`, nightly
`-Zfine-grain-locking` (rust-lang/cargo#4282), and the per-user build cache goal
(rust-lang/cargo#5931). Links to each are in the table above.

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
| **Per-build-dir lock** | Serializes cargo per build directory and profile (cargo's own granularity). Different worktrees build in parallel. |
| **Memory-aware slots** | Global admission: `light` (check/clippy/fmt) ×4, `heavy` (build/test/run/bench) ×2. RAM is machine-wide, so this cap is too. |
| **Dedupe** | An identical command already running in the same build dir is *joined*, not duplicated. Its captured output is replayed to the joiner, including failures. |
| **Reuse** | A just-finished identical job (same argv + same input digest) is reused for 120 s. Never for `test`/`run`/`bench`: those produce observations, not artifacts. |
| **Fail fast** | No slot or foreign holder → exit `75` with the holder, its age, ETA from observed durations, and the key to wait on. Agents stop hanging on locks. |
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
gbb check -p goldenbee            # light slot, per-build-dir lock
gbb test -p goldenbee filter      # heavy slot
gbb --wait 300 test -p goldenbee  # block up to 5 min for a slot/lock
gbb --priority test -p goldenbee  # merge gate: force a heavy slot
gbb status                        # slots, running jobs, free disk
gbb doctor                        # resolved build dir, digest time, tool presence
gbb cache                         # snapshots, size, budget
gbb cache --apply                 # dedupe + prune to budget
gbb gc                            # dry run: every worktree's target size
gbb gc --apply --days 7           # delete clean targets idle ≥ 7 days
gbb wait <key|prefix>             # join a job another agent started
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
real copy. It is still the cheaper half — 13 GB of deps copies in seconds versus
10+ minutes of compilation — and the copy cannot corrupt a live build dir because
seeding only ever fills an empty one.

## Environment

| variable | default | purpose |
|---|---|---|
| `GBB_WAIT` | `0` | default `--wait` seconds |
| `GBB_LOCK_WAIT` | `240` | how long to queue behind the same build dir |
| `GBB_JOIN_WAIT` | `900` | how long to wait on a joined identical job |
| `GBB_LIGHT_SLOTS` | `4` | concurrent light jobs |
| `GBB_HEAVY_SLOTS` | `2` | concurrent heavy jobs |
| `GBB_JOBS` | inherited | `CARGO_BUILD_JOBS` for launched jobs |
| `GBB_SLICE` | `dev.slice` | systemd slice |
| `GBB_NO_SYSTEMD` | unset | launch cargo directly |
| `GBB_NO_SCCACHE` | unset | do not inject `RUSTC_WRAPPER=sccache` |
| `GBB_REUSE_SECS` | `120` | reuse window for finished artifact jobs |
| `GBB_NO_CACHE` | unset | disable cache seeding and snapshots |
| `GBB_CACHE_GB` | `80` | snapshot cache budget |
| `GBB_SNAPSHOT_MAX_GB` | `30` | largest build dir worth snapshotting |
| `GBB_SNAPSHOT_MIN_SECS` | `300` | min age before re-snapshotting the same digest |

## Design notes

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

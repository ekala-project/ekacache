# jig / jigd Quint specification

Executable models of the parts of jig and jigd where getting the design wrong is a correctness
problem, not a cosmetic one. They are the abstract half of the rewrite contract. The concrete
half (byte layouts, messages, exit codes, environment) is in [`../behaviors/`](../behaviors/README.md).
Where the two disagree, `behaviors/` is authoritative about bytes and these models are
authoritative about which interleavings must stay safe.

Checked with Quint 0.33.1. Commands are run from the repository root. Source paths below are relative to
`legacy/`, where the C++ and Go implementation now lives.

| Module | Models | Source of truth | Behaviour doc |
|---|---|---|---|
| `slots.qnt` (`slots`) | jigd's job-slot semaphore: grant rule, fewest-held-first fairness, hang-up release | `jigd/slots.go` | `jigd-slots.md` |
| `slots.qnt` (`slotNesting`) | why a child jig must not take a slot while its parent holds one (`JIG_SLOT`) | `jig/src/process.cc` | `jigd-slots.md`, `environment.md` |
| `store.qnt` | the bitcask store: packs, hints, index, live accounting, tiers, demotion, crash/restart | `jigd/store.go` | `jigd-store.md` |
| `jigd_protocol.qnt` | the line protocol per connection: pipelining, reply shapes, hang up on malformed input | `jigd/main.go` | `jigd-protocol.md` |
| `twokey.qnt` | soundness of the two-key (k1 manifest → k2 artifacts) cache, absent-path tracking | `jig/src/manifest.cc`, `cc_mode.cc` | `jig-keys.md`, `jig-cc-mode.md` |
| `gocache.qnt` | GOCACHEPROG put/get over jigd with concurrent go commands and eviction | `jig/src/gocache_mode.cc` | `jig-gocacheprog.md` |
| `builder_rpc.qnt` | the `jig nix-store` client of builder-rpc-v0 (Nix worker protocol 1.38 subset) against an arbitrary daemon | `jig/src/nix_store_mode.cc` | `jig-nix-store.md` |

## Running

```sh
# scenario tests (each module's `run ...Test` definitions)
quint test --main=slots spec/slots.qnt
quint test spec/store.qnt
quint test spec/jigd_protocol.qnt
quint test spec/twokey.qnt
quint test spec/builder_rpc.qnt

# random simulation against the safety invariants
quint run --main=slots       --invariant=safety       --max-steps=60 --max-samples=5000  spec/slots.qnt
quint run --main=slotNesting --invariant=noNestedWait --max-steps=20                     spec/slots.qnt
quint run --invariant=safety --max-steps=50 --max-samples=3000  spec/store.qnt
quint run --invariant=safety --max-steps=40 --max-samples=5000  spec/jigd_protocol.qnt
quint run --invariant=sound  --max-steps=30 --max-samples=20000 spec/twokey.qnt
quint run --invariant=entriesSound --max-steps=30 --max-samples=20000 spec/twokey.qnt
quint run --invariant=safety --max-steps=30 --max-samples=10000 spec/gocache.qnt
quint run --invariant=safety --max-steps=40 --max-samples=20000 spec/builder_rpc.qnt
```

All of the above report no violation. `gocache.qnt` has no `run` tests; simulation alone covers it.

## Witnesses (expected to fail)

A `witness*` invariant is the negation of an interesting state. A violation proves that state
is reachable, so the safety properties are not holding vacuously.

| Module | Witness | Reached state |
|---|---|---|
| slots | `witnessQueueNonEmpty` | two or more waiters queued |
| slots | `witnessAbandonedWaiterGranted` | only with `CANCEL_ABANDONED = false` (Go): a waiter that hung up is still queued and will be granted (Q-SLOT-1) |
| store | `witnessEviction` | the last tier deleted packs |
| store | `witnessDemoted` | a pack moved to the cold tier |
| store | `witnessPowerLossFallback` | only with `FSYNC_ON_SEAL = false` (Go): after power loss a sealed pack's hint points past its truncated end, so an older value is served |
| store | `idleWithinBudgetEvenAfterStart` | only with `KICK_AT_START = false` (Go): an idle store stays over budget after a restart |
| twokey | `witnessReplay`, `witnessCachedFailure` | a replayed hit, and a replayed deterministic failure |
| gocache | `witnessHit` | a get that returns a body |
| builder_rpc | `witnessSubmitSuccess` | `submit` exits 0. Needs `--max-samples=200000` |
| builder_rpc | `witnessFetchSuccess` | `fetchurls` of 2 elements exits 0. Too rare for random simulation, so `fetchTwoTest` covers it deterministically |

## Flip parameters

Each one is a `pure val` at the top of its module. Set it to the other value to see the
counterexample that motivates the real design.

Adopted-change flags (signed off 2026-11-17, see `behaviors/README.md`) default to the Rust
port's behaviour. Setting one to `false` reproduces the Go daemon, and `safety` still passes
there, because the old behaviour is guarded in the invariants:

| Parameter | Adopted | Go (`false`) |
|---|---|---|
| `slots.CANCEL_ABANDONED` | waiter dequeued on hang-up; `noDeadWaiters` holds | `witnessAbandonedWaiterGranted` is reachable |
| `store.KICK_AT_START` | demoter kicked by OpenStore; `idleWithinBudgetEvenAfterStart` holds | it fails |
| `store.DROP_EMPTY_PACKS` | no empty sealed packs (`noEmptySealedPacks`) | every restart may leave an empty pack |
| `store.FSYNC_ON_SEAL` | sealed packs survive power loss; `witnessPowerLossFallback` unreachable | reachable |

Design-assumption flags:

| Parameter | Default | If flipped |
|---|---|---|
| `slotNesting.HONOR_JIG_SLOT` | `true` | `false`: a child jig asks for a slot while its parent holds the only one, and `noNestedWait` is violated (deadlock) |
| `twokey.ABSENT_LOG` | `true` | `false` (stock GCC, no `JIG_ABSENT_LOG`): a header that appears earlier on the search path is not noticed, and `sound` is violated |
| `twokey.MUTATE_DURING_COMPILE` | `false` | `true`: an input edited between the compiler reading it and jig hashing it gets cached under the wrong identity (the TOCTOU quirk), and `sound` is violated |

The sizes (`LIMIT`, `BUDGET`, `PACK_LIMIT`, `MAX_IDS`, ...) are scaled-down stand-ins for the
real constants, which are listed in the behaviour docs. Changing them only changes how much
state is explored.

## Modelling conventions

- Ghost variables (`history`, `announced`, `kickedSinceStart`, `powerLost`, `opsCompleted`, ...)
  exist only to state properties. They have no runtime counterpart.
- Hashes are modelled as structural equality, so the models assume no collisions.
- Guards named after quirks (for example `newestWins` guarded by `not(powerLost)`) mark places
  where the current implementation is deliberately weaker than the ideal. The quirk is described
  in the matching behaviour doc, together with the recommended fix.

# Glossary

**Absent path.** A file a compile looked for and did not find. Only compilers that report
these let `ec` notice when such a file appears later; stock GCC and clang do not.

**Content mode.** The identity mode in which store files are identified by their contents
and store hashes are masked out of keys. Set with `EKACACHE_STORE_IDENTITY=content`.

**Cold tier.** An optional second store directory (`EKACACHED_COLD`) that takes packs pushed
out of the main tier.

**Demotion.** Moving a whole pack from the main tier to the cold tier.

**Driver policy.** What a toolchain's `etc/ekacache.json` tells `ec` to do besides caching:
injected flags, RUNPATH, dynamic linker.

**Eviction.** Deleting a whole pack to stay within a tier's budget.

**Hint file.** The index of a sealed pack (`NNNNNN.hint`), so `ecd` can start without reading
every pack.

**Identity.** What a manifest records for an input file: its store path (store files in path
mode) or a hash of its contents.

**k1, k2.** The two cache keys. k1 covers what is known before compiling and names a
manifest; k2 covers k1 plus the manifest and names the results. See
[Cache keys](../concepts/keys.md).

**Launcher mode.** `ec /path/to/compiler …`: `ec` runs in front of the given program.

**Manifest.** The list of a compile's inputs and their identities, stored under k1.

**Pack.** An append-only data file in the store (`NNNNNN.pack`), up to about 256 MiB.

**Path mode.** The default identity mode: store files are identified by their path.

**Seal.** Closing the active pack and writing its hint file.

**Slot.** A host-wide token a compile holds while it runs. `ecd` has `EKACACHED_SLOTS` of
them.

**Tier.** A store directory with a size budget: the main tier, and optionally a cold tier.

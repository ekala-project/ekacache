# How ekacache works

```text
 compiler invocation (gcc, clang, rustc, go, …)
        │  wrapper symlink / launcher / RUSTC_WRAPPER / GOCACHEPROG
        ▼
  ec        one short-lived process per compile
        │  line protocol over a Unix socket
        │  (EKACACHE_SOCK, default /nix/var/nix/ekacache/socket)
        ▼
  ecd       one daemon per machine
        ├─ store:  append-only pack files, size budget, eviction
        ├─ slots:  host-wide limit on concurrent compiles
        └─ identities: fingerprints of Nix store files
```

## A compile, step by step

1. A build runs `gcc -c foo.c -o foo.o`, and `gcc` is really `ec`.
2. `ec` decides whether the invocation is cacheable. Queries such as `--version` or `-M`,
   and flags that write side files `ec` cannot capture, make it uncacheable. An uncacheable
   invocation just runs the real compiler.
3. `ec` computes a first key, **k1**, from the compiler's identity, the working directory, the
   normalised arguments and the bytes of the source file, and asks `ecd` for the
   *manifest* stored under it. The manifest lists every file the compile read, with an
   identity for each.
4. If every input in the manifest still has the same identity, `ec` computes a second key,
   **k2**, from k1 and the manifest, and fetches the stored result: the object file, the
   compiler's stderr, the dependency file and, for a failure, the exit status.
5. On a **hit**, `ec` writes those files out, prints the stored stderr and exits with the
   stored status. The real compiler does not run.
6. On a **miss**, `ec` first takes a *slot* from `ecd`, which may mean waiting while the
   machine is busy. It then runs the real compiler with a dependency file switched on, and
   stores the result under both keys for next time.

The details are in [Cache keys](concepts/keys.md).

## Slots

A machine running eight sandboxes, each with `make -j16`, would start 128 compilers at once.
`ecd` keeps one host-wide pool of slots, by default one per CPU. Every `ec` miss holds a slot
while the compiler runs; hits need none. When slots are short, the build holding the fewest
goes first, so one large build cannot starve the others.

You can run any other heavy command under the same limit with
[`ec slot`](reference/ec-slot.md).

## When the daemon is not there

`ec` never needs `ecd` to succeed. If the socket is missing, refuses the connection, or the
daemon fails partway through a request, `ec` treats it as a miss, runs the compiler, and
exits with the compiler's status. There is also no slot limit in that case. The only
commands that report a missing daemon are the explicit `ec cache` commands, which exit with
status 2.

The reverse is not guaranteed. `ec` has no timeouts, so a daemon that accepts connections and
then hangs will hang builds. See [Troubleshooting](operating/troubleshooting.md).

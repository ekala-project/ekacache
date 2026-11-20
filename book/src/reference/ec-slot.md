# ec slot

Run a program while holding one of `ecd`'s host-wide job slots.

```text
usage: ec slot <program> [args...]
```

Compiles that miss the cache already take a slot each. `ec slot` puts other heavy steps,
such as a code generator or a link not run through `ec`, under the same machine-wide limit
(`EKACACHED_SLOTS`, by default the number of CPUs).

```console
$ ec slot ./generate-bindings.sh
$ ec slot ld.lld @big-link.rsp
```

## Behaviour

1. If `EKACACHE_SLOT` is set to anything non-empty, `ec` assumes a parent already holds a
   slot and does not ask for one.
2. Otherwise it asks `ecd` for a slot and waits until one is free. There is no timeout.
3. The program runs with `EKACACHE_SLOT=1` in its environment, so `ec` compilers it starts
   do not wait for a second slot. stdin, stdout and stderr are inherited, and the program
   is looked up on `PATH`.
4. When the program exits, the slot is released.

If there is no daemon, the program runs straight away with no slot.

Slots are shared fairly between builds: the build holding the fewest slots is served first.
A build is identified by `$NIX_BUILD_TOP`, and everything outside Nix shares one bucket.

## Exit status

| Case | Exit |
|---|---|
| no program given | 2, after the usage message |
| program exits with N | N |
| program killed by signal N | 128 + N |
| program cannot be run | 127, with no message |

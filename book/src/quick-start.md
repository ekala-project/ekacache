# Quick start

This walks through trying ekacache by hand on one Linux machine, outside a sandbox, to see a
cache hit. For a real deployment, see [Running ecd](setup/running-ecd.md).

## 1. Build the two programs

From a checkout of the repository:

```console
$ nix-build -A ekacache -o result      # result/bin/ec, result/libexec/ekacache/*
$ nix-build -A ekacached -o result-ecd # result-ecd/bin/ecd
```

## 2. Start a daemon on a private socket

```console
$ mkdir -p /tmp/ec-demo
$ XDG_CACHE_HOME=/tmp/ec-demo/cache result-ecd/bin/ecd /tmp/ec-demo/socket
2026/11/19 10:00:00 tier 0 /tmp/ec-demo/cache/ekacache/packs budget 53687091200
2026/11/19 10:00:00 listening on /tmp/ec-demo/socket, 24 slots, keys=0 packs=1 bytes=0 live=0
```

The store goes in `$XDG_CACHE_HOME/ekacache/packs`. ecd logs to stderr and runs in the
foreground. Leave it running and open a second terminal.

## 3. Compile twice through the launcher

You need a C compiler on `PATH`. If you don't have one, run `nix-shell -p gcc` first.

```console
$ export EKACACHE_SOCK=/tmp/ec-demo/socket
$ export EKACACHE_LOG=/tmp/ec-demo/log
$ printf 'int main(void) { return 0; }\n' > hello.c
$ result/bin/ec "$(command -v gcc)" -c hello.c -o hello.o
$ rm hello.o
$ result/bin/ec "$(command -v gcc)" -c hello.c -o hello.o
$ cat /tmp/ec-demo/log
cc	compiled	hello.c new-key	254
cc	cached	hello.c	4
```

`ec /path/to/gcc …` is *launcher mode*: the path must contain a `/`, and `ec` uses that
program as the real compiler. The first run is a miss (`new-key`), the second is a hit.
The last column of the log is the time in milliseconds.

## 4. Ask the daemon for statistics

```console
$ printf 'STATS\n' | nc -U -w1 /tmp/ec-demo/socket
gets=6 hits=3 puts=3 ids=0 slots=0 waiting=0 keys=3 packs=1 bytes=654 live=528
```

Each compile stores and looks up several entries (its manifest, the object file and the
stderr), so the counters count entries, not compiles.

## 5. Clean up

Stop `ecd` with Ctrl-C (SIGINT). It seals its active pack and removes the socket. Then
`rm -rf /tmp/ec-demo`.

## Next steps

- Run `ecd` as a service and map its socket into the Nix sandbox:
  [Running ecd](setup/running-ecd.md).
- Make the compilers in your package set go through `ec`:
  [Hooking ec into a build](builds/integration.md).

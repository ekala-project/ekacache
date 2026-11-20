# Go

Go has its own build cache with a plug-in interface,
[`GOCACHEPROG`](https://pkg.go.dev/cmd/go/internal/cacheprog). `ec gocacheprog` implements
that interface on top of `ecd`, so Go build actions are shared across sandboxes.

**Go 1.24 or newer is required.**

## Setting it up

Either set `GOCACHEPROG` yourself:

```console
$ export GOCACHEPROG=$ekacache/libexec/ekacache/gocacheprog
$ go build ./...
```

or run `go` through the launcher:

```console
$ ec /nix/store/…-go/bin/go build ./...
```

The launcher sets `GOCACHEPROG="<path to ec> gocacheprog"` when `GOCACHEPROG` is unset or
empty **and** the socket file exists. It only checks that the file exists and does not try to
connect.

## Behaviour

- Go stays in charge of correctness. Go computes its own action IDs, and `ec` stores and
  returns what Go gives it, without the input checks it does for C and Rust.
- Files Go asks to read are written under `$GOCACHEPROG_DIR`, else `$TMPDIR/gocacheprog`,
  else `/tmp/gocacheprog`. `ec` never cleans this directory. In a Nix sandbox it goes away
  with the build.
- With no daemon, `ec gocacheprog` acts as a cache that always misses. Builds still work.
- cgo's C compiles run in a temporary `go-build…` directory and never hit, so `ec` does not
  cache them. Go's own compile actions are cached through `GOCACHEPROG`.

# Other toolchains

Most ecosystems reach `ec` through the C compiler. This table summarises what to expect.

| Ecosystem | Effect |
|---|---|
| Python C extensions, node-gyp, cargo's `cc` crate, JNI, C stubs of Haskell or OCaml packages in the source tree | ordinary `cc -c` calls, cached as usual |
| cgo | the C parts compile in a temporary directory and are not cached; Go itself is cached through [`GOCACHEPROG`](go.md) |
| GHC | `ghc --make` compiles Haskell in-process; only C stubs and links reach `cc`. GHC's temporary C stubs are not cached |
| OCaml | `ocamlopt`'s temporary `camlasm*.s` files are not cached |
| Fortran | passed through uncached (module files are side outputs) |
| CUDA, HIP | not cached (`nvcc` temporary files, `.cu` is not a source extension) |
| Objective-C++ (`.mm`) | not cached |
| `mpicc`, `hipcc`, `zig cc`, `swiftc` | not recognised; they benefit only if the compiler they run underneath goes through `ec` |
| JVM (javac, Kotlin, Scala) | no benefit: Gradle and Maven compile in-process |
| Node, TypeScript, Python bytecode, Zig | nothing to intercept |

## Caching anything else

For a step whose output depends on inputs you can hash yourself, such as vendoring
dependencies or code generation, use [`ec cache`](../reference/ec-cache.md) directly:

```sh
key="vendor-$(sha256sum go.sum | cut -c1-32)"
if ! ec cache get-dir "$key" vendor; then
  go mod vendor
  ec cache put-dir "$key" vendor || true
fi
```

To keep a heavy non-compiler step within the machine's slot limit, wrap it in
[`ec slot`](../reference/ec-slot.md).

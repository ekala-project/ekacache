# Agent docs

Short, high-level guides to working on ekacache. They point into the detailed docs rather than
repeating them.

| Doc | Read it when |
|---|---|
| [architecture.md](architecture.md) | you need the big picture: components, keys, where the code lives |
| [development.md](development.md) | you are building, testing or debugging CI |
| [conventions.md](conventions.md) | you are about to write code |
| [recipes.md](recipes.md) | you are fixing a bug, matching a quirk, adding a test or changing behaviour |

The detailed sources, in order of authority:

1. [`../behaviors/`](../behaviors/README.md): what the code must do, byte for byte, with every
   quirk numbered (`Q-AREA-n`).
2. [`../spec/`](../spec/README.md): Quint models of slots, the store, the protocol, the two-key
   lookup, gocacheprog and builder RPC.
3. [`../implementation/`](../implementation/README.md): how the Rust port is built, phase by
   phase. [`STATUS.md`](../implementation/STATUS.md) has the current state and the known
   differences.

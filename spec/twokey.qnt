// -*- mode: Bluespec; -*-
/**
 * Soundness of jig's two-key cache (jig/src/manifest.cc, jig/src/cc_mode.cc).
 *
 *   k1 = H(tool, cwd, normalised args, primary source bytes)     m/<k1> -> manifest
 *   k2 = H(k1, manifest lines)                                    o/ e/ d/ r/ <k2> -> artifacts
 *
 * A manifest line is "path\tid" for every input the compiler read (other than the primary
 * source, which is in k1) and "!path" for every lookup it made and missed. A hit requires every
 * id to be unchanged and every "!" path to still be absent; k2 is then recomputed from the
 * stored lines, so a hit replays exactly the artifacts stored with that manifest.
 *
 * The model: two Nix builds (separate file systems, the same cwd /build/source and the same
 * primary source, hence the SAME k1 -- the realistic case), one shared daemon store with
 * eviction, a deterministic compiler that searches `x.h` in -Ia then -Ib, includes `y.h`, and
 * fails deterministically when y.h has content 3. Hashing is modelled as structural equality
 * (no collisions: k2 is the manifest itself).
 *
 * Theorem checked (`sound`): whatever jig hands back -- replayed or freshly compiled -- equals
 * what the real compiler produces on the build's file system at that moment.
 *
 * It holds under two assumptions, each a parameter below; flip one to see the counterexample:
 *  - ABSENT_LOG: the compiler reports missed lookups (JIG_ABSENT_LOG). Without it a header that
 *    appears earlier on the search path is not noticed.
 *  - not MUTATE_DURING_COMPILE: inputs do not change between the compiler reading them and jig
 *    hashing them for the manifest (jig hashes after the compiler exits).
 *
 * Run:
 *   quint run --invariant=sound --max-steps=30 --max-samples=20000 spec/twokey.qnt
 *   quint test spec/twokey.qnt
 */
module twokey {
  pure val ABSENT_LOG = true
  pure val MUTATE_DURING_COMPILE = false

  pure val BUILDS = Set(1, 2)
  pure val PATHS = Set("a/x.h", "b/x.h", "y.h")
  pure val CONTENTS = Set(0, 1, 2, 3)   // 0 = file absent

  type Line = { path: str, id: int }    // id -1 = "!path"
  type Result = { status: int, out: int } // status 0 = success, out = object bytes (or stderr)
  type Compile = { res: Result, read: List[str], missed: List[str], missingInput: bool }

  pure def content(fs: str -> int, p: str): int = fs.get(p)

  // The real compiler on a file system: search x.h in a/ then b/, then include y.h.
  pure def compile(fs: str -> int): Compile =
    if (content(fs, "a/x.h") != 0) {
      val x = content(fs, "a/x.h")
      if (content(fs, "y.h") == 0) { res: { status: 1, out: 0 }, read: ["a/x.h"], missed: ["y.h"], missingInput: true }
      else if (content(fs, "y.h") == 3) { res: { status: 1, out: 100 + x }, read: ["a/x.h", "y.h"], missed: [], missingInput: false }
      else { res: { status: 0, out: 10 * x + content(fs, "y.h") }, read: ["a/x.h", "y.h"], missed: [], missingInput: false }
    } else if (content(fs, "b/x.h") != 0) {
      val x = content(fs, "b/x.h")
      if (content(fs, "y.h") == 0) { res: { status: 1, out: 0 }, read: ["b/x.h"], missed: ["a/x.h", "y.h"], missingInput: true }
      else if (content(fs, "y.h") == 3) { res: { status: 1, out: 100 + x }, read: ["b/x.h", "y.h"], missed: ["a/x.h"], missingInput: false }
      else { res: { status: 0, out: 10 * x + content(fs, "y.h") }, read: ["b/x.h", "y.h"], missed: ["a/x.h"], missingInput: false }
    } else {
      // "x.h: No such file or directory": MissingInput, never cached
      { res: { status: 1, out: 0 }, read: [], missed: ["a/x.h", "b/x.h"], missingInput: true }
    }

  // BuildManifest: inputs with their current identity, then absent paths still absent (deduped)
  pure def manifestOf(fs: str -> int, read: List[str], missed: List[str]): List[Line] = {
    val present = read.foldl(List(), (acc, p) => if (content(fs, p) != 0) acc.append({ path: p, id: content(fs, p) }) else acc)
    val absent = if (ABSENT_LOG) missed else List()
    absent.foldl(present, (acc, p) =>
      if (content(fs, p) != 0 or acc.foldl(false, (seen, l) => seen or (l.id == -1 and l.path == p))) acc
      else acc.append({ path: p, id: -1 }))
  }

  // ValidateManifest: every id unchanged, every "!" path still absent
  pure def valid(fs: str -> int, m: List[Line]): bool =
    m.foldl(true, (ok, l) => ok and (if (l.id == -1) content(fs, l.path) == 0 else content(fs, l.path) == l.id))

  // ----- state -----
  var fs: int -> (str -> int)             // each build's file system
  var manifest: List[Line]                // m/<k1> (k1 is the same for both builds)
  var hasManifest: bool
  var artifacts: List[Line] -> Result     // o/ or r/+e/ under k2 = the manifest
  var pending: int -> { busy: bool, c: Compile }   // a compile whose manifest is not built yet
  var last: int -> { valid: bool, got: Result, expected: Result, replayed: bool }

  pure val noCompile = { res: { status: 0, out: 0 }, read: [], missed: [], missingInput: false }

  action init = {
    nondet fs1 = PATHS.setOfMaps(CONTENTS).oneOf()
    nondet fs2 = PATHS.setOfMaps(CONTENTS).oneOf()
    all {
      fs' = Map(1 -> fs1, 2 -> fs2),
      manifest' = List(),
      hasManifest' = false,
      artifacts' = Map(),
      pending' = BUILDS.mapBy(_b => { busy: false, c: noCompile }),
      last' = BUILDS.mapBy(_b => { valid: false, got: { status: 0, out: 0 }, expected: { status: 0, out: 0 }, replayed: false }),
    }
  }

  // a build edits its tree (configure writing a header, a patch phase, ...)
  action edit(b: int, p: str, v: int): bool = all {
    not(pending.get(b).busy) or MUTATE_DURING_COMPILE,
    fs' = fs.setBy(b, f => f.set(p, v)),
    manifest' = manifest, hasManifest' = hasManifest, artifacts' = artifacts, pending' = pending, last' = last,
  }

  // jig is invoked: FindResult. Hit -> replay; miss -> run the compiler (manifest built later).
  action invoke(b: int): bool = all {
    not(pending.get(b).busy),
    invokeOn(b, fs.get(b)),
    fs' = fs, manifest' = manifest, hasManifest' = hasManifest, artifacts' = artifacts,
  }

  action invokeOn(b: int, f: str -> int): bool = {
    if (hasManifest and valid(f, manifest) and artifacts.keys().contains(manifest)) all {
      last' = last.set(b, { valid: true, got: artifacts.get(manifest), expected: compile(f).res, replayed: true }),
      pending' = pending,
    } else all {
      pending' = pending.set(b, { busy: true, c: compile(f) }),
      last' = last,
    }
  }

  // the compiler exited: forward its result; store unless the failure is not replayable
  // (CompileAndStore: MissingInput failures are never stored). Manifest ids are read NOW.
  action finish(b: int): bool = all {
    pending.get(b).busy,
    finishWith(b, pending.get(b).c, fs.get(b)),
    pending' = pending.set(b, { busy: false, c: noCompile }),
    fs' = fs,
  }

  action finishWith(b: int, c: Compile, f: str -> int): bool = {
    val m = manifestOf(f, c.read, c.missed)
    val store = not(c.res.status != 0 and c.missingInput)
    all {
      manifest' = if (store) m else manifest,
      hasManifest' = (hasManifest or store),
      artifacts' = if (store) artifacts.put(m, c.res) else artifacts,
      last' = last.set(b, { valid: true, got: c.res, expected: c.res, replayed: false }),
    }
  }

  // the daemon drops the manifest or an artifact (eviction); a dropped artifact under a live
  // manifest is "object-gone": jig recompiles
  action evictManifest: bool = all {
    hasManifest,
    hasManifest' = false, manifest' = List(),
    fs' = fs, artifacts' = artifacts, pending' = pending, last' = last,
  }
  action evictArtifact(m: List[Line]): bool = all {
    artifacts.keys().contains(m),
    artifacts' = artifacts.keys().exclude(Set(m)).mapBy(k => artifacts.get(k)),
    fs' = fs, manifest' = manifest, hasManifest' = hasManifest, pending' = pending, last' = last,
  }

  action step = {
    nondet b = BUILDS.oneOf()
    nondet p = PATHS.oneOf()
    nondet v = CONTENTS.oneOf()
    any {
      edit(b, p, v),
      invoke(b),
      finish(b),
      evictManifest,
      if (artifacts.keys().size() > 0) { nondet m = artifacts.keys().oneOf() evictArtifact(m) } else evictManifest,
    }
  }

  // ----- properties -----
  // a replay hands back exactly what compiling now would
  val sound = BUILDS.forall(b => last.get(b).valid implies last.get(b).got == last.get(b).expected)
  // every stored entry is consistent: on any file system where its manifest validates, the
  // compiler produces the stored result (the per-entry form of `sound`)
  val entriesSound = artifacts.keys().forall(m =>
    BUILDS.forall(b => valid(fs.get(b), m) implies compile(fs.get(b)).res == artifacts.get(m)))

  // witnesses (expected to fail as invariants: these situations are reachable)
  val witnessReplay = not(BUILDS.exists(b => last.get(b).replayed))
  val witnessCachedFailure = not(BUILDS.exists(b => last.get(b).replayed and last.get(b).got.status != 0))

  // ----- scenarios -----
  action stutter = all {
    fs' = fs, manifest' = manifest, hasManifest' = hasManifest, artifacts' = artifacts,
    pending' = pending, last' = last,
  }
  action setFs(f1: str -> int, f2: str -> int): bool = all {
    fs' = Map(1 -> f1, 2 -> f2),
    manifest' = List(), hasManifest' = false, artifacts' = Map(),
    pending' = BUILDS.mapBy(_b => { busy: false, c: noCompile }),
    last' = BUILDS.mapBy(_b => { valid: false, got: { status: 0, out: 0 }, expected: { status: 0, out: 0 }, replayed: false }),
  }

  // build 1 compiles with x.h from b/; build 2 has an a/x.h that shadows it: the "!a/x.h" line
  // makes build 2 miss instead of replaying build 1's object
  run shadowingHeaderTest =
    setFs(Map("a/x.h" -> 0, "b/x.h" -> 1, "y.h" -> 2), Map("a/x.h" -> 2, "b/x.h" -> 1, "y.h" -> 2))
      .then(invoke(1)).then(finish(1))
      .then(all { assert(manifest == [{ path: "b/x.h", id: 1 }, { path: "y.h", id: 2 }, { path: "a/x.h", id: -1 }]), invoke(2) })
      .then(all { assert(pending.get(2).busy), finish(2) })
      .then(all { assert(sound), assert(entriesSound), stutter })

  // a deterministic failure is cached and replayed with its status
  run cachedFailureTest =
    setFs(Map("a/x.h" -> 1, "b/x.h" -> 0, "y.h" -> 3), Map("a/x.h" -> 1, "b/x.h" -> 0, "y.h" -> 3))
      .then(invoke(1)).then(finish(1))
      .then(invoke(2))
      .then(all { assert(last.get(2).replayed), assert(last.get(2).got.status == 1), assert(sound), stutter })

  // a missing header is never cached: the next invocation compiles again
  run missingInputNotCachedTest =
    setFs(Map("a/x.h" -> 0, "b/x.h" -> 0, "y.h" -> 1), Map("a/x.h" -> 0, "b/x.h" -> 0, "y.h" -> 1))
      .then(invoke(1)).then(finish(1))
      .then(all { assert(not(hasManifest)), invoke(1) })
      .then(all { assert(pending.get(1).busy), stutter })
}

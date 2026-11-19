// -*- mode: Bluespec; -*-
/**
 * jigd's bitcask pack store (jigd/store.go): append-only packs, an in-memory index, hint files
 * for sealed packs, tiers with byte budgets, the demoter's victim order, and crash recovery.
 *
 * Abstractions:
 *  - all keys are 1 byte long; a value is (size, version). Versions are ghosts that let us say
 *    "the index only ever serves something that was really put under that key".
 *  - a pack file is a list of complete records plus `tail` bytes of a torn record.
 *  - a hint file is the set of (key, vlen, record offset) it lists; its order (Go map order)
 *    carries no meaning.
 *  - time is an integer clock; TOUCH_EVERY stands for touchEvery (1 minute).
 *  - moveDown to a slower tier is split into the unlocked copy and the locked switch so that a
 *    crash in between leaves the pack in both tiers, as on disk.
 *
 * Run:
 *   quint run --invariant=safety --max-steps=50 --max-samples=3000 spec/store.qnt
 *   quint test spec/store.qnt
 */
module store {
  // ---------------------------------------------------------------------------------------
  // Parameters
  // ---------------------------------------------------------------------------------------
  pure val NTIERS = 2                 // tiers[0] takes writes; the last tier deletes
  pure val BUDGET = [30, 45]          // bytes per tier, 0 = unbounded (JIGD_SIZE, JIGD_COLD_SIZE)
  pure val PACK_LIMIT = 24            // packLimit (256 MiB)
  pure val REC_HEADER = 8             // u32 keylen + u32 vallen
  pure val KLEN = 1
  pure val KEYS = Set("a", "b", "c")
  pure val VLENS = Set(0, 2, 5, 9)
  pure val TOUCH_EVERY = 2
  // Adopted changes (signed off 2026-11-17); false reproduces the Go daemon.
  pure val KICK_AT_START = true       // Q-STORE-5: OpenStore kicks the demoter once
  pure val DROP_EMPTY_PACKS = true    // Q-STORE-2: 0-byte packs are deleted at open (ids are not reused)
  pure val FSYNC_ON_SEAL = true       // Q-STORE-7: a sealed pack and its hint are durable

  type Record = { key: str, vlen: int, off: int, ver: int }   // off: of the record (its header)
  type HintEnt = { key: str, vlen: int, off: int }          // .hint record: klen, vlen, u64 off, key
  type File = { recs: List[Record], tail: int, hasHint: bool, hint: Set[HintEnt], mtime: int }
  type Loc = { pack: int, off: int, len: int }              // store.go loc: off is of the VALUE
  type Pack = { tier: int, size: int, live: int, used: int, touched: int }

  // --- disk ---
  var files: (int, int) -> File       // (tier, pack id) -> the .pack (+ .hint) in that tier's dir
  // --- daemon memory (lost on crash) ---
  var index: str -> Loc
  var packs: int -> Pack
  var active: int
  var totals: List[int]
  var demoting: bool                  // the demoter was kicked and is looping
  var copying: int                    // pack being copied to the next tier, -1 = none
  // --- environment and ghosts ---
  var now: int
  var nextVer: int
  var maxId: int                      // highest pack id ever created (bounds the reload scan)
  var history: str -> Set[int]        // versions acknowledged (PUT answered OK) per key
  var powerLost: bool                 // some crash dropped unsynced bytes
  var kickedSinceStart: bool          // a Put kicked the demoter since the daemon (re)started

  // ---------------------------------------------------------------------------------------
  // Pure helpers
  // ---------------------------------------------------------------------------------------
  pure def recLen(vlen: int): int = REC_HEADER + KLEN + vlen
  pure def fileSize(f: File): int = f.recs.foldl(0, (acc, r) => acc + recLen(r.vlen)) + f.tail
  pure def mapWithout(m: a -> b, k: a): a -> b = m.keys().exclude(Set(k)).mapBy(x => m.get(x))
  pure def emptyFile(t: int): File = { recs: List(), tail: 0, hasHint: false, hint: Set(), mtime: t }
  pure def valueOff(recOff: int): int = recOff + REC_HEADER + KLEN
  pure def stale(p: Pack): bool = p.live * 2 < p.size

  // seal(): the hint lists every index entry that points into pack id
  pure def hintOf(idx: str -> Loc, id: int): Set[HintEnt] =
    idx.keys().filter(k => idx.get(k).pack == id)
       .map(k => { key: k, vlen: idx.get(k).len, off: idx.get(k).off - REC_HEADER - KLEN })

  // victim(): first tier with budget > 0 and total > budget that has a non-active pack;
  // within it: stale first, then least recently used, then lowest id. -1 = none.
  pure def before(p: Pack, pid: int, q: Pack, qid: int): bool =
    if (stale(p) != stale(q)) stale(p)
    else p.used < q.used or (p.used == q.used and pid < qid)

  pure def victimIn(ps: int -> Pack, act: int, t: int): int =
    ps.keys().filter(id => ps.get(id).tier == t and id != act)
      .fold(-1, (best, id) => if (best == -1 or before(ps.get(id), id, ps.get(best), best)) id else best)

  pure def victim(ps: int -> Pack, act: int, tot: List[int]): int =
    range(0, NTIERS).foldl(-1, (found, t) =>
      if (found != -1) found
      else if (BUDGET[t] <= 0 or tot[t] <= BUDGET[t]) -1
      else victimIn(ps, act, t))

  // ---------------------------------------------------------------------------------------
  // OpenStore: rebuild memory from disk (also used after every crash)
  // ---------------------------------------------------------------------------------------
  type Loaded = { files: (int, int) -> File, index: str -> Loc, packs: int -> Pack, totals: List[int], last: int }

  // Q-STORE-2 (adopted): after loading, delete every pack with no bytes. `last` keeps counting
  // from the highest id seen, so the next pack never reuses a deleted id.
  pure def dropEmpty(s: Loaded): Loaded =
    if (not(DROP_EMPTY_PACKS)) s
    else s.packs.keys().filter(id => s.packs.get(id).size == 0).fold(s, (acc, id) =>
      acc.with("files", mapWithout(acc.files, (acc.packs.get(id).tier, id)))
         .with("packs", mapWithout(acc.packs, id)))

  // load()'s add: newest pack id wins whatever the load order; replaced bytes leave `live`
  pure def addEntry(s: Loaded, key: str, l: Loc): Loaded =
    if (s.index.keys().contains(key) and s.index.get(key).pack > l.pack) s
    else {
      val ps1 = if (s.index.keys().contains(key)) {
        val old = s.index.get(key)
        s.packs.setBy(old.pack, p => p.with("live", p.live - old.len))
      } else s.packs
      val ps2 = ps1.setBy(l.pack, p => p.with("live", p.live + l.len))
      s.with("index", s.index.put(key, l)).with("packs", ps2)
    }

  pure def loadPack(s: Loaded, t: int, id: int): Loaded = {
    val f = s.files.get((t, id))
    val fsize = fileSize(f)
    val s0 = s.with("packs", s.packs.put(id, { tier: t, size: fsize, live: 0, used: f.mtime, touched: f.mtime }))
    if (f.hasHint) {
      // hint entries past the end of the pack (pack bytes lost, hint kept) are skipped
      val s1 = f.hint.fold(s0, (acc, e) =>
        if (e.off + REC_HEADER + KLEN + e.vlen > fsize) acc
        else addEntry(acc, e.key, { pack: id, off: valueOff(e.off), len: e.vlen }))
      s1.with("totals", s1.totals.replaceAt(t, s1.totals[t] + fsize))
        .with("last", if (id > s1.last) id else s1.last)
    } else {
      // unsealed: scan complete records in order, truncate a torn tail, then seal
      val s1 = f.recs.foldl(s0, (acc, r) => addEntry(acc, r.key, { pack: id, off: valueOff(r.off), len: r.vlen }))
      val truncated = f.with("tail", 0)
      val newSize = fileSize(truncated)
      val sealed = truncated.with("hasHint", true).with("hint", hintOf(s1.index, id))
      s1.with("files", s1.files.put((t, id), sealed))
        .with("packs", s1.packs.setBy(id, p => p.with("size", newSize)))
        .with("totals", s1.totals.replaceAt(t, s1.totals[t] + newSize))
        .with("last", if (id > s1.last) id else s1.last)
    }
  }

  // tiers fast first, ids ascending; an id already loaded from a faster tier is a leftover of
  // an interrupted demotion: the slower copy (pack and hint) is deleted
  pure def openStore(fs: (int, int) -> File, upTo: int): Loaded =
    range(0, NTIERS).foldl({ files: fs, index: Map(), packs: Map(), totals: [0, 0], last: 0 }, (st, t) =>
      range(1, upTo + 1).foldl(st, (s, id) =>
        if (not(s.files.keys().contains((t, id)))) s
        else if (s.packs.keys().contains(id)) s.with("files", mapWithout(s.files, (t, id)))
        else loadPack(s, t, id)))

  // ---------------------------------------------------------------------------------------
  // Actions
  // ---------------------------------------------------------------------------------------
  action init = all {
    files' = Map((0, 1) -> emptyFile(0)),
    index' = Map(),
    packs' = Map(1 -> { tier: 0, size: 0, live: 0, used: 0, touched: 0 }),
    active' = 1,
    totals' = [0, 0],
    demoting' = false,
    copying' = -1,
    now' = 0,
    nextVer' = 1,
    maxId' = 1,
    history' = KEYS.mapBy(_k => Set()),
    powerLost' = false,
    kickedSinceStart' = false,
  }

  // Put: rotate if the record would push a non-empty active pack past PACK_LIMIT, append,
  // repoint the index, account live/total, kick the demoter when tier 0 is over budget.
  action doPut(k: str, vlen: int): bool = {
    val rl = recLen(vlen)
    val a = packs.get(active)
    val rotate = a.size + rl > PACK_LIMIT and a.size > 0
    // rotate(active+1): seal the old active, create the new one in tier 0 (used = now)
    val act2 = if (rotate) active + 1 else active
    val files1 = if (rotate)
        files.setBy((0, active), f => f.with("hasHint", true).with("hint", hintOf(index, active)))
             .put((0, act2), emptyFile(now))
      else files
    val packs1 = if (rotate) packs.put(act2, { tier: 0, size: 0, live: 0, used: now, touched: now }) else packs
    val off = packs1.get(act2).size
    val rec = { key: k, vlen: vlen, off: off, ver: nextVer }
    val packs2 = if (index.keys().contains(k))
        packs1.setBy(index.get(k).pack, p => p.with("live", p.live - index.get(k).len))
      else packs1
    val packs3 = packs2.setBy(act2, p => p.with("size", p.size + rl).with("live", p.live + vlen))
    val tot = totals.replaceAt(0, totals[0] + rl)
    all {
      files' = files1.setBy((0, act2), f => f.with("recs", f.recs.append(rec))),
      index' = index.put(k, { pack: act2, off: valueOff(off), len: vlen }),
      packs' = packs3,
      active' = act2,
      totals' = tot,
      demoting' = (demoting or (BUDGET[0] > 0 and tot[0] > BUDGET[0])),
      kickedSinceStart' = (kickedSinceStart or (BUDGET[0] > 0 and tot[0] > BUDGET[0])),
      copying' = copying,
      now' = now,
      nextVer' = nextVer + 1,
      maxId' = if (act2 > maxId) act2 else maxId,
      history' = history.setBy(k, vs => vs.union(Set(nextVer))),
      powerLost' = powerLost,
    }
  }

  // Get (also HAS): mark the pack used; move its mtime at most every TOUCH_EVERY
  action doGet(k: str): bool = all {
    index.keys().contains(k),
    getEffect(k),
    index' = index, active' = active, totals' = totals, demoting' = demoting, copying' = copying,
    now' = now, nextVer' = nextVer, maxId' = maxId, history' = history, powerLost' = powerLost, kickedSinceStart' = kickedSinceStart,
  }

  action getEffect(k: str): bool = {
    val id = index.get(k).pack
    val p = packs.get(id)
    val touch = now - p.touched > TOUCH_EVERY
    all {
      packs' = packs.set(id, p.with("used", now).with("touched", if (touch) now else p.touched)),
      files' = if (touch) files.setBy((p.tier, id), f => f.with("mtime", now)) else files,
    }
  }

  action tick = all {
    now' = now + 1,
    files' = files, index' = index, packs' = packs, active' = active, totals' = totals,
    demoting' = demoting, copying' = copying, nextVer' = nextVer, maxId' = maxId,
    history' = history, powerLost' = powerLost, kickedSinceStart' = kickedSinceStart,
  }

  // One iteration of the demoter loop: pick the victim; from the last tier delete it (with
  // its index entries), else start copying it to the next tier. No victim: the loop ends.
  action demoteStep: bool = all {
    demoting,
    copying == -1,
    demoteEffect(victim(packs, active, totals)),
    active' = active, now' = now, nextVer' = nextVer, maxId' = maxId, history' = history,
    powerLost' = powerLost, kickedSinceStart' = kickedSinceStart,
  }

  action demoteEffect(v: int): bool = {
    if (v == -1) all {
      demoting' = false,
      files' = files, index' = index, packs' = packs, totals' = totals, copying' = copying,
    } else {
      val p = packs.get(v)
      if (p.tier == NTIERS - 1) all {
        index' = index.keys().filter(k => index.get(k).pack != v).mapBy(k => index.get(k)),
        files' = mapWithout(files, (p.tier, v)),
        packs' = mapWithout(packs, v),
        totals' = totals.replaceAt(p.tier, totals[p.tier] - p.size),
        demoting' = demoting, copying' = copying,
      } else all {
        // link (or copy + rename) .pack and .hint into the next tier; mtime := used.
        // moveFile treats an existing destination as success and keeps it; startup deletes
        // later-tier duplicates and pack ids are never reused, so here it would hold the same
        // pack and overwriting is equivalent (see jigd-store.md).
        files' = files.put((p.tier + 1, v), files.get((p.tier, v)).with("mtime", p.used)),
        copying' = v,
        index' = index, packs' = packs, totals' = totals, demoting' = demoting,
      }
    }
  }

  // moveDown's locked switch: the pack now lives in the next tier; the old files go
  action demoteCommit: bool = all {
    copying != -1,
    commitEffect(packs.get(copying)),
    copying' = -1,
    index' = index, active' = active, demoting' = demoting, now' = now, nextVer' = nextVer,
    maxId' = maxId, history' = history, powerLost' = powerLost, kickedSinceStart' = kickedSinceStart,
  }

  action commitEffect(p: Pack): bool = {
    all {
      packs' = packs.set(copying, p.with("tier", p.tier + 1)),
      totals' = totals.replaceAt(p.tier, totals[p.tier] - p.size).replaceAt(p.tier + 1, totals[p.tier + 1] + p.size),
      files' = mapWithout(files, (p.tier, copying)),
    }
  }

  // The daemon dies (kill -9) or the machine loses power, then starts again. On power loss the
  // active (never fsynced) pack may lose a suffix of records and keep a torn partial one. In the
  // Go daemon (FSYNC_ON_SEAL = false) the newest sealed pack may also lose a suffix while its
  // hint survives; with the adopted fsync-on-seal, sealed packs and hints are durable.
  action crashAndRestart: bool = {
    nondet power = Set(false, true).oneOf()
    nondet keepActive = range(0, files.get((0, active)).recs.length() + 1).foldl(Set(), (s, i) => s.union(Set(i))).oneOf()
    nondet torn = Set(0, 3).oneOf()
    nondet hitSealed = Set(false, true).oneOf()
    nondet keepSealed = range(0, 4).foldl(Set(), (s, i) => s.union(Set(i))).oneOf()
    val af = files.get((0, active))
    val files1 = if (power) files.set((0, active),
        af.with("recs", af.recs.slice(0, keepActive)).with("tail", torn)) else files.set((0, active), af.with("tail", torn))
    val prev = active - 1
    val files2 = if (power and hitSealed and not(FSYNC_ON_SEAL) and files1.keys().contains((0, prev))) {
        val pf = files1.get((0, prev))
        val n = if (keepSealed < pf.recs.length()) keepSealed else pf.recs.length()
        files1.set((0, prev), pf.with("recs", pf.recs.slice(0, n)))
      } else files1
    val s = dropEmpty(openStore(files2, maxId))
    val act = s.last + 1
    all {
      files' = s.files.put((0, act), emptyFile(now)),
      index' = s.index,
      packs' = s.packs.put(act, { tier: 0, size: 0, live: 0, used: now, touched: now }),
      active' = act,
      totals' = s.totals,
      demoting' = KICK_AT_START,     // Go: nobody kicks the demoter at startup
      copying' = -1,
      now' = now,
      nextVer' = nextVer,
      maxId' = if (act > maxId) act else maxId,
      history' = history,
      powerLost' = (powerLost or power),
      kickedSinceStart' = KICK_AT_START,
    }
  }

  // SIGTERM: Close waits for the demoter, seals the active pack; the next start reads hints.
  action cleanRestart: bool = all {
    not(demoting),
    copying == -1,
    cleanEffect,
    demoting' = KICK_AT_START, copying' = -1, now' = now, nextVer' = nextVer, history' = history,
    powerLost' = powerLost, kickedSinceStart' = KICK_AT_START,
  }

  action cleanEffect: bool = {
    val files1 = files.setBy((0, active), f => f.with("hasHint", true).with("hint", hintOf(index, active)))
    val s = dropEmpty(openStore(files1, maxId))
    val act = s.last + 1
    all {
      files' = s.files.put((0, act), emptyFile(now)),
      index' = s.index,
      packs' = s.packs.put(act, { tier: 0, size: 0, live: 0, used: now, touched: now }),
      active' = act,
      totals' = s.totals,
      maxId' = if (act > maxId) act else maxId,
    }
  }

  action step = {
    nondet k = KEYS.oneOf()
    nondet v = VLENS.oneOf()
    any {
      doPut(k, v),
      doGet(k),
      tick,
      demoteStep,
      demoteCommit,
      crashAndRestart,
      cleanRestart,
    }
  }

  // ---------------------------------------------------------------------------------------
  // Invariants
  // ---------------------------------------------------------------------------------------
  pure def fileOf(fs: (int, int) -> File, ps: int -> Pack, id: int): File = fs.get((ps.get(id).tier, id))

  // every index entry points at a complete record of that key in a pack that exists, and that
  // record's value was acknowledged for that key: the store never serves garbage or another
  // key's bytes
  val indexSound = index.keys().forall(k => {
    val l = index.get(k)
    and {
      packs.keys().contains(l.pack),
      files.keys().contains((packs.get(l.pack).tier, l.pack)),
      fileOf(files, packs, l.pack).recs.foldl(false, (found, r) =>
        found or (r.key == k and valueOff(r.off) == l.off and r.vlen == l.len and history.get(k).contains(r.ver))),
    }
  })

  // live = value bytes the index still points at, per pack
  val liveAccounting = packs.keys().forall(id =>
    packs.get(id).live == index.keys().fold(0, (acc, k) => if (index.get(k).pack == id) acc + index.get(k).len else acc))

  // tier totals are the sizes of the packs the daemon counts in that tier
  val tierTotals = range(0, NTIERS).foldl(true, (ok, t) =>
    ok and totals[t] == packs.keys().fold(0, (acc, id) => if (packs.get(id).tier == t) acc + packs.get(id).size else acc))

  // the size the daemon tracks is the file's size
  val sizesMatchFiles = packs.keys().forall(id =>
    files.keys().contains((packs.get(id).tier, id)) and packs.get(id).size == fileSize(fileOf(files, packs, id)))

  // the active pack: tier 0, newest id, never has a hint, not demoted
  val activeOk = and {
    packs.keys().contains(active),
    packs.get(active).tier == 0,
    packs.keys().forall(id => id <= active),
    not(files.get((0, active)).hasHint),
    copying != active,
  }

  // sealed (non-active) packs the daemon knows all have hints
  val sealedHaveHints = packs.keys().forall(id => id != active implies fileOf(files, packs, id).hasHint)

  // a pack id is on disk in two tiers only while it is being copied
  val duplicatesOnlyWhileCopying = files.keys().forall(a => files.keys().forall(b =>
    (a._2 == b._2 and a._1 != b._1) implies copying == a._2))

  // newest wins: no known pack newer than the indexed one holds a complete record of the key.
  // After a power loss this can break (a sealed pack's hint points past its truncated end while
  // an earlier record of the same key in that pack survived: the key falls back to an older
  // pack), which is still sound (indexSound) but not "latest".
  val newestWins = not(powerLost) implies index.keys().forall(k =>
    packs.keys().forall(id => id > index.get(k).pack implies
      fileOf(files, packs, id).recs.foldl(true, (ok, r) => ok and r.key != k)))

  // with the demoter idle, every bounded tier is within budget unless only the active pack is
  // left in it -- once the demoter has been kicked since startup. With KICK_AT_START (adopted
  // Q-STORE-5) that is always true after a restart. In the Go daemon (false) OpenStore does not
  // kick, so a store that starts over budget stays over until the first Put that finds tier 0
  // over budget, and `--invariant=idleWithinBudgetEvenAfterStart` fails, showing exactly that.
  val idleWithinBudget = (not(demoting) and copying == -1 and kickedSinceStart) implies
    range(0, NTIERS).foldl(true, (ok, t) =>
      ok and (BUDGET[t] <= 0 or totals[t] <= BUDGET[t] or victimIn(packs, active, t) == -1))
  val idleWithinBudgetEvenAfterStart = (not(demoting) and copying == -1) implies range(0, NTIERS).foldl(true, (ok, t) =>
    ok and (BUDGET[t] <= 0 or totals[t] <= BUDGET[t] or victimIn(packs, active, t) == -1))

  // Q-STORE-2 (adopted): no pack other than the active one is empty
  val noEmptySealedPacks = DROP_EMPTY_PACKS implies packs.keys().forall(id => id == active or packs.get(id).size > 0)

  val safety = and {
    indexSound, liveAccounting, tierTotals, sizesMatchFiles, activeOk, sealedHaveHints,
    duplicatesOnlyWhileCopying, newestWins, idleWithinBudget, noEmptySealedPacks,
  }

  // Witnesses (expected to FAIL when used as --invariant, proving the situation is reachable)
  val witnessEviction = maxId < 4 or packs.keys().size() >= maxId - 1
  val witnessDemoted = packs.keys().forall(id => packs.get(id).tier == 0)
  val witnessPowerLossFallback = not(powerLost and index.keys().size() > 0 and not(newestWinsRaw))
  val newestWinsRaw = index.keys().forall(k =>
    packs.keys().forall(id => id > index.get(k).pack implies
      fileOf(files, packs, id).recs.foldl(true, (ok, r) => ok and r.key != k)))

  // ---------------------------------------------------------------------------------------
  // Scenario tests
  // ---------------------------------------------------------------------------------------
  // a 9-byte value makes an 18-byte record: the second one does not fit in PACK_LIMIT=24
  run rotateTest =
    init
      .then(doPut("a", 9))
      .then(doPut("b", 9))
      .then(all { assert(active == 2), assert(files.get((0, 1)).hasHint), assert(safety), tick })

  // superseding moves live bytes; a mostly superseded pack becomes stale and goes first
  run supersedeTest =
    init
      .then(doPut("a", 9))
      .then(doPut("a", 9))
      .then(all {
        assert(packs.get(1).live == 0),
        assert(packs.get(2).live == 9),
        assert(stale(packs.get(1))),
        assert(safety),
        tick,
      })

  // a crash leaves the active pack unsealed; it is scanned, its torn tail truncated, and the
  // data comes back
  run crashRecoveryTest =
    init
      .then(doPut("a", 2))
      .then(doPut("b", 2))
      .then(crashAndRestart)
      .then(all { assert(safety), tick })

  // a single record bigger than PACK_LIMIT still goes into an (empty) pack of its own
  run oversizedRecordTest =
    init
      .then(doPut("a", 2))
      .then(doPut("b", 20))
      .then(all { assert(active == 2), assert(packs.get(2).size == 29), doPut("c", 2) })
      .then(all { assert(active == 3), assert(safety), tick })
}

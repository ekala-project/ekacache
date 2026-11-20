// -*- mode: Bluespec; -*-
/**
 * Client side of builder-rpc-v0 (`ec nix-store`, jig/src/nix_store_mode.cc): the subset of the
 * Nix worker protocol 1.38 a sandboxed builder speaks over NIX_REMOTE=unix://<path>.
 * Byte layouts: behaviors/ec-nix-store.md (this module's state names follow its
 * "Client state-transition table").
 *
 * The daemon is an arbitrary environment: at every point where the client reads, it may get
 * the expected thing, something wrong, or EOF. Checked:
 *  - grammar: what the client writes is always a prefix of
 *        MAGIC_1 VERSION FEATURES 0 0 ( OP7 )*          (add-text, add-drv, fetchurls)
 *     or MAGIC_1 VERSION FEATURES 0 0 OP1000           (submit)
 *    i.e. no op before the handshake (including its stderr drain) completed, no op while a
 *    previous op's stderr or reply is unread, nothing after a failure;
 *  - exit status: 0 only from Done, which requires every op to have completed; a daemon
 *    STDERR_ERROR always yields exit 1 with "nix-daemon: <msg>" printed;
 *  - stdout is written only in Done (a fetchurls batch that fails part-way prints nothing,
 *    although earlier AddToStore calls took effect: quirk Q-NIX-17).
 *
 * Run:
 *   quint run --invariant=safety --max-steps=40 --max-samples=20000 spec/builder_rpc.qnt
 *   quint test spec/builder_rpc.qnt
 */
module builder_rpc {
  type Verb = | AddText | AddDrv | Fetchurls | Submit | BadVerb
  type Out = | WMagic1 | WVersion | WFeatures | WZero | WOp7 | WOp1000
  type St =
    | Start | SendHello | AwaitMagic | AwaitVersion | SendFeatures | AwaitFeatures | SendPost
    | AwaitInfo | DrainStartup | Ready | SendAdd | DrainAdd | AwaitPathInfo | SendSubmit
    | DrainSubmit | AwaitSubmitResult | Done | Exited
  // what the daemon puts on the wire next, abstractly
  type In = | Good | Wrong | Eof
  // stderr messages inside a drain
  type Msg = | Last | Next | Activity | Error | Unknown | MsgEof

  var st: St
  var verb: Verb
  var fetchLeft: int          // fetchurls elements still to add
  var written: List[Out]
  var exitCode: int           // -1 while running
  var stdoutWritten: bool
  var printedDaemonError: bool
  var opsCompleted: int
  var drainReturn: St         // where a successful drain continues

  action init = {
    nondet v = Set(AddText, AddDrv, Fetchurls, Submit, BadVerb).oneOf()
    nondet n = Set(0, 1, 2).oneOf()
    all {
      st' = Start, verb' = v, fetchLeft' = if (v == Fetchurls) n else 1,
      written' = List(), exitCode' = -1, stdoutWritten' = false, printedDaemonError' = false,
      opsCompleted' = 0, drainReturn' = Start,
    }
  }

  action quit(code: int): bool = all {
    st' = Exited, exitCode' = code,
    verb' = verb, fetchLeft' = fetchLeft, written' = written, stdoutWritten' = stdoutWritten,
    printedDaemonError' = printedDaemonError, opsCompleted' = opsCompleted, drainReturn' = drainReturn,
  }

  action goto(s: St, w: List[Out]): bool = all {
    st' = s, written' = written.concat(w),
    verb' = verb, fetchLeft' = fetchLeft, exitCode' = exitCode, stdoutWritten' = stdoutWritten,
    printedDaemonError' = printedDaemonError, opsCompleted' = opsCompleted, drainReturn' = drainReturn,
  }

  action drainThen(ret: St): bool = all {
    st' = match ret { | Ready => DrainStartup | AwaitPathInfo => DrainAdd | _ => DrainSubmit },
    drainReturn' = ret,
    verb' = verb, fetchLeft' = fetchLeft, written' = written, exitCode' = exitCode,
    stdoutWritten' = stdoutWritten, printedDaemonError' = printedDaemonError, opsCompleted' = opsCompleted,
  }

  // write an op and start draining its stderr; a successful drain continues at `ret`
  action sendOp(op: Out, ret: St): bool = all {
    st' = if (ret == AwaitPathInfo) DrainAdd else DrainSubmit,
    written' = written.append(op),
    drainReturn' = ret,
    verb' = verb, fetchLeft' = fetchLeft, exitCode' = exitCode, stdoutWritten' = stdoutWritten,
    printedDaemonError' = printedDaemonError, opsCompleted' = opsCompleted,
  }

  // the first op after Ready / after a completed AddToStore
  action nextOp: bool =
    if (verb == Submit) goto(SendSubmit, List())
    else if (fetchLeft > 0) goto(SendAdd, List())
    else goto(Done, List())

  action stepClient(i: In, m: Msg, writeOk: bool): bool =
    match st {
      | Start => goto(SendHello, List())       // argv non-empty; online mode (offline is not a protocol)
      | SendHello => if (writeOk) goto(AwaitMagic, [WMagic1, WVersion]) else quit(1)       // "bad daemon magic"
      | AwaitMagic => if (i == Good) goto(AwaitVersion, List()) else quit(1)               // "bad daemon magic"
      | AwaitVersion => if (i == Good) goto(SendFeatures, List()) else quit(1)             // "daemon protocol N too old"
      | SendFeatures => if (writeOk) goto(AwaitFeatures, [WFeatures]) else quit(1)
      | AwaitFeatures => if (i == Eof) quit(1) else goto(SendPost, List())                // list discarded, never checked
      | SendPost => if (writeOk) goto(AwaitInfo, [WZero, WZero]) else quit(1)
      | AwaitInfo => if (i == Eof) quit(1) else drainThen(Ready)                          // version string + trust word, discarded
      | DrainStartup => drain(m)
      | DrainAdd => drain(m)
      | DrainSubmit => drain(m)
      | Ready => if (verb == BadVerb) quit(2) else nextOp                                  // quirk Q-NIX-2: args checked after the handshake
      | SendAdd => if (writeOk) sendOp(WOp7, AwaitPathInfo) else quit(1)
      | AwaitPathInfo => if (i == Eof) quit(1) else completeAdd                            // 9 fields; path printed later
      | SendSubmit => if (writeOk) sendOp(WOp1000, AwaitSubmitResult) else quit(1)
      | AwaitSubmitResult => if (i == Eof) quit(1) else all {
          st' = Done, opsCompleted' = opsCompleted + 1,
          verb' = verb, fetchLeft' = fetchLeft, written' = written, exitCode' = exitCode,
          stdoutWritten' = stdoutWritten, printedDaemonError' = printedDaemonError, drainReturn' = drainReturn,
        }
      | Done => all {
          st' = Exited, exitCode' = 0, stdoutWritten' = true,
          verb' = verb, fetchLeft' = fetchLeft, written' = written,
          printedDaemonError' = printedDaemonError, opsCompleted' = opsCompleted, drainReturn' = drainReturn,
        }
      | Exited => stutter
    }

  action stutter: bool = all {
    st' = st, verb' = verb, fetchLeft' = fetchLeft, written' = written, exitCode' = exitCode,
    stdoutWritten' = stdoutWritten, printedDaemonError' = printedDaemonError,
    opsCompleted' = opsCompleted, drainReturn' = drainReturn,
  }

  action completeAdd: bool = all {
    st' = if (verb == Fetchurls and fetchLeft - 1 > 0) SendAdd else Done,
    fetchLeft' = fetchLeft - 1,
    opsCompleted' = opsCompleted + 1,
    verb' = verb, written' = written, exitCode' = exitCode, stdoutWritten' = stdoutWritten,
    printedDaemonError' = printedDaemonError, drainReturn' = drainReturn,
  }

  // one stderr message of the drain loop
  action drain(m: Msg): bool =
    match m {
      | Last => all {
          st' = match drainReturn { | AwaitPathInfo => AwaitPathInfo | AwaitSubmitResult => AwaitSubmitResult | _ => Ready },
          verb' = verb, fetchLeft' = fetchLeft, written' = written, exitCode' = exitCode,
          stdoutWritten' = stdoutWritten, printedDaemonError' = printedDaemonError,
          opsCompleted' = opsCompleted, drainReturn' = drainReturn,
        }
      | Next => stutter                        // text printed to stderr, keep draining
      | Activity => stutter                    // START/STOP_ACTIVITY, RESULT: ignored
      | Error => all {                         // "nix-daemon: <msg>"
          st' = Exited, exitCode' = 1, printedDaemonError' = true,
          verb' = verb, fetchLeft' = fetchLeft, written' = written, stdoutWritten' = stdoutWritten,
          opsCompleted' = opsCompleted, drainReturn' = drainReturn,
        }
      | Unknown => quit(1)                     // "unexpected daemon message 0x…"
      | MsgEof => quit(1)                      // silent
    }

  action step = {
    nondet i = Set(Good, Wrong, Eof).oneOf()
    nondet m = Set(Last, Next, Activity, Error, Unknown, MsgEof).oneOf()
    nondet w = Set(true, false).oneOf()
    stepClient(i, m, w)
  }

  // ---------------------------------------------------------------------------------------
  pure val HANDSHAKE = [WMagic1, WVersion, WFeatures, WZero, WZero]

  pure def isPrefix(a: List[Out], b: List[Out]): bool =
    a.length() <= b.length() and range(0, a.length()).foldl(true, (ok, i) => ok and a[i] == b[i])

  // the client's output follows the grammar
  val grammar = {
    val n = written.length()
    if (n <= 5) isPrefix(written, HANDSHAKE)
    else and {
      isPrefix(HANDSHAKE, written),
      range(5, n).foldl(true, (ok, i) => ok and (if (verb == Submit) written[i] == WOp1000 and n == 6 else written[i] == WOp7)),
    }
  }
  // an op goes out only after the handshake drain returned STDERR_LAST
  val opsAfterHandshake = written.length() > 5 implies (st != DrainStartup)
  // at most one op is in flight: ops written = ops completed, or one more while it is pending
  val oneOpInFlight = {
    val ops = written.length() - 5
    ops <= 0 or ops == opsCompleted or (ops == opsCompleted + 1 and Set(DrainAdd, AwaitPathInfo, DrainSubmit, AwaitSubmitResult, Exited).contains(st))
  }
  // success means every requested op completed; stdout only on success
  val exitZeroMeansComplete = exitCode == 0 implies and {
    verb != BadVerb,
    if (verb == Submit) opsCompleted == 1 else fetchLeft == 0,
    stdoutWritten,
  }
  val stdoutOnlyOnSuccess = stdoutWritten implies exitCode == 0
  val daemonErrorFails = printedDaemonError implies exitCode == 1
  val badArgsExit2 = (verb == BadVerb and st == Exited) implies exitCode != 0

  val safety = and {
    grammar, opsAfterHandshake, oneOpInFlight, exitZeroMeansComplete, stdoutOnlyOnSuccess,
    daemonErrorFails, badArgsExit2,
  }

  val witnessFetchSuccess = not(verb == Fetchurls and exitCode == 0 and opsCompleted == 2)
  val witnessSubmitSuccess = not(verb == Submit and exitCode == 0)

  // ----- scenarios -----
  action begin(v: Verb, n: int): bool = all {
    st' = Start, verb' = v, fetchLeft' = n, written' = List(), exitCode' = -1,
    stdoutWritten' = false, printedDaemonError' = false, opsCompleted' = 0, drainReturn' = Start,
  }
  action ok = stepClient(Good, Last, true)
  action handshake = ok.then(ok).then(ok).then(ok).then(ok).then(ok).then(ok).then(ok)   // Start .. AwaitInfo
                       .then(stepClient(Good, Next, true)).then(ok)                   // drain with a NEXT, then LAST

  run fetchTwoTest =
    begin(Fetchurls, 2).then(handshake)
      .then(all { assert(st == Ready), ok })                   // -> SendAdd
      .then(ok).then(stepClient(Good, Activity, true)).then(ok) // op 7, drain
      .then(all { assert(st == AwaitPathInfo), ok })
      .then(all { assert(st == SendAdd and opsCompleted == 1), ok }).then(ok)
      .then(ok).then(all { assert(st == Done), ok })
      .then(all { assert(exitCode == 0 and written.length() == 7 and safety), stutter })

  run submitTest =
    begin(Submit, 1).then(handshake).then(ok).then(ok).then(ok).then(ok).then(ok)
      .then(all { assert(exitCode == 0 and opsCompleted == 1 and written[5] == WOp1000), stutter })

  run daemonErrorTest =
    begin(AddText, 1).then(handshake).then(ok).then(ok).then(stepClient(Good, Error, true))
      .then(all { assert(exitCode == 1 and printedDaemonError and not(stdoutWritten)), stutter })

  // Q-NIX-2: a bad verb is noticed only after the full handshake
  run badVerbAfterHandshakeTest =
    begin(BadVerb, 1).then(handshake).then(ok)
      .then(all { assert(exitCode == 2 and written == HANDSHAKE), stutter })

  run oldDaemonTest =
    begin(AddDrv, 1).then(ok).then(ok).then(ok).then(stepClient(Wrong, Last, true))
      .then(all { assert(exitCode == 1 and written == [WMagic1, WVersion]), stutter })
}

//! `jig gocacheprog`: the Go toolchain's `GOCACHEPROG` protocol on stdin/stdout, backed by jigd
//! (`behaviors/jig-gocacheprog.md`, `spec/gocache.qnt`,
//! `implementation/08-jig-gocache-and-cache-cmds.md §Part 2`). A port of `gocache_mode.cc`.
//!
//! - **§Startup**: connect (a failure is silent: the session is then an always-miss cache that
//!   still writes DiskPath files), pick the DiskPath directory, create it ignoring errors, send
//!   the handshake.
//! - **§Request loop**: stdin split on `\n` only (a `\r` stays), read in 1 MiB chunks, a final
//!   unterminated line still counts. Empty lines are skipped: Go writes one after every request.
//! - **§get**, **§put**, **§Key and value layout**, **§DiskPath files**: as legacy. The body PUT
//!   goes before the pointer PUT (`gocache.qnt` `pointersAnnounced`, `hitIsConsistent`), and a
//!   torn body is refused before anything is written or stored (`noTornBodies`).
//! - **ADOPTED §Adopted: JSON handling** (Q-GO-1, Q-GO-3, Q-GO-4): each line is parsed by
//!   serde_json and each response is produced by it; base64 is decoded strictly. A bad line
//!   whose integer `ID` can be recovered gets an `Err` reply; any other bad line is fatal
//!   (`jig gocacheprog: bad request line` on stderr, exit 1). Invalid base64 gets an `Err` reply
//!   and nothing is stored.
//! - **ADOPTED Q-CLIENT-2** (`behaviors/jig-cache-client.md`): a PUT whose reply is not `OK`
//!   prints `jig: cache put <key> failed` on stderr, and the client drops the connection, so the
//!   rest of the session runs as if jigd were absent.
//!
//! Kept quirks: Q-GO-2 (no reply to an unknown or missing `Command`), Q-GO-5 (DiskPath errors
//! ignored; the Rust port does not abort where legacy's throwing `exists` would), Q-GO-6 (a
//! set-but-empty `GOCACHEPROG_DIR`/`TMPDIR` is used as it is), Q-GO-7 (`ObjectID` is not read),
//! Q-GO-8 (`compiled` is logged for every put, live or not).
//!
//! Logging (`behaviors/jig-dispatch.md §Logging`): `go\tcached\t<action>\t<ms>` on a get hit,
//! `go\tcompiled\t<action>\t<ms>` on a put; the clock starts with each request.

use std::io::{Read, Write};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ekacache_core::bytes::{concat, hex_encode, os_path};
use ekacache_core::fspath;
use ekacache_core::keys::slot;
use serde::{Deserialize, Serialize};

use crate::Ctx;
use crate::client::Client;
use crate::log::{self, Outcome, Stopwatch};
use crate::process::ProcEnv;
use crate::util::write_file;

/// §Handshake: sent unprompted, before any input is read.
pub const HANDSHAKE: &[u8] = b"{\"ID\":0,\"KnownCommands\":[\"get\",\"put\",\"close\"]}\n";

/// The fatal message for a bad line without a recoverable `ID` (§Adopted: JSON handling).
pub const BAD_LINE: &[u8] = b"jig gocacheprog: bad request line\n";

/// One `read` of stdin fills at most this much (`LineReader::kBufSize`).
const READ_CHUNK: usize = 1 << 20;

/// `RunGoCacheProg`: serve stdin until EOF, a read error or `close` (exit 0), or a bad line
/// without a recoverable `ID` (exit 1). Arguments are ignored (dispatch does not pass them).
pub fn run(ctx: &mut Ctx) -> i32 {
    let mut client = Client::new();
    // a failed connect is silent: the client stays unconnected, every get misses (§Startup 1)
    let _ = client.connect(&ctx.socket);
    let dir = disk_path_dir(&ctx.env);
    // §Startup 3, Q-GO-5: errors ignored
    let _ = std::fs::create_dir_all(os_path(&dir));
    let mut out = std::io::stdout().lock();
    send(&mut out, HANDSHAKE);
    let mut session = Session {
        client,
        dir,
        env: &ctx.env,
    };
    session.serve(&mut LineReader::new(RawStdin), &mut out)
}

/// §Startup 2: `Env("GOCACHEPROG_DIR", Env("TMPDIR", "/tmp") + "/gocacheprog")`. `Env` falls
/// back only for an **unset** variable.
#[must_use]
pub fn disk_path_dir(env: &ProcEnv) -> Vec<u8> {
    let fallback = concat(&[env.get_or("TMPDIR", b"/tmp"), b"/gocacheprog"]);
    // Q-GO-6: set but empty is used as it is ("" → DiskPaths relative to the cwd, TMPDIR="" →
    // "/gocacheprog")
    env.get_or("GOCACHEPROG_DIR", &fallback).to_vec()
}

/// One `jig gocacheprog` process: the jigd connection, the DiskPath directory and the
/// environment the log reads.
pub struct Session<'a> {
    /// Connected iff the session is live (`GoSession::live`). Q-CLIENT-2 closes it after a
    /// refused PUT, which makes the rest of the session not live.
    pub client: Client,
    /// The DiskPath directory, not made absolute (§DiskPath files).
    pub dir: Vec<u8>,
    pub env: &'a ProcEnv,
}

impl Session<'_> {
    /// The request loop (§Request loop). The handshake has already been sent.
    pub fn serve<R: Read, W: Write>(&mut self, input: &mut LineReader<R>, out: &mut W) -> i32 {
        let mut line = Vec::new();
        while input.next(&mut line) {
            if line.is_empty() {
                continue;
            }
            let (id, req) = match parse_request(&line) {
                Parsed::Request(id, req) => (id, req),
                Parsed::Bad(id, msg) => {
                    send(out, &err_reply(id, &msg));
                    continue;
                }
                Parsed::Fatal => {
                    let _ = out.flush();
                    let _ = std::io::stderr().write_all(BAD_LINE);
                    return 1;
                }
            };
            let reply = match req.command.as_deref() {
                Some("close") => {
                    // later input is not read
                    send(out, &json_line(&CloseReply { id }));
                    return 0;
                }
                Some("get") => self.get(id, &req),
                Some("put") => self.put(input, id, &req),
                // Q-GO-2: an unknown or missing Command gets no reply
                _ => continue,
            };
            send(out, &reply);
        }
        0
    }

    /// §get (`HandleGet`): `go/a/<action>` → OutputID, `go/o/<hex OutputID>` → body.
    fn get(&mut self, id: i64, req: &Req) -> Vec<u8> {
        let clock = Stopwatch::start();
        let Some(action) = decode_b64(req.action_id.as_deref()) else {
            return err_reply(id, ERR_ACTION_ID);
        };
        let action = hex_encode(&action);
        // not live: the unconnected client misses without a request
        let output_id = self.client.get(&slot::go_action(&action));
        let body = output_id
            .as_ref()
            .and_then(|o| self.client.get(&slot::go_output(&hex_encode(o))));
        // either missing (a `go/a/` hit whose body was evicted, `gocache.qnt` getFinish) → Miss,
        // and no log line
        let (Some(output_id), Some(body)) = (output_id, body) else {
            return json_line(&MissReply { id, miss: true });
        };
        log::outcome(self.env, "go", Outcome::Hit, &action, &clock);
        let disk_path = fspath::join(&self.dir, &hex_encode(&output_id));
        // an existing file is not compared or rewritten. Q-GO-5: legacy's throwing
        // `fs::exists` aborts on an error other than "not found"; here any error counts as
        // "absent", which is what the non-error path does
        if std::fs::metadata(os_path(&disk_path)).is_err() {
            let _ = write_file(&disk_path, &body);
        }
        hit_reply(id, &output_id, body.len() as u64, &disk_path)
    }

    /// §put (`HandlePut`): the body line, the size check, the DiskPath file, then `go/o/` and
    /// `go/a/`, in that order.
    fn put<R: Read>(&mut self, input: &mut LineReader<R>, id: i64, req: &Req) -> Vec<u8> {
        let clock = Stopwatch::start();
        // BodySize absent, 0 or negative: no body line is consumed, the body is empty
        let body_size = req.body_size.filter(|&n| n > 0);
        // the body line belongs to this request whatever else is wrong with it, so it is read
        // before anything can be refused: an Err reply leaves the stream in step
        let body_line = body_size.map(|_| input.next_non_empty());
        let Some(action) = decode_b64(req.action_id.as_deref()) else {
            return err_reply(id, ERR_ACTION_ID);
        };
        let action = hex_encode(&action);
        // Q-GO-7: `ObjectID` (the pre-1.24 name) is not read; without `OutputID` the output id
        // is empty
        let Some(output_id) = decode_b64(req.output_id.as_deref()) else {
            return err_reply(id, ERR_OUTPUT_ID);
        };
        let mut body = Vec::new();
        if let (Some(size), Some(line)) = (body_size, body_line) {
            // EOF before a body line leaves the body empty, and the size check refuses it
            if let Some(line) = line {
                match decode_body(&line) {
                    Ok(b) => body = b,
                    Err(msg) => return err_reply(id, msg),
                }
            }
            // a killed go command leaves a torn stream; storing it would poison the action for
            // every later build (`gocache.qnt` noTornBodies). Nothing is written, stored or
            // logged
            if body.len() as u64 != size as u64 {
                let msg = format!("body: got {} of {} bytes", body.len(), size);
                return err_reply(id, &msg);
            }
        }
        let output_hex = hex_encode(&output_id);
        let disk_path = fspath::join(&self.dir, &output_hex);
        // unconditionally, overwriting; errors ignored (Q-GO-5)
        let _ = write_file(&disk_path, &body);
        if self.client.connected() {
            // the body first: a `go/a/` entry never names a body that was never sent
            let output_key = slot::go_output(&output_hex);
            if !self.client.put(&output_key, &body) {
                log::put_failed(&output_key); // ADOPTED Q-CLIENT-2
            }
            // a refused PUT closed the connection; a value that could not be framed was not
            // sent and left it open, and then legacy still sends the pointer
            let action_key = slot::go_action(&action);
            if self.client.connected() && !self.client.put(&action_key, &output_id) {
                log::put_failed(&action_key); // ADOPTED Q-CLIENT-2
            }
        }
        // Q-GO-8: logged as stored even when not live
        log::outcome(self.env, "go", Outcome::MissStored, &action, &clock);
        put_reply(id, &disk_path)
    }
}

/// `Err` text for an `ActionID` that is not strict base64 (ADOPTED Q-GO-4).
pub const ERR_ACTION_ID: &str = "ActionID: invalid base64";
/// `Err` text for an `OutputID` that is not strict base64 (ADOPTED Q-GO-4).
pub const ERR_OUTPUT_ID: &str = "OutputID: invalid base64";
/// `Err` text for a body line that is not a JSON string (ADOPTED Q-GO-1).
pub const ERR_BODY_JSON: &str = "body: not a JSON string";
/// `Err` text for a body that is not strict base64 (ADOPTED Q-GO-4).
pub const ERR_BODY_B64: &str = "body: invalid base64";
/// Prefix of the `Err` text for a request line that does not parse (ADOPTED Q-GO-1).
pub const ERR_BAD_REQUEST: &str = "bad request: ";

/// The fields of a request line (§Adopted: JSON handling). Names are matched exactly; unknown
/// fields are ignored; a missing or `null` field is `None`.
#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
pub struct Req {
    #[serde(rename = "ID")]
    pub id: Option<i64>,
    #[serde(rename = "Command")]
    pub command: Option<String>,
    #[serde(rename = "ActionID")]
    pub action_id: Option<String>,
    #[serde(rename = "OutputID")]
    pub output_id: Option<String>,
    #[serde(rename = "BodySize")]
    pub body_size: Option<i64>,
}

/// What a request line is.
#[derive(Debug, PartialEq, Eq)]
pub enum Parsed {
    /// A JSON object with an integer `ID` and correctly typed known fields.
    Request(i64, Req),
    /// Malformed, but its integer `ID` was recovered: reply `Err` with this text.
    Bad(i64, String),
    /// Malformed without a recoverable integer `ID`: the stream can no longer be trusted.
    Fatal,
}

/// ADOPTED Q-GO-1: parse one line as a JSON object. Not valid JSON, not an object, no integer
/// `ID`, a known field of the wrong type, or a known field given twice (serde's derive refuses
/// duplicates; legacy took the first, Go takes the last, so neither can be guessed) → bad. The
/// `ID` of a bad line is recovered by a second, lenient parse into a `serde_json::Value` (where
/// a duplicate's last occurrence wins).
#[must_use]
pub fn parse_request(line: &[u8]) -> Parsed {
    // serde's derived struct visitor also accepts a JSON array, positionally
    let object = line
        .iter()
        .find(|c| !matches!(c, b' ' | b'\t' | b'\r' | b'\n'))
        == Some(&b'{');
    let error = if object {
        match serde_json::from_slice::<Req>(line) {
            Ok(req) => match req.id {
                Some(id) => return Parsed::Request(id, req),
                None => "no integer ID".to_owned(),
            },
            Err(e) => e.to_string(),
        }
    } else {
        String::new()
    };
    match recover_id(line) {
        Some(id) => Parsed::Bad(id, format!("{ERR_BAD_REQUEST}{error}")),
        None => Parsed::Fatal,
    }
}

/// The integer `ID` of a JSON object line, if it has one that fits an `int64` (Go's type).
fn recover_id(line: &[u8]) -> Option<i64> {
    let value: serde_json::Value = serde_json::from_slice(line).ok()?;
    value.get("ID")?.as_i64()
}

/// ADOPTED Q-GO-4: strict standard base64 (RFC 4648 §4, padded, canonical trailing bits, no
/// whitespace). A missing field is the empty string, as legacy's `JsonField` gives.
fn decode_b64(text: Option<&str>) -> Option<Vec<u8>> {
    STANDARD.decode(text.unwrap_or("")).ok()
}

/// The body line of a put: a JSON string of strict base64.
fn decode_body(line: &[u8]) -> Result<Vec<u8>, &'static str> {
    let text: String = serde_json::from_slice(line).map_err(|_| ERR_BODY_JSON)?;
    STANDARD.decode(text).map_err(|_| ERR_BODY_B64)
}

#[derive(Serialize)]
struct MissReply {
    #[serde(rename = "ID")]
    id: i64,
    #[serde(rename = "Miss")]
    miss: bool,
}

#[derive(Serialize)]
struct HitReply<'a> {
    #[serde(rename = "ID")]
    id: i64,
    #[serde(rename = "OutputID")]
    output_id: &'a str,
    #[serde(rename = "Size")]
    size: u64,
    #[serde(rename = "DiskPath")]
    disk_path: &'a str,
}

#[derive(Serialize)]
struct PutReply<'a> {
    #[serde(rename = "ID")]
    id: i64,
    #[serde(rename = "DiskPath")]
    disk_path: &'a str,
}

#[derive(Serialize)]
struct ErrReply<'a> {
    #[serde(rename = "ID")]
    id: i64,
    #[serde(rename = "Err")]
    err: &'a str,
}

#[derive(Serialize)]
struct CloseReply {
    #[serde(rename = "ID")]
    id: i64,
}

/// A response line: compact JSON in the field order the spec lists, plus `\n`.
fn json_line<T: Serialize>(value: &T) -> Vec<u8> {
    // invariant: these structs hold only integers, booleans and `str`s, which always serialize
    let mut line = serde_json::to_vec(value).expect("a response always serializes");
    line.push(b'\n');
    line
}

/// `{"ID":<id>,"Err":"<msg>"}`.
fn err_reply(id: i64, msg: &str) -> Vec<u8> {
    json_line(&ErrReply { id, err: msg })
}

/// `{"ID":<id>,"OutputID":"<base64>","Size":<n>,"DiskPath":"<path>"}`.
fn hit_reply(id: i64, output_id: &[u8], size: u64, disk_path: &[u8]) -> Vec<u8> {
    let output_id = STANDARD.encode(output_id);
    if let Ok(disk_path) = std::str::from_utf8(disk_path) {
        json_line(&HitReply {
            id,
            output_id: &output_id,
            size,
            disk_path,
        })
    } else {
        let head = format!("{{\"ID\":{id},\"OutputID\":\"{output_id}\",\"Size\":{size}");
        raw_disk_path(head, disk_path)
    }
}

/// `{"ID":<id>,"DiskPath":"<path>"}`.
fn put_reply(id: i64, disk_path: &[u8]) -> Vec<u8> {
    match std::str::from_utf8(disk_path) {
        Ok(disk_path) => json_line(&PutReply { id, disk_path }),
        Err(_) => raw_disk_path(format!("{{\"ID\":{id}"), disk_path),
    }
}

/// Q-GO-3 residual: a DiskPath that is not UTF-8 (a non-UTF-8 `GOCACHEPROG_DIR`/`TMPDIR`)
/// cannot be a JSON string, so its bytes go out between quotes unescaped, exactly as legacy
/// writes every DiskPath. Behaviour for such a directory is unchanged (invalid JSON).
fn raw_disk_path(head: String, disk_path: &[u8]) -> Vec<u8> {
    let mut line = head.into_bytes();
    line.extend_from_slice(b",\"DiskPath\":\"");
    line.extend_from_slice(disk_path);
    line.extend_from_slice(b"\"}\n");
    line
}

/// One response, then flush (legacy flushes after every request). Write errors are ignored,
/// as legacy ignores `fflush`'s.
fn send<W: Write>(out: &mut W, line: &[u8]) {
    let _ = out.write_all(line);
    let _ = out.flush();
}

/// stdin by `read(2)`, unbuffered: `LineReader` does the buffering.
struct RawStdin;

impl Read for RawStdin {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        Ok(rustix::io::read(std::io::stdin(), buf)?)
    }
}

/// `LineReader` (`gocache_mode.cc:49-76`): lines split on `\n` only, in `READ_CHUNK` reads.
pub struct LineReader<R> {
    src: R,
    buf: Box<[u8]>,
    /// Unread bytes of the last read: `buf[pos..end]`.
    pos: usize,
    end: usize,
}

impl<R: Read> LineReader<R> {
    pub fn new(src: R) -> Self {
        Self::with_chunk(src, READ_CHUNK)
    }

    /// A reader whose reads fill at most `chunk` bytes (tests use small ones).
    pub fn with_chunk(src: R, chunk: usize) -> Self {
        Self {
            src,
            buf: vec![0u8; chunk.max(1)].into_boxed_slice(),
            pos: 0,
            end: 0,
        }
    }

    /// `Next(line)`: the next line without its `\n` (a `\r` stays). `false` at EOF or on a read
    /// error (`EINTR` included, as legacy) unless an unterminated non-empty line was pending,
    /// which is returned first. EOF is not latched: the next call reads again, as legacy does.
    pub fn next(&mut self, line: &mut Vec<u8>) -> bool {
        line.clear();
        loop {
            let pending = &self.buf[self.pos..self.end];
            if let Some(i) = pending.iter().position(|&c| c == b'\n') {
                line.extend_from_slice(&pending[..i]);
                self.pos += i + 1;
                return true;
            }
            line.extend_from_slice(pending);
            self.pos = 0;
            self.end = 0;
            match self.src.read(&mut self.buf) {
                Ok(n) if n > 0 => self.end = n,
                _ => return !line.is_empty(),
            }
        }
    }

    /// Lines until a non-empty one (§put 2.1), or `None` at EOF.
    pub fn next_non_empty(&mut self) -> Option<Vec<u8>> {
        let mut line = Vec::new();
        while self.next(&mut line) {
            if !line.is_empty() {
                return Some(line);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ekacache_core::env::Env;

    /// `TestGoCache` (`legacy/jig/src/jig_test.cc:483`): standard base64, decoded strictly now
    /// (ADOPTED Q-GO-4), round-trips what Go encodes.
    #[test]
    fn test_go_cache() {
        for bytes in [&b""[..], b"a", b"ab", b"abc", b"abcd", b"\0\xff\x10"] {
            assert_eq!(
                decode_b64(Some(&STANDARD.encode(bytes))).as_deref(),
                Some(bytes)
            );
        }
        assert_eq!(STANDARD.encode(b"abc"), "YWJj");
        assert_eq!(STANDARD.encode(b"ab"), "YWI=");
        assert_eq!(STANDARD.encode(b"a"), "YQ==");
        assert_eq!(decode_b64(Some("YWI=")), Some(b"ab".to_vec()));
    }

    fn parse(line: &str) -> Parsed {
        parse_request(line.as_bytes())
    }

    fn req(id: i64, command: &str) -> Req {
        Req {
            id: Some(id),
            command: Some(command.to_owned()),
            ..Req::default()
        }
    }

    #[test]
    fn go_lines_parse() {
        assert_eq!(
            parse(r#"{"ID":5,"Command":"get","ActionID":"YWJj"}"#),
            Parsed::Request(
                5,
                Req {
                    action_id: Some("YWJj".into()),
                    ..req(5, "get")
                }
            )
        );
        assert_eq!(
            parse(r#"{"ID":2,"Command":"put","ActionID":"YQ==","OutputID":"YWI=","BodySize":3}"#),
            Parsed::Request(
                2,
                Req {
                    action_id: Some("YQ==".into()),
                    output_id: Some("YWI=".into()),
                    body_size: Some(3),
                    ..req(2, "put")
                }
            )
        );
        // unknown fields (Go ≤ 1.23's ObjectID among them) are ignored, whatever their type
        assert_eq!(
            parse(r#"{"ID":1,"Command":"close","ObjectID":"YQ==","X":[1,{"y":null}]}"#),
            Parsed::Request(1, req(1, "close"))
        );
        // RFC 8259 whitespace (a trailing \r included) and escapes, which legacy misread
        assert_eq!(
            parse("{ \"ID\" : 3 ,\t\"Command\": \"g\\u0065t\" }\r"),
            Parsed::Request(3, req(3, "get"))
        );
        // null is absent; a negative ID is an integer
        assert_eq!(
            parse(r#"{"ID":-3,"Command":null}"#),
            Parsed::Request(
                -3,
                Req {
                    id: Some(-3),
                    ..Req::default()
                }
            )
        );
    }

    #[test]
    fn bad_lines_with_an_id() {
        for line in [
            r#"{"ID":4,"Command":5}"#,
            r#"{"ID":4,"ActionID":["x"]}"#,
            r#"{"ID":4,"BodySize":"10"}"#,
            r#"{"ID":4,"BodySize":1.5}"#,
            r#"{"ID":1,"Command":"get","Command":"put","ID":4}"#,
        ] {
            match parse(line) {
                Parsed::Bad(4, msg) => assert!(msg.starts_with(ERR_BAD_REQUEST), "{line}: {msg}"),
                other => panic!("{line}: {other:?}"),
            }
        }
        let Parsed::Bad(_, msg) = parse(r#"{"ID":4,"Command":"x","Command":"y"}"#) else {
            panic!()
        };
        assert!(msg.contains("duplicate field `Command`"), "{msg}");
    }

    #[test]
    fn bad_lines_without_an_id() {
        for line in [
            "{\"ID\":1,\"Command\":\"get\"",
            "{\"ID\":1} x",
            "{\"Command\":\"get\"}",
            "{\"ID\":null}",
            "{\"ID\":\"7\"}",
            "{\"ID\":1.0}",
            "{\"ID\":1e3}",
            "{\"ID\":18446744073709551615}",
            "{\"id\":1}",
            "[1,\"get\"]",
            "\"YWJj\"",
            "5",
            " ",
            "\r",
            "garbage",
        ] {
            assert_eq!(parse(line), Parsed::Fatal, "{line:?}");
        }
        // invalid UTF-8 inside a string is not JSON, for the lenient parse either
        assert_eq!(
            parse_request(b"{\"ID\":1,\"Command\":\"\xff\"}"),
            Parsed::Fatal
        );
        // deep nesting overflows no stack: an unknown field is skipped without recursion, and
        // the lenient parse stops at serde_json's recursion limit
        let nest = |field: &str| {
            format!(
                "{{\"ID\":1,\"{field}\":{}{}}}",
                "[".repeat(100_000),
                "]".repeat(100_000)
            )
        };
        assert_eq!(
            parse(&nest("X")),
            Parsed::Request(
                1,
                Req {
                    id: Some(1),
                    ..Req::default()
                }
            )
        );
        assert_eq!(parse(&nest("Command")), Parsed::Fatal);
    }

    #[test]
    fn strict_base64() {
        assert_eq!(decode_b64(Some("YWJj")), Some(b"abc".to_vec()));
        assert_eq!(decode_b64(Some("YWI=")), Some(b"ab".to_vec()));
        assert_eq!(decode_b64(Some("YQ==")), Some(b"a".to_vec()));
        assert_eq!(decode_b64(Some("")), Some(Vec::new()));
        assert_eq!(decode_b64(None), Some(Vec::new()));
        // legacy decoded each of these leniently (Q-GO-4)
        for bad in [
            "YWI", "Y W I", "YW-I", "YWJ", "YW_I", "YWI=\n", "YQ=", "YR==", "====", "Y",
        ] {
            assert_eq!(decode_b64(Some(bad)), None, "{bad}");
        }
        assert_eq!(decode_body(b"\"YWJj\""), Ok(b"abc".to_vec()));
        assert_eq!(decode_body(b" \"YW\\u004aj\" \r"), Ok(b"abc".to_vec()));
        assert_eq!(decode_body(b"\"YWJj"), Err(ERR_BODY_JSON));
        assert_eq!(decode_body(b"YWJj"), Err(ERR_BODY_JSON));
        assert_eq!(decode_body(b"\"YWJj\"x"), Err(ERR_BODY_JSON));
        assert_eq!(decode_body(b"\"YW Jj\""), Err(ERR_BODY_B64));
    }

    #[test]
    fn replies() {
        assert_eq!(
            json_line(&MissReply { id: 7, miss: true }),
            b"{\"ID\":7,\"Miss\":true}\n"
        );
        assert_eq!(json_line(&CloseReply { id: -1 }), b"{\"ID\":-1}\n");
        assert_eq!(
            hit_reply(3, b"abc", 12, b"/tmp/gocacheprog/616263"),
            b"{\"ID\":3,\"OutputID\":\"YWJj\",\"Size\":12,\"DiskPath\":\"/tmp/gocacheprog/616263\"}\n"
        );
        assert_eq!(
            put_reply(9, "/tmp/é/ab".as_bytes()),
            "{\"ID\":9,\"DiskPath\":\"/tmp/é/ab\"}\n".as_bytes()
        );
        // ADOPTED Q-GO-3: escaped
        assert_eq!(
            put_reply(9, b"/t\"m\\p\n/ab"),
            b"{\"ID\":9,\"DiskPath\":\"/t\\\"m\\\\p\\n/ab\"}\n"
        );
        assert_eq!(
            err_reply(1, "body: got 3 of 5 bytes"),
            b"{\"ID\":1,\"Err\":\"body: got 3 of 5 bytes\"}\n"
        );
        // Q-GO-3 residual: raw bytes
        assert_eq!(
            put_reply(2, b"/t\xffp/ab"),
            b"{\"ID\":2,\"DiskPath\":\"/t\xffp/ab\"}\n"
        );
        assert_eq!(
            hit_reply(2, b"a", 1, b"/t\xff\"/61"),
            b"{\"ID\":2,\"OutputID\":\"YQ==\",\"Size\":1,\"DiskPath\":\"/t\xff\"/61\"}\n"
        );
    }

    #[test]
    fn line_reader() {
        let input: &[u8] = b"a\r\n\nbc\nlast";
        for chunk in [1, 2, 3, 1 << 20] {
            let mut r = LineReader::with_chunk(input, chunk);
            let mut l = Vec::new();
            let mut got = Vec::new();
            while r.next(&mut l) {
                got.push(l.clone());
            }
            assert_eq!(got, [&b"a\r"[..], b"", b"bc", b"last"], "chunk {chunk}");
            assert!(!r.next(&mut l));
            assert!(l.is_empty());
        }
        let mut r = LineReader::with_chunk(&b"\n\n\nx\n\n"[..], 2);
        assert_eq!(r.next_non_empty().as_deref(), Some(&b"x"[..]));
        assert_eq!(r.next_non_empty(), None);
    }

    #[test]
    fn dir_selection() {
        let dir = |pairs: &[(&str, &str)]| disk_path_dir(&ProcEnv::new(Env::from_pairs(pairs)));
        assert_eq!(dir(&[]), b"/tmp/gocacheprog");
        assert_eq!(dir(&[("TMPDIR", "/x")]), b"/x/gocacheprog");
        assert_eq!(dir(&[("TMPDIR", "")]), b"/gocacheprog");
        assert_eq!(dir(&[("TMPDIR", "/x"), ("GOCACHEPROG_DIR", "")]), b"");
        assert_eq!(dir(&[("GOCACHEPROG_DIR", "rel/d")]), b"rel/d");
    }

    /// A session without jigd over in-memory stdin/stdout.
    fn offline(dir: &[u8], input: &[u8], env: &ProcEnv) -> (i32, Vec<u8>) {
        let mut s = Session {
            client: Client::new(),
            dir: dir.to_vec(),
            env,
        };
        let mut out = Vec::new();
        let code = s.serve(&mut LineReader::with_chunk(input, 7), &mut out);
        (code, out)
    }

    #[test]
    fn offline_session() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().as_os_str().as_encoded_bytes();
        let log = tmp.path().join("log");
        let env = ProcEnv::new(Env::from_pairs(&[("JIG_LOG", log.to_str().unwrap())]));
        let input = concat(&[
            b"{\"ID\":1,\"Command\":\"get\",\"ActionID\":\"YWJj\"}\n\n",
            b"{\"ID\":2,\"Command\":\"put\",\"ActionID\":\"YWJj\",\"OutputID\":\"YWI=\",\"BodySize\":3}\n\n",
            b"\n\"eHl6\"\n",
            b"{\"ID\":3,\"Command\":\"put\",\"ActionID\":\"YWJj\",\"OutputID\":\"YQ==\",\"BodySize\":5}\n\n",
            b"\"eHl6\"\n",
            b"{\"ID\":4,\"Command\":\"stat\"}\n",
            b"{\"ID\":5}\n",
            b"{\"ID\":6,\"Command\":\"close\"}\n",
            b"{\"ID\":7,\"Command\":\"get\"}\n",
        ]);
        let (code, out) = offline(d, &input, &env);
        assert_eq!(code, 0);
        let dp = String::from_utf8(d.to_vec()).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            format!(
                "{{\"ID\":1,\"Miss\":true}}\n{{\"ID\":2,\"DiskPath\":\"{dp}/6162\"}}\n\
                 {{\"ID\":3,\"Err\":\"body: got 3 of 5 bytes\"}}\n{{\"ID\":6}}\n"
            )
        );
        assert_eq!(std::fs::read(tmp.path().join("6162")).unwrap(), b"xyz");
        assert!(!tmp.path().join("61").exists());
        let log = std::fs::read_to_string(&log).unwrap();
        let rows: Vec<Vec<&str>> = log.lines().map(|l| l.split('\t').collect()).collect();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][..3], ["go", "compiled", "616263"]);
    }

    #[test]
    fn offline_bad_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().as_os_str().as_encoded_bytes();
        let env = ProcEnv::new(Env::from_pairs::<&str, &str>(&[]));
        // a recoverable ID keeps the session going; an unrecoverable one ends it with 1
        let (code, out) = offline(
            d,
            b"{\"ID\":1,\"Command\":7}\n{\"ID\":2,\"Command\":\"get\",\"ActionID\":\"YWI\"}\n\
              {\"ID\":3,\"Command\":\"put\",\"OutputID\":\"!\",\"BodySize\":1}\n\"YQ==\"\n\
              {\"ID\":4,\"Command\":\"put\",\"BodySize\":1}\n\"YQ=\"\n\
              {\"ID\":5,\"Command\":\"put\",\"BodySize\":1}\nnull\n\
              {\"Command\":\"get\"}\n{\"ID\":6,\"Command\":\"close\"}\n",
            &env,
        );
        assert_eq!(code, 1);
        let out = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 5, "{out}");
        assert!(lines[0].starts_with("{\"ID\":1,\"Err\":\"bad request: invalid type"));
        assert_eq!(
            lines[1],
            format!("{{\"ID\":2,\"Err\":\"{ERR_ACTION_ID}\"}}")
        );
        assert_eq!(
            lines[2],
            format!("{{\"ID\":3,\"Err\":\"{ERR_OUTPUT_ID}\"}}")
        );
        assert_eq!(lines[3], format!("{{\"ID\":4,\"Err\":\"{ERR_BODY_B64}\"}}"));
        assert_eq!(
            lines[4],
            format!("{{\"ID\":5,\"Err\":\"{ERR_BODY_JSON}\"}}")
        );
        // nothing was written for the refused puts
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
    }
}

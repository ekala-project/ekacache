#!/bin/sh
# A stand-in for rustc in the rustc-mode tests. It writes what cargo's rustc would for a library
# crate: <out>/lib<crate><extra>.rlib (the source bytes plus $FAKE_RLIB_EXTRA) and the dep-info
# <out>/<crate><extra>.d, and does what the FAKE_* variables say:
#   FAKE_RUSTC_LOG   append "$*" to this file (did rustc run?)
#   FAKE_STDERR      written to stderr, FAKE_STDOUT to stdout
#   FAKE_STATUS      exit status (nothing is written when it is not 0)
#   FAKE_INPUTS      more prerequisites of every rule
#   FAKE_HEADS       the rule heads instead of the rlib and the .d (set but empty: no rules)
#   FAKE_ENV_DEPS    "# env-dep:" lines, one per word
#   FAKE_NO_DEPINFO  no .d at all
#   FAKE_MODE        chmod the rlib to this
out= name= extra= src= prev=
for a in "$@"; do
  case "$prev" in
    --out-dir) out=$a ;;
    --crate-name) name=$a ;;
    -C) case "$a" in extra-filename=*) extra=${a#extra-filename=} ;; esac ;;
  esac
  case "$a" in
    --out-dir=*) out=${a#--out-dir=} ;;
    -*) ;;
    *.rs) src=$a ;;
  esac
  prev=$a
done
if [ -n "$FAKE_RUSTC_LOG" ]; then printf '%s\n' "$*" >> "$FAKE_RUSTC_LOG"; fi
if [ -n "$FAKE_STDERR" ]; then printf '%s' "$FAKE_STDERR" >&2; fi
if [ -n "$FAKE_STDOUT" ]; then printf '%s' "$FAKE_STDOUT"; fi
if [ "${FAKE_STATUS:-0}" != 0 ]; then exit "$FAKE_STATUS"; fi
if [ -z "$out" ] || [ -z "$src" ]; then exit 0; fi
rlib="$out/lib$name$extra.rlib"
dep="$out/$name$extra.d"
{ cat "$src"; printf '%s' "$FAKE_RLIB_EXTRA"; } > "$rlib" || exit 1
if [ -n "$FAKE_MODE" ]; then chmod "$FAKE_MODE" "$rlib"; fi
heads=${FAKE_HEADS-"$rlib $dep"}
if [ -z "$FAKE_NO_DEPINFO" ]; then
  {
    for h in $heads; do printf '%s: %s %s\n\n' "$h" "$src" "$FAKE_INPUTS"; done
    for i in $src $FAKE_INPUTS; do printf '%s:\n' "$i"; done
    for v in $FAKE_ENV_DEPS; do printf '# env-dep:%s\n' "$v"; done
  } > "$dep"
fi
exit 0

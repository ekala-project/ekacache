// Launcher mode, ccache style: `jig /abs/path/to/<compiler> <args...>`. A package set puts this in
// front of a compiler's own drivers (gcc, g++, clang, clang++, rustc, go), so jig sees exactly the
// arguments the language's wrapper (cc-wrapper, rustc-wrapper) settled on.
//
//   C, C++     the cc mode, conf-less, with the given compiler instead of $JIG_CC
//   rustc      the rustc mode (the same calling convention as cargo's RUSTC_WRAPPER)
//   go         exec'd with GOCACHEPROG="<jig> gocacheprog" when the socket exists and the build
//              has not set its own
//   other      exec'd untouched
#pragma once

#include <cstdint>
#include <span>
#include <string>
#include <string_view>

namespace jig {

enum class LaunchKind : std::uint8_t { kCc, kRustc, kGo, kOther };

// by the program's basename, target prefix (x86_64-unknown-linux-gnu-gcc) and version suffix
// (gcc-15, clang++-21) allowed
auto ClassifyLaunch(std::string_view basename) -> LaunchKind;

// `self`: this jig binary, for GOCACHEPROG
auto RunLauncher(const std::string& program, std::span<const std::string> args, const std::string& socket_path,
                 const std::string& self) -> int;

}  // namespace jig

#include "launcher.h"

#include <stdlib.h>  // NOLINT(modernize-deprecated-headers): setenv is POSIX
#include <unistd.h>

#include <array>
#include <cctype>
#include <cerrno>
#include <filesystem>
#include <print>
#include <span>
#include <string>
#include <string_view>
#include <system_error>
#include <vector>

#include "base.h"
#include "cc_mode.h"
#include "rustc_mode.h"

namespace jig {

namespace {

using std::string_view_literals::operator""sv;

// "gcc-15" -> "gcc", "clang++-21.1" -> "clang++": a trailing -<digits and dots>
auto StripVersion(std::string_view name) -> std::string_view {
  const size_t dash = name.rfind('-');
  if (dash == std::string_view::npos || dash + 1 == name.size()) {
    return name;
  }
  const std::string_view tail = name.substr(dash + 1);
  for (const char chr : tail) {
    if (std::isdigit(static_cast<unsigned char>(chr)) == 0 && chr != '.') {
      return name;
    }
  }
  return name.substr(0, dash);
}

// replace this process: go must own the terminal, signals and exit status itself
[[noreturn]] void Exec(const std::string& program, std::span<const std::string> args) {
  std::vector<std::string> storage{program};
  storage.insert(storage.end(), args.begin(), args.end());
  std::vector<char*> argv;
  argv.reserve(storage.size() + 1);
  for (std::string& arg : storage) {
    argv.push_back(arg.data());
  }
  argv.push_back(nullptr);
  ::execv(program.c_str(), argv.data());
  std::println(stderr, "jig: cannot execute {}: {}", program, std::error_code(errno, std::generic_category()).message());
  _exit(127);  // NOLINT(concurrency-mt-unsafe)
}

}  // namespace

auto ClassifyLaunch(std::string_view basename) -> LaunchKind {
  const std::string_view name = StripVersion(basename);
  static constexpr std::array kCc{"cc"sv, "c++"sv, "gcc"sv, "g++"sv, "clang"sv, "clang++"sv};
  for (const std::string_view driver : kCc) {
    // bare, or the last word of a target-prefixed name
    if (name == driver || (name.size() > driver.size() && name.ends_with(driver) &&
                           name.at(name.size() - driver.size() - 1) == '-')) {
      return LaunchKind::kCc;
    }
  }
  if (name == "rustc") {
    return LaunchKind::kRustc;
  }
  if (name == "go") {
    return LaunchKind::kGo;
  }
  return LaunchKind::kOther;
}

auto RunLauncher(const std::string& program, std::span<const std::string> args, const std::string& socket_path,
                 const std::string& self) -> int {
  const std::string basename = std::filesystem::path(program).filename().string();
  switch (ClassifyLaunch(basename)) {
    case LaunchKind::kCc:
      return RunCcMode(basename, args, socket_path, program);
    case LaunchKind::kRustc: {
      std::vector<std::string> wrapped{program};
      wrapped.insert(wrapped.end(), args.begin(), args.end());
      return RunRustcMode(wrapped, socket_path);
    }
    case LaunchKind::kGo: {
      std::error_code ignored;
      if (Env("GOCACHEPROG").empty() && std::filesystem::exists(socket_path, ignored)) {
        const std::string prog = self + " gocacheprog";
        ::setenv("GOCACHEPROG", prog.c_str(), 1);  // NOLINT(concurrency-mt-unsafe): before exec
      }
      Exec(program, args);
    }
    case LaunchKind::kOther:
      Exec(program, args);
  }
  return 1;
}

}  // namespace jig

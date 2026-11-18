project(
  'jig',
  'cpp',
  version: '0.1.0',
  license: 'MIT',
  meson_version: '>=1.3.0',
  default_options: ['cpp_std=c++26', 'warning_level=2', 'b_ndebug=false'],
)

cxx = meson.get_compiler('cpp')

# The nix store this jig serves. Fixed at build time: keys and the default
# jigd socket (<store>/../var/nix/jigd/socket) derive from it.
store_dir = get_option('store_dir')

add_project_arguments(
  '-DJIG_STORE_DIR="@0@"'.format(store_dir),
  '-fno-exceptions',
  language: 'cpp',
)

# Bounds-checked containers on whichever standard library we build against.
if cxx.get_define('_LIBCPP_VERSION', prefix: '#include <version>') != ''
  add_project_arguments(
    '-U_LIBCPP_HARDENING_MODE',
    '-D_LIBCPP_HARDENING_MODE=_LIBCPP_HARDENING_MODE_EXTENSIVE',
    language: 'cpp',
  )
else
  add_project_arguments('-D_GLIBCXX_ASSERTIONS', language: 'cpp')
endif

deps = [
  dependency('libblake3'),
  dependency('libzstd'),
  dependency('nlohmann_json'),
]

subdir('jig')

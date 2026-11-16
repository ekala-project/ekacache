common_sources = files(
  'src/base.cc',
  'src/cache_client.cc',
  'src/cc_mode.cc',
  'src/dir_mode.cc',
  'src/driver.cc',
  'src/elf_fixup.cc',
  'src/fixup_mode.cc',
  'src/gocache_mode.cc',
  'src/launcher.cc',
  'src/macho_fixup.cc',
  'src/manifest.cc',
  'src/nix_store_mode.cc',
  'src/process.cc',
  'src/rustc_mode.cc',
  'src/store.cc',
)

libjig = static_library('jig', common_sources, dependencies: deps)

executable(
  'jig',
  'src/main.cc',
  link_with: libjig,
  dependencies: deps,
  install: true,
)

jig_test = executable(
  'jig_test',
  'src/jig_test.cc',
  link_with: libjig,
  dependencies: deps,
  # the tests are plain assert()s
  cpp_args: ['-UNDEBUG'],
)
test('jig_test', jig_test)

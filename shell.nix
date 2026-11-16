# Development shell: `meson setup build && meson test -C build` for jig, `go test ./...` in jigd/.
{
  pkgs ? import <nixpkgs> { },
}:
pkgs.mkShell {
  packages = [
    pkgs.meson
    pkgs.ninja
    pkgs.pkg-config
    pkgs.go
    pkgs.libblake3
    pkgs.zstd
    pkgs.nlohmann_json
  ];
}

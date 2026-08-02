{
  description = "fable-defs — a from-scratch def compiler for Fable: The Lost Chapters";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
  };

  outputs =
    { nixpkgs, rust-overlay, ... }:
    let
      system = "x86_64-linux";
      overlays = [ (import rust-overlay) ];
      pkgs = import nixpkgs {
        inherit system overlays;
      };
    in
    {
      devShells.${system}.default = pkgs.mkShell {
        # Pure-Rust workspace — no graphics/system libs (unlike the OpenAlbion renderer).
        buildInputs = with pkgs; [
          (rust-bin.stable.latest.default.override {
            extensions = [
              "rust-src"
              "rust-analyzer"
            ];
            targets = [
              # MSVC ABI only. def-compiler-sys ships a static library and the
              # consumer (EgoCore) is an MSVC v143 x64 project; mingw archives do
              # not link cleanly into one, so there is no reason to carry a
              # windows-gnu target or a mingw toolchain here.
              "x86_64-pc-windows-msvc"
              # Release builds of `defc` for Linux. The default gnu target bakes
              # the absolute path of *this shell's* glibc loader into the ELF
              # interpreter (a /nix/store/... path), so such a binary cannot start
              # on any machine that lacks that exact store path — it fails with a
              # misleading "no such file or directory". musl links libc statically
              # and needs no interpreter at all, so the artifact is portable.
              # The workspace is pure Rust (miniz_oxide, no libz-sys), so this
              # needs no C cross-toolchain.
              "x86_64-unknown-linux-musl"
            ];
          })
          # Cross-links the MSVC target from Linux: fetches the Windows SDK and
          # CRT import libraries and drives lld-link. See packages/def-compiler-sys.
          cargo-xwin
          llvmPackages.bintools
        ];

        shellHook = ''
          # Keep the downloaded Windows SDK out of $HOME and inside the repo.
          export XWIN_CACHE_DIR="$PWD/target/xwin"
        '';
      };
    };
}

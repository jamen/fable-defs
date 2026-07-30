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
              # def-compiler-sys ships an MSVC-ABI static library, because the
              # consumer (EgoCore) is an MSVC v143 x64 project and mingw archives
              # do not link cleanly into one.
              "x86_64-pc-windows-msvc"
              "x86_64-pc-windows-gnu"
            ];
          })
          # Cross-links the MSVC target from Linux: fetches the Windows SDK and
          # CRT import libraries and drives lld-link. See packages/def-compiler-sys.
          cargo-xwin
          llvmPackages.bintools
          pkgsCross.mingwW64.stdenv.cc
          pkgsCross.mingwW64.windows.pthreads
        ];

        shellHook = ''
          export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc
          # Keep the downloaded Windows SDK out of $HOME and inside the repo.
          export XWIN_CACHE_DIR="$PWD/target/xwin"
        '';
      };
    };
}

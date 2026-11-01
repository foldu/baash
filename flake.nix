{
  description = "baash — a POSIX- and bash-compatible interactive shell";

  # Pinned to the nixos.org channel tarball: it redirects to an immutable
  # release that the lock records, and it is smaller and not rate-limited
  # compared to the GitHub mirror.
  inputs = {
    nixpkgs.url = "https://channels.nixos.org/nixos-unstable/nixexprs.tar.zst";

    # `crane` is the derivation plumbing for Rust: it builds the dependency
    # graph once and lets the checks share it, so `nix flake check` compiles
    # the workspace once instead of once per check.
    # (crane takes nixpkgs as an argument rather than as an input, so there is
    # nothing to follow here.)
    crane.url = "github:ipetkov/crane";

    # `rust-overlay` is the only source of a Rust whose std carries a *second*
    # target, which the Windows cross build needs and nixpkgs' rustc does not.
    # It also pins the one toolchain everything else uses, so the dev shell, the
    # checks and the release build agree on a compiler.
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      crane,
      rust-overlay,
    }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];

      rustVersion = "1.99.1";

      eachSystem =
        f:
        nixpkgs.lib.genAttrs systems (
          system:
          let
            base = nixpkgs.legacyPackages.${system};
            pkgs = base.extend rust-overlay.overlays.default;

            hostToolchain = pkgs.rust-bin.stable.${rustVersion}.default;
            windowsToolchain = pkgs.rust-bin.stable.${rustVersion}.default.override {
              targets = [ "x86_64-pc-windows-gnu" ];
            };

            craneLib = (crane.mkLib pkgs).overrideToolchain (_: hostToolchain);
            windowsCraneLib = (crane.mkLib pkgs).overrideToolchain (_: windowsToolchain);

            # Not `cleanCargoSource`: the binary `include_bytes!`s
            # `config/config.sh`, which a Cargo-only filter would drop.
            src = nixpkgs.lib.cleanSource ./.;

            commonArgs = {
              inherit src;
              pname = "baash";
              version = "0.0.0";
              strictDeps = true;
            };

            # Everything the workspace builds, dependencies included, so each
            # check only relinks the crates that changed.
            cargoArtifacts = craneLib.buildDepsOnly (
              commonArgs
              // {
                cargoBuildFlags = [
                  "--workspace"
                  "--all-features"
                  "--all-targets"
                ];
              }
            );

            windows = windowsCraneLib.mkCargoDerivation {
              pname = "baash-windows";
              version = "0.0.0";
              inherit src;

              # No shared `cargoArtifacts` here: a second target's artifacts are
              # a second graph, so this builds its own. Nix caches the whole
              # derivation instead.
              cargoArtifacts = null;
              doInstallCargoArtifacts = false;
              cargoVendorDir = windowsCraneLib.vendorCargoDeps { inherit src; };

              # `cargo zigbuild` supplies the mingw linker through zig rather
              # than through a system MinGW — zig's sysroot is what carries the
              # `libpthread.a` that a windows-gnu dylib link asks for by exact
              # name, which the MinGW cross toolchain does not have.
              #
              # `ci/windres` is the shim `turso_sdk_kit`'s build script needs,
              # because it asks for `windres` by name; the compiler it execs
              # preprocesses the script and looks for `clang` to do it.
              nativeBuildInputs = [
                pkgs.cargo-zigbuild
                pkgs.zig
                pkgs.llvmPackages.bintools
                pkgs.llvmPackages.clang
                pkgs.coreutils
              ];
              buildPhaseCargoCommand = ''
                mkdir -p target/x86_64-pc-windows-gnu
                cp --no-preserve=mode ${./ci/windres} target/windres
                chmod 755 target/windres
                export PATH="$PWD/target:$PATH"
                # The unwrapped path, not `bintools`: that derivation's
                # `bin/llvm-windres` is a wrapper that needs the stdenv's
                # environment, and a build script does not have it.
                export BAASH_WINDRES="${pkgs.llvmPackages.bintools-unwrapped}/bin/llvm-windres"
                # Both caches want to live under $HOME, and the sandbox's home
                # is not writable.
                export HOME="$TMPDIR"
                export ZIG_GLOBAL_CACHE_DIR="$TMPDIR/zig-global"
                export ZIG_LOCAL_CACHE_DIR="$TMPDIR/zig-local"
                cargo zigbuild --profile windows --target x86_64-pc-windows-gnu -p baash
              '';
              installPhaseCommand = ''
                mkdir -p $out/bin
                cp target/x86_64-pc-windows-gnu/windows/baash.exe $out/bin/
              '';
            };

          in
          f {
            inherit
              pkgs
              hostToolchain
              craneLib
              cargoArtifacts
              src
              commonArgs
              windows
              system
              ;
          }
        );
    in
    {
      packages = eachSystem (
        { pkgs, craneLib, cargoArtifacts, commonArgs, windows, ... }:
        {
          default = craneLib.buildPackage (
            commonArgs
            // {
              pname = "baash";
              version = "0.0.0";
              inherit cargoArtifacts;
              # Tests are their own gate (`cargo test` in CI, on a real pty);
              # the package build stays a packaging check, as it was.
              doCheck = false;

              # The default configuration is compiled into the binary; this copy
              # is what a distributor installs as
              # `$XDG_CONFIG_DIRS/baash/config.sh` (/etc/xdg/baash/config.sh) to
              # replace it. On this machine the store path is not on
              # `XDG_CONFIG_DIRS`, so the built-in copy is what runs unless
              # someone adds it.
              postInstall = ''
                install -Dm644 config/config.sh "$out/etc/xdg/baash/config.sh"
              '';

              meta = {
                description = "POSIX- and bash-compatible interactive shell";
                mainProgram = "baash";
                license = pkgs.lib.licenses.asl20;
                platforms = nixpkgs.lib.platforms.linux;
              };
            }
          );
          inherit windows;
        }
      );

      # `nix flake check` builds these. Two gates cannot be checks, for reasons
      # the sandbox dictates, and stay jobs in `.gitlab-ci.yml`: the test suite,
      # because the `interactive_*` tests drive a real pty and the sandbox's pty
      # does not carry data (`openpty` succeeds, reads hang); and conformance,
      # because 18 of its cases read the machine (hostname, tzdata, uid,
      # timers) and a sandbox measures 2326 where a real environment measures
      # 2344.
      checks = eachSystem (
        {
          pkgs,
          craneLib,
          cargoArtifacts,
          src,
          commonArgs,
          windows,
          ...
        }:
        {
          build = self.packages.${pkgs.stdenv.hostPlatform.system}.default;

          fmt = craneLib.cargoFmt { inherit src; };

          clippy = craneLib.cargoClippy (
            commonArgs
            // {
              inherit cargoArtifacts;
              cargoClippyExtraArgs = "--workspace --all-features --all-targets -- --deny warnings";
            }
          );

          inherit windows;
        }
      );

      devShells = eachSystem (
        { pkgs, hostToolchain, ... }:
        {
          default = pkgs.mkShell {
            packages = [
              hostToolchain
              pkgs.clippy
              pkgs.rustfmt
              pkgs.rust-analyzer
              # The vendored Oils suite shells out to python helpers written for
              # python2; the harness substitutes python3 equivalents when python2
              # is absent. dash is the reference for --compare.
              pkgs.python3
              pkgs.dash
              # Task runner and the release driver (`justfile`, `.release.toml`).
              pkgs.just
              pkgs.cargo-release
              # `crates/baash/tests/interactive_zoxide.rs` drives the real
              # `zoxide init bash` script against a pty, so the integration is
              # tested against the file a user would source and not a copy of it.
              pkgs.zoxide
            ];

            shellHook = ''
              # A checkout path containing non-ASCII characters (the ß in
              # …/ßhell is enough) breaks linking: the mold linker wrapper
              # mangles the argument and rustc then reports missing object
              # files. Redirect the build directory only when the path is not
              # plain ASCII, so CI keeps its normal ./target and its cache.
              if printf '%s' "$PWD" | LC_ALL=C grep -q '[^ -~]'; then
                export CARGO_TARGET_DIR="''${TMPDIR:-/tmp}/baash-target"
                # The shell also bakes $PWD into NIX_LDFLAGS as an -rpath, and
                # non-ASCII bytes there are mangled the same way. This workspace
                # has no C dependencies, so dropping the flag is enough.
                unset NIX_LDFLAGS
                echo "baash: non-ASCII checkout path; CARGO_TARGET_DIR=$CARGO_TARGET_DIR"
              fi
              echo "baash dev shell: cargo / rustc / clippy / rustfmt / rust-analyzer"
              echo "  cargo test --workspace"
              echo "  nix flake check"
            '';
          };
        }
      );

      formatter = eachSystem ({ pkgs, ... }: pkgs.nixfmt);
    };
}

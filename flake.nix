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
            #
            # `vendor/bash-completion/` is filtered out on purpose. The build
            # copies the pinned tree in (`preBuild`), so whether a checkout
            # happens to have one — `nix develop` puts it there — must not
            # change the source hash: a developer and CI would otherwise build
            # two different derivations of the same commit and never share a
            # store path.
            src = nixpkgs.lib.cleanSourceWith {
              src = ./.;
              filter =
                path: _type:
                nixpkgs.lib.cleanSourceFilter path "regular"
                && !(
                  toString path == "${toString ./.}/vendor"
                  || nixpkgs.lib.hasPrefix "${toString ./.}/vendor/" (toString path)
                );
            };

            commonArgs = {
              inherit src;
              pname = "baash";
              version = "0.0.0";
              strictDeps = true;
            };

            # The bash-completion library the binary can embed is not
            # vendored: this is the copy step. Pinning it to nixpkgs' package
            # is what makes the embedded version move with the flake instead of
            # with whoever remembered to update a directory. `PROVENANCE`
            # records the store path, which is what an operator sees when they
            # look at the mounted tree. The Windows build uses this; the Linux
            # package leaves it out and links against the system's copy, so the
            # copy is only compiled into a binary that enables
            # `embedded-bash-completion`.
            embedCompletion = ''
              rm -rf vendor/bash-completion
              mkdir -p vendor
              # `--no-preserve=mode`: the store is read-only, and the copy has
              # to be writable — `PROVENANCE` is written into it, and a
              # checkout that cannot be cleaned up is not a checkout.
              cp -r --no-preserve=mode ${pkgs.bash-completion}/share/bash-completion \
                    vendor/bash-completion
              printf '%s\n' ${pkgs.bash-completion} > vendor/bash-completion/PROVENANCE
            '';

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

            # `cargo deny check licenses`: the allow-list in `deny.toml` is what
            # keeps `THIRD-PARTY.md` honest when a dependency appears.
            licenses = craneLib.mkCargoDerivation {
              pname = "baash-licenses";
              version = "0.0.0";
              inherit src;
              cargoArtifacts = null;
              doInstallCargoArtifacts = false;
              cargoVendorDir = craneLib.vendorCargoDeps { inherit src; };
              nativeBuildInputs = [ pkgs.cargo-deny ];
              buildPhaseCargoCommand = "cargo deny check --deny warnings licenses";
              installPhaseCommand = ''
                mkdir -p $out
                cp deny.toml $out/
              '';
            };

            # --- the Windows target -------------------------------------------
            #
            # Two builds of the same program. `windows` is the release shape —
            # fat LTO, one codegen unit, stripped — which is what CI ships and
            # what a guest runs. `windows-fast` is the iteration shape, the same
            # code with LTO off, for the edit / compile / copy-to-the-guest
            # loop. They differ by a profile name.
            #
            # Each is *two* derivations, because nearly all of the wall clock is
            # the dependency graph rather than this workspace:
            #
            #   <name>-deps  the crates.io graph, built from a copy of the tree
            #                whose workspace sources crane replaces with stubs
            #                (`mkDummySrc`). Its store path therefore depends on
            #                `Cargo.lock` and the manifests and *not* on any edit
            #                under `crates/*/src`, so a source change reuses it.
            #   <name>       the workspace crates and the final link, seeded
            #                with that `target/`.
            #
            # The rule that makes the split work is that the halves must be
            # configured identically. A linker, a C flag or a profile present in
            # only one of them is not an error: it is a silent full rebuild of
            # every dependency in the other, which is the cost the split exists
            # to remove. So everything but `cargoArtifacts` and the install
            # phase comes from one argument set, `windowsCommon`.
            mkWindows =
              {
                pname,
                profile,
              }:
              let
                windowsCommon = {
                  inherit pname src;
                  version = "0.0.0";
                  strictDeps = true;
                  preBuild = embedCompletion;
                  cargoVendorDir = windowsCraneLib.vendorCargoDeps { inherit src; };

                  # `cargo zigbuild` supplies the mingw linker through zig
                  # rather than through a system MinGW — zig's sysroot is what
                  # carries the `libpthread.a` that a windows-gnu dylib link
                  # asks for by exact name, which the MinGW cross toolchain does
                  # not have.
                  #
                  # `ci/windres` is the shim `turso_sdk_kit`'s build script
                  # needs, because it asks for `windres` by name; the compiler
                  # it execs preprocesses the script and looks for `clang` to do
                  # it.
                  nativeBuildInputs = [
                    pkgs.cargo-zigbuild
                    pkgs.zig
                    pkgs.llvmPackages.bintools
                    pkgs.llvmPackages.clang
                    pkgs.coreutils
                  ];
                  # `libmimalloc-sys` builds C, and mimalloc's `options.c`
                  # prints its build date with `__DATE__`. Nix's cc wrapper
                  # makes that an error for reproducibility, which is the right
                  # default for Nix's own packages but not for a vendored
                  # allocator whose output is never compared: the flag is
                  # relaxed for the C dependency only.
                  env = {
                    NIX_CFLAGS_COMPILE = "-Wno-error=date-time";
                    # cc-rs appends these after its own flags, so the relaxed
                    # warning wins over the `-Werror=date-time` the wrapper
                    # adds.
                    CFLAGS_x86_64_pc_windows_gnu = "-Wno-error=date-time";
                    CFLAGS_x86_64-pc-windows-gnu = "-Wno-error=date-time";
                  };
                };

                # The shim, the resource compiler and the zig caches, in front
                # of every cargo call in either half.
                prepare = ''
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
                '';

                # One command, spelled once, so the two halves cannot drift.
                cargoBuild = ''
                  cargo zigbuild --locked --profile ${profile} --target x86_64-pc-windows-gnu -p baash --features mimalloc
                '';

                deps = windowsCraneLib.buildDepsOnly (
                  windowsCommon
                  // {
                    # The default `doCheck` also compiles every test target
                    # (`--all-targets`), and the test targets here are Unix-only
                    # by design.
                    doCheck = false;
                    buildPhaseCargoCommand = ''
                      ${prepare}
                      ${cargoBuild}
                    '';
                  }
                );
              in
              {
                inherit deps;
                package = windowsCraneLib.mkCargoDerivation (
                  windowsCommon
                  // {
                    cargoArtifacts = deps;
                    doInstallCargoArtifacts = false;
                    buildPhaseCargoCommand = ''
                      ${prepare}
                      ${cargoBuild}
                    '';
                    installPhaseCommand = ''
                      mkdir -p $out/bin
                      cp target/x86_64-pc-windows-gnu/${profile}/baash.exe $out/bin/
                    '';
                  }
                );
              };

            windows = mkWindows {
              pname = "baash-windows";
              profile = "windows";
            };

            windowsFast = mkWindows {
              pname = "baash-windows-fast";
              profile = "windows-fast";
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
              embedCompletion
              licenses
              windows
              windowsFast
              system
              ;
          }
        );
    in
    {
      packages = eachSystem (
        {
          pkgs,
          craneLib,
          cargoArtifacts,
          commonArgs,
          embedCompletion,
          windows,
          windowsFast,
          ...
        }:
        let
          # The icon theme files. `share/icons/hicolor` is freedesktop's
          # fallback theme, and it is where an `Icon=baash` in a desktop entry
          # is resolved from: one vector source under `scalable/apps`, plus a
          # PNG at each size the theme spec names, for the consumers that will
          # not read SVG. They are generated here instead of being checked in,
          # so the PNGs follow `assets/logo.svg` rather than drifting from it.
          icons = pkgs.runCommand "baash-icons" { nativeBuildInputs = [ pkgs.librsvg ]; } ''
            for size in 16 24 32 48 64 128 256; do
              install -d "$out/share/icons/hicolor/''${size}x''${size}/apps"
              rsvg-convert -w "$size" -h "$size" ${./assets/logo.svg} \
                -o "$out/share/icons/hicolor/''${size}x''${size}/apps/baash.png"
            done
            install -Dm644 ${./assets/logo.svg} \
              "$out/share/icons/hicolor/scalable/apps/baash.svg"
          '';
        in
        {
          default = craneLib.buildPackage (
            commonArgs
            // {
              pname = "baash";
              version = "0.0.0";
              inherit cargoArtifacts;
              preBuild = embedCompletion;
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
                # The artwork, in the one place a Linux desktop looks for it.
                cp -r --no-preserve=mode ${icons}/share "$out/share"
              '';

              meta = {
                description = "POSIX- and bash-compatible interactive shell";
                mainProgram = "baash";
                # GPL-3.0-or-later: the same license as bash.
                license = pkgs.lib.licenses.gpl3Plus;
                platforms = nixpkgs.lib.platforms.linux;
              };

              # `environment.shells` takes a shell *package*, not a derivation
              # that happens to contain a shell: NixOS's `toShellPath` checks for
              # this attribute and builds `/run/current-system/sw/bin/baash` from
              # it, which is what `/etc/shells`, `chsh` and PAM consult. Without
              # it the module throws "is not a shell package" while the system
              # is evaluated — `nix/check-module.nix` never forced `/etc/shells`,
              # so the gate did not see it.
              passthru.shellPath = "/bin/baash";
            }
          );
          # The release executable, and the same program without LTO for the
          # edit-compile-run loop. `nix build .#windows-fast` reuses a separate
          # cached dependency build, so both are incremental in the only sense
          # Nix has: the parts that did not change are not rebuilt.
          windows = windows.package;
          windows-fast = windowsFast.package;

          # The cached halves on their own, so a drift in the split is visible:
          # these two store paths must not move when only `crates/*/src`
          # changed.
          windows-deps = windows.deps;
          windows-fast-deps = windowsFast.deps;

          # The library the binary embeds, exposed so the dev loop can copy the
          # exact tree the package build would: it is not vendored, and `nix
          # build .#bash-completion` is how `vendor/bash-completion/` is filled
          # outside the dev shell.
          bash-completion = pkgs.bash-completion;
        }
      );

      # A NixOS module: `programs.baash` installs the shell and carapace, lists
      # it in `/etc/shells`, writes the system startup file, and optionally points
      # it at bash-completion and zoxide. `nix/check-module.nix` is its gate.
      # `default` is the same module, so `imports = [ baash.nixosModules.default ]`
      # reads the way other flakes do.
      nixosModules = {
        default = self.nixosModules.baash;
        baash = import ./nix/module.nix { inherit self; };
      };

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
          licenses,
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

          inherit licenses;
          windows = windows.package;

          # The module's product is one generated startup file, and nothing
          # else in this gate ever sources it. This evaluates a system with the
          # module enabled and runs the file in the shell it installed.
          module = pkgs.callPackage ./nix/check-module.nix {
            inherit nixpkgs self;
          };
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
              pkgs.cargo-deny
              # `crates/baash/tests/interactive_zoxide.rs` drives the real
              # `zoxide init bash` script against a pty, so the integration is
              # tested against the file a user would source and not a copy of it.
              pkgs.zoxide
            ];

            shellHook = ''
              # The bash-completion library the binary embeds is not vendored;
              # copy the pinned one in, the same way the package build does. The
              # guard is on the recorded store path, not on existence, so an
              # unchanged tree is not recopied (and the crate not rebuilt) every
              # time the shell is entered. `git` is not required: without a
              # repository the current directory is the checkout.
              root=$(git rev-parse --show-toplevel 2>/dev/null || printf '%s' "$PWD")
              if [ "$(cat "$root/vendor/bash-completion/PROVENANCE" 2>/dev/null)" \
                 != "${pkgs.bash-completion}" ]; then
                # A copy from before `--no-preserve=mode` is read-only.
                chmod -R u+w "$root/vendor/bash-completion" 2>/dev/null || true
                rm -rf "$root/vendor/bash-completion"
                mkdir -p "$root/vendor"
                cp -r --no-preserve=mode ${pkgs.bash-completion}/share/bash-completion \
                      "$root/vendor/bash-completion"
                printf '%s\n' ${pkgs.bash-completion} \
                      > "$root/vendor/bash-completion/PROVENANCE"
                echo "baash: bash-completion from ${pkgs.bash-completion}"
              fi

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

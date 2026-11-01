{
  description = "baash — a POSIX- and bash-compatible interactive shell";

  # Pinned to the nixos.org channel tarball: it redirects to an immutable
  # release that the lock records, and it is smaller and not rate-limited
  # compared to the GitHub mirror.
  inputs.nixpkgs.url = "https://channels.nixos.org/nixos-unstable/nixexprs.tar.zst";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      eachSystem = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = eachSystem (pkgs: {
        default = pkgs.rustPlatform.buildRustPackage {
          pname = "baash";
          version = "0.0.0";

          src = nixpkgs.lib.cleanSource ./.;
          cargoLock.lockFile = ./Cargo.lock;
          cargoBuildFlags = [ "--workspace" ];

          # Tests and lints run in the `nix develop` check job, so the package
          # build stays a packaging gate rather than a second test run.
          doCheck = false;

          # The default configuration is compiled into the binary; this copy is
          # what a distributor installs as `$XDG_CONFIG_DIRS/baash/config.sh`
          # (/etc/xdg/baash/config.sh) to replace it. On this machine the
          # store path is not on `XDG_CONFIG_DIRS`, so the built-in copy is what
          # runs unless someone adds it.
          postInstall = ''
            install -Dm644 config/config.sh "$out/etc/xdg/baash/config.sh"
          '';

          meta = {
            description = "POSIX- and bash-compatible interactive shell";
            mainProgram = "baash";
            license = pkgs.lib.licenses.asl20;
            platforms = nixpkgs.lib.platforms.linux;
          };
        };
      });

      # `nix flake check` builds this.
      checks = eachSystem (pkgs: {
        build = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      });

      devShells = eachSystem (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            rustc
            clippy
            rustfmt
            rust-analyzer
            # The vendored Oils suite shells out to python helpers written for
            # python2; the harness substitutes python3 equivalents when python2
            # is absent. dash is the reference for --compare.
            python3
            dash
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
          '';
        };
      });

      formatter = eachSystem (pkgs: pkgs.nixfmt-rfc-style);
    };
}

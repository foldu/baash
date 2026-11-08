# The gate for the NixOS module.
#
# The module's whole product is one generated startup file, and nothing else in
# `nix flake check` ever evaluates the module, let alone sources that file. This
# evaluates a system three times — both options on, `bashCompletion = false`,
# and the module imported but not enabled — copies the generated files into
# `$XDG_CONFIG_DIRS` trees, and runs the shell the module installed against
# them. The assertions are what a user would see: the completion framework's
# functions, a real per-command script, zoxide's hook and the shell's own `ls`
# alias in one run, none of the additions in the second, and an untouched
# system in the third.
#
# What this does not prove: a real `/etc` (the file is copied to a directory
# instead), PAM (that `/etc/shells` is what allows `chsh`), or anything about a
# machine's own `bash-completion` — the store path is the one the module names.
{
  nixpkgs,
  pkgs,
  self,
}:
let
  inherit (pkgs) lib;

  evaluate =
    modules:
    nixpkgs.lib.nixosSystem {
      system = pkgs.stdenv.hostPlatform.system;
      modules = [
        self.nixosModules.default
        # Only to silence the "stateVersion is not set" warning; nothing here
        # is ever activated, so the value is not a decision.
        { system.stateVersion = "26.11"; }
      ]
      ++ modules;
    };

  # Both options on: the library and zoxide are the two things this module adds
  # to the shell's defaults, and each has a visible effect at startup.
  both = evaluate [
    {
      programs.baash = {
        enable = true;
        zoxide.enable = true;
        # Appended last, so this also pins the order: after the defaults and
        # after zoxide's hook.
        extraConfig = "echo extra";
      };
    }
  ];

  # The same module with neither: `bashCompletion`'s default is on, so this is
  # the one case where it is off, and it must not leave a library behind.
  bare = evaluate [
    {
      programs.baash = {
        enable = true;
        bashCompletion = false;
      };
    }
  ];

  # Imported, not enabled: `mkIf` has to leave the system as it was.
  disabled = evaluate [ ];

  package = both.config.programs.baash.package;

  file = config: config.environment.etc."xdg/baash/config.sh".source;

  registered = system: builtins.elem package system.config.environment.shells;
  installed = system: builtins.elem package system.config.environment.systemPackages;
  zoxideInstalled = builtins.elem both.config.programs.baash.zoxide.package both.config.environment.systemPackages;
in
if !registered both || !registered bare then
  throw "programs.baash.enable did not add the package to environment.shells"
else if !installed both || !installed bare then
  throw "programs.baash.enable did not add the package to environment.systemPackages"
else if both.config.programs.baash.zoxide.enable && !zoxideInstalled then
  throw "programs.baash.zoxide.enable did not add zoxide to environment.systemPackages"
else if
  disabled.config.environment.etc ? "xdg/baash/config.sh" || installed disabled || registered disabled
then
  throw "the module changed the system while programs.baash.enable was false"
else
  pkgs.runCommand "baash-module-test" { nativeBuildInputs = [ package ]; } ''
    export HOME=$TMPDIR
    mkdir -p $TMPDIR/on/baash $TMPDIR/off/baash
    cp ${file both.config} $TMPDIR/on/baash/config.sh
    cp ${file bare.config} $TMPDIR/off/baash/config.sh

    # `-i` is what reads a startup file; `-c` keeps the run off a pty.
    XDG_CONFIG_DIRS=$TMPDIR/on baash -i -c '
        echo "library=''${BAASH_BASH_COMPLETION-}"
        echo "framework=$(type -t _comp_initialize)"
        echo "zoxide=$(type -t __zoxide_hook)"
        echo "alias=$(alias ls)"
        _comp_load find
        echo "loaded=$(complete -p find)"
    ' > $TMPDIR/on.out 2>&1 || { cat $TMPDIR/on.out; exit 1; }

    grep -q '^library=.*/share/bash-completion/bash_completion$' $TMPDIR/on.out \
      || { echo "the module did not name the library:"; cat $TMPDIR/on.out; exit 1; }
    grep -q '^framework=function$' $TMPDIR/on.out \
      || { echo "bash-completion did not load:"; cat $TMPDIR/on.out; exit 1; }
    grep -q '^zoxide=function$' $TMPDIR/on.out \
      || { echo "zoxide's hook was not installed:"; cat $TMPDIR/on.out; exit 1; }
    # A system config replaces the built-in one, so this only survives because
    # the module sources the package's copy before adding anything.
    grep -q "^alias=alias ls=" $TMPDIR/on.out \
      || { echo "the shell's own defaults were dropped:"; cat $TMPDIR/on.out; exit 1; }
    # A per-command script, out of the library's own `completions/`, found
    # through the directory the framework derived from the store path.
    grep -q 'complete -F _comp_cmd_find find' $TMPDIR/on.out \
      || { echo "the library loaded no script for find:"; cat $TMPDIR/on.out; exit 1; }
    grep -q '^extra$' $TMPDIR/on.out \
      || { echo "extraConfig was not appended:"; cat $TMPDIR/on.out; exit 1; }

    XDG_CONFIG_DIRS=$TMPDIR/off baash -i -c '
        echo "library=''${BAASH_BASH_COMPLETION-}"
        echo "framework=$(type -t _comp_initialize)"
        echo "zoxide=$(type -t __zoxide_hook)"
        echo "alias=$(alias ls)"
    ' > $TMPDIR/off.out 2>&1 || { cat $TMPDIR/off.out; exit 1; }

    grep -q '^library=$' $TMPDIR/off.out \
      || { echo "bashCompletion = false still names a library:"; cat $TMPDIR/off.out; exit 1; }
    grep -q '^framework=$' $TMPDIR/off.out \
      || { echo "bashCompletion = false still loaded the library:"; cat $TMPDIR/off.out; exit 1; }
    grep -q '^zoxide=$' $TMPDIR/off.out \
      || { echo "zoxide's hook appeared without the option:"; cat $TMPDIR/off.out; exit 1; }
    grep -q "^alias=alias ls=" $TMPDIR/off.out \
      || { echo "the shell's own defaults were dropped:"; cat $TMPDIR/off.out; exit 1; }

    mkdir -p $out
    cp $TMPDIR/on.out $TMPDIR/off.out $out/
  ''

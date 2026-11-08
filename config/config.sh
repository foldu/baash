# baash: the shell's own defaults.
#
# Every interactive baash reads this file before yours, so anything here can be
# overridden in $XDG_CONFIG_HOME/baash/config.sh (~/.config/baash/config.sh).
# A distribution may replace it with $XDG_CONFIG_DIRS/baash/config.sh
# (/etc/xdg/baash/config.sh) without rebuilding the shell.
#
# It is *sourced*, not executed: an alias, function or option set here stays in
# your shell. Scripts read none of it — `baash script.sh`, `baash -c …` and
# pipelines never source a startup file — so nothing here can change a script's
# behaviour, only what you type.

# `ls` in a terminal: colour the file types, and emit OSC 8 hyperlinks so a
# path in the listing is clickable. GNU coreutils understands both; the BSD
# `ls` on macOS and the BSDs has no `--hyperlink` and spells colour `-G`, so
# `$OSTYPE` decides once instead of probing with a process per shell.
case ${OSTYPE-} in
    darwin* | freebsd* | dragonfly* | netbsd* | openbsd*) alias ls='ls -G' ;;
    *) alias ls='ls --color=auto --hyperlink=auto' ;;
esac

# Long listings, and a grep that colours the match. `auto` keeps both quiet
# when the output is a pipe or a file, which is what makes them safe to leave
# on: `ll | sort` is still plain text.
alias ll='ls -lh'
alias la='ls -lAh'
alias grep='grep --color=auto'

# Programmable completion for the commands on this machine, from the library
# bash-completion ships: `complete -F` functions run at Tab, so the same
# scripts that complete a command in bash complete it here. It costs a few
# milliseconds at startup and installs a `-D` rule that loads a command's script
# on its first Tab, which is what bash does through its own startup files.
#
# The shell carries a copy of the library and unpacks it into the cache
# directory on the first interactive start, so a machine with none installed —
# a Windows one, or a machine running the static binary — still has one;
# `BAASH_EMBEDDED_BASH_COMPLETION` names that copy. It is the last candidate
# because a distribution's copy is the one the rest of the machine uses, and
# because a newer one would know commands this build has not heard of. A system
# with none, and a `BAASH_BASH_COMPLETION` pointing at nothing, still gets the
# embedded library.
#
# `BAASH_BASH_COMPLETION` names a library explicitly (NixOS keeps it in the
# store, not in the profile); an unreadable or absent path is skipped, as are
# the standard ones on a system that has no bash-completion. Set
# `BAASH_NO_BASH_COMPLETION=1` to turn all of it off.
if [[ -z ${BAASH_NO_BASH_COMPLETION-} ]]; then
    for _baash_bash_completion in \
        "${BAASH_BASH_COMPLETION-}" \
        /usr/share/bash-completion/bash_completion \
        /etc/bash_completion \
        "${BAASH_EMBEDDED_BASH_COMPLETION-}"; do
        [[ -n $_baash_bash_completion && -r $_baash_bash_completion ]] || continue
        . "$_baash_bash_completion"
        break
    done
    unset -v _baash_bash_completion
fi

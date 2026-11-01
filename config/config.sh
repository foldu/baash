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

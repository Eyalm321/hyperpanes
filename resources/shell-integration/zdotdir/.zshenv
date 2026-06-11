# Hyperpanes zsh integration — bundled ZDOTDIR, stage 1 of 2 (.zshenv).
#
# The app spawns `ZDOTDIR=<this dir> HYPERPANES_ZDOTDIR_ORIG=<user's ZDOTDIR, if any>
# zsh -i`, so zsh reads OUR startup files. This .zshenv runs first (for every zsh):
# delegate to the user's real .zshenv with their ZDOTDIR restored, then — unless
# their .zshenv re-pointed ZDOTDIR itself — point ZDOTDIR back here so our .zshrc
# (stage 2) runs for the interactive shell.
_hyperpanes_zdotdir="$ZDOTDIR"
ZDOTDIR="${HYPERPANES_ZDOTDIR_ORIG:-$HOME}"
[ -f "$ZDOTDIR/.zshenv" ] && . "$ZDOTDIR/.zshenv"
if [ "$ZDOTDIR" = "${HYPERPANES_ZDOTDIR_ORIG:-$HOME}" ]; then
  ZDOTDIR="$_hyperpanes_zdotdir"
fi
unset _hyperpanes_zdotdir

# Hyperpanes zsh integration — bundled ZDOTDIR, stage 2 of 2 (.zshrc).
#
# Runs for interactive shells only. Source the user's real .zshrc first (with their
# ZDOTDIR restored, so their prompt/aliases/plugins all apply), then chain in the
# hyperpanes cwd-reporting hook from hp-init.sh (sibling of this directory).
# Guarded throughout: any failure leaves a normal interactive zsh.
_hyperpanes_init="${${(%):-%x}:A:h:h}/hp-init.sh"
ZDOTDIR="${HYPERPANES_ZDOTDIR_ORIG:-$HOME}"
unset HYPERPANES_ZDOTDIR_ORIG
[ -f "$ZDOTDIR/.zshrc" ] && . "$ZDOTDIR/.zshrc"
[ -f "$_hyperpanes_init" ] && . "$_hyperpanes_init"
unset _hyperpanes_init

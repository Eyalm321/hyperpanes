# Hyperpanes shell integration (bash / git-bash / zsh).
#
# Sourcing mechanism per shell (deploy path: resources/shell-integration/ next to
# the binary — same as Windows):
#   bash:  spawned as `bash --rcfile <this file> -i`. --rcfile REPLACES ~/.bashrc,
#          so step 1 below sources the user's startup first, then chains in cwd
#          reporting via PROMPT_COMMAND.
#   zsh:   spawned as `ZDOTDIR=<dir>/zdotdir HYPERPANES_ZDOTDIR_ORIG=$ZDOTDIR zsh -i`.
#          The bundled zdotdir/.zshenv + .zshrc chain-load the user's real zsh
#          startup (restoring their ZDOTDIR) and then source THIS file, which hooks
#          cwd reporting via a precmd hook (zsh ignores PROMPT_COMMAND).
# Strictly ADDITIVE: every step is guarded so any failure leaves a normal
# interactive shell.

# 1) bash only: --rcfile replaced ~/.bashrc, so load the user's normal startup
#    first (their prompt/aliases/env still apply). Under zsh the user's startup was
#    already loaded by the zdotdir chain before this file is sourced.
if [ -n "$BASH_VERSION" ] && [ -f "$HOME/.bashrc" ]; then
  . "$HOME/.bashrc"
fi

# 2) OSC 7 cwd reporting -----------------------------------------------------------
# Emit ESC ] 7 ; file://<percent-encoded-PWD> BEL. We use an EMPTY authority (three
# slashes) deliberately: the app accepts only empty/localhost authorities and
# rejects remote hosts, so a real (possibly SSH) hostname would be dropped. On
# git-bash $PWD is an MSYS path like /c/Users/me, which the app maps to C:\Users\me.
# The body is bash+zsh portable: ${var:offset:length}, printf -v, and "'$c" numeric
# conversion all work in both.
__hyperpanes_osc7() {
  local d="${PWD}"
  local enc="" c i hex
  for (( i=0; i<${#d}; i++ )); do
    c="${d:$i:1}"
    case "$c" in
      [a-zA-Z0-9/._~-]) enc+="$c" ;;
      *) printf -v hex '%%%02X' "'$c"; enc+="$hex" ;;
    esac
  done
  printf '\033]7;file://%s\007' "$enc"
}

if [ -n "$ZSH_VERSION" ]; then
  # zsh has no PROMPT_COMMAND — hook precmd instead. add-zsh-hook is idempotent;
  # fall back to a guarded precmd_functions append if it is unavailable.
  if autoload -Uz add-zsh-hook 2>/dev/null && add-zsh-hook precmd __hyperpanes_osc7 2>/dev/null; then
    :
  else
    case " ${precmd_functions[*]} " in
      *" __hyperpanes_osc7 "*) : ;;
      *) precmd_functions+=(__hyperpanes_osc7) ;;
    esac
  fi
else
  # bash: chain into PROMPT_COMMAND idempotently (don't double-add on re-source).
  case "$PROMPT_COMMAND" in
    *__hyperpanes_osc7*) : ;;
    *) PROMPT_COMMAND="__hyperpanes_osc7${PROMPT_COMMAND:+; $PROMPT_COMMAND}" ;;
  esac
fi

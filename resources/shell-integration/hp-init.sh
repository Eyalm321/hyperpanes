# Hyperpanes shell integration (bash / git-bash).
# Loaded via `bash --rcfile <this> -i`, which REPLACES ~/.bashrc — so source the
# user's startup first, then chain in cwd reporting. Strictly ADDITIVE: every step
# is guarded so any failure leaves a normal interactive shell.

# 1) Load the user's normal startup so their prompt/aliases/env still apply.
if [ -f "$HOME/.bashrc" ]; then
  . "$HOME/.bashrc"
fi

# 2) OSC 7 cwd reporting -----------------------------------------------------------
# Emit ESC ] 7 ; file://<percent-encoded-PWD> BEL. We use an EMPTY authority (three
# slashes) deliberately: the app accepts only empty/localhost authorities and
# rejects remote hosts, so a real (possibly SSH) hostname would be dropped. On
# git-bash $PWD is an MSYS path like /c/Users/me, which the app maps to C:\Users\me.
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

# Chain into PROMPT_COMMAND idempotently (don't double-add on re-source).
case "$PROMPT_COMMAND" in
  *__hyperpanes_osc7*) : ;;
  *) PROMPT_COMMAND="__hyperpanes_osc7${PROMPT_COMMAND:+; $PROMPT_COMMAND}" ;;
esac

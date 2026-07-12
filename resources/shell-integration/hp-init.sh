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

# 3) OSC 133 semantic prompt markers (phase 4) ------------------------------------
# Tell the app, precisely, when the shell is at a prompt (ready for input), when a
# command starts running, and what exit code each command produced — so a quietly
# thinking command is NOT misread as "idle" after 10s of silence. Format:
#   ESC ] 133 ; A BEL          prompt about to be drawn (ready for input)
#   ESC ] 133 ; C BEL          command output begins (a command is now running)
#   ESC ] 133 ; D ; <code> BEL command finished, with its exit code
# Strictly additive: a compliant 133 terminal understands A/C/D; others ignore them.
# precmd: emit D (for the command that just finished) then A. $? MUST be captured
# FIRST, before anything else clobbers it.
__hyperpanes_osc133_precmd() {
  local code=$?
  if [ -n "${__hp_ran:-}" ]; then printf '\033]133;D;%s\007' "$code"; fi
  printf '\033]133;A\007'
  __hp_ran=1
}
# preexec analogue: emit C right before a command runs.
__hyperpanes_osc133_preexec() {
  printf '\033]133;C\007'
}

# 4) hp-gui: run a GUI app OUTSIDE the pane's cgroup (Linux/systemd only) ----------
# Panes — and agent scopes inside them (e.g. a claude under agents.slice) — live in
# memory-accounted cgroups that systemd-oomd culls whole under pressure (Fedora's
# systemd-oomd-defaults applies ManagedOOMMemoryPressure=kill to EVERY user slice).
# A heavy GUI child (headed chromium, electron) launched in-pane bills its memory to
# the pane's cgroup and can take the whole pane down with it. hp-gui detaches the
# app into its own transient unit under app-graphical.slice: its memory can't
# pressure the pane's cgroup, an oomd kill hits only the app, and app and pane
# survive each other's death. Guarded: absent on non-systemd hosts (git-bash, macOS).
if [ -z "${HYPERPANES_NO_GUI_HELPER:-}" ] && command -v systemd-run >/dev/null 2>&1; then
  hp-gui() {
    if [ $# -eq 0 ]; then
      echo "usage: hp-gui <command> [args...]   # detached, own cgroup, survives pane close" >&2
      return 2
    fi
    systemd-run --user --quiet --collect --slice=app-graphical.slice --same-dir \
      --setenv=WAYLAND_DISPLAY --setenv=DISPLAY --setenv=XAUTHORITY -- "$@"
  }
fi

if [ -n "$ZSH_VERSION" ]; then
  # zsh has no PROMPT_COMMAND — hook precmd instead. add-zsh-hook is idempotent;
  # fall back to a guarded precmd_functions append if it is unavailable.
  if autoload -Uz add-zsh-hook 2>/dev/null && add-zsh-hook precmd __hyperpanes_osc7 2>/dev/null; then
    add-zsh-hook precmd __hyperpanes_osc133_precmd 2>/dev/null
    add-zsh-hook preexec __hyperpanes_osc133_preexec 2>/dev/null
  else
    case " ${precmd_functions[*]} " in
      *" __hyperpanes_osc7 "*) : ;;
      *) precmd_functions+=(__hyperpanes_osc7) ;;
    esac
    case " ${precmd_functions[*]} " in
      *" __hyperpanes_osc133_precmd "*) : ;;
      *) precmd_functions+=(__hyperpanes_osc133_precmd) ;;
    esac
    case " ${preexec_functions[*]} " in
      *" __hyperpanes_osc133_preexec "*) : ;;
      *) preexec_functions+=(__hyperpanes_osc133_preexec) ;;
    esac
  fi
else
  # bash: chain into PROMPT_COMMAND idempotently (don't double-add on re-source).
  case "$PROMPT_COMMAND" in
    *__hyperpanes_osc7*) : ;;
    *) PROMPT_COMMAND="__hyperpanes_osc7${PROMPT_COMMAND:+; $PROMPT_COMMAND}" ;;
  esac
  case "$PROMPT_COMMAND" in
    *__hyperpanes_osc133_precmd*) : ;;
    *) PROMPT_COMMAND="__hyperpanes_osc133_precmd${PROMPT_COMMAND:+; $PROMPT_COMMAND}" ;;
  esac
  # bash: the DEBUG trap is the preexec analogue. Chain idempotently, preserving any
  # existing DEBUG trap, and only fire for an interactive command (skip the trap that
  # runs for PROMPT_COMMAND itself by guarding on BASH_COMMAND not being the prompt cmd).
  if [ -z "${__hp_debug_hooked:-}" ]; then
    __hp_prev_debug_trap="$(trap -p DEBUG)"
    __hyperpanes_debug_trap() {
      case "$BASH_COMMAND" in
        __hyperpanes_osc7*|__hyperpanes_osc133_precmd*) : ;;
        *) __hyperpanes_osc133_preexec ;;
      esac
    }
    trap '__hyperpanes_debug_trap' DEBUG
    __hp_debug_hooked=1
  fi
fi

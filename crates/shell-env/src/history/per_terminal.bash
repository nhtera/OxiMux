# Per-terminal history: Up-arrow reads this terminal's own file (seeded from
# the parent pane on a split, else from your HISTFILE); each command is
# appended to it and to your HISTFILE. Off when PROMPT_COMMAND already shares
# history live (history -n/-r), under OXIMUX_PER_TERMINAL_HISTORY=0, or once
# you change or unset HISTFILE. Not yet on Git Bash (MSYSTEM): unverified there.
# The tab file is never exported: a shell started inside this one keeps to its
# own default history.
__oximux_uuid_re='^[0-9a-f]{8}(-[0-9a-f]{4}){3}-[0-9a-f]{12}$'   # bash 3.2: regex must come from a variable
if [[ "${OXIMUX_PER_TERMINAL_HISTORY:-1}" != 0 && -n "${OXIMUX_HISTORY_DIR:-}" && -n "${HISTFILE:-}" \
      && "${OXIMUX_TAB_ID:-}" =~ $__oximux_uuid_re && -z "${__oximux_hist:-}" && -z "${MSYSTEM:-}" ]]; then
  __oximux_hist_shared="$HISTFILE"
  __oximux_pc_text() {          # PROMPT_COMMAND text + bodies of functions it names
    local w; printf '%s\n' "${PROMPT_COMMAND[*]:-}"
    for w in ${PROMPT_COMMAND[*]//;/ }; do declare -F "$w" >/dev/null && declare -f "$w"; done
  }
  if ! __oximux_pc_text | grep -Eq 'history +-[a-z]*[nr]'; then
    __oximux_hist="$OXIMUX_HISTORY_DIR/$OXIMUX_TAB_ID.bash_history"
    __oximux_hist_buf="$__oximux_hist.new"
    [[ -e "$__oximux_hist" ]] || ( umask 077
      { [[ -s "$__oximux_hist_shared" ]] && command cp "$__oximux_hist_shared" "$__oximux_hist"; } \
        || printf ' \n' >"$__oximux_hist" )   # bash 3.2: history -a needs a non-empty load
    ( umask 077; : >| "$__oximux_hist_buf" )
    HISTFILE="$__oximux_hist"                 # bash loads HISTFILE after the rcfile
    export -n HISTFILE   # your rc exported it: a child bash would write (and trim) this file
    __oximux_hist_flush() {
      local __s=$?
      if [[ "${HISTFILE:-}" == "$__oximux_hist" ]] && shopt -oq history; then
        history -a "$__oximux_hist_buf"
        if [[ -s "$__oximux_hist_buf" ]]; then
          command tee -a "$__oximux_hist" <"$__oximux_hist_buf" >>"$__oximux_hist_shared"
          : >| "$__oximux_hist_buf"
        fi
      fi
      return $__s
    }
    # Also at command start (a pane closed mid-command never reaches the
    # next prompt): OxiMux's own pre-exec hook calls the flush itself; under
    # another integration, bash-preexec's hook list is the place.
    if [[ -z "${__oximux_shell_integration:-}" \
          && "$(declare -p preexec_functions 2>/dev/null)" == "declare -a"* ]]; then
      preexec_functions+=(__oximux_hist_flush)
    fi
    # bash >= 5.1 runs every element of an array PROMPT_COMMAND; older bash
    # runs only element 0, which the scalar form below prepends to.
    if [[ "$(declare -p PROMPT_COMMAND 2>/dev/null)" == "declare -a"* ]] \
       && (( BASH_VERSINFO[0] > 5 || (BASH_VERSINFO[0] == 5 && BASH_VERSINFO[1] >= 1) )); then
      PROMPT_COMMAND=(__oximux_hist_flush "${PROMPT_COMMAND[@]}")
    else
      PROMPT_COMMAND="__oximux_hist_flush${PROMPT_COMMAND:+;$PROMPT_COMMAND}"
    fi
  fi
  unset -f __oximux_pc_text
fi
unset __oximux_uuid_re

# Per-terminal history: Up-arrow reads this terminal's own file (seeded from
# the parent pane on a split, else from your HISTFILE). zsh writes that file
# per command and the new bytes are appended to your HISTFILE too. Off under
# share_history, SAVEHIST=0 or OXIMUX_PER_TERMINAL_HISTORY=0, and paused while
# HISTFILE is unset or points elsewhere.
if [[ -z ${__oximux_tab_hist:-} && ${OXIMUX_PER_TERMINAL_HISTORY:-1} != 0 \
      && -n ${OXIMUX_HISTORY_DIR:-} && -n ${HISTFILE:-} && ${SAVEHIST:-0} -gt 0 \
      && ${OXIMUX_TAB_ID:-} =~ '^[0-9a-f]{8}(-[0-9a-f]{4}){3}-[0-9a-f]{12}$' ]] \
   && [[ ! -o share_history ]] \
   && zmodload -F zsh/stat b:zstat 2>/dev/null && zmodload zsh/system 2>/dev/null; then
  typeset -g __oximux_shared_hist=$HISTFILE __oximux_savehist=$SAVEHIST
  typeset -g __oximux_tab_hist=$OXIMUX_HISTORY_DIR/$OXIMUX_TAB_ID.zsh_history
  [[ -e $__oximux_tab_hist ]] || ( umask 077
    command cp -f -- "$__oximux_shared_hist" "$__oximux_tab_hist" 2>/dev/null || : >| "$__oximux_tab_hist" )
  # zsh loads HISTFILE after this file returns. A SAVEHIST past any real
  # history keeps zsh appending to the tab file instead of rewriting it (a
  # rewrite would hide new lines from the byte-offset tee below); the file is
  # trimmed once instead, at the first prompt, to the history zsh loaded.
  HISTFILE=$__oximux_tab_hist
  SAVEHIST=1000000000
  typeset -gi __oximux_hist_off=-1 __oximux_hist_ino=0 __oximux_hist_away=0
  # Both hooks run under your options; `emulate -L zsh` keeps sh_word_split /
  # ksh_arrays from mangling them ($? is read first, emulate would reset it).
  __oximux_hist_mark() {               # zstat -A: [2] inode, [8] size
    emulate -L zsh
    local -a st
    zstat -A st -- "$__oximux_tab_hist" 2>/dev/null || st=()
    __oximux_hist_ino=${st[2]:-0} __oximux_hist_off=${st[8]:-0}
  }
  __oximux_hist_tee() {
    local __s=$?
    emulate -L zsh
    if [[ -o share_history ]]; then
      # Turned on later (a deferred plugin): hand history back for good.
      HISTFILE=$__oximux_shared_hist
      (( SAVEHIST == 1000000000 )) && SAVEHIST=$__oximux_savehist
      preexec_functions=(${preexec_functions:#__oximux_hist_tee})
      precmd_functions=(${precmd_functions:#__oximux_hist_tee})
      return $__s
    fi
    if [[ ${HISTFILE:-} != "$__oximux_tab_hist" ]]; then
      # You unset or changed HISTFILE (incognito, `fc -p`): copy nothing
      # until it points back here.
      (( SAVEHIST == 1000000000 )) && SAVEHIST=$__oximux_savehist
      __oximux_hist_away=1
      return $__s
    fi
    if (( __oximux_hist_away )); then    # back (`fc -P`): resync, carry on
      __oximux_hist_away=0
      (( SAVEHIST > 0 )) && SAVEHIST=1000000000
      (( __oximux_hist_off >= 0 )) && __oximux_hist_mark
    fi
    if (( SAVEHIST != 1000000000 )); then  # your SAVEHIST changed after your rc
      __oximux_savehist=$SAVEHIST
      (( SAVEHIST > 0 )) && SAVEHIST=1000000000
    fi
    if (( __oximux_hist_off < 0 )); then  # first prompt: trim, then start teeing
      fc -W
      __oximux_hist_mark
      return $__s
    fi
    local -a st
    zstat -A st -- "$__oximux_tab_hist" 2>/dev/null || return $__s
    local -i sz=$st[8]
    if (( st[2] != __oximux_hist_ino || sz < __oximux_hist_off )); then
      __oximux_hist_mark                   # rewritten under us (fc -W): resync
    elif (( sz > __oximux_hist_off )); then
      local fd buf
      if sysopen -r -u fd -- "$__oximux_tab_hist"; then
        sysseek -u $fd $__oximux_hist_off
        sysread -i $fd -s $(( sz - __oximux_hist_off )) buf
        exec {fd}<&-
        if sysopen -a -o creat -m 600 -u fd -- "$__oximux_shared_hist"; then
          syswrite -o $fd -- "$buf"; exec {fd}>&-
        fi
      fi
      __oximux_hist_off=$sz
    fi
    return $__s
  }
  preexec_functions+=(__oximux_hist_tee)
  precmd_functions+=(__oximux_hist_tee)
fi

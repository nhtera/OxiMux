# Per-terminal history (fish >= 4.0): this terminal keeps its own history
# session, seeded from the parent pane on a split, else from yours; each
# command fish keeps is also appended to your session. Off in private mode,
# with an empty fish_history, or under OXIMUX_PER_TERMINAL_HISTORY=0.
if test "$OXIMUX_PER_TERMINAL_HISTORY" != 0 -a -n "$OXIMUX_HISTORY_DIR"
  and not set -q __oximux_hist; and not set -q fish_private_mode
  and string match -qr '^[0-9a-f]{8}(-[0-9a-f]{4}){3}-[0-9a-f]{12}$' -- "$OXIMUX_TAB_ID"
  and string match -qr '^([4-9]|[1-9][0-9])\.' -- $version
  and begin; not set -q fish_history; or test -n "$fish_history"; end
  set -g __oximux_hist_shared fish
  set -q fish_history; and test "$fish_history" != default; and set __oximux_hist_shared $fish_history
  # Where fish keeps history: fixed at startup, so an XDG_DATA_HOME set later
  # in config.fish does not move it.
  set -l d $__fish_user_data_dir
  test -n "$d"; or set d (set -q XDG_DATA_HOME; and echo $XDG_DATA_HOME; or echo $HOME/.local/share)/fish
  set -g __oximux_hist oximux_(string replace -a -- - _ $OXIMUX_TAB_ID)
  if not test -e $d/{$__oximux_hist}_history
    test -f $d/{$__oximux_hist_shared}_history
    and cp $d/{$__oximux_hist_shared}_history $d/{$__oximux_hist}_history
  end
  printf '%s\n' $d/{$__oximux_hist}_history >$OXIMUX_HISTORY_DIR/$OXIMUX_TAB_ID.fish_path
  set -g fish_history $__oximux_hist
  # fish adds a command to history before running it; mirror that into your
  # session with the same filter fish applied.
  function __oximux_hist_global --on-event fish_preexec
    test "$fish_history" = $__oximux_hist; or return
    test -n "$fish_private_mode"; and return  # private mode turned on later
    if functions -q fish_should_add_to_history
      fish_should_add_to_history $argv[1]; or return
    else
      string match -q ' *' -- $argv[1]; and return
    end
    set -g fish_history $__oximux_hist_shared
    builtin history append -- $argv[1]
    builtin history save
    set -g fish_history $__oximux_hist
  end
end

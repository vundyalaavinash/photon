# photon: hidden completion helper behind the block terminal's Tab (crates/photon_terminal/src/completion.rs).
#
# Completion widgets only run inside ZLE, which needs a terminal, so this file has two roles:
#  - the driver (PHOTON_COMPLETER=1): reads requests on stdin and runs a second zsh in a zpty;
#  - the capture shell (PHOTON_COMPLETER_PTY=1, inside that zpty): loads the user's config like any
#    interactive shell, then answers each request by running completion with `compadd` captured.
#
# Request (one line):  <cwd>\t<buffer>, both escaped as \xHH for `\`, `;`, BEL, ESC and newline.
# Response:            <insert>\t<description> lines, then an empty line.
# Once the capture shell has loaded, the driver prints \x1dready on a line of its own.

if [[ -z $PHOTON_COMPLETER_PTY ]]; then
  __photon_complete_driver() {
    emulate -L zsh
    unset HISTFILE
    umask 077
    zmodload zsh/zpty || return 1
    export PHOTON_COMPLETER_PTY=1
    export PHOTON_COMPLETER_REQUEST=${TMPDIR:-/tmp}/photon-complete.$$
    zpty photon "exec ${(q)ZSH_ARGZERO:-zsh} -i" || return 1
    # The capture shell announces itself once ZLE is reading keys; anything sent earlier
    # could be eaten by the user's startup files.
    local out request line
    zpty -r photon out $'*\x1dready*' || return 1
    # Tell Photon it can send requests now; until then it completes paths itself.
    print -r -- $'\x1dready'
    while IFS= read -r request; do
      print -r -- $request >| $PHOTON_COMPLETER_REQUEST
      zpty -w -n photon $'\e[photon-complete~'
      zpty -r photon out $'*\x1e*' || return 1
      for line in ${(f)out}; do
        [[ $line == *$'\x1f'* ]] && print -r -- ${${line#*$'\x1f'}%$'\r'}
      done
      print
    done
  }
  __photon_complete_driver
  zpty -d photon 2>/dev/null
  [[ -n $PHOTON_COMPLETER_REQUEST ]] && rm -f -- $PHOTON_COMPLETER_REQUEST
  exit
fi

unset HISTFILE
PROMPT='' RPROMPT=''
LISTMAX=1000000
zmodload -F zsh/files b:zf_rm 2>/dev/null
(( $+functions[compdef] )) || { autoload -Uz compinit && compinit -i -d $PHOTON_ZDOTDIR/.zcompdump }

zstyle ':completion:*' list-grouped false
zstyle ':completion:*' insert-tab false
zstyle ':completion:*' list-separator ''

# Mostly from zsh-capture-completion (MIT): record every match `compadd` is given, then let the
# real call run too so completers that check the match count behave as at a normal prompt.
compadd() {
  setopt localoptions norcexpandparam extendedglob
  if [[ ${@[1,(i)(-|--)]} == *-(O|A|D)\ * ]]; then
    builtin compadd "$@"
    return $?
  fi
  typeset -a __hits __dscr __tmp
  # -d may be combined with other flags, as in -ld.
  if (( $@[(I)-[[:alpha:]]#d] )); then
    __tmp=${@[$[${@[(I)-[[:alpha:]]#d]}+1]]}
    if [[ $__tmp == \(* ]]; then
      eval "__dscr=$__tmp"
    else
      __dscr=( "${(@P)__tmp}" )
    fi
  fi
  builtin compadd -A __hits -D __dscr "$@"
  builtin compadd "$@"
  local result=$?
  (( $#__hits )) || return $result
  typeset -A apre hpre hsuf asuf
  zparseopts -E P:=apre p:=hpre S:=asuf s:=hsuf
  integer dirsuf=0 quoted=0
  [[ -z $hsuf && "${${@//-default-/}% -# *}" == *-[[:alnum:]]#f* ]] && dirsuf=1
  (( ${@[(I)-[[:alpha:]]#Q[[:alpha:]]#]} )) && quoted=1
  local hit dsuf dscr i
  for i in {1..$#__hits}; do
    hit=$__hits[$i]
    (( quoted )) || hit=${(q)hit}
    (( dirsuf )) && [[ -d $__hits[$i] && $hsuf$asuf != */ ]] && dsuf=/ || dsuf=
    (( $#__dscr >= i )) && dscr=${${__dscr[$i]}##${(b)__hits[$i]} #} || dscr=
    [[ $dscr == $__hits[$i] ]] && dscr=
    print -r -- $'\x1f'"$IPREFIX$apre$hpre$hit$dsuf$hsuf$asuf"$'\t'"$dscr" >$TTY
  done
  return $result
}

__photon_no_list() {
  compstate[list]=''
  compstate[insert]=''
}

__photon_capture() {
  local request
  request=$(<$PHOTON_COMPLETER_REQUEST)
  zf_rm -f -- $PHOTON_COMPLETER_REQUEST 2>/dev/null || command rm -f -- $PHOTON_COMPLETER_REQUEST
  local cwd=${request%%$'\t'*} buffer=${request#*$'\t'}
  {
    cd -q -- ${(g::)cwd} 2>/dev/null
    BUFFER=${(g::)buffer}
    CURSOR=$#BUFFER
    comppostfuncs=(__photon_no_list)
    zle __photon_complete_word
  } always {
    print -rn -- $'\x1e\n' >$TTY
    BUFFER=''
    CURSOR=0
  }
}

__photon_ready() {
  print -rn -- $'\x1dready\n' >$TTY
}

zle -C __photon_complete_word complete-word _main_complete
zle -N __photon_capture
bindkey $'\e[photon-complete~' __photon_capture
autoload -Uz add-zle-hook-widget
add-zle-hook-widget line-init __photon_ready

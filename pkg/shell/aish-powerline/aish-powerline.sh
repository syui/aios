# aish-powerline: aish の powerline のプロンプト
#   ~/.aishrc に: source /usr/share/aish/plugins/aish-powerline/aish-powerline.sh
# アイコンと区切りは aifont (Nerd Font と同じ位置) の文字。端末のフォントが要る
# ユーザーの色: ふつうは黄、ssh でつないでいるときは水色。git のディレクトリではブランチも出す

_e=$(printf '\033')

_git_branch() {
  _d=$PWD
  while [ -n "$_d" ]; do
    if [ -f "$_d/.git/HEAD" ]; then
      read -r _h < "$_d/.git/HEAD"
      case $_h in
        "ref: refs/heads/"*) echo "${_h#ref: refs/heads/}" ;;
        *) echo "$_h" | cut -c1-7 ;;
      esac
      return
    fi
    _d=${_d%/*}
  done
}

_user_icon() {
  case $1 in
    ai|ai.*) echo '' ;;
    syui|syui.*) echo '' ;;
    *) echo '❯' ;;
  esac
}

_powerline_prompt() {
  _c=33
  [ -n "$SSH_CONNECTION" ] && _c=36
  case $PWD in
    "$HOME") _dir='~' ;;
    "$HOME"/*) _dir="~${PWD#"$HOME"}" ;;
    *) _dir=$PWD ;;
  esac
  _git=$(_git_branch)
  echo -n "$_e[${_c};40m $(_user_icon "$USER") $_e[30;48;5;234m"
  echo -n "$_e[33;48;5;234m $USER $_e[38;5;234;48;5;236m"
  echo -n "$_e[37;48;5;236m $_dir "
  if [ -n "$_git" ]; then
    echo -n "$_e[38;5;236;48;5;234m$_e[36;48;5;234m  $_git $_e[0;38;5;234m"
  else
    echo -n "$_e[0;38;5;236m"
  fi
  echo -n "$_e[0m "
}

PS1='$(_powerline_prompt)'

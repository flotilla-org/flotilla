//! Git CLI staging for generation-pinned agent skill sources.

use std::path::Path;

use crate::providers::{ChannelLabel, CommandRunner};

pub const STAGE_DIAGNOSTIC_PREFIX: &str = "flotilla-stage-skills: ";
pub const STAGE_RETRYABLE_PREFIX: &str = "flotilla-stage-skills-retryable:";

const STAGE_SCRIPT: &str = r#"set -eu
diagnostic_prefix=$0
shift
manifest=$1
destination=$2
cleanup_tokens=$3
cache_root=$4
shift 4
export GIT_CONFIG_GLOBAL=/dev/null GIT_TERMINAL_PROMPT=0
umask 077
staged="${destination}.flotilla-staging.$$"
sources="${destination}.flotilla-sources.$$"
token_files=
succeeded=false
retrying=false
cache_tmp=
cleanup() {
  rm -rf "$staged" "$sources"
  if [ -n "$cache_tmp" ]; then rm -rf "$cache_tmp"; fi
  if [ "$retrying" != true ] && { [ "$cleanup_tokens" = true ] || [ "$succeeded" != true ]; }; then
    for token_file in $token_files; do rm -f "$token_file"; done
  fi
}
print_git_stderr() {
  token=
  if [ -n "$token_file" ] && [ -s "$token_file" ]; then
    IFS= read -r token <"$token_file" || :
  fi
  while IFS= read -r line || [ -n "$line" ]; do
    if [ -n "$token" ]; then
      printf '%s\n' "$line" | awk -v token="$token" '{
        while ((position = index($0, token)) > 0) {
          printf "%s[redacted credential]", substr($0, 1, position - 1)
          $0 = substr($0, position + length(token))
        }
        print
      }' >&2
    else
      printf '%s\n' "$line" >&2
    fi
  done <"$1"
}
trap cleanup EXIT HUP INT TERM
mkdir -p "$staged" "$sources" "$cache_root"
while [ "$#" -gt 0 ]; do
  name=$1
  repository=$2
  revision=$3
  token_file=$4
  credential=$5
  path_count=$6
  shift 6
  checkout="$sources/$name"
  paths_file="$sources/$name.paths"
  sparse_file="$sources/$name.sparse"
  : >"$paths_file"
  : >"$sparse_file"
  while [ "$path_count" -gt 0 ]; do
    printf '%s\n' "$1" >>"$paths_file"
    printf '/%s/\n' "$1" >>"$sparse_file"
    shift
    path_count=$((path_count - 1))
  done
  if [ -n "$token_file" ]; then token_files="$token_files $token_file"; fi
  if [ -n "$credential" ] && { [ -z "$token_file" ] || [ ! -s "$token_file" ]; }; then
    echo "${diagnostic_prefix}skill source $name credential $credential is unavailable at pinned revision $revision" >&2
    exit 1
  fi
  cache="$cache_root/$name-$revision"
  if [ -f "$cache/.flotilla-ready" ] && [ "$(cat "$cache/.flotilla-repository")" = "$repository" ] && cmp -s "$paths_file" "$cache/.flotilla-paths"; then
    cp -R "$cache" "$checkout"
  else
    rm -rf "$cache"
    if ! git -C "$sources" init --quiet "$name" >/dev/null 2>"$sources/git.stderr"; then
      print_git_stderr "$sources/git.stderr"
      exit 1
    fi
    print_git_stderr "$sources/git.stderr"
    if ! git -C "$checkout" remote add origin "$repository" 2>"$sources/git.stderr"; then
      print_git_stderr "$sources/git.stderr"
      exit 1
    fi
    print_git_stderr "$sources/git.stderr"
    if ! git -C "$checkout" sparse-checkout set --no-cone --stdin <"$sparse_file" >/dev/null 2>"$sources/git.stderr"; then
      print_git_stderr "$sources/git.stderr"
      exit 1
    fi
    print_git_stderr "$sources/git.stderr"
    if [ -n "$token_file" ]; then
      export GITHUB_TOKEN_FILE="$token_file"
      helper='!f() { [ "$1" = get ] || exit 0; printf "username=x-access-token\npassword="; cat "$GITHUB_TOKEN_FILE"; printf "\n"; }; f'
      if ! git -C "$checkout" config credential.helper "$helper" 2>"$sources/git.stderr"; then
        print_git_stderr "$sources/git.stderr"
        exit 1
      fi
      print_git_stderr "$sources/git.stderr"
    fi
    if [ -n "$token_file" ]; then
      if git -C "$checkout" fetch --quiet --depth=1 --filter=blob:none --no-tags origin "$revision" >"$sources/fetch.stdout" 2>"$sources/fetch.stderr"; then code=0; else code=$?; fi
    else
      if git -C "$checkout" -c credential.helper= fetch --quiet --depth=1 --filter=blob:none --no-tags origin "$revision" >"$sources/fetch.stdout" 2>"$sources/fetch.stderr"; then code=0; else code=$?; fi
    fi
    if [ "$code" -ne 0 ]; then
      if grep -Eiq 'not our ref|could not find remote ref|unadvertised object' "$sources/fetch.stderr"; then
        disposition='pinned revision does not exist'
      elif grep -Eiq 'authentication failed|authorization failed|HTTP[^[:space:]]*[[:space:]]+(401|403)|requested URL returned error: (401|403)|401 Unauthorized|403 Forbidden|permission denied|access denied' "$sources/fetch.stderr"; then
        disposition='authentication or authorization failed'
      else
        disposition='fetch failed'
        retrying=true
        echo 'flotilla-stage-skills-retryable:' >&2
      fi
      echo "${diagnostic_prefix}skill source $name $disposition at pinned revision $revision; command: git -C $checkout fetch --quiet --depth=1 --filter=blob:none --no-tags origin $revision; exit code: $code; stderr:" >&2
      print_git_stderr "$sources/fetch.stderr"
      exit 1
    fi
    if ! fetched_revision=$(git -C "$checkout" rev-parse FETCH_HEAD 2>"$sources/git.stderr"); then
      print_git_stderr "$sources/git.stderr"
      exit 1
    fi
    print_git_stderr "$sources/git.stderr"
    test "$fetched_revision" = "$revision" || { echo "${diagnostic_prefix}skill source $name fetch returned the wrong pinned revision $revision" >&2; exit 1; }
    if git -C "$checkout" checkout --quiet --detach FETCH_HEAD >"$sources/checkout.stdout" 2>"$sources/checkout.stderr"; then code=0; else code=$?; fi
    if [ "$code" -ne 0 ]; then
      if grep -Eiq 'authentication failed|authorization failed|HTTP[^[:space:]]*[[:space:]]+(401|403)|requested URL returned error: (401|403)|401 Unauthorized|403 Forbidden|permission denied|access denied' "$sources/checkout.stderr"; then
        disposition='authentication or authorization failed'
      else
        disposition='checkout failed'
        retrying=true
        echo 'flotilla-stage-skills-retryable:' >&2
      fi
      echo "${diagnostic_prefix}skill source $name $disposition at pinned revision $revision; command: git -C $checkout checkout --quiet --detach FETCH_HEAD; exit code: $code; stderr:" >&2
      print_git_stderr "$sources/checkout.stderr"
      exit 1
    fi
    print_git_stderr "$sources/checkout.stderr"
    cache_tmp="$cache.tmp.$$"
    rm -rf "$cache_tmp"
    cp -R "$checkout" "$cache_tmp"
    rm -rf "$cache_tmp/.git"
    printf '%s' "$repository" >"$cache_tmp/.flotilla-repository"
    cp "$paths_file" "$cache_tmp/.flotilla-paths"
    touch "$cache_tmp/.flotilla-ready"
    mv "$cache_tmp" "$cache"
    cache_tmp=
    if [ -n "$token_file" ]; then unset GITHUB_TOKEN_FILE; fi
  fi
  while IFS= read -r path; do
    if [ ! -d "$checkout/$path" ]; then
      echo "${diagnostic_prefix}skill source $name declared path $path is missing at pinned revision $revision" >&2
      exit 1
    fi
    find "$checkout/$path" -type f -name SKILL.md >"$sources/skill-files"
    if [ ! -s "$sources/skill-files" ]; then
      echo "${diagnostic_prefix}skill source $name declared path $path has no SKILL.md at pinned revision $revision" >&2
      exit 1
    fi
    while IFS= read -r skill_file; do
      skill_dir=${skill_file%/SKILL.md}
      skill_name=${skill_dir##*/}
      target="$staged/$skill_name"
      if [ -e "$target" ]; then
        echo "${diagnostic_prefix}duplicate skill name $skill_name from $repository" >&2
        exit 1
      fi
      mkdir -p "$target"
      cp -R "$skill_dir"/. "$target"/
    done <"$sources/skill-files"
  done <"$paths_file"
done
cp "$manifest" "$staged/.flotilla-sources.json"
rm -rf "$destination"
mv "$staged" "$destination"
succeeded=true
cleanup
trap - EXIT HUP INT TERM"#;

pub async fn stage_git_skill_sources(runner: &dyn CommandRunner, args: &[String]) -> Result<String, String> {
    // The script reads the prefix from $0, then shifts past args[0], the stage command identifier.
    let command_args = ["-c", STAGE_SCRIPT, STAGE_DIAGNOSTIC_PREFIX].into_iter().chain(args.iter().map(String::as_str)).collect::<Vec<_>>();
    runner.run("sh", &command_args, Path::new("/"), &ChannelLabel::Default).await
}

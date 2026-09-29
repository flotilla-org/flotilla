//! Git CLI staging for generation-pinned agent skill sources.

use std::path::Path;

use crate::providers::{ChannelLabel, CommandRunner};

pub const STAGE_DIAGNOSTIC_PREFIX: &str = "flotilla-stage-skills: ";

const STAGE_SCRIPT: &str = r#"set -eu
diagnostic_prefix=$0
shift
manifest=$1
destination=$2
cleanup_tokens=$3
cache_root=$4
shift 4
export GIT_CONFIG_GLOBAL=/dev/null GIT_TERMINAL_PROMPT=0
staged="${destination}.flotilla-staging.$$"
sources="${destination}.flotilla-sources.$$"
token_files=
succeeded=false
lock=
cache_tmp=
cleanup() {
  rm -rf "$staged" "$sources"
  if [ -n "$cache_tmp" ]; then rm -rf "$cache_tmp"; fi
  if [ -n "$lock" ]; then rmdir "$lock" 2>/dev/null || true; fi
  if [ "$cleanup_tokens" = true ] || [ "$succeeded" != true ]; then
    for token_file in $token_files; do rm -f "$token_file"; done
  fi
}
print_git_stderr() {
  while IFS= read -r line || [ -n "$line" ]; do
    if [ -n "$token_file" ] && [ -s "$token_file" ] && printf '%s\n' "$line" | grep -F -q -f "$token_file"; then
      echo '[redacted credential-bearing git stderr line]' >&2
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
  cache="$cache_root/$name-$revision"
  lock="$cache.lock"
  waits=0
  until mkdir "$lock" 2>/dev/null; do
    waits=$((waits + 1))
    if [ "$waits" -ge 200 ]; then
      echo "${diagnostic_prefix}skill source $name cache lock timed out at pinned revision $revision" >&2
      exit 1
    fi
    sleep 0.1
  done
  if [ -f "$cache/.flotilla-ready" ] && [ "$(cat "$cache/.flotilla-repository")" = "$repository" ] && cmp -s "$paths_file" "$cache/.flotilla-paths"; then
    cp -R "$cache" "$checkout"
  else
    rm -rf "$cache"
    git -C "$sources" init --quiet "$name" >/dev/null
    git -C "$checkout" remote add origin "$repository"
    git -C "$checkout" sparse-checkout set --no-cone --stdin <"$sparse_file" >/dev/null
    if [ -n "$token_file" ]; then
      export GITHUB_TOKEN_FILE="$token_file"
      helper='!f() { [ "$1" = get ] || exit 0; printf "username=x-access-token\npassword="; cat "$GITHUB_TOKEN_FILE"; printf "\n"; }; f'
      git -C "$checkout" config credential.helper "$helper"
    fi
    attempt=0
    while :; do
      attempt=$((attempt + 1))
      if [ -n "$token_file" ]; then
        if git -C "$checkout" fetch --quiet --depth=1 --filter=blob:none --no-tags origin "$revision" >"$sources/fetch.stdout" 2>"$sources/fetch.stderr"; then break; else code=$?; fi
      else
        if git -C "$checkout" -c credential.helper= fetch --quiet --depth=1 --filter=blob:none --no-tags origin "$revision" >"$sources/fetch.stdout" 2>"$sources/fetch.stderr"; then break; else code=$?; fi
      fi
      if grep -Eiq 'not our ref|could not find remote ref|unadvertised object' "$sources/fetch.stderr"; then disposition='pinned revision does not exist'; else disposition='fetch failed'; fi
      if [ "$disposition" = 'pinned revision does not exist' ] || [ "$attempt" -ge 3 ]; then
        echo "${diagnostic_prefix}skill source $name $disposition at pinned revision $revision; command: git -C $checkout fetch --quiet --depth=1 --filter=blob:none --no-tags origin $revision; exit code: $code; stderr:" >&2
        print_git_stderr "$sources/fetch.stderr"
        exit 1
      fi
      sleep "$attempt"
    done
    test "$(git -C "$checkout" rev-parse FETCH_HEAD)" = "$revision" || { echo "${diagnostic_prefix}skill source $name fetch returned the wrong pinned revision $revision" >&2; exit 1; }
    attempt=0
    while :; do
      attempt=$((attempt + 1))
      if git -C "$checkout" checkout --quiet --detach FETCH_HEAD >"$sources/checkout.stdout" 2>"$sources/checkout.stderr"; then break; else code=$?; fi
      if [ "$attempt" -ge 3 ]; then
        echo "${diagnostic_prefix}skill source $name checkout failed at pinned revision $revision; command: git -C $checkout checkout --quiet --detach FETCH_HEAD; exit code: $code; stderr:" >&2
        print_git_stderr "$sources/checkout.stderr"
        exit 1
      fi
      sleep "$attempt"
    done
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
  rmdir "$lock"
  lock=
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

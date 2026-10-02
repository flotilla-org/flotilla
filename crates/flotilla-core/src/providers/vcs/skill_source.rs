//! Git CLI staging for generation-pinned agent skill sources.

use std::path::Path;

use crate::providers::{ChannelLabel, CommandRunner};

pub const STAGE_DIAGNOSTIC_PREFIX: &str = "flotilla-stage-skills: ";
pub const STAGE_RETRYABLE_PREFIX: &str = "flotilla-stage-skills-retryable:";
pub const STAGE_SOURCE_PREFIX: &str = "flotilla-stage-skills-source: ";

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
token_markers=
succeeded=false
retrying=false
cache_tmp=
for abandoned in "${destination}.flotilla-staging."* "${destination}.flotilla-sources."*; do
  [ -d "$abandoned" ] && [ ! -L "$abandoned" ] || continue
  owner=${abandoned##*.}
  case "$owner" in *[!0-9]*|'') continue ;; esac
  # Staging shells and this pass share a user and PID namespace. A reused PID
  # delays reclamation; an EPERM response must not remove another user's work.
  if signal_error=$(kill -0 "$owner" 2>&1); then continue; fi
  case "$signal_error" in *[Pp]ermiss*|*permitted*) continue ;; esac
  rm -rf -- "$abandoned"
done
cleanup() {
  rm -rf "$staged" "$sources"
  if [ -n "$cache_tmp" ]; then rm -rf "$cache_tmp"; fi
  for marker in $token_markers; do rm -f -- "$marker"; done
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
      printf '%s\n' "$line" | TOKEN_TO_REDACT="$token" awk '{
        token = ENVIRON["TOKEN_TO_REDACT"]
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
is_auth_failure() {
  grep -Eiq 'authentication failed|authorization failed|could not read (Username|Password)|HTTP[^[:space:]]*[[:space:]]+(401|403)|requested URL returned error: (401|403)|401 Unauthorized|403 Forbidden|Permission denied \(publickey\)|remote:.*(permission|access) denied' "$1"
}
trap cleanup EXIT HUP INT TERM
mkdir -p "$staged" "$sources" "$cache_root"
while [ "$#" -gt 0 ]; do
  name=$1
  repository=$2
  revision=$3
  token_file=$4
  credential=$5
  echo "flotilla-stage-skills-source: $name" >&2
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
  if [ -n "$token_file" ]; then
    token_files="$token_files $token_file"
    marker="$token_file.in-use.$$"
    : >"$marker"
    token_markers="$token_markers $marker"
  fi
  if [ -n "$credential" ]; then
    if [ -z "$token_file" ] || [ ! -s "$token_file" ]; then
      echo "${diagnostic_prefix}skill source $name credential $credential is unavailable at pinned revision $revision" >&2
      exit 1
    fi
    # Hold a private snapshot through fetch and lazy checkout. The supplied
    # file could disappear after the check above but before Git asks for it.
    snapshot="$sources/$name.token"
    if ! cp -- "$token_file" "$snapshot" 2>/dev/null || [ ! -s "$snapshot" ]; then
      echo "${diagnostic_prefix}skill source $name credential $credential is unavailable at pinned revision $revision" >&2
      exit 1
    fi
    IFS= read -r token <"$snapshot" || :
    if [ -z "$token" ]; then
      echo "${diagnostic_prefix}skill source $name credential $credential is unavailable at pinned revision $revision" >&2
      exit 1
    fi
    token_file=$snapshot
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
      helper='!f() { [ "$1" = get ] || exit 0; [ -s "$GITHUB_TOKEN_FILE" ] || exit 1; IFS= read -r token <"$GITHUB_TOKEN_FILE" || :; [ -n "$token" ] || exit 1; printf "username=x-access-token\npassword=%s\n" "$token"; }; f'
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
      elif is_auth_failure "$sources/fetch.stderr"; then
        disposition='authentication or authorization failed'
      else
        disposition='fetch failed'
        # Rust owns final-failure cleanup; preserve this token for its next attempt.
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
      if is_auth_failure "$sources/checkout.stderr"; then
        disposition='authentication or authorization failed'
      else
        disposition='checkout failed'
        # Rust owns final-failure cleanup; preserve this token for its next attempt.
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

#[cfg(test)]
mod tests {
    use std::{fs, process::Command};

    use super::*;

    #[test]
    fn staging_reaps_dead_pid_directories_without_disturbing_a_live_stage() {
        let temp = tempfile::tempdir().expect("tempdir");
        let destination = temp.path().join("skills");
        let dead_pid = 999_999_999;
        let live_pid = std::process::id();
        for suffix in ["flotilla-staging", "flotilla-sources"] {
            fs::create_dir(format!("{}.{}.{}", destination.display(), suffix, dead_pid)).expect("dead stage dir");
            fs::create_dir(format!("{}.{}.{}", destination.display(), suffix, live_pid)).expect("live stage dir");
        }
        let output = Command::new("sh")
            .arg("-c")
            .arg(STAGE_SCRIPT)
            .arg(STAGE_DIAGNOSTIC_PREFIX)
            .arg("flotilla-stage-skills")
            .arg(temp.path().join("missing-manifest"))
            .arg(&destination)
            .arg("false")
            .arg(temp.path().join("cache"))
            .output()
            .expect("run stage shell");
        assert!(!output.status.success(), "missing manifest should stop after cleanup");
        for suffix in ["flotilla-staging", "flotilla-sources"] {
            assert!(!std::path::Path::new(&format!("{}.{}.{}", destination.display(), suffix, dead_pid)).exists());
            assert!(std::path::Path::new(&format!("{}.{}.{}", destination.display(), suffix, live_pid)).exists());
        }
    }
}

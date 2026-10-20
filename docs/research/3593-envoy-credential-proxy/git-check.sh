#!/bin/bash
set -euo pipefail
export GIT_CONFIG_NOSYSTEM=1 GIT_TERMINAL_PROMPT=0
export GIT_CONFIG_COUNT=4
export GIT_CONFIG_KEY_0=http.proxy GIT_CONFIG_VALUE_0=http://127.0.0.1:18080
export GIT_CONFIG_KEY_1=http.sslCAInfo GIT_CONFIG_VALUE_1=/tmp/envoy-spike/ca.pem
export GIT_CONFIG_KEY_2=credential.helper GIT_CONFIG_VALUE_2=/tmp/envoy-spike/credential-helper
export GIT_CONFIG_KEY_3=http.version GIT_CONFIG_VALUE_3=HTTP/1.1
export NO_PROXY= no_proxy=
root=/tmp/envoy-spike
if [ "$1" = small ]; then
  git init "$root/source"
  git -C "$root/source" config user.name Test
  git -C "$root/source" config user.email test@example.invalid
  echo initial > "$root/source/hello"
  git -C "$root/source" add hello
  git -C "$root/source" commit -m initial
  git -C "$root/source" remote add origin https://git.test/repo.git
  git -C "$root/source" push origin HEAD:master
  git clone https://git.test/repo.git "$root/clone"
  echo second >> "$root/source/hello"
  git -C "$root/source" commit -am second
  git -C "$root/source" push origin HEAD:master
  git -C "$root/clone" fetch origin
elif [ "$1" = large ]; then
  dd if=/dev/urandom of="$root/source/random.bin" bs=1M count=500 status=none
  git -C "$root/source" add random.bin
  git -C "$root/source" commit -m '500 MiB incompressible blob'
  git -C "$root/source" push origin HEAD:master
elif [ "$1" = push ]; then
  git -C "$root/source" push origin HEAD:master
fi

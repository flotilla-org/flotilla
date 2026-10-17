#!/usr/bin/env bash
# Install the rehearsal fake Forgejo service (#3374) as a systemd service on
# one rehearsal host, with a rehearsal-only certificate authority, then print
# what every rehearsal host needs: the CA, the token files and host-local
# daemon.toml provider bindings.
#
# Run as root on the host that will serve the fake. Re-running keeps the CA,
# the forge tokens and the admin token, reissues the server certificate,
# rewrites the config and restarts the service. It never touches flotilla's
# stores; copied Forge and Repository records stay unchanged.
#
# Usage:
#   install-fake-forge.sh --binary PATH --host NAME [--ip ADDR] \
#     --forge MOUNT=TOKEN_LABEL=SERVICE_URL [--forge ...] \
#     [--seed FILE] [--port 3443] [--admin-port 3444] [--bind 0.0.0.0] \
#     [--auto-merge-after SECONDS] [--login rehearsal-crew] \
#     [--client-token-dir /etc/flotilla-rehearsal/tokens] \
#     [--prefix /etc/flotilla-fake-forge | --stage DIR]
#
# Example (silo rehearsal; rehearsal-feta serves the fake):
#   flotilla-fake-forge seed --repositories kiwi.json --repositories feta.json \
#     --repositories udder.json --forge https://github.com=github \
#     --forge https://forgejo.lab.flotilla.work=lab --output seed.json
#   install-fake-forge.sh --binary ./flotilla-fake-forge --host rehearsal-feta --ip 10.77.0.12 \
#     --forge github=github=https://github.com \
#     --forge lab=lab=https://forgejo.lab.flotilla.work \
#     --seed seed.json --auto-merge-after 120
#
# MOUNT is the fake's path and seed name. TOKEN_LABEL labels its token file;
# it does not replace a Forge resource. SERVICE_URL is the original canonical
# HTTPS service, including any installation prefix. Legacy HOST[,HOST...] is
# accepted too; its first host supplies an unprefixed HTTPS service URL.
#
# --stage DIR writes the CA, certificate, tokens, config and bindings under DIR
# and prints the same instructions, without root, a service user, systemd or a
# readiness check. Tests use it; an operator can review the output.
set -euo pipefail

usage() { sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//'; exit "${1:-2}"; }
die() { echo "install-fake-forge: $*" >&2; exit 1; }

binary="" host="" ip="" seed="" port=3443 admin_port=3444 bind=0.0.0.0 auto_merge="" login=rehearsal-crew
prefix=/etc/flotilla-fake-forge stage="" client_tokens=/etc/flotilla-rehearsal/tokens
forges=()
while (($#)); do
  case "$1" in
    --binary) binary=${2:?}; shift 2 ;;
    --host) host=${2:?}; shift 2 ;;
    --ip) ip=${2:?}; shift 2 ;;
    --forge) forges+=("${2:?}"); shift 2 ;;
    --seed) seed=${2:?}; shift 2 ;;
    --port) port=${2:?}; shift 2 ;;
    --admin-port) admin_port=${2:?}; shift 2 ;;
    --bind) bind=${2:?}; shift 2 ;;
    --auto-merge-after) auto_merge=${2:?}; shift 2 ;;
    --login) login=${2:?}; shift 2 ;;
    --client-token-dir) client_tokens=${2:?}; shift 2 ;;
    --prefix) prefix=${2:?}; shift 2 ;;
    --stage) stage=${2:?}; shift 2 ;;
    -h|--help) usage 0 ;;
    *) echo "install-fake-forge: unknown argument $1" >&2; usage ;;
  esac
done

if [[ -z $stage ]]; then
  [[ $EUID -eq 0 ]] || die "run as root (or use --stage DIR to only write the files)"
  [[ -n $binary && -x $binary ]] || die "--binary must name the executable flotilla-fake-forge"
  for tool in systemctl curl useradd; do command -v "$tool" >/dev/null || die "$tool is required"; done
fi
command -v openssl >/dev/null || die "openssl is required"
# Every value below lands in TOML, JSON, a certificate or a URL, so each is
# checked against the characters its kind of name allows.
[[ -n $host ]] || die "--host is required: the name rehearsal hosts use to reach the fake"
[[ $host =~ ^[A-Za-z0-9]([A-Za-z0-9.-]*[A-Za-z0-9])?$ ]] || die "--host '$host' must be a DNS name or IPv4 address"
[[ -z $ip || $ip =~ ^[0-9]{1,3}(\.[0-9]{1,3}){3}$ || $ip =~ ^[0-9A-Fa-f:]+$ ]] || die "--ip '$ip' must be an IPv4 or IPv6 address"
[[ $bind =~ ^[0-9.]+$ || $bind =~ ^\[[0-9A-Fa-f:]+\]$ ]] || die "--bind '$bind' must be an IPv4 address or a bracketed IPv6 address"
((${#forges[@]})) || die "at least one --forge MOUNT=TOKEN_LABEL=SERVICE_URL is required"
[[ -z $seed || -r $seed ]] || die "seed file $seed is not readable"
[[ -z $auto_merge || $auto_merge =~ ^[1-9][0-9]*$ ]] || die "--auto-merge-after takes a positive number of seconds"
[[ $port =~ ^[0-9]+$ && $admin_port =~ ^[0-9]+$ ]] || die "ports must be numbers"
[[ $login =~ ^[A-Za-z0-9._-]+$ ]] || die "--login must be a plain user name"

declare -A forge_ids forge_services
mounts=()
for entry in "${forges[@]}"; do
  IFS='=' read -r mount forge_id hosts extra <<<"$entry"
  [[ -z ${extra:-} ]] || die "--forge $entry has more than MOUNT=FORGE_ID=HOSTS"
  [[ $mount =~ ^[A-Za-z0-9_-]+$ ]] || die "mount '$mount' must be letters, digits, '-' or '_'"
  [[ $forge_id =~ ^[a-z0-9]([a-z0-9-]*[a-z0-9])?$ ]] || die "forge id '$forge_id' must be a lowercase DNS label"
  if [[ $hosts == https://* ]]; then
    [[ $hosts =~ ^https://[A-Za-z0-9._-]+(:[0-9]+)?(/[A-Za-z0-9._~/-]*)?$ ]] || die "--forge $entry needs an HTTPS service URL"
    source_service=${hosts%/}
  else
    [[ $hosts =~ ^[A-Za-z0-9._-]+(,[A-Za-z0-9._-]+)*$ ]] || die "--forge $entry needs an HTTPS service URL or comma-separated host names"
    source_service="https://${hosts%%,*}"
  fi
  [[ -z ${forge_ids[$mount]:-} ]] || die "mount '$mount' is given twice"
  for other in "${mounts[@]}"; do
    [[ ${forge_ids[$other]} != "$forge_id" ]] || die "forge id '$forge_id' is given twice"
    [[ ${forge_services[$other]} != "$source_service" ]] || die "original service '$source_service' is given twice"
  done
  forge_ids[$mount]=$forge_id
  forge_services[$mount]=$source_service
  mounts+=("$mount")
done

# Everything is valid: only now create anything.
if [[ -n $stage ]]; then
  mkdir -p "$stage"
  prefix=$(cd "$stage" && pwd)
fi
user=flotilla-fake-forge
public_url="https://$host:$port"
install -d -m 0755 "$prefix" "$prefix/apply"
install -d -m 0700 "$prefix/ca" "$prefix/tokens"
if [[ -z $stage ]]; then
  id -u "$user" >/dev/null 2>&1 || useradd --system --home-dir /nonexistent --shell /usr/sbin/nologin "$user"
  install -m 0755 "$binary" /usr/local/bin/flotilla-fake-forge
fi
# Files the service reads belong to its group; in --stage mode, to the caller.
readable() {
  if [[ -z $stage ]]; then chgrp "$user" "$@"; fi
  chmod 0640 "$@"
}

# The rehearsal-only CA is kept across re-runs, so hosts that already trust it keep working.
if [[ ! -s $prefix/ca/ca.pem ]]; then
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 3650 \
    -subj "/CN=flotilla rehearsal CA" -addext "basicConstraints=critical,CA:TRUE" \
    -addext "keyUsage=critical,keyCertSign,cRLSign" \
    -keyout "$prefix/ca/ca-key.pem" -out "$prefix/ca/ca.pem" 2>/dev/null
fi
san="DNS:$host"
if [[ $host =~ ^[0-9.]+$ ]]; then san="IP:$host"; fi
if [[ -n $ip && $ip != "$host" ]]; then san+=",IP:$ip"; fi
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -subj "/CN=$host" \
  -keyout "$work/server-key.pem" -out "$work/server.csr" 2>/dev/null
printf 'subjectAltName=%s\nbasicConstraints=CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\n' "$san" >"$work/ext"
openssl x509 -req -in "$work/server.csr" -CA "$prefix/ca/ca.pem" -CAkey "$prefix/ca/ca-key.pem" \
  -CAserial "$prefix/ca/ca.srl" -CAcreateserial -days 825 -extfile "$work/ext" -out "$work/server.pem" 2>/dev/null
install -m 0644 "$work/server.pem" "$prefix/server.pem"
install -m 0600 "$work/server-key.pem" "$prefix/server-key.pem"
readable "$prefix/server-key.pem"
install -m 0644 "$prefix/ca/ca.pem" "$prefix/rehearsal-ca.pem"

new_token() { [[ -s $1 ]] || (umask 077; openssl rand -hex 32 >"$1"); readable "$1"; }
if [[ -z $stage ]]; then chgrp "$user" "$prefix/tokens"; fi
chmod 0750 "$prefix/tokens"
new_token "$prefix/admin-token"

{
  echo "# Written by install-fake-forge.sh; re-running the script rewrites it."
  echo "public_url = \"$public_url\""
  echo "listen = \"$bind:$port\""
  echo "admin_listen = \"$bind:$admin_port\""
  echo "tls_cert = \"$prefix/server.pem\""
  echo "tls_key = \"$prefix/server-key.pem\""
  echo "admin_token_file = \"$prefix/admin-token\""
  if [[ -n $auto_merge ]]; then echo "auto_merge_after_secs = $auto_merge"; fi
  if [[ -n $seed ]]; then echo "seed = \"$prefix/seed.json\""; fi
  for mount in "${mounts[@]}"; do
    new_token "$prefix/tokens/$mount"
    echo
    echo "[forges.$mount]"
    echo "token_file = \"$prefix/tokens/$mount\""
    echo "login = \"$login\""
  done
} >"$work/config.toml"
install -m 0644 "$work/config.toml" "$prefix/config.toml"
if [[ -n $seed && $(cd "$(dirname "$seed")" && pwd)/$(basename "$seed") != "$prefix/seed.json" ]]; then
  install -m 0644 "$seed" "$prefix/seed.json"
fi

# Host-local execution bindings preserve the copied Forge and Repository records.
{
  for mount in "${mounts[@]}"; do
    forge_id=${forge_ids[$mount]}
    echo "[forge_overrides.\"${forge_services[$mount]}\"]"
    echo 'provider = "forgejo"'
    echo "endpoint_url = \"$public_url/$mount\""
    echo "token_path = \"$client_tokens/$forge_id\""
    echo
  done
} >"$prefix/apply/daemon.toml"

if [[ -z $stage ]]; then
  cat >/etc/systemd/system/flotilla-fake-forge.service <<UNIT
[Unit]
Description=flotilla rehearsal fake Forgejo service (#3374)
After=network-online.target
Wants=network-online.target

[Service]
User=$user
Group=$user
ExecStart=/usr/local/bin/flotilla-fake-forge serve --config $prefix/config.toml
Restart=on-failure
RestartSec=2
Environment=RUST_LOG=info
NoNewPrivileges=true
# The service only reads its config, certificate, tokens and seed, and keeps
# its state in memory, so the whole file system can be read-only to it.
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true

[Install]
WantedBy=multi-user.target
UNIT
  systemctl daemon-reload
  systemctl enable flotilla-fake-forge.service >/dev/null
  systemctl restart flotilla-fake-forge.service
  # Ready when every forge answers its token over HTTPS, verified against the CA.
  # The header comes from a private file, so the token never appears in `ps`.
  resolve=()
  if [[ -n $ip && $ip == *:* ]]; then resolve=(--resolve "$host:$port:[$ip]"); elif [[ -n $ip ]]; then resolve=(--resolve "$host:$port:$ip"); fi
  for mount in "${mounts[@]}"; do
    (umask 077; printf 'Authorization: token %s\n' "$(cat "$prefix/tokens/$mount")" >"$work/auth-$mount")
    ready=""
    for _ in $(seq 1 30); do
      if curl -fsS --cacert "$prefix/ca/ca.pem" -H "Accept: application/json" -H "@$work/auth-$mount" \
        "${resolve[@]}" "$public_url/$mount/api/v1/user" >/dev/null 2>&1; then
        ready=1
        break
      fi
      sleep 1
    done
    [[ -n $ready ]] || die "forge '$mount' did not answer at $public_url/$mount; see journalctl -u flotilla-fake-forge"
  done
  echo "The fake forge is running at $public_url, with the admin API on port $admin_port."
else
  echo "Staged the fake forge under $prefix (not installed)."
fi

cat <<EOF

Files: $prefix/config.toml, the rehearsal CA $prefix/rehearsal-ca.pem, the forge
tokens in $prefix/tokens/, the admin token $prefix/admin-token, and the
records in $prefix/apply/.
Logs: journalctl -u flotilla-fake-forge
Refused requests: curl --cacert $prefix/rehearsal-ca.pem \\
  -H "Authorization: Bearer \$(cat $prefix/admin-token)" https://$host:$admin_port/admin/v1/refusals

== 1. On every rehearsal host: trust the rehearsal CA ==
  install -m 0644 rehearsal-ca.pem /usr/local/share/ca-certificates/flotilla-rehearsal-ca.crt
  update-ca-certificates
EOF
if [[ -n $ip ]]; then echo "  and make $host resolve to $ip (for example in /etc/hosts)."; fi
echo
echo "== 2. On every rehearsal host: install the forge tokens for the daemon user =="
for mount in "${mounts[@]}"; do
  echo "  $prefix/tokens/$mount  ->  $client_tokens/${forge_ids[$mount]}  (mode 0600, owned by the daemon user)"
done
# With one forge the dummy-crew stanza is complete; with several, the
# operator picks one.
shared_mount="<MOUNT>" shared_forge_id="<FORGE_ID>" shared_service="https://<ORIGINAL_HOST>" shared_kind="<github|forgejo>"
if ((${#mounts[@]} == 1)); then
  shared_mount=${mounts[0]} shared_forge_id=${forge_ids[${mounts[0]}]}
  shared_service=${forge_services[$shared_mount]} shared_kind="forgejo"
  [[ $shared_service != https://github.com ]] || shared_kind="github"
fi
cat <<EOF

== 3. On every rehearsal host: add to daemon.toml, then restart flotillad ==
$(cat "$prefix/apply/daemon.toml")

  For dummy crews, select one original service and its fake mount:
[dummy_fleet.shared_forge]
service_url = "$shared_service"
endpoint_url = "$public_url/$shared_mount"
kind = "$shared_kind"
token_path = "$client_tokens/$shared_forge_id"

Keep the copied Forge, Repository and CredentialSpec records unchanged.
Bindings are host-local: install these settings on every rehearsal daemon.
EOF

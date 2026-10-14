#!/usr/bin/env bash
# Optional deep-tier adapter. The AMI must contain the formal-modelling image tools.
set -euo pipefail
verb=${1:?provider verb required}
allocation=${2:?allocation required}
[[ "$allocation" =~ ^quint-3220-[A-Za-z0-9]+$ ]] || { echo 'invalid allocation name' >&2; exit 2; }
root=${QUINT_PROVIDER_STATE:-${XDG_RUNTIME_DIR:-/tmp}/quint-ec2-$UID}
state="$root/$allocation"
mkdir -p "$state"
chmod 700 "$root" "$state"
connection() {
  ip=$(cat "$state/ip")
  user=${QUINT_SSH_USER:-ubuntu}
  options=(-o BatchMode=yes -o ConnectTimeout=10 -o StrictHostKeyChecking=accept-new -o "UserKnownHostsFile=$state/known_hosts")
  if [ -n "${QUINT_SSH_KEY_FILE:-}" ]; then options+=(-i "$QUINT_SSH_KEY_FILE"); fi
}
case "$verb" in
  start)
    lifetime=${QUINT_MAX_LIFETIME_MINUTES:-30}
    [[ "$lifetime" =~ ^[1-9][0-9]*$ ]] || { echo 'QUINT_MAX_LIFETIME_MINUTES must be a positive integer' >&2; exit 2; }
    # Cloud-init schedules termination independently of the driver/SSH connection.
    printf '#!/bin/sh\nshutdown -h +%s\n' "$lifetime" > "$state/user-data"
    aws ec2 run-instances --image-id "${QUINT_AMI_ID:?formal-modelling AMI required}" \
      --instance-type "${QUINT_INSTANCE_TYPE:-c7i.8xlarge}" --count 1 \
      --instance-initiated-shutdown-behavior terminate --user-data "file://$state/user-data" \
      --subnet-id "${QUINT_SUBNET_ID:?subnet required}" --security-group-ids "${QUINT_SECURITY_GROUP_ID:?SSH security group required}" \
      --key-name "${QUINT_KEY_NAME:?EC2 key pair required}" --client-token "$allocation" \
      --tag-specifications "ResourceType=instance,Tags=[{Key=Name,Value=$allocation}]" \
      --query 'Instances[0].InstanceId' --output text > "$state/id"
    aws ec2 wait instance-running --instance-ids "$(cat "$state/id")"
    aws ec2 describe-instances --instance-ids "$(cat "$state/id")" \
      --query 'Reservations[0].Instances[0].PublicIpAddress' --output text > "$state/ip"
    ip=$(cat "$state/ip")
    if [ -z "$ip" ] || [ "$ip" = None ]; then
      echo 'EC2 allocation has no public IP; check subnet public-address assignment' >&2
      exit 2
    fi
    connection
    ready=false
    for _ in {1..30}; do
      if ssh "${options[@]}" "$user@$ip" 'command -v quint && command -v java && command -v cargo' >/dev/null; then ready=true; break; fi
      sleep 2
    done
    "$ready" || { echo 'instance never became SSH-ready with the required tools' >&2; exit 1; }
    ;;
  run)
    connection
    scp "${options[@]}" "${3:?source bundle required}" "$user@$ip:$allocation.tgz"
    ssh "${options[@]}" "$user@$ip" "mkdir -p '$allocation' && tar -xzf '$allocation.tgz' -C '$allocation' && cd '$allocation' && specs/primitives/deep-local.sh"
    ;;
  collect)
    connection
    mkdir -p "${3:?output directory required}"
    ssh "${options[@]}" "$user@$ip" "tar --exclude='*.states' -czf - -C '$allocation' specs/primitives/build" > "$3/$allocation.tgz"
    ;;
  stop)
    # Search by idempotency token too: start may have failed after allocation but before writing its id.
    instance_ids=$(aws ec2 describe-instances --filters "Name=client-token,Values=$allocation" \
      --query "Reservations[].Instances[?State.Name!='terminated'].InstanceId[]" --output text)
    read -r -a ids <<< "$instance_ids"
    if [ "${#ids[@]}" -gt 0 ]; then
      aws ec2 terminate-instances --instance-ids "${ids[@]}" >/dev/null
      aws ec2 wait instance-terminated --instance-ids "${ids[@]}"
    fi
    rm -rf -- "$state"
    ;;
  *) echo "unknown provider verb: $verb" >&2; exit 2 ;;
esac

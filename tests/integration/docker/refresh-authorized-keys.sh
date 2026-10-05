#!/usr/bin/env bash
set -euo pipefail

SHARED_KEYS_DIR="$1"
SSH_DIR="$2"
OWNER="$3"

if [ ! -d "${SHARED_KEYS_DIR}" ]; then
    exit 0
fi

# sshd can authenticate while peers publish keys. Keep the previous complete
# file visible until its replacement has been read and made safe for StrictModes.
KEYS_FILE=$(mktemp "${SSH_DIR}/authorized_keys.XXXXXX")
trap 'rm -f "${KEYS_FILE}"' EXIT
if ! cat "${SHARED_KEYS_DIR}"/*.pub > "${KEYS_FILE}" 2>/dev/null; then
    exit 0
fi
chmod 600 "${KEYS_FILE}"
chown "${OWNER}" "${KEYS_FILE}"
mv -f "${KEYS_FILE}" "${SSH_DIR}/authorized_keys"

#!/usr/bin/env bash
set -euo pipefail

FLOTILLA_USER="flotilla"
FLOTILLA_HOME="/home/${FLOTILLA_USER}"
SHARED_KEYS_DIR="/shared-keys"
SSH_DIR="${FLOTILLA_HOME}/.ssh"
HOSTNAME=$(hostname)

# --- SSH host keys ---
ssh-keygen -A

# --- User keypair ---
if [ ! -f "${SSH_DIR}/id_ed25519" ]; then
    ssh-keygen -t ed25519 -f "${SSH_DIR}/id_ed25519" -N "" -q
fi

# --- Share public key ---
if [ -d "${SHARED_KEYS_DIR}" ]; then
    cp "${SSH_DIR}/id_ed25519.pub" "${SHARED_KEYS_DIR}/${HOSTNAME}.pub"
fi

# --- Build authorized_keys from shared keys ---
refresh_authorized_keys() {
    if [ -d "${SHARED_KEYS_DIR}" ]; then
        cat "${SHARED_KEYS_DIR}"/*.pub > "${SSH_DIR}/authorized_keys" 2>/dev/null || true
        chmod 600 "${SSH_DIR}/authorized_keys"
        chown "${FLOTILLA_USER}:${FLOTILLA_USER}" "${SSH_DIR}/authorized_keys"
    fi
}

# --- Fix ownership (before first refresh so the file is created with correct parent) ---
chown -R "${FLOTILLA_USER}:${FLOTILLA_USER}" "${SSH_DIR}"

refresh_authorized_keys

# --- Background refresh loop (pick up late-starting peers) ---
(
    while true; do
        sleep 5
        refresh_authorized_keys
    done
) &

# --- Start sshd ---
/usr/sbin/sshd

# --- Drop to flotilla user and exec CMD ---
exec gosu "${FLOTILLA_USER}" "$@"

#!/bin/sh
# =============================================================================
# Ferroma backup
# =============================================================================
# Backs up, in one consistent pass:
#
#   * PostgreSQL       — the authoritative metadata (users, mailboxes, queue,
#                        sync state, change log)
#   * the Maildir      — the authoritative message bytes
#   * configuration    — ferroma.toml, DKIM private keys, TLS certificates
#
# A backup that contains only one of the first two is not a backup: the database
# says a message exists and the Maildir holds its bytes, and restoring either alone
# gives you a mailbox full of dangling rows or a directory of orphaned files.
#
# Usage
#   scripts/backup.sh                    # one run, then exit (cron / compose sidecar)
#   scripts/backup.sh --once             # the same thing, spelled out: this is what
#                                        #   `docker compose run --rm backup --once` calls
#   BACKUP_INTERVAL_SECONDS=86400 scripts/backup.sh --loop
#
# Environment
#   PGHOST PGPORT PGUSER PGPASSWORD PGDATABASE   standard libpq variables
#   BACKUP_DIR                                   where to write (default /backups)
#   MAIL_DIR                                     maildir root (default /mail)
#   CONFIG_DIR                                   config + keys (default /etc/ferroma)
#   RETENTION_DAYS                               delete backups older than this (default 14)
#   BACKUP_INTERVAL_SECONDS                      loop period (default 86400)
# =============================================================================
set -eu

BACKUP_DIR="${BACKUP_DIR:-/backups}"
MAIL_DIR="${MAIL_DIR:-/mail}"
CONFIG_DIR="${CONFIG_DIR:-/etc/ferroma}"
RETENTION_DAYS="${RETENTION_DAYS:-14}"
BACKUP_INTERVAL_SECONDS="${BACKUP_INTERVAL_SECONDS:-86400}"
PGDATABASE="${PGDATABASE:-ferroma}"

log() { printf '%s [backup] %s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')" "$*" >&2; }
die() { log "ERROR: $*"; exit 1; }

require() { command -v "$1" >/dev/null 2>&1 || die "$1 is not installed"; }

# -----------------------------------------------------------------------------
# One backup pass
# -----------------------------------------------------------------------------
run_backup() {
    stamp="$(date -u '+%Y%m%dT%H%M%SZ')"
    target="${BACKUP_DIR}/${stamp}"
    mkdir -p "$target"

    # --- 1. PostgreSQL -------------------------------------------------------
    # Custom format so a single table can be restored without replaying everything.
    log "dumping PostgreSQL database ${PGDATABASE}"
    pg_dump --format=custom --compress=6 --file="${target}/ferroma.dump" "$PGDATABASE" \
        || die "pg_dump failed"

    # A schema-only dump restores an empty but correct structure in seconds; handy
    # when a migration went wrong and you need the shape without the data.
    pg_dump --schema-only --file="${target}/schema.sql" "$PGDATABASE" || die "pg_dump --schema-only failed"

    # --- 2. Mail store -------------------------------------------------------
    # `-a` preserves permissions and ownership (uid 10001 inside the container).
    # Maildir writes are atomic renames, so a live tar can miss an in-flight
    # delivery but can never capture a half-written message.
    if [ -d "$MAIL_DIR" ]; then
        log "archiving the mail store from ${MAIL_DIR}"
        tar -czf "${target}/maildir.tar.gz" -C "$(dirname "$MAIL_DIR")" "$(basename "$MAIL_DIR")" \
            || die "mail store archive failed"
    else
        log "warning: ${MAIL_DIR} does not exist; skipping the mail store"
    fi

    # --- 3. Configuration and secrets ---------------------------------------
    if [ -d "$CONFIG_DIR" ]; then
        log "archiving configuration from ${CONFIG_DIR}"
        # `--exclude` keeps any credentials file an operator may have dropped in.
        tar -czf "${target}/config.tar.gz" \
            --exclude='*.env' --exclude='credentials*' \
            -C "$(dirname "$CONFIG_DIR")" "$(basename "$CONFIG_DIR")" \
            || die "configuration archive failed"
    fi

    # --- 4. Manifest ---------------------------------------------------------
    # Restoring needs to know what it is looking at and what produced it.
    {
        printf 'ferroma_backup_version=1\n'
        printf 'created_at=%s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
        printf 'database=%s\n' "$PGDATABASE"
        printf 'postgres_version=%s\n' "$(psql -tAc 'SHOW server_version' "$PGDATABASE" 2>/dev/null || echo unknown)"
        printf 'ferroma_version=%s\n' "${FERROMA_VERSION:-unknown}"
        printf 'hostname=%s\n' "$(cat /etc/hostname 2>/dev/null || echo unknown)"
    } > "${target}/MANIFEST"

    # Checksums so a restore can prove the archive survived the trip.
    ( cd "$target" && sha256sum ./* > SHA256SUMS 2>/dev/null || true )

    size="$(du -sh "$target" | cut -f1)"
    log "backup complete: ${target} (${size})"
    echo "$target"
}

# -----------------------------------------------------------------------------
# Retention
# -----------------------------------------------------------------------------
prune() {
    [ "$RETENTION_DAYS" -gt 0 ] || return 0
    log "pruning backups older than ${RETENTION_DAYS} days"
    find "$BACKUP_DIR" -mindepth 1 -maxdepth 1 -type d -mtime "+${RETENTION_DAYS}" -print -exec rm -rf {} + \
        || log "warning: pruning incomplete"
}

# -----------------------------------------------------------------------------
# Entry point
# -----------------------------------------------------------------------------
require pg_dump
require psql

case "${1:-}" in
    --loop)
        log "running every ${BACKUP_INTERVAL_SECONDS}s; retention ${RETENTION_DAYS} days"
        while true; do
            if run_backup; then prune; else log "backup failed; will retry next cycle"; fi
            sleep "$BACKUP_INTERVAL_SECONDS"
        done
        ;;
    ''|--once)
        run_backup
        prune
        ;;
    *)
        echo "usage: backup.sh [--once|--loop]" >&2
        exit 2
        ;;
esac

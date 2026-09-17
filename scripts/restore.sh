#!/bin/sh
# =============================================================================
# Ferroma restore
# =============================================================================
# Restores a backup produced by scripts/backup.sh, in the order the specification
# §47 prescribes: PostgreSQL, then the mail store, then configuration.
#
#   scripts/restore.sh /backups/20260916T030000Z            # everything
#   scripts/restore.sh /backups/20260916T030000Z --db-only
#   scripts/restore.sh /backups/20260916T030000Z --mail-only
#   scripts/restore.sh /backups/20260916T030000Z --verify-only
#
# Restoring the database without the mail store (or the reverse) leaves the server
# inconsistent. `--db-only`/`--mail-only` exist for partial disaster recovery and
# print a warning; run `ferroma storage verify` afterwards either way.
#
# Environment: PGHOST PGPORT PGUSER PGPASSWORD PGDATABASE, MAIL_DIR, CONFIG_DIR.
# =============================================================================
set -eu

BACKUP_PATH="${1:-}"
[ -n "$BACKUP_PATH" ] || { echo "usage: restore.sh <backup-dir> [--db-only|--mail-only|--verify-only]" >&2; exit 2; }
shift || true

MODE="all"
for arg in "$@"; do
    case "$arg" in
        --db-only) MODE="db" ;;
        --mail-only) MODE="mail" ;;
        --verify-only) MODE="verify" ;;
        *) echo "unknown option: $arg" >&2; exit 2 ;;
    esac
done

MAIL_DIR="${MAIL_DIR:-/mail}"
CONFIG_DIR="${CONFIG_DIR:-/etc/ferroma}"
PGDATABASE="${PGDATABASE:-ferroma}"

log() { printf '%s [restore] %s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')" "$*" >&2; }
die() { log "ERROR: $*"; exit 1; }

[ -d "$BACKUP_PATH" ] || die "no such backup directory: $BACKUP_PATH"

# -----------------------------------------------------------------------------
# 0. Verify
# -----------------------------------------------------------------------------
log "verifying $BACKUP_PATH"
if [ -f "${BACKUP_PATH}/SHA256SUMS" ]; then
    ( cd "$BACKUP_PATH" && sha256sum -c SHA256SUMS ) || die "checksum mismatch — the backup is corrupt"
else
    log "warning: no SHA256SUMS in this backup; integrity is unverified"
fi
[ -f "${BACKUP_PATH}/MANIFEST" ] && sed 's/^/  /' "${BACKUP_PATH}/MANIFEST" >&2

[ "$MODE" = "verify" ] && { log "verification only; nothing restored"; exit 0; }

# -----------------------------------------------------------------------------
# 1. PostgreSQL
# -----------------------------------------------------------------------------
restore_db() {
    [ -f "${BACKUP_PATH}/ferroma.dump" ] || die "ferroma.dump is missing"
    log "restoring PostgreSQL database ${PGDATABASE}"

    # Refuse to restore into a database that already holds data unless the operator
    # explicitly asks for it: silently merging two mail stores is how data is lost.
    existing="$(psql -tAc "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema='public'" "$PGDATABASE" 2>/dev/null || echo 0)"
    if [ "${existing:-0}" -gt 0 ]; then
        if [ "${FORCE_RESTORE:-0}" != "1" ]; then
            die "database ${PGDATABASE} is not empty (${existing} tables). Set FORCE_RESTORE=1 to overwrite, or restore into a fresh database."
        fi
        log "FORCE_RESTORE=1: dropping and recreating the public schema"
        psql -v ON_ERROR_STOP=1 -c 'DROP SCHEMA public CASCADE; CREATE SCHEMA public;' "$PGDATABASE" \
            || die "could not reset the schema"
    fi

    pg_restore --no-owner --no-privileges --exit-on-error --dbname="$PGDATABASE" "${BACKUP_PATH}/ferroma.dump" \
        || die "pg_restore failed"
    log "database restored"
}

# -----------------------------------------------------------------------------
# 2. Mail store
# -----------------------------------------------------------------------------
restore_mail() {
    [ -f "${BACKUP_PATH}/maildir.tar.gz" ] || die "maildir.tar.gz is missing"
    log "restoring the mail store into ${MAIL_DIR}"
    parent="$(dirname "$MAIL_DIR")"
    mkdir -p "$parent"
    tar -xzf "${BACKUP_PATH}/maildir.tar.gz" -C "$parent" || die "mail store extraction failed"
    log "mail store restored"
}

# -----------------------------------------------------------------------------
# 3. Configuration
# -----------------------------------------------------------------------------
restore_config() {
    [ -f "${BACKUP_PATH}/config.tar.gz" ] || { log "no configuration archive; skipping"; return 0; }
    log "restoring configuration into ${CONFIG_DIR}"
    parent="$(dirname "$CONFIG_DIR")"
    mkdir -p "$parent"
    tar -xzf "${BACKUP_PATH}/config.tar.gz" -C "$parent" || die "configuration extraction failed"
    log "configuration restored"
}

case "$MODE" in
    all)
        restore_db
        restore_mail
        restore_config
        log "restore complete. Next: 'ferroma storage verify' then start the server."
        ;;
    db)
        log "WARNING: restoring only the database leaves the mail store stale."
        restore_db
        ;;
    mail)
        log "WARNING: restoring only the mail store leaves the database stale."
        restore_mail
        ;;
esac

log "done"

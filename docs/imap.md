# IMAP in Ferroma

**Who should read this:** anyone implementing `ferroma-imap`, anyone writing a
compatibility test against Thunderbird or Apple Mail, and any operator whose
user cannot get their mail client to log in.

Ferroma is an IMAP4rev1 server (RFC 3501) for third-party clients — Thunderbird,
Apple Mail, Outlook, iPhone Mail and Android clients are first-class citizens
and are not required to speak FCP (specification §53). This document specifies
which commands ship in the first release, the session state machine, folder
naming including the `INBOX` special case and the Maildir++ mapping, UID and
UIDVALIDITY semantics, the flag vocabulary and its Maildir encoding, the `FETCH`
items and `SEARCH` keys supported, `IDLE`, `APPEND` limits, and the compatibility
target list.

> **Status:** design specification. The `ferroma-imap` crate is implemented against this
> document; sections marked _(planned)_ describe behaviour that is specified but not yet shipped.
> Everything in this file is _(planned)_ as of now: `crates/ferroma-imap/src/lib.rs`
> is a skeleton. The folder, flag, Maildir and search behaviour it relies on **is**
> implemented, in `crates/ferroma-storage/src/maildir.rs`,
> `crates/ferroma-storage/src/repository/mailboxes.rs`,
> `crates/ferroma-storage/src/repository/messages.rs` and
> `crates/ferroma-mail/src/flags.rs`.

---

## 1. Protocol baseline

| Property | Value |
|---|---|
| Protocol | IMAP4rev1, RFC 3501 |
| Greeting | `* OK [CAPABILITY …] <imap.banner>`, e.g. `* OK [CAPABILITY IMAP4rev1 …] Ferroma IMAP4rev1 ready` |
| Port | `imap.port`, 143, plaintext with `STARTTLS` |
| Implicit TLS port | `imap.imaps_port`, 993, `0` disables it |
| Line terminator | `CRLF` |
| Literals | `{n}` synchronising literals; `{n+}` non-synchronising literals are accepted, so `APPEND` can be pipelined |
| Authentication | `LOGIN` (user + password), `AUTHENTICATE PLAIN`, `AUTHENTICATE LOGIN` |
| Encryption | rustls only — see [architecture.md](architecture.md) §8 |

`imap.require_tls_for_login` (default `false`) refuses `LOGIN` and `AUTHENTICATE`
on an unencrypted connection with `NO [PRIVACYREQUIRED]`. `docker-compose.prod.yml`
sets `FERROMA__IMAP__REQUIRE_TLS_FOR_LOGIN` to `'true'`; a production deployment
should keep it. `LOGIN` sends the password in cleartext, and there is no
`CRAM-MD5` or `SCRAM-*` to fall back on (see §8).

**rustls only, and no `LOGINDISABLED`-versus-`AUTH=PLAIN` dance.** When
`require_tls_for_login` is on, `LOGIN` is not advertised as available over
cleartext; the client sees `STARTTLS` in `CAPABILITY` and is expected to use it.

---

## 2. Commands: first release and planned

Specification §12 splits the command set in two.

### 2.1 First release

```text
CAPABILITY   LOGIN    LOGOUT   NOOP
LIST         LSUB
SELECT       EXAMINE  STATUS
FETCH        STORE
SEARCH       UID
```

Plus the ones any client needs to work at all:

| Command | Why it is in the first release |
|---|---|
| `AUTHENTICATE` | Thunderbird and Apple Mail default to `AUTHENTICATE PLAIN` rather than `LOGIN` |
| `STARTTLS` | the only way to reach 143 encrypted |
| `CLOSE` | sent by several clients before `LOGOUT`; without it they log an error |
| `CHECK` | a no-op checkpoint, but its absence shows up as a protocol error in some clients |

### 2.2 Planned

```text
APPEND       COPY         MOVE         EXPUNGE      IDLE
```

| Command | Gate | Status |
|---|---|---|
| `APPEND` | `imap.max_append_size` | _(planned)_ |
| `COPY` | — | _(planned)_ |
| `MOVE` | `imap.enable_move` | _(planned)_ — RFC 6851 |
| `EXPUNGE` | `storage.soft_delete` | _(planned)_ |
| `IDLE` | `imap.enable_idle`, `imap.max_idle_secs` | _(planned)_ — RFC 2177 |
| `UIDPLUS` (`UID EXPUNGE`) | _(planned)_ with `EXPUNGE` | RFC 4315 |
| `NAMESPACE` | _(planned)_ | RFC 2342; Thunderbird asks for it and copes with `NO` |
| `SORT` / `THREAD` | not planned for v1 | Thunderbird falls back gracefully |
| `CONDSTORE` / `QRESYNC` | schema is ready (`folders.highest_modseq`, `messages.modseq`), protocol not implemented | _(planned)_ |
| `COMPRESS=DEFLATE` | not planned for v1 | |
| `NOTIFY` | not planned for v1 | `IDLE` covers the same need for one folder at a time |
| `ACL`, `QUOTA`, `METADATA` | not planned for v1 | quota is enforced server-side without an IMAP extension |
| `CATENATE`, `BINARY`, `MULTIAPPEND` | not planned for v1 | |

The gates are real config keys and are validated at boot:
`ImapConfig::enable_idle`, `ImapConfig::enable_move`, `ImapConfig::max_idle_secs`,
`ImapConfig::max_append_size` in `crates/ferroma-core/src/config.rs`.

### 2.3 `CAPABILITY`

The advertised list is built from the configuration, never hard-coded:

```text
* CAPABILITY IMAP4rev1
             LOGINDISABLED        (only when imap.require_tls_for_login and not encrypted)
             STARTTLS             (only when tls.enabled and not already encrypted)
             AUTH=PLAIN AUTH=LOGIN
             IDLE                 (only when imap.enable_idle)
             MOVE UIDPLUS         (only when imap.enable_move)
             UNSELECT
             LITERAL+
             UIDPLUS              (once EXPUNGE ships)
             CHILDREN             (Maildir++ has real hierarchies)
```

`LOGINDISABLED` is the RFC 3501 §7.2.1 way to say "`LOGIN` is not available on
this connection"; it is advertised *instead of* removing `LOGIN` from the list,
because a client that sees neither treats it as a broken server.

---

## 3. Session states

```text
   ┌──────────────────────────┐
   │     Not Authenticated    │◄── connection, "* OK [CAPABILITY …]"
   └────────────┬─────────────┘
                │  LOGIN / AUTHENTICATE succeeds
                │  (STARTTLS here returns to Not Authenticated)
                ▼
   ┌──────────────────────────┐
   │       Authenticated      │◄── CLOSE, or a failing SELECT
   └────────────┬─────────────┘
                │  SELECT / EXAMINE
                ▼
   ┌──────────────────────────┐
   │         Selected         │  one mailbox at a time
   └────────────┬─────────────┘
                │  LOGOUT (from any state)
                ▼
   ┌──────────────────────────┐
   │         Logout           │  "* BYE", then TCP close
   └──────────────────────────┘
```

Rules the implementation must respect:

| Rule | Detail |
|---|---|
| One selected mailbox per session | `SELECT` while already `Selected` deselects the first and selects the second, silently. There is no implicit `CLOSE`. |
| `SELECT` is read-write, `EXAMINE` is read-only | the mode is session state and gates `STORE` and `EXPUNGE`: `NO [READ-ONLY]` |
| State, not global | every session has its own selected mailbox, its own tag namespace and its own command pipeline position |
| Commands legal in the wrong state | `BAD` with a hint, never a silent no-op: `BAD Command not valid in this state` |
| `NOOP` is legal everywhere | including `Selected`; it is also where pending untagged updates are flushed |
| Untagged responses | `EXISTS`, `RECENT`, `FLAGS`, `UIDVALIDITY`, `UIDNEXT`, `PERMANENTFLAGS` on `SELECT`; `EXPUNGE`, `FETCH`, `EXISTS`, `RECENT` while `Selected` |
| Tag echo | every tagged reply repeats the client's tag verbatim, including `*` does not |
| `imap.idle_timeout_secs` | a session that has sent nothing for this long gets `* BYE Autologout; idle for too long` and the socket closes. Default 1800. |

Session struct _(planned)_:

```rust
/// crates/ferroma-imap/src/session.rs
pub struct ImapSession {
    pub connection_id: Uuid,
    pub remote_addr: SocketAddr,
    pub state: ImapState,               // NotAuthenticated | Authenticated | Selected | Logout
    pub encrypted: bool,
    pub authenticated_user: Option<UserId>,
    pub session_id: Option<SessionId>,
    pub mailbox_id: Option<MailboxId>,  // the address currently open
    pub folder_id: Option<MailboxId>,   // the folder currently selected
    pub read_only: bool,
    /// UIDs the session has marked \Deleted but not expunged. Needed even with
    /// UIDPLUS, because a client that did not request UIDPLUS still expects
    /// EXPUNGE to remove exactly the messages it flagged.
    pub deleted_uids: BTreeSet<i64>,
}
```

---

## 4. Folders

### 4.1 The schema behind them

Folders are rows in `folders`, not directories walked at request time
(`migrations/0001_initial.sql`):

| Column | Meaning |
|---|---|
| `folders.mailbox_id` | the address that owns the folder |
| `folders.name` | the IMAP name, e.g. `Archive/2026` |
| `folders.parent_id` | self-reference for hierarchy; `NULL` for a top-level folder |
| `folders.special_use` | `\Sent`, `\Drafts`, `\Trash`, `\Junk`, `\Archive`, `\All`, `\Flagged`, or `NULL` |
| `folders.subscribed` | `LSUB` vs `LIST` |
| `folders.uid_validity`, `folders.uid_next` | see §5 |
| `folders.highest_modseq` | reserved for `CONDSTORE` _(planned)_ |
| `folders.message_count`, `unseen_count`, `total_bytes` | counters kept current by `FoldersRepository::recount` |

Indexes: `folders_name_key (mailbox_id, name)` unique — two folders of one
address cannot share a name; `folders_special_use_key (mailbox_id, special_use)
WHERE special_use IS NOT NULL` — at most one `\Sent` per address.

The schema's `CHECK` restricts `special_use` to that list. `INBOX` is stored
with `special_use = NULL`.

### 4.2 The `INBOX` special case

RFC 3501 §5.1 makes `INBOX` special in three ways, and all three are implemented:

1. **`INBOX` is case-insensitive.** `select inbox`, `SELECT Inbox` and
   `SELECT INBOX` are the same folder. `FoldersRepository::find_by_name` compares
   case-insensitively for `INBOX` and case-sensitively for everything else, and
   `normalise_folder` in `crates/ferroma-storage/src/maildir.rs` canonicalises the
   spelling before it reaches the filesystem:
   `normalise_folder("") == "INBOX"`, `normalise_folder("/") == "INBOX"`,
   `normalise_folder("/Sent/") == "Sent"`.
2. **`INBOX` cannot be created, renamed or deleted.** `CREATE INBOX` is `NO`
   (it already exists, conceptually); `RENAME INBOX` is `NO`; `DELETE INBOX` is
   `NO`. `Maildir::delete_folder` and `Maildir::rename_folder` refuse it too, so
   the protection holds even if a protocol layer forgets:
   `StorageError::Invalid("INBOX cannot be deleted")`.
3. **`INBOX` always sorts first** in `LIST` and in `Maildir::list_folders`.
   Clients display the first folder as the inbox; a `LIST` that returns `Archive`
   before `INBOX` looks broken to a user.

`INBOX` carries no `special_use`. `FoldersRepository::ensure_standard` creates it
with `None` deliberately: RFC 6154 reserves `\Inbox` for a folder that contains
all mail, and a client that maps folders by `special_use` alone must fall back to
matching the name `INBOX`.

### 4.3 The standard folder set

`FoldersRepository::ensure_standard` (and `Maildir::ensure_mailbox`) create:

| Folder | `special_use` | Maildir directory (`storage.layout = "maildir"`) |
|---|---|---|
| `INBOX` | `NULL` | `<root>/<domain>/<local>/Maildir/` |
| `Sent` | `\Sent` | `<root>/<domain>/<local>/Maildir/.Sent/` |
| `Drafts` | `\Drafts` | `<root>/<domain>/<local>/Maildir/.Drafts/` |
| `Trash` | `\Trash` | `<root>/<domain>/<local>/Maildir/.Trash/` |
| `Junk` | `\Junk` | `<root>/<domain>/<local>/Maildir/.Junk/` |
| `Archive` | `\Archive` | `<root>/<domain>/<local>/Maildir/.Archive/` |

Idempotent and safe to race with itself — the second caller's `INSERT` loses to
the unique index and is ignored.

### 4.4 Maildir++ mapping

Nested IMAP folders are hierarchical with `/` as the separator. Maildir++ encodes
the whole path into one directory name, dot-separated:

```text
Archive/2026     ↔     .Archive.2026
Archive/2026/Q1  ↔     .Archive.2026.Q1
```

`maildir_folder_name(folder)` implements one direction
(`format!(".{}", folder.replace('/', "."))`) and `Maildir::list_folders`
implements the other (`name.trim_start_matches('.').replace('.', "/")`).

The nesting is *flat on disk*: `.Archive.2026` is a sibling of `.Archive`, not a
child of it. That is what Maildir++ means and it is why any mail tool can read
the tree without understanding IMAP hierarchies. It also means a folder name may
not contain a `.` in a position that would be ambiguous with a separator — a
folder literally named `a.b` and a nested `a/b` would collide. Ferroma resolves
this the way every Maildir++ implementation does: **the IMAP name is
authoritative and lives in `folders.name`; the directory name is derived.** A
name containing `.` is stored and served faithfully, and the on-disk directory is
just an encoding. `Maildir::folder_dir` calls `maildir_folder_name` on every
access, so the mapping is never cached and cannot drift.

Folder names are validated before they reach the filesystem.
`sanitize_component` is applied to **each path segment**, so `Archive/../../etc`
is rejected segment by segment rather than by pattern-matching the whole string:

```rust
// crates/ferroma-storage/src/maildir.rs
fn maildir_folder_name(folder: &str) -> Result<String> {
    for part in folder.split('/') {
        sanitize_component(part)?;
    }
    Ok(format!(".{}", folder.replace('/', ".")))
}
```

`sanitize_component` rejects `""`, `"."`, `".."`, anything containing `/`, `\`,
`\0` or `:` (see §10), and returns `StorageError::Invalid` otherwise.

### 4.5 `LIST`, `LSUB`, `STATUS`, `SUBSCRIBE`

| Command | Behaviour |
|---|---|
| `LIST "" "*"` | every folder, `INBOX` first, then case-insensitive alphabetical. `\HasChildren` / `\HasNoChildren` from `parent_id` |
| `LIST` attributes | `\HasChildren`, `\HasNoChildren`, `\Noselect` for a parent with no messages of its own _(planned)_ |
| `LIST "" "Archive/%"` | `%` matches one level, `*` matches any depth, per RFC 3501 §6.3.8 |
| `LSUB` | folders with `folders.subscribed = true`. Default is subscribed |
| `SUBSCRIBE` / `UNSUBSCRIBE` | `FoldersRepository::set_subscribed`. Never affects whether a folder exists |
| `STATUS mailbox (MESSAGES RECENT UIDNEXT UIDVALIDITY UNSEEN)` | reads `folders.message_count`, `unseen_count`, `uid_next`, `uid_validity`. Legal in `Authenticated`, and for the selected mailbox too |
| `CREATE` with a trailing separator | creates the hierarchy for the parent folders as well, per RFC 3501 §6.3.3 |

---

## 5. UIDs and UIDVALIDITY

Two numbers, two different jobs. Confusing them is the classic IMAP bug.

| | UID | UIDVALIDITY |
|---|---|---|
| Scope | unique and monotonically increasing **within one folder** | identifies a *generation* of a folder's UID space |
| Storage | `messages.uid`, `UNIQUE (folder_id, uid)` | `folders.uid_validity` |
| Allocator | `folders.uid_next` | assigned when the folder is created, default `1` |
| Stable across | flag changes, moves *within* the same folder (there are none) | nothing — it changes only when the UID space is rebuilt |
| Never reused | `MessagesRepository::move_to_folder` allocates a **fresh** UID in the destination; the old UID is never reissued | — |

### 5.1 Allocation is atomic

The `INSERT` that stores a message allocates its UID in the same statement, under
the folder's row lock:

```sql
WITH next_uid AS (
    UPDATE folders SET uid_next = uid_next + 1, updated_at = NOW()
     WHERE id = $1
     RETURNING uid_next - 1 AS uid
)
INSERT INTO messages (folder_id, mailbox_id, uid, …)
SELECT $2, $3, next_uid.uid, … FROM next_uid
```

`crates/ferroma-storage/src/repository/messages.rs`. Two simultaneous deliveries
into one folder therefore cannot receive the same UID, and a crash cannot leave a
gap that a later message would fill. `copy_to_folder` and `move_to_folder` use the
same CTE.

### 5.2 UIDs are never reused, and expunged UIDs stay gone

`MessagesRepository::find_by_uid` and `list_by_uids` filter
`expunged_at IS NULL`: once a client has expunged a UID, referring to it again is
`NO` rather than a resurrected message. `expunge()` sets `expunged_at` and returns
the affected rows sorted by `uid`, because PostgreSQL's `UPDATE` gives no
ordering guarantee and IMAP's `EXPUNGE` responses must be in ascending UID order
(descending index-based order is what RFC 3501 actually requires for untagged
`EXPUNGE`; see §6.3).

### 5.3 What changes UIDVALIDITY

`FoldersRepository::set_uid_validity(id, uid_validity)` exists for exactly the
cases where the UID space is rebuilt:

| Event | UIDVALIDITY |
|---|---|
| Folder created | assigned once, default `1` |
| Normal operation | **unchanged** |
| Folder renamed | unchanged — it is the same UID space |
| Messages expunged | unchanged |
| A rebuild from the Maildir after data loss | **bumped** |
| A restore from a backup taken before a rebuild | **bumped** |
| `ferroma storage verify --repair` renumbers a folder | **bumped** |

The rule the implementation follows: if any operation could make an old
UID → message mapping wrong, bump UIDVALIDITY. A conservative extra bump costs a
client one folder resync; a missing bump costs it a wrong message.

### 5.4 What a client must do when UIDVALIDITY changes

Specification §55's "Server = Source of Truth" makes this unambiguous, and the
FCP contract states it in [fcp.md](fcp.md) §4:

1. Notice that the `UIDVALIDITY` in the `SELECT` response (or in
   `GET /api/v1/client/mailboxes`) differs from the value cached for that folder.
2. **Discard every cached UID for that folder**, and every cached message that
   was identified only by its UID. Message bodies keyed by `rfc_message_id` or by
   the server's `message_id` may be kept.
3. Resync the folder from cursor `0`. UIDs will be handed out afresh.
4. Never assume a UID from before the change refers to the same message.

A server-side corollary: because the client throws its cache away, Ferroma must
not bump UIDVALIDITY casually. Bumping it on every delivery would make every
client re-download the folder on every new message.

---

## 6. Flags and keywords

### 6.1 The vocabulary

RFC 3501 §2.3.2 defines six system flags:

```text
\Seen     \Answered     \Flagged     \Deleted     \Draft     \Recent
```

Plus **keywords**: anything else a client sends — `$Junk`, `$label1`,
`$Forwarded`, `NonJunk`. `ferroma_mail::Flags` stores the six system flags as a
`u8` bitset (bit 0 `\Seen`, 1 `\Answered`, 2 `\Flagged`, 3 `\Deleted`, 4 `\Draft`,
5 `\Recent`) and the keywords as a `Vec<String>` in insertion order. Keywords are
lower-cased on the way in and deduplicated, which is what makes
`Flags::to_db_string()` deterministic.

`Flags::parse` accepts `(\Seen \Flagged $Label1)`, the bare form without
parentheses, doubled spaces and quoted keywords. An unknown `\Foo` name is stored
as a keyword — a client's private flags survive a round trip through a server
that has never heard of them.

### 6.2 Three renderings, one flag set

| Rendering | Method | Example | Where it is used |
|---|---|---|---|
| IMAP | `Flags::to_imap_string()` | `(\Seen \Flagged "$Label1")` | `FETCH FLAGS`, `STORE` replies, `PERMANENTFLAGS` |
| Database | `Flags::to_db_string()` | `seen,flagged,$label1` | `messages.flags` |
| Maildir | `flags_to_maildir()` | `FS` | the file name in `cur/` |

`messages.flags` is `TEXT NOT NULL DEFAULT ''`, and the schema comment describes
it as a space-separated list. `Flags::to_db_string()` writes **comma**-separated
values (`parts.join(",")`), lower-cased with the backslash stripped:
`\Seen` → `seen`, `$Label1` → `$label1`. The column is opaque text; the comma
form is what the implemented writer produces and what `Flags::from_db_string()`
reads back. Treat `Flags::to_db_string` / `from_db_string` as the only correct
way to touch that column — hand-written SQL against it is a bug waiting to
happen.

`\Recent` is the awkward one. It is a system flag, but it is *session state*, not
stored state: RFC 3501 says a message is `\Recent` if this is the first session
to see it. Maildir expresses it by directory — a message that has not been seen by
a mail client lives in `new/`, and one that has lives in `cur/`. So Ferroma
derives `\Recent` from the directory, and `flags_to_maildir("recent")` returns
the empty string because `\Recent` must never end up in a file name.

### 6.3 Maildir ↔ IMAP flag mapping

The two vocabularies meet in exactly two functions,
`Maildir`'s `flags_to_maildir` and `maildir_to_flags`, and nowhere else.
`crates/ferroma-storage/src/maildir.rs` is the single place to change if a
letter is ever wrong.

| IMAP flag | Maildir letter | Database form | Notes |
|---|---|---|---|
| `\Draft` | `D` | `draft` | |
| `\Flagged` | `F` | `flagged` | |
| `\Answered` | `R` | `answered` | |
| `\Seen` | `S` | `seen` | |
| `\Deleted` | `T` | `deleted` | `T` is "trashed", which is what `\Deleted` means in Maildir |
| `\Recent` | *(none)* | `recent` | derived from `new/` vs `cur/`; not storable in a name |
| keyword `$Junk` | *(none)* | `$junk` | keywords live in the database only; Maildir has nowhere to put them |
| — | `P` | — | "passed"; accepted on read, never written |

The write order is the Maildir convention `D F P R S T`, implemented as
`D`, `F`, `R`, `S`, `T`:

```rust
pub fn flags_to_maildir(flags: &str) -> String {
    // … if has("draft") { out.push('D') } if has("flagged") { out.push('F') }
    //    if has("answered") { out.push('R') } if has("seen") { out.push('S') }
    //    if has("deleted") { out.push('T') }
}
```

`maildir_to_flags("FS") == "flagged seen"`; unknown letters are ignored
(`maildir_to_flags("XYZ") == ""`), which is what makes a store written by another
tool readable.

**Keywords are the lossy part, by design.** Moving a message between folders
rewrites its Maildir file name from the flag set, and custom keywords have no
letter. They stay in `messages.flags`; the file name simply does not mention
them. Re-reading is therefore database-first for keywords and Maildir-first for
the letters — which is why `iter_messages` returning `maildir_flags` is a repair
input, not the source of truth.

`PERMANENTFLAGS` on `SELECT` advertises `(\Answered \Flagged \Deleted \Seen
\Draft \*)`; `\*` is the RFC 3501 way to say "arbitrary keywords are accepted",
and Ferroma does accept them.

### 6.4 `STORE` semantics

| Form | Behaviour |
|---|---|
| `STORE 1:5 +FLAGS (\Seen)` | add |
| `STORE 1:5 -FLAGS (\Seen)` | remove |
| `STORE 1:5 FLAGS (\Seen)` | replace |
| `STORE 1:5 +FLAGS.SILENT (…)` | same, no untagged `FETCH` reply |
| `UID STORE …` | same, addressed by UID |
| `\Recent` in a `STORE` | ignored, not an error — it is not settable |

Every `STORE` that changes something does three things, in this order:

1. `MessagesRepository::set_flags` / `add_flags` / `remove_flags` — the database
   row, which is what every other surface reads.
2. `Maildir::set_flags(relative_path, flags)` — renames the file, moving it
   between `new/` and `cur/` when the flag set becomes empty or non-empty.
   It returns the possibly-new path, and the caller must persist it via
   `MessagesRepository::set_storage_path` if it changed.
3. `EventBus::publish(EventScope::User(user_id), Event::mail_flag_changed(…))`, then
   one `change_log` append, so a synced client learns about it without polling.

Step 2 is idempotent: `set_flags` returns the original path when the computed name
is unchanged, so a repeated `STORE` does not churn the filesystem.

`FETCH` writes `\Seen` implicitly when `BODY[…]` is fetched without `.PEEK`, and a
`\Seen` transition likewise publishes `Event::mail_read`. Clients that want to
peek — every mail client rendering a list — send `BODY.PEEK[]`.

### 6.5 What Maildir cannot express, and what happens

| Situation | Behaviour |
|---|---|
| A keyword is set | stored in `messages.flags`; the file name does not change |
| All flags are cleared | the file moves from `cur/` back to `new/`, which is what a client marking a message unread expects |
| A flag is set | the file moves from `new/` to `cur/` with `2,<letters>` appended |
| The read-only mode is active (`EXAMINE`) | `STORE` is `NO [READ-ONLY]` |

---

## 7. `FETCH` items

| Item | Support | Source |
|---|---|---|
| `FLAGS` | yes | `messages.flags` via `Flags::to_imap_string()` |
| `UID` | yes | `messages.uid` |
| `RFC822.SIZE` | yes | `messages.size_bytes` |
| `INTERNALDATE` | yes | `messages.internal_date` |
| `ENVELOPE` | yes | parsed from the stored headers: `Date`, `Subject`, `From`, `Sender`, `Reply-To`, `To`, `Cc`, `Bcc`, `In-Reply-To`, `Message-ID` |
| `BODY` / `BODY[]` | yes | the full RFC 5322 bytes from the Maildir |
| `BODY[HEADER]` | yes | `Maildir::read_prefix` for the header block, parsed with `ferroma_mail::Headers` |
| `BODY[HEADER.FIELDS (…)]` | yes | selected headers, re-folded with `fold_header_line` |
| `BODY[HEADER.FIELDS.NOT (…)]` | yes | the complement |
| `BODY[TEXT]` | yes | everything after the blank line |
| `BODY[<section>]` / `BODY[<section>]<partial>` | yes | MIME part addressing, and `BODY[]<0.1024>` partial fetches for resumable downloads |
| `BODY.PEEK[…]` | yes | same, without setting `\Seen` |
| `BODYSTRUCTURE` | yes | non-extensible form first; the extension data (`BODYSTRUCTURE`) is _(planned)_ |
| `BODY` (non-extensible `BODYSTRUCTURE`) | yes | |
| `MODSEQ` | _(planned)_ | `messages.modseq` is stored and indexed for `CONDSTORE` |
| `BINARY[…]` | no | `BINARY` is not advertised |
| `X-GM-*` | no | Gmail extensions are not emulated |

Three practical rules:

* **Fetched bytes are the stored bytes.** `BODY[]` returns exactly what is in the
  Maildir — the message as received, with Ferroma's `Received:` header on top.
  Nothing is re-rendered, so a `FETCH` and a `curl` of
  `GET /api/v1/messages/:id/raw` return the same octets.
* **`max_fetch_messages` caps one command.** `limits.max_fetch_messages` (5000)
  bounds `FETCH 1:*` on a huge folder; a larger range is rejected rather than
  allowed to allocate. Clients that ask for more are broken anyway — they should
  page by UID range.
* **A missing body is `NO`**, not a truncated result:
  `StorageError::BodyMissing` becomes
  `NO [SERVERBUG] message body missing`, and the message is listed in the
  integrity report from `ferroma storage verify` (see [storage.md](storage.md) §9).

Sequence sets and UID sets accept the full RFC 3501 §9 grammar: `1`, `1:5`,
`1:*`, `1,3,5:7`, `*` (the highest UID or sequence number in the folder). `*` on
an empty folder is an error, not an empty set.

---

## 8. `SEARCH`

`SEARCH` is answered with a space-separated list of matching *sequence numbers*;
`UID SEARCH` with UIDs. `SEARCH` is not a great fit for a relational store, so
the translation is explicit:

| Key | Translation |
|---|---|
| `ALL` | `expunged_at IS NULL` |
| `SEEN` / `UNSEEN` | `flags` contains / does not contain `seen` |
| `ANSWERED` / `UNANSWERED` | `answered` |
| `FLAGGED` / `UNFLAGGED` | `flagged` |
| `DELETED` / `UNDELETED` | `deleted` |
| `DRAFT` / `UNDRAFT` | `draft` |
| `RECENT` / `OLD` | `new/` vs `cur/` — derived, so it is evaluated in Rust after the SQL pass |
| `KEYWORD <kw>` / `UNKEYWORD <kw>` | exact match on a lower-cased keyword in `messages.flags` |
| `FROM <s>` | `messages.sender` / `message_recipients` where `kind = 'sender'` |
| `TO <s>`, `CC <s>`, `BCC <s>` | `message_recipients.address` where `kind` matches |
| `SUBJECT <s>` | `messages_subject_fts_idx`, a GIN index on `to_tsvector('simple', coalesce(subject, ''))` |
| `BODY <s>` | _(planned)_ — needs a body index; the Maildir has no index over message text |
| `TEXT <s>` | _(planned)_ — headers plus body |
| `HEADER <name> <s>` | _(planned)_ — headers are not stored relationally; `messages` keeps a denormalised subset |
| `LARGER <n>` / `SMALLER <n>` | `messages.size_bytes` |
| `BEFORE <date>` / `ON <date>` / `SINCE <date>` | `messages.internal_date` |
| `SENTBEFORE` / `SENTON` / `SENTSINCE` | `messages.sent_at` |
| `UID <set>` | `messages.uid` |
| `NEW`, `OLD`, `RECENT` | composed from the above |
| `NOT <key>`, `<key1> <key2>` (AND), `OR <key1> <key2>` | composed |
| `<sequence set> <key>` | composed |
| `CHARSET UTF-8` | accepted; anything else is `NO [BADCHARSET (UTF-8)]` |

`MessagesRepository::search(MessageSearch)` is the query builder behind all of
this; the API's `/api/v1/messages` filters and the Admin search use the same
function, so a `SEARCH` and an API query over the same fields return the same
messages. That is the layering rule doing real work: the IMAP layer contributes
only the parser.

Implementing `BODY`/`TEXT` search would mean either indexing message text in
PostgreSQL or walking the Maildir per query. Neither is acceptable for v1, so it
is explicitly _(planned)_ and clients that use it get `NO [CANNOT] BODY search is
not supported` rather than wrong results. The Admin/API side offers a
subject-and-header search that does work.

---

## 9. `IDLE` and the 29-minute rule

RFC 2177 recommends that a client not hold `IDLE` for more than 29 minutes, and
requires the server to terminate it. `config/ferroma.toml`:

```toml
[imap]
enable_idle = true
# Longest a client may hold IDLE, in seconds (RFC 2177 recommends < 30 min).
max_idle_secs = 1740
```

1740 seconds is 29 minutes exactly.

```text
C: A001 IDLE
S: + idling
      … the session stays Selected and pushes untagged updates …
C: DONE
S: A001 OK IDLE terminated
```

| Rule | Detail |
|---|---|
| `+ idling` | sent immediately; the connection is now in a literal-ish continuation, and only `DONE` is legal |
| What is pushed | `* n EXISTS`, `* n RECENT`, `* n FETCH (FLAGS …)`, `* n EXPUNGE` — a subset of the events that reach the session from `ferroma-events` |
| Data source | `EventBus::subscribe_filtered(EventScope::User(user_id))`, one subscription per selected folder per session |
| Server-side timeout | at `imap.max_idle_secs` the server sends `* BYE Idle timeout` and closes. A well-behaved client re-issues `IDLE`, which is what keeps a phone's socket alive |
| Anything but `DONE` | `BAD` |
| `IDLE` when `imap.enable_idle = false` | `BAD` and `IDLE` is not advertised |
| `IDLE` in `Authenticated` (nothing selected) | `BAD` — there is no folder to report on |
| Session idle timeout | `imap.idle_timeout_secs` (1800) is separate: it is the timeout for a session sending *nothing at all*, and `IDLE` on a live folder is not "nothing at all" |

The event bus is in-process only. A session `IDLE`ing on a folder gets pushed
updates for changes made in the same process; a change made by another process
sharing the database is not pushed and arrives on the next `NOOP`, `CHECK` or
`SELECT` — and is always visible to a client that reconnects. This is the same
limitation noted in [architecture.md](architecture.md) §6 and
[security.md](security.md).

`IDLE` is the reason most clients do not need `CONDSTORE`: the server pushes what
changed, so the client does not have to poll `STATUS` to find out.

---

## 10. `APPEND` and the Windows info separator

### 10.1 `APPEND` limits

| Limit | Value | Reply |
|---|---|---|
| Largest literal | `imap.max_append_size`, 26214400 (25 MiB) | `NO [TOOBIG] Literal too large` |
| Literal syntax | `{n}` and `{n+}` | — |
| Date-time | `APPEND "Sent" (\Seen) "16-Sep-2026 09:12:31 +0000" {n}` | an unparsable date is `BAD`, not ignored |
| Flags | any flag or keyword the server accepts | an unacceptable flag is `BAD` |
| Target folder must exist | yes | `NO [TRYCREATE]` — the RFC 3501 §6.3.11 signal that the client should `CREATE` first |
| Quota | `MailboxesRepository::check_quota` before the write | `NO [OVERQUOTA]` |
| Result | message stored in the Maildir, row inserted, UID allocated, change logged | `OK [APPENDUID <uidvalidity> <uid>]` when `UIDPLUS` is advertised |

`APPEND` stores the literal verbatim. Unlike submission, it does not prepend a
`Received:` header — the message did not travel over SMTP to get here, and
inventing a hop would corrupt the header chain a client is trying to preserve.
It does set flags, move the file into `cur/` or `new/` accordingly, and allocate a
fresh UID in the target folder.

`APPEND` to `Drafts` is how a third-party client's "save draft" ends up in the
same place as the official client's `POST /api/v1/client/drafts`; the two must
not diverge, so a server-side draft is mirrored into the Drafts folder with the
`\Draft` flag ([fcp.md](fcp.md) §7).

### 10.2 The Windows `;` versus `:` separator

A Maildir file name carries its flags after an "info" section:

```text
1758012751.M4821_P3210.mail:2,S
└───────── base ──────────┘ │ │
                            │ └── flags
                            └──── version
```

On Unix the separator is `:` — that is the de-facto standard every Maildir
implementation has used for twenty years. On Windows it cannot be, because NTFS
treats `name:stream` as an **alternate data stream**: creating a file literally
named `1758012751.M4821_P3210.mail:2,S` either fails or silently creates something
other than a normal file, and the flags would be invisible or the write would
fail outright. Maildir implementations on Windows have therefore used `;`
instead for just as long.

Ferroma carries both, in `crates/ferroma-storage/src/maildir.rs`:

```rust
/// The canonical Maildir "info" separator (RFC-less de-facto standard, Dovecot et al).
pub const INFO_SEPARATOR_UNIX: char = ':';

/// The separator used on filesystems that reserve `:` in file names.
pub const INFO_SEPARATOR_WINDOWS: char = ';';

/// The "info" separator this build writes.
pub fn info_separator() -> char {
    if cfg!(windows) { INFO_SEPARATOR_WINDOWS } else { INFO_SEPARATOR_UNIX }
}
```

The rule, and why both constants exist:

| Function | Behaviour |
|---|---|
| `info_separator()` | what *this build writes*: `;` on Windows, `:` elsewhere |
| `with_info(base, flags)` | appends `{sep}2,{flags}` using `info_separator()`; returns `base` unchanged when there are no flags |
| `split_info(file_name)` | **accepts either separator**, and both are tried, so a store written on Linux is readable on Windows and vice versa |

```rust
assert_eq!(split_info("1234.M1P.host:2,S"), ("1234.M1P.host", "S"));
assert_eq!(split_info("1234.M1P.host;2,FS"), ("1234.M1P.host", "FS"));
assert_eq!(split_info("1234.M1P.host"), ("1234.M1P.host", ""));
// Some tools omit the `2,` version marker.
assert_eq!(split_info("1234.M1P.host:S"), ("1234.M1P.host", "S"));
```

Three consequences:

1. **A mail store is portable.** Copying `Maildir/` from a Linux server to a
   Windows workstation and pointing Ferroma at it does not require rewriting a
   single file name — reads accept both forms. New writes use the local
   separator, so a tree can legitimately contain both, which `split_info` handles.
2. **`split_info` ignores a one-character prefix.** `if base.len() <= 1 { continue; }`
   exists because `C:` at the start of a Windows path looks exactly like an info
   separator; without the guard, a path prefix would be mistaken for flags.
3. **Mounting matters.** A Maildir on a CIFS/SMB share mounted on Linux will be
   written with `:` while the server is Linux — and if the far side is Windows,
   the write can still fail at the NTFS layer. The layout setting is deliberately
   per-deployment: `storage.layout = "maildir"` uses `Maildir/.Folder`
   subdirectories, and `"maildirperfolder"` uses one Maildir per folder
   (`<local>/INBOX`, `<local>/Sent`) with no dotted directory names, which is the
   safer choice on a filesystem with unusual name rules.

`sanitize_component` rejects `:` in every path component it is given, which keeps
a *folder* name from smuggling an info section into a directory name. Message
file names are built by `unique_filename`, never from user input.

---

## 11. Compatibility targets

Specification §12: *"Thunderbird、Apple Mail、Outlook、iPhone Mail、Android
邮件客户端能够逐步兼容"* — progressive compatibility with those five.

| Client | Platform | What it uses | Notes |
|---|---|---|---|
| **Thunderbird** | Windows, Linux, macOS | `CAPABILITY`, `LOGIN`/`AUTHENTICATE PLAIN`, `LIST`, `LSUB`, `SELECT`, `FETCH`, `STORE`, `SEARCH`, `IDLE`, `APPEND` | the strictest of the five: it uses `LSUB` for the folder pane, `IDLE` on the selected folder, `APPEND` into `Drafts`, and `UID SEARCH` after a resync. It asks for `NAMESPACE` and copes with `NO` |
| **Apple Mail** | macOS | the same set, plus `STATUS` polling and `UID FETCH` | expects `\Sent` / `\Drafts` / `\Trash` / `\Junk` special-use markers to place folders |
| **Outlook** | Windows | `CAPABILITY`, `LOGIN`, `SELECT`, `FETCH`, `STORE`, `APPEND`, `IDLE` | less tolerant of unadvertised extensions; must be tested against a real build |
| **iPhone Mail** | iOS | `CAPABILITY`, `LOGIN`, `SELECT`, `FETCH`, `STORE`, `APPEND`, `IDLE`, `SEARCH` | the most `IDLE`-dependent; a broken `IDLE` looks like "mail does not arrive" |
| **Android mail clients** | Android | `CAPABILITY`, `LOGIN`, `SELECT`, `FETCH`, `STORE`, `SEARCH` | K-9 Mail and FairEmail are the realistic targets; both handle a missing `MOVE` |

The compatibility bar for the first release, from specification §44:

```text
CAPABILITY   LOGIN   SELECT   FETCH   STORE   SEARCH   UID   IDLE
```

Each of these must be exercised against a real client, not only against a
conformance script. `openssl s_client -crlf -connect host:143` and a hand-typed
session catch most of what a test suite does not.

Interaction with the special-use markers (§4.3) is what makes folder placement
work across all five: a client that maps `\Sent` rather than guessing the name
`Sent` still lines up when an account's folder is called `Sent Items`.

---

## 12. Testing IMAP by hand

```bash
# Greeting, capabilities and a full login/select/fetch, unencrypted.
openssl s_client -crlf -connect 127.0.0.1:143

# Implicit TLS.
openssl s_client -connect 127.0.0.1:993

# Just the capabilities of a running server.
printf 'a CAPABILITY\r\nb LOGOUT\r\n' | openssl s_client -quiet -crlf -connect 127.0.0.1:143
```

Illustrative session output:

```text
* OK [CAPABILITY IMAP4rev1 STARTTLS AUTH=PLAIN AUTH=LOGIN IDLE MOVE UIDPLUS LITERAL+ CHILDREN] Ferroma IMAP4rev1 ready
a LOGIN alice@example.com "…"
a OK LOGIN completed
b LIST "" "*"
* LIST (\HasNoChildren) "/" "INBOX"
* LIST (\HasNoChildren \Sent) "/" "Sent"
* LIST (\HasNoChildren \Drafts) "/" "Drafts"
* LIST (\HasNoChildren) "/" "Archive"
* LIST (\HasChildren) "/" "Archive/2026"
b OK LIST completed
c SELECT INBOX
* 412 EXISTS
* 3 RECENT
* FLAGS (\Answered \Flagged \Deleted \Seen \Draft)
* OK [PERMANENTFLAGS (\Answered \Flagged \Deleted \Seen \Draft \*)] Flags permitted
* OK [UIDVALIDITY 1] UIDs valid
* OK [UIDNEXT 118] Predicted next UID
c OK [READ-WRITE] SELECT completed
d UID FETCH 117 (FLAGS RFC822.SIZE INTERNALDATE BODY.PEEK[HEADER.FIELDS (SUBJECT FROM)])
* 117 FETCH (UID 117 FLAGS (\Seen) RFC822.SIZE 24831 INTERNALDATE "16-Sep-2026 09:12:44 +0000" BODY[HEADER.FIELDS (SUBJECT FROM)] {68}
Subject: Invoice for September
From: Bob <bob@example.net>
)
d OK UID FETCH completed
e LOGOUT
* BYE Ferroma IMAP4rev1 server signing off
e OK LOGOUT completed
```

For "IMAP login fails", see [deployment.md](deployment.md) §11.

---

## 13. Related documents

| Topic | Document |
|---|---|
| Why IMAP and FCP both exist; the official client's own protocol | [fcp.md](fcp.md), [client.md](client.md) |
| Folder and message tables, Maildir delivery, integrity checks | [storage.md](storage.md) |
| Sync cursors, tombstones, conflict resolution | [sync.md](sync.md) |
| TLS, `require_tls_for_login`, threat model | [security.md](security.md), [deployment.md](deployment.md) |
| Crate layering, event bus, request lifecycle | [architecture.md](architecture.md) |

//! Local search over the cached SQLite database (specification §30, `docs/fcp.md` §10).
//!
//! Search is a *client* feature: it must work offline, so it runs against the
//! local cache first and only asks the server when the cache has no answer.
//!
//! ```text
//! 搜索本地缓存 → 没有结果 → 请求服务器搜索
//! ```
//!
//! # The query language
//!
//! A query is a whitespace-separated list of tokens:
//!
//! * `from:alice@example.com`, `to:bob`, `subject:invoice`, `body:agenda`,
//!   `attachment:pdf` — a text search restricted to one field;
//! * `has:attachment` — the message carries at least one attachment;
//! * `is:unread`, `is:flagged` — a flag test;
//! * `before:2026-01-01` (strictly older) and `after:2026-01-01` (on or after);
//! * `folder:INBOX` — the folder name, matched case-insensitively;
//! * anything else is a *bare term*, matched against every indexed field.
//!
//! A double-quoted value keeps its spaces: `subject:"invoice for september"`.
//! The tokenizer has no escape character — a `"` always opens or closes a quoted
//! segment — and an unterminated quote runs to the end of the input.
//!
//! Two rules are deliberate:
//!
//! * **An unknown key is not an error.** `colour:red` becomes a bare term that
//!   still contains its colon, so it is searched for literally: a typo narrows
//!   the result set instead of failing the query. The same holds for an unknown
//!   value of a known flag operator — `is:something-else` is a bare term, as is
//!   `has:whatever`.
//! * **A known operator with an empty value is an error.** `from:` is
//!   [`ClientError::Invalid`], and so is a `before:`/`after:` value that is not a
//!   `YYYY-MM-DD` date.
//!
//! [`SearchQuery::parse`] on an empty (or entirely blank) string yields an empty
//! query: [`SearchQuery::is_empty`] is `true`, which is legal — and matches
//! nothing.
//!
//! # Execution
//!
//! When the cache was opened in [`SearchMode::Fts5`], text is matched with an
//! FTS5 `MATCH` expression over `messages_fts`; when it was opened in
//! [`SearchMode::Like`] (a SQLite build without FTS5), the same fields are
//! scanned with `LIKE` over `messages`, `message_bodies` and `attachments`.
//! Either way the hits are then filtered by joining `messages` on
//! `(account_id, message_id)`, so a stale index entry can never surface a
//! message that is gone:
//!
//! * `deleted = 1` rows are never returned — a tombstone is not a message;
//! * `is:unread` means the `flags` do **not** contain `seen`, `is:flagged` means
//!   they do contain `flagged`;
//! * `before:`/`after:` compare `date(internal_date)` — the RFC 3339 timestamps
//!   the cache stores sort lexicographically, and `date()` makes the comparison a
//!   plain day comparison;
//! * `folder:` matches `folders.name` case-insensitively for the account.
//!
//! The text index holds only what the client has cached, so a query built purely
//! from flag and folder operators (`is:unread`, `folder:INBOX`) is answered from
//! `messages` itself: those are not text searches, and answering them from the
//! cache is both faster and more useful than hiding a message merely because its
//! index row has not been written yet. Every query that names a *term* — a bare
//! word or a `from:`/`to:`/`subject:`/`body:`/`attachment:` operator — goes
//! through the index, so removing a document from the index (a folder reset, a
//! wipe) removes its text hits immediately.
//!
//! Results are newest first (`internal_date DESC`, then `id DESC` so the order is
//! stable for messages that share a timestamp), capped at `limit`. The reporting
//! `total` is every match, not just the returned page.
//!
//! # The index
//!
//! [`index_document`], [`unindex_document`] and [`unindex_folder`] are the
//! maintenance entry points the sync engine calls **inside its own transaction**,
//! so the index and the rows it describes commit together. In
//! [`SearchMode::Like`] all three are no-ops: the fallback scans the tables
//! directly, which is exactly why it cannot go stale.
//!
//! In `messages_fts`, `account_id` is stored as **text** (and is bound as
//! `account_id.to_string()` accordingly); `message_id` is compared through a
//! `CAST(... AS INTEGER)` so the statement is correct whichever storage class
//! FTS5 used for the row.
//!
//! Message bodies are indexed and matched, but never logged.

use std::sync::Arc;

use chrono::NaiveDate;
use sqlx::{Row, SqliteConnection};

use crate::api::FcpClient;
use crate::database::{ClientDatabase, SearchDocument, SearchMode};
use crate::error::{ClientError, ClientResult};

/// Where a set of results came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchSource {
    /// The local SQLite cache — the normal case, and the only one that works
    /// offline.
    Local,
    /// `GET /search` on the server (`docs/fcp.md` §10), the fallback used when
    /// the cache has no answer.
    Server,
}

/// One operator of the search query language.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchOp {
    /// `from:<address>` — the sender address.
    From(String),
    /// `to:<address>` — one of the recipients.
    To(String),
    /// `subject:<text>` — the subject.
    Subject(String),
    /// `body:<text>` — the text body.
    Body(String),
    /// `attachment:<text>` — an attachment file name.
    Attachment(String),
    /// `has:attachment` — the message has at least one attachment.
    HasAttachment,
    /// `is:unread` — the message has no `seen` flag.
    IsUnread,
    /// `is:flagged` — the message carries the `flagged` flag.
    IsFlagged,
    /// `before:<YYYY-MM-DD>` — strictly older than that day.
    Before(NaiveDate),
    /// `after:<YYYY-MM-DD>` — that day or newer.
    After(NaiveDate),
    /// `folder:<name>` — the folder name, matched case-insensitively.
    Folder(String),
}

/// A parsed search query: the bare terms plus the typed operators.
///
/// [`SearchQuery::parse`] is the only way to build one from user input; the
/// public fields exist so a UI can inspect or extend a query (a saved search, a
/// chip the user removed) without re-parsing it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SearchQuery {
    /// Bare words, matched against every indexed field.
    pub terms: Vec<String>,
    /// The `key:value` operators and the flag operators, in input order.
    pub operators: Vec<SearchOp>,
}

impl SearchQuery {
    /// Parse the query language (`docs/fcp.md` §10).
    ///
    /// ```
    /// # use ferroma_client::search::SearchQuery;
    /// let query = SearchQuery::parse("from:bob subject:\"invoice for september\" is:unread").expect("parse");
    /// assert_eq!(query.operators.len(), 3);
    /// assert_eq!(query.to_wire(), "from:bob subject:\"invoice for september\" is:unread");
    /// ```
    ///
    /// # Errors
    ///
    /// [`ClientError::Invalid`] when a *known* operator has an empty value
    /// (`from:`, `is:`, `has:`) or when `before:`/`after:` is not a
    /// `YYYY-MM-DD` date. An unknown key is never an error: `colour:red` becomes
    /// a bare term that keeps its colon.
    pub fn parse(input: &str) -> ClientResult<SearchQuery> {
        let mut query = SearchQuery::default();
        for token in tokenize(input) {
            parse_token(&token, &mut query)?;
        }
        Ok(query)
    }

    /// Whether the query constrains nothing — `parse("")`, or a string of blanks.
    ///
    /// An empty query is legal and matches nothing; the executors return an empty
    /// result for it rather than running an unconstrained scan.
    pub fn is_empty(&self) -> bool {
        self.terms.is_empty() && self.operators.is_empty()
    }

    /// Render the query back to the `from:x subject:"y z"` form.
    ///
    /// This is what the server fallback sends as `q` (`docs/fcp.md` §10), so the
    /// round trip `parse(q.to_wire()) == q` holds for every query the parser can
    /// produce. A value that contains whitespace is quoted; a value that contains
    /// a double quote is emitted verbatim (the tokenizer has no escape for it).
    pub fn to_wire(&self) -> String {
        let mut parts: Vec<String> = Vec::with_capacity(self.terms.len() + self.operators.len());
        for term in &self.terms {
            parts.push(render_term(term));
        }
        for op in &self.operators {
            parts.push(render_op(op));
        }
        parts.join(" ")
    }
}

/// One hit of a search: the list-view fields of a message, nothing more.
///
/// No body is ever carried in a hit — the reading pane downloads it through the
/// normal message path, which keeps a search result cheap to hold and impossible
/// to leak into a log.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    /// The account the message belongs to.
    pub account_id: i64,
    /// The server-side message id.
    pub message_id: i64,
    /// The folder the message lives in, when the cache knows it.
    pub folder_id: Option<i64>,
    /// The subject.
    pub subject: String,
    /// The sender address.
    pub from_address: String,
    /// A short plain-text preview, when one was cached.
    pub snippet: Option<String>,
    /// Space separated IMAP flags.
    pub flags: String,
    /// When the server received the message, as an RFC 3339 string.
    pub internal_date: Option<String>,
}

/// The answer to one search: the page of hits plus where they came from.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchResults {
    /// The hits, newest first, at most `limit` of them.
    pub hits: Vec<SearchHit>,
    /// Whether these came from the cache or from the server.
    pub source: SearchSource,
    /// Every match, not just the returned page.
    pub total: i64,
}

/// Runs a [`SearchQuery`] against the local cache, with an optional server
/// fallback (`docs/fcp.md` §10).
///
/// Cheap to clone: the database and the HTTP client are both shared handles.
#[derive(Debug, Clone)]
pub struct SearchExecutor {
    /// The cache every query runs against.
    db: Arc<ClientDatabase>,
    /// The FCP client used when the cache has no answer. `None` keeps search
    /// local-only, which is what an offline or signed-out client gets.
    client: Option<FcpClient>,
}

impl SearchExecutor {
    /// A local-only executor: search never leaves the cache.
    pub fn local(db: Arc<ClientDatabase>) -> Self {
        SearchExecutor { db, client: None }
    }

    /// An executor that falls back to the server when the cache has no answer.
    pub fn with_server(db: Arc<ClientDatabase>, client: FcpClient) -> Self {
        SearchExecutor {
            db,
            client: Some(client),
        }
    }

    /// Search the local cache; when it yields nothing and a server client is
    /// configured, fall back to `GET /search` (§10).
    ///
    /// The fallback only fires for a query that constrains *something*: an empty
    /// query matches nothing by definition, and asking the server for it would
    /// return the whole mailbox.
    pub async fn search(
        &self,
        account_id: i64,
        query: &SearchQuery,
        limit: usize,
    ) -> ClientResult<SearchResults> {
        let local = self.search_local(account_id, query, limit).await?;
        if !local.hits.is_empty() || query.is_empty() {
            return Ok(local);
        }

        let Some(client) = self.client.clone() else {
            return Ok(local);
        };

        let page = client
            .search(&query.to_wire(), None, None, Some(limit))
            .await?;
        let hits: Vec<SearchHit> = page
            .items
            .iter()
            .map(|item| hit_from_item(account_id, item))
            .collect();
        tracing::debug!(
            hits = hits.len(),
            total = page.total,
            "server search answered a query the local cache could not"
        );
        Ok(SearchResults {
            hits,
            source: SearchSource::Server,
            total: page.total,
        })
    }

    /// Search only the local cache (never the server).
    ///
    /// This is the offline path, and the one the reading pane's "find in this
    /// mailbox" uses even when the network is up.
    pub async fn search_local(
        &self,
        account_id: i64,
        query: &SearchQuery,
        limit: usize,
    ) -> ClientResult<SearchResults> {
        if query.is_empty() {
            return Ok(SearchResults {
                hits: Vec::new(),
                source: SearchSource::Local,
                total: 0,
            });
        }
        let mut conn = self.db.pool().acquire().await?;
        let (hits, total) =
            query_hits(&mut conn, self.db.search_mode(), account_id, query, limit).await?;
        Ok(SearchResults {
            hits,
            source: SearchSource::Local,
            total,
        })
    }
}

/// Add (or replace) one document in the local search index.
///
/// Called by the sync engine *inside* the transaction that wrote the message, so
/// the index can never advance past the rows it describes. Indexing the same
/// document twice replaces it instead of duplicating it.
///
/// In [`SearchMode::Like`] this is a no-op: there is no index, and search scans
/// `messages` / `message_bodies` / `attachments` directly.
pub async fn index_document(
    conn: &mut SqliteConnection,
    mode: SearchMode,
    doc: &SearchDocument,
) -> ClientResult<()> {
    if mode != SearchMode::Fts5 {
        return Ok(());
    }
    unindex_document(conn, mode, doc.account_id, doc.message_id).await?;
    sqlx::query(
        "INSERT INTO messages_fts \
         (subject, sender, recipients, body, attachments, account_id, message_id) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&doc.subject)
    .bind(&doc.sender)
    .bind(&doc.recipients)
    .bind(&doc.body)
    .bind(&doc.attachments)
    .bind(doc.account_id.to_string())
    .bind(doc.message_id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Drop one document from the local search index.
///
/// Idempotent: removing a document that was never indexed (or is already gone)
/// succeeds. A no-op in [`SearchMode::Like`].
pub async fn unindex_document(
    conn: &mut SqliteConnection,
    mode: SearchMode,
    account_id: i64,
    message_id: i64,
) -> ClientResult<()> {
    if mode != SearchMode::Fts5 {
        return Ok(());
    }
    sqlx::query(
        "DELETE FROM messages_fts \
         WHERE CAST(account_id AS TEXT) = ? AND CAST(message_id AS INTEGER) = ?",
    )
    .bind(account_id.to_string())
    .bind(message_id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Drop every indexed document of one folder.
///
/// Used when a folder's cache is cleared (§3 `uid_validity` invalidation, a
/// resync, a wipe): the folder's message ids are read from the cache and each one
/// is unindexed, so an interrupted index can never leave an orphan row behind. A
/// no-op in [`SearchMode::Like`].
pub async fn unindex_folder(
    conn: &mut SqliteConnection,
    mode: SearchMode,
    account_id: i64,
    folder_id: i64,
) -> ClientResult<()> {
    if mode != SearchMode::Fts5 {
        return Ok(());
    }
    let rows = sqlx::query("SELECT id FROM messages WHERE account_id = ? AND folder_id = ?")
        .bind(account_id)
        .bind(folder_id)
        .fetch_all(&mut *conn)
        .await?;
    let ids: Vec<i64> = rows
        .into_iter()
        .map(|row| row.get::<i64, _>("id"))
        .collect();
    for message_id in ids {
        unindex_document(conn, mode, account_id, message_id).await?;
    }
    Ok(())
}

// -- query execution -------------------------------------------------------

/// One bound parameter of the dynamically-built search statement.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Bind {
    /// An integer (the account id, a `LIMIT`).
    Int(i64),
    /// Text (a `LIKE` pattern, a date, a folder name).
    Text(String),
}

/// Apply every binding, in order, to a statement.
///
/// The search statement is assembled at run time, so `sqlx`'s typed builder chain
/// cannot be written out literally; this keeps the bind order next to the SQL
/// that produced it. `macro_rules!` is scoped textually, so this has to sit above
/// its first use.
macro_rules! bind_all {
    ($statement:expr, $binds:expr) => {{
        let mut statement = $statement;
        for value in $binds {
            statement = match value {
                Bind::Int(number) => statement.bind(*number),
                Bind::Text(text) => statement.bind(text.clone()),
            };
        }
        statement
    }};
}

/// Run one query against the cache and return `(page, total)`.
async fn query_hits(
    conn: &mut SqliteConnection,
    mode: SearchMode,
    account_id: i64,
    query: &SearchQuery,
    limit: usize,
) -> ClientResult<(Vec<SearchHit>, i64)> {
    let (where_sql, binds) = build_where(mode, account_id, query);

    let count_sql = format!("SELECT COUNT(*) AS n FROM messages m WHERE {where_sql}");
    let count_statement = bind_all!(sqlx::query(&count_sql), &binds);
    let count_row = count_statement.fetch_one(&mut *conn).await?;
    let total: i64 = count_row.get("n");

    let select_sql = format!(
        "SELECT m.account_id AS account_id, m.id AS message_id, m.folder_id AS folder_id, \
         m.subject AS subject, m.from_address AS from_address, m.snippet AS snippet, \
         m.flags AS flags, m.internal_date AS internal_date \
         FROM messages m WHERE {where_sql} \
         ORDER BY m.internal_date DESC, m.id DESC LIMIT ?"
    );
    let select_statement = bind_all!(sqlx::query(&select_sql), &binds);
    let rows = select_statement
        .bind(limit as i64)
        .fetch_all(&mut *conn)
        .await?;

    let hits: Vec<SearchHit> = rows.into_iter().map(hit_from_row).collect();
    Ok((hits, total))
}

/// Build the `WHERE` clause and its bindings for one query.
///
/// The clause always starts with the two invariants: the account scope and
/// `deleted = 1` tombstones. Everything else is appended in a fixed order, and
/// the bindings are pushed in exactly the order the `?` placeholders appear.
fn build_where(mode: SearchMode, account_id: i64, query: &SearchQuery) -> (String, Vec<Bind>) {
    let mut clauses: Vec<String> =
        vec!["m.account_id = ?".to_string(), "m.deleted = 0".to_string()];
    let mut binds: Vec<Bind> = vec![Bind::Int(account_id)];

    let text = text_clauses(query);
    if !text.is_empty() {
        if mode == SearchMode::Fts5 {
            let mut parts: Vec<String> = Vec::new();
            for (target, value) in &text {
                if !value.is_empty() {
                    parts.push(fts_target_clause(*target, value));
                }
            }
            if parts.is_empty() {
                clauses.push(NEVER.to_string());
            } else {
                clauses.push(FTS_SUBQUERY.to_string());
                binds.push(Bind::Text(parts.join(" AND ")));
                binds.push(Bind::Text(account_id.to_string()));
            }
        } else {
            for (target, value) in &text {
                if value.is_empty() {
                    clauses.push(NEVER.to_string());
                    continue;
                }
                let pattern = Bind::Text(like_pattern(value));
                match target {
                    TextTarget::Any => {
                        clauses.push(LIKE_ANY.to_string());
                        for _ in 0..5 {
                            binds.push(pattern.clone());
                        }
                    }
                    TextTarget::Subject => {
                        clauses.push("m.subject LIKE ? ESCAPE '\\'".to_string());
                        binds.push(pattern);
                    }
                    TextTarget::Sender => {
                        clauses.push("m.from_address LIKE ? ESCAPE '\\'".to_string());
                        binds.push(pattern);
                    }
                    TextTarget::Recipients => {
                        clauses.push("IFNULL(m.to_summary, '') LIKE ? ESCAPE '\\'".to_string());
                        binds.push(pattern);
                    }
                    TextTarget::Body => {
                        clauses.push(LIKE_BODY.to_string());
                        binds.push(pattern);
                    }
                    TextTarget::Attachments => {
                        clauses.push(LIKE_ATTACHMENT.to_string());
                        binds.push(pattern);
                    }
                }
            }
        }
    }

    for op in &query.operators {
        match op {
            SearchOp::HasAttachment => clauses.push(HAS_ATTACHMENT.to_string()),
            SearchOp::IsUnread => clauses.push("instr(lower(m.flags), 'seen') = 0".to_string()),
            SearchOp::IsFlagged => clauses.push("instr(lower(m.flags), 'flagged') > 0".to_string()),
            SearchOp::Before(date) => {
                clauses.push("date(m.internal_date) < ?".to_string());
                binds.push(Bind::Text(date.to_string()));
            }
            SearchOp::After(date) => {
                clauses.push("date(m.internal_date) >= ?".to_string());
                binds.push(Bind::Text(date.to_string()));
            }
            SearchOp::Folder(name) => {
                clauses.push(
                    "m.folder_id IN (SELECT id FROM folders \
                     WHERE account_id = ? AND lower(name) = lower(?))"
                        .to_string(),
                );
                binds.push(Bind::Int(account_id));
                binds.push(Bind::Text(name.clone()));
            }
            SearchOp::From(_)
            | SearchOp::To(_)
            | SearchOp::Subject(_)
            | SearchOp::Body(_)
            | SearchOp::Attachment(_) => {}
        }
    }

    (clauses.join(" AND "), binds)
}

/// The text part of a query: which field to search, and what for.
fn text_clauses(query: &SearchQuery) -> Vec<(TextTarget, String)> {
    let mut out: Vec<(TextTarget, String)> = Vec::new();
    for term in &query.terms {
        out.push((TextTarget::Any, term.clone()));
    }
    for op in &query.operators {
        match op {
            SearchOp::From(value) => out.push((TextTarget::Sender, value.clone())),
            SearchOp::To(value) => out.push((TextTarget::Recipients, value.clone())),
            SearchOp::Subject(value) => out.push((TextTarget::Subject, value.clone())),
            SearchOp::Body(value) => out.push((TextTarget::Body, value.clone())),
            SearchOp::Attachment(value) => out.push((TextTarget::Attachments, value.clone())),
            SearchOp::HasAttachment
            | SearchOp::IsUnread
            | SearchOp::IsFlagged
            | SearchOp::Before(_)
            | SearchOp::After(_)
            | SearchOp::Folder(_) => {}
        }
    }
    out
}

/// One FTS5 clause: a quoted phrase, restricted to one column (or to the five
/// text columns).
///
/// Every user value is a quoted *phrase* — `"` doubled by
/// [`crate::util::escape_fts`] — so FTS5's own operators (`OR`, `-`, `*`, `NEAR`)
/// can never be injected from the search box.
fn fts_target_clause(target: TextTarget, value: &str) -> String {
    let literal = format!("\"{}\"", crate::util::escape_fts(value));
    match target {
        TextTarget::Any => format!(
            "(subject:{literal} OR sender:{literal} OR recipients:{literal} \
             OR body:{literal} OR attachments:{literal})"
        ),
        TextTarget::Subject => format!("subject:{literal}"),
        TextTarget::Sender => format!("sender:{literal}"),
        TextTarget::Recipients => format!("recipients:{literal}"),
        TextTarget::Body => format!("body:{literal}"),
        TextTarget::Attachments => format!("attachments:{literal}"),
    }
}

/// The `LIKE` pattern for one value: escaped, then wrapped in wildcards.
fn like_pattern(value: &str) -> String {
    format!("%{}%", crate::util::escape_like(value))
}

/// Turn one cache row into a hit.
fn hit_from_row(row: sqlx::sqlite::SqliteRow) -> SearchHit {
    SearchHit {
        account_id: row.get::<i64, _>("account_id"),
        message_id: row.get::<i64, _>("message_id"),
        folder_id: row.get::<Option<i64>, _>("folder_id"),
        subject: row.get::<String, _>("subject"),
        from_address: row.get::<String, _>("from_address"),
        snippet: row.get::<Option<String>, _>("snippet"),
        flags: row.get::<String, _>("flags"),
        internal_date: row.get::<Option<String>, _>("internal_date"),
    }
}

/// Turn one server list item into a hit (`docs/fcp.md` §10 returns message list
/// items, so the mapping is the one the list view already uses).
fn hit_from_item(account_id: i64, item: &crate::api::MessageItem) -> SearchHit {
    SearchHit {
        account_id,
        message_id: item.id,
        folder_id: item.folder_id,
        subject: item.subject.clone().unwrap_or_default(),
        from_address: item
            .from
            .as_ref()
            .map(|from| from.address.clone())
            .unwrap_or_default(),
        snippet: item.snippet.clone(),
        flags: item.flags.clone().unwrap_or_default(),
        internal_date: item.internal_date.clone(),
    }
}

// -- query parsing helpers -------------------------------------------------

/// One whitespace-separated token, with its quotes already removed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Token {
    /// The literal text, without its surrounding quotes.
    text: String,
    /// Whether the token *began* inside a quoted segment (`"a:b"` is a term,
    /// `subject:"a b"` is an operator).
    leading_quoted: bool,
}

/// Split `input` on whitespace, keeping quoted segments together.
fn tokenize(input: &str) -> Vec<Token> {
    let mut tokens: Vec<Token> = Vec::new();
    let mut current: Option<Token> = None;
    let mut quoted = false;

    for ch in input.chars() {
        if ch == '"' {
            if current.is_none() {
                current = Some(Token {
                    text: String::new(),
                    leading_quoted: true,
                });
            }
            quoted = !quoted;
            continue;
        }
        if ch.is_whitespace() && !quoted {
            if let Some(token) = current.take() {
                tokens.push(token);
            }
            continue;
        }
        if current.is_none() {
            current = Some(Token {
                text: String::new(),
                leading_quoted: false,
            });
        }
        if let Some(token) = current.as_mut() {
            token.text.push(ch);
        }
    }

    if let Some(token) = current.take() {
        tokens.push(token);
    }
    tokens
}

/// Fold one token into the query under construction.
fn parse_token(token: &Token, query: &mut SearchQuery) -> ClientResult<()> {
    // `""` is an empty token: it contributes nothing (and an empty term could
    // never match anyway).
    if token.text.is_empty() {
        return Ok(());
    }
    // A token that starts inside quotes is a literal, never an operator.
    if token.leading_quoted {
        query.terms.push(token.text.clone());
        return Ok(());
    }

    let Some((key, value)) = token.text.split_once(':') else {
        query.terms.push(token.text.clone());
        return Ok(());
    };
    let key = key.to_ascii_lowercase();

    match key.as_str() {
        "from" | "to" | "subject" | "body" | "attachment" => {
            let value = require_value(&key, value)?;
            query.operators.push(match key.as_str() {
                "from" => SearchOp::From(value),
                "to" => SearchOp::To(value),
                "subject" => SearchOp::Subject(value),
                "body" => SearchOp::Body(value),
                _ => SearchOp::Attachment(value),
            });
        }
        "folder" => {
            let value = require_value(&key, value)?;
            query.operators.push(SearchOp::Folder(value));
        }
        "has" => {
            let value = require_value(&key, value)?;
            if value.eq_ignore_ascii_case("attachment") {
                query.operators.push(SearchOp::HasAttachment);
            } else {
                // An unknown flag value is a term, not an error.
                query.terms.push(token.text.clone());
            }
        }
        "is" => {
            let value = require_value(&key, value)?;
            if value.eq_ignore_ascii_case("unread") {
                query.operators.push(SearchOp::IsUnread);
            } else if value.eq_ignore_ascii_case("flagged") {
                query.operators.push(SearchOp::IsFlagged);
            } else {
                query.terms.push(token.text.clone());
            }
        }
        "before" | "after" => {
            let value = require_value(&key, value)?;
            let date = NaiveDate::parse_from_str(&value, "%Y-%m-%d").map_err(|_| {
                ClientError::invalid(format!("{key}:{value} is not a YYYY-MM-DD date"))
            })?;
            query.operators.push(if key == "before" {
                SearchOp::Before(date)
            } else {
                SearchOp::After(date)
            });
        }
        // An unknown key is not an error: the token stays a bare term, colon and
        // all, so `colour:red` is searched for literally.
        _ => query.terms.push(token.text.clone()),
    }

    Ok(())
}

/// The value of a known operator, refusing an empty one.
fn require_value(key: &str, value: &str) -> ClientResult<String> {
    if value.is_empty() {
        return Err(ClientError::invalid(format!(
            "the {key}: operator needs a value"
        )));
    }
    Ok(value.to_string())
}

/// Render a bare term for the wire, quoting it when it contains whitespace.
fn render_term(term: &str) -> String {
    if term.is_empty() || term.chars().any(char::is_whitespace) {
        format!("\"{term}\"")
    } else {
        term.to_string()
    }
}

/// Render an operator for the wire (`docs/fcp.md` §10 uses the same syntax the
/// client parses).
fn render_op(op: &SearchOp) -> String {
    match op {
        SearchOp::From(value) => render_key_value("from", value),
        SearchOp::To(value) => render_key_value("to", value),
        SearchOp::Subject(value) => render_key_value("subject", value),
        SearchOp::Body(value) => render_key_value("body", value),
        SearchOp::Attachment(value) => render_key_value("attachment", value),
        SearchOp::HasAttachment => "has:attachment".to_string(),
        SearchOp::IsUnread => "is:unread".to_string(),
        SearchOp::IsFlagged => "is:flagged".to_string(),
        SearchOp::Before(date) => format!("before:{date}"),
        SearchOp::After(date) => format!("after:{date}"),
        SearchOp::Folder(value) => render_key_value("folder", value),
    }
}

/// `key:value`, with the value quoted when it contains whitespace.
fn render_key_value(key: &str, value: &str) -> String {
    if value.is_empty() || value.chars().any(char::is_whitespace) {
        format!("{key}:\"{value}\"")
    } else {
        format!("{key}:{value}")
    }
}

// -- SQL helpers -----------------------------------------------------------

/// Which field a text clause searches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextTarget {
    /// Every indexed field — what a bare term gets.
    Any,
    /// The subject.
    Subject,
    /// The sender.
    Sender,
    /// The recipients.
    Recipients,
    /// The text body.
    Body,
    /// Attachment file names.
    Attachments,
}

/// A predicate that is true for nothing — used for a text clause with an empty
/// value, which can only come from a hand-built query.
const NEVER: &str = "0 = 1";

/// The FTS5 half of a text query: `messages_fts` holds `account_id` as text, and
/// `message_id` is cast on both sides so the comparison holds whichever storage
/// class FTS5 gave the row.
const FTS_SUBQUERY: &str = "m.id IN (SELECT CAST(message_id AS INTEGER) FROM messages_fts \
     WHERE messages_fts MATCH ? AND CAST(account_id AS TEXT) = ?)";

/// A bare term, scanned across every field the index would have covered.
const LIKE_ANY: &str = "(m.subject LIKE ? ESCAPE '\\' \
     OR m.from_address LIKE ? ESCAPE '\\' \
     OR IFNULL(m.to_summary, '') LIKE ? ESCAPE '\\' \
     OR EXISTS (SELECT 1 FROM message_bodies b \
                WHERE b.account_id = m.account_id AND b.message_id = m.id \
                  AND IFNULL(b.text_body, '') LIKE ? ESCAPE '\\') \
     OR EXISTS (SELECT 1 FROM attachments a \
                WHERE a.account_id = m.account_id AND a.message_id = m.id \
                  AND a.filename LIKE ? ESCAPE '\\'))";

/// `body:` in the `LIKE` fallback — the body lives in `message_bodies`.
const LIKE_BODY: &str = "EXISTS (SELECT 1 FROM message_bodies b \
     WHERE b.account_id = m.account_id AND b.message_id = m.id \
       AND IFNULL(b.text_body, '') LIKE ? ESCAPE '\\')";

/// `attachment:` in the `LIKE` fallback — the names live in `attachments`.
const LIKE_ATTACHMENT: &str = "EXISTS (SELECT 1 FROM attachments a \
     WHERE a.account_id = m.account_id AND a.message_id = m.id \
       AND a.filename LIKE ? ESCAPE '\\')";

/// `has:attachment`: either the cached flag or a real attachment row.
const HAS_ATTACHMENT: &str = "(m.has_attachments = 1 \
     OR EXISTS (SELECT 1 FROM attachments a \
                WHERE a.account_id = m.account_id AND a.message_id = m.id))";

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use crate::api::{MemoryTokenStore, RetryPolicy};
    use crate::testutil::{MockServer, TempDir};

    // -- seeding -----------------------------------------------------------

    /// Insert one cached message plus its body.
    #[allow(clippy::too_many_arguments)]
    async fn insert_message(
        db: &ClientDatabase,
        account_id: i64,
        id: i64,
        folder_id: i64,
        subject: &str,
        from_address: &str,
        flags: &str,
        internal_date: &str,
        has_attachments: bool,
        deleted: bool,
        body: &str,
    ) {
        sqlx::query(
            "INSERT INTO messages \
             (account_id, id, folder_id, mailbox_id, subject, from_address, to_summary, snippet, \
              flags, size_bytes, has_attachments, attachment_count, internal_date, body_state, \
              deleted, cached_at) \
             VALUES (?, ?, ?, 3, ?, ?, 'alice@example.com', ?, ?, 1024, ?, ?, ?, 'ready', ?, 'now')",
        )
        .bind(account_id)
        .bind(id)
        .bind(folder_id)
        .bind(subject)
        .bind(from_address)
        .bind(format!("{subject} …"))
        .bind(flags)
        .bind(i64::from(has_attachments))
        .bind(i64::from(has_attachments))
        .bind(internal_date)
        .bind(i64::from(deleted))
        .execute(db.pool())
        .await
        .expect("insert message");

        sqlx::query(
            "INSERT INTO message_bodies (account_id, message_id, text_body, fetched_at) \
             VALUES (?, ?, ?, 'now')",
        )
        .bind(account_id)
        .bind(id)
        .bind(body)
        .execute(db.pool())
        .await
        .expect("insert body");
    }

    /// The five documents the seeded cache indexes.
    fn documents() -> Vec<SearchDocument> {
        vec![
            SearchDocument {
                account_id: 1,
                message_id: 101,
                folder_id: Some(5),
                subject: "Invoice for September".to_string(),
                sender: "billing@example.com".to_string(),
                recipients: "alice@example.com".to_string(),
                body: "Please find the attached invoice for September in the portal.".to_string(),
                attachments: "invoice-september.pdf".to_string(),
            },
            SearchDocument {
                account_id: 1,
                message_id: 102,
                folder_id: Some(5),
                subject: "Lunch tomorrow".to_string(),
                sender: "bob@example.com".to_string(),
                recipients: "alice@example.com".to_string(),
                body: "Are we still on for lunch tomorrow?".to_string(),
                attachments: String::new(),
            },
            SearchDocument {
                account_id: 1,
                message_id: 103,
                folder_id: Some(5),
                subject: "Project plan for Q4".to_string(),
                sender: "carol@example.com".to_string(),
                recipients: "alice@example.com".to_string(),
                body: "Here is the project plan for Q4.".to_string(),
                attachments: String::new(),
            },
            SearchDocument {
                account_id: 1,
                message_id: 104,
                folder_id: Some(6),
                subject: "Old newsletter".to_string(),
                sender: "news@example.com".to_string(),
                recipients: "alice@example.com".to_string(),
                body: "December newsletter with the usual links.".to_string(),
                attachments: String::new(),
            },
            // Indexed even though the message is a tombstone: the row filter, not
            // the index, is what hides it.
            SearchDocument {
                account_id: 1,
                message_id: 105,
                folder_id: Some(5),
                subject: "Deleted spam".to_string(),
                sender: "spam@example.com".to_string(),
                recipients: "alice@example.com".to_string(),
                body: "buy now, limited offer".to_string(),
                attachments: String::new(),
            },
            SearchDocument {
                account_id: 2,
                message_id: 201,
                folder_id: Some(5),
                subject: "Invoice for another account".to_string(),
                sender: "eve@example.com".to_string(),
                recipients: "alice@example.com".to_string(),
                body: "the invoice for another account".to_string(),
                attachments: String::new(),
            },
        ]
    }

    /// Two accounts, two folders, six messages (one deleted), one attachment.
    async fn seed_rows(db: &ClientDatabase) {
        for (id, email) in [(1i64, "alice@example.com"), (2, "bob@example.com")] {
            sqlx::query(
                "INSERT INTO accounts (id, email, base_url, device_uid, created_at, updated_at) \
                 VALUES (?, ?, 'http://localhost/api/v1/client', 'dev-1', 'now', 'now')",
            )
            .bind(id)
            .bind(email)
            .execute(db.pool())
            .await
            .expect("insert account");
        }
        for (account_id, folder_id, name) in
            [(1i64, 5i64, "INBOX"), (1, 6, "Archive"), (2, 5, "INBOX")]
        {
            sqlx::query(
                "INSERT INTO folders (account_id, id, mailbox_id, name) VALUES (?, ?, 3, ?)",
            )
            .bind(account_id)
            .bind(folder_id)
            .bind(name)
            .execute(db.pool())
            .await
            .expect("insert folder");
        }

        insert_message(
            db,
            1,
            101,
            5,
            "Invoice for September",
            "billing@example.com",
            "",
            "2026-09-16T12:00:00Z",
            true,
            false,
            "Please find the attached invoice for September in the portal.",
        )
        .await;
        insert_message(
            db,
            1,
            102,
            5,
            "Lunch tomorrow",
            "bob@example.com",
            "seen",
            "2026-09-10T09:00:00Z",
            false,
            false,
            "Are we still on for lunch tomorrow?",
        )
        .await;
        insert_message(
            db,
            1,
            103,
            5,
            "Project plan for Q4",
            "carol@example.com",
            "seen flagged",
            "2026-09-20T18:30:00Z",
            false,
            false,
            "Here is the project plan for Q4.",
        )
        .await;
        insert_message(
            db,
            1,
            104,
            6,
            "Old newsletter",
            "news@example.com",
            "",
            "2025-12-01T08:00:00Z",
            false,
            false,
            "December newsletter with the usual links.",
        )
        .await;
        insert_message(
            db,
            1,
            105,
            5,
            "Deleted spam",
            "spam@example.com",
            "",
            "2026-08-01T10:00:00Z",
            false,
            true,
            "buy now, limited offer",
        )
        .await;
        insert_message(
            db,
            2,
            201,
            5,
            "Invoice for another account",
            "eve@example.com",
            "",
            "2026-09-16T12:00:00Z",
            false,
            false,
            "the invoice for another account",
        )
        .await;

        sqlx::query(
            "INSERT INTO attachments (account_id, id, message_id, filename, content_type, size_bytes) \
             VALUES (1, 900, 101, 'invoice-september.pdf', 'application/pdf', 4096)",
        )
        .execute(db.pool())
        .await
        .expect("insert attachment");
    }

    /// Index every seeded document through the public entry point.
    async fn index_documents(db: &ClientDatabase) {
        let mode = db.search_mode();
        let mut conn = db.pool().acquire().await.expect("connection");
        for doc in documents() {
            index_document(&mut conn, mode, &doc).await.expect("index");
        }
    }

    /// A cache with the detected search backend, seeded and indexed.
    async fn seeded() -> (TempDir, Arc<ClientDatabase>, i64) {
        let dir = TempDir::new().expect("temp dir");
        let db = ClientDatabase::open(dir.path().join("cache.db"))
            .await
            .expect("open");
        seed_rows(&db).await;
        index_documents(&db).await;
        (dir, Arc::new(db), 1)
    }

    /// The same cache with the `LIKE` fallback forced (`SearchMode::Like`).
    async fn seeded_like() -> (TempDir, Arc<ClientDatabase>, i64) {
        let dir = TempDir::new().expect("temp dir");
        let db =
            ClientDatabase::open_with_search_mode(dir.path().join("cache.db"), SearchMode::Like)
                .await
                .expect("open");
        seed_rows(&db).await;
        index_documents(&db).await;
        (dir, Arc::new(db), 1)
    }

    /// Run one query locally and return the whole result.
    async fn results_for(
        db: &Arc<ClientDatabase>,
        account_id: i64,
        query: &str,
        limit: usize,
    ) -> SearchResults {
        let executor = SearchExecutor::local(Arc::clone(db));
        let parsed = SearchQuery::parse(query).expect("parse");
        let results = executor
            .search_local(account_id, &parsed, limit)
            .await
            .expect("search");
        assert_eq!(results.source, SearchSource::Local);
        results
    }

    /// The ids a query finds, newest first.
    async fn ids(db: &Arc<ClientDatabase>, account_id: i64, query: &str) -> Vec<i64> {
        results_for(db, account_id, query, 50)
            .await
            .hits
            .iter()
            .map(|hit| hit.message_id)
            .collect()
    }

    /// Whether the cache under test really uses the FTS5 index.
    fn has_index(db: &ClientDatabase) -> bool {
        db.search_mode() == SearchMode::Fts5
    }

    /// A date, for the parser assertions and the `SearchOp` literals.
    fn date(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).expect("valid date")
    }

    // -- the parser --------------------------------------------------------

    #[test]
    fn a_bare_word_is_a_term() {
        let query = SearchQuery::parse("invoice").expect("parse");
        assert_eq!(query.terms, vec!["invoice".to_string()]);
        assert!(query.operators.is_empty());
        assert!(!query.is_empty());
    }

    #[test]
    fn whitespace_separates_tokens_and_blank_input_is_empty() {
        let query = SearchQuery::parse("  \t invoice \n september  ").expect("parse");
        assert_eq!(
            query.terms,
            vec!["invoice".to_string(), "september".to_string()]
        );
        assert!(SearchQuery::parse("").expect("parse").is_empty());
        assert!(SearchQuery::parse("   \t\n ").expect("parse").is_empty());
        assert_eq!(SearchQuery::default().to_wire(), "");
    }

    #[test]
    fn from_to_subject_body_and_attachment_parse_into_operators() {
        let query = SearchQuery::parse(
            "from:alice@example.com to:bob subject:invoice body:agenda attachment:pdf",
        )
        .expect("parse");
        assert_eq!(
            query.operators,
            vec![
                SearchOp::From("alice@example.com".to_string()),
                SearchOp::To("bob".to_string()),
                SearchOp::Subject("invoice".to_string()),
                SearchOp::Body("agenda".to_string()),
                SearchOp::Attachment("pdf".to_string()),
            ]
        );
        assert!(query.terms.is_empty());
    }

    #[test]
    fn flag_operators_parse() {
        let query = SearchQuery::parse("has:attachment is:unread is:flagged").expect("parse");
        assert_eq!(
            query.operators,
            vec![
                SearchOp::HasAttachment,
                SearchOp::IsUnread,
                SearchOp::IsFlagged
            ]
        );
        // The flag keywords are case-insensitive.
        let upper = SearchQuery::parse("has:ATTACHMENT is:Unread").expect("parse");
        assert_eq!(
            upper.operators,
            vec![SearchOp::HasAttachment, SearchOp::IsUnread]
        );
    }

    #[test]
    fn date_operators_parse_into_naive_dates() {
        let query = SearchQuery::parse("before:2026-01-01 after:2025-12-31").expect("parse");
        assert_eq!(
            query.operators,
            vec![
                SearchOp::Before(date(2026, 1, 1)),
                SearchOp::After(date(2025, 12, 31))
            ]
        );
    }

    #[test]
    fn folder_parses_and_keeps_its_case() {
        let query = SearchQuery::parse("folder:INBOX").expect("parse");
        assert_eq!(query.operators, vec![SearchOp::Folder("INBOX".to_string())]);
    }

    #[test]
    fn quoted_values_keep_their_spaces() {
        let query =
            SearchQuery::parse("subject:\"invoice for september\" from:bob").expect("parse");
        assert_eq!(
            query.operators,
            vec![
                SearchOp::Subject("invoice for september".to_string()),
                SearchOp::From("bob".to_string()),
            ]
        );
        assert_eq!(query.terms.len(), 0);
    }

    #[test]
    fn a_quoted_bare_term_keeps_its_spaces() {
        let query = SearchQuery::parse("\"project plan\" Q4").expect("parse");
        assert_eq!(
            query.terms,
            vec!["project plan".to_string(), "Q4".to_string()]
        );
    }

    #[test]
    fn a_token_that_starts_quoted_is_never_an_operator() {
        let query = SearchQuery::parse("\"subject:invoice\"").expect("parse");
        assert_eq!(query.terms, vec!["subject:invoice".to_string()]);
        assert!(query.operators.is_empty());
    }

    #[test]
    fn an_unknown_operator_becomes_a_bare_term_that_keeps_its_colon() {
        let query =
            SearchQuery::parse("colour:red is:something-else has:whatever :odd").expect("parse");
        assert_eq!(
            query.terms,
            vec![
                "colour:red".to_string(),
                "is:something-else".to_string(),
                "has:whatever".to_string(),
                ":odd".to_string(),
            ]
        );
        assert!(query.operators.is_empty());
    }

    #[test]
    fn an_empty_operator_value_is_invalid() {
        for input in [
            "from:",
            "to:",
            "subject:",
            "body:",
            "attachment:",
            "folder:",
            "is:",
            "has:",
        ] {
            let err = SearchQuery::parse(input).expect_err(input);
            assert!(matches!(err, ClientError::Invalid(_)), "{input}: {err}");
        }
        // A quoted empty value is still an empty value.
        assert!(SearchQuery::parse("from:\"\"").is_err());
    }

    #[test]
    fn a_bad_date_is_invalid() {
        for input in [
            "before:yesterday",
            "after:2026-13-45",
            "before:20260916",
            "after:",
        ] {
            let err = SearchQuery::parse(input).expect_err(input);
            assert!(matches!(err, ClientError::Invalid(_)), "{input}: {err}");
        }
    }

    #[test]
    fn an_empty_quoted_token_contributes_nothing() {
        let query = SearchQuery::parse("\"\"").expect("parse");
        assert!(query.is_empty());
    }

    #[test]
    fn to_wire_round_trips_through_the_parser() {
        let input = "invoice from:bob subject:\"invoice for september\" is:unread before:2026-01-01 folder:INBOX colour:red \"project plan\"";
        let query = SearchQuery::parse(input).expect("parse");
        // Terms first, then operators in input order; the wire form is canonical,
        // so it is what the round trip is asserted against.
        let wire = query.to_wire();
        assert_eq!(
            wire,
            "invoice colour:red \"project plan\" from:bob \
             subject:\"invoice for september\" is:unread before:2026-01-01 folder:INBOX"
        );
        let reparsed = SearchQuery::parse(&wire).expect("reparse");
        assert_eq!(reparsed, query);
        assert_eq!(
            reparsed.to_wire(),
            wire,
            "the wire form must be a fixed point"
        );
    }

    #[test]
    fn to_wire_quotes_only_what_needs_quoting() {
        let query = SearchQuery::parse("subject:\"a b\" from:bob").expect("parse");
        assert_eq!(query.to_wire(), "subject:\"a b\" from:bob");
    }

    // -- local search ------------------------------------------------------

    #[tokio::test]
    async fn the_cache_is_seeded_with_the_expected_rows() {
        let (_dir, db, account) = seeded().await;
        let results = results_for(&db, account, "folder:INBOX", 50).await;
        assert_eq!(
            results.hits.len(),
            3,
            "105 is a tombstone and must not appear"
        );
        assert_eq!(results.total, 3);
        assert_eq!(ids(&db, account, "folder:Archive").await, vec![104]);
        assert_eq!(results_for(&db, 2, "folder:INBOX", 50).await.total, 1);
    }

    #[tokio::test]
    async fn a_hit_carries_the_list_view_fields() {
        let (_dir, db, account) = seeded().await;
        let results = results_for(&db, account, "subject:invoice", 50).await;
        assert_eq!(results.hits.len(), 1);
        let hit = &results.hits[0];
        assert_eq!(hit.account_id, 1);
        assert_eq!(hit.message_id, 101);
        assert_eq!(hit.folder_id, Some(5));
        assert_eq!(hit.subject, "Invoice for September");
        assert_eq!(hit.from_address, "billing@example.com");
        assert_eq!(hit.flags, "");
        assert_eq!(hit.internal_date.as_deref(), Some("2026-09-16T12:00:00Z"));
        assert!(hit.snippet.is_some());
    }

    #[tokio::test]
    async fn subject_from_body_and_attachment_operators_filter() {
        let (_dir, db, account) = seeded().await;
        assert_eq!(ids(&db, account, "subject:invoice").await, vec![101]);
        assert_eq!(ids(&db, account, "from:billing").await, vec![101]);
        assert_eq!(
            ids(&db, account, "from:billing@example.com").await,
            vec![101]
        );
        assert_eq!(ids(&db, account, "body:portal").await, vec![101]);
        assert_eq!(ids(&db, account, "body:lunch").await, vec![102]);
        assert_eq!(ids(&db, account, "attachment:pdf").await, vec![101]);
        assert_eq!(
            ids(&db, account, "attachment:invoice-september.pdf").await,
            vec![101]
        );
    }

    #[tokio::test]
    async fn a_bare_term_searches_every_field() {
        let (_dir, db, account) = seeded().await;
        assert_eq!(ids(&db, account, "invoice").await, vec![101]);
        assert_eq!(ids(&db, account, "september").await, vec![101]);
        assert_eq!(ids(&db, account, "carol").await, vec![103], "the sender");
        assert_eq!(
            ids(&db, account, "alice").await,
            vec![103, 101, 102, 104],
            "the recipients"
        );
    }

    #[tokio::test]
    async fn has_attachment_filters_to_messages_with_files() {
        let (_dir, db, account) = seeded().await;
        assert_eq!(ids(&db, account, "has:attachment").await, vec![101]);
    }

    #[tokio::test]
    async fn is_unread_excludes_seen_messages() {
        let (_dir, db, account) = seeded().await;
        assert_eq!(ids(&db, account, "is:unread").await, vec![101, 104]);
        assert!(
            !ids(&db, account, "is:unread").await.contains(&102),
            "102 carries the seen flag"
        );
    }

    #[tokio::test]
    async fn is_flagged_filters_to_flagged_messages() {
        let (_dir, db, account) = seeded().await;
        assert_eq!(ids(&db, account, "is:flagged").await, vec![103]);
    }

    #[tokio::test]
    async fn before_and_after_filter_by_internal_date() {
        let (_dir, db, account) = seeded().await;
        assert_eq!(ids(&db, account, "before:2026-01-01").await, vec![104]);
        assert_eq!(ids(&db, account, "after:2026-09-15").await, vec![103, 101]);
        assert_eq!(
            ids(&db, account, "after:2026-09-15 before:2026-09-17").await,
            vec![101]
        );
        assert!(ids(&db, account, "before:2025-01-01").await.is_empty());
    }

    #[tokio::test]
    async fn folder_matches_the_name_case_insensitively() {
        let (_dir, db, account) = seeded().await;
        assert_eq!(ids(&db, account, "folder:INBOX").await, vec![103, 101, 102]);
        assert_eq!(ids(&db, account, "folder:inbox").await, vec![103, 101, 102]);
        assert_eq!(ids(&db, account, "folder:Archive").await, vec![104]);
        assert!(ids(&db, account, "folder:Trash").await.is_empty());
    }

    #[tokio::test]
    async fn results_are_newest_first_and_respect_the_limit() {
        let (_dir, db, account) = seeded().await;
        let page = results_for(&db, account, "folder:INBOX", 2).await;
        assert_eq!(page.hits.len(), 2);
        assert_eq!(
            page.total, 3,
            "total counts every match, not just the returned page"
        );
        assert_eq!(page.hits[0].message_id, 103);
        assert_eq!(page.hits[1].message_id, 101);
        assert!(results_for(&db, account, "folder:INBOX", 0)
            .await
            .hits
            .is_empty());
    }

    #[tokio::test]
    async fn operators_combine_with_and_semantics() {
        let (_dir, db, account) = seeded().await;
        assert_eq!(
            ids(&db, account, "is:unread subject:invoice").await,
            vec![101]
        );
        assert!(ids(&db, account, "subject:invoice folder:Archive")
            .await
            .is_empty());
    }

    #[tokio::test]
    async fn results_are_scoped_to_the_account() {
        let (_dir, db, _account) = seeded().await;
        assert_eq!(ids(&db, 1, "subject:invoice").await, vec![101]);
        assert_eq!(ids(&db, 2, "subject:invoice").await, vec![201]);
        assert!(ids(&db, 9, "subject:invoice").await.is_empty());
    }

    #[tokio::test]
    async fn an_empty_query_matches_nothing() {
        let (_dir, db, account) = seeded().await;
        let executor = SearchExecutor::local(Arc::clone(&db));
        let empty = SearchQuery::parse("").expect("parse");
        let results = executor
            .search_local(account, &empty, 50)
            .await
            .expect("search");
        assert!(results.hits.is_empty());
        assert_eq!(results.total, 0);
        assert_eq!(results.source, SearchSource::Local);
    }

    #[tokio::test]
    async fn a_hand_built_empty_term_matches_nothing() {
        let (_dir, db, account) = seeded().await;
        let query = SearchQuery {
            terms: vec![String::new()],
            operators: Vec::new(),
        };
        assert!(!query.is_empty());
        let executor = SearchExecutor::local(Arc::clone(&db));
        let results = executor
            .search_local(account, &query, 50)
            .await
            .expect("search");
        assert!(results.hits.is_empty());
    }

    #[tokio::test]
    async fn a_query_with_no_text_clause_scans_the_filters_only() {
        let (_dir, db, account) = seeded().await;
        assert_eq!(ids(&db, account, "is:flagged").await, vec![103]);
        assert_eq!(ids(&db, account, "has:attachment").await, vec![101]);
    }

    // -- the LIKE fallback -------------------------------------------------

    #[tokio::test]
    async fn the_like_fallback_finds_the_same_rows_as_the_index() {
        let (_dir_indexed, indexed, account) = seeded().await;
        let (_dir_like, like, _) = seeded_like().await;
        assert_eq!(like.search_mode(), SearchMode::Like);

        for query in [
            "subject:invoice",
            "from:billing",
            "body:lunch",
            "attachment:pdf",
            "has:attachment",
            "is:unread",
            "is:flagged",
            "before:2026-01-01",
            "after:2026-09-15",
            "after:2026-09-15 before:2026-09-17",
            "folder:INBOX",
            "folder:inbox",
            "folder:Archive",
            "invoice",
            "september",
            "carol",
            "is:unread subject:invoice",
        ] {
            assert_eq!(
                ids(&like, account, query).await,
                ids(&indexed, account, query).await,
                "the two backends disagree on `{query}`"
            );
        }
    }

    #[tokio::test]
    async fn the_like_fallback_scans_the_tables_instead_of_an_index() {
        let (_dir, db, account) = seeded_like().await;
        let mut conn = db.pool().acquire().await.expect("connection");
        let doc = SearchDocument {
            account_id: account,
            message_id: 999,
            folder_id: Some(5),
            subject: "only in the document".to_string(),
            sender: "ghost@example.com".to_string(),
            recipients: "ghost@example.com".to_string(),
            body: "not in any table".to_string(),
            attachments: String::new(),
        };
        index_document(&mut conn, SearchMode::Like, &doc)
            .await
            .expect("no-op");
        unindex_document(&mut conn, SearchMode::Like, account, 999)
            .await
            .expect("no-op");
        unindex_folder(&mut conn, SearchMode::Like, account, 5)
            .await
            .expect("no-op");
        drop(conn);

        assert!(
            ids(&db, account, "subject:\"only in the document\"")
                .await
                .is_empty(),
            "Like mode has no index to search"
        );
        // The rows themselves are still found.
        assert_eq!(ids(&db, account, "subject:invoice").await, vec![101]);
    }

    #[tokio::test]
    async fn the_like_fallback_limits_and_counts_like_the_index() {
        let (_dir, db, account) = seeded_like().await;
        let page = results_for(&db, account, "folder:INBOX", 2).await;
        assert_eq!(page.hits.len(), 2);
        assert_eq!(page.total, 3);
        assert_eq!(page.hits[0].message_id, 103);
    }

    #[tokio::test]
    async fn a_deleted_message_is_never_returned() {
        let (_dir, db, account) = seeded().await;
        for query in ["is:unread", "folder:INBOX", "spam", "buy now"] {
            assert!(
                !ids(&db, account, query).await.contains(&105),
                "the tombstone leaked into `{query}`"
            );
        }
    }

    // -- index maintenance -------------------------------------------------

    #[tokio::test]
    async fn indexing_the_same_document_twice_does_not_duplicate_it() {
        let (_dir, db, account) = seeded().await;
        if !has_index(&db) {
            return; // the `LIKE` fallback has no index to duplicate rows in
        }
        let doc = documents()
            .into_iter()
            .find(|doc| doc.message_id == 101)
            .expect("document");
        let mut conn = db.pool().acquire().await.expect("connection");
        index_document(&mut conn, SearchMode::Fts5, &doc)
            .await
            .expect("reindex");
        index_document(&mut conn, SearchMode::Fts5, &doc)
            .await
            .expect("reindex again");
        let row = sqlx::query(
            "SELECT COUNT(*) AS n FROM messages_fts \
             WHERE CAST(account_id AS TEXT) = ? AND CAST(message_id AS INTEGER) = ?",
        )
        .bind(account.to_string())
        .bind(101i64)
        .fetch_one(&mut *conn)
        .await
        .expect("count");
        assert_eq!(row.get::<i64, _>("n"), 1);
        drop(conn);

        assert_eq!(ids(&db, account, "subject:invoice").await, vec![101]);
    }

    #[tokio::test]
    async fn unindex_document_removes_exactly_one_hit() {
        let (_dir, db, account) = seeded().await;
        if !has_index(&db) {
            return;
        }
        let mut conn = db.pool().acquire().await.expect("connection");
        unindex_document(&mut conn, SearchMode::Fts5, account, 104)
            .await
            .expect("unindex");
        // Unindexing a document that is already gone is fine.
        unindex_document(&mut conn, SearchMode::Fts5, account, 104)
            .await
            .expect("unindex again");
        drop(conn);

        assert!(ids(&db, account, "subject:newsletter").await.is_empty());
        assert_eq!(
            ids(&db, account, "subject:invoice").await,
            vec![101],
            "the other documents are untouched"
        );
        // Flag and folder filters read the cached message table rather than the
        // text index (see the module docs), so unindexing does not hide a
        // message that is still in the cache — only its *text* stops matching.
        assert_eq!(ids(&db, account, "is:unread").await, vec![101, 104]);
    }

    #[tokio::test]
    async fn unindex_folder_removes_the_folders_hits() {
        let (_dir, db, account) = seeded().await;
        if !has_index(&db) {
            return;
        }
        let mut conn = db.pool().acquire().await.expect("connection");
        unindex_folder(&mut conn, SearchMode::Fts5, account, 5)
            .await
            .expect("unindex folder");
        // Unindexing an unknown folder is a no-op, not an error.
        unindex_folder(&mut conn, SearchMode::Fts5, account, 99)
            .await
            .expect("unindex unknown folder");
        drop(conn);

        assert!(
            ids(&db, account, "subject:invoice").await.is_empty(),
            "101 lives in the cleared folder"
        );
        assert_eq!(
            ids(&db, account, "subject:newsletter").await,
            vec![104],
            "the other folder's documents are still indexed"
        );
    }

    // -- the server fallback ----------------------------------------------

    /// An `FcpClient` pointed at the mock server, already authenticated.
    async fn server_client(server: &MockServer) -> FcpClient {
        let client = FcpClient::new(server.fcp_base_url(), Arc::new(MemoryTokenStore::new()))
            .expect("client");
        client.set_access_token("t", Duration::from_secs(60)).await;
        client
    }

    #[tokio::test]
    async fn an_empty_local_result_falls_back_to_the_server() {
        let server = MockServer::start().await;
        server.json_route(
            "GET",
            "/api/v1/client/search",
            200,
            r#"{"items":[{"id":7,"folder_id":6,"subject":"Server hit","from":{"address":"bob@example.com"},
                 "snippet":"from the server","flags":"seen","internal_date":"2026-09-16T12:00:00Z"}],
                 "total":1,"limit":20,"offset":0}"#,
        );
        let (_dir, db, account) = seeded().await;
        let executor = SearchExecutor::with_server(Arc::clone(&db), server_client(&server).await);

        let query = SearchQuery::parse("from:nobody").expect("parse");
        let results = executor.search(account, &query, 20).await.expect("search");

        assert_eq!(results.source, SearchSource::Server);
        assert_eq!(results.total, 1);
        assert_eq!(results.hits.len(), 1);
        let hit = &results.hits[0];
        assert_eq!(hit.account_id, account);
        assert_eq!(hit.message_id, 7);
        assert_eq!(hit.folder_id, Some(6));
        assert_eq!(hit.subject, "Server hit");
        assert_eq!(hit.from_address, "bob@example.com");
        assert_eq!(hit.snippet.as_deref(), Some("from the server"));
        assert_eq!(hit.flags, "seen");

        assert_eq!(server.count_for("/api/v1/client/search"), 1);
        let request = server.requests_for("/api/v1/client/search").remove(0);
        assert!(
            request.query.contains("from%3Anobody"),
            "the wire query went through to_wire: {}",
            request.query
        );
    }

    #[tokio::test]
    async fn a_local_hit_does_not_call_the_server() {
        let server = MockServer::start().await;
        server.json_route(
            "GET",
            "/api/v1/client/search",
            200,
            r#"{"items":[],"total":0}"#,
        );
        let (_dir, db, account) = seeded().await;
        let executor = SearchExecutor::with_server(Arc::clone(&db), server_client(&server).await);

        let query = SearchQuery::parse("subject:invoice").expect("parse");
        let results = executor.search(account, &query, 20).await.expect("search");

        assert_eq!(results.source, SearchSource::Local);
        assert_eq!(results.hits.len(), 1);
        assert_eq!(server.count_for("/api/v1/client/search"), 0);
    }

    #[tokio::test]
    async fn search_local_never_calls_the_server() {
        let server = MockServer::start().await;
        server.json_route(
            "GET",
            "/api/v1/client/search",
            200,
            r#"{"items":[],"total":0}"#,
        );
        let (_dir, db, account) = seeded().await;
        let executor = SearchExecutor::with_server(Arc::clone(&db), server_client(&server).await);

        let query = SearchQuery::parse("from:nobody").expect("parse");
        let local = executor
            .search_local(account, &query, 20)
            .await
            .expect("search_local");
        assert_eq!(local.source, SearchSource::Local);
        assert!(local.hits.is_empty());
        assert_eq!(server.count_for("/api/v1/client/search"), 0);

        // The same query through `search` does reach it — which is what makes the
        // assertion above meaningful.
        let fallback = executor.search(account, &query, 20).await.expect("search");
        assert_eq!(fallback.source, SearchSource::Server);
        assert_eq!(server.count_for("/api/v1/client/search"), 1);
    }

    #[tokio::test]
    async fn an_empty_query_never_reaches_the_server() {
        let server = MockServer::start().await;
        server.json_route(
            "GET",
            "/api/v1/client/search",
            200,
            r#"{"items":[],"total":0}"#,
        );
        let (_dir, db, account) = seeded().await;
        let executor = SearchExecutor::with_server(Arc::clone(&db), server_client(&server).await);

        let empty = SearchQuery::parse("").expect("parse");
        let results = executor.search(account, &empty, 20).await.expect("search");

        assert_eq!(results.source, SearchSource::Local);
        assert!(results.hits.is_empty());
        assert_eq!(results.total, 0);
        assert_eq!(server.count_for("/api/v1/client/search"), 0);
    }

    #[tokio::test]
    async fn without_a_client_an_empty_local_result_stays_local() {
        let (_dir, db, account) = seeded().await;
        let executor = SearchExecutor::local(Arc::clone(&db));

        let query = SearchQuery::parse("from:nobody").expect("parse");
        let results = executor.search(account, &query, 20).await.expect("search");

        assert_eq!(results.source, SearchSource::Local);
        assert!(results.hits.is_empty());
        assert_eq!(results.total, 0);
    }

    #[tokio::test]
    async fn a_server_error_surfaces_as_a_client_error() {
        let server = MockServer::start().await;
        server.error_route(
            "GET",
            "/api/v1/client/search",
            503,
            "internal_error",
            "the search backend is down",
        );
        let (_dir, db, account) = seeded().await;
        // No retries: the mock would just answer 503 four times, and the point of
        // the test is the error the caller sees.
        let client = FcpClient::with_retry(
            server.fcp_base_url(),
            Arc::new(MemoryTokenStore::new()),
            RetryPolicy::none(),
        )
        .expect("client");
        client.set_access_token("t", Duration::from_secs(60)).await;
        let executor = SearchExecutor::with_server(Arc::clone(&db), client);

        let query = SearchQuery::parse("from:nobody").expect("parse");
        let err = executor
            .search(account, &query, 20)
            .await
            .expect_err("the server refused");
        assert!(err.is_retryable(), "a 5xx is temporary: {err}");
    }
}

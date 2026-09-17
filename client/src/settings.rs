//! The settings surface of specification §52.
//!
//! §52 lists twelve sections: 账户 (accounts), 同步 (sync), 通知 (notifications),
//! 外观 (appearance), 阅读 (reading), 写信 (composing), 附件 (attachments), 搜索
//! (search), 存储 (storage), 安全 (security), 设备 (devices) and 关于 (about).
//! Accounts, devices and about are runtime state rather than preferences — the
//! account manager and the device list own them — so this module persists the
//! nine sections that really are settings.
//!
//! # Storage
//!
//! Every section is one row of the `settings` table: `key` is the section name
//! and `value` is that section's JSON (`migrations/0001_init.sql`). One row per
//! section means a write touches one row, a read of one section never parses the
//! rest, and a section invented by a later build is a new key rather than a schema
//! change.
//!
//! # A bad row must never make the client unusable
//!
//! [`SettingsStore::load`] starts from [`Settings::default`] and overlays only the
//! sections it can read. A missing row, a truncated value, or a section written by
//! a *newer* build with a value we do not understand all fall back to the default
//! for that section — with a warning — while the other sections are still applied.
//! Refusing to start because of one unreadable preference would be a support
//! ticket, not a feature.
//!
//! # Other keys in the same table
//!
//! The table is shared with unrelated bookkeeping — the shell's interactive/UI
//! state (`interactive`, the `ui.*` keys a front end keeps for itself) and the
//! cache's own `cache.search_mode`. Those keys are **left untouched**:
//! [`SettingsStore::save`] writes only the nine section rows, and
//! [`SettingsStore::load`] ignores every key that is not one of them.

use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::database::ClientDatabase;
use crate::error::ClientResult;

/// The `settings.key` of the 同步 section.
const KEY_SYNC: &str = "sync";
/// The `settings.key` of the 通知 section.
const KEY_NOTIFICATIONS: &str = "notifications";
/// The `settings.key` of the 外观 section.
const KEY_APPEARANCE: &str = "appearance";
/// The `settings.key` of the 阅读 section.
const KEY_READING: &str = "reading";
/// The `settings.key` of the 写信 section.
const KEY_COMPOSING: &str = "composing";
/// The `settings.key` of the 附件 section.
const KEY_ATTACHMENTS: &str = "attachments";
/// The `settings.key` of the 搜索 section.
const KEY_SEARCH: &str = "search";
/// The `settings.key` of the 存储 section.
const KEY_STORAGE: &str = "storage";
/// The `settings.key` of the 安全 section.
const KEY_SECURITY: &str = "security";

/// How much mail the client pulls down (specification §52, 同步).
///
/// The window is a *paging* predicate, not a deletion rule: changes outside it
/// are recorded as seen — so the cursor still advances — but their messages are
/// never stored and their bodies are never fetched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SyncWindow {
    /// 同步全部邮件 — sync everything the server offers.
    #[default]
    All,
    /// 仅同步最近 30 天 — sync the last 30 days only.
    Days30,
    /// 仅同步最近 90 天 — sync the last 90 days only.
    Days90,
}

impl SyncWindow {
    /// The window in days, or `None` for [`SyncWindow::All`].
    ///
    /// This is what the sync engine writes to `accounts.sync_window_days`
    /// (`NULL` = sync everything) before it pages `GET /client/sync`.
    pub fn days(self) -> Option<u32> {
        match self {
            SyncWindow::All => None,
            SyncWindow::Days30 => Some(30),
            SyncWindow::Days90 => Some(90),
        }
    }

    /// The short label the settings widget shows: `"all"`, `"30d"` or `"90d"`.
    ///
    /// The label is for humans and is not what gets stored; the stored form is
    /// the `snake_case` serde name (`"all"`, `"days30"`, `"days90"`).
    pub fn label(self) -> &'static str {
        match self {
            SyncWindow::All => "all",
            SyncWindow::Days30 => "30d",
            SyncWindow::Days90 => "90d",
        }
    }
}

/// The 同步 section: what to sync, how much of it, and how often (§52).
///
/// The §52 options are the sync window, 附件自动下载 (`auto_download_attachments`),
/// 仅 Wi-Fi 下载 (`wifi_only`) and 最大缓存大小 (`max_cache_bytes`). `interval_secs`
/// is the client-side cadence of the background sync, which §3 leaves to us.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncSettings {
    /// 同步窗口: everything, the last 30 days, or the last 90 days.
    pub window: SyncWindow,
    /// 附件自动下载 — fetch attachments without being asked.
    pub auto_download_attachments: bool,
    /// 仅 Wi-Fi 下载 — never spend metered traffic on a sync or a download.
    pub wifi_only: bool,
    /// 最大缓存大小 — the byte budget for the local cache (§52, 存储).
    pub max_cache_bytes: u64,
    /// How often the background sync runs, in seconds.
    pub interval_secs: u64,
}

impl Default for SyncSettings {
    /// The §52 default: sync everything, download nothing by itself, and keep a
    /// 2 GiB cache, syncing every five minutes.
    fn default() -> Self {
        SyncSettings {
            window: SyncWindow::All,
            auto_download_attachments: false,
            wifi_only: false,
            max_cache_bytes: 2 * 1024 * 1024 * 1024,
            interval_secs: 300,
        }
    }
}

/// The 通知 section (§52).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NotificationSettings {
    /// Whether desktop notifications are shown at all.
    pub enabled: bool,
    /// Whether a delivered notification also plays a sound.
    pub sound: bool,
    /// Whether only mail the server marked as important is announced.
    pub only_important: bool,
}

impl Default for NotificationSettings {
    /// Notifications are on and audible, and are not filtered down to important
    /// mail — the user asked for the client to tell them about their mail.
    fn default() -> Self {
        NotificationSettings {
            enabled: true,
            sound: true,
            only_important: false,
        }
    }
}

/// The 外观 section (§52).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
    /// A light palette.
    Light,
    /// A dark palette.
    Dark,
    /// Follow the operating system's preference.
    #[default]
    System,
}

/// How tightly the mail list is packed (the 外观 density option).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Density {
    /// Roomy rows, one line of snippet under the subject.
    #[default]
    Comfortable,
    /// Tight rows for a small screen or a long folder.
    Compact,
}

/// The 外观 section: theme, density and font scaling.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppearanceSettings {
    /// Light, dark, or whatever the desktop is doing.
    pub theme: Theme,
    /// How much air each list row gets.
    pub density: Density,
    /// A multiplier on the shell's base font size (`1.0` = unchanged).
    pub font_scale: f32,
}

impl Default for AppearanceSettings {
    /// Follow the system theme, comfortable rows, unscaled text.
    fn default() -> Self {
        AppearanceSettings {
            theme: Theme::System,
            density: Density::Comfortable,
            font_scale: 1.0,
        }
    }
}

/// The 阅读 section (§52): how a message is presented and marked.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReadingSettings {
    /// Mark a message as read as soon as it is opened.
    pub mark_read_on_open: bool,
    /// How long the message must stay open before it counts as read.
    pub mark_read_delay_secs: u64,
    /// Prefer the HTML part when the message has one.
    pub show_html: bool,
    /// Fetch images and other remote content referenced by an HTML body.
    pub load_remote_images: bool,
    /// How many body lines a list row previews.
    pub preview_lines: u32,
}

impl Default for ReadingSettings {
    /// Read on open after three seconds, plain text by default, and no remote
    /// content — the safe reading defaults of §52.
    fn default() -> Self {
        ReadingSettings {
            mark_read_on_open: true,
            mark_read_delay_secs: 3,
            show_html: false,
            load_remote_images: false,
            preview_lines: 2,
        }
    }
}

/// How a new message is composed (the 写信 format option).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ComposeFormat {
    /// A `text/plain` body.
    #[default]
    Plain,
    /// A `text/html` body, with a plain-text alternative generated on send.
    Html,
}

/// The 写信 section (§52): the signature and the reply conventions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ComposingSettings {
    /// The signature appended to a new message (empty = no signature).
    pub signature: String,
    /// Whether to compose plain text or HTML.
    pub format: ComposeFormat,
    /// Quote the original message when replying.
    pub quote_reply: bool,
    /// Put the reply above the quoted text rather than below it.
    pub reply_above: bool,
}

impl Default for ComposingSettings {
    /// No signature, plain text, and the conventional reply-above-with-quote.
    fn default() -> Self {
        ComposingSettings {
            signature: String::new(),
            format: ComposeFormat::Plain,
            quote_reply: true,
            reply_above: false,
        }
    }
}

/// The 附件 section (§52): when attachments are fetched and where they land.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AttachmentSettings {
    /// The largest attachment the client downloads without being asked.
    pub auto_download_max_bytes: u64,
    /// The byte budget for the attachment blob cache alone.
    pub cache_limit_bytes: u64,
    /// Where downloaded attachments are saved, or `None` for the platform's
    /// downloads directory.
    pub download_dir: Option<String>,
}

impl Default for AttachmentSettings {
    /// Auto-download up to 5 MiB per attachment, keep 1 GiB of blobs, and let
    /// the shell pick the download directory.
    fn default() -> Self {
        AttachmentSettings {
            auto_download_max_bytes: 5 * 1024 * 1024,
            cache_limit_bytes: 1024 * 1024 * 1024,
            download_dir: None,
        }
    }
}

/// The 搜索 section (§30, §52).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchSettings {
    /// Search the local cache first — it is always reachable and usually enough.
    pub local_first: bool,
    /// Ask the server when the local search looks thin.
    pub server_fallback: bool,
    /// How many hits a search returns before it stops.
    pub max_results: usize,
}

impl Default for SearchSettings {
    /// Local first, with the server as the fallback, and 100 hits per query.
    fn default() -> Self {
        SearchSettings {
            local_first: true,
            server_fallback: true,
            max_results: 100,
        }
    }
}

/// The 存储 section (§52): the cache budget and what the client keeps.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StorageSettings {
    /// The byte budget for everything the cache holds.
    pub max_cache_bytes: u64,
    /// How long a downloaded body is kept before it is evicted again.
    pub keep_bodies_days: u32,
    /// Drop the whole cache when the last account signs out.
    pub wipe_on_logout: bool,
}

impl Default for StorageSettings {
    /// A 2 GiB cache, bodies kept for a month, and the cache kept across a
    /// sign-out (a shared machine can turn `wipe_on_logout` on).
    fn default() -> Self {
        StorageSettings {
            max_cache_bytes: 2 * 1024 * 1024 * 1024,
            keep_bodies_days: 30,
            wipe_on_logout: false,
        }
    }
}

/// The 安全 section (§52).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SecuritySettings {
    /// Lock the client after a period without input.
    pub lock_on_idle: bool,
    /// How long "idle" is, in seconds.
    pub idle_lock_secs: u64,
    /// Put an HTML body through the sanitiser before rendering it.
    pub sanitize_html: bool,
    /// Allow the remote content an HTML body references.
    pub allow_remote_content: bool,
}

impl Default for SecuritySettings {
    /// Lock after fifteen minutes idle, always sanitise HTML, and never load
    /// remote content by default.
    fn default() -> Self {
        SecuritySettings {
            lock_on_idle: true,
            idle_lock_secs: 900,
            sanitize_html: true,
            allow_remote_content: false,
        }
    }
}

/// Every persistable section of the §52 settings surface.
///
/// Each field is one `settings` row; see the module documentation for how the
/// sections are stored and what happens to a row this build cannot read.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Settings {
    /// 同步 — what to sync and how much (§52).
    pub sync: SyncSettings,
    /// 通知 — desktop notifications (§34, §52).
    pub notifications: NotificationSettings,
    /// 外观 — theme, density, font scale.
    pub appearance: AppearanceSettings,
    /// 阅读 — how messages are presented and marked read.
    pub reading: ReadingSettings,
    /// 写信 — signature, format and reply conventions.
    pub composing: ComposingSettings,
    /// 附件 — auto-download policy and the blob cache budget.
    pub attachments: AttachmentSettings,
    /// 搜索 — local-first search and its server fallback.
    pub search: SearchSettings,
    /// 存储 — the cache budget and retention.
    pub storage: StorageSettings,
    /// 安全 — idle lock, HTML sanitising, remote content.
    pub security: SecuritySettings,
}

impl Settings {
    /// The sync window in days, or `None` to sync everything.
    ///
    /// A convenience for the sync engine, which needs the number and not the
    /// enum.
    pub fn sync_window_days(&self) -> Option<u32> {
        self.sync.window.days()
    }

    /// The byte budget the attachment cache may actually use.
    ///
    /// Two caps apply: the attachment cache's own limit (`附件`) and the budget
    /// the caller passes as `total_cache` — normally
    /// [`SyncSettings::max_cache_bytes`], the 最大缓存大小 of §52. The attachment
    /// cache is a part of the whole, so it gets the smaller of the two.
    pub fn attachment_cache_limit(&self, total_cache: u64) -> u64 {
        self.attachments.cache_limit_bytes.min(total_cache)
    }
}

/// Reads and writes [`Settings`] in the local cache.
///
/// Cheap to clone: it holds an `Arc` on the cache, so the shell and the sync
/// engine can each hold one.
#[derive(Debug, Clone)]
pub struct SettingsStore {
    db: Arc<ClientDatabase>,
}

impl SettingsStore {
    /// A store over `db`.
    pub fn new(db: Arc<ClientDatabase>) -> Self {
        SettingsStore { db }
    }

    /// The cache this store reads and writes.
    pub fn database(&self) -> &Arc<ClientDatabase> {
        &self.db
    }

    /// Load every section, falling back to the default for the ones this build
    /// cannot read.
    ///
    /// A missing row, a value that is not JSON, and a section written by a newer
    /// build are all the same case: the default for that section is used, a
    /// warning is logged, and the other sections are applied normally. Keys in
    /// the `settings` table that are not §52 sections (the shell's interactive/UI
    /// state, `cache.search_mode`) are ignored and left untouched.
    pub async fn load(&self) -> ClientResult<Settings> {
        let rows = sqlx::query("SELECT key, value FROM settings")
            .fetch_all(self.db.pool())
            .await?;

        let mut settings = Settings::default();
        for row in rows {
            let key: String = row.get("key");
            let value: String = row.get("value");
            apply_section(&mut settings, &key, &value);
        }
        Ok(settings)
    }

    /// Write every section.
    ///
    /// The nine rows are written in one transaction, so a crash mid-save cannot
    /// leave the surface half-updated. Rows that are not §52 sections (the
    /// shell's interactive/UI state, `cache.search_mode`) are not touched.
    pub async fn save(&self, settings: &Settings) -> ClientResult<()> {
        let sections = serialize_sections(settings)?;
        let now = crate::util::now_rfc3339();

        let mut tx = self.db.pool().begin().await?;
        for (key, value) in &sections {
            sqlx::query(
                "INSERT INTO settings (key, value, updated_at) VALUES (?, ?, ?)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            )
            .bind(*key)
            .bind(value.as_str())
            .bind(now.as_str())
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Read, mutate and write in one step, returning the saved settings.
    ///
    /// This is how a settings pane changes one switch: it loads the current
    /// surface, applies the change, and saves the whole thing back. Sections the
    /// closure did not touch keep the values that were just read, so a
    /// concurrent change to another section is not silently reverted beyond this
    /// round trip.
    pub async fn update<F>(&self, mutate: F) -> ClientResult<Settings>
    where
        F: FnOnce(&mut Settings),
    {
        let mut settings = self.load().await?;
        mutate(&mut settings);
        self.save(&settings).await?;
        Ok(settings)
    }
}

/// Every `(key, value)` row a [`Settings`] becomes.
fn serialize_sections(settings: &Settings) -> ClientResult<Vec<(&'static str, String)>> {
    Ok(vec![
        (KEY_SYNC, serde_json::to_string(&settings.sync)?),
        (KEY_NOTIFICATIONS, serde_json::to_string(&settings.notifications)?),
        (KEY_APPEARANCE, serde_json::to_string(&settings.appearance)?),
        (KEY_READING, serde_json::to_string(&settings.reading)?),
        (KEY_COMPOSING, serde_json::to_string(&settings.composing)?),
        (KEY_ATTACHMENTS, serde_json::to_string(&settings.attachments)?),
        (KEY_SEARCH, serde_json::to_string(&settings.search)?),
        (KEY_STORAGE, serde_json::to_string(&settings.storage)?),
        (KEY_SECURITY, serde_json::to_string(&settings.security)?),
    ])
}

/// Overlay one stored section onto `settings`, ignoring anything else.
fn apply_section(settings: &mut Settings, key: &str, raw: &str) {
    match key {
        KEY_SYNC => {
            if let Some(section) = parse_section::<SyncSettings>(key, raw) {
                settings.sync = section;
            }
        }
        KEY_NOTIFICATIONS => {
            if let Some(section) = parse_section::<NotificationSettings>(key, raw) {
                settings.notifications = section;
            }
        }
        KEY_APPEARANCE => {
            if let Some(section) = parse_section::<AppearanceSettings>(key, raw) {
                settings.appearance = section;
            }
        }
        KEY_READING => {
            if let Some(section) = parse_section::<ReadingSettings>(key, raw) {
                settings.reading = section;
            }
        }
        KEY_COMPOSING => {
            if let Some(section) = parse_section::<ComposingSettings>(key, raw) {
                settings.composing = section;
            }
        }
        KEY_ATTACHMENTS => {
            if let Some(section) = parse_section::<AttachmentSettings>(key, raw) {
                settings.attachments = section;
            }
        }
        KEY_SEARCH => {
            if let Some(section) = parse_section::<SearchSettings>(key, raw) {
                settings.search = section;
            }
        }
        KEY_STORAGE => {
            if let Some(section) = parse_section::<StorageSettings>(key, raw) {
                settings.storage = section;
            }
        }
        KEY_SECURITY => {
            if let Some(section) = parse_section::<SecuritySettings>(key, raw) {
                settings.security = section;
            }
        }
        _ => {
            // Not a §52 section: `cache.search_mode`, a shell's `ui.*` state, or a
            // key from a newer build. It is neither read nor written by this
            // module, so it survives every save.
            tracing::trace!(key, "ignoring a settings key that is not a §52 section");
        }
    }
}

/// Parse one stored section, or `None` when the row is not JSON we understand.
fn parse_section<T>(key: &str, raw: &str) -> Option<T>
where
    T: DeserializeOwned,
{
    match serde_json::from_str::<T>(raw) {
        Ok(section) => Some(section),
        Err(err) => {
            tracing::warn!(
                key,
                error = %err,
                "unreadable settings section; the default is used instead"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    /// The nine section keys, in the order `save` writes them.
    const SECTION_KEYS: [&str; 9] = [
        KEY_SYNC,
        KEY_NOTIFICATIONS,
        KEY_APPEARANCE,
        KEY_READING,
        KEY_COMPOSING,
        KEY_ATTACHMENTS,
        KEY_SEARCH,
        KEY_STORAGE,
        KEY_SECURITY,
    ];

    async fn store() -> (TempDir, Arc<ClientDatabase>, SettingsStore) {
        let dir = TempDir::new().expect("temp dir");
        let db = Arc::new(
            ClientDatabase::open(dir.path().join("c.db"))
                .await
                .expect("open"),
        );
        let store = SettingsStore::new(Arc::clone(&db));
        (dir, db, store)
    }

    /// Every key currently in the `settings` table, sorted.
    async fn stored_keys(db: &ClientDatabase) -> Vec<String> {
        let rows = sqlx::query("SELECT key FROM settings ORDER BY key ASC")
            .fetch_all(db.pool())
            .await
            .expect("keys");
        rows.iter().map(|row| row.get::<String, _>("key")).collect()
    }

    /// Every `(key, value)` pair currently in the `settings` table.
    async fn stored_values(db: &ClientDatabase) -> Vec<(String, String)> {
        let rows = sqlx::query("SELECT key, value FROM settings ORDER BY key ASC")
            .fetch_all(db.pool())
            .await
            .expect("values");
        rows.iter()
            .map(|row| {
                (
                    row.get::<String, _>("key"),
                    row.get::<String, _>("value"),
                )
            })
            .collect()
    }

    /// A settings value that differs from the defaults in every section.
    fn custom_settings() -> Settings {
        Settings {
            sync: SyncSettings {
                window: SyncWindow::Days30,
                auto_download_attachments: true,
                wifi_only: true,
                max_cache_bytes: 512 * 1024 * 1024,
                interval_secs: 900,
            },
            notifications: NotificationSettings {
                enabled: false,
                sound: false,
                only_important: true,
            },
            appearance: AppearanceSettings {
                theme: Theme::Dark,
                density: Density::Compact,
                font_scale: 1.25,
            },
            reading: ReadingSettings {
                mark_read_on_open: false,
                mark_read_delay_secs: 10,
                show_html: true,
                load_remote_images: true,
                preview_lines: 4,
            },
            composing: ComposingSettings {
                signature: "-- alice".to_string(),
                format: ComposeFormat::Html,
                quote_reply: false,
                reply_above: true,
            },
            attachments: AttachmentSettings {
                auto_download_max_bytes: 1024,
                cache_limit_bytes: 2048,
                download_dir: Some("D:\\mail\\attachments".to_string()),
            },
            search: SearchSettings {
                local_first: false,
                server_fallback: false,
                max_results: 25,
            },
            storage: StorageSettings {
                max_cache_bytes: 4096,
                keep_bodies_days: 7,
                wipe_on_logout: true,
            },
            security: SecuritySettings {
                lock_on_idle: false,
                idle_lock_secs: 60,
                sanitize_html: false,
                allow_remote_content: true,
            },
        }
    }

    #[test]
    fn sync_defaults_match_spec_52() {
        let sync = SyncSettings::default();
        assert_eq!(sync.window, SyncWindow::All);
        assert!(!sync.auto_download_attachments);
        assert!(!sync.wifi_only);
        assert_eq!(sync.max_cache_bytes, 2 * 1024 * 1024 * 1024);
        assert_eq!(sync.interval_secs, 300);
    }

    #[test]
    fn notification_defaults_are_on_but_not_only_important() {
        let notifications = NotificationSettings::default();
        assert!(notifications.enabled);
        assert!(notifications.sound);
        assert!(!notifications.only_important);
    }

    #[test]
    fn reading_defaults_match_the_documented_values() {
        let reading = ReadingSettings::default();
        assert!(reading.mark_read_on_open);
        assert_eq!(reading.mark_read_delay_secs, 3);
        assert!(!reading.show_html);
        assert!(!reading.load_remote_images);
        assert_eq!(reading.preview_lines, 2);
    }

    #[test]
    fn appearance_composing_storage_and_security_defaults_are_sane() {
        let appearance = AppearanceSettings::default();
        assert_eq!(appearance.theme, Theme::System);
        assert_eq!(appearance.density, Density::Comfortable);
        assert_eq!(appearance.font_scale, 1.0);

        let composing = ComposingSettings::default();
        assert!(composing.signature.is_empty());
        assert_eq!(composing.format, ComposeFormat::Plain);
        assert!(composing.quote_reply);
        assert!(!composing.reply_above);

        let storage = StorageSettings::default();
        assert_eq!(storage.max_cache_bytes, 2 * 1024 * 1024 * 1024);
        assert_eq!(storage.keep_bodies_days, 30);
        assert!(!storage.wipe_on_logout);

        let security = SecuritySettings::default();
        assert!(security.lock_on_idle);
        assert_eq!(security.idle_lock_secs, 900);
        assert!(security.sanitize_html);
        assert!(!security.allow_remote_content);
    }

    #[test]
    fn attachment_and_search_defaults_are_sane() {
        let attachments = AttachmentSettings::default();
        assert_eq!(attachments.auto_download_max_bytes, 5 * 1024 * 1024);
        assert_eq!(attachments.cache_limit_bytes, 1024 * 1024 * 1024);
        assert!(attachments.download_dir.is_none());

        let search = SearchSettings::default();
        assert!(search.local_first);
        assert!(search.server_fallback);
        assert_eq!(search.max_results, 100);
    }

    #[test]
    fn the_whole_surface_defaults_to_the_documented_defaults() {
        let settings = Settings::default();
        assert_eq!(settings.sync, SyncSettings::default());
        assert_eq!(settings.notifications, NotificationSettings::default());
        assert_eq!(settings.appearance, AppearanceSettings::default());
        assert_eq!(settings.reading, ReadingSettings::default());
        assert_eq!(settings.composing, ComposingSettings::default());
        assert_eq!(settings.attachments, AttachmentSettings::default());
        assert_eq!(settings.search, SearchSettings::default());
        assert_eq!(settings.storage, StorageSettings::default());
        assert_eq!(settings.security, SecuritySettings::default());
    }

    #[tokio::test]
    async fn a_fresh_store_loads_the_defaults() {
        let (_dir, db, store) = store().await;
        let loaded = store.load().await.expect("load");
        assert_eq!(loaded, Settings::default());
        // Only the cache's own bookkeeping row exists; no section was written.
        let keys = stored_keys(&db).await;
        assert!(keys.iter().any(|key| key == "cache.search_mode"));
        assert!(
            !keys
                .iter()
                .any(|key| SECTION_KEYS.contains(&key.as_str())),
            "loading must not create section rows: {keys:?}"
        );
    }

    #[tokio::test]
    async fn save_then_load_round_trips_every_section() {
        let (_dir, _db, store) = store().await;
        let settings = custom_settings();
        store.save(&settings).await.expect("save");
        let loaded = store.load().await.expect("load");
        assert_eq!(loaded, settings);
    }

    #[tokio::test]
    async fn save_writes_one_row_per_section_and_upserts_on_the_second_save() {
        let (_dir, db, store) = store().await;
        store.save(&Settings::default()).await.expect("save");
        let keys = stored_keys(&db).await;
        for section in SECTION_KEYS {
            assert!(keys.iter().any(|key| key == section), "missing {section}");
        }
        assert_eq!(
            keys.iter().filter(|key| SECTION_KEYS.contains(&key.as_str())).count(),
            9
        );

        store
            .save(&custom_settings())
            .await
            .expect("second save");
        let keys = stored_keys(&db).await;
        assert_eq!(
            keys.iter().filter(|key| SECTION_KEYS.contains(&key.as_str())).count(),
            9,
            "saving twice must update the rows, not duplicate them"
        );
    }

    #[tokio::test]
    async fn section_rows_carry_an_updated_at() {
        let (_dir, db, store) = store().await;
        store.save(&Settings::default()).await.expect("save");
        let rows = sqlx::query("SELECT key, updated_at FROM settings WHERE key = ?")
            .bind(KEY_SYNC)
            .fetch_all(db.pool())
            .await
            .expect("rows");
        assert_eq!(rows.len(), 1);
        let stamp: String = rows[0].get("updated_at");
        assert_eq!(crate::util::to_rfc3339(crate::util::parse_rfc3339(&stamp).expect("rfc3339")), stamp);
    }

    #[tokio::test]
    async fn an_unreadable_section_falls_back_to_its_default() {
        let (_dir, db, store) = store().await;
        store.save(&custom_settings()).await.expect("save");
        // A truncated value: the row exists but is not JSON any more.
        db.put_setting(KEY_APPEARANCE, "{\"theme\":")
            .await
            .expect("put");

        let loaded = store.load().await.expect("load");
        assert_eq!(loaded.appearance, AppearanceSettings::default());
        // Every other section is still the stored one.
        assert_eq!(loaded.sync, custom_settings().sync);
        assert_eq!(loaded.security, custom_settings().security);
    }

    #[tokio::test]
    async fn a_section_from_a_newer_build_falls_back_to_the_default() {
        let (_dir, db, store) = store().await;
        store.save(&custom_settings()).await.expect("save");
        // A theme name this build has never heard of (a newer build's value).
        db.put_setting(KEY_APPEARANCE, "{\"theme\":\"solarized\"}")
            .await
            .expect("put");

        let loaded = store.load().await.expect("load");
        assert_eq!(loaded.appearance, AppearanceSettings::default());
        assert_eq!(loaded.search, custom_settings().search);
    }

    #[tokio::test]
    async fn a_missing_section_falls_back_to_its_default() {
        let (_dir, db, store) = store().await;
        let sync = serde_json::to_string(&custom_settings().sync).expect("json");
        db.put_setting(KEY_SYNC, &sync).await.expect("put");

        let loaded = store.load().await.expect("load");
        assert_eq!(loaded.sync, custom_settings().sync);
        assert_eq!(loaded.notifications, NotificationSettings::default());
        assert_eq!(loaded.reading, ReadingSettings::default());
    }

    #[tokio::test]
    async fn update_persists_only_the_intended_field() {
        let (_dir, db, store) = store().await;
        store.save(&custom_settings()).await.expect("save");
        let before = stored_values(&db).await;

        let updated = store
            .update(|settings| settings.appearance.theme = Theme::Light)
            .await
            .expect("update");
        assert_eq!(updated.appearance.theme, Theme::Light);
        assert_eq!(updated.appearance.density, custom_settings().appearance.density);
        assert_eq!(updated.sync, custom_settings().sync);

        let after = stored_values(&db).await;
        let changed: Vec<String> = after
            .iter()
            .filter(|(key, value)| !before.iter().any(|(k, v)| k == key && v == value))
            .map(|(key, _)| key.clone())
            .collect();
        assert_eq!(changed, vec![KEY_APPEARANCE.to_string()]);
    }

    #[tokio::test]
    async fn update_returns_what_it_persisted() {
        let (_dir, _db, store) = store().await;
        let returned = store
            .update(|settings| {
                settings.reading.preview_lines = 5;
                settings.search.max_results = 7;
            })
            .await
            .expect("update");
        assert_eq!(returned.reading.preview_lines, 5);
        assert_eq!(returned.search.max_results, 7);
        let loaded = store.load().await.expect("load");
        assert_eq!(loaded, returned);
    }

    #[tokio::test]
    async fn two_stores_over_the_same_file_agree() {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("c.db");

        let first_db = Arc::new(ClientDatabase::open(&path).await.expect("open"));
        let first = SettingsStore::new(Arc::clone(&first_db));
        first.save(&custom_settings()).await.expect("save");

        // A second handle on the same file, as a restarted shell would open it.
        let second_db = Arc::new(ClientDatabase::open(&path).await.expect("reopen"));
        let second = SettingsStore::new(Arc::clone(&second_db));
        assert_eq!(second.load().await.expect("load"), custom_settings());

        // A change made through the second store is visible to the first.
        second
            .update(|settings| settings.notifications.enabled = false)
            .await
            .expect("update");
        let seen = first.load().await.expect("reload");
        assert!(!seen.notifications.enabled);
        assert_eq!(seen.appearance, custom_settings().appearance);
    }

    #[tokio::test]
    async fn save_leaves_other_settings_keys_untouched() {
        let (_dir, db, store) = store().await;
        db.put_setting("ui.sidebar_width", "280")
            .await
            .expect("put");
        db.put_setting("interactive", "{\"reader_open\":true}")
            .await
            .expect("put");
        let search_mode = db.get_setting("cache.search_mode").await.expect("get");

        store.save(&custom_settings()).await.expect("save");
        store.update(|_| {}).await.expect("update");

        assert_eq!(
            db.get_setting("interactive").await.expect("get"),
            Some("{\"reader_open\":true}".to_string())
        );
        assert_eq!(
            db.get_setting("ui.sidebar_width").await.expect("get"),
            Some("280".to_string())
        );
        assert_eq!(
            db.get_setting("cache.search_mode").await.expect("get"),
            search_mode,
            "the cache's own bookkeeping is not a §52 section"
        );
        let keys = stored_keys(&db).await;
        assert!(keys.iter().any(|key| key == "ui.sidebar_width"));
    }

    #[test]
    fn each_sync_window_maps_to_its_days_and_label() {
        assert_eq!(SyncWindow::All.days(), None);
        assert_eq!(SyncWindow::Days30.days(), Some(30));
        assert_eq!(SyncWindow::Days90.days(), Some(90));
        assert_eq!(SyncWindow::All.label(), "all");
        assert_eq!(SyncWindow::Days30.label(), "30d");
        assert_eq!(SyncWindow::Days90.label(), "90d");
        assert_eq!(SyncWindow::default(), SyncWindow::All);

        let settings = Settings {
            sync: SyncSettings {
                window: SyncWindow::Days90,
                ..SyncSettings::default()
            },
            ..Settings::default()
        };
        assert_eq!(settings.sync_window_days(), Some(90));
    }

    #[test]
    fn sync_window_serde_uses_the_documented_snake_case_strings() {
        for (window, wire) in [
            (SyncWindow::All, "\"all\""),
            (SyncWindow::Days30, "\"days30\""),
            (SyncWindow::Days90, "\"days90\""),
        ] {
            assert_eq!(serde_json::to_string(&window).expect("serialize"), wire);
            let parsed: SyncWindow = serde_json::from_str(wire).expect("deserialize");
            assert_eq!(parsed, window);
        }
        assert_eq!(
            serde_json::to_string(&SyncWindow::default()).expect("serialize"),
            "\"all\""
        );
    }

    #[test]
    fn theme_density_and_compose_format_serde_uses_snake_case() {
        assert_eq!(serde_json::to_string(&Theme::Light).expect("s"), "\"light\"");
        assert_eq!(serde_json::to_string(&Theme::Dark).expect("s"), "\"dark\"");
        assert_eq!(serde_json::to_string(&Theme::System).expect("s"), "\"system\"");
        assert_eq!(Theme::default(), Theme::System);

        assert_eq!(
            serde_json::to_string(&Density::Comfortable).expect("s"),
            "\"comfortable\""
        );
        assert_eq!(
            serde_json::to_string(&Density::Compact).expect("s"),
            "\"compact\""
        );
        assert_eq!(Density::default(), Density::Comfortable);

        assert_eq!(
            serde_json::to_string(&ComposeFormat::Plain).expect("s"),
            "\"plain\""
        );
        assert_eq!(
            serde_json::to_string(&ComposeFormat::Html).expect("s"),
            "\"html\""
        );
        assert_eq!(ComposeFormat::default(), ComposeFormat::Plain);

        // The stored form round-trips, and a section is readable back.
        let section: AppearanceSettings =
            serde_json::from_str("{\"theme\":\"dark\",\"density\":\"compact\",\"font_scale\":1.5}")
                .expect("deserialize");
        assert_eq!(section.theme, Theme::Dark);
        assert_eq!(section.font_scale, 1.5);
    }

    #[test]
    fn a_section_missing_a_newer_field_still_loads() {
        // A section written before `interval_secs` existed keeps every value it
        // does carry and takes the default for the rest.
        let section: SyncSettings =
            serde_json::from_str("{\"window\":\"days30\",\"wifi_only\":true}").expect("deserialize");
        assert_eq!(section.window, SyncWindow::Days30);
        assert!(section.wifi_only);
        assert_eq!(section.interval_secs, SyncSettings::default().interval_secs);
        assert!(!section.auto_download_attachments);
    }

    #[test]
    fn attachment_cache_limit_takes_the_smaller_cap() {
        let mut settings = Settings::default();
        settings.attachments.cache_limit_bytes = 1024 * 1024 * 1024;
        assert_eq!(
            settings.attachment_cache_limit(2 * 1024 * 1024 * 1024),
            1024 * 1024 * 1024,
            "the attachment cap wins when it is smaller"
        );
        assert_eq!(
            settings.attachment_cache_limit(256 * 1024 * 1024),
            256 * 1024 * 1024,
            "the overall cache budget wins when it is smaller"
        );
        assert_eq!(settings.attachment_cache_limit(0), 0);
        assert_eq!(
            settings.attachment_cache_limit(1024 * 1024 * 1024),
            1024 * 1024 * 1024,
            "equal caps give that cap"
        );
    }
}

//! Localisation of the one thing the API says in prose: the error envelope's
//! `message`.
//!
//! # What is localised, and what is not
//!
//! A `code` is already machine-readable and language-neutral (`docs/api.md` §1.3), so
//! it never changes with the locale. `message` is the human half, and it is the only
//! field this module touches.
//!
//! The message is *derived*, not authored per call site: [`ferroma_core::FerromaError`]
//! renders as `"<kind>: <detail>"`, and so does every error the API surfaces. The kind
//! comes from a closed set of variants, and the detail from a small, enumerable
//! vocabulary — `not found: domain 7`, `unauthorized: session expired or revoked`,
//! `domain longer than 253 bytes`. Translating the two halves separately is what makes
//! full coverage possible without a message key threaded through 126 call sites.
//!
//! A detail this build has no translation for is passed through in English rather than
//! dropped: a partially translated message is useful, a blank one is not.
//!
//! # Negotiation
//!
//! [`negotiate`] reads `Accept-Language` (RFC 9110) once per request and stores the
//! result in a task-local, which [`ErrorBody`](crate::error::ErrorBody) rendering reads
//! back. That keeps every handler signature untouched: an error is written the way it
//! always was, and only its rendering is locale-aware.
//!
//! English is the default for a request that asks for nothing, asks for a language this
//! build does not have, or arrives without the header at all.

use axum::extract::Request;
use axum::http::header::ACCEPT_LANGUAGE;
use axum::middleware::Next;
use axum::response::Response;

/// The languages this build answers in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Locale {
    /// English, and the fallback for everything unrecognised.
    En,
    /// Simplified Chinese.
    ZhCn,
}

tokio::task_local! {
    /// The locale negotiated for the request being handled.
    static LOCALE: Locale;
}

/// The locale of the request being handled, or [`Locale::En`] outside a request.
///
/// Outside a request — a unit test, a background task — there is nothing to negotiate
/// with, so English is the honest answer rather than a guess.
#[must_use]
pub fn current() -> Locale {
    LOCALE.try_with(|locale| *locale).unwrap_or(Locale::En)
}

/// The axum middleware that negotiates [`LOCALE`] for one request.
pub async fn negotiate(request: Request, next: Next) -> Response {
    let locale = Locale::from_accept_language(
        request
            .headers()
            .get(ACCEPT_LANGUAGE)
            .and_then(|value| value.to_str().ok()),
    );
    LOCALE.scope(locale, next.run(request)).await
}

impl Locale {
    /// The tag this locale labels itself with, matching what it accepts.
    #[must_use]
    pub fn tag(self) -> &'static str {
        match self {
            Locale::En => "en",
            Locale::ZhCn => "zh-CN",
        }
    }

    /// Negotiate from an `Accept-Language` header value.
    ///
    /// A `zh` of any region (`zh`, `zh-CN`, `zh-Hans`, `zh_TW`) selects Simplified
    /// Chinese: this build ships one Chinese catalog, and answering a Traditional
    /// reader in Simplified is closer than answering them in English.
    #[must_use]
    pub fn from_accept_language(raw: Option<&str>) -> Locale {
        let Some(raw) = raw else {
            return Locale::En;
        };
        // RFC 9110: the entry with the highest quality wins, and the header's own order
        // breaks a tie. `q=0` means "explicitly not acceptable".
        let mut best: Option<(f32, Locale)> = None;
        for entry in raw.split(',') {
            let mut parts = entry.split(';');
            let tag = parts.next().unwrap_or("").trim();
            if tag.is_empty() {
                continue;
            }
            let mut quality = 1.0_f32;
            for parameter in parts {
                if let Some(value) = parameter.trim().strip_prefix("q=") {
                    quality = value.trim().parse().unwrap_or(0.0);
                }
            }
            if quality <= 0.0 {
                continue;
            }
            let Some(locale) = Self::from_tag(tag) else {
                continue;
            };
            if best.is_none_or(|(seen, _)| quality > seen) {
                best = Some((quality, locale));
            }
        }
        best.map(|(_, locale)| locale).unwrap_or(Locale::En)
    }

    /// Map one language tag onto a catalog, if this build has one.
    fn from_tag(tag: &str) -> Option<Locale> {
        let primary = tag
            .split(['-', '_'])
            .next()?
            .trim()
            .to_ascii_lowercase();
        match primary.as_str() {
            "zh" => Some(Locale::ZhCn),
            "en" => Some(Locale::En),
            _ => None,
        }
    }

    /// Translate one English API message into this locale.
    #[must_use]
    pub fn message(self, english: &str) -> String {
        if self == Locale::En {
            return english.to_string();
        }
        // `FerromaError` renders as `"<kind>: <detail>"`. Both halves are translated
        // when they are known; the detail is passed through when it is not.
        if let Some((kind, detail)) = english.split_once(": ") {
            if let Some(head) = kind_zh(kind) {
                let body = detail_zh(detail).unwrap_or_else(|| detail.to_string());
                return format!("{head}：{body}");
            }
        }
        detail_zh(english).unwrap_or_else(|| english.to_string())
    }
}

/// The translation of a `FerromaError` variant's prefix, if it has one.
fn kind_zh(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "configuration error" => "配置错误",
        "i/o error" => "I/O 错误",
        "parse error" => "解析错误",
        "not found" => "未找到",
        "conflict" => "冲突",
        "invalid input" => "输入无效",
        "unauthorized" => "未授权",
        "forbidden" => "无权限",
        "limit exceeded" => "超出限制",
        "mailbox full" => "邮箱已满",
        "rate limited" => "请求过于频繁",
        "protocol error" => "协议错误",
        "tls error" => "TLS 错误",
        "dns error" => "DNS 错误",
        "storage error" => "存储错误",
        "network error" => "网络错误",
        "unsupported" => "不支持",
        "timed out" => "已超时",
        _ => return None,
    })
}

/// The nouns that appear inside a `"<entity> <id>"` or `"no such <entity>"` detail.
fn entity_zh(entity: &str) -> Option<&'static str> {
    Some(match entity {
        "domain" => "域名",
        "user" => "用户",
        "mailbox" => "邮箱",
        "folder" => "文件夹",
        "message" => "邮件",
        "draft" => "草稿",
        "attachment" => "附件",
        "queue entry" => "队列条目",
        "device" => "设备",
        "alias" => "别名",
        "session" => "会话",
        "token" => "令牌",
        "contact" => "联系人",
        "label" => "标签",
        _ => return None,
    })
}

/// Translate a message or a message's detail half.
///
/// `None` means "no translation here", never "empty": the caller falls back to English.
fn detail_zh(detail: &str) -> Option<String> {
    // Whole sentences and short phrases, matched exactly.
    if let Some(hit) = EXACT.get(detail) {
        return Some((*hit).to_string());
    }

    // `"<entity> <id>"` — the shape `not_found(format!("domain {id}"))` produces.
    if let Some((entity, id)) = detail.rsplit_once(' ') {
        if !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()) {
            if let Some(name) = entity_zh(entity) {
                return Some(format!("{name} {id}"));
            }
        }
    }

    // `"no such mailbox alice@example.com"`.
    if let Some(what) = detail.strip_prefix("no such ") {
        let (head, tail) = match what.split_once(' ') {
            Some((head, tail)) => (head, Some(tail)),
            None => (what, None),
        };
        let name = entity_zh(head).unwrap_or(head);
        return Some(match tail {
            Some(tail) => format!("没有这个{name} {tail}"),
            None => format!("没有这个{name}"),
        });
    }

    // Messages that carry a value: `"<template>{}<suffix>"`.
    for (prefix, suffix, template) in TEMPLATES {
        if let Some(rest) = detail
            .strip_prefix(prefix)
            .and_then(|rest| rest.strip_suffix(suffix))
        {
            return Some(template.replace("{}", rest));
        }
    }

    None
}

/// Messages with no parameters.
static EXACT: std::sync::LazyLock<std::collections::HashMap<&'static str, &'static str>> =
    std::sync::LazyLock::new(|| {
        [
            // Authentication and sessions.
            ("current password is incorrect", "当前密码不正确"),
            ("invalid email address or password", "邮箱地址或密码不正确"),
            ("account disabled", "账号已被禁用"),
            ("account no longer exists", "账号已不存在"),
            ("session expired or revoked", "会话已过期或被撤销"),
            ("session no longer exists", "会话已不存在"),
            ("invalid refresh token", "刷新令牌无效"),
            ("refresh token expired", "刷新令牌已过期"),
            ("token expired", "令牌已过期"),
            ("token does not match its session", "令牌与会话不匹配"),
            ("rate limited", "请求过于频繁"),
            ("the server could not complete the request", "服务器无法完成该请求"),
            // Registration and credentials.
            ("password must not be blank", "密码不能为空"),
            ("device_uid must not be empty", "device_uid 不能为空"),
            ("device_uid is too long", "device_uid 过长"),
            ("operation_id must not be empty", "operation_id 不能为空"),
            // Folders and mail.
            ("no such folder", "没有这个文件夹"),
            ("the Archive folder is missing", "缺少「归档」文件夹"),
            ("the Trash folder is missing", "缺少「废件箱」文件夹"),
            // Address validation (`ferroma_core::address`).
            ("empty local part", "地址的本地部分为空"),
            ("empty domain", "域名为空"),
            (
                "domain literals are not valid mailbox domains",
                "不能使用域名标识（[..]）作为邮箱域名",
            ),
            ("control character in quoted local part", "引号本地部分中含有控制字符"),
            ("this server already has an administrator", "本服务器已存在管理员账号"),
            (
                "the setup wizard is disabled by api.enable_setup_wizard",
                "初始化向导已被 api.enable_setup_wizard 关闭",
            ),
        ]
        .into_iter()
        .collect()
    });

/// `(prefix, suffix, template)`: a message that carries one value in the middle.
///
/// The value is substituted for `{}` in the template.
static TEMPLATES: &[(&str, &str, &str)] = &[
    ("upload chunk ", " has not been received", "上传分块 {} 尚未收到"),
    ("from is not a valid address: ", "", "发件人不是有效地址：{}"),
    ("request body must be JSON: ", "", "请求体必须是 JSON：{}"),
    ("malformed multipart body: ", "", "multipart 请求体格式错误：{}"),
    ("invalid base64: ", "", "base64 无效：{}"),
    ("message body missing at ", "", "以下位置缺少邮件正文：{}"),
    ("malformed address: ", "", "地址格式错误：{}"),
    ("address has no domain: ", "", "地址缺少域名：{}"),
    ("address has no local part: ", "", "地址缺少本地部分：{}"),
    (
        "invalid dot placement in local part: ",
        "",
        "本地部分的点号位置无效：{}",
    ),
    ("invalid character in domain ", "", "域名 {} 中含有无效字符"),
    ("empty label in domain ", "", "域名 {} 中含有空标签"),
    ("label too long in domain ", "", "域名 {} 中含有过长的标签"),
    (
        "label starts or ends with hyphen in ",
        "",
        "域名 {} 的标签以连字符开头或结尾",
    ),
    ("operation already applied: ", "", "操作已应用：{}"),
    ("this server advertises ", "", "本服务器对外声明的域名是 {}"),
    ("domain longer than ", " bytes", "域名长度超过 {} 字节"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_header_is_english() {
        assert_eq!(Locale::from_accept_language(None), Locale::En);
        assert_eq!(Locale::from_accept_language(Some("")), Locale::En);
    }

    #[test]
    fn chinese_of_any_region_selects_the_simplified_catalog() {
        for header in ["zh", "zh-CN", "zh_CN", "zh-Hans", "zh-Hans-CN", "ZH-cn"] {
            assert_eq!(
                Locale::from_accept_language(Some(header)),
                Locale::ZhCn,
                "{header}"
            );
        }
    }

    #[test]
    fn the_highest_quality_entry_wins() {
        assert_eq!(
            Locale::from_accept_language(Some("en-US,en;q=0.9,zh-CN;q=0.8")),
            Locale::En
        );
        assert_eq!(
            Locale::from_accept_language(Some("en;q=0.2,zh-CN;q=0.9")),
            Locale::ZhCn
        );
        // Order breaks a tie, as RFC 9110 says.
        assert_eq!(
            Locale::from_accept_language(Some("zh-CN,en;q=0.8")),
            Locale::ZhCn
        );
    }

    #[test]
    fn an_unsupported_language_falls_back_to_english() {
        assert_eq!(Locale::from_accept_language(Some("fr-FR,de;q=0.9")), Locale::En);
        assert_eq!(Locale::from_accept_language(Some("*")), Locale::En);
    }

    #[test]
    fn a_language_marked_unacceptable_is_not_chosen() {
        assert_eq!(
            Locale::from_accept_language(Some("zh;q=0,en;q=0.5")),
            Locale::En
        );
    }

    #[test]
    fn english_is_returned_unchanged() {
        let message = "not found: domain 7";
        assert_eq!(Locale::En.message(message), message);
    }

    #[test]
    fn a_kind_and_a_detail_are_both_translated() {
        assert_eq!(
            Locale::ZhCn.message("not found: domain 7"),
            "未找到：域名 7"
        );
        assert_eq!(
            Locale::ZhCn.message("unauthorized: session expired or revoked"),
            "未授权：会话已过期或被撤销"
        );
        assert_eq!(
            Locale::ZhCn.message("unauthorized: current password is incorrect"),
            "未授权：当前密码不正确"
        );
        assert_eq!(
            Locale::ZhCn.message("rate limited"),
            "请求过于频繁"
        );
        assert_eq!(
            Locale::ZhCn.message("invalid input: domain longer than 253 bytes"),
            "输入无效：域名长度超过 253 字节"
        );
    }

    #[test]
    fn an_unknown_detail_keeps_its_english_rather_than_vanishing() {
        // A partially translated message is useful; a blank one is not.
        assert_eq!(
            Locale::ZhCn.message("invalid input: something this build has never seen"),
            "输入无效：something this build has never seen"
        );
        assert_eq!(
            Locale::ZhCn.message("a message with no kind at all"),
            "a message with no kind at all"
        );
    }

    #[test]
    fn the_current_locale_is_english_outside_a_request() {
        assert_eq!(current(), Locale::En);
    }

    #[tokio::test]
    async fn the_scope_makes_the_locale_visible_to_the_handler_below_it() {
        let seen = LOCALE
            .scope(Locale::ZhCn, async { current() })
            .await;
        assert_eq!(seen, Locale::ZhCn);
        // …and it does not leak out of the scope.
        assert_eq!(current(), Locale::En);
    }
}

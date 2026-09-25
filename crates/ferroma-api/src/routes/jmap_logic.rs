//! Pure JMAP request pieces the HTTP adapter shares.
//!
//! Result references, keyword patches and filter conditions are data
//! transformations. They live here so a test can fail them without a database,
//! and so [`super::jmap`] stays an adapter over the mail service.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Map, Value};

/// One resolved method response, in call order.
pub struct CallOutcome {
    /// The `methodCall` id the client sent.
    pub id: String,
    /// The response method name (`Email/get`, `error`, …).
    pub name: String,
    /// The response arguments.
    pub body: Value,
}

/// Replace `#argument` result references before a method runs.
///
/// RFC 8620 §3.7: a name beginning with `#` is a `ResultReference`. The same
/// argument in both forms is `invalidArguments`. A reference that does not
/// resolve is `invalidResultReference`, and the method is not run.
pub fn resolve_references(
    args: &Map<String, Value>,
    prior: &[CallOutcome],
) -> Result<Map<String, Value>, (String, String)> {
    let mut resolved = Map::new();
    let mut referenced = BTreeSet::new();
    for (name, value) in args {
        if let Some(argument) = name.strip_prefix('#') {
            if args.contains_key(argument) {
                return Err((
                    "invalidArguments".to_string(),
                    format!("{argument} is given both directly and as a result reference"),
                ));
            }
            let reference = value.as_object().ok_or_else(|| {
                (
                    "invalidResultReference".to_string(),
                    format!("#{argument} is not a ResultReference"),
                )
            })?;
            let result_of = reference.get("resultOf").and_then(Value::as_str).ok_or_else(|| {
                (
                    "invalidResultReference".to_string(),
                    "resultOf is required".to_string(),
                )
            })?;
            let response_name = reference.get("name").and_then(Value::as_str).ok_or_else(|| {
                (
                    "invalidResultReference".to_string(),
                    "name is required".to_string(),
                )
            })?;
            let path = reference.get("path").and_then(Value::as_str).ok_or_else(|| {
                (
                    "invalidResultReference".to_string(),
                    "path is required".to_string(),
                )
            })?;
            let previous = prior.iter().find(|call| call.id == result_of).ok_or_else(|| {
                (
                    "invalidResultReference".to_string(),
                    format!("no previous call {result_of}"),
                )
            })?;
            if previous.name != response_name {
                return Err((
                    "invalidResultReference".to_string(),
                    format!("call {result_of} did not return {response_name}"),
                ));
            }
            let found = pointer(&previous.body, path).ok_or_else(|| {
                (
                    "invalidResultReference".to_string(),
                    format!("path {path} did not resolve"),
                )
            })?;
            resolved.insert(argument.to_string(), found);
            referenced.insert(argument.to_string());
        }
    }
    for (name, value) in args {
        if name.starts_with('#') || referenced.contains(name) {
            continue;
        }
        resolved.insert(name.clone(), value.clone());
    }
    Ok(resolved)
}

/// A JSON Pointer with the leading slash, as RFC 8620 uses it (`/ids`, `/list/*/id`).
///
/// `*` walks every element of an array. Anything else is a literal property or
/// index. A missing step is a failed reference, not an empty value.
fn pointer(value: &Value, path: &str) -> Option<Value> {
    let path = path.strip_prefix('/')?;
    if path.is_empty() {
        return Some(value.clone());
    }
    walk(value, path)
}

fn walk(value: &Value, path: &str) -> Option<Value> {
    let (step, rest) = match path.split_once('/') {
        Some((step, rest)) => (step, Some(rest)),
        None => (path, None),
    };
    let step = step.replace("~1", "/").replace("~0", "~");
    let next = if step == "*" {
        let items = value.as_array()?;
        if let Some(rest) = rest {
            let mut collected = Vec::new();
            for item in items {
                collected.push(walk(item, rest)?);
            }
            Value::Array(collected)
        } else {
            value.clone()
        }
    } else if let Some(object) = value.as_object() {
        let child = object.get(&step)?;
        match rest {
            Some(rest) => walk(child, rest)?,
            None => child.clone(),
        }
    } else if let Ok(index) = step.parse::<usize>() {
        let child = value.as_array()?.get(index)?;
        match rest {
            Some(rest) => walk(child, rest)?,
            None => child.clone(),
        }
    } else {
        return None;
    };
    Some(next)
}

/// Apply a JMAP keyword patch to the stored flag string.
///
/// A full `keywords` object replaces the set. `keywords/$seen` adds or removes
/// one keyword (RFC 8620 §5.3). A keyword Ferroma does not store — anything
/// other than the four shared flags and a `$`-less private keyword — is refused
/// rather than silently dropped, because a client that set `$answered` and then
/// read it back would otherwise see it gone.
pub fn apply_keyword_patch(
    flags: &str,
    patch: &Map<String, Value>,
) -> Result<Option<String>, (String, String)> {
    let mut tokens: BTreeSet<String> = flags
        .split_whitespace()
        .map(|flag| flag.to_ascii_lowercase())
        .collect();
    let mut touched = false;
    if let Some(keywords) = patch.get("keywords") {
        let object = keywords.as_object().ok_or_else(|| {
            (
                "invalidProperties".to_string(),
                "keywords must be an object".to_string(),
            )
        })?;
        tokens.clear();
        for (keyword, value) in object {
            if value.as_bool() != Some(true) {
                continue;
            }
            tokens.insert(keyword_to_flag(keyword)?);
        }
        touched = true;
    }
    for (name, value) in patch {
        let Some(keyword) = name.strip_prefix("keywords/") else {
            continue;
        };
        let keyword = keyword.replace("~1", "/").replace("~0", "~");
        let present = value.as_bool().ok_or_else(|| {
            (
                "invalidProperties".to_string(),
                format!("{name} must be a boolean"),
            )
        })?;
        let flag = keyword_to_flag(&keyword)?;
        if present {
            tokens.insert(flag);
        } else {
            tokens.remove(&flag);
        }
        touched = true;
    }
    if !touched {
        return Ok(None);
    }
    // Stable order so two equal sets compare equal and a no-op patch does not
    // rewrite the Maildir name.
    let mut ordered: Vec<String> = tokens.into_iter().collect();
    ordered.sort();
    Ok(Some(ordered.join(" ")))
}

fn keyword_to_flag(keyword: &str) -> Result<String, (String, String)> {
    let flag = match keyword {
        "$seen" => "seen".to_string(),
        "$flagged" => "flagged".to_string(),
        "$answered" => "answered".to_string(),
        "$draft" => "draft".to_string(),
        other if keyword_is_private(other) => other.to_ascii_lowercase(),
        _ => {
            return Err((
                "invalidProperties".to_string(),
                format!("unsupported keyword {keyword}"),
            ));
        }
    };
    Ok(flag)
}

/// A private keyword a client invented: no `$`, no whitespace, not empty.
fn keyword_is_private(keyword: &str) -> bool {
    !keyword.is_empty()
        && !keyword.starts_with('$')
        && keyword.chars().all(|ch| !ch.is_whitespace() && ch != '/')
}

/// The one mailbox an Email may belong to.
///
/// The account advertises `maxMailboxesPerEmail: 1`, so a patch that names two
/// mailboxes is `invalidProperties` rather than a silent pick of the first.
pub fn single_mailbox(mailbox_ids: &Map<String, Value>) -> Result<i64, (String, String)> {
    let chosen = mailbox_ids
        .iter()
        .filter(|(_, member)| member.as_bool() == Some(true))
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    if chosen.len() != 1 {
        return Err((
            "invalidProperties".to_string(),
            "an email belongs to exactly one mailbox".to_string(),
        ));
    }
    chosen[0].parse::<i64>().map_err(|_| {
        (
            "invalidProperties".to_string(),
            "mailboxIds contains an unknown mailbox".to_string(),
        )
    })
}

/// A filter a client can express against the stored message columns.
///
/// Unsupported conditions are rejected. Silently ignoring `from` or a date
/// would return a superset and a client would show mail the filter excluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmailFilter {
    /// `inMailbox`.
    pub in_mailbox: Option<i64>,
    /// Case-insensitive substrings of the sender (`from`).
    pub from: Vec<String>,
    /// Case-insensitive substrings of To (`to`).
    pub to: Vec<String>,
    /// Case-insensitive substrings of the subject.
    pub subject: Vec<String>,
    /// Free text matched against subject, sender and body (`text` and `body`).
    pub text: Vec<String>,
    /// Keywords the message must have, as stored flag names.
    pub has_keyword: Vec<String>,
    /// Keywords the message must not have.
    pub not_keyword: Vec<String>,
    /// `hasAttachment`.
    pub has_attachment: Option<bool>,
    /// `after`, inclusive, RFC 3339.
    pub after: Option<String>,
    /// `before`, exclusive, RFC 3339.
    pub before: Option<String>,
}

impl EmailFilter {
    /// Parse a `FilterCondition`. `FilterOperator` is not implemented.
    pub fn parse(value: Option<&Value>) -> Result<Self, (String, String)> {
        let Some(value) = value else {
            return Ok(Self::empty());
        };
        let object = value.as_object().ok_or_else(|| {
            (
                "invalidArguments".to_string(),
                "filter must be an object".to_string(),
            )
        })?;
        if object.contains_key("operator") {
            return Err((
                "invalidArguments".to_string(),
                "filter operators are not supported".to_string(),
            ));
        }
        let mut filter = Self::empty();
        for (name, value) in object {
            match name.as_str() {
                "inMailbox" => {
                    let id = value.as_str().ok_or_else(|| invalid_filter("inMailbox"))?;
                    filter.in_mailbox = Some(id.parse::<i64>().map_err(|_| invalid_filter("inMailbox"))?);
                }
                "from" => filter.from.push(required_string(value, "from")?),
                "to" => filter.to.push(required_string(value, "to")?),
                "subject" => filter.subject.push(required_string(value, "subject")?),
                "text" | "body" => filter.text.push(required_string(value, name)?),
                "hasKeyword" => filter
                    .has_keyword
                    .push(keyword_to_flag(value.as_str().ok_or_else(|| invalid_filter("hasKeyword"))?)?),
                "notKeyword" => filter
                    .not_keyword
                    .push(keyword_to_flag(value.as_str().ok_or_else(|| invalid_filter("notKeyword"))?)?),
                "hasAttachment" => {
                    filter.has_attachment =
                        Some(value.as_bool().ok_or_else(|| invalid_filter("hasAttachment"))?);
                }
                "after" => filter.after = Some(required_string(value, "after")?),
                "before" => filter.before = Some(required_string(value, "before")?),
                "inMailboxOtherThan"
                | "cc"
                | "bcc"
                | "header"
                | "minSize"
                | "maxSize"
                | "allInThreadHaveKeyword"
                | "someInThreadHaveKeyword"
                | "noneInThreadHaveKeyword" => {
                    return Err((
                        "invalidArguments".to_string(),
                        format!("filter condition {name} is not supported"),
                    ));
                }
                other => {
                    return Err((
                        "invalidArguments".to_string(),
                        format!("unknown filter condition {other}"),
                    ));
                }
            }
        }
        Ok(filter)
    }

    fn empty() -> Self {
        Self {
            in_mailbox: None,
            from: Vec::new(),
            to: Vec::new(),
            subject: Vec::new(),
            text: Vec::new(),
            has_keyword: Vec::new(),
            not_keyword: Vec::new(),
            has_attachment: None,
            after: None,
            before: None,
        }
    }
}

fn required_string(value: &Value, name: &str) -> Result<String, (String, String)> {
    value
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| invalid_filter(name))
}

fn invalid_filter(name: &str) -> (String, String) {
    (
        "invalidArguments".to_string(),
        format!("filter condition {name} has the wrong type"),
    )
}

/// A sort the query can apply. The property is one `emailQuerySortOptions` names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmailSort {
    /// `receivedAt`.
    ReceivedAt,
    /// `sentAt`, with undated mail last.
    SentAt,
    /// `size`.
    Size,
    /// The first From address.
    From,
    /// The subject.
    Subject,
}

impl EmailSort {
    /// Parse `sort`. An empty list is the default, newest first.
    ///
    /// Only one comparator is applied. A second one is rejected so a client
    /// does not believe a tie-break happened.
    pub fn parse(value: Option<&Value>) -> Result<(Self, bool), (String, String)> {
        let Some(value) = value else {
            return Ok((EmailSort::ReceivedAt, true));
        };
        let list = value.as_array().ok_or_else(|| {
            (
                "invalidArguments".to_string(),
                "sort must be an array".to_string(),
            )
        })?;
        if list.is_empty() {
            return Ok((EmailSort::ReceivedAt, true));
        }
        if list.len() > 1 {
            return Err((
                "invalidArguments".to_string(),
                "only one sort comparator is supported".to_string(),
            ));
        }
        let comparator = list[0].as_object().ok_or_else(|| {
            (
                "invalidArguments".to_string(),
                "a sort comparator must be an object".to_string(),
            )
        })?;
        let property = comparator.get("property").and_then(Value::as_str).ok_or_else(|| {
            (
                "invalidArguments".to_string(),
                "sort property is required".to_string(),
            )
        })?;
        let descending = comparator.get("isAscending").and_then(Value::as_bool) == Some(false);
        let sort = match property {
            "receivedAt" => EmailSort::ReceivedAt,
            "sentAt" => EmailSort::SentAt,
            "size" => EmailSort::Size,
            "from" => EmailSort::From,
            "subject" => EmailSort::Subject,
            other => {
                return Err((
                    "invalidArguments".to_string(),
                    format!("cannot sort by {other}"),
                ));
            }
        };
        Ok((sort, descending))
    }
}

/// `Email/get` properties the server can return.
pub fn known_email_property(name: &str) -> bool {
    matches!(
        name,
        "id" | "blobId"
            | "threadId"
            | "mailboxIds"
            | "keywords"
            | "size"
            | "receivedAt"
            | "messageId"
            | "inReplyTo"
            | "references"
            | "sender"
            | "from"
            | "to"
            | "cc"
            | "bcc"
            | "replyTo"
            | "subject"
            | "sentAt"
            | "hasAttachment"
            | "preview"
            | "textBody"
            | "htmlBody"
            | "bodyValues"
            | "attachments"
            | "bodyStructure"
    ) || name.starts_with("header:")
}

/// Keep only the properties a client asked for. `id` is always returned.
pub fn select_properties(email: &mut Map<String, Value>, properties: Option<&[String]>) {
    let Some(properties) = properties else {
        return;
    };
    let keep: BTreeSet<&str> = properties.iter().map(String::as_str).chain(["id"]).collect();
    email.retain(|name, _| keep.contains(name.as_str()));
}

/// A creation id (`#draft`) recorded while this request runs.
pub fn creation_id(created: &BTreeMap<String, String>, reference: &str) -> Option<String> {
    let id = reference.strip_prefix('#')?;
    created.get(id).cloned()
}

/// The arguments object of a method error.
pub fn method_error(kind: &str, description: &str) -> Value {
    json!({"type": kind, "description": description})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(id: &str, name: &str, body: Value) -> CallOutcome {
        CallOutcome {
            id: id.to_string(),
            name: name.to_string(),
            body,
        }
    }

    #[test]
    fn a_result_reference_fills_the_ids_of_the_previous_query() {
        let prior = vec![outcome(
            "q",
            "Email/query",
            json!({"ids": ["3", "9"], "position": 0}),
        )];
        let args = serde_json::from_value(json!({
            "accountId": "u1",
            "#ids": {"resultOf": "q", "name": "Email/query", "path": "/ids"}
        }))
        .expect("args");
        let resolved = resolve_references(&args, &prior).expect("reference");
        assert_eq!(resolved.get("ids"), Some(&json!(["3", "9"])));
        assert!(resolved.get("#ids").is_none());
        assert_eq!(resolved.get("accountId").and_then(Value::as_str), Some("u1"));
    }

    #[test]
    fn a_star_path_collects_every_id() {
        let prior = vec![outcome(
            "g",
            "Email/get",
            json!({"list": [{"id": "3"}, {"id": "9"}]}),
        )];
        let args = serde_json::from_value(json!({
            "#ids": {"resultOf": "g", "name": "Email/get", "path": "/list/*/id"}
        }))
        .expect("args");
        let resolved = resolve_references(&args, &prior).expect("reference");
        assert_eq!(resolved.get("ids"), Some(&json!(["3", "9"])));
    }

    #[test]
    fn a_missing_reference_is_rejected_before_the_method_runs() {
        let args = serde_json::from_value(json!({
            "#ids": {"resultOf": "missing", "name": "Email/query", "path": "/ids"}
        }))
        .expect("args");
        let error = resolve_references(&args, &[]).expect_err("missing call");
        assert_eq!(error.0, "invalidResultReference");
    }

    #[test]
    fn the_same_argument_cannot_arrive_twice() {
        let args = serde_json::from_value(json!({
            "ids": ["1"],
            "#ids": {"resultOf": "q", "name": "Email/query", "path": "/ids"}
        }))
        .expect("args");
        assert_eq!(
            resolve_references(&args, &[]).expect_err("duplicate").0,
            "invalidArguments"
        );
    }

    #[test]
    fn a_keyword_patch_adds_and_removes_without_dropping_the_rest() {
        let patch = serde_json::from_value(json!({
            "keywords/$seen": true,
            "keywords/$flagged": false
        }))
        .expect("patch");
        let flags = apply_keyword_patch("flagged answered project", &patch)
            .expect("patch")
            .expect("changed");
        assert_eq!(flags, "answered project seen");
    }

    #[test]
    fn replacing_keywords_is_the_whole_set() {
        let patch = serde_json::from_value(json!({"keywords": {"$draft": true, "$seen": true}}))
            .expect("patch");
        let flags = apply_keyword_patch("flagged", &patch)
            .expect("patch")
            .expect("changed");
        assert_eq!(flags, "draft seen");
    }

    #[test]
    fn an_unknown_dollar_keyword_is_not_silently_dropped() {
        let patch = serde_json::from_value(json!({"keywords/$junk": true})).expect("patch");
        assert!(apply_keyword_patch("", &patch).is_err());
    }

    #[test]
    fn seen_means_seen() {
        let filter = EmailFilter::parse(Some(&json!({"hasKeyword": "$seen"}))).expect("filter");
        assert_eq!(filter.has_keyword, ["seen"]);
        let unread = EmailFilter::parse(Some(&json!({"notKeyword": "$seen"}))).expect("filter");
        assert_eq!(unread.not_keyword, ["seen"]);
    }

    #[test]
    fn an_or_filter_is_rejected_rather_than_applied_as_and() {
        let filter = json!({"operator": "OR", "conditions": []});
        assert!(EmailFilter::parse(Some(&filter)).is_err());
    }

    #[test]
    fn sort_defaults_to_newest_received_and_rejects_a_tie_break() {
        assert_eq!(
            EmailSort::parse(None).expect("default"),
            (EmailSort::ReceivedAt, true)
        );
        let sort = EmailSort::parse(Some(&json!([{"property": "subject", "isAscending": true}])))
            .expect("subject");
        assert_eq!(sort, (EmailSort::Subject, false));
        assert!(EmailSort::parse(Some(&json!([
            {"property": "receivedAt"},
            {"property": "size"}
        ])))
        .is_err());
    }

    #[test]
    fn property_selection_always_keeps_the_id() {
        let mut email = serde_json::from_value(json!({"id": "4", "subject": "Hi", "preview": "there"}))
            .expect("email");
        select_properties(&mut email, Some(&["subject".to_string()]));
        assert!(email.contains_key("id"));
        assert!(email.contains_key("subject"));
        assert!(!email.contains_key("preview"));
    }
}

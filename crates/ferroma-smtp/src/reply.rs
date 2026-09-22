//! SMTP replies: the exact bytes we send back.
//!
//! Every reply the server can produce is constructed here, so the same failure is
//! never worded two ways. The catalogue follows RFC 5321 §4.2 (reply codes),
//! RFC 3463 (enhanced status codes) and RFC 4954 (SASL).
//!
//! ```text
//! 250 2.1.0 Ok
//! 250-STARTTLS
//! 250 SIZE 26214400
//! ```
//!
//! A single-line reply is `<code> <text>`; a multi-line reply is `<code>-<text>` for
//! every line but the last, which closes with `<code> <text>` (RFC 5321 §4.2.1).
//! Every reply rendered here ends in CRLF — [`Reply::render`] is the only place in
//! the crate that writes line endings, so there is exactly one place to get wrong.

use std::fmt;

use ferroma_core::FerromaError;

/// One SMTP reply, or one block of a multi-line reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// A single line: `<code> <text>`.
    Line {
        /// The three-digit reply code.
        code: u16,
        /// A human-readable reason, never containing CR or LF.
        text: String,
    },
    /// A block: `250-…` lines closed by the *same* `250 …` code.
    ///
    /// Used for the `EHLO` extension list and for multi-line errors such as an
    /// oversized message with an explanation.
    Multi {
        /// The three-digit reply code shared by every line.
        code: u16,
        /// The lines, in order. The last one closes the block.
        lines: Vec<String>,
    },
}

impl Reply {
    /// A single-line reply.
    pub fn line(code: u16, text: impl Into<String>) -> Self {
        Reply::Line {
            code,
            text: sanitise(&text.into()),
        }
    }

    /// A multi-line reply. An empty `lines` becomes a single empty line, because
    /// a reply whose payload vanishes would leave the client waiting.
    pub fn multi(code: u16, lines: Vec<String>) -> Self {
        if lines.is_empty() {
            return Reply::line(code, "");
        }
        Reply::Multi {
            code,
            lines: lines.iter().map(|l| sanitise(l)).collect(),
        }
    }

    /// The reply code: `250`, `334`, `550`, …
    pub fn code(&self) -> u16 {
        match self {
            Reply::Line { code, .. } | Reply::Multi { code, .. } => *code,
        }
    }

    /// The first line's text, for logging and for tests.
    pub fn text(&self) -> &str {
        match self {
            Reply::Line { text, .. } => text,
            Reply::Multi { lines, .. } => lines.first().map(String::as_str).unwrap_or(""),
        }
    }

    /// Every line's text.
    pub fn lines(&self) -> Vec<&str> {
        match self {
            Reply::Line { text, .. } => vec![text.as_str()],
            Reply::Multi { lines, .. } => lines.iter().map(String::as_str).collect(),
        }
    }

    /// `true` for a 2xx reply.
    pub fn is_positive(&self) -> bool {
        (200..300).contains(&self.code())
    }

    /// `true` for a 3xx reply (an intermediate step the client must continue).
    pub fn is_intermediate(&self) -> bool {
        (300..400).contains(&self.code())
    }

    /// `true` for a 4xx reply: the peer may retry later.
    pub fn is_transient_negative(&self) -> bool {
        (400..500).contains(&self.code())
    }

    /// `true` for a 5xx reply: the peer must not retry.
    pub fn is_permanent_negative(&self) -> bool {
        (500..600).contains(&self.code())
    }

    /// The wire bytes, always CRLF-terminated.
    ///
    /// For byte-exact assertions in tests and for the write path in [`crate::server`].
    pub fn render(&self) -> Vec<u8> {
        match self {
            Reply::Line { code, text } => {
                if text.is_empty() {
                    format!("{code} \r\n").into_bytes()
                } else {
                    format!("{code} {text}\r\n").into_bytes()
                }
            }
            Reply::Multi { code, lines } => {
                let mut out = Vec::with_capacity(lines.len() * 24);
                let last = lines.len() - 1;
                for (index, line) in lines.iter().enumerate() {
                    let separator = if index == last { ' ' } else { '-' };
                    out.extend_from_slice(format!("{code}{separator}{line}\r\n").as_bytes());
                }
                out
            }
        }
    }

    /// The reply as a `String`, for logs.
    pub fn to_wire_string(&self) -> String {
        String::from_utf8_lossy(&self.render()).into_owned()
    }

    // -----------------------------------------------------------------
    // The catalogue. Grouped by the phase of a session it belongs to.
    // -----------------------------------------------------------------

    // --- greeting and connection -------------------------------------

    /// `220 <hostname> <banner>` — the service-ready greeting.
    pub fn service_ready(hostname: &str, banner: &str) -> Self {
        let text = if banner.is_empty() {
            format!("{hostname} ESMTP ready")
        } else {
            format!("{hostname} {banner}")
        };
        Reply::line(220, text)
    }

    /// `221 2.0.0 Bye` — the answer to `QUIT`.
    pub fn closing() -> Self {
        Reply::line(221, "2.0.0 Bye")
    }

    /// `221 4.4.2 <hostname> Service closing transmission channel` — a shutdown or
    /// a timeout that ends the session.
    pub fn closing_with(reason: &str) -> Self {
        Reply::line(221, format!("4.4.2 {reason}"))
    }

    /// `421 4.3.2 Too many connections, try again later` — overload.
    pub fn too_many_connections() -> Self {
        Reply::line(421, "4.3.2 Too many connections, try again later")
    }

    /// `421 4.7.0 <reason>` — a rate limit the peer should back off from.
    pub fn rate_limited(reason: &str) -> Self {
        Reply::line(421, format!("4.7.0 {reason}"))
    }

    /// `421 4.4.2 Timeout` — the peer went quiet.
    pub fn timeout() -> Self {
        Reply::line(421, "4.4.2 Idle timeout, closing connection")
    }

    /// `421 4.3.2 Service disabled` — an administrator stopped this listener.
    pub fn service_disabled() -> Self {
        Reply::line(421, "4.3.2 Service disabled by administrator")
    }

    // --- greetings ----------------------------------------------------

    /// The `EHLO` extension block.
    pub fn ehlo(hostname: &str, extensions: &[String]) -> Self {
        let mut lines = Vec::with_capacity(extensions.len() + 1);
        lines.push(format!("{hostname} greets you"));
        lines.extend(extensions.iter().cloned());
        Reply::multi(250, lines)
    }

    /// The bare `250` a client gets for `HELO`.
    pub fn helo(hostname: &str) -> Self {
        Reply::line(250, hostname)
    }

    /// `502 5.5.1 <verb> not implemented` — a recognised but unsupported verb.
    pub fn not_implemented(what: &str) -> Self {
        Reply::line(502, format!("5.5.1 {what} not implemented"))
    }

    /// `504 5.5.4 <param> not implemented` — an unsupported ESMTP parameter.
    pub fn param_not_implemented(param: &str) -> Self {
        Reply::line(504, format!("5.5.4 {param} not implemented"))
    }

    /// `500 5.5.2 Command unrecognized: <verb>`.
    pub fn unknown_command(verb: &str) -> Self {
        let shown: String = verb
            .chars()
            .filter(|c| !c.is_control())
            .take(32)
            .collect();
        Reply::line(500, format!("5.5.2 Command unrecognized: {shown}"))
    }

    /// `501 5.5.4 <reason>` — the command was understood but malformed.
    pub fn syntax_error(reason: &str) -> Self {
        Reply::line(501, format!("5.5.4 {reason}"))
    }

    /// `500 5.5.6 Line too long` — the per-command length cap.
    pub fn line_too_long() -> Self {
        Reply::line(500, "5.5.6 Line too long")
    }

    /// `503 5.5.1 Bad sequence of commands` — a state-machine violation.
    pub fn bad_sequence() -> Self {
        Reply::line(503, "5.5.1 Bad sequence of commands")
    }

    /// `503 5.5.1 Bad sequence of commands: <reason>`.
    pub fn bad_sequence_because(reason: &str) -> Self {
        Reply::line(503, format!("5.5.1 Bad sequence of commands: {reason}"))
    }

    /// `214 2.0.0 <text>` — the answer to `HELP`.
    pub fn help(text: &str) -> Self {
        Reply::line(214, format!("2.0.0 {text}"))
    }

    /// `252 2.1.5 Cannot VRFY user, but will accept message` — the RFC 5321 §3.5.3
    /// answer that neither confirms nor denies an address exists.
    pub fn cannot_vrfy() -> Self {
        Reply::line(252, "2.1.5 Cannot VRFY user, but will accept message")
    }

    /// `250 2.1.0 Ok` — the generic success used by `MAIL`, `RCPT`, `RSET`, `NOOP`.
    pub fn ok() -> Self {
        Reply::line(250, "2.1.0 Ok")
    }

    /// `250 2.0.0 Ok: queued as <id>` — the answer to `DATA`.
    pub fn accepted(id: &str) -> Self {
        Reply::line(250, format!("2.0.0 Ok: queued as {id}"))
    }

    /// `354 End data with <CR><LF>.<CR><LF>` — the `DATA` go-ahead.
    pub fn start_mail_input() -> Self {
        Reply::line(354, "End data with <CR><LF>.<CR><LF>")
    }

    // --- transaction --------------------------------------------------

    /// `550 5.1.1 <address>: Recipient address rejected: User unknown`.
    pub fn user_unknown(address: &str) -> Self {
        Reply::line(
            550,
            format!("5.1.1 <{address}>: Recipient address rejected: User unknown"),
        )
    }

    /// `550 5.1.1 <reason>` — the generic "no such mailbox" reply.
    pub fn mailbox_unavailable(reason: &str) -> Self {
        Reply::line(550, format!("5.1.1 {reason}"))
    }

    /// `501 5.1.7 <reason>` — an unparseable sender address.
    pub fn bad_sender_address(reason: &str) -> Self {
        Reply::line(501, format!("5.1.7 Bad sender address syntax: {reason}"))
    }

    /// `501 5.1.3 <reason>` — an unparseable recipient address.
    pub fn bad_recipient_address(reason: &str) -> Self {
        Reply::line(501, format!("5.1.3 Bad recipient address syntax: {reason}"))
    }

    /// `452 4.5.3 Too many recipients: limit is <n>`.
    pub fn too_many_recipients(limit: usize) -> Self {
        Reply::line(452, format!("4.5.3 Too many recipients: limit is {limit}"))
    }

    /// `452 4.2.2 Mailbox full` — a quota the peer can retry against later.
    pub fn mailbox_full() -> Self {
        Reply::line(452, "4.2.2 Mailbox full: over quota")
    }

    /// `452 4.2.2 <reason>` — a quota or storage failure worth retrying.
    pub fn mailbox_unavailable_temporary(reason: &str) -> Self {
        Reply::line(452, format!("4.2.2 {reason}"))
    }

    /// `550 5.7.1 Relaying denied` — the open-relay refusal.
    pub fn relay_denied(reason: &str) -> Self {
        Reply::line(550, format!("5.7.1 Relaying denied: {reason}"))
    }

    /// `552 5.3.4 Message size exceeds fixed maximum message size`.
    pub fn size_exceeded(limit: u64) -> Self {
        Reply::line(
            552,
            format!("5.3.4 Message size exceeds fixed maximum message size of {limit} bytes"),
        )
    }

    /// `451 4.3.0 <reason>` — a transient internal failure during `DATA`.
    pub fn temporary_failure(reason: &str) -> Self {
        Reply::line(451, format!("4.3.0 {reason}"))
    }

    /// `554 5.3.0 <reason>` — the message was refused outright.
    pub fn transaction_failed(reason: &str) -> Self {
        Reply::line(554, format!("5.3.0 {reason}"))
    }

    /// `554 5.6.0 <reason>` — the content itself is unacceptable.
    pub fn content_rejected(reason: &str) -> Self {
        Reply::line(554, format!("5.6.0 {reason}"))
    }

    /// `550 5.7.1 <reason>` — a delivery policy refused the message.
    ///
    /// RFC 3463 registers `5.7.1` as "delivery not authorized, message refused", which
    /// is exactly what a DMARC `p=reject` failure is. Distinct from
    /// [`Reply::relay_denied`], which is the open-relay rule.
    ///
    /// Sent *before* the message is stored — the inbound policy is evaluated at the end
    /// of `DATA` and before delivery — so this reply can never contradict a `250` the
    /// peer has already seen.
    pub fn policy_rejected(reason: &str) -> Self {
        Reply::line(550, format!("5.7.1 {reason}"))
    }

    /// `550 5.7.1 Message rejected by the DMARC policy of <domain>`.
    pub fn dmarc_rejected(domain: &str) -> Self {
        Reply::policy_rejected(&format!("Message rejected by the DMARC policy of {domain}"))
    }

    // --- STARTTLS -----------------------------------------------------

    /// `220 2.0.0 Ready to start TLS`.
    pub fn ready_to_start_tls() -> Self {
        Reply::line(220, "2.0.0 Ready to start TLS")
    }

    /// `454 4.7.0 TLS not available due to temporary reason`.
    pub fn tls_unavailable() -> Self {
        Reply::line(454, "4.7.0 TLS not available due to temporary reason")
    }

    /// `554 5.7.0 <reason>` — TLS was required and is missing or failed.
    pub fn tls_required(reason: &str) -> Self {
        Reply::line(554, format!("5.7.0 {reason}"))
    }

    /// `523 5.7.10 Encryption required for requested authentication mechanism`.
    pub fn encryption_required() -> Self {
        Reply::line(523, "5.7.10 Encryption required for requested authentication mechanism")
    }

    /// `503 5.5.1 TLS already active`.
    pub fn already_tls() -> Self {
        Reply::line(503, "5.5.1 TLS is already active")
    }

    // --- AUTH ---------------------------------------------------------

    /// `334 <challenge>` — an intermediate SASL step.
    pub fn auth_challenge(challenge: &str) -> Self {
        Reply::line(334, challenge)
    }

    /// `235 2.7.0 Authentication successful`.
    pub fn auth_successful() -> Self {
        Reply::line(235, "2.7.0 Authentication successful")
    }

    /// `535 5.7.8 Authentication credentials invalid`.
    pub fn auth_failed() -> Self {
        Reply::line(535, "5.7.8 Authentication credentials invalid")
    }

    /// `504 5.5.4 Unrecognized authentication type`.
    pub fn auth_mechanism_unsupported() -> Self {
        Reply::line(504, "5.5.4 Unrecognized authentication type")
    }

    /// `501 5.5.4 <reason>` — a malformed SASL exchange.
    pub fn auth_bad_exchange(reason: &str) -> Self {
        Reply::line(501, format!("5.5.4 {reason}"))
    }

    /// `503 5.5.1 Already authenticated`.
    pub fn already_authenticated() -> Self {
        Reply::line(503, "5.5.1 Already authenticated")
    }

    /// `538 5.7.11 Encryption required for requested authentication mechanism`.
    pub fn auth_requires_tls() -> Self {
        Reply::line(538, "5.7.11 Encryption required for requested authentication mechanism")
    }

    /// `454 4.7.0 Temporary authentication failure`.
    pub fn auth_temporary_failure() -> Self {
        Reply::line(454, "4.7.0 Temporary authentication failure")
    }

    /// `530 5.7.0 Authentication required`.
    pub fn auth_required() -> Self {
        Reply::line(530, "5.7.0 Authentication required")
    }

    /// `530 5.7.0 Authentication required` with a reason (`Submission requires…`).
    pub fn auth_required_because(reason: &str) -> Self {
        Reply::line(530, format!("5.7.0 Authentication required: {reason}"))
    }

    /// `421 4.7.0 <reason>` — a limit that ends the session.
    pub fn submission_rate_limited() -> Self {
        Reply::line(421, "4.7.0 Too many messages from this account, try again later")
    }

    /// `452 4.2.2 Daily send limit reached`.
    pub fn daily_send_limit_reached(limit: u32) -> Self {
        Reply::line(
            452,
            format!("4.2.2 Daily send limit of {limit} messages reached"),
        )
    }

    /// `451 4.7.1 <reason>` — the greylisting-style "come back later" reply.
    pub fn try_again_later(reason: &str) -> Self {
        Reply::line(451, format!("4.7.1 {reason}"))
    }

    /// Map a [`FerromaError`] onto the reply a peer should see.
    ///
    /// # The one invariant
    ///
    /// `from_error(err).is_transient_negative() == err.is_temporary()`. The 4xx/5xx
    /// decision comes from [`FerromaError::is_temporary`] — the single source of
    /// truth the queue also uses — so an error the queue would retry is an error the
    /// peer is told to retry, and vice versa. Only two cases need their own arm
    /// because they need a *different wording* while keeping the same class:
    ///
    /// * [`FerromaError::MailboxFull`] is **temporary** (`452 4.2.2`): RFC 3463
    ///   classifies a full mailbox as retryable, because the owner can free space.
    /// * [`FerromaError::LimitExceeded`] is **permanent** (`552 5.3.4`): "this
    ///   message is too big" or "too many recipients" will never become true by
    ///   waiting.
    ///
    /// A configuration error is deliberately **permanent** here: it is our bug, and
    /// telling a peer to retry a broken deployment just multiplies the noise.
    pub fn from_error(err: &FerromaError) -> Self {
        match err {
            FerromaError::MailboxFull(_) => Reply::mailbox_full(),
            FerromaError::LimitExceeded(msg) => Reply::line(552, format!("5.3.4 {msg}")),
            FerromaError::RateLimited => Reply::rate_limited("Too many requests, try again later"),
            FerromaError::Forbidden(msg) => Reply::line(554, format!("5.7.1 {msg}")),
            FerromaError::Unauthorized(msg) => Reply::line(535, format!("5.7.8 {msg}")),
            FerromaError::Unsupported(msg) => Reply::not_implemented(msg),
            other if other.is_temporary() => Reply::temporary_failure(&brief(other)),
            other => Reply::transaction_failed(&brief(other)),
        }
    }

    /// The reply used when delivery itself fails inside `DATA`.
    ///
    /// Distinct from [`Reply::from_error`] because the recipient set is known by
    /// then: a per-recipient failure gets the per-recipient wording.
    pub fn from_delivery_error(err: &FerromaError) -> Self {
        match err {
            FerromaError::MailboxFull(_) => Reply::mailbox_full(),
            FerromaError::LimitExceeded(msg) => Reply::line(552, format!("5.3.4 {msg}")),
            FerromaError::NotFound(msg) => Reply::mailbox_unavailable(msg),
            other if other.is_temporary() => {
                Reply::mailbox_unavailable_temporary(&format!("temporary failure: {}", brief(other)))
            }
            other => Reply::mailbox_unavailable(&brief(other)),
        }
    }
}

/// One-line, CR/LF-free rendering of an error, for a reply text.
fn brief(err: &FerromaError) -> String {
    let text = err.to_string();
    let cleaned = sanitise(&text);
    if cleaned.len() > 200 {
        // Keep it single-line and bounded; `char_indices` avoids splitting a
        // multi-byte character in half.
        let mut cut = 0;
        for (idx, _) in cleaned.char_indices() {
            if idx > 200 {
                break;
            }
            cut = idx;
        }
        format!("{}…", &cleaned[..cut])
    } else {
        cleaned
    }
}

/// Strip anything that could break the line-oriented protocol or inject a reply.
fn sanitise(text: &str) -> String {
    text.chars().filter(|c| *c != '\r' && *c != '\n' && *c != '\0').collect()
}

impl fmt::Display for Reply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for line in self.lines() {
            writeln!(f, "{} {}", self.code(), line)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn a_single_line_renders_with_crlf() {
        assert_eq!(Reply::ok().render(), b"250 2.1.0 Ok\r\n");
        assert_eq!(Reply::closing().render(), b"221 2.0.0 Bye\r\n");
        assert_eq!(
            Reply::start_mail_input().render(),
            b"354 End data with <CR><LF>.<CR><LF>\r\n"
        );
    }

    #[test]
    fn the_banner_carries_the_hostname_and_the_configured_text() {
        assert_eq!(
            Reply::service_ready("mx.example.com", "Ferroma ESMTP ready").render(),
            b"220 mx.example.com Ferroma ESMTP ready\r\n"
        );
        assert_eq!(
            Reply::service_ready("mx.example.com", "").render(),
            b"220 mx.example.com ESMTP ready\r\n"
        );
    }

    #[test]
    fn a_multi_line_reply_uses_dashes_until_the_last_line() {
        let reply = Reply::multi(
            250,
            vec!["mx.example.com greets you".into(), "SIZE 100".into(), "8BITMIME".into()],
        );
        assert_eq!(
            reply.render(),
            b"250-mx.example.com greets you\r\n250-SIZE 100\r\n250 8BITMIME\r\n"
        );
    }

    #[test]
    fn a_single_line_multi_reply_still_closes_correctly() {
        let reply = Reply::multi(250, vec!["only".into()]);
        assert_eq!(reply.render(), b"250 only\r\n");
    }

    #[test]
    fn an_empty_multi_reply_degrades_to_one_line() {
        let reply = Reply::multi(250, Vec::new());
        assert_eq!(reply.render(), b"250 \r\n");
    }

    #[test]
    fn ehlo_lists_the_extension_block_after_the_greeting() {
        let reply = Reply::ehlo(
            "mx.example.com",
            &["SIZE 26214400".to_string(), "PIPELINING".to_string()],
        );
        assert_eq!(
            reply.render(),
            b"250-mx.example.com greets you\r\n250-SIZE 26214400\r\n250 PIPELINING\r\n"
        );
        assert_eq!(reply.code(), 250);
    }

    #[test]
    fn helo_is_a_bare_250_with_just_the_hostname() {
        assert_eq!(Reply::helo("mx.example.com").render(), b"250 mx.example.com\r\n");
    }

    #[test]
    fn crlf_in_a_reply_text_cannot_inject_a_line() {
        let reply = Reply::line(550, "evil\r\n250 Ok");
        assert_eq!(reply.render(), b"550 evil250 Ok\r\n");
        assert!(!reply.text().contains('\r'));
        assert!(!reply.text().contains('\n'));
    }

    #[test]
    fn nul_in_a_reply_text_is_removed() {
        assert_eq!(Reply::line(500, "a\0b").text(), "ab");
    }

    #[test]
    fn every_catalogue_entry_renders_to_crlf_terminated_ascii() {
        let replies = vec![
            Reply::service_ready("mx", "ready"),
            Reply::closing(),
            Reply::closing_with("shutting down"),
            Reply::too_many_connections(),
            Reply::rate_limited("slow down"),
            Reply::timeout(),
            Reply::ehlo("mx", &["SIZE 1".to_string()]),
            Reply::helo("mx"),
            Reply::not_implemented("BDAT"),
            Reply::param_not_implemented("BODY=BINARYMIME"),
            Reply::unknown_command("XYZZY"),
            Reply::syntax_error("bad"),
            Reply::line_too_long(),
            Reply::bad_sequence(),
            Reply::bad_sequence_because("no EHLO"),
            Reply::help("Commands: EHLO HELO"),
            Reply::cannot_vrfy(),
            Reply::ok(),
            Reply::accepted("1A2B3C"),
            Reply::start_mail_input(),
            Reply::user_unknown("bob@example.com"),
            Reply::mailbox_unavailable("no such mailbox"),
            Reply::bad_sender_address("no domain"),
            Reply::bad_recipient_address("no domain"),
            Reply::too_many_recipients(100),
            Reply::mailbox_full(),
            Reply::mailbox_unavailable_temporary("quota check failed"),
            Reply::relay_denied("not a local domain"),
            Reply::size_exceeded(1000),
            Reply::temporary_failure("storage busy"),
            Reply::transaction_failed("refused"),
            Reply::content_rejected("spam"),
            Reply::policy_rejected("policy"),
            Reply::dmarc_rejected("example.com"),
            Reply::ready_to_start_tls(),
            Reply::tls_unavailable(),
            Reply::tls_required("TLS is required"),
            Reply::encryption_required(),
            Reply::already_tls(),
            Reply::auth_challenge("VXNlcm5hbWU6"),
            Reply::auth_successful(),
            Reply::auth_failed(),
            Reply::auth_mechanism_unsupported(),
            Reply::auth_bad_exchange("bad base64"),
            Reply::already_authenticated(),
            Reply::auth_requires_tls(),
            Reply::auth_temporary_failure(),
            Reply::auth_required(),
            Reply::auth_required_because("submission port"),
            Reply::submission_rate_limited(),
            Reply::daily_send_limit_reached(500),
            Reply::try_again_later("greylisted"),
        ];
        for reply in replies {
            let rendered = reply.render();
            assert!(rendered.ends_with(b"\r\n"), "{reply:?} does not end in CRLF");
            assert!(
                rendered.is_ascii(),
                "{reply:?} rendered non-ASCII: {:?}",
                String::from_utf8_lossy(&rendered)
            );
            assert_eq!(
                rendered.windows(2).filter(|w| w == b"\r\n").count(),
                reply.lines().len(),
                "{reply:?} has a stray CRLF"
            );
            for line in reply.lines() {
                assert!(!line.contains('\r') && !line.contains('\n'), "{reply:?}");
            }
        }
    }

    #[test]
    fn codes_are_classified() {
        assert!(Reply::ok().is_positive());
        assert!(Reply::start_mail_input().is_intermediate());
        assert!(Reply::too_many_connections().is_transient_negative());
        assert!(Reply::user_unknown("x").is_permanent_negative());

        assert!(!Reply::ok().is_transient_negative());
        assert!(!Reply::user_unknown("x").is_positive());
    }

    #[test]
    fn the_specific_replies_use_the_documented_codes() {
        assert_eq!(Reply::size_exceeded(1).code(), 552);
        assert_eq!(Reply::rate_limited("x").code(), 421);
        assert_eq!(Reply::too_many_connections().code(), 421);
        assert_eq!(Reply::relay_denied("x").code(), 550);
        assert_eq!(Reply::auth_required().code(), 530);
        assert_eq!(Reply::auth_failed().code(), 535);
        assert_eq!(Reply::auth_successful().code(), 235);
        assert_eq!(Reply::auth_challenge("x").code(), 334);
        assert_eq!(Reply::bad_sequence().code(), 503);
        assert_eq!(Reply::unknown_command("x").code(), 500);
        assert_eq!(Reply::mailbox_unavailable("x").code(), 550);
        assert_eq!(Reply::user_unknown("x").code(), 550);
        assert_eq!(Reply::too_many_recipients(1).code(), 452);
        assert_eq!(Reply::mailbox_full().code(), 452);
        assert_eq!(Reply::cannot_vrfy().code(), 252);
        assert_eq!(Reply::not_implemented("BDAT").code(), 502);
    }

    #[test]
    fn enhanced_status_codes_appear_where_rfc_3463_defines_them() {
        assert_eq!(Reply::ok().render(), b"250 2.1.0 Ok\r\n");
        assert!(Reply::user_unknown("x").text().starts_with("5.1.1"));
        assert!(Reply::relay_denied("x").text().starts_with("5.7.1"));
        assert!(Reply::size_exceeded(1).text().starts_with("5.3.4"));
        assert!(Reply::too_many_recipients(1).text().starts_with("4.5.3"));
        assert!(Reply::mailbox_full().text().starts_with("4.2.2"));
        assert!(Reply::auth_successful().text().starts_with("2.7.0"));
        assert!(Reply::auth_failed().text().starts_with("5.7.8"));
        assert!(Reply::bad_sequence().text().starts_with("5.5.1"));
        assert!(Reply::line_too_long().text().starts_with("5.5.6"));
    }

    // ------------------------------------------------------------------
    // FerromaError mapping
    // ------------------------------------------------------------------

    #[test]
    fn a_temporary_error_maps_to_a_4xx() {
        let reply = Reply::from_error(&FerromaError::Network("connection reset".into()));
        assert_eq!(reply.code(), 451);
        assert!(reply.is_transient_negative());
        assert!(reply.text().contains("connection reset"));
    }

    #[test]
    fn a_permanent_error_maps_to_a_5xx() {
        let reply = Reply::from_error(&FerromaError::Invalid("bad address".into()));
        assert_eq!(reply.code(), 554);
        assert!(reply.is_permanent_negative());
    }

    #[test]
    fn rate_limiting_maps_to_421() {
        assert_eq!(Reply::from_error(&FerromaError::RateLimited).code(), 421);
    }

    #[test]
    fn a_limit_error_maps_to_552() {
        let reply = Reply::from_error(&FerromaError::LimitExceeded("too big".into()));
        assert_eq!(reply.code(), 552);
        assert!(reply.text().contains("too big"));
        assert!(
            reply.is_permanent_negative(),
            "an oversized message will never become acceptable by waiting"
        );
        assert!(!reply.is_transient_negative());
    }

    /// A full mailbox is **temporary** (RFC 3463 `4.2.2`) and an exceeded limit is
    /// **permanent** (`5.3.4`). Getting that pair backwards silently loses mail: a
    /// `5xx` on a full mailbox makes the sending MTA bounce it for good.
    #[test]
    fn a_full_mailbox_is_temporary_and_a_limit_breach_is_permanent() {
        let full = Reply::from_error(&FerromaError::MailboxFull("bob@example.org".into()));
        assert_eq!(full.render(), b"452 4.2.2 Mailbox full: over quota\r\n");
        assert!(full.is_transient_negative());
        assert!(FerromaError::MailboxFull("x".into()).is_temporary());

        let limit = Reply::from_error(&FerromaError::LimitExceeded("too many recipients".into()));
        assert_eq!(limit.code(), 552);
        assert!(limit.is_permanent_negative());
        assert!(!FerromaError::LimitExceeded("x".into()).is_temporary());

        // And the same pair holds on the delivery path, which is where quota meets
        // SMTP.
        let full = Reply::from_delivery_error(&FerromaError::MailboxFull("x".into()));
        assert_eq!(full.code(), 452);
        assert!(full.text().starts_with("4.2.2"));
        let limit = Reply::from_delivery_error(&FerromaError::LimitExceeded("x".into()));
        assert_eq!(limit.code(), 552);
    }

    #[test]
    fn a_forbidden_error_maps_to_a_relay_refusal() {
        let reply = Reply::from_error(&FerromaError::Forbidden("not local".into()));
        assert_eq!(reply.code(), 554);
        assert!(reply.text().starts_with("5.7.1"));
    }

    #[test]
    fn an_unauthorized_error_maps_to_535() {
        assert_eq!(Reply::from_error(&FerromaError::Unauthorized("nope".into())).code(), 535);
    }

    #[test]
    fn an_unsupported_error_maps_to_502() {
        let reply = Reply::from_error(&FerromaError::Unsupported("BDAT".into()));
        assert_eq!(reply.code(), 502);
        assert!(reply.text().contains("BDAT"));
    }

    #[test]
    fn the_temporary_classification_drives_the_code_for_every_variant() {
        let cases: Vec<(FerromaError, bool)> = vec![
            (FerromaError::Io(std::io::Error::other("disk")), true),
            (FerromaError::Network("x".into()), true),
            (FerromaError::Dns("x".into()), true),
            (FerromaError::RateLimited, true),
            (FerromaError::MailboxFull("x".into()), true),
            (FerromaError::Timeout("x".into()), true),
            (FerromaError::Internal("x".into()), true),
            (FerromaError::storage(std::io::Error::other("pool")), true),
            (FerromaError::Parse("x".into()), false),
            (FerromaError::NotFound("x".into()), false),
            (FerromaError::Conflict("x".into()), false),
            (FerromaError::Invalid("x".into()), false),
            (FerromaError::Tls("x".into()), false),
            (FerromaError::Protocol("x".into()), false),
            (FerromaError::Config("x".into()), false),
        ];
        for (err, temporary) in cases {
            let reply = Reply::from_error(&err);
            assert_eq!(
                reply.is_transient_negative(),
                temporary,
                "{err:?} mapped to {}",
                reply.code()
            );
            assert_eq!(err.is_temporary(), temporary);
        }
    }

    #[test]
    fn a_storage_error_is_temporary() {
        let err = FerromaError::storage(std::io::Error::other("pool exhausted"));
        assert_eq!(Reply::from_error(&err).code(), 451);
    }

    #[test]
    fn a_policy_rejection_is_550_with_the_delivery_authorization_enhanced_code() {
        let reply = Reply::dmarc_rejected("example.com");
        assert_eq!(
            reply.render(),
            b"550 5.7.1 Message rejected by the DMARC policy of example.com\r\n"
        );
        assert!(reply.is_permanent_negative());
        // Distinct from the open-relay refusal, which says "Relaying denied".
        assert!(!reply.text().contains("Relaying"));
    }

    #[test]
    fn a_full_mailbox_maps_to_452_and_not_a_policy_5xx() {
        assert_eq!(Reply::mailbox_full().code(), 452);
        assert_ne!(Reply::policy_rejected("x").code(), 452);
    }

    #[test]
    fn delivery_errors_use_recipient_wording() {
        assert_eq!(
            Reply::from_delivery_error(&FerromaError::NotFound("mailbox 7".into())).code(),
            550
        );
        assert_eq!(Reply::from_delivery_error(&FerromaError::RateLimited).code(), 452);
        assert_eq!(
            Reply::from_delivery_error(&FerromaError::LimitExceeded("huge".into())).code(),
            552
        );
        assert_eq!(
            Reply::from_delivery_error(&FerromaError::MailboxFull("full".into())).render(),
            b"452 4.2.2 Mailbox full: over quota\r\n"
        );
    }

    #[test]
    fn an_error_message_with_crlf_is_sanitised_into_the_reply() {
        let err = FerromaError::Network("reset\r\n250 Ok\r\n".into());
        let reply = Reply::from_error(&err);
        assert_eq!(reply.render().windows(2).filter(|w| w == b"\r\n").count(), 1);
    }

    #[test]
    fn a_very_long_error_message_is_truncated() {
        let err = FerromaError::Invalid("x".repeat(5000));
        let reply = Reply::from_error(&err);
        assert!(reply.text().len() <= 220, "{} bytes", reply.text().len());
        assert!(reply.text().ends_with('…'));
    }

    #[test]
    fn truncation_never_splits_a_utf8_character() {
        let err = FerromaError::Invalid("é".repeat(500));
        let reply = Reply::from_error(&err);
        // The render is ASCII-safe because `sanitise` keeps the é; what matters is
        // that the slice did not panic and the string is still valid UTF-8.
        assert!(reply.text().chars().count() > 0);
        let _ = reply.render();
    }

    #[test]
    fn display_renders_one_line_per_reply_line() {
        let reply = Reply::multi(250, vec!["a".into(), "b".into()]);
        assert_eq!(reply.to_string(), "250 a\n250 b\n");
    }

    #[test]
    fn to_wire_string_is_lossless_for_ascii_replies() {
        assert_eq!(Reply::ok().to_wire_string(), "250 2.1.0 Ok\r\n");
    }

    #[test]
    fn unknown_command_text_is_bounded_and_control_free() {
        let reply = Reply::unknown_command(&format!("{}\u{7}", "V".repeat(100)));
        assert!(reply.text().len() < 80, "{}", reply.text());
        assert!(!reply.text().contains('\u{7}'));
    }
}

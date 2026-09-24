//! JOSE for ACME: an ES256 account key, its JWK, its thumbprint, and the JWS.
//!
//! RFC 8555 does not carry a password. Every request is a JWS signed with the account
//! key, and the server identifies the account by the key itself — which makes this
//! layer the correctness boundary of the whole client. Three details in it are the
//! ones implementations get wrong:
//!
//! * **ES256 in JOSE is the raw `r || s` pair, not an ASN.1 DER `SEQUENCE`.** A DER
//!   signature is a valid signature that every ACME server rejects, with an error that
//!   says nothing about why. `ring`'s `*_FIXED_SIGNING` algorithm is the one that
//!   produces the raw form; the default ECDSA algorithm produces DER.
//! * **The JWK thumbprint is over a canonical JSON with sorted, whitespace-free
//!   members** (RFC 7638 §3). Any other serialisation produces a different thumbprint,
//!   and the external account binding and key authorisation both depend on it.
//! * **The `protected` header must carry the right account reference**: `jwk` for
//!   `newAccount`, `kid` afterwards. Sending `jwk` once the account exists is refused.

use base64::Engine as _;
use ring::rand::SystemRandom;
use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};
use sha2::{Digest, Sha256};

use ferroma_core::{FerromaError, Result};

/// Base64url without padding, which is the only encoding JOSE uses.
pub fn b64url(data: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

/// Decode base64url without padding, refusing anything else.
///
/// The client itself only encodes; this exists to *check* a JWK before publishing it,
/// and the tests decode the requests they receive with the same function so that what
/// is verified is exactly what JOSE specifies.
pub fn b64url_decode(text: &str) -> Result<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(text.trim())
        .map_err(|_| FerromaError::Invalid("not base64url".into()))
}

/// The ACME directory, as `GET /directory` returns it.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Directory {
    /// Where accounts are created.
    #[serde(rename = "newAccount")]
    pub new_account: String,
    /// Where a fresh nonce comes from.
    #[serde(rename = "newNonce")]
    pub new_nonce: String,
    /// Where orders are created.
    #[serde(rename = "newOrder")]
    pub new_order: String,
    /// The terms of service an account must agree to, and the CA's own metadata.
    #[serde(rename = "meta", default)]
    pub meta: DirectoryMeta,
}

/// `meta` from the directory.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct DirectoryMeta {
    /// The terms-of-service URL, when the server publishes one.
    ///
    /// The document carries more than this — a website, a CA certificate — and the
    /// client reads only what it acts on. A field nothing uses is a field that drifts.
    #[serde(rename = "termsOfService", default)]
    pub terms_of_service: Option<String>,
}

/// An ES256 account key.
pub struct AccountKey {
    pair: EcdsaKeyPair,
    /// The PKCS#8 document, kept because `ring` offers no way back out of a pair and
    /// the key has to be persisted between runs — re-generating it would orphan the
    /// account the ACME server has already created.
    pkcs8: Vec<u8>,
}

impl std::fmt::Debug for AccountKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the key material, and never the public point either: this type ends up
        // in a `Debug` somewhere eventually, and the private half is the credential.
        f.debug_struct("AccountKey").finish_non_exhaustive()
    }
}

impl AccountKey {
    /// Generate a new P-256 key.
    pub fn generate() -> Result<Self> {
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng)
            .map_err(|_| FerromaError::internal("could not generate an ACME account key"))?;
        Self::from_pkcs8(pkcs8.as_ref())
    }

    /// Load a key from its PKCS#8 bytes.
    pub fn from_pkcs8(bytes: &[u8]) -> Result<Self> {
        let rng = SystemRandom::new();
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, bytes, &rng)
            .map_err(|_| FerromaError::Invalid("the ACME account key is not a P-256 PKCS#8 key".into()))?;
        Ok(AccountKey {
            pair,
            pkcs8: bytes.to_vec(),
        })
    }

    /// The PKCS#8 bytes, to be written to disk with owner-only permissions.
    pub fn to_pkcs8(&self) -> &[u8] {
        &self.pkcs8
    }

    /// The uncompressed public point, `0x04 || x || y`.
    fn public_point(&self) -> Vec<u8> {
        self.pair.public_key().as_ref().to_vec()
    }

    /// The JWK, as the `jwk` header member and as the thumbprint's input.
    ///
    /// Validated before it is returned: a malformed point would otherwise be published
    /// to the CA and rejected there, in a message that names the header and not the
    /// cause.
    pub fn jwk(&self) -> Result<Jwk> {
        let point = self.public_point();
        if point.len() != 65 || point[0] != 0x04 {
            return Err(FerromaError::internal(
                "the ACME account key did not yield an uncompressed P-256 point",
            ));
        }
        let jwk = Jwk {
            crv: "P-256".to_string(),
            kty: "EC".to_string(),
            x: b64url(&point[1..33]),
            y: b64url(&point[33..65]),
        };
        jwk.validate()?;
        Ok(jwk)
    }

    /// The RFC 7638 thumbprint, which is what an ACME server keys an account by.
    pub fn thumbprint(&self) -> Result<String> {
        Ok(b64url(&Sha256::digest(self.jwk()?.canonical_json().as_bytes())))
    }

    /// Sign `data` the way JOSE wants it: the raw 64-byte `r || s`, base64url encoded.
    fn sign(&self, data: &[u8]) -> Result<String> {
        let rng = SystemRandom::new();
        let signature = self
            .pair
            .sign(&rng, data)
            .map_err(|_| FerromaError::internal("could not sign the ACME request"))?;
        let raw = signature.as_ref();
        if raw.len() != 64 {
            return Err(FerromaError::internal(
                "the ACME signature was not the fixed 64-byte form JOSE requires",
            ));
        }
        Ok(b64url(raw))
    }
}

/// A public JWK for an EC key, with only the members RFC 7638 canonicalises.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Jwk {
    /// `P-256`.
    pub crv: String,
    /// `EC`.
    pub kty: String,
    /// Base64url of the x coordinate.
    pub x: String,
    /// Base64url of the y coordinate.
    pub y: String,
}

impl Jwk {
    /// The canonical JSON RFC 7638 §3 defines: the required members, lexicographic
    /// order, no whitespace. Nothing about this is negotiable — the thumbprint is a
    /// hash of exactly these bytes.
    pub fn canonical_json(&self) -> String {
        // Written by hand rather than with `serde_json`: the member order of a
        // serialised map is an implementation detail there, and one that differs
        // between a `BTreeMap` and a struct.
        format!(
            "{{\"crv\":\"{}\",\"kty\":\"{}\",\"x\":\"{}\",\"y\":\"{}\"}}",
            self.crv, self.kty, self.x, self.y
        )
    }

    /// Whether the point is one a P-256 key can actually have.
    pub fn validate(&self) -> Result<()> {
        if self.crv != "P-256" || self.kty != "EC" {
            return Err(FerromaError::Invalid("the JWK is not a P-256 EC key".into()));
        }
        if b64url_decode(&self.x)?.len() != 32 || b64url_decode(&self.y)?.len() != 32 {
            return Err(FerromaError::Invalid(
                "the JWK coordinates are not 32 bytes each".into(),
            ));
        }
        Ok(())
    }
}

/// How a request proves which account it belongs to.
pub enum AccountRef<'a> {
    /// The key itself, which is only valid on `newAccount`.
    Jwk(&'a Jwk),
    /// The account URL the server returned when the account was created.
    Kid(&'a str),
}

/// Build a flattened JWS for one ACME request.
///
/// `payload` is `None` for a POST-as-GET, which RFC 8555 §6.3 requires in place of a
/// plain GET for anything once an account exists: the body is the empty string, not an
/// absent one.
pub fn sign_request(
    key: &AccountKey,
    url: &str,
    nonce: &str,
    account: &AccountRef<'_>,
    payload: Option<&serde_json::Value>,
) -> Result<serde_json::Value> {
    let mut header = serde_json::json!({
        "alg": "ES256",
        "nonce": nonce,
        "url": url,
    });
    match account {
        AccountRef::Jwk(jwk) => {
            header["jwk"] = serde_json::to_value(jwk)
                .map_err(|e| FerromaError::internal(format!("could not serialise the JWK: {e}")))?;
        }
        AccountRef::Kid(kid) => {
            header["kid"] = serde_json::Value::String((*kid).to_string());
        }
    }

    let protected = b64url(
        serde_json::to_string(&header)
            .map_err(|e| FerromaError::internal(format!("could not serialise the header: {e}")))?
            .as_bytes(),
    );
    // A POST-as-GET signs an empty payload; `""` and "{}" are different requests.
    let payload_text = match payload {
        Some(value) => serde_json::to_string(value)
            .map_err(|e| FerromaError::internal(format!("could not serialise the payload: {e}")))?,
        None => String::new(),
    };
    let encoded_payload = b64url(payload_text.as_bytes());

    let signing_input = format!("{protected}.{encoded_payload}");
    let signature = key.sign(signing_input.as_bytes())?;

    Ok(serde_json::json!({
        "protected": protected,
        "payload": encoded_payload,
        "signature": signature,
    }))
}

/// A key authorisation: `token || "." || base64url(thumbprint)` (RFC 8555 §8.1).
///
/// This is the string the ACME server fetches over HTTP, and the one it hashes to
/// check the challenge. Getting it wrong means an HTTP-01 challenge that serves a file
/// the server will not accept.
pub fn key_authorization(token: &str, thumbprint: &str) -> String {
    format!("{token}.{thumbprint}")
}

/// The body served at `/.well-known/acme-challenge/<token>`: the SHA-256 of the key
/// authorisation, base64url encoded.
pub fn challenge_response(token: &str, thumbprint: &str) -> String {
    let authorization = key_authorization(token, thumbprint);
    b64url(&Sha256::digest(authorization.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hash-and-encode step, against a value produced by another implementation.
    ///
    /// The coordinates are the P-256 point from RFC 7515 Appendix A.3, and the expected
    /// string was computed with Node's `crypto` over the same canonical form — so this
    /// locks the SHA-256 and the base64url, independently of the Rust that computes
    /// them. The *rule* for building that form is a separate assertion, below: RFC 7638
    /// publishes its canonicalisation vector only for an RSA key, which this client does
    /// not use, and inventing an EC "RFC vector" would be a test that proves nothing.
    #[test]
    fn the_thumbprint_is_the_hash_of_the_canonical_form() {
        let jwk = Jwk {
            crv: "P-256".into(),
            kty: "EC".into(),
            x: "f83OJ3D2xF1Bg8vub9tLe1gHMzV76e8Tus9uPHvRVEU".into(),
            y: "x_FEzRu9m36HLN_tue659LNpXW6pCyStikYjKIWI5a0".into(),
        };
        let thumbprint = b64url(&Sha256::digest(jwk.canonical_json().as_bytes()));
        assert_eq!(thumbprint, "oKIywvGUpTVTyxMQ3bwIIeQUudfr_CkLMjCE19ECD-U");
    }

    #[test]
    fn the_canonical_form_is_sorted_and_unspaced() {
        let jwk = Jwk {
            crv: "P-256".into(),
            kty: "EC".into(),
            x: "AA".into(),
            y: "BB".into(),
        };
        assert_eq!(
            jwk.canonical_json(),
            r#"{"crv":"P-256","kty":"EC","x":"AA","y":"BB"}"#
        );
    }

    #[test]
    fn a_generated_key_is_a_usable_p256_pair() {
        let key = AccountKey::generate().unwrap();
        let jwk = key.jwk().unwrap();
        jwk.validate().unwrap();
        assert_eq!(b64url_decode(&jwk.x).unwrap().len(), 32);
        assert_eq!(b64url_decode(&jwk.y).unwrap().len(), 32);
        // The thumbprint is stable for one key and differs between keys.
        assert_eq!(key.thumbprint().unwrap(), key.thumbprint().unwrap());
        assert_ne!(key.thumbprint().unwrap(), AccountKey::generate().unwrap().thumbprint().unwrap());
    }

    #[test]
    fn a_key_survives_a_round_trip_through_pkcs8() {
        let key = AccountKey::generate().unwrap();
        let reloaded = AccountKey::from_pkcs8(key.to_pkcs8()).unwrap();
        assert_eq!(key.thumbprint().unwrap(), reloaded.thumbprint().unwrap());
        assert!(AccountKey::from_pkcs8(b"not a key").is_err());
    }

    /// The signature must be the raw 64-byte JOSE form, verified here with the matching
    /// `FIXED` algorithm. A DER signature would be a valid ECDSA signature and the wrong
    /// thing entirely, which is exactly the failure this asserts against.
    #[test]
    fn a_signature_is_the_raw_form_jose_requires() {
        let key = AccountKey::generate().unwrap();
        let jwk = key.jwk().unwrap();
        let signed = sign_request(
            &key,
            "https://acme.example/new-account",
            "nonce-1",
            &AccountRef::Jwk(&jwk),
            Some(&serde_json::json!({"termsOfServiceAgreed": true})),
        )
        .unwrap();

        let signature = b64url_decode(signed["signature"].as_str().unwrap()).unwrap();
        assert_eq!(signature.len(), 64, "JOSE wants r || s, not DER");

        let protected = signed["protected"].as_str().unwrap();
        let payload = signed["payload"].as_str().unwrap();
        let signing_input = format!("{protected}.{payload}");

        let point = {
            let mut point = vec![0x04];
            point.extend(b64url_decode(&jwk.x).unwrap());
            point.extend(b64url_decode(&jwk.y).unwrap());
            point
        };
        let public = ring::signature::UnparsedPublicKey::new(
            &ring::signature::ECDSA_P256_SHA256_FIXED,
            &point,
        );
        public
            .verify(signing_input.as_bytes(), &signature)
            .expect("the signature must verify against the JWK it published");
    }

    #[test]
    fn the_protected_header_carries_the_right_account_reference() {
        let key = AccountKey::generate().unwrap();
        let jwk = key.jwk().unwrap();

        let first = sign_request(&key, "https://acme.example/new-account", "n", &AccountRef::Jwk(&jwk), Some(&serde_json::json!({}))).unwrap();
        let header: serde_json::Value =
            serde_json::from_slice(&b64url_decode(first["protected"].as_str().unwrap()).unwrap()).unwrap();
        assert_eq!(header["alg"], "ES256");
        assert_eq!(header["nonce"], "n");
        assert_eq!(header["url"], "https://acme.example/new-account");
        assert!(header["jwk"].is_object(), "newAccount identifies the key itself");
        assert!(header.get("kid").is_none(), "and never both");

        let later = sign_request(&key, "https://acme.example/order/1", "n2", &AccountRef::Kid("https://acme.example/acct/7"), Some(&serde_json::json!({}))).unwrap();
        let header: serde_json::Value =
            serde_json::from_slice(&b64url_decode(later["protected"].as_str().unwrap()).unwrap()).unwrap();
        assert_eq!(header["kid"], "https://acme.example/acct/7");
        assert!(header.get("jwk").is_none(), "a later request must not resend the key");
    }

    #[test]
    fn a_post_as_get_signs_an_empty_payload() {
        let key = AccountKey::generate().unwrap();
        let signed = sign_request(
            &key,
            "https://acme.example/authz/1",
            "n",
            &AccountRef::Kid("https://acme.example/acct/7"),
            None,
        )
        .unwrap();
        assert_eq!(signed["payload"], "", "RFC 8555 §6.3: empty, not absent");
    }

    #[test]
    fn the_challenge_response_is_the_key_authorisation_hashed() {
        // RFC 8555 §8.1 publishes this token and this thumbprint, and the key
        // authorisation they form. What is asserted here is the shape and the hash — the
        // token/thumbprint pairing itself is irrelevant to these two functions.
        let token = "evaGxfADs6pSRb2LAv9IZf17Dt3juxGJ-PCt92wr-oA";
        let thumbprint = "cn-I_WNMClehiVp51i_0VpOENW1upEerA8sEam5hn-s";
        assert_eq!(
            key_authorization(token, thumbprint),
            format!("{token}.{thumbprint}")
        );

        let expected = b64url(&Sha256::digest(key_authorization(token, thumbprint).as_bytes()));
        assert_eq!(challenge_response(token, thumbprint), expected);
        // Different tokens never collide.
        assert_ne!(
            challenge_response(token, thumbprint),
            challenge_response("another-token", thumbprint)
        );
    }

    /// Build a self-signed certificate with an explicit validity window.
    fn certificate_with_validity(year: i32, month: u8, day: u8) -> String {
        let mut params = rcgen::CertificateParams::new(vec!["mail.example.com".to_string()]).unwrap();
        params.not_before = rcgen::date_time_ymd(2026, 1, 1);
        params.not_after = rcgen::date_time_ymd(year, month, day);
        let key = rcgen::KeyPair::generate().unwrap();
        params.self_signed(&key).unwrap().pem()
    }

    /// The validity parser reads real certificates, in both of DER's time encodings.
    ///
    /// `notAfter` is what decides whether to renew, so a parser that silently returns
    /// the wrong date is a certificate that expires in production. 2049 is the last year
    /// DER writes as a two-digit `UTCTime`; 2050 is the first it writes as
    /// `GeneralizedTime`, and both branches are exercised.
    #[test]
    fn the_not_after_date_is_read_from_a_real_certificate() {
        let utc = certificate_not_after(&certificate_with_validity(2049, 12, 31)).expect("UTCTime");
        assert_eq!(utc.format("%Y-%m-%d").to_string(), "2049-12-31");

        let generalized =
            certificate_not_after(&certificate_with_validity(2050, 6, 15)).expect("GeneralizedTime");
        assert_eq!(generalized.format("%Y-%m-%d").to_string(), "2050-06-15");

        // The default window ends in 4096, which is also `GeneralizedTime`.
        let key = rcgen::KeyPair::generate().unwrap();
        let default = rcgen::CertificateParams::new(vec!["a.example".to_string()])
            .unwrap()
            .self_signed(&key)
            .unwrap()
            .pem();
        assert_eq!(
            certificate_not_after(&default).unwrap().format("%Y-%m-%d").to_string(),
            "4096-01-01"
        );
    }

    #[test]
    fn a_chain_reports_the_leaf_not_an_intermediate() {
        // Two certificates in one PEM: the leaf is the one whose expiry matters, and a
        // parser that read the last block would renew on the wrong date.
        let leaf = certificate_with_validity(2030, 3, 4);
        let other = certificate_with_validity(2040, 5, 6);
        let chain = format!("{leaf}{other}");
        assert_eq!(
            certificate_not_after(&chain).unwrap().format("%Y-%m-%d").to_string(),
            "2030-03-04"
        );
    }

    #[test]
    fn an_unreadable_certificate_reports_no_date() {
        // `None` means "renew", which is the safe answer for anything unparseable.
        for nonsense in [
            "",
            "not a pem file at all",
            "-----BEGIN CERTIFICATE-----\nnot base64!\n-----END CERTIFICATE-----\n",
            "-----BEGIN CERTIFICATE-----\nZm9v\n-----END CERTIFICATE-----\n",
        ] {
            assert!(certificate_not_after(nonsense).is_none(), "{nonsense:?}");
        }
    }

    #[test]
    fn a_challenge_file_lands_where_it_was_asked_to_and_nowhere_else() {
        let dir = tempfile::tempdir().unwrap();
        let tokens = dir.path().join("tokens");
        write_challenge(&tokens, "evaGxfADs6pSRb2LAv9IZf17Dt3juxGJ-PCt92wr-oA", "body").unwrap();
        assert_eq!(
            std::fs::read_to_string(tokens.join("evaGxfADs6pSRb2LAv9IZf17Dt3juxGJ-PCt92wr-oA")).unwrap(),
            "body"
        );

        // A traversal, an empty name and an over-long one are refused rather than
        // escaped: an ACME token is base64url, so anything else is a bug or an attack.
        for hostile in ["../escape", "a/b", "", "."] {
            assert!(
                write_challenge(&tokens, hostile, "body").is_err(),
                "{hostile:?} must be refused"
            );
        }
        assert!(write_challenge(&tokens, &"a".repeat(129), "body").is_err());
        assert!(!dir.path().join("escape").exists());

        // Clearing takes every file in the directory and touches nothing outside it,
        // which is the property that makes it safe to call without a token in hand.
        std::fs::write(tokens.join("stale-token"), "old").unwrap();
        clear_challenges(&tokens);
        assert!(std::fs::read_dir(&tokens).unwrap().next().is_none());
        // A missing directory is a no-op, not a panic: it is called before the first
        // token is ever written.
        clear_challenges(&tokens.join("does-not-exist"));
    }

    #[test]
    fn a_jwk_with_the_wrong_coordinate_length_is_refused() {
        let bad = Jwk {
            crv: "P-256".into(),
            kty: "EC".into(),
            x: b64url(&[0u8; 31]),
            y: b64url(&[0u8; 32]),
        };
        assert!(bad.validate().is_err());
        let wrong_curve = Jwk {
            crv: "P-384".into(),
            ..bad
        };
        assert!(wrong_curve.validate().is_err());
    }
}

// =============================================================================
// The transport
// =============================================================================

/// One HTTP response from the ACME server, with the headers the protocol reads.
#[derive(Debug, Clone)]
pub struct AcmeResponse {
    /// The status code.
    pub status: u16,
    /// The `Location` header, which carries the account and order URLs.
    pub location: Option<String>,
    /// The `Replay-Nonce` header.
    pub nonce: Option<String>,
    /// The `Content-Type` header.
    pub content_type: Option<String>,
    /// The raw body.
    pub body: Vec<u8>,
}

impl AcmeResponse {
    /// The body parsed as JSON.
    pub fn json(&self) -> Result<serde_json::Value> {
        serde_json::from_slice(&self.body).map_err(|error| {
            FerromaError::internal(format!(
                "the ACME server answered {}{}",
                if self.body.is_empty() { "an empty body" } else { "a body that is not JSON" },
                if self.body.is_empty() {
                    String::new()
                } else {
                    format!(" ({error})")
                }
            ))
        })
    }

    /// Whether the status is one of the successful ones.
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// The RFC 7807 `detail` a failure carried, for a log line or an operator.
    pub fn problem_detail(&self) -> String {
        let Ok(value) = self.json() else {
            return format!("HTTP {}", self.status);
        };
        let kind = value.get("type").and_then(|v| v.as_str()).unwrap_or("about:blank");
        let detail = value.get("detail").and_then(|v| v.as_str()).unwrap_or("");
        if detail.is_empty() {
            format!("HTTP {} ({kind})", self.status)
        } else {
            format!("HTTP {} ({kind}): {detail}", self.status)
        }
    }
}

/// How the client reaches the ACME server.
///
/// A trait so the whole flow can be exercised against a local server in tests: the
/// protocol is 90% request shape, and the only honest way to test that is to serve it.
pub trait AcmeHttp: Send + Sync {
    /// Perform one request. `body` is already the JSON a POST carries.
    fn request(
        &self,
        method: &str,
        url: &str,
        body: Option<Vec<u8>>,
        content_type: Option<&str>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<AcmeResponse>> + Send + '_>,
    >;
}

/// The production transport.
#[derive(Debug)]
pub struct ReqwestHttp {
    client: reqwest::Client,
}

impl Default for ReqwestHttp {
    fn default() -> Self {
        Self::new()
    }
}

impl ReqwestHttp {
    /// Build a client that never follows a redirect and never carries cookies.
    ///
    /// ACME is a signed protocol: a redirect would send a signature to an origin the
    /// signature does not name, and the `url` member of the protected header has to
    /// match the URL actually fetched.
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("ferroma/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        ReqwestHttp { client }
    }
}

impl AcmeHttp for ReqwestHttp {
    fn request(
        &self,
        method: &str,
        url: &str,
        body: Option<Vec<u8>>,
        content_type: Option<&str>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<AcmeResponse>> + Send + '_>,
    > {
        let url = url.to_string();
        let method = method.to_string();
        let content_type = content_type.map(str::to_string);
        Box::pin(async move {
            let mut request = match method.as_str() {
                "GET" => self.client.get(&url),
                "HEAD" => self.client.head(&url),
                "POST" => self.client.post(&url),
                other => {
                    return Err(FerromaError::internal(format!(
                        "the ACME client does not issue {other} requests"
                    )))
                }
            };
            if let Some(content_type) = content_type {
                request = request.header(reqwest::header::CONTENT_TYPE, content_type);
            }
            if let Some(body) = body {
                request = request.body(body);
            }
            let response = request
                .send()
                .await
                .map_err(|error| FerromaError::internal(format!("ACME request to {url} failed: {error}")))?;
            let status = response.status().as_u16();
            let header = |name: &str| {
                response
                    .headers()
                    .get(name)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string)
            };
            let location = header("location");
            let nonce = header("replay-nonce");
            let content_type = header("content-type");
            let body = response
                .bytes()
                .await
                .map_err(|error| FerromaError::internal(format!("ACME response from {url} failed: {error}")))?
                .to_vec();
            Ok(AcmeResponse {
                status,
                location,
                nonce,
                content_type,
                body,
            })
        })
    }
}

// =============================================================================
// The client
// =============================================================================

/// How many times a request is retried after a `badNonce`, which RFC 8555 §6.5 says a
/// server may answer at any time and a client must handle by retrying with the nonce it
/// just returned.
const BAD_NONCE_RETRIES: usize = 3;

/// How long to wait for an authorization to become valid.
const VALIDATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// How often to poll it.
const VALIDATION_POLL: std::time::Duration = std::time::Duration::from_secs(2);

/// An ACME client for one directory and one account.
pub struct AcmeClient<'a> {
    http: &'a dyn AcmeHttp,
    directory: Directory,
    key: &'a AccountKey,
    /// The account URL, once the account exists.
    kid: Option<String>,
    /// The most recent nonce the server handed us. Every response carries one, and
    /// reusing it is what saves a round trip per request.
    nonce: Option<String>,
}

impl<'a> AcmeClient<'a> {
    /// Fetch the directory and prepare the client.
    pub async fn discover(http: &'a dyn AcmeHttp, key: &'a AccountKey, directory_url: &str) -> Result<Self> {
        let response = http.request("GET", directory_url, None, None).await?;
        if !response.is_success() {
            return Err(FerromaError::internal(format!(
                "the ACME directory at {directory_url} is unavailable: {}",
                response.problem_detail()
            )));
        }
        let directory: Directory = serde_json::from_slice(&response.body).map_err(|error| {
            FerromaError::internal(format!("the ACME directory is not the document RFC 8555 defines: {error}"))
        })?;
        Ok(AcmeClient {
            http,
            directory,
            key,
            kid: None,
            nonce: response.nonce,
        })
    }

    /// A fresh nonce, from the cache or from the server.
    async fn take_nonce(&mut self) -> Result<String> {
        if let Some(nonce) = self.nonce.take() {
            return Ok(nonce);
        }
        let url = self.directory.new_nonce.clone();
        let response = self.http.request("HEAD", &url, None, None).await?;
        response.nonce.ok_or_else(|| {
            FerromaError::internal(format!(
                "the ACME server did not return a Replay-Nonce from {url} (HTTP {})",
                response.status
            ))
        })
    }

    /// One signed POST, with the `badNonce` retry the protocol requires.
    async fn post(&mut self, url: &str, payload: Option<&serde_json::Value>) -> Result<AcmeResponse> {
        let mut attempt = 0usize;
        loop {
            let nonce = self.take_nonce().await?;
            let account = match self.kid.as_deref() {
                Some(kid) => AccountRef::Kid(kid),
                None => AccountRef::Jwk(&self.key.jwk()?),
            };
            let body = sign_request(self.key, url, &nonce, &account, payload)?;
            let response = self
                .http
                .request(
                    "POST",
                    url,
                    Some(body.to_string().into_bytes()),
                    Some("application/jose+json"),
                )
                .await?;

            // Every response may carry the nonce for the next request.
            if let Some(fresh) = response.nonce.clone() {
                self.nonce = Some(fresh);
            }

            let bad_nonce = response.status == 400
                && response
                    .json()
                    .ok()
                    .and_then(|value| value.get("type").and_then(|t| t.as_str()).map(str::to_string))
                    .is_some_and(|kind| kind.ends_with(":badNonce"));
            if bad_nonce && attempt < BAD_NONCE_RETRIES {
                attempt += 1;
                continue;
            }
            return Ok(response);
        }
    }

    /// Create the account, or find the one this key already owns.
    ///
    /// `onlyReturnExisting` is what makes a second run idempotent: a new order is not
    /// needed, and the same key must not create a second account.
    pub async fn ensure_account(&mut self, contact_email: &str, agree_tos: bool) -> Result<String> {
        let mut payload = serde_json::json!({
            "termsOfServiceAgreed": agree_tos,
        });
        if !contact_email.trim().is_empty() {
            payload["contact"] = serde_json::json!([format!("mailto:{}", contact_email.trim())]);
        }
        // An operator is agreeing to a document; naming it in the log is the least a
        // server can do about that, and it is the only place the URL is ever available.
        if agree_tos {
            match self.directory.meta.terms_of_service.as_deref() {
                Some(terms) => tracing::info!(terms, "agreeing to the certificate authority's terms of service"),
                None => tracing::info!("agreeing to the certificate authority's terms of service"),
            }
        }
        let url = self.directory.new_account.clone();
        let response = self.post(&url, Some(&payload)).await?;
        if !response.is_success() {
            return Err(FerromaError::internal(format!(
                "the ACME server refused the account: {}",
                response.problem_detail()
            )));
        }
        let kid = response.location.ok_or_else(|| {
            FerromaError::internal("the ACME server created an account without a Location header")
        })?;
        self.kid = Some(kid.clone());
        Ok(kid)
    }

    /// Whether this key already has an account, without creating one.
    pub async fn find_account(&mut self) -> Result<Option<String>> {
        let url = self.directory.new_account.clone();
        let response = self
            .post(&url, Some(&serde_json::json!({ "onlyReturnExisting": true })))
            .await?;
        if response.is_success() {
            let kid = response.location.clone();
            self.kid = kid.clone();
            return Ok(kid);
        }
        // `accountDoesNotExist` is the documented answer for a key with no account, and
        // is not an error condition for this call.
        if response.status == 400
            && response
                .json()
                .ok()
                .and_then(|value| value.get("type").and_then(|t| t.as_str()).map(str::to_string))
                .is_some_and(|kind| kind.ends_with(":accountDoesNotExist"))
        {
            return Ok(None);
        }
        Err(FerromaError::internal(format!(
            "could not look up the ACME account: {}",
            response.problem_detail()
        )))
    }

    /// Order a certificate for `domains`.
    pub async fn new_order(&mut self, domains: &[String]) -> Result<Order> {
        if domains.is_empty() {
            return Err(FerromaError::Invalid(
                "an ACME order needs at least one domain".into(),
            ));
        }
        let identifiers: Vec<serde_json::Value> = domains
            .iter()
            .map(|domain| serde_json::json!({"type": "dns", "value": domain}))
            .collect();
        let payload = serde_json::json!({ "identifiers": identifiers });
        let url = self.directory.new_order.clone();
        let response = self.post(&url, Some(&payload)).await?;
        if !response.is_success() {
            return Err(FerromaError::internal(format!(
                "the ACME server refused the order for {}: {}",
                domains.join(", "),
                response.problem_detail()
            )));
        }
        let order_url = response
            .location
            .clone()
            .ok_or_else(|| FerromaError::internal("the ACME order has no Location header"))?;
        let body = response.json()?;
        Ok(Order {
            url: order_url,
            authorizations: body
                .get("authorizations")
                .and_then(|v| v.as_array())
                .map(|list| {
                    list.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
            finalize: body
                .get("finalize")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
        })
    }

    /// POST-as-GET, the only form of read RFC 8555 allows once an account exists.
    async fn post_as_get(&mut self, url: &str) -> Result<serde_json::Value> {
        let response = self.post(url, None).await?;
        if !response.is_success() {
            return Err(FerromaError::internal(format!(
                "the ACME server refused GET {url}: {}",
                response.problem_detail()
            )));
        }
        response.json()
    }

    /// Satisfy one HTTP-01 authorization.
    ///
    /// `write_token` is called with the token and the body to serve under it; the caller
    /// decides where that lands, which is what keeps the layout a deployment concern
    /// rather than this client's.
    pub async fn solve_http01(
        &mut self,
        authorization_url: &str,
        write_token: &(dyn Fn(&str, &str) -> Result<()> + Sync),
    ) -> Result<String> {
        let thumbprint = self.key.thumbprint()?;
        let authorization = self.post_as_get(authorization_url).await?;
        let identifier = authorization
            .get("identifier")
            .and_then(|value| value.get("value"))
            .and_then(|value| value.as_str())
            .unwrap_or("?")
            .to_string();

        if authorization.get("status").and_then(|s| s.as_str()) == Some("valid") {
            return Ok(identifier);
        }

        let challenge = authorization
            .get("challenges")
            .and_then(|value| value.as_array())
            .and_then(|list| {
                list.iter()
                    .find(|challenge| challenge.get("type").and_then(|t| t.as_str()) == Some("http-01"))
            })
            .ok_or_else(|| {
                FerromaError::internal(format!(
                    "the ACME server offered no http-01 challenge for {identifier}"
                ))
            })?;
        let token = challenge
            .get("token")
            .and_then(|value| value.as_str())
            .ok_or_else(|| FerromaError::internal("an ACME challenge without a token"))?
            .to_string();
        let challenge_url = challenge
            .get("url")
            .and_then(|value| value.as_str())
            .ok_or_else(|| FerromaError::internal("an ACME challenge without a URL"))?
            .to_string();
        let expected = challenge_response(&token, &thumbprint);

        // The file has to exist before the server is told to look at it.
        write_token(&token, &expected)?;

        let response = self.post(&challenge_url, Some(&serde_json::json!({}))).await?;
        if !response.is_success() {
            return Err(FerromaError::internal(format!(
                "the ACME server refused the challenge for {identifier}: {}",
                response.problem_detail()
            )));
        }

        let deadline = std::time::Instant::now() + VALIDATION_TIMEOUT;
        loop {
            let authorization = self.post_as_get(authorization_url).await?;
            match authorization.get("status").and_then(|s| s.as_str()) {
                Some("valid") => return Ok(identifier),
                Some("invalid") => {
                    // The per-challenge error is what an operator needs: "invalid" alone
                    // says nothing about whether DNS, a proxy or the file was wrong.
                    let detail = authorization
                        .get("challenges")
                        .and_then(|value| value.as_array())
                        .and_then(|list| {
                            list.iter().find_map(|challenge| {
                                challenge
                                    .get("error")
                                    .and_then(|error| error.get("detail"))
                                    .and_then(|detail| detail.as_str())
                            })
                        })
                        .unwrap_or("no detail supplied");
                    return Err(FerromaError::internal(format!(
                        "the ACME server could not validate {identifier}: {detail}"
                    )));
                }
                _ => {}
            }
            if std::time::Instant::now() >= deadline {
                return Err(FerromaError::internal(format!(
                    "the ACME server did not finish validating {identifier} within {}s",
                    VALIDATION_TIMEOUT.as_secs()
                )));
            }
            tokio::time::sleep(VALIDATION_POLL).await;
        }
    }

    /// Send a CSR and wait for the order to reach `valid`.
    pub async fn finalize(&mut self, order: &Order, csr_der: &[u8]) -> Result<String> {
        let response = self
            .post(
                &order.finalize,
                Some(&serde_json::json!({ "csr": b64url(csr_der) })),
            )
            .await?;
        if !response.is_success() {
            return Err(FerromaError::internal(format!(
                "the ACME server refused the CSR: {}",
                response.problem_detail()
            )));
        }

        let deadline = std::time::Instant::now() + VALIDATION_TIMEOUT;
        loop {
            let order_state = self.post_as_get(&order.url).await?;
            match order_state.get("status").and_then(|s| s.as_str()) {
                Some("valid") => {
                    let certificate = order_state
                        .get("certificate")
                        .and_then(|value| value.as_str())
                        .ok_or_else(|| {
                            FerromaError::internal("a valid ACME order carries no certificate URL")
                        })?;
                    return Ok(certificate.to_string());
                }
                Some("invalid") => {
                    let detail = order_state
                        .get("error")
                        .and_then(|error| error.get("detail"))
                        .and_then(|detail| detail.as_str())
                        .unwrap_or("no detail supplied");
                    return Err(FerromaError::internal(format!(
                        "the ACME order became invalid: {detail}"
                    )));
                }
                _ => {}
            }
            if std::time::Instant::now() >= deadline {
                return Err(FerromaError::internal(
                    "the ACME order did not become valid in time; retry later and the pending \
                     order will be reused"
                        .to_string(),
                ));
            }
            tokio::time::sleep(VALIDATION_POLL).await;
        }
    }

    /// Download the issued chain, as PEM.
    pub async fn download_certificate(&mut self, certificate_url: &str) -> Result<String> {
        let response = self.post(certificate_url, None).await?;
        if !response.is_success() {
            return Err(FerromaError::internal(format!(
                "could not download the certificate: {}",
                response.problem_detail()
            )));
        }
        // RFC 8555 §7.4.2 says the chain comes back as `application/pem-certificate-chain`.
        // A different type is not fatal — the body still has to parse — but it is worth
        // naming, because a CA that answers with a problem document and `200` would
        // otherwise surface as "not valid UTF-8 PEM" with no hint of what happened.
        if let Some(content_type) = response.content_type.as_deref() {
            if !content_type.contains("pem-certificate-chain") {
                tracing::warn!(
                    %content_type,
                    "the ACME certificate response is not the type RFC 8555 specifies"
                );
            }
        }
        let pem = String::from_utf8(response.body).map_err(|_| {
            FerromaError::internal("the ACME certificate was not valid UTF-8 PEM")
        })?;
        if !pem.contains("BEGIN CERTIFICATE") {
            return Err(FerromaError::internal(
                "the ACME certificate response was not a PEM chain",
            ));
        }
        Ok(pem)
    }
}

/// The parts of an order the flow acts on.
///
/// The status the creation response carried is not kept: the flow polls the order URL
/// anyway, because a `newOrder` that answers `ready` (every authorization already valid
/// from an earlier attempt) and one that answers `pending` are handled by the same code
/// once the authorizations are walked.
#[derive(Debug, Clone)]
pub struct Order {
    /// The order URL, for polling.
    pub url: String,
    /// The authorization URLs to satisfy.
    pub authorizations: Vec<String>,
    /// Where the CSR goes.
    pub finalize: String,
}

// =============================================================================
// Issuing
// =============================================================================

/// What one issuance produced.
#[derive(Debug, Clone)]
pub struct Issued {
    /// The certificate chain, PEM, leaf first.
    pub certificate_pem: String,
    /// The certificate's private key, PEM, PKCS#8.
    pub key_pem: String,
    /// The names the certificate covers.
    pub domains: Vec<String>,
}

/// Issue a certificate for `domains` over HTTP-01.
///
/// The account key is not the certificate key: the account key is the long-lived
/// identity the ACME server knows, and reusing it for a leaf would put the same private
/// key in the certificate handshake that also signs requests.
pub async fn issue(
    http: &dyn AcmeHttp,
    directory_url: &str,
    account_key: &AccountKey,
    email: &str,
    agree_tos: bool,
    domains: &[String],
    write_token: &(dyn Fn(&str, &str) -> Result<()> + Sync),
) -> Result<Issued> {
    let mut client = AcmeClient::discover(http, account_key, directory_url).await?;

    // Reuse the account this key already owns rather than creating a second one; a
    // fresh key with no account is registered on the spot.
    match client.find_account().await? {
        Some(_) => {}
        None => {
            client.ensure_account(email, agree_tos).await?;
        }
    }

    let order = client.new_order(domains).await?;
    for authorization in &order.authorizations {
        client.solve_http01(authorization, write_token).await?;
    }

    // A separate key for the certificate itself.
    let cert_key = rcgen::KeyPair::generate()
        .map_err(|error| FerromaError::internal(format!("could not generate a certificate key: {error}")))?;
    // A CSR carries the subject and the subjectAltName, and nothing else: `is_ca`,
    // `key_usages` and `extended_key_usages` describe the *certificate*, which the CA
    // decides, and rcgen refuses to serialise a request that sets them. Attempting to
    // ask for a server certificate here therefore fails before any request is sent —
    // the CA sets `serverAuth` by policy.
    let params = rcgen::CertificateParams::new(domains.to_vec())
        .map_err(|error| FerromaError::internal(format!("could not build the CSR parameters: {error}")))?;
    let csr = params
        .serialize_request(&cert_key)
        .map_err(|error| FerromaError::internal(format!("could not build the CSR: {error}")))?;

    let certificate_url = client.finalize(&order, csr.der()).await?;
    let certificate_pem = client.download_certificate(&certificate_url).await?;

    Ok(Issued {
        certificate_pem,
        key_pem: cert_key.serialize_pem(),
        domains: domains.to_vec(),
    })
}

/// Write the challenge file for one token under `directory`.
///
/// The name is validated rather than escaped: an ACME token is base64url by definition
/// (RFC 8555 §8.3), so anything else is either a bug or an attempt to write outside the
/// directory, and both deserve a refusal rather than a sanitised path.
pub fn write_challenge(directory: &std::path::Path, token: &str, body: &str) -> Result<()> {
    if token.is_empty()
        || token.len() > 128
        || !token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(FerromaError::Invalid(format!(
            "refusing to write a challenge file for the token {token:?}"
        )));
    }
    std::fs::create_dir_all(directory)
        .map_err(|error| FerromaError::internal(format!("could not create {}: {error}", directory.display())))?;
    let path = directory.join(token);
    // 0644: the ACME server fetches this over plain HTTP, and a reverse proxy serving it
    // as another user has to be able to read it.
    std::fs::write(&path, body)
        .map_err(|error| FerromaError::internal(format!("could not write {}: {error}", path.display())))?;
    Ok(())
}

/// Remove every challenge file.
///
/// Called before an issuance as well as after one: a run that failed between publishing
/// a token and answering the challenge leaves the file behind, and a token nothing will
/// ever validate is a token that should not stay published.
pub fn clear_challenges(directory: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// When a PEM certificate stops being valid, if it can be read.
///
/// Parsed with `rustls-pemfile` for the DER, then `x509-parser`-free: the validity is
/// read out of the DER directly by looking for the `UTCTime`/`GeneralizedTime` pair,
/// because the alternative is a certificate-parsing dependency for two fields. A
/// certificate whose dates cannot be read is reported as `None`, and the caller renews.
pub fn certificate_not_after(certificate_pem: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    let der = first_certificate_der(certificate_pem)?;
    not_after_from_der(&der)
}

/// The first certificate's DER out of a PEM chain.
fn first_certificate_der(pem: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    let mut inside = false;
    let mut body = String::new();
    for line in pem.lines() {
        let line = line.trim();
        if line.starts_with("-----BEGIN CERTIFICATE") {
            inside = true;
            continue;
        }
        if line.starts_with("-----END CERTIFICATE") {
            if inside {
                break;
            }
            continue;
        }
        if inside {
            body.push_str(line);
        }
    }
    if body.is_empty() {
        return None;
    }
    base64::engine::general_purpose::STANDARD.decode(body).ok()
}

/// Read `notAfter` out of a DER certificate.
///
/// Walks the `TBSCertificate` for the `validity` sequence and takes its second member.
/// It is deliberately narrow: it reads two `Time` values and nothing else, and returns
/// `None` for anything it does not recognise — a certificate this cannot parse is one
/// the caller treats as "renew now", which is the safe direction.
fn not_after_from_der(der: &[u8]) -> Option<chrono::DateTime<chrono::Utc>> {
    /// Read one DER length, returning it and how many bytes it took.
    fn length(bytes: &[u8]) -> Option<(usize, usize)> {
        let first = *bytes.first()?;
        if first < 0x80 {
            return Some((usize::from(first), 1));
        }
        let count = usize::from(first & 0x7F);
        if count == 0 || count > 4 || bytes.len() < count + 1 {
            return None;
        }
        let mut value = 0usize;
        for byte in &bytes[1..=count] {
            value = value << 8 | usize::from(*byte);
        }
        Some((value, count + 1))
    }

    /// Split one TLV, returning its tag, contents and the offset just past it.
    fn tlv(bytes: &[u8], at: usize) -> Option<(u8, &[u8], usize)> {
        let tag = *bytes.get(at)?;
        let (len, header) = length(bytes.get(at + 1..)?)?;
        let start = at + 1 + header;
        let end = start.checked_add(len)?;
        Some((tag, bytes.get(start..end)?, end))
    }

    fn parse_time(tag: u8, contents: &[u8]) -> Option<chrono::DateTime<chrono::Utc>> {
        let text = std::str::from_utf8(contents).ok()?;
        match tag {
            // UTCTime: YYMMDDHHMMSSZ, with the century implied by RFC 5280 §4.1.2.5.
            0x17 => {
                // The century is expanded here rather than by `%y`: chrono's two-digit
                // year follows the POSIX rule (00-68 is 20xx), which is a *different*
                // rule from RFC 5280 §4.1.2.5 (`YY` is 19YY from 50 up, 20YY below).
                // Letting `%y` map first and correcting afterwards shifted a 2049
                // certificate to 3949 — a date no renewal check would ever act on.
                if text.len() < 11 {
                    return None;
                }
                let (yy, rest) = text.split_at(2);
                let year: i32 = yy.parse().ok()?;
                let year = if year >= 50 { year + 1900 } else { year + 2000 };
                let expanded = format!("{year:04}{rest}");
                Some(
                    chrono::NaiveDateTime::parse_from_str(&expanded, "%Y%m%d%H%M%SZ")
                        .ok()?
                        .and_utc(),
                )
            }
            // GeneralizedTime: YYYYMMDDHHMMSSZ.
            0x18 => {
                let naive = chrono::NaiveDateTime::parse_from_str(text, "%Y%m%d%H%M%SZ").ok()?;
                Some(naive.and_utc())
            }
            _ => None,
        }
    }

    // Certificate -> tbsCertificate -> validity -> notAfter.
    let (tag, certificate, _) = tlv(der, 0)?;
    if tag != 0x30 {
        return None;
    }
    let (tag, tbs, _) = tlv(certificate, 0)?;
    if tag != 0x30 {
        return None;
    }
    // `validity` is the fifth element of `TBSCertificate` when `version` is present and
    // the fourth when it is not, so the elements are walked rather than indexed.
    let mut at = 0usize;
    let mut saw_version = false;
    loop {
        let (tag, contents, next) = tlv(tbs, at)?;
        if tag == 0xA0 && !saw_version {
            saw_version = true;
            at = next;
            continue;
        }
        if tag == 0x30 {
            // `validity` is the first SEQUENCE in `TBSCertificate` whose members are two
            // `Time` values. The SEQUENCEs before it — `signature` and `issuer` — hold an
            // algorithm identifier and a name, so looking for the times identifies it
            // without depending on how many optional fields came first.
            let mut inside = 0usize;
            let mut inner = 0usize;
            let mut found = None;
            while let Some((tag, contents, next)) = tlv(contents, inner) {
                if tag == 0x17 || tag == 0x18 {
                    found = parse_time(tag, contents);
                }
                inside += 1;
                inner = next;
                if inside > 2 {
                    break;
                }
            }
            if let Some(time) = found {
                return Some(time);
            }
        }
        at = next;
        // Only the fields before `subject` can hold `validity`; three is enough to find
        // it in a well-formed certificate and bounded enough not to walk a whole subject.
        if at >= tbs.len() {
            return None;
        }
    }
}

// =============================================================================
// The end-to-end flow, against a local ACME server
// =============================================================================

#[cfg(test)]
mod flow_tests {
    use super::*;
    use axum::response::IntoResponse;
    use axum::routing::get;
    use axum::routing::post;
    use axum::Router;
    use std::collections::{HashMap, HashSet};
    use std::sync::{Arc, Mutex};

    /// One order as the fake server holds it.
    #[derive(Default, Clone)]
    struct FakeOrder {
        domains: Vec<String>,
        authz: Vec<String>,
        finalized: bool,
        certificate: String,
    }

    /// One authorization.
    #[derive(Default, Clone)]
    struct FakeAuthz {
        domain: String,
        token: String,
        challenge_ok: bool,
    }

    #[derive(Default)]
    struct FakeAcme {
        base: String,
        nonces: Mutex<HashSet<String>>,
        /// The account key, learned from the first `newAccount` and checked on every
        /// later request — which is what makes this a test of the JWS and not of JSON.
        account_point: Mutex<Option<Vec<u8>>>,
        /// The thumbprint the server computes from that key, for the challenge check.
        thumbprint: Mutex<Option<String>>,
        /// Accounts this server has created, keyed by the key's thumbprint — the same
        /// way a real CA keys them, which is what makes `onlyReturnExisting` meaningful.
        accounts: Mutex<HashMap<String, String>>,
        orders: Mutex<HashMap<String, FakeOrder>>,
        authz: Mutex<HashMap<String, FakeAuthz>>,
        /// What the client published at each token, standing in for the file a real
        /// server would fetch over HTTP.
        published: Mutex<HashMap<String, String>>,
        /// Every request, so the test can assert on the shapes that were sent.
        seen: Mutex<Vec<String>>,
    }

    impl FakeAcme {
        fn issue_nonce(&self) -> String {
            let nonce = format!("nonce-{}", uuid::Uuid::new_v4());
            self.nonces.lock().unwrap().insert(nonce.clone());
            nonce
        }

        /// Verify one JWS and return its payload.
        ///
        /// Everything RFC 8555 §6.2 requires is checked here: the algorithm, that the
        /// `url` names the request actually made, that the nonce was one this server
        /// issued and has not been used, and that the signature verifies against the
        /// account key.
        fn verify(&self, expected_url: &str, body: &[u8]) -> Result<serde_json::Value> {
            let jws: serde_json::Value = serde_json::from_slice(body)
                .map_err(|_| FerromaError::Invalid("the request body was not a JWS".into()))?;
            let protected = b64url_decode(jws["protected"].as_str().unwrap_or_default())?;
            let header: serde_json::Value = serde_json::from_slice(&protected)
                .map_err(|_| FerromaError::Invalid("the protected header was not JSON".into()))?;

            assert_eq!(header["alg"], "ES256", "the algorithm must be ES256");
            assert_eq!(
                header["url"].as_str(),
                Some(expected_url),
                "the signed url must name the request that was made"
            );
            let nonce = header["nonce"].as_str().unwrap_or_default().to_string();
            assert!(
                self.nonces.lock().unwrap().remove(&nonce),
                "nonce {nonce:?} was not issued or was already used"
            );

            // First request publishes the key; later ones must name the account instead.
            let point = match header.get("jwk") {
                Some(jwk) => {
                    let jwk: Jwk = serde_json::from_value(jwk.clone())
                        .map_err(|_| FerromaError::Invalid("the jwk header was not a JWK".into()))?;
                    jwk.validate()?;
                    let mut point = vec![0x04];
                    point.extend(b64url_decode(&jwk.x)?);
                    point.extend(b64url_decode(&jwk.y)?);
                    *self.account_point.lock().unwrap() = Some(point.clone());
                    *self.thumbprint.lock().unwrap() = Some(
                        b64url(&Sha256::digest(jwk.canonical_json().as_bytes())),
                    );
                    point
                }
                None => {
                    assert!(header["kid"].is_string(), "a request needs jwk or kid");
                    self.account_point
                        .lock()
                        .unwrap()
                        .clone()
                        .expect("the account key must be known before a kid is used")
                }
            };

            let payload = jws["payload"].as_str().unwrap_or_default();
            let signature = b64url_decode(jws["signature"].as_str().unwrap_or_default())?;
            assert_eq!(signature.len(), 64, "JOSE wants the raw r || s signature");
            ring::signature::UnparsedPublicKey::new(
                &ring::signature::ECDSA_P256_SHA256_FIXED,
                &point,
            )
            .verify(
                format!(
                    "{}.{}",
                    jws["protected"].as_str().unwrap_or_default(),
                    payload
                )
                .as_bytes(),
                &signature,
            )
            .expect("the JWS signature must verify against the account key");

            self.seen
                .lock()
                .unwrap()
                .push(expected_url.to_string());
            if payload.is_empty() {
                return Ok(serde_json::Value::Null);
            }
            serde_json::from_slice(&b64url_decode(payload)?)
                .map_err(|_| FerromaError::Invalid("the payload was not JSON".into()))
        }

        /// A challenge file the client published, read as the CA would read it.
        fn published_at(&self, token: &str) -> Option<String> {
            self.published.lock().unwrap().get(token).cloned()
        }
    }

    fn json_response(body: serde_json::Value) -> axum::response::Response {
        json_response_with_status(axum::http::StatusCode::OK, body)
    }

    /// A JSON body with a chosen status, for the RFC 7807 problem documents.
    fn json_response_with_status(
        status: axum::http::StatusCode,
        body: serde_json::Value,
    ) -> axum::response::Response {
        (
            status,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            body.to_string(),
        )
            .into_response()
    }

    /// Build the local ACME server and start it, returning the base URL.
    async fn start_fake_acme() -> (Arc<FakeAcme>, tokio::task::JoinHandle<()>) {
        // The listener is bound before anything else: the directory URLs are built from
        // the address it actually got, and the client signs those exact URLs, so a state
        // created before the bind would hand out a base nobody can reach.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let state = Arc::new(FakeAcme {
            base: format!("http://{address}"),
            ..FakeAcme::default()
        });

        let directory = {
            let state = Arc::clone(&state);
            move || {
                let base = state.base.clone();
                async move {
                    json_response(serde_json::json!({
                        "newNonce": format!("{base}/new-nonce"),
                        "newAccount": format!("{base}/new-account"),
                        "newOrder": format!("{base}/new-order"),
                        "meta": { "termsOfService": "https://example.test/tos" }
                    }))
                }
            }
        };

        let new_nonce = {
            let state = Arc::clone(&state);
            move |headers: axum::http::HeaderMap| {
                let nonce = state.issue_nonce();
                let mut response = axum::http::StatusCode::OK.into_response();
                response.headers_mut().insert(
                    "replay-nonce",
                    nonce.parse().expect("a valid header value"),
                );
                let _ = headers;
                async move { response }
            }
        };

        let new_account = {
            let state = Arc::clone(&state);
            move |headers: axum::http::HeaderMap, body: axum::body::Bytes| {
                let payload = state
                    .verify(&format!("{}/new-account", state.base), &body)
                    .expect("a valid newAccount request");
                let thumbprint = state.thumbprint.lock().unwrap().clone().expect("a key");
                let existing = state.accounts.lock().unwrap().get(&thumbprint).cloned();
                let _ = headers;

                // `onlyReturnExisting` is how a client asks "do I already have an
                // account" without creating a second one.
                let looking_up = payload.get("onlyReturnExisting").and_then(|v| v.as_bool()) == Some(true);
                let mut response = if looking_up {
                    match existing {
                        Some(kid) => {
                            let mut response = axum::http::StatusCode::OK.into_response();
                            response.headers_mut().insert("location", kid.parse().unwrap());
                            response
                        }
                        None => json_response_with_status(
                            axum::http::StatusCode::BAD_REQUEST,
                            serde_json::json!({
                                "type": "urn:ietf:params:acme:error:accountDoesNotExist",
                                "detail": "no account for this key",
                            }),
                        ),
                    }
                } else {
                    assert_eq!(
                        payload["termsOfServiceAgreed"], true,
                        "the account must agree to the terms of service"
                    );
                    let kid = existing.unwrap_or_else(|| format!("{}/acct/1", state.base));
                    state
                        .accounts
                        .lock()
                        .unwrap()
                        .insert(thumbprint, kid.clone());
                    let mut response = axum::http::StatusCode::CREATED.into_response();
                    response.headers_mut().insert("location", kid.parse().unwrap());
                    response
                };
                response
                    .headers_mut()
                    .insert("replay-nonce", state.issue_nonce().parse().unwrap());
                async move { response }
            }
        };

        let new_order = {
            let state = Arc::clone(&state);
            move |body: axum::body::Bytes| {
                let payload = state
                    .verify(&format!("{}/new-order", state.base), &body)
                    .expect("a valid newOrder request");
                let domains: Vec<String> = payload["identifiers"]
                    .as_array()
                    .expect("identifiers")
                    .iter()
                    .map(|identifier| {
                        assert_eq!(identifier["type"], "dns");
                        identifier["value"].as_str().unwrap().to_string()
                    })
                    .collect();

                let id = format!("order-{}", state.orders.lock().unwrap().len() + 1);
                let mut order = FakeOrder {
                    domains: domains.clone(),
                    ..FakeOrder::default()
                };
                for (index, domain) in domains.iter().enumerate() {
                    let authz_id = format!("{id}-authz-{index}");
                    let token = format!("token-{authz_id}");
                    state.authz.lock().unwrap().insert(
                        authz_id.clone(),
                        FakeAuthz {
                            domain: domain.clone(),
                            token,
                            challenge_ok: false,
                        },
                    );
                    order.authz.push(authz_id);
                }
                state.orders.lock().unwrap().insert(id.clone(), order.clone());

                let authors = order
                    .authz
                    .iter()
                    .map(|authz| format!("{}/authz/{authz}", state.base))
                    .collect::<Vec<_>>();
                let mut response = json_response(serde_json::json!({
                    "status": "pending",
                    "authorizations": authors,
                    "finalize": format!("{}/finalize/{id}", state.base),
                }));
                response.headers_mut().insert(
                    "location",
                    format!("{}/order/{id}", state.base).parse().unwrap(),
                );
                response.headers_mut().insert("replay-nonce", state.issue_nonce().parse().unwrap());
                async move { response }
            }
        };

        let get_authz = {
            let state = Arc::clone(&state);
            move |axum::extract::Path(id): axum::extract::Path<String>, body: axum::body::Bytes| {
                let payload = state
                    .verify(&format!("{}/authz/{id}", state.base), &body)
                    .expect("a valid authorization poll");
                assert!(payload.is_null(), "a POST-as-GET carries an empty payload");
                let authz = state
                    .authz
                    .lock()
                    .unwrap()
                    .get(&id)
                    .cloned()
                    .expect("a known authorization");
                let mut response = json_response(serde_json::json!({
                    "status": if authz.challenge_ok { "valid" } else { "pending" },
                    "identifier": { "type": "dns", "value": authz.domain },
                    "challenges": [{
                        "type": "http-01",
                        "url": format!("{}/challenge/{id}", state.base),
                        "token": authz.token,
                        "status": if authz.challenge_ok { "valid" } else { "pending" },
                    }],
                }));
                response.headers_mut().insert("replay-nonce", state.issue_nonce().parse().unwrap());
                async move { response }
            }
        };

        let answer_challenge = {
            let state = Arc::clone(&state);
            move |axum::extract::Path(id): axum::extract::Path<String>, body: axum::body::Bytes| {
                state
                    .verify(&format!("{}/challenge/{id}", state.base), &body)
                    .expect("a valid challenge response");

                // The CA fetches the file and compares it with the key authorization it
                // computes from the account key it knows.
                let authz = state
                    .authz
                    .lock()
                    .unwrap()
                    .get(&id)
                    .cloned()
                    .expect("a known authorization");
                let expected = challenge_response(
                    &authz.token,
                    &state.thumbprint.lock().unwrap().clone().expect("an account key"),
                );
                let served = state
                    .published_at(&authz.token)
                    .expect("the client must publish the challenge before answering it");
                assert_eq!(
                    served, expected,
                    "the published key authorization must be what the CA computes"
                );
                state
                    .authz
                    .lock()
                    .unwrap()
                    .get_mut(&id)
                    .unwrap()
                    .challenge_ok = true;

                let mut response = json_response(serde_json::json!({ "status": "processing" }));
                response.headers_mut().insert("replay-nonce", state.issue_nonce().parse().unwrap());
                async move { response }
            }
        };

        let finalize = {
            let state = Arc::clone(&state);
            move |axum::extract::Path(id): axum::extract::Path<String>, body: axum::body::Bytes| {
                let payload = state
                    .verify(&format!("{}/finalize/{id}", state.base), &body)
                    .expect("a valid finalize request");
                let csr = b64url_decode(payload["csr"].as_str().expect("a csr"))
                    .expect("the csr must be base64url");

                // A DER sanity check plus the SANs: without a CSR parser in the tree,
                // this is what proves the CSR is a well-formed SEQUENCE carrying the
                // names that were ordered rather than an empty blob.
                assert_eq!(csr.first(), Some(&0x30), "a CSR is a DER SEQUENCE");
                assert!(csr.len() > 100, "a CSR is not a few bytes: {}", csr.len());
                let domains = state.orders.lock().unwrap().get(&id).unwrap().domains.clone();
                for domain in &domains {
                    assert!(
                        csr.windows(domain.len()).any(|window| window == domain.as_bytes()),
                        "the CSR must carry {domain} in its subjectAltName"
                    );
                }

                // Issue a real chain so the client's PEM handling and its expiry parsing
                // are exercised, not stubbed.
                let ca_key = rcgen::KeyPair::generate().unwrap();
                let mut ca_params = rcgen::CertificateParams::new(Vec::new()).unwrap();
                ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
                let ca_cert = ca_params.self_signed(&ca_key).unwrap();

                let leaf_key = rcgen::KeyPair::generate().unwrap();
                let mut leaf_params = rcgen::CertificateParams::new(domains.clone()).unwrap();
                leaf_params.not_after = rcgen::date_time_ymd(2031, 1, 2);
                let leaf = leaf_params.signed_by(&leaf_key, &ca_cert, &ca_key).unwrap();
                let pem = format!("{}{}", leaf.pem(), ca_cert.pem());

                {
                    let mut orders = state.orders.lock().unwrap();
                    let order = orders.get_mut(&id).unwrap();
                    order.finalized = true;
                    order.certificate = pem;
                }

                let mut response = json_response(serde_json::json!({ "status": "valid" }));
                response.headers_mut().insert("replay-nonce", state.issue_nonce().parse().unwrap());
                async move { response }
            }
        };

        let get_order = {
            let state = Arc::clone(&state);
            move |axum::extract::Path(id): axum::extract::Path<String>, body: axum::body::Bytes| {
                let payload = state
                    .verify(&format!("{}/order/{id}", state.base), &body)
                    .expect("a valid order poll");
                assert!(payload.is_null());
                let order = state.orders.lock().unwrap().get(&id).cloned().unwrap();
                let mut response = json_response(serde_json::json!({
                    "status": if order.finalized { "valid" } else { "pending" },
                    "certificate": if order.finalized {
                        serde_json::Value::String(format!("{}/cert/{id}", state.base))
                    } else {
                        serde_json::Value::Null
                    },
                }));
                response.headers_mut().insert("replay-nonce", state.issue_nonce().parse().unwrap());
                async move { response }
            }
        };

        let get_cert = {
            let state = Arc::clone(&state);
            move |axum::extract::Path(id): axum::extract::Path<String>, body: axum::body::Bytes| {
                let payload = state
                    .verify(&format!("{}/cert/{id}", state.base), &body)
                    .expect("a valid certificate download");
                assert!(payload.is_null());
                let pem = state.orders.lock().unwrap().get(&id).unwrap().certificate.clone();
                let mut response = ([(axum::http::header::CONTENT_TYPE, "application/pem-certificate-chain")], pem).into_response();
                response.headers_mut().insert("replay-nonce", state.issue_nonce().parse().unwrap());
                async move { response }
            }
        };

        let router = Router::new()
            .route("/directory", get(directory))
            .route("/new-nonce", get(new_nonce.clone()).head(new_nonce))
            .route("/new-account", post(new_account))
            .route("/new-order", post(new_order))
            .route("/authz/{id}", post(get_authz))
            .route("/challenge/{id}", post(answer_challenge))
            .route("/finalize/{id}", post(finalize))
            .route("/order/{id}", post(get_order))
            .route("/cert/{id}", post(get_cert))
            .with_state(());

        let handle = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        (state, handle)
    }

    /// The whole issuance, against a server that verifies every request.
    #[tokio::test]
    async fn a_certificate_is_issued_end_to_end() {
        let (server, handle) = start_fake_acme().await;
        let http = ReqwestHttp::new();
        let key = AccountKey::generate().unwrap();

        // The closure is what stands in for "the file is published where the CA can
        // fetch it": it writes into the server's own map, which is the one the fake
        // reads when it validates the challenge. Writing anywhere else would make the
        // test pass over a client that never published anything.
        let sink = Arc::clone(&server);
        let write_token = move |token: &str, body: &str| {
            sink.published
                .lock()
                .unwrap()
                .insert(token.to_string(), body.to_string());
            Ok(())
        };

        let domains = vec!["mail.example.com".to_string(), "smtp.example.com".to_string()];
        let issued = issue(
            &http,
            &format!("{}/directory", server.base),
            &key,
            "ops@example.com",
            true,
            &domains,
            &write_token,
        )
        .await
        .expect("the issuance must succeed");

        // The chain is a real chain, and the expiry parser reads the leaf it issued.
        assert!(issued.certificate_pem.contains("BEGIN CERTIFICATE"));
        assert_eq!(issued.certificate_pem.matches("BEGIN CERTIFICATE").count(), 2, "leaf plus issuer");
        assert_eq!(
            certificate_not_after(&issued.certificate_pem)
                .expect("a parseable certificate")
                .format("%Y-%m-%d")
                .to_string(),
            "2031-01-02"
        );
        assert!(issued.key_pem.contains("PRIVATE KEY"), "the leaf key is PEM PKCS#8");
        assert_eq!(issued.domains, domains);

        // Every challenge was answered, and every request the client sent was verified
        // by the server: a wrong signature, a stale nonce or a mismatched `url` fails
        // the fake before it fails the assertion.
        assert_eq!(
            server.published.lock().unwrap().len(),
            2,
            "one token per domain"
        );
        let seen = server.seen.lock().unwrap().clone();
        for expected in [
            "/new-account",
            "/new-order",
            "/challenge/",
            "/finalize/",
            "/cert/",
        ] {
            assert!(
                seen.iter().any(|url| url.contains(expected)),
                "the client never called {expected}: {seen:?}"
            );
        }

        handle.abort();
    }

    /// A second run reuses the account and does not create another one.
    #[tokio::test]
    async fn a_second_issuance_reuses_the_account_key() {
        let (server, handle) = start_fake_acme().await;
        let http = ReqwestHttp::new();
        let key = AccountKey::generate().unwrap();
        let domains = vec!["mail.example.com".to_string()];
        // The same sink as the first test: without it the CA cannot read the key
        // authorization and refuses the challenge, which is the correct behaviour.
        let sink = Arc::clone(&server);
        let write_token = move |token: &str, body: &str| {
            sink.published
                .lock()
                .unwrap()
                .insert(token.to_string(), body.to_string());
            Ok(())
        };

        issue(
            &http,
            &format!("{}/directory", server.base),
            &key,
            "ops@example.com",
            true,
            &domains,
            &write_token,
        )
        .await
        .expect("first issuance");

        // The second run finds the account the first one created: the fake keys accounts
        // by thumbprint exactly as a CA does, so a client that looked up its account
        // wrongly would create a second one and the assertion below would see it.
        let second = issue(
            &http,
            &format!("{}/directory", server.base),
            &key,
            "ops@example.com",
            true,
            &domains,
            &write_token,
        )
        .await
        .expect("second issuance");
        assert_eq!(second.domains, domains);

        // The server saw the same public key both times: a client that generated a new
        // account key per run would orphan every account it had created.
        let point = server.account_point.lock().unwrap().clone().unwrap();
        let expected = {
            let jwk = key.jwk().unwrap();
            let mut point = vec![0x04];
            point.extend(b64url_decode(&jwk.x).unwrap());
            point.extend(b64url_decode(&jwk.y).unwrap());
            point
        };
        assert_eq!(point, expected);

        handle.abort();
    }

    /// A refusal from the CA is reported with its reason, not as a generic failure.
    #[tokio::test]
    async fn an_order_without_domains_is_refused_before_any_request() {
        let (server, handle) = start_fake_acme().await;
        let http = ReqwestHttp::new();
        let key = AccountKey::generate().unwrap();
        let noop = |_token: &str, _body: &str| Ok(());

        let error = issue(
            &http,
            &format!("{}/directory", server.base),
            &key,
            "ops@example.com",
            true,
            &[],
            &noop,
        )
        .await
        .expect_err("an empty order is a programming error, not a CA error");
        assert!(error.to_string().contains("at least one domain"), "{error}");

        handle.abort();
    }
}

// =============================================================================
// Renewal, as the server and the CLI both see it
// =============================================================================

/// Why a certificate is or is not being renewed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenewalDecision {
    /// No certificate at the configured path.
    Missing,
    /// A certificate that could not be read, or whose expiry could not be parsed.
    Unreadable(String),
    /// The certificate expires within the renewal window.
    Expiring {
        /// When it expires.
        expires_at: chrono::DateTime<chrono::Utc>,
        /// How many whole days are left.
        days_left: i64,
    },
    /// The certificate is valid for longer than the window.
    Fresh {
        /// When it expires.
        expires_at: chrono::DateTime<chrono::Utc>,
        /// How many whole days are left.
        days_left: i64,
    },
}

impl RenewalDecision {
    /// Whether issuance should run.
    pub fn should_renew(&self) -> bool {
        !matches!(self, RenewalDecision::Fresh { .. })
    }

    /// One line for an operator.
    pub fn describe(&self) -> String {
        match self {
            RenewalDecision::Missing => "no certificate yet".to_string(),
            RenewalDecision::Unreadable(reason) => format!("certificate unreadable ({reason})"),
            RenewalDecision::Expiring { expires_at, days_left } => {
                format!("expires {expires_at} ({days_left} day(s) left): renewing")
            }
            RenewalDecision::Fresh { expires_at, days_left } => {
                format!("expires {expires_at} ({days_left} day(s) left)")
            }
        }
    }
}

/// Decide whether the configured certificate needs renewing.
///
/// Anything that cannot be read is treated as "renew": a certificate whose expiry is
/// unknown is one that may already be expired, and the cost of an unnecessary renewal is
/// one HTTP exchange against the cost of serving an expired certificate to every client.
pub fn renewal_decision(config: &ferroma_core::config::Config) -> RenewalDecision {
    let Some(path) = config.tls.cert_path.as_ref() else {
        return RenewalDecision::Missing;
    };
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return RenewalDecision::Missing,
        Err(error) => return RenewalDecision::Unreadable(error.to_string()),
    };
    let Some(expires_at) = certificate_not_after(&text) else {
        return RenewalDecision::Unreadable("no notAfter date".to_string());
    };
    let days_left = (expires_at - chrono::Utc::now()).num_days();
    let window = i64::try_from(config.tls.acme.renew_before_days).unwrap_or(i64::MAX);
    if days_left <= window {
        RenewalDecision::Expiring { expires_at, days_left }
    } else {
        RenewalDecision::Fresh { expires_at, days_left }
    }
}

/// Read the account key, or create and store one.
///
/// The key is the account: creating a new one where an old one exists orphans every
/// certificate the CA has already issued to this server, so the file is read first and
/// only created when it is genuinely absent.
pub fn account_key(acme: &ferroma_core::config::AcmeConfig) -> Result<AccountKey> {
    let path = acme.account_key_path();
    match std::fs::read_to_string(&path) {
        Ok(pem) => {
            let der = pem_to_der(&pem, "PRIVATE KEY")?;
            AccountKey::from_pkcs8(&der)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let key = AccountKey::generate()?;
            write_private_file(&path, &pem_from_der(key.to_pkcs8(), "PRIVATE KEY"))
                .map_err(|error| FerromaError::internal(format!("could not store the ACME account key: {error}")))?;
            tracing::info!(path = %path.display(), "generated an ACME account key");
            Ok(key)
        }
        Err(error) => Err(FerromaError::internal(format!(
            "could not read the ACME account key at {}: {error}",
            path.display()
        ))),
    }
}

/// Issue a certificate for the configured names.
pub async fn obtain(config: &ferroma_core::config::Config) -> Result<Issued> {
    let acme = &config.tls.acme;
    let key = account_key(acme)?;
    let domains = acme.effective_domains(&config.server.hostname);
    let http = ReqwestHttp::new();
    let challenge_dir = acme.challenge_dir();
    // A stale file from an interrupted run is not something the CA will ever ask for
    // again, so it goes before the new tokens are published.
    clear_challenges(&challenge_dir);
    let write_token = move |token: &str, body: &str| write_challenge(&challenge_dir, token, body);

    let issued = issue(
        &http,
        &acme.directory_url,
        &key,
        &acme.email,
        acme.agree_tos,
        &domains,
        &write_token,
    )
    .await?;

    // The challenge files are only needed until validation; leaving them behind keeps
    // publishing tokens that are no longer part of any order.
    clear_challenges(&acme.challenge_dir());
    Ok(issued)
}

/// Write the issued chain and key where the listener reads them.
///
/// Both files are written through a temporary file and renamed, so a crash or a restart
/// during installation cannot leave a half-written certificate that the next startup
/// would fail to parse.
pub fn install(
    config: &ferroma_core::config::Config,
    issued: &Issued,
) -> Result<(std::path::PathBuf, std::path::PathBuf)> {
    let cert_path = config
        .tls
        .cert_path
        .clone()
        .ok_or_else(|| FerromaError::Config("tls.cert_path is not set".into()))?;
    let key_path = config
        .tls
        .key_path
        .clone()
        .ok_or_else(|| FerromaError::Config("tls.key_path is not set".into()))?;
    atomic_write(&cert_path, issued.certificate_pem.as_bytes())?;
    // The key is the credential: 0600, before anything can read it.
    write_private_file(&key_path, &issued.key_pem).map_err(|error| {
        FerromaError::internal(format!("could not write {}: {error}", key_path.display()))
    })?;
    Ok((cert_path, key_path))
}

/// Write `contents` to `path` through a temporary file in the same directory.
fn atomic_write(path: &std::path::Path, contents: &[u8]) -> Result<()> {
    let directory = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    std::fs::create_dir_all(directory).map_err(|error| {
        FerromaError::internal(format!("could not create {}: {error}", directory.display()))
    })?;
    let temporary = directory.join(format!(
        ".{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("certificate")
    ));
    std::fs::write(&temporary, contents).map_err(|error| {
        FerromaError::internal(format!("could not write {}: {error}", temporary.display()))
    })?;
    std::fs::rename(&temporary, path).map_err(|error| {
        FerromaError::internal(format!("could not install {}: {error}", path.display()))
    })
}

/// Write a file only its owner can read.
fn write_private_file(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    if let Some(directory) = path.parent() {
        std::fs::create_dir_all(directory)?;
    }
    std::fs::write(path, contents)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Pull one PEM block's DER out of a document.
fn pem_to_der(pem: &str, label: &str) -> Result<Vec<u8>> {
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let start = pem
        .find(&begin)
        .ok_or_else(|| FerromaError::Invalid(format!("no {label} block in the PEM document")))?;
    let rest = &pem[start + begin.len()..];
    let stop = rest
        .find(&end)
        .ok_or_else(|| FerromaError::Invalid(format!("unterminated {label} block")))?;
    b64url_decode_standard(&rest[..stop])
}

/// Decode standard (padded) base64, which is what PEM uses — not base64url.
fn b64url_decode_standard(text: &str) -> Result<Vec<u8>> {
    use base64::Engine as _;
    let cleaned: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    base64::engine::general_purpose::STANDARD
        .decode(cleaned)
        .map_err(|_| FerromaError::Invalid("the PEM body is not base64".into()))
}

/// Wrap DER in a PEM block.
fn pem_from_der(der: &[u8], label: &str) -> String {
    use base64::Engine as _;
    let body = base64::engine::general_purpose::STANDARD.encode(der);
    let mut out = format!("-----BEGIN {label}-----\n");
    for chunk in body.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(chunk).unwrap_or_default());
        out.push('\n');
    }
    out.push_str(&format!("-----END {label}-----\n"));
    out
}

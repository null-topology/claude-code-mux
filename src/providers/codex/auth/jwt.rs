use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct IdTokenClaims {
    #[serde(default)]
    chatgpt_account_id: Option<String>,
    #[serde(default)]
    organizations: Option<Vec<OrgClaim>>,
    #[allow(dead_code)]
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    #[serde(rename = "https://api.openai.com/auth")]
    openai_auth: Option<OpenAiAuthClaim>,
    #[serde(default)]
    #[serde(rename = "https://api.openai.com/auth.chatgpt_account_id")]
    openai_chatgpt_account_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OrgClaim {
    id: String,
}

#[derive(Debug, Deserialize)]
struct OpenAiAuthClaim {
    #[serde(default)]
    chatgpt_account_id: Option<String>,
}

/// A token endpoint answer. Only a string `access_token` is required. An optional
/// field of the wrong type counts as absent: a `refresh_token` or `id_token` that is
/// not a string, and an `expires_in` that is not a positive integer.
#[derive(Debug, Deserialize)]
pub struct TokenResponse {
    #[serde(default, deserialize_with = "string_or_absent")]
    pub id_token: Option<String>,
    pub access_token: String,
    #[serde(default, deserialize_with = "string_or_absent")]
    pub refresh_token: Option<String>,
    #[serde(default, deserialize_with = "positive_integer_or_absent")]
    pub expires_in: Option<u64>,
}

impl TokenResponse {
    /// The new refresh token, when the response carried a non-empty one.
    pub fn new_refresh_token(&self) -> Option<&str> {
        non_empty(self.refresh_token.as_deref())
    }

    /// The new id token, when the response carried a non-empty one.
    pub fn new_id_token(&self) -> Option<&str> {
        non_empty(self.id_token.as_deref())
    }

    /// When the new access token expires, in epoch milliseconds: its `exp` claim when
    /// readable, which is how `auth.json` is read back, else `expires_in` seconds from
    /// `now_ms`, else an hour from `now_ms`.
    pub fn expires_at_ms(&self, now_ms: u64) -> u64 {
        token_exp_ms(&self.access_token).unwrap_or_else(|| {
            now_ms.saturating_add(self.expires_in.unwrap_or(3600).saturating_mul(1000))
        })
    }
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.filter(|v| !v.trim().is_empty())
}

fn string_or_absent<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(match serde_json::Value::deserialize(deserializer)? {
        serde_json::Value::String(value) => Some(value),
        _ => None,
    })
}

fn positive_integer_or_absent<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(serde_json::Value::deserialize(deserializer)?
        .as_u64()
        .filter(|secs| *secs > 0))
}

/// The id token claims the Codex CLI deserializes when it loads `tokens.id_token`
/// from `auth.json` (`IdClaims`, `ProfileClaims` and `AuthClaims` in
/// `codex-rs/login/src/token_data.rs` at rust-v0.166.0), with the same names, types
/// and defaults. Unknown claims are allowed there and here. The values are never read.
#[derive(Deserialize)]
#[allow(dead_code)]
struct CliIdClaims {
    #[serde(default)]
    email: Option<String>,
    #[serde(rename = "https://api.openai.com/profile", default)]
    profile: Option<CliProfileClaims>,
    #[serde(rename = "https://api.openai.com/auth", default)]
    auth: Option<CliAuthClaims>,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct CliProfileClaims {
    #[serde(default)]
    email: Option<String>,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct CliAuthClaims {
    /// The CLI's `PlanType` (`codex-rs/protocol/src/auth.rs` at rust-v0.166.0) is
    /// untagged over its known plans and any other string, so every string passes;
    /// only a string or null is taken here.
    #[serde(default)]
    chatgpt_plan_type: Option<String>,
    #[serde(default)]
    chatgpt_user_id: Option<String>,
    #[serde(default)]
    user_id: Option<String>,
    #[serde(default)]
    chatgpt_account_id: Option<String>,
    #[serde(default)]
    chatgpt_account_is_fedramp: bool,
}

/// Whether the Codex CLI can load `token` as `tokens.id_token` from `auth.json`. The
/// CLI takes the first three dot-separated parts, each non-empty, and ignores any
/// further one; it decodes the second as unpadded URL-safe base64 and deserializes it
/// into typed claims (`decode_jwt_payload` and `parse_chatgpt_jwt_claims` in
/// `codex-rs/login/src/token_data.rs` at rust-v0.166.0). A token that fails any step
/// would leave the CLI unable to load the file.
pub fn is_codex_cli_readable_jwt(token: &str) -> bool {
    let mut parts = token.split('.');
    let (Some(header), Some(payload), Some(signature)) = (parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    if header.is_empty() || payload.is_empty() || signature.is_empty() {
        return false;
    }
    use base64::Engine;
    let Ok(bytes) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload) else {
        return false;
    };
    serde_json::from_slice::<CliIdClaims>(&bytes).is_ok()
}

fn decode_jwt_payload(token: &str) -> Option<Vec<u8>> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let payload_b64 = parts[1].replace('-', "+").replace('_', "/");
    let padded = match payload_b64.len() % 4 {
        2 => format!("{payload_b64}=="),
        3 => format!("{payload_b64}="),
        _ => payload_b64,
    };
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(&padded)
        .ok()
}

fn parse_jwt_claims(token: &str) -> Option<IdTokenClaims> {
    serde_json::from_slice(&decode_jwt_payload(token)?).ok()
}

/// Decode a JWT's `exp` claim (seconds since the Unix epoch) into epoch milliseconds.
pub fn token_exp_ms(token: &str) -> Option<u64> {
    #[derive(Deserialize)]
    struct ExpClaim {
        exp: Option<u64>,
    }
    let claim: ExpClaim = serde_json::from_slice(&decode_jwt_payload(token)?).ok()?;
    claim.exp.map(|secs| secs.saturating_mul(1000))
}

fn extract_account_id_from_claims(claims: &IdTokenClaims) -> Option<String> {
    claims
        .chatgpt_account_id
        .clone()
        .or_else(|| claims.openai_auth.as_ref()?.chatgpt_account_id.clone())
        .or_else(|| claims.openai_chatgpt_account_id.clone())
        .or_else(|| claims.organizations.as_ref()?.first()?.id.clone().into())
}

pub fn validate_token_response(tokens: &TokenResponse) -> anyhow::Result<()> {
    if tokens.access_token.trim().is_empty() {
        anyhow::bail!("token response missing access token");
    }
    Ok(())
}

pub fn extract_account_id(tokens: &TokenResponse) -> Option<String> {
    if let Some(ref id_token) = tokens.id_token
        && let Some(claims) = parse_jwt_claims(id_token)
        && let Some(account_id) = extract_account_id_from_claims(&claims)
    {
        return Some(account_id);
    }
    let claims = parse_jwt_claims(&tokens.access_token)?;
    extract_account_id_from_claims(&claims)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_account_id_from_access_token() {
        let token = TokenResponse {
            id_token: None,
            access_token: "eyJhbGciOiJIUzI1NiJ9.eyJjaGF0Z3B0X2FjY291bnRfaWQiOiJhY2N0XzEyMyJ9.sig"
                .into(),
            refresh_token: Some("r".into()),
            expires_in: Some(3600),
        };
        assert_eq!(extract_account_id(&token), Some("acct_123".into()));
    }

    #[test]
    fn extract_account_id_from_id_token_takes_precedence() {
        let token = TokenResponse {
            id_token: Some(
                "eyJhbGciOiJIUzI1NiJ9.eyJjaGF0Z3B0X2FjY291bnRfaWQiOiJpZF9hY2N0In0.sig".into(),
            ),
            access_token: "eyJhbGciOiJIUzI1NiJ9.eyJjaGF0Z3B0X2FjY291bnRfaWQiOiJhY2NfYWNjIn0.sig"
                .into(),
            refresh_token: Some("r".into()),
            expires_in: Some(3600),
        };
        assert_eq!(extract_account_id(&token), Some("id_acct".into()));
    }

    #[test]
    fn extract_account_id_returns_none_for_invalid_token() {
        let token = TokenResponse {
            id_token: None,
            access_token: "invalid".into(),
            refresh_token: Some("r".into()),
            expires_in: None,
        };
        assert_eq!(extract_account_id(&token), None);
    }

    #[test]
    fn validate_token_response_rejects_empty_access_token() {
        let token = TokenResponse {
            access_token: "".into(),
            refresh_token: Some("r".into()),
            expires_in: Some(3600),
            id_token: None,
        };
        assert!(validate_token_response(&token).is_err());
        assert!(
            validate_token_response(&token)
                .unwrap_err()
                .to_string()
                .contains("missing access token")
        );
    }

    #[test]
    fn validate_token_response_accepts_absent_or_empty_refresh_token() {
        let token: TokenResponse = serde_json::from_str(r#"{"access_token":"a"}"#).unwrap();
        assert!(validate_token_response(&token).is_ok());
        assert_eq!(token.new_refresh_token(), None);
        assert_eq!(token.new_id_token(), None);

        let token: TokenResponse =
            serde_json::from_str(r#"{"access_token":"a","refresh_token":"","id_token":null}"#)
                .unwrap();
        assert!(validate_token_response(&token).is_ok());
        assert_eq!(token.new_refresh_token(), None);
        assert_eq!(token.new_id_token(), None);
    }

    #[test]
    fn token_response_requires_access_token() {
        assert!(serde_json::from_str::<TokenResponse>(r#"{"refresh_token":"r"}"#).is_err());
    }

    /// A toy JWT whose payload is `payload`, in unpadded URL-safe base64.
    fn jwt_with_payload(payload: &str) -> String {
        use base64::Engine;
        format!(
            "eyJhbGciOiJub25lIn0.{}.sig",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload)
        )
    }

    #[test]
    fn codex_cli_readable_jwt_checks_the_envelope_and_the_claim_types() {
        // {"email":"user@example.com"} in unpadded URL-safe base64.
        assert!(is_codex_cli_readable_jwt(
            "eyJhbGciOiJub25lIn0.eyJlbWFpbCI6InVzZXJAZXhhbXBsZS5jb20ifQ.sig"
        ));
        assert!(!is_codex_cli_readable_jwt("not-a-jwt"));
        assert!(!is_codex_cli_readable_jwt("a.b"));
        assert!(!is_codex_cli_readable_jwt(".eyJ9.sig"));
        assert!(!is_codex_cli_readable_jwt("eyJhbGciOiJub25lIn0.eyJ9.")); // empty signature
        // Padded base64 is rejected by an unpadded decoder.
        assert!(!is_codex_cli_readable_jwt("h.e30=.sig"));
        // A payload that decodes to a JSON string, not claims.
        assert!(!is_codex_cli_readable_jwt("h.ImEi.sig"));
        // The CLI reads the first three parts and ignores a fourth.
        assert!(is_codex_cli_readable_jwt(&format!(
            "{}.extra",
            jwt_with_payload("{}")
        )));
        assert!(!is_codex_cli_readable_jwt("a.b.c.d"));

        let readable = [
            "{}",
            r#"{"email":null,"unknown":[1,{"x":2}]}"#,
            r#"{"https://api.openai.com/profile":{"email":"p@example.com","name":"kept"}}"#,
            r#"{"https://api.openai.com/auth":null}"#,
            r#"{"https://api.openai.com/auth":{
                "chatgpt_plan_type":"plus","chatgpt_user_id":"u","user_id":"u2",
                "chatgpt_account_id":"acct","chatgpt_account_is_fedramp":true,"other":5}}"#,
            r#"{"https://api.openai.com/auth":{"chatgpt_plan_type":"a-plan-not-listed"}}"#,
            r#"{"https://api.openai.com/auth":{"chatgpt_plan_type":null}}"#,
        ];
        for payload in readable {
            assert!(
                is_codex_cli_readable_jwt(&jwt_with_payload(payload)),
                "{payload}"
            );
        }

        let unreadable = [
            r#"{"email":42}"#,
            r#"{"email":"a","email":"b"}"#,
            r#"{"https://api.openai.com/profile":"p@example.com"}"#,
            r#"{"https://api.openai.com/profile":{"email":["p@example.com"]}}"#,
            r#"{"https://api.openai.com/auth":"acct"}"#,
            r#"{"https://api.openai.com/auth":{"chatgpt_plan_type":5}}"#,
            r#"{"https://api.openai.com/auth":{"chatgpt_plan_type":{"name":"plus"}}}"#,
            r#"{"https://api.openai.com/auth":{"chatgpt_account_is_fedramp":"true"}}"#,
            r#"{"https://api.openai.com/auth":{"chatgpt_account_is_fedramp":null}}"#,
            r#"{"https://api.openai.com/auth":{"chatgpt_user_id":7}}"#,
            r#"{"https://api.openai.com/auth":{"user_id":false}}"#,
            r#"{"https://api.openai.com/auth":{"chatgpt_account_id":{"id":"acct"}}}"#,
            "null",
            "5",
        ];
        for payload in unreadable {
            assert!(
                !is_codex_cli_readable_jwt(&jwt_with_payload(payload)),
                "{payload}"
            );
        }
    }

    #[test]
    fn validate_token_response_accepts_zero_expires_in() {
        let token: TokenResponse =
            serde_json::from_str(r#"{"access_token":"a","expires_in":0}"#).unwrap();
        assert!(validate_token_response(&token).is_ok());
        assert_eq!(token.expires_in, None);
    }

    #[test]
    fn token_response_treats_wrongly_typed_optional_fields_as_absent() {
        for body in [
            r#"{"access_token":"a","expires_in":"3600","id_token":5,"refresh_token":{"r":1}}"#,
            r#"{"access_token":"a","expires_in":1.5,"id_token":["x"],"refresh_token":true}"#,
            r#"{"access_token":"a","expires_in":-30,"id_token":{},"refresh_token":7}"#,
        ] {
            let token: TokenResponse = serde_json::from_str(body).unwrap();
            assert!(validate_token_response(&token).is_ok(), "{body}");
            assert_eq!(token.expires_in, None, "{body}");
            assert_eq!(token.new_id_token(), None, "{body}");
            assert_eq!(token.new_refresh_token(), None, "{body}");
        }
        // A wrongly typed access token is still a malformed response.
        assert!(serde_json::from_str::<TokenResponse>(r#"{"access_token":5}"#).is_err());
    }

    #[test]
    fn token_response_expiry_prefers_the_access_token_claim() {
        let now = 1_000_000;
        // {"exp":4102444800} (year 2100) wins over expires_in.
        let token: TokenResponse = serde_json::from_str(
            r#"{"access_token":"eyJhbGciOiJub25lIn0.eyJleHAiOjQxMDI0NDQ4MDB9.sig","expires_in":60}"#,
        )
        .unwrap();
        assert_eq!(token.expires_at_ms(now), 4_102_444_800_000);

        let token: TokenResponse =
            serde_json::from_str(r#"{"access_token":"opaque","expires_in":60}"#).unwrap();
        assert_eq!(token.expires_at_ms(now), now + 60_000);

        let token: TokenResponse =
            serde_json::from_str(r#"{"access_token":"opaque","expires_in":0}"#).unwrap();
        assert_eq!(token.expires_at_ms(now), now + 3_600_000);

        // A huge expires_in saturates instead of overflowing.
        let token: TokenResponse =
            serde_json::from_str(r#"{"access_token":"opaque","expires_in":18446744073709551615}"#)
                .unwrap();
        assert_eq!(token.expires_at_ms(now), u64::MAX);
    }

    #[test]
    fn validate_token_response_accepts_valid() {
        let token = TokenResponse {
            access_token: "a".into(),
            refresh_token: Some("r".into()),
            expires_in: Some(3600),
            id_token: None,
        };
        assert!(validate_token_response(&token).is_ok());
    }

    #[test]
    fn validate_token_response_accepts_no_expires_in() {
        let token = TokenResponse {
            access_token: "a".into(),
            refresh_token: Some("r".into()),
            expires_in: None,
            id_token: None,
        };
        assert!(validate_token_response(&token).is_ok());
    }
}

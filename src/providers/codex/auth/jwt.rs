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

/// A token endpoint answer. Only `access_token` is required; an absent, `null` or
/// empty `refresh_token` or `id_token` means the response carried none.
#[derive(Debug, Deserialize)]
pub struct TokenResponse {
    #[serde(default)]
    pub id_token: Option<String>,
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
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
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.filter(|v| !v.trim().is_empty())
}

/// Whether the Codex CLI can decode `token` when it loads `tokens.id_token` from
/// `auth.json`: three non-empty dot-separated parts and a payload that is unpadded
/// URL-safe base64 of a JSON object (`codex-rs/login/src/token_data.rs` at
/// rust-v0.166.0). A token that fails this would leave the CLI unable to load the file.
pub fn is_codex_cli_readable_jwt(token: &str) -> bool {
    let mut parts = token.split('.');
    let (Some(header), Some(payload), Some(signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
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
    matches!(
        serde_json::from_slice::<serde_json::Value>(&bytes),
        Ok(serde_json::Value::Object(_))
    )
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
    if matches!(tokens.expires_in, Some(0)) {
        anyhow::bail!("token response has invalid expiration");
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

    #[test]
    fn codex_cli_readable_jwt_matches_the_cli_decoder() {
        // {"email":"user@example.com"} in unpadded URL-safe base64.
        assert!(is_codex_cli_readable_jwt(
            "eyJhbGciOiJub25lIn0.eyJlbWFpbCI6InVzZXJAZXhhbXBsZS5jb20ifQ.sig"
        ));
        assert!(!is_codex_cli_readable_jwt("not-a-jwt"));
        assert!(!is_codex_cli_readable_jwt("a.b"));
        assert!(!is_codex_cli_readable_jwt("a.b.c.d"));
        assert!(!is_codex_cli_readable_jwt(".eyJ9.sig"));
        assert!(!is_codex_cli_readable_jwt("eyJhbGciOiJub25lIn0.eyJ9.")); // empty signature
        // Padded base64 is rejected by an unpadded decoder.
        assert!(!is_codex_cli_readable_jwt("h.e30=.sig"));
        // A payload that decodes to a JSON string, not an object.
        assert!(!is_codex_cli_readable_jwt("h.ImEi.sig"));
    }

    #[test]
    fn validate_token_response_rejects_zero_expires_in() {
        let token = TokenResponse {
            access_token: "a".into(),
            refresh_token: Some("r".into()),
            expires_in: Some(0),
            id_token: None,
        };
        assert!(validate_token_response(&token).is_err());
        assert!(
            validate_token_response(&token)
                .unwrap_err()
                .to_string()
                .contains("invalid expiration")
        );
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

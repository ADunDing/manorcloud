//! 與身分提供者（Authentik 等）的 OpenID Connect 協定：只負責協定本身，不碰資料庫與 Cookie。
//!
//! 流程（Authorization Code + PKCE）：
//! 1. `begin`：產生登入網址，以及三個一次性的值：
//!    - state：綁定這個瀏覽器（存在 Cookie），回來時核對，防止別人把自己的登入結果塞給你
//!    - nonce：寫進 ID token，回來時核對，防止舊的 ID token 被重放
//!    - PKCE verifier：交換 token 時證明「發起登入的就是我」，防止授權碼被攔截後冒用
//! 2. 使用者在身分提供者登入，被帶回 ManorCloud 的回呼網址，網址中有授權碼
//! 3. `finish`：用授權碼向身分提供者換 token，驗證 ID token 的簽章、發行者、對象、到期時間與 nonce
//!
//! 密碼學與格式的細節交給成熟的 openidconnect 套件，這裡不自己實作。
//! 每次都重新讀取身分提供者的設定（discovery）：家用規模的登入次數很少，
//! 不快取就不必處理身分提供者換金鑰時的快取失效。

use std::fmt;

use openidconnect::{
    AuthorizationCode, ClientId, ClientSecret, CsrfToken, IssuerUrl, Nonce,
    OAuth2TokenResponse as _, PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope,
    TokenResponse as _,
    core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata},
};

/// 連線到身分提供者需要的設定
pub struct ProviderConfig<'a> {
    pub issuer_url: &'a str,
    pub client_id: &'a str,
    pub client_secret: &'a str,
    pub redirect_uri: &'a str,
}

/// 開始登入的結果：把使用者送到 `url`，其餘三個值保存起來，回來時核對
pub struct Begin {
    pub url: String,
    pub state: String,
    pub nonce: String,
    pub pkce_verifier: String,
}

/// 驗證通過的身分
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// 發行者，與設定的 issuer 相同（openidconnect 已核對）
    pub issuer: String,
    /// 身分提供者中的使用者編號：不會改變，是對應帳號的唯一依據
    pub subject: String,
    /// 建議的帳號名稱：只在自動建立帳號時參考，不用於辨識
    pub preferred_username: Option<String>,
    pub email: Option<String>,
}

/// 失敗的原因。只記在 log；對使用者一律顯示通用的訊息
#[derive(Debug)]
pub enum OidcError {
    /// 讀不到身分提供者的設定（網址錯誤、連不上、憑證不被信任）
    Discovery(String),
    /// 設定的值格式不正確
    Config(String),
    /// 用授權碼交換 token 失敗（授權碼錯誤或過期、client secret 錯誤）
    Exchange(String),
    /// ID token 不存在或驗證失敗（簽章、發行者、對象、到期、nonce）
    InvalidToken(String),
}

impl fmt::Display for OidcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OidcError::Discovery(e) => write!(f, "cannot read the provider configuration: {e}"),
            OidcError::Config(e) => write!(f, "invalid SSO setting: {e}"),
            OidcError::Exchange(e) => write!(f, "token exchange failed: {e}"),
            OidcError::InvalidToken(e) => write!(f, "invalid ID token: {e}"),
        }
    }
}

impl std::error::Error for OidcError {}

/// 讀取身分提供者的設定，確認它可以使用。用於 Administration → Sign-in 的「Test connection」
pub async fn check(http: &reqwest::Client, issuer_url: &str) -> Result<(), OidcError> {
    discover(http, issuer_url).await.map(|_| ())
}

/// 產生登入網址與三個一次性的值
pub async fn begin(
    http: &reqwest::Client,
    config: &ProviderConfig<'_>,
) -> Result<Begin, OidcError> {
    let metadata = discover(http, config.issuer_url).await?;
    let client = CoreClient::from_provider_metadata(
        metadata,
        ClientId::new(config.client_id.to_string()),
        Some(ClientSecret::new(config.client_secret.to_string())),
    )
    .set_redirect_uri(redirect_url(config)?);

    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let (url, state, nonce) = client
        .authorize_url(
            CoreAuthenticationFlow::AuthorizationCode,
            CsrfToken::new_random,
            Nonce::new_random,
        )
        // openid 由套件自動加上；profile 提供建議的帳號名稱，email 作為沒有名稱時的備案
        .add_scope(Scope::new("profile".to_string()))
        .add_scope(Scope::new("email".to_string()))
        .set_pkce_challenge(challenge)
        .url();

    Ok(Begin {
        url: url.to_string(),
        state: state.secret().clone(),
        nonce: nonce.secret().clone(),
        pkce_verifier: verifier.secret().clone(),
    })
}

/// 用授權碼換 token，驗證 ID token，回傳身分
pub async fn finish(
    http: &reqwest::Client,
    config: &ProviderConfig<'_>,
    code: &str,
    nonce: &str,
    pkce_verifier: &str,
) -> Result<Identity, OidcError> {
    let metadata = discover(http, config.issuer_url).await?;
    let client = CoreClient::from_provider_metadata(
        metadata,
        ClientId::new(config.client_id.to_string()),
        Some(ClientSecret::new(config.client_secret.to_string())),
    )
    .set_redirect_uri(redirect_url(config)?);

    let token = client
        .exchange_code(AuthorizationCode::new(code.to_string()))
        .map_err(|e| OidcError::Discovery(format!("the provider has no token endpoint: {e}")))?
        .set_pkce_verifier(PkceCodeVerifier::new(pkce_verifier.to_string()))
        .request_async(http)
        .await
        .map_err(|e| OidcError::Exchange(error_chain(&e)))?;
    // access token 用不到：ManorCloud 只需要知道「這個人是誰」
    let _ = token.access_token();

    let id_token = token
        .id_token()
        .ok_or_else(|| OidcError::InvalidToken("the response has no ID token".to_string()))?;
    // 驗證簽章（身分提供者公布的金鑰）、發行者、對象（我們的 client ID）、到期時間與 nonce
    let claims = id_token
        .claims(&client.id_token_verifier(), &Nonce::new(nonce.to_string()))
        .map_err(|e| OidcError::InvalidToken(error_chain(&e)))?;

    Ok(Identity {
        issuer: claims.issuer().as_str().to_string(),
        subject: claims.subject().as_str().to_string(),
        preferred_username: claims
            .preferred_username()
            .map(|name| name.as_str().to_string()),
        email: claims.email().map(|email| email.as_str().to_string()),
    })
}

/// 讀取身分提供者的設定：{issuer}/.well-known/openid-configuration 與它的金鑰
async fn discover(
    http: &reqwest::Client,
    issuer_url: &str,
) -> Result<CoreProviderMetadata, OidcError> {
    let issuer = IssuerUrl::new(issuer_url.to_string())
        .map_err(|e| OidcError::Config(format!("issuer URL: {e}")))?;
    CoreProviderMetadata::discover_async(issuer, http)
        .await
        .map_err(|e| OidcError::Discovery(error_chain(&e)))
}

fn redirect_url(config: &ProviderConfig<'_>) -> Result<RedirectUrl, OidcError> {
    RedirectUrl::new(config.redirect_uri.to_string())
        .map_err(|e| OidcError::Config(format!("redirect URI: {e}")))
}

/// 把錯誤與它所有的來源串成一行：連線失敗時，真正的原因（例如憑證不被信任）在最裡層
fn error_chain(e: &dyn std::error::Error) -> String {
    let mut parts = vec![e.to_string()];
    let mut source = e.source();
    while let Some(s) = source {
        parts.push(s.to_string());
        source = s.source();
    }
    parts.join(" <- ")
}

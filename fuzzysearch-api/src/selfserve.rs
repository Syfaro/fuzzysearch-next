use std::borrow::Cow;

use askama::Template;
use askama_axum::IntoResponse;
use axum::{
    async_trait,
    extract::FromRequestParts,
    http::{request::Parts, HeaderValue},
    routing, Extension, Form, Router,
};
use eyre::ContextCompat;
use pasetors::{
    claims::{Claims, ClaimsValidationRules},
    keys::SymmetricKey,
    token::UntrustedToken,
};
use rand::distributions::DistString;
use reqwest::StatusCode;
use serde::Deserialize;
use sqlx::PgPool;

type Response = axum::response::Response;

#[derive(Clone)]
pub struct Config {
    pub secret: Vec<u8>,

    pub passwordless_public: String,
    pub passwordless_secret: String,
}

pub fn router() -> Router {
    Router::new()
        .route("/", routing::get(index))
        .nest(
            "/auth",
            Router::new()
                .route("/", routing::post(auth_form))
                .route("/register", routing::post(auth_register))
                .route("/register/complete", routing::post(auth_register_complete))
                .route("/login", routing::post(auth_login)),
        )
        .nest(
            "/key",
            Router::new()
                .route("/create", routing::post(key_create))
                .route("/delete", routing::post(key_delete)),
        )
}

struct HxError(eyre::Report);

impl<R> From<R> for HxError
where
    R: Into<eyre::Report>,
{
    fn from(err: R) -> Self {
        HxError(err.into())
    }
}

#[derive(Template)]
#[template(path = "selfserve/error.html")]
struct ErrorTemplate {
    message: String,
}

impl IntoResponse for HxError {
    fn into_response(self) -> Response {
        tracing::error!("building error for client: {}", self.0);

        let message = self.0.to_string();

        let mut resp = ErrorTemplate { message }.into_response();
        *resp.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;

        let headers = resp.headers_mut();
        headers.insert("hx-error", HeaderValue::from_static("true"));
        headers.insert("hx-retarget", HeaderValue::from_static("body"));

        resp
    }
}

#[derive(Template)]
#[template(path = "selfserve/alert.html")]
struct AlertTemplate<'a> {
    classes: Cow<'a, str>,
    message: Cow<'a, str>,
}

impl<'a> AlertTemplate<'a> {
    pub fn new<M, C>(message: M, classes: C) -> Self
    where
        M: Into<Cow<'a, str>>,
        C: Into<Cow<'a, str>>,
    {
        Self {
            classes: classes.into(),
            message: message.into(),
        }
    }
}

#[derive(Template)]
#[template(path = "selfserve/api_keys.html")]
struct ApiKeysTemplate<'a> {
    alert: Option<AlertTemplate<'a>>,
    api_keys: Vec<crate::db::UserApiKey>,
    can_create_key: bool,
}

async fn api_keys_resp<'a>(
    pool: &PgPool,
    user_id: i32,
    alert: Option<AlertTemplate<'a>>,
) -> Result<ApiKeysTemplate<'a>, HxError> {
    let api_keys = sqlx::query_file_as!(
        crate::db::UserApiKey,
        "queries/selfserve/lookup_user_api_keys.sql",
        user_id
    )
    .fetch_all(pool)
    .await?;

    let can_create_key = api_keys.len() < 3;

    Ok(ApiKeysTemplate {
        alert,
        api_keys,
        can_create_key,
    })
}

fn token_for_user(secret: &[u8], user_id: i32) -> eyre::Result<String> {
    let mut claims = Claims::new()?;
    claims.issuer("fuzzysearch.net")?;
    claims.audience("fuzzysearch.net")?;
    claims.subject("selfservice")?;
    claims.add_additional("user_id", user_id)?;

    let sk = SymmetricKey::from(secret)?;
    let token = pasetors::local::encrypt(&sk, &claims, None, None)?;

    Ok(token)
}

#[derive(Template)]
#[template(path = "selfserve/index.html")]
struct IndexTemplate<'a> {
    auth_form: AuthFormTemplate<'a>,
    passwordless_public: &'a str,
}

async fn index(Extension(config): Extension<Config>) -> Response {
    IndexTemplate {
        auth_form: Default::default(),
        passwordless_public: &config.passwordless_public,
    }
    .into_response()
}

#[derive(Deserialize)]
struct AuthForm {
    username: String,
}

#[derive(Default, Clone, PartialEq, Eq)]
enum AuthFormState<'a> {
    #[default]
    Empty,
    Error(Cow<'a, str>),
    UnknownUsername,
    KnownUsername,
}

#[derive(Default, Template)]
#[template(path = "selfserve/auth_form.html")]
struct AuthFormTemplate<'a> {
    state: AuthFormState<'a>,
    username: &'a str,
}

impl AuthFormTemplate<'_> {
    fn action(&self) -> &'static str {
        match self.state {
            AuthFormState::Empty | AuthFormState::Error(_) => "/selfserve/auth",
            AuthFormState::KnownUsername => "/selfserve/auth/signin",
            AuthFormState::UnknownUsername => "/selfserve/auth/register",
        }
    }

    fn username_attrs(&self) -> &'static str {
        match self.state {
            AuthFormState::Empty | AuthFormState::Error(_) => "class=form-control",
            AuthFormState::KnownUsername => "readonly class=form-control-plaintext",
            AuthFormState::UnknownUsername => "readonly class=form-control",
        }
    }
}

async fn auth_form(
    Extension(pool): Extension<PgPool>,
    Form(form): Form<AuthForm>,
) -> Result<Response, HxError> {
    let state = if form.username.len() < 5 || form.username.len() > 250 {
        AuthFormState::Error("Invalid username length.".into())
    } else {
        let user_id =
            sqlx::query_file_scalar!("queries/selfserve/lookup_username.sql", form.username)
                .fetch_optional(&pool)
                .await?;

        user_id
            .map(|_id| AuthFormState::KnownUsername)
            .unwrap_or(AuthFormState::UnknownUsername)
    };

    let should_perform_login = matches!(state, AuthFormState::KnownUsername);

    let mut resp = AuthFormTemplate {
        state,
        username: &form.username,
    }
    .into_response();

    if should_perform_login {
        let event = serde_json::json!({
            "performLogin": form.username,
        })
        .to_string();

        resp.headers_mut()
            .insert("hx-trigger", HeaderValue::try_from(event)?);
    }

    Ok(resp)
}

async fn auth_register(
    Extension(config): Extension<Config>,
    Extension(pool): Extension<PgPool>,
    Extension(client): Extension<reqwest::Client>,
    Form(form): Form<AuthForm>,
) -> Result<Response, HxError> {
    let user_id = sqlx::query_file_scalar!("queries/selfserve/insert_account.sql", form.username)
        .fetch_one(&pool)
        .await?;

    let payload = serde_json::json!({
        "userId": user_id,
        "username": form.username,
        "displayName": form.username,
        "aliases": [form.username],
    });

    let token: String = client
        .post("https://v3.passwordless.dev/register/token")
        .header("ApiSecret", config.passwordless_secret)
        .json(&payload)
        .send()
        .await?
        .text()
        .await?;

    let event = serde_json::json!({
        "performRegistration": token,
    })
    .to_string();

    let mut resp = AlertTemplate::new(
        "Account created, please perform WebAuthn registration.",
        "alert alert-success",
    )
    .into_response();

    let token = token_for_user(&config.secret, user_id)?;
    resp.headers_mut()
        .insert("x-user-token", HeaderValue::try_from(token)?);

    resp.headers_mut()
        .insert("hx-trigger", HeaderValue::try_from(event)?);

    Ok(resp)
}

async fn auth_register_complete(
    Extension(pool): Extension<PgPool>,
    HxUser(user_id): HxUser,
) -> Result<Response, HxError> {
    sqlx::query_file!("queries/selfserve/update_account_completed.sql", user_id)
        .execute(&pool)
        .await?;

    let resp = api_keys_resp(&pool, user_id, None).await.into_response();
    Ok(resp)
}

#[derive(Deserialize)]
struct AuthLoginForm {
    token: String,
}

async fn auth_login(
    Extension(config): Extension<Config>,
    Extension(pool): Extension<PgPool>,
    Extension(client): Extension<reqwest::Client>,
    Form(form): Form<AuthLoginForm>,
) -> Result<Response, HxError> {
    let payload = serde_json::json!({
        "token": form.token,
    });

    let data: serde_json::Value = client
        .post("https://v3.passwordless.dev/signin/verify")
        .json(&payload)
        .header("ApiSecret", config.passwordless_secret)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    if !data["success"].as_bool().unwrap_or(false) {
        return Err(eyre::eyre!("Verify was not successful").into());
    }

    let user_id: i32 = match data["userId"].as_str().and_then(|id| id.parse().ok()) {
        Some(id) => id,
        _ => return Err(eyre::eyre!("Login was missing userId").into()),
    };

    let mut resp = api_keys_resp(&pool, user_id, None).await.into_response();

    let token = token_for_user(&config.secret, user_id)?;
    resp.headers_mut()
        .insert("x-user-token", HeaderValue::try_from(token)?);

    Ok(resp)
}

struct HxUser(i32);

impl HxUser {
    fn validation_rules() -> ClaimsValidationRules {
        let mut claims = ClaimsValidationRules::new();

        claims.validate_issuer_with("fuzzysearch.net");
        claims.validate_audience_with("fuzzysearch.net");
        claims.validate_subject_with("selfservice");

        claims
    }
}

#[async_trait]
impl<S> FromRequestParts<S> for HxUser
where
    S: Send + Sync,
{
    type Rejection = HxError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let config = parts
            .extensions
            .get::<Config>()
            .ok_or_else(|| eyre::eyre!("missing selfserve config"))?;
        let sk = SymmetricKey::from(&config.secret)?;

        let token = match parts
            .headers
            .get("x-user-token")
            .and_then(|val| val.to_str().ok())
        {
            Some(token) => token,
            None => return Err(eyre::eyre!("`x-user-token` header is missing or invalid").into()),
        };
        let untrusted_token = UntrustedToken::try_from(token)?;
        let trusted_token =
            pasetors::local::decrypt(&sk, &untrusted_token, &Self::validation_rules(), None, None)?;

        let claims = trusted_token
            .payload_claims()
            .context("missing payload claims")?;
        let user_id = claims
            .get_claim("user_id")
            .context("missing user id")?
            .as_i64()
            .ok_or_else(|| eyre::eyre!("user id was not i32"))? as i32;

        Ok(Self(user_id))
    }
}

struct HxPrompt(String);

#[async_trait]
impl<S> FromRequestParts<S> for HxPrompt
where
    S: Send + Sync,
{
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        if let Some(hx_prompt) = parts
            .headers
            .get("hx-prompt")
            .and_then(|val| val.to_str().ok())
        {
            Ok(Self(hx_prompt.to_owned()))
        } else {
            Err((
                StatusCode::BAD_REQUEST,
                "`hx-prompt` header is missing or invalid",
            ))
        }
    }
}

async fn key_create(
    Extension(pool): Extension<PgPool>,
    HxUser(user_id): HxUser,
    prompt: Option<HxPrompt>,
) -> Result<Response, HxError> {
    let count = sqlx::query_file_scalar!("queries/selfserve/count_user_api_keys.sql", user_id)
        .fetch_optional(&pool)
        .await?
        .unwrap_or_default()
        .unwrap_or_default();

    if count >= 3 {
        return Ok(api_keys_resp(
            &pool,
            user_id,
            Some(AlertTemplate::new(
                "Too many existing API keys.",
                "alert alert-danger",
            )),
        )
        .await?
        .into_response());
    }

    let name = prompt
        .map(|prompt| prompt.0)
        .map(|name| {
            name.chars()
                .filter(char::is_ascii)
                .take(50)
                .collect::<String>()
        })
        .filter(|s| !s.is_empty());

    let key = rand::distributions::Alphanumeric.sample_string(&mut rand::thread_rng(), 48);
    let key = format!("fzs1-{key}");

    sqlx::query_file!("queries/selfserve/insert_api_key.sql", user_id, name, key)
        .execute(&pool)
        .await?;

    let name = name.as_deref().unwrap_or("unnamed");

    let resp = api_keys_resp(
        &pool,
        user_id,
        Some(AlertTemplate::new(
            format!("Created API key {name}."),
            "alert alert-success",
        )),
    )
    .await?
    .into_response();
    Ok(resp)
}

#[derive(Deserialize)]
struct KeyDeleteForm {
    key_id: i32,
}

async fn key_delete(
    Extension(pool): Extension<PgPool>,
    HxUser(user_id): HxUser,
    Form(form): Form<KeyDeleteForm>,
) -> Result<Response, HxError> {
    let name = sqlx::query_file_scalar!("queries/selfserve/delete_api_key.sql", form.key_id)
        .fetch_one(&pool)
        .await?;

    let name = name.as_deref().unwrap_or("unnamed");

    let resp = api_keys_resp(
        &pool,
        user_id,
        Some(AlertTemplate::new(
            format!("Deleted API key {name}."),
            "alert alert-success",
        )),
    )
    .await?
    .into_response();
    Ok(resp)
}

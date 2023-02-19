use std::{borrow::Cow, sync::Arc};

use askama::Template;
use askama_axum::IntoResponse;
use axum::{
    async_trait,
    extract::FromRequestParts,
    http::{request::Parts, HeaderValue},
    routing, Extension, Form, Router,
};
use axum_sessions::extractors::{ReadableSession, WritableSession};
use eyre::ContextCompat;
use rand::distributions::DistString;
use reqwest::StatusCode;
use serde::Deserialize;
use sqlx::{types::Uuid, PgPool};
use webauthn_rs::{
    prelude::{Credential, Passkey},
    Webauthn,
};
use webauthn_rs_proto::UserVerificationPolicy;

type Response = axum::response::Response;

pub fn router() -> Router {
    Router::new()
        .route("/", routing::get(index))
        .nest(
            "/auth",
            Router::new()
                .route("/", routing::post(auth_form))
                .route("/register/start", routing::post(register_start))
                .route("/register/finish", routing::post(register_finish))
                .route("/login/finish", routing::post(login_finish)),
        )
        .nest(
            "/key",
            Router::new()
                .route("/create", routing::post(key_create))
                .route("/delete", routing::post(key_delete)),
        )
}

struct HxError(eyre::Report);

impl std::fmt::Display for HxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

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

#[tracing::instrument(skip(pool, alert))]
async fn api_keys_resp<'a>(
    pool: &PgPool,
    user_id: Uuid,
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

    tracing::debug!(keys = api_keys.len(), can_create_key, "found user api keys");

    Ok(ApiKeysTemplate {
        alert,
        api_keys,
        can_create_key,
    })
}

#[tracing::instrument(skip(pool))]
async fn credentials_for_user(pool: &PgPool, username: &str) -> Result<Vec<Passkey>, HxError> {
    let passkeys: Vec<Passkey> = sqlx::query_file_scalar!(
        "queries/selfserve/lookup_credentials_for_user.sql",
        username
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .filter_map(|cred| serde_json::from_value(cred).ok())
    .collect();

    tracing::debug!(len = passkeys.len(), "found passkeys for username");

    Ok(passkeys)
}

#[derive(Template)]
#[template(path = "selfserve/index.html")]
struct IndexTemplate<'a> {
    auth_form: AuthFormTemplate<'a>,
}

async fn index() -> Response {
    IndexTemplate {
        auth_form: Default::default(),
    }
    .into_response()
}

#[derive(Deserialize)]
struct AuthForm {
    username: Option<String>,
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
            AuthFormState::KnownUsername => "/selfserve/auth/login/start",
            AuthFormState::UnknownUsername => "/selfserve/auth/register/start",
        }
    }

    fn username_attrs(&self) -> &'static str {
        match self.state {
            AuthFormState::Empty | AuthFormState::Error(_) => "class=form-control",
            AuthFormState::KnownUsername => "readonly class=form-control-plaintext",
            AuthFormState::UnknownUsername => "readonly class=form-control",
        }
    }

    fn message_attrs(&self) -> &'static str {
        match self.state {
            AuthFormState::Empty | AuthFormState::UnknownUsername => "",
            _ => "hidden",
        }
    }
}

#[tracing::instrument(err, skip_all)]
async fn auth_form(
    Extension(webauthn): Extension<Arc<Webauthn>>,
    Extension(pool): Extension<PgPool>,
    mut session: WritableSession,
    Form(form): Form<AuthForm>,
) -> Result<Response, HxError> {
    let username = form.username.unwrap_or_default();
    let state = if username.len() < 5 || username.len() > 250 {
        AuthFormState::Error("Invalid username length.".into())
    } else {
        let user_id = sqlx::query_file_scalar!("queries/selfserve/lookup_username.sql", username)
            .fetch_optional(&pool)
            .await?;

        user_id
            .map(|_id| AuthFormState::KnownUsername)
            .unwrap_or(AuthFormState::UnknownUsername)
    };

    let should_perform_login = matches!(state, AuthFormState::KnownUsername);

    let mut resp = AuthFormTemplate {
        state,
        username: &username,
    }
    .into_response();

    if should_perform_login {
        tracing::info!("user should perform login");

        let creds = credentials_for_user(&pool, &username).await?;
        let (mut rcr, passkey_auth) = webauthn.start_passkey_authentication(&creds)?;
        if creds
            .iter()
            .cloned()
            .any(|cred| Credential::from(cred).user_verified)
        {
            tracing::info!("passkey was created with verification, requiring");
            rcr.public_key.user_verification = UserVerificationPolicy::Required;
        }

        session.insert("auth_state", passkey_auth)?;

        let event = serde_json::json!({
            "performLogin": {
                "rcr": rcr,
            },
        })
        .to_string();

        resp.headers_mut()
            .insert("hx-trigger", HeaderValue::try_from(event)?);
    }

    Ok(resp)
}

struct HxUser(Uuid);

#[async_trait]
impl<S> FromRequestParts<S> for HxUser
where
    S: Send + Sync,
{
    type Rejection = HxError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let session = ReadableSession::from_request_parts(parts, state).await?;

        let user_id = session.get("user_id").context("missing user_id")?;
        tracing::info!(%user_id, "found user from request");

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

#[tracing::instrument(err, skip_all)]
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

    tracing::info!(name, count = count + 1, "created new api key");

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

#[tracing::instrument(err, skip_all)]
async fn key_delete(
    Extension(pool): Extension<PgPool>,
    HxUser(user_id): HxUser,
    Form(form): Form<KeyDeleteForm>,
) -> Result<Response, HxError> {
    let name = sqlx::query_file_scalar!("queries/selfserve/delete_api_key.sql", form.key_id)
        .fetch_one(&pool)
        .await?;

    let name = name.as_deref().unwrap_or("unnamed");

    tracing::info!(name, "deleted api key");

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

fn filter_name_to_len(s: &str, n: usize) -> String {
    s.chars().filter(char::is_ascii).take(n).collect::<String>()
}

#[tracing::instrument(err, skip_all)]
async fn register_start(
    Extension(webauthn): Extension<Arc<Webauthn>>,
    Extension(pool): Extension<PgPool>,
    mut session: WritableSession,
    Form(form): Form<AuthForm>,
) -> Result<Response, HxError> {
    let username = filter_name_to_len(&form.username.unwrap_or_default(), 24);
    if username.len() < 5 {
        return Ok(AuthFormTemplate {
            state: AuthFormState::Error("Username must be greater than 5 characters.".into()),
            username: &username,
        }
        .into_response());
    }

    let mut tx = pool.begin().await?;

    let user_id = sqlx::query_file_scalar!("queries/selfserve/insert_account.sql", username)
        .fetch_one(&mut tx)
        .await?;

    let (ccr, reg_state) =
        webauthn.start_passkey_registration(user_id, &username, &username, None)?;

    session.insert("user_id", user_id)?;
    session.insert("reg_state", reg_state)?;

    tx.commit().await?;

    tracing::info!(%user_id, "created new account");

    let event = serde_json::json!({
        "performRegistration": {
            "ccr": ccr,
        },
    })
    .to_string();

    let mut resp = AlertTemplate::new(
        "Account created, please perform WebAuthn registration.",
        "alert alert-success",
    )
    .into_response();

    resp.headers_mut()
        .insert("hx-trigger", HeaderValue::try_from(event)?);

    Ok(resp)
}

#[derive(Deserialize)]
struct AuthRegisterFinishForm {
    att: String,
}

#[tracing::instrument(err, skip_all)]
async fn register_finish(
    Extension(webauthn): Extension<Arc<Webauthn>>,
    Extension(pool): Extension<PgPool>,
    HxUser(user_id): HxUser,
    mut session: WritableSession,
    Form(reg): Form<AuthRegisterFinishForm>,
) -> Result<Response, HxError> {
    let auth_state = session.get("reg_state").context("missing reg_state")?;

    let reg = serde_json::from_str(&reg.att)?;
    let auth_result = webauthn.finish_passkey_registration(&reg, &auth_state)?;

    let cred_id = auth_result.cred_id().0.to_owned();

    sqlx::query_file!(
        "queries/selfserve/insert_credential.sql",
        user_id,
        cred_id,
        serde_json::to_value(auth_result)?
    )
    .execute(&pool)
    .await?;

    session.remove("reg_state");

    tracing::info!("finished registering user");

    let resp = api_keys_resp(&pool, user_id, None).await.into_response();
    Ok(resp)
}

#[derive(Deserialize)]
struct AuthLoginFinishForm {
    pkc: String,
}

#[tracing::instrument(err, skip_all)]
async fn login_finish(
    Extension(webauthn): Extension<Arc<Webauthn>>,
    Extension(pool): Extension<PgPool>,
    mut session: WritableSession,
    Form(reg): Form<AuthLoginFinishForm>,
) -> Result<Response, HxError> {
    let auth_state = session.get("auth_state").context("missing auth_state")?;
    session.remove("auth_state");

    let reg = serde_json::from_str(&reg.pkc)?;
    let auth_result = webauthn.finish_passkey_authentication(&reg, &auth_state)?;

    let user_id = sqlx::query_file_scalar!(
        "queries/selfserve/lookup_user_by_credential.sql",
        auth_result.cred_id().0
    )
    .fetch_one(&pool)
    .await?;

    session.insert("user_id", user_id)?;
    tracing::info!(%user_id, "finished signing in user");

    Ok(api_keys_resp(&pool, user_id, None).await.into_response())
}

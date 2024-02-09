use std::{borrow::Cow, sync::Arc};

use askama::Template;
use askama_axum::IntoResponse;
use axum::{
    async_trait,
    extract::{FromRequestParts, Request},
    http::{request::Parts, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::Response,
    routing, Extension, Form, Json, RequestPartsExt, Router,
};
use eyre::ContextCompat;
use foxlib::flags::Context;
use futures::TryFutureExt;
use rand::distributions::DistString;
use serde::Deserialize;
use sqlx::{types::Uuid, PgPool};
use thiserror::Error;
use tower_sessions::Session;
use webauthn_rs::{prelude::Passkey, Webauthn};
use webauthn_rs_proto::{
    AuthenticatorAttachment, AuthenticatorSelectionCriteria, UserVerificationPolicy,
};

use crate::{Features, Unleash};

pub fn router() -> Router {
    Router::new()
        .route("/", routing::get(index))
        .nest(
            "/auth",
            Router::new()
                .route("/", routing::post(auth_form))
                .route("/register/start", routing::post(register_start))
                .route("/register/finish", routing::post(register_finish))
                .route("/login/start", routing::post(login_start))
                .route("/login/finish", routing::post(login_finish)),
        )
        .nest(
            "/key",
            Router::new()
                .route("/create", routing::post(key_create))
                .route("/delete", routing::post(key_delete)),
        )
        .route_layer(middleware::from_fn(unleash_context))
}

#[derive(Clone, Debug)]
struct UnleashContext(Arc<Context>);

impl std::ops::Deref for UnleashContext {
    type Target = Context;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

pub async fn unleash_context(req: Request, next: Next) -> Result<Response, HxError> {
    let (mut parts, body) = req.into_parts();

    let (session_id, user_id) = if let Ok(session) = parts.extract::<Session>().await {
        let session_id = session.id().map(|id| id.to_string());

        let user_id = if let Ok(user_id) = session
            .get::<Uuid>("user_id")
            .map_ok(|id| id.map(|id| id.to_string()))
            .await
        {
            user_id
        } else {
            None
        };

        (session_id, user_id)
    } else {
        (None, None)
    };

    let unleash = match parts.extensions.get::<Unleash>() {
        Some(unleash) => unleash,
        None => {
            return Err(HxError::message(
                "Server is misconfigured.",
                StatusCode::SERVICE_UNAVAILABLE,
            ))
        }
    };

    let context = Context {
        session_id,
        user_id,
        properties: [(
            "appVersion".to_string(),
            env!("CARGO_PKG_VERSION").to_string(),
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    };

    tracing::trace!("created context for user: {context:?}");

    if !unleash.is_enabled(Features::SelfServe, Some(&context), false) {
        return Err(HxError::message(
            "Self serve is currently disabled.",
            StatusCode::FORBIDDEN,
        ));
    }

    parts.extensions.insert(UnleashContext(Arc::new(context)));

    let req = Request::from_parts(parts, body);

    Ok(next.run(req).await)
}

const USERNAME_MIN_LENGTH: usize = 5;
const USERNAME_MAX_LENGTH: usize = 24;

const KEY_COUNT_MAXIMUM: usize = 3;
const KEY_NAME_MAX_LENGTH: usize = 24;
const KEY_LENGTH: usize = 48;

#[derive(Error, Debug)]
pub enum HxError {
    #[error("Database Error")]
    Database(#[from] sqlx::Error),
    #[error("Serialization Error")]
    Serialization(#[from] serde_json::Error),
    #[error("WebAuthn Error: {0}")]
    WebAuthn(#[from] webauthn_rs::prelude::WebauthnError),
    #[error("Value Error")]
    Value(#[from] reqwest::header::InvalidHeaderValue),
    #[error("Unknown Error: {0}")]
    Unknown(#[from] eyre::Report),
    #[error("Unauthorized")]
    Unauthorized,
    #[error("Error Message: {text}")]
    Message {
        text: Cow<'static, str>,
        status_code: StatusCode,
    },
    #[error("Session Error: {0}")]
    Session(#[from] tower_sessions::session::Error),
    #[error("Header Error: {0}")]
    Header(#[from] axum::http::header::InvalidHeaderValue),
}

impl HxError {
    fn message<M>(text: M, status_code: StatusCode) -> Self
    where
        M: Into<Cow<'static, str>>,
    {
        Self::Message {
            text: text.into(),
            status_code,
        }
    }

    fn status_code(&self) -> StatusCode {
        match self {
            Self::Database(_) | Self::Unknown(_) | Self::Session(_) | Self::Header(_) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
            Self::Serialization(_) | Self::WebAuthn(_) | Self::Value(_) => StatusCode::BAD_REQUEST,
            Self::Message { status_code, .. } => *status_code,
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
        }
    }
}

struct HxUser(Uuid);

#[async_trait]
impl<S> FromRequestParts<S> for HxUser
where
    S: Send + Sync,
{
    type Rejection = HxError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let session = Session::from_request_parts(parts, state)
            .await
            .map_err(|(_status_code, message)| eyre::Report::msg(message))?;

        let user_id = session
            .get("user_id")
            .await
            .ok()
            .flatten()
            .ok_or(HxError::Unauthorized)?;
        tracing::info!(%user_id, "found user from request");

        Ok(Self(user_id))
    }
}

#[derive(Template)]
#[template(path = "selfserve/error.html")]
struct ErrorTemplate<'a> {
    message: Cow<'a, str>,
    status_code: StatusCode,
}

impl IntoResponse for HxError {
    fn into_response(self) -> Response {
        let status_code = self.status_code();

        let message = if let Self::Message { text, .. } = self {
            tracing::warn!("building message for client: {text}");
            text
        } else {
            tracing::error!("build error for client: {:?}", self);
            self.to_string().into()
        };

        let mut resp = ErrorTemplate {
            message,
            status_code,
        }
        .into_response();
        *resp.status_mut() = status_code;

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

    let can_create_key = api_keys.len() < KEY_COUNT_MAXIMUM;

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

fn filter_name_to_len(s: &str, n: usize) -> String {
    s.chars()
        .filter(char::is_ascii_alphanumeric)
        .take(n)
        .collect::<String>()
}

#[tracing::instrument(skip(webauthn, resp, session))]
async fn prepare_registration(
    webauthn: &Webauthn,
    resp: &mut Response,
    session: Session,
    user_id: Uuid,
    username: &str,
) -> Result<(), HxError> {
    let (mut ccr, reg_state) =
        webauthn.start_passkey_registration(user_id, username, username, None)?;
    ccr.public_key.authenticator_selection = Some(AuthenticatorSelectionCriteria {
        authenticator_attachment: Some(AuthenticatorAttachment::Platform),
        require_resident_key: false,
        user_verification: UserVerificationPolicy::Required,
    });

    session.insert("user_id", user_id).await?;
    session.insert("reg_state", reg_state).await?;

    let event = serde_json::json!({
        "performRegistration": {
            "ccr": ccr,
        },
    })
    .to_string();

    resp.headers_mut()
        .insert("hx-trigger", HeaderValue::try_from(event)?);

    Ok(())
}

#[tracing::instrument(skip_all)]
async fn prepare_login(
    webauthn: &Webauthn,
    resp: &mut Response,
    session: Session,
    creds: &[Passkey],
) -> Result<(), HxError> {
    let (mut rcr, passkey_auth) = webauthn.start_passkey_authentication(creds)?;
    rcr.public_key.user_verification = UserVerificationPolicy::Required;

    session.insert("auth_state", passkey_auth).await?;

    let event = serde_json::json!({
        "performLogin": {
            "rcr": rcr,
        },
    })
    .to_string();

    resp.headers_mut()
        .insert("hx-trigger", HeaderValue::try_from(event)?);

    Ok(())
}

#[derive(Template)]
#[template(path = "selfserve/index.html")]
struct IndexTemplate<'a> {
    auth_form: AuthFormTemplate<'a>,
}

async fn find_username(pool: &PgPool, session: Session) -> Option<String> {
    let user_id: Uuid = session.get("user_id").await.ok()??;

    sqlx::query_file_scalar!("queries/selfserve/lookup_username_by_id.sql", user_id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
}

async fn index(Extension(pool): Extension<PgPool>, session: Session) -> Response {
    let username = find_username(&pool, session).await;

    IndexTemplate {
        auth_form: AuthFormTemplate {
            username: username.as_deref().unwrap_or_default(),
            ..Default::default()
        },
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
            AuthFormState::Empty | AuthFormState::Error(_) | AuthFormState::UnknownUsername => "",
            _ => "hidden",
        }
    }
}

#[tracing::instrument(err, skip_all)]
async fn auth_form(
    Extension(webauthn): Extension<Arc<Webauthn>>,
    Extension(pool): Extension<PgPool>,
    session: Session,
    Form(form): Form<AuthForm>,
) -> Result<Response, HxError> {
    let username = filter_name_to_len(&form.username.unwrap_or_default(), USERNAME_MAX_LENGTH);
    let (state, created_at) = if username.len() < USERNAME_MIN_LENGTH {
        let state = AuthFormState::Error("Username must be greater than 5 characters.".into());

        (state, None)
    } else {
        let user = sqlx::query_file!("queries/selfserve/lookup_username.sql", username)
            .fetch_optional(&pool)
            .await?;

        let state = user
            .as_ref()
            .map(|_id| AuthFormState::KnownUsername)
            .unwrap_or(AuthFormState::UnknownUsername);
        let created_at = user.and_then(|user| user.registered_at.map(|reg_at| (user.uuid, reg_at)));

        (state, created_at)
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

        if creds.is_empty() {
            return handle_no_creds(&webauthn, session, &username, created_at).await;
        } else {
            prepare_login(&webauthn, &mut resp, session, &creds).await?;
        }
    }

    Ok(resp)
}

async fn handle_no_creds(
    webauthn: &Webauthn,
    session: Session,
    username: &str,
    created_at: Option<(Uuid, chrono::DateTime<chrono::Utc>)>,
) -> Result<Response, HxError> {
    tracing::warn!("user has no credentials");

    let resp = if let Some((user_id, reg_at)) = created_at {
        tracing::debug!(%reg_at, "found user created at");

        if reg_at + chrono::Duration::minutes(5) < chrono::Utc::now() {
            tracing::info!("created at older than 5 minutes, allowing registration");

            let mut resp = AlertTemplate::new(
                "Account already exists, allowing new credential registration because none existed.",
                "alert alert-warning",
            )
            .into_response();

            prepare_registration(webauthn, &mut resp, session, user_id, username).await?;

            resp
        } else {
            tracing::info!("created at too recent, warning user");

            AlertTemplate::new(
                "Account already exists, try again later if no credentials are added.",
                "alert alert-danger",
            )
            .into_response()
        }
    } else {
        AlertTemplate::new(
            "Account already exists, but no credentials are registered.",
            "alert alert-danger",
        )
        .into_response()
    };

    Ok(resp)
}

#[tracing::instrument(err, skip_all)]
async fn register_start(
    Extension(webauthn): Extension<Arc<Webauthn>>,
    Extension(pool): Extension<PgPool>,
    session: Session,
    Form(form): Form<AuthForm>,
) -> Result<Response, HxError> {
    let username = filter_name_to_len(&form.username.unwrap_or_default(), USERNAME_MAX_LENGTH);
    if username.len() < USERNAME_MIN_LENGTH {
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

    let mut resp = AlertTemplate::new(
        "Account created, please perform WebAuthn registration.",
        "alert alert-success",
    )
    .into_response();

    prepare_registration(&webauthn, &mut resp, session, user_id, &username).await?;

    tx.commit().await?;

    tracing::info!(%user_id, "created new account");

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
    session: Session,
    Form(reg): Form<AuthRegisterFinishForm>,
) -> Result<Response, HxError> {
    let auth_state = session
        .get("reg_state")
        .await?
        .context("missing reg_state")?;

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

    let _ = session.remove::<()>("reg_state").await;

    tracing::info!("finished registering user");

    Ok(api_keys_resp(&pool, user_id, None).await.into_response())
}

async fn login_start(
    Extension(unleash): Extension<Unleash>,
    Extension(webauthn): Extension<Arc<Webauthn>>,
    Extension(context): Extension<UnleashContext>,
    session: Session,
) -> Result<Response, HxError> {
    if !unleash.is_enabled(Features::DiscoverableAuth, Some(&context), false) {
        return Ok(Json(false).into_response());
    }

    let (mut rcr, discoverable_auth) = webauthn.start_discoverable_authentication()?;
    rcr.public_key.user_verification = UserVerificationPolicy::Required;

    session
        .insert("discoverable_auth", discoverable_auth)
        .await?;

    Ok(Json(rcr).into_response())
}

#[derive(Deserialize)]
struct AuthLoginFinishForm {
    pkc: String,
}

#[tracing::instrument(err, skip_all)]
async fn login_finish(
    Extension(unleash): Extension<Unleash>,
    Extension(webauthn): Extension<Arc<Webauthn>>,
    Extension(pool): Extension<PgPool>,
    Extension(context): Extension<UnleashContext>,
    session: Session,
    Form(reg): Form<AuthLoginFinishForm>,
) -> Result<Response, HxError> {
    let reg = serde_json::from_str(&reg.pkc)?;

    let auth_result = if let Ok(Some(auth_state)) = session.remove("auth_state").await {
        webauthn.finish_passkey_authentication(&reg, &auth_state)?
    } else if let Ok(Some(discoverable_auth)) = session.remove("discoverable_auth").await {
        if !unleash.is_enabled(Features::DiscoverableAuth, Some(&context), false) {
            return Err(HxError::message(
                "Discoverable authentication is not enabled.",
                StatusCode::FORBIDDEN,
            ));
        }

        let credential = sqlx::query_file!(
            "queries/selfserve/lookup_credential_by_id.sql",
            reg.raw_id.0
        )
        .fetch_one(&pool)
        .await?;

        let creds = vec![serde_json::from_value(credential.credential)?];
        webauthn.finish_discoverable_authentication(&reg, discoverable_auth, &creds)?
    } else {
        return Err(HxError::message(
            "Missing authentication state, please retry.",
            StatusCode::UNAUTHORIZED,
        ));
    };

    let user_id = sqlx::query_file_scalar!(
        "queries/selfserve/lookup_user_by_credential.sql",
        auth_result.cred_id().0
    )
    .fetch_one(&pool)
    .await?;

    session.insert("user_id", user_id).await?;
    tracing::info!(%user_id, "finished signing in user");

    Ok(api_keys_resp(&pool, user_id, None).await.into_response())
}

struct HxPrompt(String);

#[async_trait]
impl<S> FromRequestParts<S> for HxPrompt
where
    S: Send + Sync,
{
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let was_encoded = parts.headers.contains_key("hx-prompt-uri-autoencoded");

        if let Some(hx_prompt) = parts
            .headers
            .get("hx-prompt")
            .and_then(|val| val.to_str().ok())
        {
            let prompt = if was_encoded {
                tracing::debug!("prompt was encoded, attempting decode");
                match urlencoding::decode(hx_prompt) {
                    Ok(prompt) => prompt,
                    Err(err) => {
                        tracing::warn!("could not decode prompt: {err}");
                        hx_prompt.into()
                    }
                }
            } else {
                hx_prompt.into()
            };

            Ok(Self(prompt.to_string()))
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

    if count >= KEY_COUNT_MAXIMUM as i64 {
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
                .take(KEY_NAME_MAX_LENGTH)
                .collect::<String>()
        })
        .filter(|s| !s.is_empty());

    let key = rand::distributions::Alphanumeric.sample_string(&mut rand::thread_rng(), KEY_LENGTH);
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
    let name =
        sqlx::query_file_scalar!("queries/selfserve/delete_api_key.sql", form.key_id, user_id)
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

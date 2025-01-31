mod authorization;
pub mod db;
mod telemetry;
mod token;

use crate::{
    api::CloudflareIpCountryHeader, build_clickhouse_client, executor::Executor, Config, Error,
    Result,
};
use anyhow::{anyhow, Context as _};
use authorization::authorize_access_to_language_model;
use axum::{
    body::Body,
    http::{self, HeaderName, HeaderValue, Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::post,
    Extension, Json, Router, TypedHeader,
};
use chrono::{DateTime, Duration, Utc};
use db::{ActiveUserCount, LlmDatabase};
use futures::{Stream, StreamExt as _};
use http_client::IsahcHttpClient;
use rpc::{
    proto::Plan, LanguageModelProvider, PerformCompletionParams, EXPIRED_LLM_TOKEN_HEADER_NAME,
};
use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use telemetry::{report_llm_usage, LlmUsageEventRow};
use tokio::sync::RwLock;
use util::ResultExt;

pub use token::*;

pub struct LlmState {
    pub config: Config,
    pub executor: Executor,
    pub db: Arc<LlmDatabase>,
    pub http_client: IsahcHttpClient,
    pub clickhouse_client: Option<clickhouse::Client>,
    active_user_count: RwLock<Option<(DateTime<Utc>, ActiveUserCount)>>,
}

const ACTIVE_USER_COUNT_CACHE_DURATION: Duration = Duration::seconds(30);

impl LlmState {
    pub async fn new(config: Config, executor: Executor) -> Result<Arc<Self>> {
        let database_url = config
            .llm_database_url
            .as_ref()
            .ok_or_else(|| anyhow!("missing LLM_DATABASE_URL"))?;
        let max_connections = config
            .llm_database_max_connections
            .ok_or_else(|| anyhow!("missing LLM_DATABASE_MAX_CONNECTIONS"))?;

        let mut db_options = db::ConnectOptions::new(database_url);
        db_options.max_connections(max_connections);
        let mut db = LlmDatabase::new(db_options, executor.clone()).await?;
        db.initialize().await?;

        let db = Arc::new(db);

        let user_agent = format!("Zed Server/{}", env!("CARGO_PKG_VERSION"));
        let http_client = IsahcHttpClient::builder()
            .default_header("User-Agent", user_agent)
            .build()
            .context("failed to construct http client")?;

        let initial_active_user_count =
            Some((Utc::now(), db.get_active_user_count(Utc::now()).await?));

        let this = Self {
            executor,
            db,
            http_client,
            clickhouse_client: config
                .clickhouse_url
                .as_ref()
                .and_then(|_| build_clickhouse_client(&config).log_err()),
            active_user_count: RwLock::new(initial_active_user_count),
            config,
        };

        Ok(Arc::new(this))
    }

    pub async fn get_active_user_count(&self) -> Result<ActiveUserCount> {
        let now = Utc::now();

        if let Some((last_updated, count)) = self.active_user_count.read().await.as_ref() {
            if now - *last_updated < ACTIVE_USER_COUNT_CACHE_DURATION {
                return Ok(*count);
            }
        }

        let mut cache = self.active_user_count.write().await;
        let new_count = self.db.get_active_user_count(now).await?;
        *cache = Some((now, new_count));
        Ok(new_count)
    }
}

pub fn routes() -> Router<(), Body> {
    Router::new()
        .route("/completion", post(perform_completion))
        .layer(middleware::from_fn(validate_api_token))
}

async fn validate_api_token<B>(mut req: Request<B>, next: Next<B>) -> impl IntoResponse {
    let token = req
        .headers()
        .get(http::header::AUTHORIZATION)
        .and_then(|header| header.to_str().ok())
        .ok_or_else(|| {
            Error::http(
                StatusCode::BAD_REQUEST,
                "missing authorization header".to_string(),
            )
        })?
        .strip_prefix("Bearer ")
        .ok_or_else(|| {
            Error::http(
                StatusCode::BAD_REQUEST,
                "invalid authorization header".to_string(),
            )
        })?;

    let state = req.extensions().get::<Arc<LlmState>>().unwrap();
    match LlmTokenClaims::validate(&token, &state.config) {
        Ok(claims) => {
            req.extensions_mut().insert(claims);
            Ok::<_, Error>(next.run(req).await.into_response())
        }
        Err(ValidateLlmTokenError::Expired) => Err(Error::Http(
            StatusCode::UNAUTHORIZED,
            "unauthorized".to_string(),
            [(
                HeaderName::from_static(EXPIRED_LLM_TOKEN_HEADER_NAME),
                HeaderValue::from_static("true"),
            )]
            .into_iter()
            .collect(),
        )),
        Err(_err) => Err(Error::http(
            StatusCode::UNAUTHORIZED,
            "unauthorized".to_string(),
        )),
    }
}

async fn perform_completion(
    Extension(state): Extension<Arc<LlmState>>,
    Extension(claims): Extension<LlmTokenClaims>,
    country_code_header: Option<TypedHeader<CloudflareIpCountryHeader>>,
    Json(params): Json<PerformCompletionParams>,
) -> Result<impl IntoResponse> {
    let model = normalize_model_name(params.provider, params.model);

    authorize_access_to_language_model(
        &state.config,
        &claims,
        country_code_header.map(|header| header.to_string()),
        params.provider,
        &model,
    )?;

    check_usage_limit(&state, params.provider, &model, &claims).await?;

    let stream = match params.provider {
        LanguageModelProvider::Anthropic => {
            let api_key = state
                .config
                .anthropic_api_key
                .as_ref()
                .context("no Anthropic AI API key configured on the server")?;

            let mut request: anthropic::Request =
                serde_json::from_str(&params.provider_request.get())?;

            // Parse the model, throw away the version that was included, and then set a specific
            // version that we control on the server.
            // Right now, we use the version that's defined in `model.id()`, but we will likely
            // want to change this code once a new version of an Anthropic model is released,
            // so that users can use the new version, without having to update Zed.
            request.model = match anthropic::Model::from_id(&request.model) {
                Ok(model) => model.id().to_string(),
                Err(_) => request.model,
            };

            let chunks = anthropic::stream_completion(
                &state.http_client,
                anthropic::ANTHROPIC_API_URL,
                api_key,
                request,
                None,
            )
            .await
            .map_err(|err| match err {
                anthropic::AnthropicError::ApiError(ref api_error) => {
                    if api_error.code() == Some(anthropic::ApiErrorCode::RateLimitError) {
                        return Error::http(
                            StatusCode::TOO_MANY_REQUESTS,
                            "Upstream Anthropic rate limit exceeded.".to_string(),
                        );
                    }

                    Error::Internal(anyhow!(err))
                }
                anthropic::AnthropicError::Other(err) => Error::Internal(err),
            })?;

            chunks
                .map(move |event| {
                    let chunk = event?;
                    let (input_tokens, output_tokens) = match &chunk {
                        anthropic::Event::MessageStart {
                            message: anthropic::Response { usage, .. },
                        }
                        | anthropic::Event::MessageDelta { usage, .. } => (
                            usage.input_tokens.unwrap_or(0) as usize,
                            usage.output_tokens.unwrap_or(0) as usize,
                        ),
                        _ => (0, 0),
                    };

                    anyhow::Ok((
                        serde_json::to_vec(&chunk).unwrap(),
                        input_tokens,
                        output_tokens,
                    ))
                })
                .boxed()
        }
        LanguageModelProvider::OpenAi => {
            let api_key = state
                .config
                .openai_api_key
                .as_ref()
                .context("no OpenAI API key configured on the server")?;
            let chunks = open_ai::stream_completion(
                &state.http_client,
                open_ai::OPEN_AI_API_URL,
                api_key,
                serde_json::from_str(&params.provider_request.get())?,
                None,
            )
            .await?;

            chunks
                .map(|event| {
                    event.map(|chunk| {
                        let input_tokens =
                            chunk.usage.as_ref().map_or(0, |u| u.prompt_tokens) as usize;
                        let output_tokens =
                            chunk.usage.as_ref().map_or(0, |u| u.completion_tokens) as usize;
                        (
                            serde_json::to_vec(&chunk).unwrap(),
                            input_tokens,
                            output_tokens,
                        )
                    })
                })
                .boxed()
        }
        LanguageModelProvider::Google => {
            let api_key = state
                .config
                .google_ai_api_key
                .as_ref()
                .context("no Google AI API key configured on the server")?;
            let chunks = google_ai::stream_generate_content(
                &state.http_client,
                google_ai::API_URL,
                api_key,
                serde_json::from_str(&params.provider_request.get())?,
            )
            .await?;

            chunks
                .map(|event| {
                    event.map(|chunk| {
                        // TODO - implement token counting for Google AI
                        let input_tokens = 0;
                        let output_tokens = 0;
                        (
                            serde_json::to_vec(&chunk).unwrap(),
                            input_tokens,
                            output_tokens,
                        )
                    })
                })
                .boxed()
        }
        LanguageModelProvider::Zed => {
            let api_key = state
                .config
                .qwen2_7b_api_key
                .as_ref()
                .context("no Qwen2-7B API key configured on the server")?;
            let api_url = state
                .config
                .qwen2_7b_api_url
                .as_ref()
                .context("no Qwen2-7B URL configured on the server")?;
            let chunks = open_ai::stream_completion(
                &state.http_client,
                &api_url,
                api_key,
                serde_json::from_str(&params.provider_request.get())?,
                None,
            )
            .await?;

            chunks
                .map(|event| {
                    event.map(|chunk| {
                        let input_tokens =
                            chunk.usage.as_ref().map_or(0, |u| u.prompt_tokens) as usize;
                        let output_tokens =
                            chunk.usage.as_ref().map_or(0, |u| u.completion_tokens) as usize;
                        (
                            serde_json::to_vec(&chunk).unwrap(),
                            input_tokens,
                            output_tokens,
                        )
                    })
                })
                .boxed()
        }
    };

    Ok(Response::new(Body::wrap_stream(TokenCountingStream {
        state,
        claims,
        provider: params.provider,
        model,
        input_tokens: 0,
        output_tokens: 0,
        inner_stream: stream,
    })))
}

fn normalize_model_name(provider: LanguageModelProvider, name: String) -> String {
    let prefixes: &[_] = match provider {
        LanguageModelProvider::Anthropic => &[
            "claude-3-5-sonnet",
            "claude-3-haiku",
            "claude-3-opus",
            "claude-3-sonnet",
        ],
        LanguageModelProvider::OpenAi => &[
            "gpt-3.5-turbo",
            "gpt-4-turbo-preview",
            "gpt-4o-mini",
            "gpt-4o",
            "gpt-4",
        ],
        LanguageModelProvider::Google => &[],
        LanguageModelProvider::Zed => &[],
    };

    if let Some(prefix) = prefixes
        .iter()
        .filter(|&&prefix| name.starts_with(prefix))
        .max_by_key(|&&prefix| prefix.len())
    {
        prefix.to_string()
    } else {
        name
    }
}

async fn check_usage_limit(
    state: &Arc<LlmState>,
    provider: LanguageModelProvider,
    model_name: &str,
    claims: &LlmTokenClaims,
) -> Result<()> {
    let model = state.db.model(provider, model_name)?;
    let usage = state
        .db
        .get_usage(claims.user_id as i32, provider, model_name, Utc::now())
        .await?;

    let active_users = state.get_active_user_count().await?;

    let per_user_max_requests_per_minute =
        model.max_requests_per_minute as usize / active_users.users_in_recent_minutes.max(1);
    let per_user_max_tokens_per_minute =
        model.max_tokens_per_minute as usize / active_users.users_in_recent_minutes.max(1);
    let per_user_max_tokens_per_day =
        model.max_tokens_per_day as usize / active_users.users_in_recent_days.max(1);

    let checks = [
        (
            usage.requests_this_minute,
            per_user_max_requests_per_minute,
            "requests per minute",
        ),
        (
            usage.tokens_this_minute,
            per_user_max_tokens_per_minute,
            "tokens per minute",
        ),
        (
            usage.tokens_this_day,
            per_user_max_tokens_per_day,
            "tokens per day",
        ),
    ];

    for (usage, limit, resource) in checks {
        // Temporarily bypass rate-limiting for staff members.
        if claims.is_staff {
            continue;
        }

        if usage > limit {
            return Err(Error::http(
                StatusCode::TOO_MANY_REQUESTS,
                format!("Rate limit exceeded. Maximum {} reached.", resource),
            ));
        }
    }

    Ok(())
}

struct TokenCountingStream<S> {
    state: Arc<LlmState>,
    claims: LlmTokenClaims,
    provider: LanguageModelProvider,
    model: String,
    input_tokens: usize,
    output_tokens: usize,
    inner_stream: S,
}

impl<S> Stream for TokenCountingStream<S>
where
    S: Stream<Item = Result<(Vec<u8>, usize, usize), anyhow::Error>> + Unpin,
{
    type Item = Result<Vec<u8>, anyhow::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match Pin::new(&mut self.inner_stream).poll_next(cx) {
            Poll::Ready(Some(Ok((mut bytes, input_tokens, output_tokens)))) => {
                bytes.push(b'\n');
                self.input_tokens += input_tokens;
                self.output_tokens += output_tokens;
                Poll::Ready(Some(Ok(bytes)))
            }
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(e))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<S> Drop for TokenCountingStream<S> {
    fn drop(&mut self) {
        let state = self.state.clone();
        let claims = self.claims.clone();
        let provider = self.provider;
        let model = std::mem::take(&mut self.model);
        let input_token_count = self.input_tokens;
        let output_token_count = self.output_tokens;
        self.state.executor.spawn_detached(async move {
            let usage = state
                .db
                .record_usage(
                    claims.user_id as i32,
                    provider,
                    &model,
                    input_token_count,
                    output_token_count,
                    Utc::now(),
                )
                .await
                .log_err();

            if let Some((clickhouse_client, usage)) = state.clickhouse_client.as_ref().zip(usage) {
                report_llm_usage(
                    clickhouse_client,
                    LlmUsageEventRow {
                        time: Utc::now().timestamp_millis(),
                        user_id: claims.user_id as i32,
                        is_staff: claims.is_staff,
                        plan: match claims.plan {
                            Plan::Free => "free".to_string(),
                            Plan::ZedPro => "zed_pro".to_string(),
                        },
                        model,
                        provider: provider.to_string(),
                        input_token_count: input_token_count as u64,
                        output_token_count: output_token_count as u64,
                        requests_this_minute: usage.requests_this_minute as u64,
                        tokens_this_minute: usage.tokens_this_minute as u64,
                        tokens_this_day: usage.tokens_this_day as u64,
                        input_tokens_this_month: usage.input_tokens_this_month as u64,
                        output_tokens_this_month: usage.output_tokens_this_month as u64,
                        spending_this_month: usage.spending_this_month as u64,
                    },
                )
                .await
                .log_err();
            }
        })
    }
}

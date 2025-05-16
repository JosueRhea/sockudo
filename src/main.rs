mod adapter;
mod app;
mod cache;
mod channel;
mod error;
mod http_handler;
pub mod log;
mod metrics;
mod namespace;
mod options;
mod protocol;
mod queue;
mod rate_limiter;
mod token;
pub mod utils;
mod webhook;
mod websocket;
mod ws_handler;

use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::net::SocketAddr;
use std::path::Path;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use app::dynamodb_app_manager::DynamoDbConfig;
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE, HeaderName};
use axum::http::uri::Authority;
use axum::http::Method;
use axum::http::{HeaderValue, StatusCode, Uri};
use axum::response::Redirect;
use axum::routing::{get, post};
use axum::{serve, BoxError, Router, ServiceExt};

use axum_extra::extract::Host;
use axum_server::tls_rustls::RustlsConfig;
use error::Error;
use serde_json::{from_str, json, Value};
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;
use tokio::signal;
use tokio::sync::{Mutex, RwLock};

// Updated factory imports
use crate::adapter::factory::AdapterFactory;
use crate::app::factory::AppManagerFactory;
use crate::cache::factory::CacheManagerFactory;
use crate::channel::ChannelManager;
use crate::error::Result;
use crate::http_handler::{
    batch_events, channel, channel_users, channels, events, metrics, terminate_user_connections,
    up, usage,
};
use crate::log::Log;
use crate::metrics::MetricsFactory;
use crate::options::{
    ServerOptions, AdapterDriver, AppManagerDriver, CacheDriver, QueueDriver, MetricsDriver,
    MemoryCacheOptions, // Import MemoryCacheOptions
};
use crate::queue::manager::{QueueManager, QueueManagerFactory};
use crate::rate_limiter::factory::RateLimiterFactory;
use crate::rate_limiter::middleware::IpKeyExtractor;
use crate::rate_limiter::RateLimiter;
use crate::webhook::integration::{BatchingConfig, WebhookConfig, WebhookIntegration};
use crate::ws_handler::handle_ws_upgrade;
use tower_http::cors::CorsLayer;
use tracing::{error, info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

// Import concrete adapter types for downcasting if set_metrics is specific
use crate::adapter::local_adapter::LocalAdapter;
use crate::adapter::nats_adapter::NatsAdapter;
use crate::adapter::redis_adapter::RedisAdapter;
use crate::adapter::redis_cluster_adapter::RedisClusterAdapter;
use crate::adapter::Adapter;
use crate::adapter::ConnectionHandler;
use crate::app::auth::AuthValidator;
use crate::app::config::App;
// AppManager trait and concrete types
use crate::app::manager::AppManager;
// CacheManager trait and concrete types
use crate::cache::manager::CacheManager;
use crate::cache::memory_cache_manager::MemoryCacheManager; // Import for fallback
// MetricsInterface trait
use crate::metrics::MetricsInterface;


/// Server state containing all managers
#[derive(Clone)]
struct ServerState {
    app_manager: Arc<dyn AppManager + Send + Sync>,
    channel_manager: Arc<RwLock<ChannelManager>>,
    connection_manager: Arc<Mutex<Box<dyn Adapter + Send + Sync>>>,
    auth_validator: Arc<AuthValidator>,
    cache_manager: Arc<Mutex<dyn CacheManager + Send + Sync>>,
    queue_manager: Option<Arc<QueueManager>>,
    webhooks_integration: Arc<WebhookIntegration>,
    metrics: Option<Arc<Mutex<dyn MetricsInterface + Send + Sync>>>,
    running: Arc<AtomicBool>,
    http_api_rate_limiter: Option<Arc<dyn RateLimiter + Send + Sync>>,
    debug_enabled: bool,
}

/// Main server struct
struct SockudoServer {
    config: ServerOptions,
    state: ServerState,
    handler: Arc<ConnectionHandler>,
}

impl SockudoServer {
    fn get_http_addr(&self) -> SocketAddr {
        format!("{}:{}", self.config.host, self.config.port)
            .parse()
            .unwrap_or_else(|_| "127.0.0.1:6001".parse().unwrap())
    }

    fn get_metrics_addr(&self) -> SocketAddr {
        format!("{}:{}", self.config.metrics.host, self.config.metrics.port)
            .parse()
            .unwrap_or_else(|_| "127.0.0.1:9601".parse().unwrap())
    }

    async fn new(config: ServerOptions) -> Result<Self> {
        let debug_enabled = config.debug;
        Log::info("Initializing Sockudo server with new configuration...".to_string());

        let app_manager = AppManagerFactory::create(
            &config.app_manager,
            &config.database,
        ).await?;
        Log::info(format!("AppManager initialized with driver: {:?}", config.app_manager.driver));

        let connection_manager_box = AdapterFactory::create(
            &config.adapter,
            &config.database,
            debug_enabled,
        ).await?;
        let connection_manager_arc = Arc::new(Mutex::new(connection_manager_box));
        Log::info(format!("Adapter initialized with driver: {:?}", config.adapter.driver));


        let cache_manager = CacheManagerFactory::create(
            &config.cache,
            &config.database.redis,
            debug_enabled,
        ).await.unwrap_or_else(|e| {
            Log::warning(format!("CacheManagerFactory creation failed: {}. Using a NoOp (Memory) Cache.", e));
            // Fallback to a memory cache with default options from ServerOptions.cache.memory
            let fallback_cache_options = config.cache.memory.clone();
            Arc::new(Mutex::new(Box::new(MemoryCacheManager::new(
                "fallback_cache".to_string(), // Provide a default prefix
                fallback_cache_options
            )) as Box<dyn CacheManager + Send + Sync>))
        });
        Log::info(format!("CacheManager initialized with driver: {:?}", config.cache.driver));


        let channel_manager =
            Arc::new(RwLock::new(ChannelManager::new(connection_manager_arc.clone())));
        let auth_validator = Arc::new(AuthValidator::new(app_manager.clone()));

        let metrics = if config.metrics.enabled {
            Log::info(format!("Initializing metrics with driver: {:?}", config.metrics.driver));
            match MetricsFactory::create(
                &config.metrics.driver.into(),
                config.metrics.port,
                Some(&config.metrics.prometheus.prefix),
            ).await {
                Some(metrics_driver) => {
                    Log::info("Metrics driver initialized successfully".to_string());
                    Some(metrics_driver)
                }
                None => {
                    Log::warning("Failed to initialize metrics driver, metrics will be disabled".to_string());
                    None
                }
            }
        } else {
            Log::info("Metrics are disabled in configuration".to_string());
            None
        };

        let http_api_rate_limiter_instance = if config.rate_limiter.enabled {
            RateLimiterFactory::create(
                &config.rate_limiter,
                &config.database.redis,
                debug_enabled
            ).await.unwrap_or_else(|e| {
                Log::error(format!("Failed to initialize HTTP API rate limiter: {}. Using a permissive limiter.", e));
                Arc::new(rate_limiter::memory_limiter::MemoryRateLimiter::new(u32::MAX, 1))
            })
        } else {
            Log::info("HTTP API Rate limiting is globally disabled. Using a permissive limiter.".to_string());
            Arc::new(rate_limiter::memory_limiter::MemoryRateLimiter::new(u32::MAX, 1))
        };
        Log::info(format!("HTTP API RateLimiter initialized (enabled: {}) with driver: {:?}", config.rate_limiter.enabled, config.rate_limiter.driver));


        let queue_manager_opt = if config.queue.driver != QueueDriver::None {
            match QueueManagerFactory::create(
                &config.queue.driver,
                // Use url_override from RedisQueueConfig if Some, else construct from global RedisConnection
                config.queue.redis.url_override.as_deref().or_else(||
                    Some(&format!("redis://{}:{}", config.database.redis.host, config.database.redis.port))
                ),
                // Use prefix from RedisQueueConfig if Some, else a default
                Some(config.queue.redis.prefix.as_deref().unwrap_or("sockudo_queue:")),
                Some(config.queue.redis.concurrency as usize),
            ).await {
                Ok(queue_driver_impl) => {
                    Log::info(format!("Queue manager initialized with driver: {:?}", config.queue.driver));
                    Some(Arc::new(QueueManager::new(queue_driver_impl)))
                }
                Err(e) => {
                    Log::warning(format!("Failed to initialize queue manager with driver '{:?}': {}, queues will be disabled", config.queue.driver, e));
                    None
                }
            }
        } else {
            Log::info("Queue driver set to None, queue manager will be disabled.".to_string());
            None
        };

        let webhook_config_for_integration = WebhookConfig {
            enabled: true,
            batching: BatchingConfig {
                enabled: config.webhooks.batching.enabled,
                duration: config.webhooks.batching.duration,
            },
            queue_driver: config.queue.driver.clone(),
            redis_url: Option::from(config.database.redis.host.clone()),
            redis_prefix: Some(config.database.redis.key_prefix.clone() + "webhooks:"),
            redis_concurrency: Some(config.queue.redis.concurrency as usize),
            process_id: config.instance.process_id.clone(),
            debug: config.debug,
        };

        let webhook_integration =
            match WebhookIntegration::new(webhook_config_for_integration, app_manager.clone()).await {
                Ok(integration) => {
                    Log::info("Webhook integration initialized successfully".to_string());
                    Arc::new(integration)
                }
                Err(e) => {
                    Log::warning(format!("Failed to initialize webhook integration: {}, webhooks will be disabled", e));
                    let disabled_config = WebhookConfig { enabled: false, ..Default::default() };
                    Arc::new(WebhookIntegration::new(disabled_config, app_manager.clone()).await?)
                }
            };

        let state = ServerState {
            app_manager: app_manager.clone(),
            channel_manager: channel_manager.clone(),
            connection_manager: connection_manager_arc.clone(),
            auth_validator,
            cache_manager,
            queue_manager: queue_manager_opt,
            webhooks_integration: webhook_integration.clone(),
            metrics: metrics.clone(),
            running: Arc::new(AtomicBool::new(true)),
            http_api_rate_limiter: Some(http_api_rate_limiter_instance.clone()),
            debug_enabled,
        };

        let handler = Arc::new(ConnectionHandler::new(
            state.app_manager.clone(),
            state.channel_manager.clone(),
            state.connection_manager.clone(),
            state.cache_manager.clone(),
            state.metrics.clone(),
            Some(webhook_integration),
            state.http_api_rate_limiter.clone(),
        ));

        if let Some(metrics_instance_arc) = &metrics {
            let mut connection_manager_guard = state.connection_manager.lock().await;
            let adapter_as_any: &mut dyn std::any::Any = connection_manager_guard.as_any_mut();

            match config.adapter.driver {
                AdapterDriver::Redis => {
                    if let Some(adapter_mut) = adapter_as_any.downcast_mut::<RedisAdapter>() {
                        adapter_mut.set_metrics(metrics_instance_arc.clone()).await.ok();
                        Log::info("Set metrics for RedisAdapter".to_string());
                    } else {
                        Log::warning("Failed to downcast to RedisAdapter for metrics setup".to_string());
                    }
                }
                AdapterDriver::Nats => {
                    if let Some(adapter_mut) = adapter_as_any.downcast_mut::<NatsAdapter>() {
                        adapter_mut.set_metrics(metrics_instance_arc.clone()).await.ok();
                        Log::info("Set metrics for NatsAdapter".to_string());
                    } else {
                        Log::warning("Failed to downcast to NatsAdapter for metrics setup".to_string());
                    }
                }
                AdapterDriver::RedisCluster => {
                    if let Some(_adapter_mut) = adapter_as_any.downcast_mut::<RedisClusterAdapter>() {
                        Log::info("Metrics setup for RedisClusterAdapter (set_metrics call placeholder)".to_string());
                    } else {
                        Log::warning("Failed to downcast to RedisClusterAdapter for metrics setup".to_string());
                    }
                }
                AdapterDriver::Local => {
                    if let Some(_adapter_mut) = adapter_as_any.downcast_mut::<LocalAdapter>() {
                        Log::info("Metrics setup for LocalAdapter (if applicable)".to_string());
                    } else {
                        Log::warning("Failed to downcast to LocalAdapter for metrics setup".to_string());
                    }
                }
            }
        }
        Ok(Self { config, state, handler })
    }

    async fn init(&self) -> Result<()> {
        let debug_enabled = self.config.debug;
        Log::info("Server init sequence started.".to_string());
        {
            let mut connection_manager = self.state.connection_manager.lock().await;
            connection_manager.init().await;
        }

        if !self.config.app_manager.array.apps.is_empty() {
            Log::info(format!("Registering {} apps from configuration", self.config.app_manager.array.apps.len()));
            let apps_to_register = self.config.app_manager.array.apps.clone();
            for app in apps_to_register {
                Log::info(format!("Registering app: id={}, key={}", app.id, app.key));
                match self.state.app_manager.register_app(app.clone()).await {
                    Ok(_) => Log::info(format!("Successfully registered app: {}", app.id)),
                    Err(e) => {
                        Log::warning(format!("Failed to register app {}: {}", app.id, e));
                        match self.state.app_manager.get_app(&app.id).await {
                            Ok(Some(_)) => {
                                Log::info(format!("App {} already exists, updating instead", app.id));
                                if let Err(update_err) = self.state.app_manager.update_app(app.clone()).await {
                                    Log::error(format!("Failed to update existing app {}: {}", app.id, update_err));
                                } else {
                                    Log::info(format!("Successfully updated app: {}", app.id));
                                }
                            }
                            _ => Log::error(format!("Error retrieving app {}: {}", app.id, e)),
                        }
                    }
                }
            }
        } else {
            Log::info("No apps found in configuration, registering demo app".to_string());
            let demo_app = App {
                id: "demo-app".to_string(),
                key: "demo-key".to_string(),
                secret: "demo-secret".to_string(),
                enable_client_messages: true,
                enabled: true,
                max_connections: 1000,
                max_client_events_per_second: 100,
                webhooks: Some(vec![crate::webhook::types::Webhook {
                    url: Some("http://localhost:3000/pusher/webhooks".parse().unwrap()),
                    lambda_function: None, lambda: None,
                    event_types: vec!["member_added".to_string(), "member_removed".to_string()],
                    filter: None, headers: None,
                }]),
                ..Default::default()
            };
            match self.state.app_manager.register_app(demo_app).await {
                Ok(_) => Log::info("Successfully registered demo app".to_string()),
                Err(e) => Log::warning(format!("Failed to register demo app: {}", e)),
            }
        }

        match self.state.app_manager.get_apps().await {
            Ok(apps) => {
                Log::info(format!("Server has {} registered apps:", apps.len()));
                for app in apps {
                    Log::info(format!("- App: id={}, key={}, enabled={}", app.id, app.key, app.enabled));
                }
            }
            Err(e) => Log::warning(format!("Failed to retrieve registered apps: {}", e)),
        }

        if let Some(metrics) = &self.state.metrics {
            let metrics_guard = metrics.lock().await;
            if let Err(e) = metrics_guard.init().await {
                Log::warning(format!("Failed to initialize metrics: {}", e));
            }
        }
        Log::info("Server init sequence completed.".to_string());
        Ok(())
    }

    fn configure_http_routes(&self) -> Router {
        let debug_enabled = self.config.debug;
        let cors = CorsLayer::new()
            .allow_origin(self.config.cors.origin.iter().map(|s| s.parse::<HeaderValue>().expect("Failed to parse CORS origin")).collect::<Vec<_>>())
            .allow_methods(self.config.cors.methods.iter().map(|s| Method::from_str(s).expect("Failed to parse CORS method")).collect::<Vec<_>>())
            .allow_headers(self.config.cors.allowed_headers.iter().map(|s| HeaderName::from_str(s).expect("Failed to parse CORS header")).collect::<Vec<_>>())
            .allow_credentials(self.config.cors.credentials);

        let rate_limiter_middleware_layer = if self.config.rate_limiter.enabled {
            if let Some(rate_limiter_instance) = &self.state.http_api_rate_limiter {
                let options = crate::rate_limiter::middleware::RateLimitOptions {
                    include_headers: true,
                    fail_open: false,
                    key_prefix: Some("api:".to_string()),
                };
                let trust_hops = self.config.rate_limiter.api_rate_limit.trust_hops.unwrap_or(0) as usize;
                let ip_key_extractor = IpKeyExtractor::new(trust_hops);

                Log::info(format!("Applying custom rate limiting middleware with trust_hops: {}", trust_hops));
                Some(crate::rate_limiter::middleware::RateLimitLayer::with_options(
                    rate_limiter_instance.clone(),
                    ip_key_extractor,
                    options,
                ))
            } else {
                Log::warning("Rate limiting is enabled in config, but no RateLimiter instance found in server state for HTTP API.".to_string());
                None
            }
        } else {
            Log::info("Custom HTTP API Rate limiting is disabled in configuration.".to_string());
            None
        };

        let mut router = Router::new()
            .route("/app/:appKey", get(handle_ws_upgrade))
            .route("/apps/:appId/events", post(events))
            .route("/apps/:appId/batch_events", post(batch_events))
            .route("/apps/:appId/channels", get(channels))
            .route("/apps/:appId/channels/:channelName", get(channel))
            .route("/apps/:appId/channels/:channelName/users", get(channel_users))
            .route("/apps/:appId/users/:userId/terminate_connections", post(terminate_user_connections))
            .route("/usage", get(usage))
            .route("/up/:app_id", get(up))
            .layer(cors);

        if let Some(middleware) = rate_limiter_middleware_layer {
            router = router.layer(middleware);
        }

        router.with_state(self.handler.clone())
    }

    fn configure_metrics_routes(&self) -> Router {
        Router::new()
            .route("/metrics", get(metrics))
            .with_state(self.handler.clone())
    }

    async fn start(&self) -> Result<()> {
        let debug_enabled = self.config.debug;
        Log::info("Starting Sockudo server services...".to_string());
        self.init().await?;

        let http_router = self.configure_http_routes();
        let metrics_router = self.configure_metrics_routes();

        let http_addr = self.get_http_addr();
        let metrics_addr = self.get_metrics_addr();

        if self.config.ssl.enabled && !self.config.ssl.cert_path.is_empty() && !self.config.ssl.key_path.is_empty() {
            Log::info("SSL is enabled, starting HTTPS server".to_string());
            let tls_config = self.load_tls_config().await?;

            if self.config.ssl.redirect_http {
                let http_port = self.config.ssl.http_port.unwrap_or(80);
                let host_ip = self.config.host.parse::<std::net::IpAddr>().unwrap_or_else(|_| "0.0.0.0".parse().unwrap());
                let redirect_addr = SocketAddr::from((host_ip, http_port));
                Log::info(format!("Starting HTTP to HTTPS redirect server on {}", redirect_addr));
                let https_port = self.config.port;
                let redirect_app = Router::new().fallback(move |Host(host): Host, uri: Uri| async move {
                    match make_https(&host, uri, https_port) {
                        Ok(uri_https) => Ok(Redirect::permanent(&uri_https.to_string())),
                        Err(error) => {
                            warn!(%error, "failed to convert URI to HTTPS");
                            Err(StatusCode::BAD_REQUEST)
                        }
                    }
                });
                match TcpListener::bind(redirect_addr).await {
                    Ok(redirect_listener) => {
                        tokio::spawn(async move {
                            if let Err(e) = axum::serve(redirect_listener, redirect_app.into_make_service_with_connect_info::<SocketAddr>()).await {
                                error!("HTTP redirect server error: {}", e);
                            }
                        });
                    }
                    Err(e) => Log::warning(format!("Failed to bind HTTP redirect server: {}", e)),
                }
            }

            if self.config.metrics.enabled {
                if let Ok(metrics_listener) = TcpListener::bind(metrics_addr).await {
                    Log::info(format!("Metrics server listening on {}", metrics_addr));
                    let metrics_router_clone = metrics_router.clone();
                    tokio::spawn(async move {
                        if let Err(e) = axum::serve(metrics_listener, metrics_router_clone.into_make_service()).await {
                            error!("Metrics server error: {}", e);
                        }
                    });
                } else {
                    Log::warning(format!("Failed to start metrics server on {}", metrics_addr));
                }
            }

            Log::info(format!("HTTPS server listening on {}", http_addr));
            let running = self.state.running.clone();
            let server = axum_server::bind_rustls(http_addr, tls_config);
            tokio::select! {
                result = server.serve(http_router.into_make_service_with_connect_info::<SocketAddr>()) => {
                    if let Err(err) = result { error!("HTTPS server error: {}", err); }
                }
                _ = self.shutdown_signal() => {
                    Log::info("Shutdown signal received for HTTPS server".to_string());
                    running.store(false, Ordering::SeqCst);
                }
            }
        } else {
            Log::info("SSL is not enabled, starting HTTP server".to_string());
            let http_listener = TcpListener::bind(http_addr).await?;
            let metrics_listener_opt = if self.config.metrics.enabled {
                match TcpListener::bind(metrics_addr).await {
                    Ok(listener) => {
                        Log::info(format!("Metrics server listening on {}", metrics_addr));
                        Some(listener)
                    }
                    Err(e) => {
                        Log::warning(format!("Failed to bind metrics server: {}", e)); None
                    }
                }
            } else { None };

            Log::info(format!("HTTP server listening on {}", http_addr));
            let running = self.state.running.clone();

            if let Some(metrics_listener) = metrics_listener_opt {
                let metrics_router_clone = metrics_router.clone();
                tokio::spawn(async move {
                    if let Err(e) = axum::serve(metrics_listener, metrics_router_clone.into_make_service()).await {
                        error!("Metrics server error: {}", e);
                    }
                });
            }

            let http_server = axum::serve(http_listener, http_router.into_make_service_with_connect_info::<SocketAddr>());
            tokio::select! {
                res = http_server => {
                    if let Err(err) = res { error!("HTTP server error: {}", err); }
                }
                _ = self.shutdown_signal() => {
                    Log::info("Shutdown signal received for HTTP server".to_string());
                    running.store(false, Ordering::SeqCst);
                }
            }
        }
        Log::info("Server shutting down".to_string());
        Ok(())
    }

    async fn load_tls_config(&self) -> Result<RustlsConfig> {
        let cert_path = std::path::PathBuf::from(&self.config.ssl.cert_path);
        let key_path = std::path::PathBuf::from(&self.config.ssl.key_path);
        RustlsConfig::from_pem_file(cert_path, key_path)
            .await
            .map_err(|e| Error::InternalError(format!("Failed to load TLS configuration: {}", e)))
    }

    async fn shutdown_signal(&self) {
        let ctrl_c = async { signal::ctrl_c().await.expect("Failed to install Ctrl+C handler"); };
        #[cfg(unix)]
        let terminate = async {
            signal::unix::signal(signal::unix::SignalKind::terminate())
                .expect("Failed to install signal handler")
                .recv().await;
        };
        #[cfg(not(unix))]
        let terminate = std::future::pending::<()>();
        tokio::select! { _ = ctrl_c => {}, _ = terminate => {}, }
        Log::info( "Shutdown signal received, starting graceful shutdown".to_string());
    }

    #[allow(dead_code)]
    async fn stop(&self) -> Result<()> {
        let debug_enabled = self.config.debug;
        Log::info("Stopping server...".to_string());
        self.state.running.store(false, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_secs(self.config.shutdown_grace_period)).await;
        {
            let cache_manager_locked = self.state.cache_manager.lock().await;
            let _ = cache_manager_locked.disconnect().await;
        }
        if let Some(queue_manager_arc) = &self.state.queue_manager {
            let _ = queue_manager_arc.disconnect().await;
        }
        Log::info("Server stopped".to_string());
        Ok(())
    }

    #[allow(dead_code)]
    pub async fn load_options_from_file<P: AsRef<Path>>(&mut self, path: P) -> Result<()> {
        let mut file = tokio::fs::File::open(path).await?;
        let mut contents = String::new();
        file.read_to_string(&mut contents).await?;
        let options: ServerOptions = from_str(&contents)?;
        self.config = options;
        Log::info( format!("Loaded options from file, app_manager config: {:?}", self.config.app_manager));
        Ok(())
    }

    #[allow(dead_code)]
    async fn register_apps(&self, apps: Vec<App>) -> Result<()> {
        let debug_enabled = self.config.debug;
        for app in apps {
            let existing_app = self.state.app_manager.get_app(&app.id).await?;
            if existing_app.is_some() {
                Log::info(format!("Updating app: {}", app.id));
                self.state.app_manager.update_app(app).await?;
            } else {
                Log::info(format!("Registering new app: {}", app.id));
                self.state.app_manager.register_app(app).await?;
            }
        }
        Ok(())
    }
}

// Helper function to parse string to enum
fn parse_driver_enum<T: FromStr + Default + std::fmt::Debug>(driver_str: String, default_driver: T, driver_name: &str) -> T
where <T as FromStr>::Err: std::fmt::Debug {
    match T::from_str(&driver_str.to_lowercase()) {
        Ok(driver_enum) => driver_enum,
        Err(e) => {
            Log::warning(format!( // Corrected: Pass debug flag first
                                      "Failed to parse {} driver '{}': {:?}. Using default: {:?}.",
                                      driver_name, driver_str, e, default_driver
            ));
            default_driver
        }
    }
}


#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,sockudo=debug".into()))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let initial_debug = std::env::var("DEBUG")
        .map(|v| v == "1" || v.to_lowercase() == "true")
        .unwrap_or(false);

    Log::info( "Starting Sockudo server initialization process...".to_string());

    let mut config = ServerOptions::default();

    config.debug = initial_debug;
    if let Ok(host) = std::env::var("HOST") { config.host = host; }
    if let Ok(port_str) = std::env::var("PORT") {
        if let Ok(port) = port_str.parse() { config.port = port; }
        else { Log::warning( format!("Failed to parse PORT env var: '{}'", port_str)); }
    }

    if let Ok(driver_str) = std::env::var("ADAPTER_DRIVER") {
        config.adapter.driver = parse_driver_enum(driver_str, config.adapter.driver, "Adapter");
    }
    if let Ok(driver_str) = std::env::var("CACHE_DRIVER") {
        config.cache.driver = parse_driver_enum(driver_str, config.cache.driver, "Cache");
    }
    if let Ok(driver_str) = std::env::var("QUEUE_DRIVER") {
        config.queue.driver = parse_driver_enum(driver_str, config.queue.driver, "Queue");
    }
    if let Ok(driver_str) = std::env::var("METRICS_DRIVER") {
        config.metrics.driver = parse_driver_enum(driver_str, config.metrics.driver, "Metrics");
    }
    if let Ok(driver_str) = std::env::var("APP_MANAGER_DRIVER") {
        config.app_manager.driver = parse_driver_enum(driver_str, config.app_manager.driver, "AppManager");
    }
    if let Ok(driver_str) = std::env::var("RATE_LIMITER_DRIVER") {
        config.rate_limiter.driver = parse_driver_enum(driver_str, config.rate_limiter.driver, "RateLimiter Backend");
    }

    if let Ok(val) = std::env::var("SSL_ENABLED") { config.ssl.enabled = val == "1" || val.to_lowercase() == "true"; }
    if let Ok(val) = std::env::var("SSL_CERT_PATH") { config.ssl.cert_path = val; }
    if let Ok(val) = std::env::var("SSL_KEY_PATH") { config.ssl.key_path = val; }
    if let Ok(val) = std::env::var("SSL_REDIRECT_HTTP") { config.ssl.redirect_http = val == "1" || val.to_lowercase() == "true"; }
    if let Ok(val_str) = std::env::var("SSL_HTTP_PORT") {
        if let Ok(port) = val_str.parse() { config.ssl.http_port = Some(port); }
        else { Log::warning( format!("Failed to parse SSL_HTTP_PORT env var: '{}'", val_str));}
    }

    if let Ok(redis_url) = std::env::var("REDIS_URL") {
        Log::info( format!("Using Redis URL from environment: {}", redis_url));
        config.adapter.redis.redis_pub_options.insert("url".to_string(), json!(redis_url.clone()));
        config.adapter.redis.redis_sub_options.insert("url".to_string(), json!(redis_url.clone()));
        config.cache.redis.url_override = Some(redis_url.clone());
        config.queue.redis.url_override = Some(redis_url.clone()); // Set url_override for queue
        config.rate_limiter.redis.url_override = Some(redis_url);
    }
    if let Ok(nats_url) = std::env::var("NATS_URL") {
        Log::info( format!("Using NATS URL from environment: {}", nats_url));
        config.adapter.nats.servers = vec![nats_url];
    }
    // For Redis prefixes from environment variables
    if let Ok(prefix) = std::env::var("CACHE_REDIS_PREFIX") { config.cache.redis.prefix = Some(prefix); }
    if let Ok(prefix) = std::env::var("QUEUE_REDIS_PREFIX") { config.queue.redis.prefix = Some(prefix); }
    if let Ok(prefix) = std::env::var("RATE_LIMITER_REDIS_PREFIX") { config.rate_limiter.redis.prefix = Some(prefix); }


    if let Ok(val) = std::env::var("METRICS_ENABLED") { config.metrics.enabled = val == "1" || val.to_lowercase() == "true"; }
    if let Ok(val_str) = std::env::var("METRICS_PORT") {
        if let Ok(port) = val_str.parse() { config.metrics.port = port; }
        else { Log::warning( format!("Failed to parse METRICS_PORT env var: '{}'", val_str));}
    }

    if let Ok(val) = std::env::var("RATE_LIMITER_ENABLED") { config.rate_limiter.enabled = val == "1" || val.to_lowercase() == "true"; }
    if let Ok(val_str) = std::env::var("RATE_LIMITER_API_MAX_REQUESTS") {
        if let Ok(num) = val_str.parse() { config.rate_limiter.api_rate_limit.max_requests = num; }
        else { Log::warning( format!("Failed to parse RATE_LIMITER_API_MAX_REQUESTS: '{}'", val_str));}
    }
    if let Ok(val_str) = std::env::var("RATE_LIMITER_API_WINDOW_SECONDS") {
        if let Ok(num) = val_str.parse() { config.rate_limiter.api_rate_limit.window_seconds = num; }
        else { Log::warning( format!("Failed to parse RATE_LIMITER_API_WINDOW_SECONDS: '{}'", val_str));}
    }
    if let Ok(val_str) = std::env::var("RATE_LIMITER_API_TRUST_HOPS") {
        if let Ok(num) = val_str.parse() { config.rate_limiter.api_rate_limit.trust_hops = Some(num); }
        else { Log::warning( format!("Failed to parse RATE_LIMITER_API_TRUST_HOPS: '{}'", val_str));}
    }

    let config_path = std::env::var("CONFIG_FILE").unwrap_or_else(|_| "config.json".to_string());
    if Path::new(&config_path).exists() {
        Log::info( format!("Loading configuration from file: {}", config_path));
        let mut file = File::open(&config_path).map_err(|e| Error::ConfigFileError(format!("Failed to open {}: {}", config_path, e)))?;
        let mut contents = String::new();
        file.read_to_string(&mut contents).map_err(|e| Error::ConfigFileError(format!("Failed to read {}: {}", config_path, e)))?;

        match from_str::<ServerOptions>(&contents) {
            Ok(file_config) => {
                config = file_config;
                Log::info( format!("Successfully loaded and applied configuration from {}", config_path));
            }
            Err(e) => {
                Log::error( format!("Failed to parse configuration file {}: {}. Using defaults and environment variables.", config_path, e));
            }
        }
    } else {
        Log::info( format!("No configuration file found at {}, using defaults and environment variables.", config_path));
    }

    let final_debug_enabled = config.debug;
    Log::info("Final configuration loaded. Initializing server components.".to_string());


    let server = match SockudoServer::new(config).await {
        Ok(s) => s,
        Err(e) => {
            Log::error(format!("Failed to create server: {}", e));
            return Err(e);
        }
    };

    Log::info("Starting Sockudo server services...".to_string());
    if let Err(e) = server.start().await {
        Log::error(format!("Server runtime error: {}", e));
        return Err(e);
    }

    Log::info("Server shutdown complete.".to_string());
    Ok(())
}

fn make_https(host: &str, uri: Uri, https_port: u16) -> core::result::Result<Uri, BoxError> {
    let mut parts = uri.into_parts();
    parts.scheme = Some(axum::http::uri::Scheme::HTTPS);
    if parts.path_and_query.is_none() {
        parts.path_and_query = Some("/".parse().unwrap());
    }
    let authority_val: Authority = host.parse()?;
    let bare_host_str = match authority_val.port() {
        Some(port_struct) => authority_val.as_str().strip_suffix(&format!(":{}", port_struct)).unwrap_or(authority_val.as_str()),
        None => authority_val.as_str(),
    };
    parts.authority = Some(format!("{}:{}", bare_host_str, https_port).parse()?);
    Uri::from_parts(parts).map_err(Into::into)
}

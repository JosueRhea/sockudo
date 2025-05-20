mod adapter;
mod app;
mod cache;
mod channel;
mod error;
mod http_handler;
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

use std::fs::File;
use std::io::Read;
use std::net::SocketAddr;
use std::path::Path;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::http::header::HeaderName;
use axum::http::uri::Authority;
use axum::http::Method;
use axum::http::{HeaderValue, StatusCode, Uri};
use axum::response::Redirect;
use axum::routing::{get, post};
use axum::{BoxError, Router, ServiceExt};

use axum_extra::extract::Host;
use axum_server::tls_rustls::RustlsConfig;
use clap::Parser;
use error::Error;
use serde_json::{from_str, json}; // Added json import
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

use crate::metrics::MetricsFactory;
use crate::options::{AdapterDriver, QueueDriver, ServerOptions}; // Added QueueDriver
use crate::queue::manager::{QueueManager, QueueManagerFactory};
use crate::rate_limiter::factory::RateLimiterFactory;
use crate::rate_limiter::middleware::IpKeyExtractor;
use crate::rate_limiter::RateLimiter;
use crate::webhook::integration::{BatchingConfig, WebhookConfig, WebhookIntegration};
use crate::ws_handler::handle_ws_upgrade;
use tower_http::cors::{AllowOrigin, CorsLayer};
// Import tracing and tracing_subscriber parts
use tracing::{error, info, warn, level_filters::LevelFilter}; // Added LevelFilter
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, fmt};

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
use crate::websocket::WebSocketRef;

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

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    #[arg(short, long)]
    config: String,
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
        info!(
            "Initializing Sockudo server with new configuration... Debug mode: {}",
            debug_enabled
        );

        let app_manager = AppManagerFactory::create(&config.app_manager, &config.database).await?;
        info!(
            "AppManager initialized with driver: {:?}",
            config.app_manager.driver
        );

        let connection_manager_box =
            AdapterFactory::create(&config.adapter, &config.database, debug_enabled).await?;
        let connection_manager_arc = Arc::new(Mutex::new(connection_manager_box));
        info!(
            "Adapter initialized with driver: {:?}",
            config.adapter.driver
        );

        let cache_manager =
            CacheManagerFactory::create(&config.cache, &config.database.redis, debug_enabled)
                .await
                .unwrap_or_else(|e| {
                    warn!(
                        "CacheManagerFactory creation failed: {}. Using a NoOp (Memory) Cache.",
                        e
                    );
                    let fallback_cache_options = config.cache.memory.clone();
                    Arc::new(Mutex::new(MemoryCacheManager::new(
                        "fallback_cache".to_string(),
                        fallback_cache_options,
                    )))
                });
        info!(
            "CacheManager initialized with driver: {:?}",
            config.cache.driver
        );

        let channel_manager = Arc::new(RwLock::new(ChannelManager::new(
            connection_manager_arc.clone(),
        )));
        let auth_validator = Arc::new(AuthValidator::new(app_manager.clone()));

        let metrics = if config.metrics.enabled {
            info!(
                "Initializing metrics with driver: {:?}",
                config.metrics.driver
            );
            match MetricsFactory::create(
                config.metrics.driver.as_ref(),
                config.metrics.port,
                Some(&config.metrics.prometheus.prefix),
            )
                .await
            {
                Some(metrics_driver) => {
                    info!("Metrics driver initialized successfully");
                    Some(metrics_driver)
                }
                None => {
                    warn!(
                        "Failed to initialize metrics driver, metrics will be disabled"
                    );
                    None
                }
            }
        } else {
            info!("Metrics are disabled in configuration");
            None
        };

        let http_api_rate_limiter_instance = if config.rate_limiter.enabled {
            RateLimiterFactory::create(
                &config.rate_limiter,
                &config.database.redis,
                debug_enabled
            ).await.unwrap_or_else(|e| {
                error!("Failed to initialize HTTP API rate limiter: {}. Using a permissive limiter.", e);
                Arc::new(rate_limiter::memory_limiter::MemoryRateLimiter::new(u32::MAX, 1)) // Permissive limiter
            })
        } else {
            info!(
                "HTTP API Rate limiting is globally disabled. Using a permissive limiter."
            );
            Arc::new(rate_limiter::memory_limiter::MemoryRateLimiter::new( // Permissive limiter
                                                                           u32::MAX,
                                                                           1,
            ))
        };
        info!(
            "HTTP API RateLimiter initialized (enabled: {}) with driver: {:?}",
            config.rate_limiter.enabled, config.rate_limiter.driver
        );

        let owned_default_queue_redis_url: String;
        let queue_redis_url_arg: Option<&str>;

        if let Some(url_override) = config.queue.redis.url_override.as_ref() {
            queue_redis_url_arg = Some(url_override.as_str());
        } else {
            owned_default_queue_redis_url = format!(
                "redis://{}:{}",
                config.database.redis.host, config.database.redis.port
            );
            queue_redis_url_arg = Some(&owned_default_queue_redis_url);
        }

        let queue_manager_opt = if config.queue.driver != QueueDriver::None {
            match QueueManagerFactory::create(
                config.queue.driver.as_ref(),
                queue_redis_url_arg,
                Some(
                    config
                        .queue
                        .redis
                        .prefix
                        .as_deref()
                        .unwrap_or("sockudo_queue:"),
                ),
                Some(config.queue.redis.concurrency as usize),
            )
                .await
            {
                Ok(queue_driver_impl) => {
                    info!(
                        "Queue manager initialized with driver: {:?}",
                        config.queue.driver
                    );
                    Some(Arc::new(QueueManager::new(queue_driver_impl)))
                }
                Err(e) => {
                    warn!("Failed to initialize queue manager with driver '{:?}': {}, queues will be disabled", config.queue.driver, e);
                    None
                }
            }
        } else {
            info!(
                "Queue driver set to None, queue manager will be disabled."
            );
            None
        };

        let webhook_redis_url = format!(
            "redis://{}:{}",
            config.database.redis.host, config.database.redis.port
        );

        let webhook_config_for_integration = WebhookConfig {
            enabled: true, // Assuming webhooks are generally enabled if configured
            batching: BatchingConfig {
                enabled: config.webhooks.batching.enabled,
                duration: config.webhooks.batching.duration,
            },
            queue_driver: config.queue.driver.as_ref().to_string(),
            redis_url: Some(webhook_redis_url),
            redis_prefix: Some(config.database.redis.key_prefix.clone() + "webhooks:"), // Ensure key_prefix exists
            redis_concurrency: Some(config.queue.redis.concurrency as usize),
            process_id: config.instance.process_id.clone(),
            debug: config.debug,
        };

        let webhook_integration = match WebhookIntegration::new(
            webhook_config_for_integration,
            app_manager.clone(),
        )
            .await
        {
            Ok(integration) => {
                info!(
                    "Webhook integration initialized successfully"
                );
                Arc::new(integration)
            }
            Err(e) => {
                warn!(
                    "Failed to initialize webhook integration: {}, webhooks will be disabled",
                    e
                );
                // Create a disabled WebhookIntegration as a fallback
                let disabled_config = WebhookConfig {
                    enabled: false,
                    ..Default::default() // Use default for other fields
                };
                // This should not fail if enabled is false
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
            Some(webhook_integration), // Pass the (potentially disabled) webhook_integration
            state.http_api_rate_limiter.clone(),
        ));

        // Set metrics for adapters
        if let Some(metrics_instance_arc) = &metrics {
            let mut connection_manager_guard = state.connection_manager.lock().await;
            // Get a mutable reference to the trait object inside the MutexGuard
            let adapter_as_any: &mut dyn std::any::Any = connection_manager_guard.as_any_mut();

            match config.adapter.driver {
                AdapterDriver::Redis => {
                    if let Some(adapter_mut) = adapter_as_any.downcast_mut::<RedisAdapter>() {
                        adapter_mut
                            .set_metrics(metrics_instance_arc.clone())
                            .await
                            .ok(); // .ok() converts Result to Option, ignoring error
                        info!("Set metrics for RedisAdapter");
                    } else {
                        warn!(
                            "Failed to downcast to RedisAdapter for metrics setup"
                        );
                    }
                }
                AdapterDriver::Nats => {
                    if let Some(adapter_mut) = adapter_as_any.downcast_mut::<NatsAdapter>() {
                        adapter_mut
                            .set_metrics(metrics_instance_arc.clone())
                            .await
                            .ok();
                        info!("Set metrics for NatsAdapter");
                    } else {
                        warn!(
                            "Failed to downcast to NatsAdapter for metrics setup"
                        );
                    }
                }
                AdapterDriver::RedisCluster => {
                    // Assuming RedisClusterAdapter also has a set_metrics method
                    if let Some(adapter_mut) = adapter_as_any.downcast_mut::<RedisClusterAdapter>()
                    {
                        // adapter_mut.set_metrics(metrics_instance_arc.clone()).await.ok(); // Uncomment if method exists
                        info!(
                            "Metrics setup for RedisClusterAdapter (call set_metrics if available)"
                        );
                    } else {
                        warn!(
                            "Failed to downcast to RedisClusterAdapter for metrics setup"
                        );
                    }
                }
                AdapterDriver::Local => {
                    // Assuming LocalAdapter might have a set_metrics method
                    if let Some(adapter_mut) = adapter_as_any.downcast_mut::<LocalAdapter>() {
                        // adapter_mut.set_metrics(metrics_instance_arc.clone()).await.ok(); // Uncomment if method exists
                        info!(
                            "Metrics setup for LocalAdapter (call set_metrics if applicable)"
                        );
                    } else {
                        warn!(
                            "Failed to downcast to LocalAdapter for metrics setup"
                        );
                    }
                }
            }
        }
        Ok(Self {
            config,
            state,
            handler,
        })
    }

    async fn init(&self) -> Result<()> {
        info!("Server init sequence started.");
        // Initialize AppManager first as other components might depend on it
        self.state.app_manager.init().await?; // Assuming AppManager has an init method

        // Initialize ConnectionManager (Adapter)
        { // Scope for MutexGuard
            let mut connection_manager = self.state.connection_manager.lock().await;
            connection_manager.init().await; // Assuming Adapter has an init method
        }


        // Register apps from configuration
        if !self.config.app_manager.array.apps.is_empty() {
            info!(
                "Registering {} apps from configuration",
                self.config.app_manager.array.apps.len()
            );
            let apps_to_register = self.config.app_manager.array.apps.clone();
            for app in apps_to_register {
                info!(
                    "Attempting to register app: id={}, key={}", app.id, app.key
                );
                match self.state.app_manager.find_by_id(&app.id).await {
                    Ok(Some(_existing_app)) => {
                        info!("App {} already exists, attempting to update.", app.id);
                        if let Err(update_err) = self.state.app_manager.update_app(app.clone()).await {
                            error!("Failed to update existing app {}: {}", app.id, update_err);
                        } else {
                            info!("Successfully updated app: {}", app.id);
                        }
                    }
                    Ok(None) => {
                        // App does not exist, create it
                        match self.state.app_manager.create_app(app.clone()).await {
                            Ok(_) => info!("Successfully registered new app: {}", app.id),
                            Err(create_err) => error!("Failed to register new app {}: {}", app.id, create_err),
                        }
                    }
                    Err(e) => {
                        // Error trying to find the app, could be a DB issue
                        error!("Error checking existence of app {}: {}. Skipping registration/update.", app.id, e);
                    }
                }
            }
        } else {
            info!(
                "No apps found in configuration, registering demo app"
            );
            let demo_app = App {
                id: "demo-app".to_string(),
                key: "demo-key".to_string(),
                secret: "demo-secret".to_string(),
                enable_client_messages: true,
                enabled: true,
                max_connections: 1000,
                max_client_events_per_second: 100,
                webhooks: Some(vec![crate::webhook::types::Webhook {
                    url: Some("http://localhost:3000/pusher/webhooks".parse().unwrap()), // Example URL
                    lambda_function: None,
                    lambda: None, // Assuming this is an Option<LambdaConfig> or similar
                    event_types: vec!["member_added".to_string(), "member_removed".to_string()],
                    filter: None, // Assuming Option<WebhookFilter>
                    headers: None, // Assuming Option<HashMap<String, String>>
                }]),
                ..Default::default()
            };
            match self.state.app_manager.create_app(demo_app).await {
                Ok(_) => info!("Successfully registered demo app"),
                Err(e) => warn!("Failed to register demo app: {}", e), // It might already exist from a previous run
            }
        }

        // Log registered apps
        match self.state.app_manager.get_apps().await {
            Ok(apps) => {
                info!("Server has {} registered apps:", apps.len());
                for app in apps {
                    info!(
                        "- App: id={}, key={}, enabled={}",
                        app.id, app.key, app.enabled
                    );
                }
            }
            Err(e) => warn!("Failed to retrieve registered apps: {}", e),
        }

        // Initialize Metrics
        if let Some(metrics) = &self.state.metrics {
            let metrics_guard = metrics.lock().await; // Lock the Mutex to get access
            if let Err(e) = metrics_guard.init().await { // Call init on the MetricsInterface implementor
                warn!("Failed to initialize metrics: {}", e);
            }
        }
        info!("Server init sequence completed.");
        Ok(())
    }


    fn configure_http_routes(&self) -> Router {
        let mut cors_builder = CorsLayer::new()
            .allow_methods(
                self.config
                    .cors
                    .methods
                    .iter()
                    .map(|s| Method::from_str(s).expect("Failed to parse CORS method"))
                    .collect::<Vec<_>>(),
            )
            .allow_headers(
                self.config
                    .cors
                    .allowed_headers
                    .iter()
                    .map(|s| HeaderName::from_str(s).expect("Failed to parse CORS header"))
                    .collect::<Vec<_>>(),
            );

        let use_allow_origin_any = self.config.cors.origin.contains(&"*".to_string())
            || self.config.cors.origin.contains(&"Any".to_string()) // Case-insensitive check
            || self.config.cors.origin.contains(&"any".to_string());

        if use_allow_origin_any {
            cors_builder = cors_builder.allow_origin(AllowOrigin::any());
            if self.config.cors.credentials {
                // This is a common pitfall with CORS.
                warn!("CORS config: 'Access-Control-Allow-Credentials' was true but 'Access-Control-Allow-Origin' is '*'. Forcing credentials to false to comply with CORS specification.");
                cors_builder = cors_builder.allow_credentials(false);
            }
            if self.config.cors.origin.len() > 1 { // If "*" is present with others
                warn!("CORS config: Wildcard '*' or 'Any' is present in origins list along with other specific origins. Wildcard will take precedence, allowing all origins.");
            }
        } else if !self.config.cors.origin.is_empty() {
            let origins = self
                .config
                .cors
                .origin
                .iter()
                .map(|s| {
                    s.parse::<HeaderValue>()
                        .expect("Failed to parse CORS origin")
                })
                .collect::<Vec<_>>();
            cors_builder = cors_builder.allow_origin(AllowOrigin::list(origins));
            // Only allow credentials if specific origins are set (not wildcard)
            cors_builder = cors_builder.allow_credentials(self.config.cors.credentials);
        } else {
            // No origins specified, and not wildcard. This usually means CORS is effectively off or very restrictive.
            warn!("CORS origins list is empty and no wildcard ('*' or 'Any') is specified. CORS might be highly restrictive or disabled depending on tower-http defaults. Consider setting origins or '*' for AllowOrigin::any().");
            if self.config.cors.credentials {
                warn!("CORS origins list is empty, and credentials set to true. Forcing credentials to false for safety as no origin is explicitly allowed.");
                cors_builder = cors_builder.allow_credentials(false);
            }
        }

        let cors = cors_builder;

        let rate_limiter_middleware_layer = if self.config.rate_limiter.enabled {
            if let Some(rate_limiter_instance) = &self.state.http_api_rate_limiter {
                let options = crate::rate_limiter::middleware::RateLimitOptions {
                    include_headers: true, // Include X-RateLimit-* headers
                    fail_open: false,      // If rate limiter fails, deny request
                    key_prefix: Some("api:".to_string()), // Prefix for keys in store
                };
                // Get trust_hops from config, default to 0 if not present
                let trust_hops = self
                    .config
                    .rate_limiter
                    .api_rate_limit
                    .trust_hops
                    .unwrap_or(0) as usize;
                let ip_key_extractor = IpKeyExtractor::new(trust_hops);

                info!(
                    "Applying custom rate limiting middleware with trust_hops: {}",
                    trust_hops
                );
                Some(
                    crate::rate_limiter::middleware::RateLimitLayer::with_options(
                        rate_limiter_instance.clone(),
                        ip_key_extractor,
                        options,
                    ),
                )
            } else {
                warn!("Rate limiting is enabled in config, but no RateLimiter instance found in server state for HTTP API. Rate limiting will not be applied.");
                None
            }
        } else {
            info!(
                "Custom HTTP API Rate limiting is disabled in configuration."
            );
            None
        };

        let mut router = Router::new()
            .route("/app/{appKey}", get(handle_ws_upgrade)) // Corrected Axum path param syntax
            .route("/apps/{appId}/events", post(events))
            .route("/apps/{appId}/batch_events", post(batch_events))
            .route("/apps/{appId}/channels", get(channels))
            .route("/apps/{appId}/channels/{channelName}", get(channel))
            .route(
                "/apps/{appId}/channels/{channelName}/users",
                get(channel_users),
            )
            .route(
                "/apps/{appId}/users/{userId}/terminate_connections",
                post(terminate_user_connections),
            )
            .route("/usage", get(usage))
            .route("/up/{appId}", get(up)) // Corrected Axum path param syntax
            .layer(cors); // Apply CORS layer

        // Apply rate limiter middleware if it was created
        if let Some(middleware) = rate_limiter_middleware_layer {
            router = router.layer(middleware);
        }

        router.with_state(self.handler.clone()) // Pass the handler state to all routes
    }


    fn configure_metrics_routes(&self) -> Router {
        Router::new()
            .route("/metrics", get(metrics))
            .with_state(self.handler.clone()) // Metrics endpoint also needs the handler for state
    }


    async fn start(&self) -> Result<()> {
        info!("Starting Sockudo server services (after init)...");

        let http_router = self.configure_http_routes();
        let metrics_router = self.configure_metrics_routes();

        let http_addr = self.get_http_addr();
        let metrics_addr = self.get_metrics_addr();

        if self.config.ssl.enabled
            && !self.config.ssl.cert_path.is_empty()
            && !self.config.ssl.key_path.is_empty()
        {
            info!("SSL is enabled, starting HTTPS server");
            let tls_config = self.load_tls_config().await?;

            // HTTP to HTTPS redirect server
            if self.config.ssl.redirect_http {
                let http_port = self.config.ssl.http_port.unwrap_or(80);
                // Use the configured host for the redirect server binding, default to 0.0.0.0 if parsing fails
                let host_ip = self
                    .config
                    .host
                    .parse::<std::net::IpAddr>()
                    .unwrap_or_else(|_| "0.0.0.0".parse().unwrap());
                let redirect_addr = SocketAddr::from((host_ip, http_port));
                info!(
                    "Starting HTTP to HTTPS redirect server on {}",
                    redirect_addr
                );
                let https_port = self.config.port; // The main HTTPS port
                let redirect_app =
                    Router::new().fallback(move |Host(host): Host, uri: Uri| async move {
                        match make_https(&host, uri, https_port) {
                            Ok(uri_https) => Ok(Redirect::permanent(&uri_https.to_string())),
                            Err(error) => {
                                error!(error = ?error, "failed to convert URI to HTTPS for redirect");
                                Err(StatusCode::BAD_REQUEST)
                            }
                        }
                    });
                match TcpListener::bind(redirect_addr).await {
                    Ok(redirect_listener) => {
                        tokio::spawn(async move {
                            if let Err(e) = axum::serve(
                                redirect_listener,
                                redirect_app.into_make_service_with_connect_info::<SocketAddr>(),
                            )
                                .await
                            {
                                error!("HTTP redirect server error: {}", e);
                            }
                        });
                    }
                    Err(e) => warn!("Failed to bind HTTP redirect server on {}: {}. Redirect will not be available.", redirect_addr, e),
                }
            }

            // Metrics server (always HTTP for Prometheus, typically)
            if self.config.metrics.enabled {
                if let Ok(metrics_listener) = TcpListener::bind(metrics_addr).await {
                    info!(
                        "Metrics server listening on http://{}", metrics_addr // Clarify HTTP
                    );
                    let metrics_router_clone = metrics_router.clone(); // Clone for the new task
                    tokio::spawn(async move {
                        if let Err(e) =
                            axum::serve(metrics_listener, metrics_router_clone.into_make_service())
                                .await
                        {
                            error!("Metrics server error: {}", e);
                        }
                    });
                } else {
                    warn!(
                        "Failed to start metrics server on {}: {}. Metrics will not be available.", metrics_addr, metrics_addr // Corrected variable
                    );
                }
            }

            // Main HTTPS server
            info!("HTTPS server listening on https://{}", http_addr); // Clarify HTTPS
            let running = self.state.running.clone();
            let server = axum_server::bind_rustls(http_addr, tls_config);
            tokio::select! {
                result = server.serve(http_router.into_make_service_with_connect_info::<SocketAddr>()) => {
                    if let Err(err) = result { error!("HTTPS server error: {}", err); }
                }
                _ = self.shutdown_signal() => {
                    info!("Shutdown signal received, stopping HTTPS server...");
                    running.store(false, Ordering::SeqCst);
                    // Graceful shutdown for axum_server might be handled by its drop or a specific method if available
                }
            }
        } else {
            // HTTP only mode
            info!("SSL is not enabled, starting HTTP server");
            let http_listener = TcpListener::bind(http_addr).await?;

            // Metrics server (HTTP)
            let metrics_listener_opt = if self.config.metrics.enabled {
                match TcpListener::bind(metrics_addr).await {
                    Ok(listener) => {
                        info!(
                            "Metrics server listening on http://{}", metrics_addr
                        );
                        Some(listener)
                    }
                    Err(e) => {
                        warn!("Failed to bind metrics server on {}: {}. Metrics will not be available.", metrics_addr, e);
                        None
                    }
                }
            } else {
                None
            };

            info!("HTTP server listening on http://{}", http_addr);
            let running = self.state.running.clone();

            if let Some(metrics_listener) = metrics_listener_opt {
                let metrics_router_clone = metrics_router.clone(); // Clone for the new task
                tokio::spawn(async move {
                    if let Err(e) =
                        axum::serve(metrics_listener, metrics_router_clone.into_make_service())
                            .await
                    {
                        error!("Metrics server error: {}", e);
                    }
                });
            }

            // Main HTTP server
            let http_server = axum::serve(
                http_listener,
                http_router.into_make_service_with_connect_info::<SocketAddr>(),
            ); // .with_graceful_shutdown(self.shutdown_signal()); // Add graceful shutdown

            tokio::select! {
                res = http_server => {
                    if let Err(err) = res { error!("HTTP server error: {}", err); }
                }
                _ = self.shutdown_signal() => {
                    info!("Shutdown signal received, stopping HTTP server...");
                    running.store(false, Ordering::SeqCst);
                }
            }
        }
        info!("Server main loop ended. Initiating final stop sequence."); // Clarified message
        Ok(())
    }


    async fn load_tls_config(&self) -> Result<RustlsConfig> {
        let cert_path = std::path::PathBuf::from(&self.config.ssl.cert_path);
        let key_path = std::path::PathBuf::from(&self.config.ssl.key_path);
        if !cert_path.exists() {
            return Err(Error::ConfigFileError(format!("SSL cert_path not found: {:?}", cert_path)));
        }
        if !key_path.exists() {
            return Err(Error::ConfigFileError(format!("SSL key_path not found: {:?}", key_path)));
        }
        RustlsConfig::from_pem_file(cert_path, key_path)
            .await
            .map_err(|e| Error::InternalError(format!("Failed to load TLS configuration: {}", e)))
    }

    async fn shutdown_signal(&self) {
        let ctrl_c = async {
            signal::ctrl_c()
                .await
                .expect("Failed to install Ctrl+C handler");
        };

        #[cfg(unix)]
        let terminate = async {
            signal::unix::signal(signal::unix::SignalKind::terminate())
                .expect("Failed to install signal handler")
                .recv()
                .await;
        };

        #[cfg(not(unix))]
        let terminate = std::future::pending::<()>(); // On non-Unix, this future never completes

        tokio::select! {
            _ = ctrl_c => info!("Ctrl+C received, initiating shutdown..."),
            _ = terminate => info!("Terminate signal received, initiating shutdown..."),
        }
        // The actual .stop() is called after server.start() returns in main
    }

    async fn stop(&self) -> Result<()> {
        info!("Stopping server...");
        self.state.running.store(false, Ordering::SeqCst); // Signal other tasks to stop
        let mut connection_manager = self.state.connection_manager.lock().await;
        // Gracefully shutdown adapter connections
        let namespaces = connection_manager.get_namespaces().await;
        match namespaces {
            Ok(namespaces) => {
                for (app_id, namespace) in namespaces {
                    let sockets = namespace.get_sockets().await?;
                    for (_socket_id, ws) in sockets {
                        connection_manager
                            .cleanup_connection(app_id.as_str(), WebSocketRef(ws))
                            .await;
                    }
                }
            }
            Err(e) => warn!("{}", format!("Failed to get namespaces: {}", e)),
        }
        drop(connection_manager); // Release lock

        // Disconnect from backend services
        {
            let mut cache_manager_locked = self.state.cache_manager.lock().await;
            if let Err(e) = cache_manager_locked.disconnect().await {
                warn!("Error disconnecting cache manager: {}", e);
            }
        }
        if let Some(queue_manager_arc) = &self.state.queue_manager {
            if let Err(e) = queue_manager_arc.disconnect().await {
                warn!("Error disconnecting queue manager: {}", e);
            }
        }
        // Add disconnect for app_manager if it has such a method
        // self.state.app_manager.disconnect().await?;


        info!("Waiting for shutdown grace period: {} seconds", self.config.shutdown_grace_period);
        tokio::time::sleep(Duration::from_secs(self.config.shutdown_grace_period)).await;
        info!("Server stopped");
        Ok(())
    }


    #[allow(dead_code)]
    pub async fn load_options_from_file<P: AsRef<Path>>(&mut self, path: P) -> Result<()> {
        let mut file = tokio::fs::File::open(path).await?;
        let mut contents = String::new();
        file.read_to_string(&mut contents).await?;
        let options: ServerOptions = from_str(&contents)?;
        self.config = options; // Replace current config
        info!(
            "Successfully loaded and applied options from file, app_manager config: {:?}",
            self.config.app_manager // Example to show new config is active
        );
        Ok(())
    }

    #[allow(dead_code)]
    async fn register_apps(&self, apps: Vec<App>) -> Result<()> {
        for app in apps {
            let existing_app = self.state.app_manager.find_by_id(&app.id).await?;
            if existing_app.is_some() {
                info!("Updating app during dynamic registration: {}", app.id);
                self.state.app_manager.update_app(app).await?;
            } else {
                info!("Registering new app dynamically: {}", app.id);
                self.state.app_manager.create_app(app).await?;
            }
        }
        Ok(())
    }
}

// Helper function to parse string to enum, with improved error message
fn parse_driver_enum<T: FromStr + Default + std::fmt::Debug>(
    driver_str: String,
    default_driver: T, // Pass the default value for logging
    driver_name: &str,
) -> T
where
    <T as FromStr>::Err: std::fmt::Debug, // Ensure the error type is Debug
{
    match T::from_str(&driver_str.to_lowercase()) {
        Ok(driver_enum) => driver_enum,
        Err(e) => {
            // Using eprintln! as logging might not be fully initialized when this is called
            eprintln!(
                "[CONFIG-WARN] Failed to parse {} driver from string '{}': {:?}. Using default: {:?}.",
                driver_name, driver_str, e, default_driver
            );
            default_driver
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // --- Part 1: Determine final config.debug ---
    let initial_debug_from_env = std::env::var("DEBUG")
        .map(|v| v == "1" || v.to_lowercase() == "true")
        .unwrap_or(false); // Default to false if DEBUG env var is not set

    let mut config = ServerOptions::default();
    config.debug = initial_debug_from_env; // Set initial debug state

    // --- Apply environment variables to default config (before loading from file) ---
    // This allows ENV to provide defaults if not in file, or be overridden by file.
    if let Ok(host) = std::env::var("HOST") { config.host = host; }
    if let Ok(port_str) = std::env::var("PORT") {
        if let Ok(port) = port_str.parse() { config.port = port; }
        else { eprintln!("[CONFIG-WARN] Failed to parse PORT env var: '{}'. Using default: {}", port_str, config.port); }
    }

    // Drivers
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

    // SSL
    if let Ok(val) = std::env::var("SSL_ENABLED") { config.ssl.enabled = val == "1" || val.to_lowercase() == "true"; }
    if let Ok(val) = std::env::var("SSL_CERT_PATH") { config.ssl.cert_path = val; }
    if let Ok(val) = std::env::var("SSL_KEY_PATH") { config.ssl.key_path = val; }
    if let Ok(val_str) = std::env::var("SSL_HTTP_PORT") {
        if let Ok(port) = val_str.parse() { config.ssl.http_port = Some(port); }
        else { eprintln!("[CONFIG-WARN] Failed to parse SSL_HTTP_PORT env var: '{}'", val_str); }
    }


    // Database - Redis specific (more granular than just REDIS_URL)
    if let Ok(val) = std::env::var("DATABASE_REDIS_HOST") { config.database.redis.host = val; }
    if let Ok(val_str) = std::env::var("DATABASE_REDIS_PORT") {
        if let Ok(port) = val_str.parse() { config.database.redis.port = port; }
        else { eprintln!("[CONFIG-WARN] Failed to parse DATABASE_REDIS_PORT env var: '{}'", val_str); }
    }
    if let Ok(val) = std::env::var("DATABASE_REDIS_PASSWORD") { config.database.redis.password = Some(val); }
    if let Ok(val_str) = std::env::var("DATABASE_REDIS_DB") {
        if let Ok(db) = val_str.parse() { config.database.redis.db = db; }
        else { eprintln!("[CONFIG-WARN] Failed to parse DATABASE_REDIS_DB env var: '{}'", val_str); }
    }
    if let Ok(val) = std::env::var("DATABASE_REDIS_KEY_PREFIX") { config.database.redis.key_prefix = val; }


    // Metrics specific
    if let Ok(val) = std::env::var("METRICS_ENABLED") { config.metrics.enabled = val == "1" || val.to_lowercase() == "true"; }
    if let Ok(val) = std::env::var("METRICS_HOST") { config.metrics.host = val; }
    if let Ok(val_str) = std::env::var("METRICS_PORT") {
        if let Ok(port) = val_str.parse() { config.metrics.port = port; }
        else { eprintln!("[CONFIG-WARN] Failed to parse METRICS_PORT env var: '{}'", val_str); }
    }
    if let Ok(val) = std::env::var("METRICS_PROMETHEUS_PREFIX") { config.metrics.prometheus.prefix = val; }

    // Instance specific
    if let Ok(val) = std::env::var("INSTANCE_PROCESS_ID") { config.instance.process_id = val; }
    if let Ok(val_str) = std::env::var("SHUTDOWN_GRACE_PERIOD") {
        if let Ok(period) = val_str.parse() { config.shutdown_grace_period = period; }
        else { eprintln!("[CONFIG-WARN] Failed to parse SHUTDOWN_GRACE_PERIOD env var: '{}'", val_str); }
    }


    // --- Load configuration from file ---
    // File settings will override ENV vars set above, except for `config.debug` if ENV DEBUG was explicitly set.
    // And high-priority ENV vars like REDIS_URL which are applied *after* file loading.
    let args = Args::parse();
    let mut config_path = args.config;
    if config_path.is_empty() {
        // Fallback to default path if not provided
        config_path = std::env::var("CONFIG_FILE").unwrap_or_else(|_| "src/config.json".to_string());
    }

    if Path::new(&config_path).exists() {
        println!("[PRE-LOG] Loading configuration from file: {}", config_path); // Basic print before logging init
        let mut file = File::open(&config_path).map_err(|e| {
            Error::ConfigFileError(format!("Failed to open {}: {}", config_path, e))
        })?;
        let mut contents = String::new();
        file.read_to_string(&mut contents).map_err(|e| {
            Error::ConfigFileError(format!("Failed to read {}: {}", config_path, e))
        })?;

        match from_str::<ServerOptions>(&contents) {
            Ok(file_config) => {
                config = file_config; // File config overrides previous defaults and ENV vars
                println!("[PRE-LOG] Successfully loaded and applied configuration from {}", config_path);
            }
            Err(e) => {
                eprintln!("[PRE-LOG-ERROR] Failed to parse configuration file {}: {}. Using defaults and environment variables already set.", config_path, e);
            }
        }
    } else {
        println!("[PRE-LOG] No configuration file found at {}, using defaults and environment variables.", config_path);
    }

    // --- Re-apply specific high-priority ENV vars (to override file) ---
    if let Ok(redis_url_env) = std::env::var("REDIS_URL") {
        println!("[PRE-LOG] Applying REDIS_URL environment variable override: {}", redis_url_env);
        // This will override any host/port/db/password from file or previous ENVs for these components
        config.adapter.redis.redis_pub_options.insert("url".to_string(), json!(redis_url_env.clone()));
        config.adapter.redis.redis_sub_options.insert("url".to_string(), json!(redis_url_env.clone()));
        config.cache.redis.url_override = Some(redis_url_env.clone());
        config.queue.redis.url_override = Some(redis_url_env.clone());
        config.rate_limiter.redis.url_override = Some(redis_url_env);
        // Note: This doesn't clear individual host/port fields in config.database.redis, but components using url_override will prefer it.
    }
    // Re-assert DEBUG from ENV if it should always override file
    if initial_debug_from_env {
        // If DEBUG env was set to true, make sure config.debug is true, regardless of file.
        if !config.debug {
            println!("[PRE-LOG] Overriding file config: DEBUG environment variable forces debug mode ON.");
            config.debug = true;
        }
    }


    // --- Part 2: Initialize logging using final config.debug ---
    let final_debug_is_enabled = config.debug;

    let default_log_directive_str = if final_debug_is_enabled {
        std::env::var("SOCKUDO_LOG_DEBUG").unwrap_or_else(|_| "info,sockudo=debug,tower_http=debug".to_string()) // Added tower_http
    } else {
        std::env::var("SOCKUDO_LOG_PROD").unwrap_or_else(|_| "info".to_string()) // Changed from "off" to "info" for minimal prod logging
    };

    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(default_log_directive_str));

    let subscriber_builder = fmt::Subscriber::builder().with_env_filter(env_filter);

    if final_debug_is_enabled {
        subscriber_builder
            .with_file(true)
            .with_line_number(true)
            .with_target(true) // Show module paths
            .finish()
            .init();
    } else {
        subscriber_builder
            .with_target(true) // Keep target for prod for some context
            .finish()
            .init();
    }

    info!("Logging initialized. Debug mode: {}. Effective RUST_LOG/default filter: '{}'",
        final_debug_is_enabled,
        EnvFilter::try_from_default_env().map(|f| f.to_string()).unwrap_or("None".to_string()) // Show the effective filter
    );


    // --- Part 3: Rest of the application logic ---
    info!("Starting Sockudo server initialization process with resolved configuration...");

    let server = match SockudoServer::new(config).await { // Pass the fully resolved config
        Ok(s) => s,
        Err(e) => {
            error!("Failed to create server instance: {}", e);
            return Err(e);
        }
    };

    if let Err(e) = server.init().await {
        error!("Failed to initialize server components: {}", e);
        return Err(e);
    }

    info!("Starting Sockudo server main services...");
    if let Err(e) = server.start().await {
        error!("Server runtime error: {}", e);
        // Attempt to stop server components even if start failed or exited with error
        if let Err(stop_err) = server.stop().await {
            error!("Error during server stop after runtime error: {}", stop_err);
        }
        return Err(e); // Propagate the original runtime error
    }

    // This part is reached if server.start() completes without error (e.g., due to shutdown signal)
    info!("Server main services concluded. Performing final shutdown...");
    if let Err(e) = server.stop().await {
        error!("Error during final server stop: {}", e);
    }

    info!("Sockudo server shutdown complete.");
    Ok(())
}

fn make_https(host: &str, uri: Uri, https_port: u16) -> core::result::Result<Uri, BoxError> {
    let mut parts = uri.into_parts();
    parts.scheme = Some(axum::http::uri::Scheme::HTTPS); // Use HTTPS scheme

    // Ensure path_and_query is present, default to "/"
    if parts.path_and_query.is_none() {
        parts.path_and_query = Some("/".parse().unwrap());
    }

    // Correctly parse host and replace/add port for HTTPS
    let authority_val: Authority = host.parse().map_err(|e| {
        format!("Failed to parse host '{}' into authority: {}", host, e)
    })?;

    let bare_host_str = authority_val.host(); // Get just the host part

    // Construct new authority with the HTTPS port
    parts.authority = Some(
        format!("{}:{}", bare_host_str, https_port)
            .parse()
            .map_err(|e| {
                format!(
                    "Failed to create new authority '{}:{}': {}",
                    bare_host_str, https_port, e
                )
            })?,
    );

    Uri::from_parts(parts).map_err(Into::into)
}

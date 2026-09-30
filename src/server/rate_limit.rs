//! Per-peer throttling with an explicit, desktop-approved bucket reset.

use std::{
    collections::HashMap,
    net::IpAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    Router,
    extract::{Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::Response,
};
use tower_governor::{
    GovernorError, GovernorLayer,
    governor::GovernorConfigBuilder,
    key_extractor::{KeyExtractor, PeerIpKeyExtractor},
};

const BURST: u32 = 50;
const REFILL: Duration = Duration::from_secs(1);
const FULL_COOLDOWN: Duration = Duration::from_secs(BURST as u64);
const IDLE_RETENTION: Duration = Duration::from_secs(60);
const PROMPT_TIMEOUT: Duration = Duration::from_secs(60);

type ResetPrompt = Arc<dyn Fn(IpAddr, Duration) -> bool + Send + Sync>;

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
struct BucketKey {
    ip: IpAddr,
    generation: u64,
}

struct Peer {
    generation: u64,
    pending: bool,
    quiet_until: Instant,
    last_seen: Instant,
}

#[derive(Clone, Default)]
struct ResettablePeerIp {
    peers: Arc<Mutex<HashMap<IpAddr, Peer>>>,
}

impl KeyExtractor for ResettablePeerIp {
    type Key = BucketKey;

    fn extract<T>(&self, request: &axum::http::Request<T>) -> Result<BucketKey, GovernorError> {
        // Use the same bucket throughout a request, even if another request's
        // approval resets the peer while this one is in flight.
        if let Some(key) = request.extensions().get::<BucketKey>() {
            return Ok(*key);
        }
        let ip = PeerIpKeyExtractor.extract(request)?;
        let now = Instant::now();
        let mut peers = self.peers.lock().unwrap();
        let peer = peers.entry(ip).or_insert(Peer {
            generation: 0,
            pending: false,
            quiet_until: now,
            last_seen: now,
        });
        peer.last_seen = now;
        Ok(BucketKey {
            ip,
            generation: peer.generation,
        })
    }
}

#[derive(Clone)]
struct Notifications {
    extractor: ResettablePeerIp,
    approval_lock: Arc<tokio::sync::Mutex<()>>,
    prompt: ResetPrompt,
}

impl Notifications {
    fn on_limit(&self, key: BucketKey, wait: Duration) {
        {
            let mut peers = self.extractor.peers.lock().unwrap();
            let Some(peer) = peers.get_mut(&key.ip) else {
                return;
            };
            if peer.generation != key.generation
                || peer.pending
                || Instant::now() < peer.quiet_until
            {
                return;
            }
            peer.pending = true;
        }
        let state = self.clone();
        tokio::spawn(async move {
            // Serialize with other host approval prompts, but never block an
            // HTTP response on a desktop interaction.
            let _guard = state.approval_lock.lock().await;
            let prompt = state.prompt.clone();
            // A notification daemon may ignore its requested UI timeout. Do
            // not let that hold up future host approvals indefinitely.
            let reset = matches!(
                tokio::time::timeout(
                    PROMPT_TIMEOUT,
                    tokio::task::spawn_blocking(move || prompt(key.ip, wait)),
                )
                .await,
                Ok(Ok(true))
            );
            let mut peers = state.extractor.peers.lock().unwrap();
            if let Some(peer) = peers.get_mut(&key.ip) {
                peer.pending = false;
                if reset {
                    // Governor has no keyed reset API. A fresh generation gives
                    // only this peer a new bucket; old buckets are pruned below.
                    peer.generation += 1;
                } else {
                    // Denial, dismissal, timeout, or notification failure:
                    // preserve throttling and don't nag during bucket recovery.
                    peer.quiet_until = Instant::now() + FULL_COOLDOWN;
                }
            }
        });
    }
}

async fn notify_on_limit(
    State(state): State<Notifications>,
    mut request: Request,
    next: Next,
) -> Response {
    let key = state.extractor.extract(&request).ok();
    if let Some(key) = key {
        request.extensions_mut().insert(key);
    }
    let mut response = next.run(request).await;
    if response.status() == StatusCode::TOO_MANY_REQUESTS {
        // Keep the standard Retry-After contract for clients waiting on refill.
        if !response.headers().contains_key(header::RETRY_AFTER) {
            if let Some(wait) = response.headers().get("x-ratelimit-after").cloned() {
                response.headers_mut().insert(header::RETRY_AFTER, wait);
            }
        }
        let wait = response
            .headers()
            .get(header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .map(Duration::from_secs)
            .unwrap_or(REFILL);
        if let Some(key) = key {
            state.on_limit(key, wait);
        }
    }
    response
}

pub(super) fn wrap<S>(router: Router<S>, approval_lock: Arc<tokio::sync::Mutex<()>>) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    wrap_with_notifications(
        router,
        Notifications {
            extractor: ResettablePeerIp::default(),
            approval_lock,
            prompt: Arc::new(super::notify::request_rate_limit_reset),
        },
    )
}

fn wrap_with_notifications<S>(router: Router<S>, notifications: Notifications) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    let extractor = notifications.extractor.clone();
    let governor_conf = Arc::new(
        GovernorConfigBuilder::default()
            .period(REFILL)
            .burst_size(BURST)
            .key_extractor(extractor.clone())
            .finish()
            .expect("valid governor config"),
    );
    let limiter = governor_conf.limiter().clone();
    let peers = extractor.peers.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(IDLE_RETENTION);
        loop {
            tick.tick().await;
            limiter.retain_recent();
            peers
                .lock()
                .unwrap()
                .retain(|_, peer| peer.pending || peer.last_seen.elapsed() < IDLE_RETENTION);
        }
    });
    router
        .layer(GovernorLayer::new(governor_conf))
        .layer(middleware::from_fn_with_state(
            notifications,
            notify_on_limit,
        ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, extract::ConnectInfo, routing::get};
    use std::{
        net::SocketAddr,
        sync::atomic::{AtomicUsize, Ordering},
    };
    use tower::ServiceExt;

    fn app(reset: bool) -> (Router, Notifications, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let notifications = Notifications {
            extractor: ResettablePeerIp::default(),
            approval_lock: Arc::new(tokio::sync::Mutex::new(())),
            prompt: Arc::new(move |_, _| {
                count.fetch_add(1, Ordering::SeqCst);
                reset
            }),
        };
        let router = Router::new().route("/health", get(|| async { "ok" }));
        (
            wrap_with_notifications(router, notifications.clone()),
            notifications,
            calls,
        )
    }

    async fn request(app: &Router, ip: IpAddr) -> Response {
        let mut request = Request::builder()
            .uri("/health")
            .body(Body::empty())
            .unwrap();
        request
            .extensions_mut()
            .insert(ConnectInfo(SocketAddr::new(ip, 12345)));
        app.clone().oneshot(request).await.unwrap()
    }

    async fn drain(app: &Router, ip: IpAddr) {
        for _ in 0..BURST {
            assert_eq!(request(app, ip).await.status(), StatusCode::OK);
        }
        let response = request(app, ip).await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(response.headers().contains_key(header::RETRY_AFTER));
    }

    async fn decision_finished(state: &Notifications, ip: IpAddr) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let pending = state
                    .extractor
                    .peers
                    .lock()
                    .unwrap()
                    .get(&ip)
                    .unwrap()
                    .pending;
                if !pending {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("prompt should complete");
    }

    #[tokio::test]
    async fn approval_refills_only_the_requesting_peer() {
        let (app, state, calls) = app(true);
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        let other: IpAddr = "127.0.0.2".parse().unwrap();
        let guard = state.approval_lock.lock().await;
        // Leave the other peer with just one token, without triggering a prompt.
        for _ in 0..BURST - 1 {
            assert_eq!(request(&app, other).await.status(), StatusCode::OK);
        }
        drain(&app, ip).await;
        for _ in 0..10 {
            assert_eq!(
                request(&app, ip).await.status(),
                StatusCode::TOO_MANY_REQUESTS
            );
        }
        drop(guard);
        decision_finished(&state, ip).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        for _ in 0..BURST {
            assert_eq!(request(&app, ip).await.status(), StatusCode::OK);
        }
        assert_eq!(request(&app, other).await.status(), StatusCode::OK);
        let guard = state.approval_lock.lock().await;
        assert_eq!(
            request(&app, other).await.status(),
            StatusCode::TOO_MANY_REQUESTS
        );
        // An old in-flight rejection must not prompt/reset the fresh generation.
        state.on_limit(BucketKey { ip, generation: 0 }, REFILL);
        assert!(
            !state
                .extractor
                .peers
                .lock()
                .unwrap()
                .get(&ip)
                .unwrap()
                .pending
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        drop(guard);
    }

    #[tokio::test]
    async fn denial_keeps_cooldown_and_suppresses_duplicate_prompts() {
        let (app, state, calls) = app(false);
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        let guard = state.approval_lock.lock().await;
        drain(&app, ip).await;
        for _ in 0..20 {
            assert_eq!(
                request(&app, ip).await.status(),
                StatusCode::TOO_MANY_REQUESTS
            );
        }
        drop(guard);
        decision_finished(&state, ip).await;
        for _ in 0..20 {
            assert_eq!(
                request(&app, ip).await.status(),
                StatusCode::TOO_MANY_REQUESTS
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            state
                .extractor
                .peers
                .lock()
                .unwrap()
                .get(&ip)
                .unwrap()
                .generation,
            0
        );
        tokio::time::sleep(REFILL + Duration::from_millis(50)).await;
        assert_eq!(request(&app, ip).await.status(), StatusCode::OK);
        assert_eq!(
            request(&app, ip).await.status(),
            StatusCode::TOO_MANY_REQUESTS
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn in_flight_requests_keep_their_original_bucket() {
        let extractor = ResettablePeerIp::default();
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        let mut request = Request::new(Body::empty());
        request
            .extensions_mut()
            .insert(ConnectInfo(SocketAddr::new(ip, 12345)));
        let original = extractor.extract(&request).unwrap();
        request.extensions_mut().insert(original);
        extractor
            .peers
            .lock()
            .unwrap()
            .get_mut(&ip)
            .unwrap()
            .generation += 1;
        assert_eq!(extractor.extract(&request).unwrap(), original);
        request.extensions_mut().remove::<BucketKey>();
        assert_eq!(extractor.extract(&request).unwrap().generation, 1);
    }

    #[tokio::test]
    async fn missing_peer_info_does_not_prompt() {
        let (app, _, calls) = app(false);
        let request = Request::builder()
            .uri("/health")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.oneshot(request).await.unwrap().status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}

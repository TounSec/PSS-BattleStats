//! MITM interception of Pixel Starships network traffic, scopred to `api.pixelstarships.com` only.
//! Every other host's TLS connection is left untouched (hudsucker intercepts everything by
//! default; we opt out at the CONNECT stage for anything that isn't our target.
//!
//! On every `BattleService/FinaliseBattle` POST, the raw request body is decoded (pss-codec),
//! parsed (pss-model), and stored (pss-store), then the request is forwarded to the real server
//! unmodified so the game client never notices the interception.
//!
//! On every `BattleService/CreateBattle` POST's *response* (matchmaking, not the battle result),
//! both ships' identity (user_id, ship_name, ship_level, faction, power_score) rides along for
//! free - see `pss_store::parse_create_battle_ships` and CLAUDE.md's ship-identity notes. The
//! response is gzip-compressed on the wire; `hudsucker::decode_response` strips that before we
//! read it, and we forward the now-decoded body onward unmodified (still valid HTTP, and what the
//! real client would have ended up with after its own gzip decoding anyway).

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use http_body_util::BodyExt;
use hudsucker::certificate_authority::RcgenAuthority;
use hudsucker::hyper::{Method, Request, Response};
use hudsucker::rcgen::{Issuer, KeyPair};
use hudsucker::rustls::crypto::aws_lc_rs;
use hudsucker::{Body, HttpContext, HttpHandler, Proxy, RequestOrResponse, decode_response};
use pss_model::FinaliseBattlePayload;
use pss_store::BattleStore;

const TARGET_HOST_SUFFIX: &str = "api.pixelstarships.com";

fn is_target_host(host: &str) -> bool {
    let host = host.split(':').next().unwrap_or(host);
    host == TARGET_HOST_SUFFIX || host.ends_with(&format!(".{TARGET_HOST_SUFFIX}"))
}

#[derive(Clone)]
struct BattleCaptureHandler {
    store: Arc<Mutex<BattleStore>>,
    /// Set by `handle_request` when the in-flight request is a `CreateBattle` POST, consumed by
    /// the matching `handle_response` call right after - safe because hudsucker calls both on the
    /// very same handler instance, strictly in order, for one request/response pair at a time (see
    /// `InternalProxy::proxy`), never interleaved with another exchange.
    pending_create_battle: bool,
}

impl BattleCaptureHandler {
    fn handle_finalise_battle(&self, raw: &[u8]) -> Result<()> {
        let decoded = pss_codec::decode_battle_report(raw).context("Failed to decode capture")?;
        let payload: FinaliseBattlePayload = serde_json::from_str(&decoded)
            .context("Decoded capture did not match the expected battleReport shape")?;

        let store = self
            .store
            .lock()
            .map_err(|_| anyhow::anyhow!("Battle store lock poisoned"))?;
        match store.insert_battle(&payload)? {
            pss_store::InsertOutcome::Inserted { id } => {
                tracing::info!(battle_id = id, "Captured and stored new battle");
            }
            pss_store::InsertOutcome::AlreadyExists { id } => {
                tracing::info!(battle_id = id, "Captured battle was already stored");
            }
        }

        Ok(())
    }

    fn handle_create_battle_response(&self, xml: &str) -> Result<()> {
        let (attacker, defender) = pss_store::parse_create_battle_ships(xml)
            .context("Failed to parse CreateBattle response")?;

        let store = self
            .store
            .lock()
            .map_err(|_| anyhow::anyhow!("Battle store lock poisoned"))?;
        store.upsert_ship_identity(&attacker)?;
        store.upsert_ship_identity(&defender)?;
        tracing::info!(
            attacker = %attacker.ship_name,
            defender = %defender.ship_name,
            "Captured ship identities from CreateBattle"
        );

        Ok(())
    }
}

impl HttpHandler for BattleCaptureHandler {
    /// Only intercept TLS for api.pixelstarships.com - every other host's connection is tunneled
    /// through untouched, never decrypted.
    async fn should_intercept_connect(&mut self, _ctx: &HttpContext, req: &Request<Body>) -> bool {
        req.uri().host().map(is_target_host).unwrap_or(false)
    }

    async fn handle_request(
        &mut self,
        _ctx: &HttpContext,
        req: Request<Body>,
    ) -> RequestOrResponse {
        self.pending_create_battle =
            req.method() == Method::POST && req.uri().path().contains("CreateBattle");

        let is_finalise_battle =
            req.method() == Method::POST && req.uri().path().contains("FinaliseBattle");
        if !is_finalise_battle {
            return req.into();
        }

        let (parts, body) = req.into_parts();
        let bytes = match body.collect().await {
            Ok(collected) => collected.to_bytes(),
            Err(err) => {
                tracing::warn!(?err, "Failed to read FinaliseBattle request body");
                return Request::from_parts(parts, Body::empty()).into();
            }
        };

        if let Err(err) = self.handle_finalise_battle(&bytes) {
            tracing::warn!(?err, "Failed to decode/store captured battle");
        }

        Request::from_parts(parts, Body::from(bytes)).into()
    }

    async fn handle_response(&mut self, _ctx: &HttpContext, res: Response<Body>) -> Response<Body> {
        if !std::mem::take(&mut self.pending_create_battle) {
            return res;
        }

        let res = match decode_response(res) {
            Ok(res) => res,
            Err(err) => {
                tracing::warn!(?err, "Failed to decode CreateBattle response body");
                return Response::builder()
                    .status(502)
                    .body(Body::empty())
                    .unwrap_or_else(|_| Response::new(Body::empty()));
            }
        };

        let (parts, body) = res.into_parts();
        let bytes = match body.collect().await {
            Ok(collected) => collected.to_bytes(),
            Err(err) => {
                tracing::warn!(?err, "Failed to read CreateBattle response body");
                return Response::from_parts(parts, Body::empty());
            }
        };

        if let Ok(xml) = std::str::from_utf8(&bytes) {
            if let Err(err) = self.handle_create_battle_response(xml) {
                tracing::warn!(?err, "Failed to parse/store CreateBattle ship identities");
            }
        } else {
            tracing::warn!("CreateBattle response body was not valid UTF-8");
        }

        Response::from_parts(parts, Body::from(bytes))
    }
}

/// A freshly generated self-signed root CA (PEM-encoded cert + private key), to be trusted by the
/// system/client whose traffic will be intercepted.
pub struct GenerateCa {
    pub cert_pem: String,
    pub key_pem: String,
}

/// Generates a new self-signed root CA for this proxy. Run once; save the returned PEM strings to
/// disk and trust the certificate on the machine running PSS client.
pub fn generate_ca() -> Result<GenerateCa> {
    use hudsucker::rcgen::{BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa};

    let mut params =
        CertificateParams::new(vec![]).context("Failed to create CA certificate params")?;
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);

    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "PSS-BattleStats local MITM CA");
    params.distinguished_name = dn;

    let key_pair = KeyPair::generate().context("Failed to generate CA key pair")?;
    let cert = params
        .self_signed(&key_pair)
        .context("Failed to self-sign CA certificate")?;

    Ok(GenerateCa {
        cert_pem: cert.pem(),
        key_pem: key_pair.serialize_pem(),
    })
}

/// Runs the MITM proxy until CTLR+C is pressed. Every captured `FinaliseBattlePayload` request is
/// decoded and stored via `store`; all other traffic passes through untouched.
pub async fn run(
    listen_addr: SocketAddr,
    ca_cert_pem: &str,
    ca_key_pem: &str,
    store: BattleStore,
) -> Result<()> {
    run_until(listen_addr, ca_cert_pem, ca_key_pem, store, async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await
}

async fn run_until(
    listen_addr: SocketAddr,
    ca_cert_pem: &str,
    ca_key_pem: &str,
    store: BattleStore,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let key_pair = KeyPair::from_pem(ca_key_pem).context("Failed to parse CA private key")?;
    let issuer = Issuer::from_ca_cert_pem(ca_cert_pem, key_pair)
        .context("Failed to parse CA certificate")?;
    let ca = RcgenAuthority::new(issuer, 1000, aws_lc_rs::default_provider());
    let handler = BattleCaptureHandler {
        store: Arc::new(Mutex::new(store)),
        pending_create_battle: false,
    };

    let proxy = Proxy::builder()
        .with_addr(listen_addr)
        .with_ca(ca)
        .with_rustls_connector(aws_lc_rs::default_provider())
        .with_http_handler(handler)
        .with_graceful_shutdown(shutdown)
        .build()
        .context("Failed to build proxy")?;

    proxy.start().await.context("Proxy exited with an error")
}

/// A running capture proxy that can be stopped on demand - for embedding in pss-gui, as an
/// alternative to `run`'s CTRL+C-only shutdown.
pub struct CaptureHandle {
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
    join_handle: Option<std::thread::JoinHandle<Result<()>>>,
}

impl CaptureHandle {
    /// Signals the proxy to stop. Non-blocking call `join` afterwards if you need to wait for it
    /// to actually finish.
    pub fn stop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }

    /// Stops the proxy (if not already) and waits for its background thread to exit.
    pub fn join(mut self) -> Result<()> {
        self.stop();

        match self.join_handle.take() {
            Some(handle) => handle
                .join()
                .map_err(|_| anyhow::anyhow!("Capture thread panicked"))?,
            None => Ok(()),
        }
    }
}

/// Starts the MITM proxy on its own thread (with its own tiny tokio runtime and its own SQLite
/// connection to `db_path`), returning a handle to stop it later. Use this from a synchronous
/// context like a GUI event loop; use `run` directly from an async CLI instead.
pub fn start(
    listen_addr: SocketAddr,
    ca_cert_pem: String,
    ca_key_pem: String,
    db_path: String,
) -> CaptureHandle {
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();

    let join_handle = std::thread::spawn(move || -> Result<()> {
        let store = BattleStore::open(&db_path)?;
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(run_until(
            listen_addr,
            &ca_cert_pem,
            &ca_key_pem,
            store,
            async {
                let _ = shutdown_rx.await;
            },
        ))
    });

    CaptureHandle {
        shutdown_tx: Some(shutdown_tx),
        join_handle: Some(join_handle),
    }
}

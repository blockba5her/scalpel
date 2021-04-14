use crate::backend::TLSPayload;
use crate::constants as c;
use crate::GlobalState;
use actix_web::{
    dev, error, http, middleware, web, App, HttpRequest, HttpResponse, HttpServer,
    Result as WebResult,
};
use openssl::ssl;
use std::io;
use std::sync::{atomic, Arc};

mod handler;

#[derive(serde::Deserialize)]
struct MdPathArgs {
    token: Option<String>,
    archive_type: String, // either data or data-saver
    chap_hash: String,
    image: String,
}

/// Request handler for the Actix web server
///
/// This is the main portion of the program, as it takes requests, verifies tokens, and then
/// interacts with the cache to stream the image to the client. However, most of this work is
/// offloaded to other modules.
///
/// - **Token Verification** is handled by the `tokens.rs` file ([`TokenVerifier`])
/// - **Cache HIT/MISS Logic** is handled by the `handler.rs` file
///
/// [`TokenVerifier`]: crate::tokens::TokenVerifier
async fn md_service(
    req: HttpRequest,
    path: web::Path<MdPathArgs>,
    gs: web::Data<Arc<GlobalState>>,
) -> WebResult<HttpResponse> {
    let peer_addr = req
        .connection_info()
        .realip_remote_addr()
        .map(|x| x.to_string())
        .unwrap_or_else(|| "-".to_string());

    // stop early if archive type is not valid
    if path.archive_type != "data" && path.archive_type != "data-saver" {
        let fmt = format!(
            "invalid archive type. must be one of {:?}",
            ["data", "data-saver"]
        );
        return Ok(HttpResponse::NotFound().body(fmt));
    }
    let saver = path.archive_type == "data-saver";

    // verify the token provided in the request url if verify tokens is enabled
    if !gs.config.skip_tokens && gs.backend.get_verify_tokens() {
        // unlock verifier mutex
        let v = gs.verifier.read().unwrap();

        match path
            .token
            .as_ref()
            .map(|token| v.verify_url_token(token, &path.chap_hash))
        {
            // result is good, so bypass
            Some(Ok(_)) => {}

            // there was an error with the token, so transform into response and return
            Some(Err(e)) => {
                log::warn!("({}) error verifying token in URL ({})", peer_addr, e);
                return Err(e.into());
            }

            // no token was even provided, so just say request is unauthorized
            None => return Err(error::ErrorUnauthorized("no token provided")),
        }
    }

    // increment request counter
    // only count requests if they've made it past token verification
    gs.request_counter.fetch_add(1, atomic::Ordering::Relaxed);

    // respond using CacheResponder, which will handle cache HITs and MISSes
    Ok(
        handler::response_from_cache(&peer_addr, &req, &gs, (&path.chap_hash, &path.image, saver))
            .await,
    )
}

/// Represents an error the HTTP error can cause where there is some io error binding to the port
/// specified in the client configuration
#[derive(Debug)]
pub struct PortBindError(io::Error);
impl std::fmt::Display for PortBindError {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(fmt, "error binding HTTP server to port (base: {})", self.0)
    }
}
impl std::error::Error for PortBindError {}

// TODO: Doc
fn spawn_http_server(
    gs: Arc<GlobalState>,
    acceptor: ssl::SslAcceptorBuilder,
) -> Result<dev::Server, PortBindError> {
    const COMPRESS: http::ContentEncoding = http::ContentEncoding::Identity;

    // obtain config options
    let (server_info, bind_addr) = {
        (
            format!(
                "{name} v{version} ({spec}) - {url}",
                name = c::PROG_NAME,
                version = c::VERSION,
                spec = c::SPEC,
                url = c::REPO_URL
            ),
            format!("{}:{}", &gs.config.bind_address, gs.config.port),
        )
    };

    // create the shared data object
    let data = web::Data::new(Arc::clone(&gs));

    // initialize server object
    let mut server = HttpServer::new(move || {
        App::new()
            .app_data(data.clone())
            .wrap(middleware::Logger::new(
                "(%a) \"%r\" (status = %s, size = %bb) in %Dms",
            ))
            .wrap(
                middleware::DefaultHeaders::new()
                    // Advertisement Headers
                    .header("Server", server_info.clone())
                    .header("X-Powered-By", "Actix Web")
                    .header("X-Version", c::VERSION)
                    // Headers required by client spec
                    .header("X-Content-Type-Options", "nosniff")
                    .header("Access-Control-Allow-Origin", "https://mangadex.org")
                    .header("Access-Control-Expose-Headers", "*")
                    .header("Cache-Control", "public, max-age=1209600")
                    .header("Timing-Allow-Origin", "https://mangadex.org"),
            )
            .wrap(middleware::Compress::new(COMPRESS))
            // regular MD@Home routes
            .route(
                "/{token}/{archive_type}/{chap_hash}/{image}", // tokenized route
                web::get().to(md_service),
            )
            .route(
                "/{archive_type}/{chap_hash}/{image}", // untokenized route
                web::get().to(md_service),
            )
    })
    .keep_alive(gs.config.keep_alive)
    .shutdown_timeout(60)
    .disable_signals();

    // manually set worker thread count to config amount
    if let Some(worker_threads) = gs.config.worker_threads {
        server = server.workers(worker_threads);
    }

    server
        .bind_openssl(&bind_addr, acceptor)
        .map_err(|x| PortBindError(x))
        .map(|s| s.run())
}

/// Error that represents all of the addressable errors of creating the HTTP Server.
#[derive(Debug)]
pub enum Error {
    Acceptor(ssl::Error),
    Port(PortBindError),
}
impl std::fmt::Display for Error {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Acceptor(e) => write!(fmt, "{}", e),
            Self::Port(e) => write!(fmt, "{}", e),
        }
    }
}
impl std::error::Error for Error {}

/// Lifecycle handler for the MD@Home HTTP server.
///
/// Responsible for spawning and respawning the HTTP server and converting the specified plaintext
/// certificates into the OpenSSL counterparts
pub struct HttpServerLifecycle {
    gs: Arc<GlobalState>,
    actix: dev::Server,
}

impl HttpServerLifecycle {
    /// Creates a new HTTP Server that will accept requests for the MD@Home client.
    ///
    /// This will take the certificate it should use and the current global state and return a new
    /// instance of `Self` if successful. Errors will be propagated up the stack.
    pub fn new(gs: Arc<GlobalState>, cert: &TLSPayload) -> Result<Self, Error> {
        // configures the SSL certificate with OpenSSL
        let acceptor = Self::cert_payload_to_acceptor(cert, gs.config.enforce_secure_tls)
            .map_err(|e| Error::Acceptor(e))?;

        // spawn the HTTP server and begin accepting requests
        let srv = spawn_http_server(Arc::clone(&gs), acceptor).map_err(|e| Error::Port(e))?;

        Ok(Self { gs, actix: srv })
    }

    /// Forcefully shuts down the last instance of the Actix Web Server, respawning with a new
    /// fullchain certificate and private key for SSL.
    // NOTE: Unfortunately, there is no way (to my knowledge) to change SSL cert while the Actix
    // Web server is running, therefore it must be shutdown and respawned
    pub async fn respawn_with_new_cert(&mut self, cert: &TLSPayload) -> Result<(), Error> {
        // stop old server immediately. if this were graceful, it would wait for all keep-alive
        // connections to close off first.
        self.shutdown(false).await;

        let acceptor = Self::cert_payload_to_acceptor(cert, self.gs.config.enforce_secure_tls)
            .map_err(|e| Error::Acceptor(e))?;

        let srv = spawn_http_server(Arc::clone(&self.gs), acceptor).map_err(|e| Error::Port(e))?;
        self.actix = srv;

        Ok(())
    }

    /// Converts a [`TLSPayload`] into an Ssl Builder that ActixWeb will use for TLS
    fn cert_payload_to_acceptor(
        cert: &TLSPayload,
        secure_tls: bool,
    ) -> Result<ssl::SslAcceptorBuilder, ssl::Error> {
        use openssl::pkey::PKey;
        use openssl::rsa::Rsa;
        use openssl::x509::X509;

        let mut builder = ssl::SslAcceptor::mozilla_intermediate(ssl::SslMethod::tls())?;

        // push the full-chain certificate into the SslAcceptorBuilder
        let full_chain = X509::stack_from_pem(cert.certificate.as_bytes())?;
        let mut full_chain_iter = full_chain.iter();
        if let Some(x509) = full_chain_iter.next() {
            builder.set_certificate(x509.as_ref())?;
        }
        for next_chain in full_chain_iter {
            builder.add_extra_chain_cert(next_chain.clone())?;
        }

        // push the private key to the SslAcceptorBuilder
        let priv_key = Rsa::private_key_from_pem(cert.private_key.as_bytes())?;
        builder.set_private_key(PKey::from_rsa(priv_key)?.as_ref())?;

        // set minimum ssl version based on config
        builder.set_min_proto_version(Some(if secure_tls {
            ssl::SslVersion::TLS1_2
        } else {
            ssl::SslVersion::TLS1
        }))?;

        Ok(builder)
    }

    /// Wrapper for the internal Actix Web server stop function
    pub async fn shutdown(&self, graceful: bool) {
        self.actix.stop(graceful).await
    }
}

//
//  Lots of code pulled from hyper's examples (specifically multi-server and http proxy)
//
//

use bytes::Bytes;
use futures_util::future::join;
use http_body_util::{combinators::BoxBody as OtherBoxBody, BodyExt, Empty, Full};
use hyper::client::conn::http1::Builder;
use hyper::http;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::upgrade::Upgraded;
use hyper::{body::Incoming as IncomingBody, header, Method, Request, Response, StatusCode};

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::sync::RwLock;

#[path = "support/mod.rs"]
mod support;
use support::TokioIo;

type GenericError = Box<dyn std::error::Error + Send + Sync>;
type Result<T> = std::result::Result<T, GenericError>;
type BoxBody = OtherBoxBody<Bytes, hyper::Error>;
type Rule = dyn Fn(String) -> bool;

//static PROXY_ADDR: std::net::SocketAddr = SocketAddr::from(([127, 0, 0, 1], 8100));
//static MANAGE_ADDR: std::net::SocketAddr = SocketAddr::from(([127, 0, 0, 1], 8101));
static PROXY_ADDR: &str = "127.0.0.1:8100";
static MANAGE_ADDR: &str = "127.0.0.1:8101";

static INDEX: &[u8] = b"<a href=\"test.html\">test.html</a>";
static _INTERNAL_SERVER_ERROR: &[u8] = b"Internal Server Error";
static NOTFOUND: &[u8] = b"Not Found";
static POST_DATA: &str = r#"{"original": "data"}"#;

#[derive(Clone, Debug)]
struct RuleBook {
    //rules: Vec<Box<Rule>>,
    rules: Vec<u8>,
}

impl RuleBook {
    fn new() -> RuleBook {
        let b = RuleBook { rules: Vec::new() };
        //b.rules.push(Box::new(|s: String| -> bool {
        //    if s.contains("watermark") {
        //        return true;
        //    } else {
        //        return false;
        //    }
        //}));
        return b;
    }
}

// To try this example:
// 1. cargo run --example http_proxy
// 2. config http_proxy in command line
//    $ export http_proxy=http://127.0.0.1:8100
//    $ export https_proxy=http://127.0.0.1:8100
// 3. send requests
//    $ curl -i https://www.some_domain.com/

#[deny(warnings)]
#[tokio::main]
async fn main() -> Result<()> {
    let rules = RuleBook::new();
    let rules_lock = Arc::new(RwLock::new(rules));
    let c_lock = rules_lock.clone();

    let proxy_addr: SocketAddr = PROXY_ADDR.parse().unwrap();
    let manage_addr: SocketAddr = MANAGE_ADDR.parse().unwrap();

    let p = async move {
        let listener = TcpListener::bind(proxy_addr).await.unwrap();
        println!("Listening on http://{}", proxy_addr);

        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let io = TokioIo::new(stream);

            let book_copy = Arc::clone(&c_lock);

            let _ = tokio::task::spawn(async move {
                let rule_lock_loop = book_copy.read().await;
                if let Err(err) = http1::Builder::new()
                    .preserve_header_case(true)
                    .title_case_headers(true)
                    .serve_connection(
                        io,
                        service_fn(move |req| proxy(req, rule_lock_loop.clone())),
                    )
                    .await
                {
                    println!("Failed to serve connection: {:?}", err);
                }
            })
            .await;
        }
    };

    let m = async move {
        let listener = TcpListener::bind(manage_addr).await.unwrap();
        println!("Listening on http://{}", manage_addr);

        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let io = TokioIo::new(stream);

            let book_copy = Arc::clone(&rules_lock);

            let _ = tokio::task::spawn(async move {
                if let Err(err) = http1::Builder::new()
                    .preserve_header_case(true)
                    .title_case_headers(true)
                    .serve_connection(
                        io,
                        service_fn(move |req| response_manager(req, Arc::clone(&book_copy))),
                    )
                    .await
                {
                    println!("Failed to serve connection: {:?}", err);
                }
            })
            .await;
        }
    };

    _ = join(p, m).await;
    Ok(())
}

async fn proxy(
    req: Request<hyper::body::Incoming>,
    _rule_lock: RuleBook,
) -> Result<Response<BoxBody>> {
    println!("req: {:?}", req);

    if Method::CONNECT == req.method() {
        // Received an HTTP request like:
        // ```
        // CONNECT www.domain.com:443 HTTP/1.1
        // Host: www.domain.com:443
        // Proxy-Connection: Keep-Alive
        // ```
        //
        // When HTTP method is CONNECT we should return an empty body
        // then we can eventually upgrade the connection and talk a new protocol.
        //
        // Note: only after client received an empty body with STATUS_OK can the
        // connection be upgraded, so we can't return a response inside
        // `on_upgrade` future.
        if let Some(addr) = host_addr(req.uri()) {
            tokio::task::spawn(async move {
                match hyper::upgrade::on(req).await {
                    Ok(upgraded) => {
                        if let Err(e) = tunnel(upgraded, addr).await {
                            eprintln!("server io error: {}", e);
                        };
                    }
                    Err(e) => eprintln!("upgrade error: {}", e),
                }
            });

            Ok(Response::new(empty()))
        } else {
            eprintln!("CONNECT host is not socket addr: {:?}", req.uri());
            let mut resp = Response::new(full("CONNECT must be to a socket address"));
            *resp.status_mut() = http::StatusCode::BAD_REQUEST;

            Ok(resp)
        }
    } else {
        let host = req.uri().host().expect("uri has no host");
        let port = req.uri().port_u16().unwrap_or(80);

        let stream = TcpStream::connect((host, port)).await.unwrap();
        let io = TokioIo::new(stream);

        let (mut sender, conn) = Builder::new()
            .preserve_header_case(true)
            .title_case_headers(true)
            .handshake(io)
            .await?;
        tokio::task::spawn(async move {
            if let Err(err) = conn.await {
                println!("Connection failed: {:?}", err);
            }
        });

        let resp = sender.send_request(req).await?;
        Ok(resp.map(|b| b.boxed()))
    }
}

async fn api_post_response(_req: Request<IncomingBody>) -> Result<Response<BoxBody>> {
    let response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(full(NOTFOUND))?;
    Ok(response)
}

async fn client_request_response() -> Result<Response<BoxBody>> {
    let req = Request::builder()
        .method(Method::POST)
        .uri(MANAGE_ADDR)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(POST_DATA)))
        .unwrap();

    let host = req.uri().host().expect("uri has no host");
    let port = req.uri().port_u16().expect("uri has no port");
    let stream = TcpStream::connect(format!("{}:{}", host, port)).await?;
    let io = TokioIo::new(stream);

    let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await?;

    tokio::task::spawn(async move {
        if let Err(err) = conn.await {
            println!("Connection error: {:?}", err);
        }
    });

    let web_res = sender.send_request(req).await?;

    let res_body = web_res.into_body().boxed();

    Ok(Response::new(res_body))
}

async fn response_manager(
    req: Request<IncomingBody>,
    rule_lock: Arc<RwLock<RuleBook>>,
) -> Result<Response<BoxBody>> {
    println!("req: {:?}", req);
    println!("rulebook: {:?}", rule_lock.write().await);
    match (req.method(), req.uri().path()) {
        (&Method::GET, "/") | (&Method::GET, "/index.html") => Ok(Response::new(full(INDEX))),
        (&Method::GET, "/test.html") => client_request_response().await,
        (&Method::POST, "/json_api") => api_post_response(req).await,
        _ => {
            // Return 404 not found response.
            Ok(Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(full(NOTFOUND))
                .unwrap())
        }
    }
}

fn host_addr(uri: &http::Uri) -> Option<String> {
    uri.authority().and_then(|auth| Some(auth.to_string()))
}

fn empty() -> BoxBody {
    Empty::<Bytes>::new()
        .map_err(|never| match never {})
        .boxed()
}

fn full<T: Into<Bytes>>(chunk: T) -> BoxBody {
    Full::new(chunk.into())
        .map_err(|never| match never {})
        .boxed()
}

// Create a TCP connection to host:port, build a tunnel between the connection and
// the upgraded connection
async fn tunnel(upgraded: Upgraded, addr: String) -> std::io::Result<()> {
    // Connect to remote server
    let mut server = TcpStream::connect(addr).await?;
    let mut upgraded = TokioIo::new(upgraded);

    // Proxying data
    let (from_client, from_server) =
        tokio::io::copy_bidirectional(&mut upgraded, &mut server).await?;

    // Print message when done
    println!(
        "client wrote {} bytes and received {} bytes",
        from_client, from_server
    );

    Ok(())
}

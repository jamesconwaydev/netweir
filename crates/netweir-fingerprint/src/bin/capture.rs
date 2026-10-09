//! Prints the fingerprint of the next connection to https://localhost:PORT.
//!
//!     cargo run -p netweir-fingerprint --bin capture -- 8443 [--http1]
//!
//! then open https://localhost:8443/ in the browser being captured.

#[tokio::main]
async fn main() {
    let port = std::env::args()
        .nth(1)
        .and_then(|p| p.parse().ok())
        .unwrap_or(8443);
    let http1 = std::env::args().any(|a| a == "--http1");
    let mut server = if http1 {
        netweir_fingerprint::Server::start_http1(port).await
    } else {
        netweir_fingerprint::Server::start_on(port).await
    }
    .expect("bind");
    eprintln!("listening on https://localhost:{}/", server.port);
    while let Some(capture) = server.next().await {
        // Browsers open extra connections (favicon, preconnect); print each.
        println!("{}", serde_json::to_string_pretty(&capture).unwrap());
    }
}

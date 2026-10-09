//! Prints the fingerprint of the next connection to https://localhost:PORT.
//!
//!     cargo run -p netweir-fingerprint --bin capture -- 8443 [--http1]
//!
//! then open https://localhost:8443/ in the browser being captured. The
//! certificate is kept in ~/.netweir/capture/ ($NETWEIR_HOME moves it), so
//! a browser that can't be told to skip the check, such as Safari, can be
//! made to trust cert.pem there once.

#[tokio::main]
async fn main() {
    let port = std::env::args()
        .nth(1)
        .and_then(|p| p.parse().ok())
        .unwrap_or(8443);
    let http1 = std::env::args().any(|a| a == "--http1");
    let home = std::env::var_os("NETWEIR_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".netweir")))
        .expect("$HOME or $NETWEIR_HOME");
    let dir = home.join("capture");
    let mut server = netweir_fingerprint::Server::start_trusted(port, http1, &dir)
        .await
        .expect("bind");
    eprintln!("certificate: {}", dir.join("cert.pem").display());
    eprintln!("listening on https://localhost:{}/", server.port);
    while let Some(capture) = server.next().await {
        // Browsers open extra connections (favicon, preconnect); print each.
        println!("{}", serde_json::to_string_pretty(&capture).unwrap());
    }
}

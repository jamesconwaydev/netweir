use crate::client_hello::is_grease;
use crate::server::Capture;

/// Everything about `actual` that would tell a server it is not the browser
/// `expected` was captured from. Empty means indistinguishable.
///
/// GREASE values and extension order are ignored, because browsers
/// randomise both on every connection. So is whether the TLS session was
/// resumed (the pre_shared_key extension, which also changes JA4), since
/// that depends on connection history, not on the browser. The `:authority`
/// and `Host` values are ignored because they name the test server.
pub fn differences(expected: &Capture, actual: &Capture) -> Vec<String> {
    let mut out = Vec::new();
    let mut check = |what: &str, a: String, b: String| {
        if a != b {
            out.push(format!("{what}: expected {a}, got {b}"));
        }
    };
    let strip = |v: &[u16]| -> Vec<u16> { v.iter().copied().filter(|&x| !is_grease(x)).collect() };
    let (e, a) = (&expected.client_hello, &actual.client_hello);
    const PRE_SHARED_KEY: u16 = 0x0029;
    let resumed = |c: &Vec<u16>| c.contains(&PRE_SHARED_KEY);
    if resumed(&e.extensions) == resumed(&a.extensions) {
        check("ja4", expected.ja4.clone(), actual.ja4.clone());
    }
    let fresh =
        |set: Vec<u16>| -> Vec<u16> { set.into_iter().filter(|&x| x != PRE_SHARED_KEY).collect() };
    check(
        "ciphers",
        format!("{:04x?}", strip(&e.ciphers)),
        format!("{:04x?}", strip(&a.ciphers)),
    );
    check(
        "extensions",
        format!("{:04x?}", fresh(e.extension_set())),
        format!("{:04x?}", fresh(a.extension_set())),
    );
    check(
        "groups",
        format!("{:04x?}", strip(&e.supported_groups)),
        format!("{:04x?}", strip(&a.supported_groups)),
    );
    check(
        "key shares",
        format!("{:04x?}", strip(&e.key_share_groups)),
        format!("{:04x?}", strip(&a.key_share_groups)),
    );
    check(
        "signature algorithms",
        format!("{:04x?}", strip(&e.signature_algorithms)),
        format!("{:04x?}", strip(&a.signature_algorithms)),
    );
    check(
        "supported versions",
        format!("{:04x?}", strip(&e.supported_versions)),
        format!("{:04x?}", strip(&a.supported_versions)),
    );
    check("alpn", format!("{:?}", e.alpn), format!("{:?}", a.alpn));
    check("alps", format!("{:?}", e.alps), format!("{:?}", a.alps));
    check(
        "certificate compression",
        format!("{:?}", e.cert_compression),
        format!("{:?}", a.cert_compression),
    );
    check(
        "psk modes",
        format!("{:?}", e.psk_modes),
        format!("{:?}", a.psk_modes),
    );
    check(
        "ec point formats",
        format!("{:?}", e.ec_point_formats),
        format!("{:?}", a.ec_point_formats),
    );
    check(
        "record size limit",
        format!("{:?}", e.record_size_limit),
        format!("{:?}", a.record_size_limit),
    );
    check(
        "trust anchors",
        format!("{:?}", e.trust_anchors),
        format!("{:?}", a.trust_anchors),
    );
    check(
        "protocol",
        expected.protocol.clone(),
        actual.protocol.clone(),
    );
    match (&expected.http2, &actual.http2) {
        (Some(e2), Some(a2)) => {
            check("akamai", e2.akamai.clone(), a2.akamai.clone());
            check(
                "headers priority",
                format!("{:?}", e2.headers_priority),
                format!("{:?}", a2.headers_priority),
            );
        }
        (e2, a2) => check(
            "http2",
            format!("{}", e2.is_some()),
            format!("{}", a2.is_some()),
        ),
    }
    let names = |c: &Capture| c.headers.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>();
    check(
        "header order",
        format!("{:?}", names(expected)),
        format!("{:?}", names(actual)),
    );
    for ((k, ev), (_, av)) in expected.headers.iter().zip(&actual.headers) {
        if k != ":authority" && !k.eq_ignore_ascii_case("host") {
            check(&format!("header {k}"), ev.clone(), av.clone());
        }
    }
    out
}

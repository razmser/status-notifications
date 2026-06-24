//! Optional DNS-over-HTTPS (DoH) resolver.
//!
//! Resolves feed hostnames by querying a DoH-JSON endpoint that is reached *by
//! IP* (default Cloudflare `1.1.1.1`), so the lookup bypasses a hijacked local
//! resolver (e.g. a fake-IP VPN that points every name at an unroutable
//! `198.18.x.x` address). ureq still performs the TLS handshake against the
//! original hostname, so certificate validation is unaffected — we only swap out
//! the name→IP step.
//!
//! Wired in via ureq's pluggable [`Resolver`]; enabled by the `doh` config key.

use std::io::Read;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use serde::Deserialize;
use ureq::Error;
use ureq::config::Config as UreqConfig;
use ureq::http::Uri;
use ureq::unversioned::resolver::{ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::NextTimeout;

/// Default DoH-JSON endpoint used when `doh = true`. Addressed by IP so reaching
/// the resolver itself needs no DNS.
pub const DEFAULT_DOH_ENDPOINT: &str = "https://1.1.1.1/dns-query";

/// Timeout for a single DoH lookup. Kept short so a slow resolver can't stall a
/// feed fetch for the whole global feed timeout.
const DOH_TIMEOUT: Duration = Duration::from_secs(5);

/// Cap on the DoH response body (defensive; answers are tiny).
const MAX_DOH_BYTES: u64 = 64 * 1024;

/// `User-Agent` sent on DoH queries.
const USER_AGENT: &str = concat!("status-notifications/", env!("CARGO_PKG_VERSION"));

/// DNS record types we accept from the DoH answer: `A` (IPv4) and `AAAA` (IPv6).
const RTYPE_A: u16 = 1;
const RTYPE_AAAA: u16 = 28;

/// Cap on resolved addresses handed back to ureq (matches ureq's internal
/// `MAX_ADDRS`); pushing beyond the backing [`ResolvedSocketAddrs`] would panic.
const MAX_RESOLVED_ADDRS: usize = 16;

/// A ureq [`Resolver`] that performs name resolution over DoH-JSON.
pub struct DohResolver {
    endpoint: String,
    agent: ureq::Agent,
}

impl std::fmt::Debug for DohResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Deliberately omit the inner agent; surface only the endpoint.
        f.debug_struct("DohResolver")
            .field("endpoint", &self.endpoint)
            .finish()
    }
}

impl DohResolver {
    /// Build a resolver that queries `endpoint` (a DoH-JSON URL).
    pub fn new(endpoint: String) -> Self {
        // The querying agent uses ureq's *default* resolver, so it never calls
        // back into this DoH resolver. Because the endpoint is addressed by IP,
        // the default resolver handles it without any DNS.
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(DOH_TIMEOUT))
            .user_agent(USER_AGENT)
            .build()
            .into();
        Self { endpoint, agent }
    }

    /// Query the DoH endpoint for `host`, returning the resolved IPs.
    fn query(&self, host: &str) -> Result<Vec<IpAddr>, Box<dyn std::error::Error + Send + Sync>> {
        // DoH-JSON: GET <endpoint>?name=<host>&type=A with the dns-json accept
        // header. Hostnames contain only URL-safe characters, so no escaping.
        let url = format!("{}?name={host}&type=A", self.endpoint);
        let mut raw = Vec::new();
        self.agent
            .get(&url)
            .header("accept", "application/dns-json")
            .call()?
            .into_body()
            .into_reader()
            .take(MAX_DOH_BYTES)
            .read_to_end(&mut raw)?;

        let parsed: DohResponse = serde_json::from_slice(&raw)?;
        Ok(parsed.ips())
    }
}

impl Resolver for DohResolver {
    fn resolve(
        &self,
        uri: &Uri,
        _config: &UreqConfig,
        _timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, Error> {
        let host = uri.host().ok_or(Error::HostNotFound)?;
        let port = uri
            .port_u16()
            .or_else(|| match uri.scheme_str() {
                Some("https") => Some(443),
                Some("http") => Some(80),
                _ => None,
            })
            .ok_or(Error::HostNotFound)?;

        // An IP literal needs no lookup — return it directly. This also keeps a
        // numeric feed host working and avoids a pointless DoH round-trip.
        if let Ok(ip) = host.parse::<IpAddr>() {
            let mut result = self.empty();
            result.push(SocketAddr::new(ip, port));
            return Ok(result);
        }

        let ips = self
            .query(host)
            .map_err(|e| Error::Other(format!("DoH resolution of {host} failed: {e}").into()))?;

        let mut result = self.empty();
        for ip in ips.into_iter().take(MAX_RESOLVED_ADDRS) {
            result.push(SocketAddr::new(ip, port));
        }

        if result.is_empty() {
            Err(Error::HostNotFound)
        } else {
            Ok(result)
        }
    }
}

/// A DoH-JSON response (only the answer section is of interest).
#[derive(Debug, Deserialize)]
struct DohResponse {
    #[serde(default, rename = "Answer")]
    answer: Vec<DohAnswer>,
}

impl DohResponse {
    /// Extract the A/AAAA record addresses, skipping CNAMEs and unparseable data.
    fn ips(self) -> Vec<IpAddr> {
        self.answer
            .into_iter()
            .filter(|a| a.rtype == RTYPE_A || a.rtype == RTYPE_AAAA)
            .filter_map(|a| a.data.parse::<IpAddr>().ok())
            .collect()
    }
}

/// One record in a DoH-JSON answer.
#[derive(Debug, Deserialize)]
struct DohAnswer {
    #[serde(rename = "type")]
    rtype: u16,
    data: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_records_and_skips_cname() {
        // Cloudflare-style answer: a CNAME (type 5) followed by two A records.
        let json = r#"{
            "Status": 0,
            "Answer": [
                {"name": "status.deepseek.com", "type": 5, "data": "cdn.example.net."},
                {"name": "cdn.example.net", "type": 1, "data": "203.0.113.7"},
                {"name": "cdn.example.net", "type": 1, "data": "203.0.113.8"}
            ]
        }"#;
        let resp: DohResponse = serde_json::from_str(json).expect("parse");
        let ips = resp.ips();
        assert_eq!(
            ips,
            vec![
                "203.0.113.7".parse::<IpAddr>().unwrap(),
                "203.0.113.8".parse::<IpAddr>().unwrap(),
            ]
        );
    }

    #[test]
    fn parses_aaaa_records() {
        let json = r#"{"Answer":[{"name":"h","type":28,"data":"2606:4700::1111"}]}"#;
        let resp: DohResponse = serde_json::from_str(json).expect("parse");
        assert_eq!(
            resp.ips(),
            vec!["2606:4700::1111".parse::<IpAddr>().unwrap()]
        );
    }

    #[test]
    fn empty_answer_yields_no_ips() {
        let resp: DohResponse = serde_json::from_str(r#"{"Status":3}"#).expect("parse");
        assert!(resp.ips().is_empty());
    }

    #[test]
    fn skips_unparseable_record_data() {
        let json = r#"{"Answer":[{"type":1,"data":"not-an-ip"},{"type":1,"data":"198.51.100.9"}]}"#;
        let resp: DohResponse = serde_json::from_str(json).expect("parse");
        assert_eq!(resp.ips(), vec!["198.51.100.9".parse::<IpAddr>().unwrap()]);
    }

    #[test]
    fn debug_does_not_leak_agent_internals() {
        let r = DohResolver::new(DEFAULT_DOH_ENDPOINT.to_string());
        let dbg = format!("{r:?}");
        assert!(dbg.contains("DohResolver"));
        assert!(dbg.contains(DEFAULT_DOH_ENDPOINT));
    }
}

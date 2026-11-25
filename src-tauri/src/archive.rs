//! Native archive.org client — replaces the external `ia` (internetarchive) CLI.
//!
//! Everything the app needs is plain HTTPS:
//!   * sign-in     → POST archive.org/services/xauthn/?op=login  (returns S3 keys)
//!   * identifier  → GET  archive.org/services/check_identifier.php
//!   * item lookup → GET  archive.org/metadata/<identifier>
//!   * upload      → PUT  s3.us.archive.org/<identifier>/<remote-name>
//!
//! The upload headers mirror what the `internetarchive` Python library sends, so
//! items come out the same as they did when the app shelled out to `ia`.

use md5::{Digest, Md5};
use serde_json::Value;
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

const USER_AGENT: &str = concat!("ArchiveUI/", env!("CARGO_PKG_VERSION"));

/// Shared HTTP agent. No overall timeout (uploads can run for hours); instead
/// connect/read/write stalls are bounded so a dead connection still fails.
fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::AgentBuilder::new()
            .user_agent(USER_AGENT)
            .timeout_connect(Duration::from_secs(30))
            .timeout_read(Duration::from_secs(600))
            .timeout_write(Duration::from_secs(600))
            .build()
    })
}

/// Short requests (sign-in, lookups) get an overall deadline.
const SHORT: Duration = Duration::from_secs(30);

/// Read a response body whether the request succeeded or returned an HTTP error
/// status (archive.org puts useful JSON/XML in error bodies too).
fn body_of(result: Result<ureq::Response, ureq::Error>) -> Result<(u16, String), String> {
    let resp = match result {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(e) => return Err(format!("Could not reach archive.org: {e}")),
    };
    let status = resp.status();
    let body = resp
        .into_string()
        .map_err(|e| format!("Could not read archive.org's response: {e}"))?;
    Ok((status, body))
}

fn snippet(s: &str) -> String {
    let s = s.trim();
    if s.chars().count() > 200 {
        format!("{}…", s.chars().take(200).collect::<String>())
    } else {
        s.to_string()
    }
}

/// Percent-encode like Python's `urllib.parse.quote` (unreserved chars and `/`
/// pass through) — used for S3 object keys and `uri(...)` metadata values.
pub fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ── Sign-in ────────────────────────────────────────────────────────────────────

/// archive.org S3-style keys used to authorize uploads (`LOW access:secret`).
#[derive(Clone)]
pub struct S3Keys {
    pub access: String,
    pub secret: String,
}

pub struct Login {
    pub keys: S3Keys,
    pub screenname: String,
}

/// Exchange an email + password for the account's S3 keys — the same call
/// `ia configure` makes.
pub fn login(email: &str, password: &str) -> Result<Login, String> {
    let (_, body) = body_of(
        agent()
            .post("https://archive.org/services/xauthn/?op=login")
            .timeout(SHORT)
            .send_form(&[("email", email), ("password", password)]),
    )?;
    let j: Value = serde_json::from_str(&body)
        .map_err(|_| format!("Unexpected response from archive.org: {}", snippet(&body)))?;

    if j["success"].as_bool() != Some(true) {
        let reason = j["values"]["reason"]
            .as_str()
            .or_else(|| j["error"].as_str())
            .unwrap_or("");
        return Err(match reason {
            "account_not_found" => {
                "Account not found — sign in with the email address of your archive.org account."
                    .to_string()
            }
            "account_bad_password" => "Incorrect password — try again.".to_string(),
            "" => "Sign-in failed (archive.org gave no reason).".to_string(),
            other => format!("Sign-in failed: {other}"),
        });
    }

    let v = &j["values"];
    let access = v["s3"]["access"].as_str().unwrap_or("").to_string();
    let secret = v["s3"]["secret"].as_str().unwrap_or("").to_string();
    if access.is_empty() || secret.is_empty() {
        return Err("Signed in, but archive.org returned no upload keys for this account.".into());
    }
    Ok(Login {
        keys: S3Keys { access, secret },
        screenname: v["screenname"].as_str().unwrap_or("").to_string(),
    })
}

// ── Lookups ────────────────────────────────────────────────────────────────────

/// `(available, message)` from archive.org's identifier-availability service —
/// the one the web uploader uses (it correctly reports reserved identifiers).
pub fn check_identifier(id: &str) -> Result<(bool, Option<String>), String> {
    let (_, body) = body_of(
        agent()
            .get("https://archive.org/services/check_identifier.php")
            .query("identifier", id)
            .query("output", "json")
            .timeout(SHORT)
            .call(),
    )?;
    let j: Value = serde_json::from_str(&body)
        .map_err(|_| format!("Unexpected response from archive.org: {}", snippet(&body)))?;
    let available = j["code"].as_str() == Some("available");
    Ok((available, j["message"].as_str().map(str::to_string)))
}

/// An item's full metadata document (`{}` when the item doesn't exist).
pub fn item_metadata(id: &str) -> Result<Value, String> {
    let url = format!("https://archive.org/metadata/{}", quote(id));
    let (status, body) = body_of(agent().get(&url).timeout(SHORT).call())?;
    if status >= 400 {
        return Err(format!("Lookup failed (HTTP {status}): {}", snippet(&body)));
    }
    serde_json::from_str(&body)
        .map_err(|_| format!("Unexpected response from archive.org: {}", snippet(&body)))
}

/// Files already in an item, keyed by name → (size, md5). Used to skip files
/// that are already uploaded (the old `ia upload --checksum` behaviour).
pub fn existing_files(id: &str) -> Result<HashMap<String, (u64, String)>, String> {
    let j = item_metadata(id)?;
    let mut map = HashMap::new();
    if let Some(files) = j["files"].as_array() {
        for f in files {
            let (Some(name), Some(md5)) = (f["name"].as_str(), f["md5"].as_str()) else {
                continue;
            };
            // `size` is a string in the metadata API.
            let size = f["size"]
                .as_str()
                .and_then(|s| s.parse().ok())
                .or_else(|| f["size"].as_u64());
            if let Some(size) = size {
                map.insert(name.to_string(), (size, md5.to_lowercase()));
            }
        }
    }
    Ok(map)
}

/// MD5 of a local file as lowercase hex. Aborts if `cancel` is raised.
pub fn md5_file(path: &Path, cancel: &AtomicBool) -> Result<String, String> {
    let mut f = File::open(path).map_err(|e| format!("Cannot read {}: {e}", path.display()))?;
    let mut hasher = Md5::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Err("cancelled".into());
        }
        let n = f
            .read(&mut buf)
            .map_err(|e| format!("Cannot read {}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

// ── Upload ─────────────────────────────────────────────────────────────────────

/// Whether a metadata value must be sent as `uri(<percent-encoded>)` — the same
/// rule as the internetarchive library: anything non-ASCII or containing
/// whitespace (HTTP headers can't carry it raw).
fn needs_quote(s: &str) -> bool {
    !s.is_ascii() || s.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// Turn item metadata into `x-archive-metaNN-<key>` headers. Repeated keys
/// (e.g. several `subject`s) become an array on archive.org.
pub fn metadata_headers(fields: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    let mut out = Vec::new();
    for (key, value) in fields {
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        let n = counts.entry(key).or_insert(0);
        // S3 header names can't contain `_`; IA-S3 translates `--` back into `_`.
        let name = format!("x-archive-meta{:02}-{}", n, key.replace('_', "--"));
        *n += 1;
        let value = if needs_quote(value) {
            format!("uri({})", quote(value))
        } else {
            value.to_string()
        };
        out.push((name, value));
    }
    out
}

pub enum PutError {
    Cancelled,
    /// Worth retrying (server busy / network hiccup).
    Retry(String),
    /// Won't succeed on retry (bad keys, forbidden, bad request…).
    Fatal(String),
}

/// Pull `<Code>`/`<Message>` out of an S3 XML error body.
fn s3_error(status: u16, body: &str) -> String {
    let tag = |t: &str| {
        let open = format!("<{t}>");
        let close = format!("</{t}>");
        let start = body.find(&open)? + open.len();
        let end = body[start..].find(&close)? + start;
        Some(body[start..end].trim().to_string())
    };
    match (tag("Code"), tag("Message")) {
        (Some(c), Some(m)) => format!("{c}: {m} (HTTP {status})"),
        (None, Some(m)) | (Some(m), None) => format!("{m} (HTTP {status})"),
        _ if body.trim().is_empty() => format!("HTTP {status}"),
        _ => format!("HTTP {status}: {}", snippet(body)),
    }
}

/// Wraps the file being uploaded: reports bytes sent and aborts the transfer as
/// soon as the user cancels.
struct ProgressReader<'a, R: Read> {
    inner: R,
    sent: u64,
    cancel: &'a AtomicBool,
    on_progress: &'a mut dyn FnMut(u64),
}

impl<R: Read> Read for ProgressReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.cancel.load(Ordering::SeqCst) {
            return Err(io::Error::other("cancelled"));
        }
        let n = self.inner.read(buf)?;
        self.sent += n as u64;
        (self.on_progress)(self.sent);
        Ok(n)
    }
}

/// PUT one local file into an item via archive.org's S3-compatible API.
pub fn put_file(
    keys: &S3Keys,
    identifier: &str,
    path: &Path,
    remote: &str,
    size: u64,
    headers: &[(String, String)],
    cancel: &AtomicBool,
    on_progress: &mut dyn FnMut(u64),
) -> Result<(), PutError> {
    let url = format!(
        "https://s3.us.archive.org/{}/{}",
        quote(identifier),
        quote(remote.trim_start_matches('/'))
    );
    let file = File::open(path)
        .map_err(|e| PutError::Fatal(format!("Cannot read {}: {e}", path.display())))?;

    let mut req = agent()
        .put(&url)
        .set("Authorization", &format!("LOW {}:{}", keys.access, keys.secret))
        .set("Content-Length", &size.to_string());
    for (k, v) in headers {
        req = req.set(k, v);
    }

    let reader = ProgressReader {
        inner: BufReader::with_capacity(1 << 20, file),
        sent: 0,
        cancel,
        on_progress,
    };

    match req.send(reader) {
        Ok(_) => Ok(()),
        Err(_) if cancel.load(Ordering::SeqCst) => Err(PutError::Cancelled),
        Err(ureq::Error::Status(status, resp)) => {
            let msg = s3_error(status, &resp.into_string().unwrap_or_default());
            // 503 SlowDown / 5xx are transient; 4xx means the request itself is bad.
            if status >= 500 || status == 429 {
                Err(PutError::Retry(msg))
            } else {
                Err(PutError::Fatal(msg))
            }
        }
        Err(e) => Err(PutError::Retry(format!("network error: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_matches_python() {
        assert_eq!(quote("a b/c.iso"), "a%20b/c.iso");
        assert_eq!(quote("é"), "%C3%A9");
    }

    #[test]
    fn metadata_headers_shape() {
        let h = metadata_headers(&[
            ("title", "Hello World"),
            ("mediatype", "software"),
            ("subject", "one"),
            ("subject", "two"),
            ("creator", "  "),
            ("license_url", "x"),
        ]);
        assert_eq!(
            h,
            vec![
                ("x-archive-meta00-title".into(), "uri(Hello%20World)".into()),
                ("x-archive-meta00-mediatype".into(), "software".into()),
                ("x-archive-meta00-subject".into(), "one".into()),
                ("x-archive-meta01-subject".into(), "two".into()),
                ("x-archive-meta00-license--url".into(), "x".into()),
            ]
        );
    }

    #[test]
    fn s3_error_parsing() {
        let b = "<?xml?><Error><Code>AccessDenied</Code><Message>nope</Message></Error>";
        assert_eq!(s3_error(403, b), "AccessDenied: nope (HTTP 403)");
    }
}

/// Live checks against archive.org using throwaway inputs (no real account).
/// Run with: cargo test -- --ignored --nocapture
#[cfg(test)]
mod live {
    use super::*;

    #[test]
    #[ignore]
    fn live_endpoints() {
        let bad = login("no-such-user-archiveui-test@example.invalid", "x").err().unwrap();
        println!("login(bad)        → {bad}");
        println!("check('test')     → {:?}", check_identifier("test").unwrap());
        let free = format!("archiveui-free-{}", std::process::id());
        println!("check('{free}') → {:?}", check_identifier(&free).unwrap());
        let files = existing_files("nasa").unwrap();
        println!("existing('nasa')  → {} files", files.len());
        println!("metadata(missing) → {}", item_metadata(&free).unwrap());

        let keys = S3Keys { access: "bogus".into(), secret: "bogus".into() };
        let tmp = std::env::temp_dir().join("archiveui-live.txt");
        std::fs::write(&tmp, b"hello").unwrap();
        let never = AtomicBool::new(false);
        let mut seen = 0u64;
        let r = put_file(&keys, &free, &tmp, "a b.txt", 5, &[], &never, &mut |s| seen = s);
        match r {
            Err(PutError::Fatal(m)) => println!("put(bad keys)     → Fatal: {m} (sent {seen} B)"),
            Err(PutError::Retry(m)) => println!("put(bad keys)     → Retry: {m}"),
            Err(PutError::Cancelled) => println!("put → cancelled?"),
            Ok(()) => panic!("bogus keys accepted?!"),
        }
        let stop = AtomicBool::new(true);
        assert!(matches!(
            put_file(&keys, &free, &tmp, "x", 5, &[], &stop, &mut |_| {}),
            Err(PutError::Cancelled)
        ));
        println!("put(cancelled)    → Cancelled ✓");
    }
}

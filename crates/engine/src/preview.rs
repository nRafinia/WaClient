//! Link previews.
//!
//! Incoming previews are read from the message itself and never trigger a
//! request (privacy: the sender already did the fetching). Outgoing previews
//! are generated on the sender side, like the official clients: the page is
//! fetched, Open Graph metadata is scanned out of it and a small JPEG
//! thumbnail is made from `og:image`.
//!
//! Limits of the network guard (see [`fetch_preview`]):
//! - Hosts are checked by literal IP and by resolving DNS before each request
//!   (with a proxy configured DNS is left to the proxy, so only literal IPs
//!   and local-looking names are checked),
//!   but the HTTP client resolves again when it connects, so a DNS-rebinding
//!   server could still answer differently the second time.
//! - The library client follows redirects on its own and does not expose the
//!   hops, so a public URL that redirects to a LAN address is not caught.
//! - The client buffers whole bodies; the byte limits below are applied to
//!   what is parsed or decoded, not to what is downloaded.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use whatsapp_rust::buffa::MessageField;
use whatsapp_rust::http::{HttpClient, HttpRequest};
use whatsapp_rust::prelude::wa;

/// Only this much of a page is scanned for metadata.
const MAX_HTML_BYTES: usize = 1024 * 1024;
/// Larger preview images are not used.
const MAX_IMAGE_BYTES: usize = 2 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const DNS_TIMEOUT: Duration = Duration::from_secs(3);
const THUMB_MAX_SIDE: u32 = 140;
const THUMB_QUALITY: u8 = 70;
/// Decoder bounds so a small file cannot expand into a huge bitmap.
const MAX_IMAGE_SIDE: u32 = 4096;
const MAX_IMAGE_ALLOC: u64 = 64 * 1024 * 1024;
const MAX_TITLE_CHARS: usize = 200;
const MAX_DESCRIPTION_CHARS: usize = 400;
const MAX_URL_BYTES: usize = 2048;
/// Bounds for the attacker-controlled fields of an incoming preview.
const MAX_INCOMING_TITLE_CHARS: usize = 300;
const MAX_INCOMING_DESCRIPTION_CHARS: usize = 1000;
const MAX_INCOMING_THUMB_BYTES: usize = 64 * 1024;
const MAX_INCOMING_THUMB_SIDE: u32 = 512;
const MAX_INCOMING_THUMB_ALLOC: u64 = 4 * 1024 * 1024;

/// What a link preview card shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkPreview {
    pub url: String,
    /// Empty when unknown.
    pub title: String,
    /// Empty when unknown.
    pub description: String,
    pub thumbnail_jpeg: Option<Vec<u8>>,
}

// ---------------------------------------------------------------------------
// Incoming
// ---------------------------------------------------------------------------

/// Deepest message inside any nesting of the known wrappers.
fn unwrap_message(msg: &wa::Message) -> &wa::Message {
    const MAX_DEPTH: usize = 10;
    let mut current = msg;
    for _ in 0..MAX_DEPTH {
        let inner = [
            current.device_sent_message.as_option().map(|w| &w.message),
            current.ephemeral_message.as_option().map(|w| &w.message),
            current.view_once_message.as_option().map(|w| &w.message),
            current.view_once_message_v2.as_option().map(|w| &w.message),
            current.view_once_message_v2_extension.as_option().map(|w| &w.message),
            current.document_with_caption_message.as_option().map(|w| &w.message),
            current.edited_message.as_option().map(|w| &w.message),
        ]
        .into_iter()
        .flatten()
        .find_map(|m| m.as_option());
        match inner {
            Some(next) => current = next,
            None => break,
        }
    }
    current
}

/// The preview attached to a received text message, if any. Never touches the
/// network. `ExtendedTextMessage` carries the link in `matchedText`.
pub fn preview_from_message(msg: &wa::Message) -> Option<LinkPreview> {
    let ext = unwrap_message(msg).extended_text_message.as_option()?;
    let url = ext.matched_text.as_deref()?.trim();
    if url.len() > MAX_URL_BYTES || parse_http_url(url).is_err() {
        return None;
    }
    // The card must belong to a link the reader can see in the text; otherwise
    // it could present one address while the text shows another (phishing).
    if !ext.text.as_deref().is_some_and(|t| t.contains(url)) {
        return None;
    }
    let title = bounded_text(ext.title.as_deref().unwrap_or_default(), MAX_INCOMING_TITLE_CHARS);
    let description =
        bounded_text(ext.description.as_deref().unwrap_or_default(), MAX_INCOMING_DESCRIPTION_CHARS);
    let thumbnail_jpeg = ext.jpeg_thumbnail.as_ref().filter(|t| incoming_thumbnail_is_sane(t)).cloned();
    if title.is_empty() && description.is_empty() && thumbnail_jpeg.is_none() {
        return None;
    }
    Some(LinkPreview { url: url.to_string(), title, description, thumbnail_jpeg })
}

/// Drops control characters and keeps at most `max_chars` characters.
fn bounded_text(s: &str, max_chars: usize) -> String {
    s.chars().filter(|c| !c.is_control()).take(max_chars).collect()
}

/// Whether `data` is a small, decodable image. Decoding happens with tight
/// limits so a tiny hostile file cannot expand into a huge bitmap.
fn incoming_thumbnail_is_sane(data: &[u8]) -> bool {
    if data.is_empty() || data.len() > MAX_INCOMING_THUMB_BYTES {
        return false;
    }
    let Ok(mut reader) = image::ImageReader::new(std::io::Cursor::new(data)).with_guessed_format() else {
        return false;
    };
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_INCOMING_THUMB_SIDE);
    limits.max_image_height = Some(MAX_INCOMING_THUMB_SIDE);
    limits.max_alloc = Some(MAX_INCOMING_THUMB_ALLOC);
    reader.limits(limits);
    reader.decode().is_ok()
}

/// The first `http(s)://` link in `text`, as a verbatim slice of it (so the
/// recipient can match the card to the link). A link must start a word;
/// trailing sentence punctuation and unbalanced closing brackets are cut off.
pub fn first_url(text: &str) -> Option<&str> {
    let lower = text.to_ascii_lowercase();
    let mut from = 0;
    while let Some(rel) = lower[from..].find("http") {
        let start = from + rel;
        from = start + 4;
        let after = &lower[start..];
        if !(after.starts_with("http://") || after.starts_with("https://")) {
            continue;
        }
        let at_word_start = text[..start]
            .chars()
            .next_back()
            .is_none_or(|c| c.is_whitespace() || matches!(c, '(' | '[' | '<' | '"' | '\''));
        if !at_word_start {
            continue;
        }
        let end = text[start..].find(char::is_whitespace).map_or(text.len(), |i| start + i);
        let mut url = &text[start..end];
        loop {
            let Some(last) = url.chars().next_back() else { break };
            let unbalanced_close = |open: char| {
                url.matches(last).count() > url.matches(open).count()
            };
            let cut = match last {
                '.' | ',' | ';' | ':' | '!' | '?' | '"' | '\'' | '>' => true,
                ')' => unbalanced_close('('),
                ']' => unbalanced_close('['),
                _ => false,
            };
            if !cut {
                break;
            }
            url = &url[..url.len() - last.len_utf8()];
        }
        if url.len() > "https://".len() && url.len() <= MAX_URL_BYTES {
            return Some(url);
        }
    }
    None
}

/// Fills the preview fields of an outgoing text message. `ext.text` must
/// contain `p.url` verbatim for recipients to attach the card to the link.
pub fn apply_preview(ext: &mut wa::message::ExtendedTextMessage, p: &LinkPreview) {
    use wa::message::extended_text_message::PreviewType;
    ext.matched_text = Some(p.url.clone());
    ext.title = (!p.title.is_empty()).then(|| p.title.clone());
    ext.description = (!p.description.is_empty()).then(|| p.description.clone());
    ext.jpeg_thumbnail = p.thumbnail_jpeg.clone();
    ext.preview_type = Some(PreviewType::NONE);
}

/// Convenience for building an outgoing message with a preview.
pub fn text_message_with_preview(text: &str, p: &LinkPreview) -> wa::Message {
    let mut ext = wa::message::ExtendedTextMessage { text: Some(text.to_string()), ..Default::default() };
    apply_preview(&mut ext, p);
    wa::Message { extended_text_message: MessageField::some(ext), ..Default::default() }
}

// ---------------------------------------------------------------------------
// URL handling and the network guard
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
struct ParsedUrl {
    https: bool,
    host: String,
    port: u16,
    /// Path and query, starting with `/`; fragment removed.
    path: String,
}

impl ParsedUrl {
    fn origin(&self) -> String {
        let scheme = if self.https { "https" } else { "http" };
        let default = if self.https { 443 } else { 80 };
        let host = if self.host.contains(':') { format!("[{}]", self.host) } else { self.host.clone() };
        if self.port == default {
            format!("{scheme}://{host}")
        } else {
            format!("{scheme}://{host}:{}", self.port)
        }
    }
}

/// Accepts only `http`/`https` URLs without credentials.
fn parse_http_url(url: &str) -> Result<ParsedUrl> {
    let url = url.trim();
    if url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        bail!("URL contains whitespace or control characters");
    }
    let (https, rest) = match url.split_once("://") {
        Some((s, rest)) if s.eq_ignore_ascii_case("https") => (true, rest),
        Some((s, rest)) if s.eq_ignore_ascii_case("http") => (false, rest),
        _ => bail!("only http and https links get previews"),
    };
    let rest = rest.split('#').next().unwrap_or_default();
    let auth_end = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(auth_end);
    if authority.contains('@') {
        bail!("URLs with credentials are not previewed");
    }
    let default_port = if https { 443 } else { 80 };
    let (host, port) = if let Some(inner) = authority.strip_prefix('[') {
        let (host, after) = inner.split_once(']').context("unterminated IPv6 literal")?;
        if host.parse::<Ipv6Addr>().is_err() {
            bail!("invalid IPv6 literal");
        }
        let port = match after.strip_prefix(':') {
            Some(p) => p.parse().context("invalid port")?,
            None if after.is_empty() => default_port,
            None => bail!("invalid authority"),
        };
        (host.to_string(), port)
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse().context("invalid port")?),
            None => (authority.to_string(), default_port),
        }
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() || host.contains(':') && host.parse::<Ipv6Addr>().is_err() {
        bail!("missing or invalid host");
    }
    let path = if tail.starts_with('/') { tail.to_string() } else { format!("/{tail}") };
    Ok(ParsedUrl { https, host, port, path })
}

fn ipv4_is_public(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    !(o[0] == 0
        || o[0] == 10
        || o[0] == 127
        || (o[0] == 100 && (64..=127).contains(&o[1]))
        || (o[0] == 169 && o[1] == 254)
        || (o[0] == 172 && (16..=31).contains(&o[1]))
        || (o[0] == 192 && o[1] == 168)
        || (o[0] == 192 && o[1] == 0 && o[2] == 0)
        || (o[0] == 192 && o[1] == 0 && o[2] == 2)
        || (o[0] == 198 && (o[1] == 18 || o[1] == 19))
        || (o[0] == 198 && o[1] == 51 && o[2] == 100)
        || (o[0] == 203 && o[1] == 0 && o[2] == 113)
        || o[0] >= 224)
}

fn ip_is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => ipv4_is_public(v4),
        IpAddr::V6(v6) => {
            let s = v6.segments();
            if v6.is_unspecified() || v6.is_loopback() || v6.is_multicast() {
                return false;
            }
            if s[0] & 0xffc0 == 0xfe80 || s[0] & 0xfe00 == 0xfc00 {
                return false; // link-local, unique local
            }
            if s[0] == 0x2001 && s[1] == 0x0db8 {
                return false; // documentation
            }
            if s[0] & 0xffc0 == 0xfec0 {
                return false; // deprecated site-local
            }
            if s[0] == 0x2001 && s[1] == 0 {
                return false; // Teredo
            }
            let embedded = |a: u16, b: u16| Ipv4Addr::new((a >> 8) as u8, a as u8, (b >> 8) as u8, b as u8);
            if let Some(v4) = v6.to_ipv4_mapped() {
                return ipv4_is_public(v4);
            }
            if s[..6].iter().all(|&x| x == 0) {
                return ipv4_is_public(embedded(s[6], s[7])); // IPv4-compatible
            }
            if s[0] == 0x0064 && s[1] == 0xff9b && s[2..6].iter().all(|&x| x == 0) {
                return ipv4_is_public(embedded(s[6], s[7])); // NAT64
            }
            if s[0] == 0x2002 {
                return ipv4_is_public(embedded(s[1], s[2])); // 6to4
            }
            true
        }
    }
}

/// Rejects names that are local by convention, before any DNS lookup.
fn host_name_is_local(host: &str) -> bool {
    host == "localhost"
        || host.ends_with(".localhost")
        || host.ends_with(".local")
        || host.ends_with(".internal")
        || host.ends_with(".localdomain")
}

/// A host made only of decimal / `0x` hex parts separated by dots (`2130706433`,
/// `0x7f.1`, `127.1`, `0177.0.0.1`): `inet_aton` style shorthand that a proxy or
/// resolver may read as an IPv4 address even though `IpAddr` does not.
fn host_is_numeric_shorthand(host: &str) -> bool {
    let host = host.strip_suffix('.').unwrap_or(host);
    !host.is_empty()
        && host.split('.').all(|part| match part.get(..2).filter(|p| p.eq_ignore_ascii_case("0x")) {
            Some(_) => part[2..].bytes().all(|b| b.is_ascii_hexdigit()),
            None => !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()),
        })
}

/// Parses and validates `url`. Fails for non-http(s) schemes and for hosts that
/// are or resolve to non-public addresses. With `proxied` the host name is not
/// resolved here (that would leak it past the proxy); only literal IPs and
/// local-looking names are refused and the proxy does the resolving.
async fn check_url(url: &str, proxied: bool) -> Result<ParsedUrl> {
    let parsed = parse_http_url(url)?;
    if let Ok(ip) = parsed.host.parse::<IpAddr>() {
        if !ip_is_public(ip) {
            bail!("refusing to fetch a non-public address");
        }
        return Ok(parsed);
    }
    if host_name_is_local(&parsed.host) {
        bail!("refusing to fetch a local host name");
    }
    if proxied {
        if host_is_numeric_shorthand(&parsed.host) {
            bail!("refusing to fetch a numeric host");
        }
        return Ok(parsed);
    }
    let target = (parsed.host.clone(), parsed.port);
    let lookup = tokio::task::spawn_blocking(move || {
        target.to_socket_addrs().map(|it| it.collect::<Vec<_>>())
    });
    let addrs = tokio::time::timeout(DNS_TIMEOUT, lookup)
        .await
        .context("DNS lookup timed out")?
        .context("DNS lookup task failed")?
        .context("DNS lookup failed")?;
    if addrs.is_empty() {
        bail!("host does not resolve");
    }
    if addrs.iter().any(|a| !ip_is_public(a.ip())) {
        bail!("host resolves to a non-public address");
    }
    Ok(parsed)
}

/// Resolves `reference` (an `og:image` value) against the page URL.
fn resolve_reference(base: &ParsedUrl, reference: &str) -> Option<String> {
    let r = reference.trim();
    if r.is_empty() {
        return None;
    }
    let scheme = if base.https { "https" } else { "http" };
    if let Some(rest) = r.strip_prefix("//") {
        return Some(format!("{scheme}://{rest}"));
    }
    if r.contains("://") {
        return Some(r.to_string()); // validated (or refused) by the caller
    }
    if r.starts_with('/') {
        return Some(format!("{}{r}", base.origin()));
    }
    let path = base.path.split('?').next().unwrap_or("/");
    let dir = &path[..path.rfind('/').map_or(0, |i| i + 1)];
    Some(format!("{}{dir}{r}", base.origin()))
}

// ---------------------------------------------------------------------------
// Fetching
// ---------------------------------------------------------------------------

async fn get(http: &dyn HttpClient, url: &str) -> Result<Vec<u8>> {
    let mut request = HttpRequest::get(url);
    request.headers.insert("User-Agent".into(), "Mozilla/5.0 (compatible; link preview)".into());
    request.headers.insert("Accept-Language".into(), "en".into());
    let response = tokio::time::timeout(REQUEST_TIMEOUT, http.execute(request))
        .await
        .context("request timed out")??;
    if !(200..300).contains(&response.status_code) {
        bail!("server answered HTTP {}", response.status_code);
    }
    Ok(response.body)
}

/// Builds the preview of `url` by fetching the page. Only public http(s)
/// hosts are contacted (see the module docs for the limits of that guard).
/// Callers that must not wait should wrap the call in their own timeout.
pub async fn fetch_preview(http: &dyn HttpClient, url: &str, proxied: bool) -> Result<LinkPreview> {
    let page_url = parse_http_url(url)?;
    check_url(url, proxied).await?;
    let body = get(http, url).await?;
    let meta = parse_html(&body);

    let title = meta.title.unwrap_or_default();
    let description = meta.description.unwrap_or_default();
    let thumbnail_jpeg = match meta.image.and_then(|i| resolve_reference(&page_url, &i)) {
        Some(image_url) => fetch_thumbnail(http, &image_url, proxied).await.map_err(|e| {
            log::debug!("link preview image skipped: {e:#}");
        }).ok(),
        None => None,
    };
    if title.is_empty() && description.is_empty() && thumbnail_jpeg.is_none() {
        bail!("page has no preview metadata");
    }
    Ok(LinkPreview { url: url.trim().to_string(), title, description, thumbnail_jpeg })
}

async fn fetch_thumbnail(http: &dyn HttpClient, image_url: &str, proxied: bool) -> Result<Vec<u8>> {
    check_url(image_url, proxied).await?;
    let data = get(http, image_url).await?;
    if data.len() > MAX_IMAGE_BYTES {
        bail!("image is larger than {MAX_IMAGE_BYTES} bytes");
    }
    tokio::task::spawn_blocking(move || make_thumbnail(&data))
        .await
        .context("thumbnail task failed")?
}

/// Decodes `data` and returns a JPEG whose longest side is at most 140 px.
fn make_thumbnail(data: &[u8]) -> Result<Vec<u8>> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(data)).with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_SIDE);
    limits.max_image_height = Some(MAX_IMAGE_SIDE);
    limits.max_alloc = Some(MAX_IMAGE_ALLOC);
    reader.limits(limits);
    let img = reader.decode().context("image does not decode")?;
    let thumb = img.thumbnail(THUMB_MAX_SIDE, THUMB_MAX_SIDE).to_rgb8();
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, THUMB_QUALITY)
        .encode_image(&thumb)
        .context("thumbnail encoding failed")?;
    Ok(out)
}

// ---------------------------------------------------------------------------
// HTML metadata scanner
// ---------------------------------------------------------------------------

#[derive(Debug, Default, PartialEq, Eq)]
struct PageMeta {
    title: Option<String>,
    description: Option<String>,
    image: Option<String>,
}

#[derive(Default)]
struct Found {
    og_title: Option<String>,
    og_description: Option<String>,
    og_image: Option<String>,
    twitter_image: Option<String>,
    title_tag: Option<String>,
    meta_description: Option<String>,
}

/// Scans at most the first [`MAX_HTML_BYTES`] of `body` for metadata. Tolerant
/// of broken markup: it never fails, it just finds less.
fn parse_html(body: &[u8]) -> PageMeta {
    let body = &body[..body.len().min(MAX_HTML_BYTES)];
    let html = String::from_utf8_lossy(body);
    let mut found = Found::default();
    scan_tags(&html, &mut found);
    let first = |a: Option<String>, b: Option<String>| a.or(b);
    PageMeta {
        title: first(found.og_title, found.title_tag).and_then(|t| clean(&t, MAX_TITLE_CHARS)),
        description: first(found.og_description, found.meta_description)
            .and_then(|d| clean(&d, MAX_DESCRIPTION_CHARS)),
        image: first(found.og_image, found.twitter_image).map(|i| i.trim().to_string()).filter(|i| !i.is_empty()),
    }
}

fn find_ci(hay: &str, from: usize, needle: &str) -> Option<usize> {
    let (h, n) = (hay.as_bytes(), needle.as_bytes());
    if n.is_empty() || h.len() < n.len() || from > h.len() - n.len() {
        return None;
    }
    (from..=h.len() - n.len()).find(|&i| h[i..i + n.len()].eq_ignore_ascii_case(n))
}

fn scan_tags(html: &str, found: &mut Found) {
    let bytes = html.as_bytes();
    let mut i = 0;
    while let Some(lt) = html[i..].find('<').map(|o| i + o) {
        i = lt + 1;
        let rest = &html[i..];
        if rest.starts_with("!--") {
            i = html[i..].find("-->").map_or(html.len(), |o| i + o + 3);
            continue;
        }
        if rest.starts_with(['/', '!', '?']) {
            i = html[i..].find('>').map_or(html.len(), |o| i + o + 1);
            continue;
        }
        let name_len = rest.bytes().take_while(|b| b.is_ascii_alphanumeric() || *b == b'-').count();
        if name_len == 0 {
            continue;
        }
        let name = rest[..name_len].to_ascii_lowercase();
        let (attrs, end) = parse_attrs(html, i + name_len);
        i = end;
        match name.as_str() {
            "meta" => record_meta(&attrs, found),
            "title" if found.title_tag.is_none() => {
                let close = find_ci(html, i, "</title").unwrap_or(html.len());
                found.title_tag = Some(decode_entities(&html[i..close]));
                i = close;
            }
            "script" | "style" => {
                let close = format!("</{name}");
                i = find_ci(html, i, &close).unwrap_or(bytes.len());
            }
            _ => {}
        }
    }
}

/// Reads attributes starting at `pos` (just after the tag name). Returns them
/// with lowercase names and decoded values, plus the position after the `>`.
fn parse_attrs(html: &str, mut pos: usize) -> (Vec<(String, String)>, usize) {
    let b = html.as_bytes();
    let mut attrs = Vec::new();
    loop {
        while pos < b.len() && (b[pos].is_ascii_whitespace() || b[pos] == b'/') {
            pos += 1;
        }
        if pos >= b.len() {
            return (attrs, b.len());
        }
        if b[pos] == b'>' {
            return (attrs, pos + 1);
        }
        let start = pos;
        while pos < b.len() && !(b[pos].is_ascii_whitespace() || matches!(b[pos], b'=' | b'>' | b'/')) {
            pos += 1;
        }
        if pos == start {
            pos += 1; // stray '=' or similar
            continue;
        }
        let name = html[start..pos].to_ascii_lowercase();
        while pos < b.len() && b[pos].is_ascii_whitespace() {
            pos += 1;
        }
        let mut value = String::new();
        if pos < b.len() && b[pos] == b'=' {
            pos += 1;
            while pos < b.len() && b[pos].is_ascii_whitespace() {
                pos += 1;
            }
            if pos < b.len() && (b[pos] == b'"' || b[pos] == b'\'') {
                let quote = b[pos];
                let vstart = pos + 1;
                let vend = b[vstart..].iter().position(|&c| c == quote).map_or(b.len(), |o| vstart + o);
                value = decode_entities(&html[vstart..vend]);
                pos = (vend + 1).min(b.len());
            } else {
                let vstart = pos;
                while pos < b.len() && !(b[pos].is_ascii_whitespace() || b[pos] == b'>') {
                    pos += 1;
                }
                value = decode_entities(&html[vstart..pos]);
            }
        }
        attrs.push((name, value));
    }
}

fn record_meta(attrs: &[(String, String)], found: &mut Found) {
    let attr = |n: &str| attrs.iter().find(|(k, _)| k == n).map(|(_, v)| v.as_str());
    let Some(content) = attr("content") else { return };
    let Some(key) = attr("property").or_else(|| attr("name")) else { return };
    let slot = match key.trim().to_ascii_lowercase().as_str() {
        "og:title" => &mut found.og_title,
        "og:description" => &mut found.og_description,
        "og:image" | "og:image:url" | "og:image:secure_url" => &mut found.og_image,
        "twitter:image" | "twitter:image:src" => &mut found.twitter_image,
        "description" => &mut found.meta_description,
        _ => return,
    };
    if slot.is_none() && !content.trim().is_empty() {
        *slot = Some(content.to_string());
    }
}

/// Collapses whitespace, drops empty results and caps the length.
fn clean(s: &str, max_chars: usize) -> Option<String> {
    let joined = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let capped: String = joined.chars().take(max_chars).collect();
    (!capped.is_empty()).then_some(capped)
}

fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        let decoded = after
            .find(';')
            .filter(|&semi| semi <= 10)
            .and_then(|semi| entity(&after[..semi]).map(|c| (c, semi)));
        match decoded {
            Some((c, semi)) => {
                out.push(c);
                rest = &after[semi + 1..];
            }
            None => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

fn entity(name: &str) -> Option<char> {
    match name {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some(' '),
        "ndash" => Some('\u{2013}'),
        "mdash" => Some('\u{2014}'),
        "hellip" => Some('\u{2026}'),
        _ => {
            let num = name.strip_prefix('#')?;
            let code = match num.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => num.parse().ok()?,
            };
            char::from_u32(code).filter(|c| *c != '\0')
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(html: &str) -> PageMeta {
        parse_html(html.as_bytes())
    }

    #[test]
    fn reads_open_graph_tags() {
        let m = meta(
            r#"<html><head><title>Plain</title>
            <meta property="og:title" content="OG Title">
            <meta property="og:description" content="OG Desc">
            <meta property="og:image" content="https://x.test/i.png"></head></html>"#,
        );
        assert_eq!(m.title.as_deref(), Some("OG Title"));
        assert_eq!(m.description.as_deref(), Some("OG Desc"));
        assert_eq!(m.image.as_deref(), Some("https://x.test/i.png"));
    }

    #[test]
    fn falls_back_to_title_and_meta_description() {
        let m = meta("<title>\n  Hello\n  world </title><meta name=description content='About it'>");
        assert_eq!(m.title.as_deref(), Some("Hello world"));
        assert_eq!(m.description.as_deref(), Some("About it"));
        assert_eq!(m.image, None);
    }

    #[test]
    fn tolerates_casing_quotes_and_attribute_order() {
        let m = meta(
            "<META CONTENT=\"Upper\" PROPERTY='OG:Title'/>\
             <meta content=unquoted name=twitter:image>\
             <Meta  property = \"og:description\"  content = 'spaced' >",
        );
        assert_eq!(m.title.as_deref(), Some("Upper"));
        assert_eq!(m.description.as_deref(), Some("spaced"));
        assert_eq!(m.image.as_deref(), Some("unquoted"));
    }

    #[test]
    fn decodes_entities() {
        let m = meta(
            r#"<meta property="og:title" content="Fish &amp; Chips &#8211; &#x41;&lt;b&gt; &bogus; &amp">"#,
        );
        assert_eq!(m.title.as_deref(), Some("Fish & Chips \u{2013} A<b> &bogus; &amp"));
    }

    #[test]
    fn ignores_comments_scripts_and_closing_tags() {
        let m = meta(
            "<!-- <meta property=\"og:title\" content=\"hidden\"> -->\
             <script>var s = '<meta property=\"og:title\" content=\"js\">';</script>\
             <style>a{}</style></p><?xml?><!DOCTYPE html>\
             <meta property=\"og:title\" content=\"real\">",
        );
        assert_eq!(m.title.as_deref(), Some("real"));
    }

    #[test]
    fn first_tag_wins_and_empty_content_is_skipped() {
        let m = meta(
            "<meta property=\"og:title\" content=\"  \"><meta property=\"og:title\" content=\"one\">\
             <meta property=\"og:title\" content=\"two\">",
        );
        assert_eq!(m.title.as_deref(), Some("one"));
    }

    #[test]
    fn survives_truncated_and_odd_markup() {
        assert_eq!(meta("<meta property=\"og:title\" content=\"cut off"), PageMeta {
            title: Some("cut off".into()),
            ..PageMeta::default()
        });
        assert_eq!(meta("<title>no end"), PageMeta { title: Some("no end".into()), ..PageMeta::default() });
        assert_eq!(meta("<<<>>> < <a =b <meta"), PageMeta::default());
        assert_eq!(meta("<p>\u{e9}\u{4e2d}<meta property=og:title content=\u{e9}\u{4e2d}>").title.as_deref(),
            Some("\u{e9}\u{4e2d}"));
    }

    #[test]
    fn huge_input_is_cut_at_the_limit() {
        let mut page = String::from("<meta property=\"og:title\" content=\"early\">");
        page.push_str(&" ".repeat(MAX_HTML_BYTES));
        page.push_str("<meta property=\"og:description\" content=\"late\">");
        let m = meta(&page);
        assert_eq!(m.title.as_deref(), Some("early"));
        assert_eq!(m.description, None);
    }

    #[test]
    fn long_text_is_capped() {
        let long = "a".repeat(5000);
        let m = meta(&format!("<title>{long}</title><meta name=description content='{long}'>"));
        assert_eq!(m.title.map(|t| t.chars().count()), Some(MAX_TITLE_CHARS));
        assert_eq!(m.description.map(|t| t.chars().count()), Some(MAX_DESCRIPTION_CHARS));
    }

    #[test]
    fn classifies_non_public_addresses() {
        for ip in [
            "127.0.0.1", "127.8.8.8", "10.0.0.1", "172.16.5.5", "172.31.255.255", "192.168.1.1",
            "169.254.169.254", "0.0.0.0", "100.64.0.1", "224.0.0.1", "255.255.255.255", "::1", "::",
            "fe80::1", "fc00::1", "fd12::1", "::ffff:127.0.0.1", "::ffff:10.1.2.3", "::10.0.0.1",
            "64:ff9b::7f00:1", "2002:c0a8:101::1", "2001:db8::1", "ff02::1", "fec0::1", "feff::1",
            "2001:0:4136:e378:8000:63bf:3fff:fdd2",
        ] {
            assert!(!ip_is_public(ip.parse().unwrap()), "{ip} must be refused");
        }
        for ip in ["8.8.8.8", "172.32.0.1", "100.63.0.1", "1.1.1.1", "2606:4700::1111", "::ffff:8.8.8.8"] {
            assert!(ip_is_public(ip.parse().unwrap()), "{ip} must be allowed");
        }
    }

    async fn rejected(url: &str) -> bool {
        check_url(url, false).await.is_err()
    }

    #[tokio::test]
    async fn rejects_private_and_local_hosts_without_dns() {
        for url in [
            "http://127.0.0.1/", "http://10.1.2.3:8080/x", "https://192.168.0.10/", "http://[::1]/",
            "http://[fe80::1]:80/", "http://localhost/", "http://LOCALHOST:3000/", "http://app.localhost/",
            "http://printer.local/", "http://169.254.169.254/latest/meta-data", "http://127.0.0.1./",
            "http://[::ffff:127.0.0.1]/",
        ] {
            assert!(rejected(url).await, "{url} must be refused");
        }
    }

    #[tokio::test]
    async fn proxied_rejects_private_literals_and_local_names() {
        for url in [
            "http://127.0.0.1/", "http://10.1.2.3:8080/x", "https://192.168.0.10/", "http://[::1]/",
            "http://[fe80::1]:80/", "http://localhost/", "http://app.localhost/", "http://printer.local/",
            "http://169.254.169.254/latest/meta-data", "http://[::ffff:127.0.0.1]/", "http://nas.internal/",
        ] {
            assert!(check_url(url, true).await.is_err(), "{url} must be refused");
        }
    }

    #[tokio::test]
    async fn proxied_rejects_numeric_ipv4_shorthand() {
        for url in [
            "http://2130706433/", "http://0x7f.1/", "http://127.1/", "http://0177.0.0.1/", "http://0x7f000001/",
            "http://0X7F.0.0.1:8080/x", "http://127.1./", "http://10.1/", "http://169.254.43518/",
        ] {
            assert!(check_url(url, true).await.is_err(), "{url} must be refused");
        }
        assert!(check_url("https://0x.example.com/", true).await.is_ok());
        assert!(check_url("https://123.example.com/", true).await.is_ok());
        assert!(check_url("https://a1.b2/", true).await.is_ok());
    }

    #[tokio::test]
    async fn proxied_does_not_resolve_names_locally() {
        // `.invalid` never resolves: without a proxy this fails the DNS check,
        // with one the name is handed to the proxy untouched.
        let parsed = check_url("https://no-such-host.invalid/page", true).await.expect("allowed");
        assert_eq!(parsed.host, "no-such-host.invalid");
        assert!(check_url("https://8.8.8.8/x", true).await.is_ok());
        assert!(check_url("ftp://example.com/", true).await.is_err());
    }

    #[tokio::test]
    async fn accepts_public_literals() {
        assert!(check_url("https://8.8.8.8/x", false).await.is_ok());
        assert!(check_url("http://[2606:4700::1111]/", false).await.is_ok());
    }

    #[tokio::test]
    async fn rejects_other_schemes_and_malformed_urls() {
        for url in [
            "ftp://example.com/", "file:///etc/passwd", "javascript:alert(1)", "example.com", "//example.com",
            "http://", "http:///path", "https://user:pw@example.com/", "http://exa mple.com/",
            "http://example.com:99999/", "http://[::1/", "http://[fe80::1%25eth0]/",
        ] {
            assert!(rejected(url).await, "{url} must be refused");
        }
    }

    #[test]
    fn parses_http_urls() {
        let u = parse_http_url("HTTPS://Example.COM:8443/a/b?q=1#frag").unwrap();
        assert_eq!(
            u,
            ParsedUrl { https: true, host: "example.com".into(), port: 8443, path: "/a/b?q=1".into() }
        );
        let u = parse_http_url("http://example.com?x=1").unwrap();
        assert_eq!((u.port, u.path.as_str()), (80, "/?x=1"));
        assert_eq!(parse_http_url("http://[2606:4700::1111]:81/").unwrap().origin(), "http://[2606:4700::1111]:81");
    }

    #[test]
    fn resolves_image_references() {
        let base = parse_http_url("https://example.com/blog/post?x=a/b").unwrap();
        let r = |s| resolve_reference(&base, s);
        assert_eq!(r("https://cdn.test/i.png").as_deref(), Some("https://cdn.test/i.png"));
        assert_eq!(r("//cdn.test/i.png").as_deref(), Some("https://cdn.test/i.png"));
        assert_eq!(r("/img/i.png").as_deref(), Some("https://example.com/img/i.png"));
        assert_eq!(r("i.png").as_deref(), Some("https://example.com/blog/i.png"));
        assert_eq!(r("  "), None);
    }

    fn png(width: u32, height: u32) -> Vec<u8> {
        let img = image::RgbImage::from_pixel(width, height, image::Rgb([200, 30, 30]));
        let mut out = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png).unwrap();
        out
    }

    #[test]
    fn thumbnail_is_a_small_jpeg() {
        let jpeg = make_thumbnail(&png(600, 300)).unwrap();
        let decoded = image::load_from_memory_with_format(&jpeg, image::ImageFormat::Jpeg).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (140, 70));
        assert!(make_thumbnail(b"not an image").is_err());
    }

    #[test]
    fn thumbnail_refuses_oversized_dimensions() {
        assert!(make_thumbnail(&png(MAX_IMAGE_SIDE + 1, 1)).is_err());
    }

    #[test]
    fn thumbnail_refuses_a_header_declaring_8000_by_8000() {
        // Only the header matters: the decoder must refuse before allocating.
        let mut data = png(1, 1);
        data[16..20].copy_from_slice(&8000u32.to_be_bytes());
        data[20..24].copy_from_slice(&8000u32.to_be_bytes());
        assert!(make_thumbnail(&data).is_err());
    }

    fn jpeg(width: u32, height: u32) -> Vec<u8> {
        let img = image::RgbImage::from_pixel(width, height, image::Rgb([10, 200, 10]));
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 70).encode_image(&img).unwrap();
        out
    }

    fn card(text: &str, matched: &str) -> wa::Message {
        incoming(wa::message::ExtendedTextMessage {
            text: Some(text.into()),
            matched_text: Some(matched.into()),
            title: Some("T".into()),
            ..Default::default()
        })
    }

    #[test]
    fn incoming_preview_requires_the_link_verbatim_in_the_text() {
        assert!(preview_from_message(&card("go https://x.test/a now", "https://x.test/a")).is_some());
        assert_eq!(preview_from_message(&card("go https://evil.test/ now", "https://x.test/a")), None);
        assert_eq!(preview_from_message(&card("go HTTPS://X.TEST/A now", "https://x.test/a")), None);
        let no_text = incoming(wa::message::ExtendedTextMessage {
            matched_text: Some("https://x.test/a".into()),
            title: Some("T".into()),
            ..Default::default()
        });
        assert_eq!(preview_from_message(&no_text), None);
        assert_eq!(preview_from_message(&card("ftp://x.test/a", "ftp://x.test/a")), None);
    }

    #[test]
    fn incoming_text_fields_are_bounded_and_cleaned() {
        let msg = incoming(wa::message::ExtendedTextMessage {
            text: Some("https://x.test/".into()),
            matched_text: Some("https://x.test/".into()),
            title: Some(format!("a\u{0}b\n{}", "\u{e9}".repeat(1000))),
            description: Some("\u{4e2d}".repeat(5000)),
            ..Default::default()
        });
        let p = preview_from_message(&msg).unwrap();
        assert_eq!(p.title.chars().count(), MAX_INCOMING_TITLE_CHARS);
        assert!(p.title.starts_with("ab\u{e9}"));
        assert_eq!(p.description.chars().count(), MAX_INCOMING_DESCRIPTION_CHARS);
    }

    #[test]
    fn incoming_thumbnail_is_validated() {
        let with = |t: Vec<u8>| {
            preview_from_message(&incoming(wa::message::ExtendedTextMessage {
                text: Some("https://x.test/".into()),
                matched_text: Some("https://x.test/".into()),
                title: Some("T".into()),
                jpeg_thumbnail: Some(t),
                ..Default::default()
            }))
            .unwrap()
            .thumbnail_jpeg
        };
        let good = jpeg(100, 50);
        assert_eq!(with(good.clone()), Some(good));
        assert_eq!(with(vec![1, 2, 3]), None, "not an image");
        assert_eq!(with(jpeg(600, 600)), None, "too many pixels");
        let mut big = jpeg(100, 50);
        big.resize(MAX_INCOMING_THUMB_BYTES + 1, 0);
        assert_eq!(with(big), None, "too many bytes");
    }

    fn incoming(ext: wa::message::ExtendedTextMessage) -> wa::Message {
        wa::Message { extended_text_message: MessageField::some(ext), ..Default::default() }
    }

    #[test]
    fn maps_incoming_preview() {
        let msg = incoming(wa::message::ExtendedTextMessage {
            text: Some("look https://x.test/a".into()),
            matched_text: Some("https://x.test/a".into()),
            title: Some("T".into()),
            description: Some("D".into()),
            jpeg_thumbnail: Some(jpeg(20, 10)),
            ..Default::default()
        });
        assert_eq!(
            preview_from_message(&msg),
            Some(LinkPreview {
                url: "https://x.test/a".into(),
                title: "T".into(),
                description: "D".into(),
                thumbnail_jpeg: Some(jpeg(20, 10)),
            })
        );
    }

    #[test]
    fn incoming_preview_is_found_inside_wrappers() {
        let inner = incoming(wa::message::ExtendedTextMessage {
            text: Some("http://x.test/".into()),
            matched_text: Some("http://x.test/".into()),
            title: Some("T".into()),
            ..Default::default()
        });
        let wrapped = wa::Message {
            ephemeral_message: MessageField::some(wa::message::FutureProofMessage {
                message: MessageField::some(inner),
            }),
            ..Default::default()
        };
        let p = preview_from_message(&wrapped).unwrap();
        assert_eq!((p.url.as_str(), p.title.as_str(), p.thumbnail_jpeg), ("http://x.test/", "T", None));
    }

    #[test]
    fn incoming_without_usable_preview_is_none() {
        assert_eq!(preview_from_message(&wa::Message::default()), None);
        let plain = incoming(wa::message::ExtendedTextMessage { text: Some("hi".into()), ..Default::default() });
        assert_eq!(preview_from_message(&plain), None);
        let bad_scheme = incoming(wa::message::ExtendedTextMessage {
            text: Some("javascript:alert(1)".into()),
            matched_text: Some("javascript:alert(1)".into()),
            title: Some("T".into()),
            ..Default::default()
        });
        assert_eq!(preview_from_message(&bad_scheme), None);
        let bare = incoming(wa::message::ExtendedTextMessage {
            text: Some("https://x.test/".into()),
            matched_text: Some("https://x.test/".into()),
            jpeg_thumbnail: Some(Vec::new()),
            ..Default::default()
        });
        assert_eq!(preview_from_message(&bare), None);
    }

    #[test]
    fn apply_preview_roundtrips() {
        let p = LinkPreview {
            url: "https://x.test/a".into(),
            title: "T".into(),
            description: String::new(),
            thumbnail_jpeg: Some(jpeg(20, 10)),
        };
        let msg = text_message_with_preview("see https://x.test/a", &p);
        let ext = msg.extended_text_message.as_option().unwrap();
        assert_eq!(ext.text.as_deref(), Some("see https://x.test/a"));
        assert_eq!(ext.description, None);
        assert_eq!(preview_from_message(&msg), Some(p));
    }

    #[test]
    fn first_url_picks_the_first_link_and_trims_punctuation() {
        assert_eq!(first_url("no links here"), None);
        assert_eq!(first_url("see https://a.test/x, ok"), Some("https://a.test/x"));
        assert_eq!(first_url("(http://a.test/p_(1)) and https://b.test"), Some("http://a.test/p_(1)"));
        assert_eq!(first_url("HTTPS://A.TEST/Path?q=1."), Some("HTTPS://A.TEST/Path?q=1"));
        assert_eq!(first_url("xhttps://a.test then https://b.test"), Some("https://b.test"));
        assert_eq!(first_url("ftp://a.test https://"), None);
        assert_eq!(first_url("https://a.test/\u{e9}!\nhttps://b.test"), Some("https://a.test/\u{e9}"));
    }
}

//! Booking sites (fork addition): one calrs instance serves several booking
//! hosts, such as `book.tomelliot.net` and `book.coscope.ai`. Each host has its
//! own skin, its own list of meeting types and its own email sender.
//!
//! Configured in `<data_dir>/sites.toml`. Without that file nothing changes:
//! every host behaves like stock calrs.
//!
//! A request whose `Host` names a site runs inside [`CURRENT`], so templates
//! (the `site()` global), the logo helper and the profile page read the site
//! without each handler passing it on. On a site host only the public booking
//! paths answer ([`path_allowed`]); everything else, the dashboard included,
//! is a 404.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// The file read from the data directory.
pub const CONFIG_FILE: &str = "sites.toml";
/// Skins live in `<data_dir>/skins/<name>/`.
pub const SKINS_DIR: &str = "skins";
/// Largest skin file accepted (fonts are the big ones).
const MAX_SKIN_FILE_BYTES: u64 = 2 * 1024 * 1024;

tokio::task_local! {
    /// The site of the request being handled, set by [`site_middleware`].
    static CURRENT: Option<Arc<Site>>;
}

/// The site of the current request, `None` outside a site host (or outside a
/// request, as in background tasks).
pub fn current() -> Option<Arc<Site>> {
    CURRENT.try_with(|site| site.clone()).ok().flatten()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    /// Where the source of this modified calrs is published (AGPL-3.0 §13).
    source_url: String,
    #[serde(default)]
    sites: HashMap<String, SiteConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SiteConfig {
    skin: String,
    title: String,
    /// Username whose meeting types this site offers.
    user: String,
    types: Vec<String>,
    /// The main site of the domain, linked from the logo.
    link: Option<String>,
    from_email: Option<String>,
    from_name: Option<String>,
    reply_to: Option<String>,
}

/// One file of a skin, held in memory.
#[derive(Debug)]
pub struct SkinFile {
    pub bytes: Vec<u8>,
    pub content_type: &'static str,
    /// Hex prefix of the SHA-256 of the bytes: the ETag and the `?v=` of URLs.
    pub version: String,
}

#[derive(Debug)]
pub struct Site {
    pub host: String,
    pub skin: String,
    pub title: String,
    pub user: String,
    pub types: Vec<String>,
    pub link: Option<String>,
    pub from_email: Option<String>,
    pub from_name: Option<String>,
    pub reply_to: Option<String>,
    pub source_url: String,
    files: HashMap<String, SkinFile>,
}

impl Site {
    pub fn offers(&self, slug: &str) -> bool {
        self.types.iter().any(|t| t == slug)
    }

    /// URL of a skin file with its version, `None` when the skin lacks it.
    pub fn file_url(&self, name: &str) -> Option<String> {
        self.files
            .get(name)
            .map(|f| format!("/skin/{}/{}?v={}", self.skin, name, f.version))
    }

    /// The first of `names` the skin has.
    fn first_url(&self, names: &[&str]) -> Option<String> {
        names.iter().find_map(|name| self.file_url(name))
    }

    pub fn logo_url(&self) -> Option<String> {
        self.first_url(&["logo.svg", "logo.png", "logo.webp"])
    }

    pub fn logo_dark_url(&self) -> Option<String> {
        self.first_url(&["logo-dark.svg", "logo-dark.png", "logo-dark.webp"])
    }

    pub fn favicon_url(&self) -> Option<String> {
        self.first_url(&["favicon.svg", "favicon.png", "favicon.ico"])
    }

    /// `https://<host>`, the base of every guest link for this site.
    pub fn base_url(&self) -> String {
        format!("https://{}", self.host)
    }
}

/// Every configured site, by host.
#[derive(Debug, Default)]
pub struct Sites {
    by_host: HashMap<String, Arc<Site>>,
    by_skin: HashMap<String, Arc<Site>>,
}

impl Sites {
    pub fn is_empty(&self) -> bool {
        self.by_host.is_empty()
    }

    pub fn get(&self, host: &str) -> Option<Arc<Site>> {
        self.by_host.get(host).cloned()
    }

    /// Every site, ordered by host.
    pub fn all(&self) -> Vec<Arc<Site>> {
        let mut sites: Vec<_> = self.by_host.values().cloned().collect();
        sites.sort_by(|a, b| a.host.cmp(&b.host));
        sites
    }

    fn skin_file(&self, skin: &str, file: &str) -> Option<&SkinFile> {
        self.by_skin.get(skin)?.files.get(file)
    }

    /// Reads `sites.toml` and the skins it names. A missing file is no sites;
    /// anything wrong in it is an error naming what is wrong.
    pub fn load(data_dir: &Path) -> anyhow::Result<Sites> {
        let path = data_dir.join(CONFIG_FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Sites::default()),
            Err(e) => anyhow::bail!("{}: {e}", path.display()),
        };
        Self::parse(&text, &data_dir.join(SKINS_DIR))
            .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))
    }

    fn parse(text: &str, skins_dir: &Path) -> anyhow::Result<Sites> {
        let config: ConfigFile = toml::from_str(text)?;
        if !config.source_url.starts_with("https://") {
            anyhow::bail!("source_url must be an https URL");
        }
        let mut sites = Sites::default();
        for (host, site) in config.sites {
            let key = normalize_host(&host)
                .filter(|h| *h == host)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "[sites.\"{host}\"]: write the host in lowercase, without a port"
                    )
                })?;
            let ctx = |msg: &str| anyhow::anyhow!("[sites.\"{host}\"]: {msg}");
            if site.types.is_empty() {
                return Err(ctx("types is empty"));
            }
            if let Some(slug) = site.types.iter().find(|s| !is_slug(s)) {
                return Err(ctx(&format!("\"{slug}\" in types is not a slug")));
            }
            if !is_slug(&site.skin) {
                return Err(ctx("skin must be a folder name (letters, digits, - and _)"));
            }
            if site.user.trim().is_empty() {
                return Err(ctx("user is empty"));
            }
            for (field, value) in [
                ("from_email", &site.from_email),
                ("reply_to", &site.reply_to),
            ] {
                if let Some(address) = value {
                    if address.parse::<lettre::Address>().is_err() {
                        return Err(ctx(&format!(
                            "{field} \"{address}\" is not an email address"
                        )));
                    }
                }
            }
            if let Some(link) = &site.link {
                if !link.starts_with("https://") {
                    return Err(ctx("link must be an https URL"));
                }
            }
            let files = match sites.by_skin.get(&site.skin) {
                // Two sites may share one skin: read it once.
                Some(other) => other
                    .files
                    .iter()
                    .map(|(name, f)| {
                        (
                            name.clone(),
                            SkinFile {
                                bytes: f.bytes.clone(),
                                content_type: f.content_type,
                                version: f.version.clone(),
                            },
                        )
                    })
                    .collect(),
                None => load_skin(&skins_dir.join(&site.skin))
                    .map_err(|e| ctx(&format!("skin \"{}\": {e}", site.skin)))?,
            };
            let site = Arc::new(Site {
                host: key.clone(),
                skin: site.skin,
                title: site.title,
                user: site.user,
                types: site.types,
                link: site.link,
                from_email: site.from_email,
                from_name: site.from_name,
                reply_to: site.reply_to,
                source_url: config.source_url.clone(),
                files,
            });
            sites
                .by_skin
                .entry(site.skin.clone())
                .or_insert_with(|| site.clone());
            sites.by_host.insert(key, site);
        }
        Ok(sites)
    }
}

fn is_slug(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Content type of a file a skin may hold, `None` for any other name.
fn skin_content_type(name: &str) -> Option<&'static str> {
    let valid_name = !name.starts_with('.')
        && name.len() <= 100
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !valid_name {
        return None;
    }
    let ext = name.rsplit_once('.')?.1;
    Some(match ext {
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        _ => return None,
    })
}

fn load_skin(dir: &Path) -> anyhow::Result<HashMap<String, SkinFile>> {
    let mut files = HashMap::new();
    let entries = std::fs::read_dir(dir).map_err(|e| anyhow::anyhow!("{}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(content_type) = skin_content_type(&name) else {
            if !name.starts_with('.') {
                tracing::warn!(file = %name, dir = %dir.display(), "skin file ignored: name or type not allowed");
            }
            continue;
        };
        let meta = entry.metadata()?;
        if !meta.is_file() {
            continue;
        }
        if meta.len() > MAX_SKIN_FILE_BYTES {
            anyhow::bail!("{name} is larger than {MAX_SKIN_FILE_BYTES} bytes");
        }
        let bytes = std::fs::read(entry.path())?;
        let version = hex::encode(&Sha256::digest(&bytes)[..6]);
        files.insert(
            name,
            SkinFile {
                bytes,
                content_type,
                version,
            },
        );
    }
    if !files.contains_key("skin.css") {
        anyhow::bail!("{} has no skin.css", dir.display());
    }
    Ok(files)
}

/// Lowercase host without port or trailing dot; `None` for an IP literal in
/// brackets or an empty value.
pub fn normalize_host(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() || raw.starts_with('[') {
        return None;
    }
    let host = raw.split(':').next()?.trim_end_matches('.');
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

fn request_host(req: &Request) -> Option<String> {
    req.headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .or_else(|| req.uri().host())
        .and_then(normalize_host)
}

/// Paths a site host answers. Everything else is a 404 there, so the
/// dashboard, sign-in, API, OAuth, MCP, team pages and host token pages are
/// reachable only on the admin host.
pub fn path_allowed(site: &Site, path: &str) -> bool {
    const STATIC_EXACT: &[&str] = &[
        "/",
        "/accent.css",
        "/favicon",
        "/favicon.ico",
        "/logo",
        "/logo/dark",
        "/brand-logo",
        "/robots.txt",
    ];
    const STATIC_PREFIX: &[&str] = &[
        "/fonts/",
        "/avatar/",
        "/static/intl-tel-input/",
        "/skin/",
        // Guest token pages: the token is the secret; the booking may be from
        // any site, as links from mails sent before sites existed still work.
        "/booking/cancel/",
        "/booking/reschedule/",
        "/booking/ics/",
    ];
    if STATIC_EXACT.contains(&path) || STATIC_PREFIX.iter().any(|p| path.starts_with(p)) {
        return true;
    }
    let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    match segments.as_slice() {
        // /{slug}, /{slug}/book
        [slug] | [slug, "book"] => site.offers(slug),
        // /u/{user}, /u/{user}/
        ["u", user] | ["u", user, ""] => *user == site.user,
        // /u/{user}/{slug}, /u/{user}/{slug}/book
        ["u", user, slug] | ["u", user, slug, "book"] => *user == site.user && site.offers(slug),
        _ => false,
    }
}

/// Resolves the site of the request and runs the rest of the stack inside it.
pub async fn site_middleware(
    State(sites): State<Arc<Sites>>,
    req: Request,
    next: Next,
) -> Response {
    if sites.is_empty() {
        return next.run(req).await;
    }
    let site = request_host(&req).and_then(|host| sites.get(&host));
    if let Some(site) = &site {
        if !path_allowed(site, req.uri().path()) {
            return (StatusCode::NOT_FOUND, "Not found").into_response();
        }
    }
    CURRENT.scope(site, next.run(req)).await
}

/// `GET /skin/{skin}/{file}`: a file of a configured skin, from memory.
pub async fn serve_skin_file(
    sites: &Sites,
    headers: &HeaderMap,
    skin: &str,
    file: &str,
    versioned: bool,
) -> Response {
    let Some(f) = sites.skin_file(skin, file) else {
        return (StatusCode::NOT_FOUND, "Not found").into_response();
    };
    let etag = format!("\"{}\"", f.version);
    let fresh = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|tag| tag.trim() == etag));
    let builder = Response::builder()
        .header(header::ETAG, &etag)
        // Pages link skin files with ?v=<version>, so a changed file has a new
        // URL. Files a stylesheet names (fonts) have no version: revalidate.
        .header(
            header::CACHE_CONTROL,
            if versioned {
                "public, max-age=31536000, immutable"
            } else {
                "public, max-age=3600, must-revalidate"
            },
        )
        // An SVG opened on its own must not run script on a booking host.
        .header(
            header::CONTENT_SECURITY_POLICY,
            "default-src 'none'; style-src 'unsafe-inline'; img-src data:",
        );
    if fresh {
        return builder
            .status(StatusCode::NOT_MODIFIED)
            .body(Body::empty())
            .unwrap_or_else(|_| StatusCode::NOT_MODIFIED.into_response());
    }
    builder
        .header(
            header::CONTENT_TYPE,
            HeaderValue::from_static(f.content_type),
        )
        .body(Body::from(f.bytes.clone()))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// Registers the `site()` template global: the current site's display data,
/// or none.
pub fn register(env: &mut minijinja::Environment<'static>) {
    env.add_function("site", || {
        let Some(site) = current() else {
            return minijinja::Value::from(());
        };
        let safe = |url: Option<String>| url.map(minijinja::Value::from_safe_string);
        minijinja::context! {
            host => &site.host,
            title => &site.title,
            link => &site.link,
            user => &site.user,
            css => safe(site.file_url("skin.css")),
            logo => safe(site.logo_url()),
            logo_dark => safe(site.logo_dark_url()),
            favicon => safe(site.favicon_url()),
            source_url => &site.source_url,
        }
    });
}

// --- Guest emails (the site a booking was made on) ---
//
// Emails are sent from handlers and from background loops (reminders, expiry,
// write-back alerts), and the email layer holds no database handle. So the
// serving process keeps the sites and a booking uid → host map in memory: the
// map is loaded from `bookings.site_host` at startup and extended by
// [`record_booking`]. Only bookings made on a site host are in it.

static REGISTRY: std::sync::RwLock<Option<Arc<Sites>>> = std::sync::RwLock::new(None);
static BOOKING_HOSTS: std::sync::LazyLock<std::sync::RwLock<HashMap<String, String>>> =
    std::sync::LazyLock::new(Default::default);

/// Makes `sites` the ones guest emails use, and loads the bookings made on
/// them. Called by `create_router` when `sites.toml` names at least one site.
pub async fn install(sites: Arc<Sites>, pool: &sqlx::SqlitePool) {
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT uid, site_host FROM bookings WHERE site_host IS NOT NULL")
            .fetch_all(pool)
            .await
            .unwrap_or_else(|e| {
                tracing::error!(error = %e, "cannot read booking sites; guest emails use the default sender");
                Vec::new()
            });
    if let Ok(mut map) = BOOKING_HOSTS.write() {
        map.extend(rows);
    }
    if let Ok(mut registry) = REGISTRY.write() {
        *registry = Some(sites);
    }
}

/// Stores the current site on a booking just inserted in `tx`. Does nothing
/// outside a site host.
pub async fn record_booking(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    booking_id: &str,
    uid: &str,
) {
    let Some(site) = current() else { return };
    if let Err(e) = sqlx::query("UPDATE bookings SET site_host = ? WHERE id = ?")
        .bind(&site.host)
        .bind(booking_id)
        .execute(&mut **tx)
        .await
    {
        tracing::error!(error = %e, booking = booking_id, "cannot store the booking site");
        return;
    }
    if let Ok(mut map) = BOOKING_HOSTS.write() {
        map.insert(uid.to_string(), site.host.clone());
    }
}

/// The site the booking with this uid was made on, if it still exists.
pub fn for_booking(uid: &str) -> Option<Arc<Site>> {
    let host = BOOKING_HOSTS.read().ok()?.get(uid)?.clone();
    REGISTRY.read().ok()?.as_ref()?.get(&host)
}

/// A guest email's text for a booking made on `site`: links to the instance
/// base URL point at the site, and the host's own address (Reply-To, ICS
/// ORGANIZER, body rows) becomes the site's `reply_to`.
pub fn guest_text(site: &Site, host_email: &str, text: &str) -> String {
    let mut text = match crate::settings::base_url() {
        Some(base) => {
            let base = base.trim_end_matches('/');
            text.replace(&format!("{base}/"), &format!("{}/", site.base_url()))
        }
        None => text.to_string(),
    };
    if let Some(reply_to) = &site.reply_to {
        let host_email = host_email.trim();
        if !host_email.is_empty() && !host_email.eq_ignore_ascii_case(reply_to) {
            text = text.replace(host_email, reply_to);
        }
    }
    text
}

/// Keeps the rows of a public list the current site offers; all rows outside
/// a site host.
pub fn retain_offered<T>(rows: Vec<T>, slug: impl Fn(&T) -> &str) -> Vec<T> {
    match current() {
        Some(site) => rows
            .into_iter()
            .filter(|row| site.offers(slug(row)))
            .collect(),
        None => rows,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skins_dir(skins: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for skin in skins {
            let path = dir.path().join(skin);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(path.join("skin.css"), ":root{--accent:#123456}").unwrap();
            std::fs::write(path.join("logo.svg"), "<svg/>").unwrap();
            std::fs::write(path.join("notes.txt"), "ignored").unwrap();
        }
        dir
    }

    const CONFIG: &str = r#"
source_url = "https://github.com/example/calrs"

[sites."book.a.example"]
skin = "a"
title = "A"
user = "tom"
types = ["intro", "consulting"]
from_email = "tom@a.example"

[sites."book.b.example"]
skin = "b"
title = "B"
user = "tom"
types = ["demo"]
"#;

    fn sites() -> Sites {
        let dir = skins_dir(&["a", "b"]);
        Sites::parse(CONFIG, dir.path()).unwrap()
    }

    #[test]
    fn host_is_normalized() {
        assert_eq!(
            normalize_host("Book.A.Example:443").as_deref(),
            Some("book.a.example")
        );
        assert_eq!(
            normalize_host("book.a.example.").as_deref(),
            Some("book.a.example")
        );
        assert_eq!(normalize_host("[::1]:3000"), None);
        assert_eq!(normalize_host(""), None);
    }

    #[test]
    fn loads_sites_and_skins() {
        let sites = sites();
        let a = sites.get("book.a.example").unwrap();
        assert!(a.offers("intro") && !a.offers("demo"));
        assert!(a
            .file_url("skin.css")
            .unwrap()
            .starts_with("/skin/a/skin.css?v="));
        assert!(a.logo_url().is_some());
        assert!(a.file_url("notes.txt").is_none());
        assert_eq!(a.base_url(), "https://book.a.example");
        assert!(sites.get("calrs.a.example").is_none());
    }

    #[test]
    fn missing_file_is_no_sites() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Sites::load(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn bad_config_names_the_problem() {
        let dir = skins_dir(&["a"]);
        let bad = |text: &str| Sites::parse(text, dir.path()).unwrap_err().to_string();
        let base = "source_url = \"https://x.example\"\n";
        assert!(bad(&format!(
            "{base}[sites.\"Book.A\"]\nskin=\"a\"\ntitle=\"A\"\nuser=\"t\"\ntypes=[\"x\"]"
        ))
        .contains("lowercase"));
        assert!(bad(&format!(
            "{base}[sites.\"book.a\"]\nskin=\"a\"\ntitle=\"A\"\nuser=\"t\"\ntypes=[]"
        ))
        .contains("types is empty"));
        assert!(bad(&format!(
            "{base}[sites.\"book.a\"]\nskin=\"zz\"\ntitle=\"A\"\nuser=\"t\"\ntypes=[\"x\"]"
        ))
        .contains("skin \"zz\""));
        assert!(bad(&format!("{base}[sites.\"book.a\"]\nskin=\"a\"\ntitle=\"A\"\nuser=\"t\"\ntypes=[\"x\"]\nfrom_email=\"nope\"")).contains("from_email"));
        assert!(bad(&format!("{base}[sites.\"book.a\"]\nskin=\"a\"\ntitle=\"A\"\nuser=\"t\"\ntypes=[\"x\"]\ncolour=\"red\"")).contains("colour"));
        assert!(bad("source_url = \"http://x.example\"").contains("https"));
    }

    #[test]
    fn site_host_answers_only_public_booking_paths() {
        let sites = sites();
        let a = sites.get("book.a.example").unwrap();
        for ok in [
            "/",
            "/intro",
            "/intro/book",
            "/u/tom",
            "/u/tom/",
            "/u/tom/consulting",
            "/u/tom/consulting/book",
            "/booking/cancel/abc",
            "/booking/reschedule/abc",
            "/booking/ics/abc",
            "/accent.css",
            "/fonts/inter-latin.woff2",
            "/skin/a/skin.css",
            "/avatar/123",
        ] {
            assert!(path_allowed(&a, ok), "{ok} should be allowed");
        }
        for blocked in [
            "/demo",
            "/demo/book",
            "/u/tom/demo",
            "/u/alice",
            "/u/alice/intro",
            "/dashboard",
            "/dashboard/bookings",
            "/auth/login",
            "/api/v1/openapi.json",
            "/oauth/authorize",
            "/mcp",
            "/.well-known/oauth-authorization-server",
            "/booking/approve/abc",
            "/booking/decline/abc",
            "/booking/claim/abc",
            "/team/x",
            "/g/x",
            "/t/x",
            "/embed.js",
            "/intro/book/extra",
        ] {
            assert!(!path_allowed(&a, blocked), "{blocked} should be blocked");
        }
    }

    #[tokio::test]
    async fn current_is_set_only_inside_the_scope() {
        let sites = sites();
        assert!(current().is_none());
        let a = sites.get("book.a.example");
        let host = CURRENT
            .scope(a, async { current().map(|s| s.host.clone()) })
            .await;
        assert_eq!(host.as_deref(), Some("book.a.example"));
        let kept = CURRENT
            .scope(sites.get("book.b.example"), async {
                retain_offered(vec!["intro", "demo"], |s| s)
            })
            .await;
        assert_eq!(kept, vec!["demo"]);
    }
}

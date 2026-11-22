//! Booking sites (fork addition): one calrs instance serves several booking
//! hosts, such as `book.tomelliot.net` and `book.coscope.ai`. Each host has its
//! own skin and its own email sender. Which meeting types a site offers is
//! chosen per event type in the event type form (`event_type_sites`).
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
    /// The main site of the domain, linked from the logo.
    link: Option<String>,
    /// Former hosts of this site. A request on one is redirected here with
    /// its path, and bookings and choices stored under one move here at
    /// startup.
    #[serde(default)]
    aliases: Vec<String>,
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
    pub link: Option<String>,
    pub from_email: Option<String>,
    pub from_name: Option<String>,
    pub reply_to: Option<String>,
    pub source_url: String,
    files: HashMap<String, SkinFile>,
}

impl Site {
    /// The domain the site belongs to, its name in the UI: the host without
    /// its first label (`book.coscope.ai` → `coscope.ai`).
    pub fn domain(&self) -> &str {
        match self.host.split_once('.') {
            Some((_, rest)) if rest.contains('.') => rest,
            _ => &self.host,
        }
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
    /// Former host → its site.
    aliases: HashMap<String, Arc<Site>>,
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

    /// The site a former host now redirects to.
    pub fn alias(&self, host: &str) -> Option<Arc<Site>> {
        self.aliases.get(host).cloned()
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
            let mut aliases = Vec::new();
            for alias in &site.aliases {
                match normalize_host(alias).filter(|h| h == alias) {
                    Some(h) if h != host => aliases.push(h),
                    _ => {
                        return Err(ctx(&format!(
                            "alias \"{alias}\": write a host in lowercase, without a port"
                        )))
                    }
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
            for alias in aliases {
                sites.aliases.insert(alias, site.clone());
            }
            sites.by_host.insert(key, site);
        }
        if let Some(host) = sites
            .aliases
            .keys()
            .find(|a| sites.by_host.contains_key(*a))
        {
            anyhow::bail!("{host} is both a site and an alias");
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

/// What a site host does with a path.
#[derive(Debug, PartialEq, Eq)]
pub enum Access<'a> {
    Allowed,
    Denied,
    /// A meeting type's page: allowed when the type is offered on the site
    /// ([`offers`]).
    MeetingType(&'a str),
}

/// Paths a site host answers. Everything else is a 404 there, so the
/// dashboard, sign-in, API, OAuth, MCP, team pages and host token pages are
/// reachable only on the admin host.
pub fn path_access<'a>(site: &Site, path: &'a str) -> Access<'a> {
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
        return Access::Allowed;
    }
    let segments: Vec<&'a str> = path.trim_start_matches('/').split('/').collect();
    match segments.as_slice() {
        // /{slug}, /{slug}/book
        [slug] | [slug, "book"] if !slug.is_empty() => Access::MeetingType(slug),
        // /u/{user}, /u/{user}/
        ["u", user] | ["u", user, ""] if *user == site.user => Access::Allowed,
        // /u/{user}/{slug}, /u/{user}/{slug}/book
        ["u", user, slug] | ["u", user, slug, "book"] if *user == site.user && !slug.is_empty() => {
            Access::MeetingType(slug)
        }
        _ => Access::Denied,
    }
}

/// Whether the site owner's enabled personal event type `slug` is offered on
/// `site`. A failed read is "no".
pub async fn offers(pool: &sqlx::SqlitePool, site: &Site, slug: &str) -> bool {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM event_types et
             JOIN accounts a ON a.id = et.account_id
             JOIN users u ON u.id = a.user_id
             JOIN event_type_sites s ON s.event_type_id = et.id
             WHERE et.slug = ? AND u.username = ? AND et.enabled = 1
               AND et.team_id IS NULL AND s.site_host = ?)",
    )
    .bind(slug)
    .bind(&site.user)
    .bind(&site.host)
    .fetch_one(pool)
    .await
    .unwrap_or_else(|e| {
        tracing::error!(error = %e, "cannot read event type sites");
        false
    })
}

/// State of [`site_middleware`].
#[derive(Clone)]
pub struct SiteLayer {
    pub sites: Arc<Sites>,
    pub pool: sqlx::SqlitePool,
}

/// Resolves the site of the request and runs the rest of the stack inside it.
pub async fn site_middleware(State(layer): State<SiteLayer>, req: Request, next: Next) -> Response {
    if layer.sites.is_empty() {
        return next.run(req).await;
    }
    let host = request_host(&req);
    if let Some(site) = host.as_deref().and_then(|h| layer.sites.alias(h)) {
        // A former host: the same path on the site's current host. 308 keeps
        // the method, so a form posted to an old page still arrives.
        let path = req
            .uri()
            .path_and_query()
            .map(|p| p.as_str())
            .unwrap_or("/");
        return (
            StatusCode::PERMANENT_REDIRECT,
            [(header::LOCATION, format!("{}{}", site.base_url(), path))],
        )
            .into_response();
    }
    let site = host.and_then(|host| layer.sites.get(&host));
    if let Some(site) = &site {
        let allowed = match path_access(site, req.uri().path()) {
            Access::Allowed => true,
            Access::Denied => false,
            Access::MeetingType(slug) => offers(&layer.pool, site, slug).await,
        };
        if !allowed {
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
    // Rows stored under a former host move to the site's current host.
    for (alias, site) in &sites.aliases {
        let moved = async {
            let mut tx = pool.begin().await?;
            sqlx::query(
                "INSERT OR IGNORE INTO event_type_sites (event_type_id, site_host)
                 SELECT event_type_id, ? FROM event_type_sites WHERE site_host = ?",
            )
            .bind(&site.host)
            .bind(alias)
            .execute(&mut *tx)
            .await?;
            sqlx::query("DELETE FROM event_type_sites WHERE site_host = ?")
                .bind(alias)
                .execute(&mut *tx)
                .await?;
            let bookings = sqlx::query("UPDATE bookings SET site_host = ? WHERE site_host = ?")
                .bind(&site.host)
                .bind(alias)
                .execute(&mut *tx)
                .await?
                .rows_affected();
            tx.commit().await?;
            Ok::<u64, sqlx::Error>(bookings)
        }
        .await;
        match moved {
            Ok(n) if n > 0 => {
                tracing::info!(from = %alias, to = %site.host, bookings = n, "moved bookings to the site's current host")
            }
            Ok(_) => {}
            Err(e) => {
                tracing::error!(error = %e, from = %alias, "cannot move rows from a former host")
            }
        }
    }
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
    let stored: Vec<String> = sqlx::query_scalar("SELECT DISTINCT site_host FROM event_type_sites")
        .fetch_all(pool)
        .await
        .unwrap_or_default();
    let unknown: Vec<&String> = stored.iter().filter(|h| sites.get(h).is_none()).collect();
    if !unknown.is_empty() {
        tracing::warn!(hosts = ?unknown, "event types are offered on hosts no longer in sites.toml; they appear on no site");
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

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
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
    // The footer names the site, not calrs (HTML link and plain signature).
    let site_link = site.link.clone().unwrap_or_else(|| site.base_url());
    text = text
        .replace(
            r#"<a href="https://cal.rs" style="font-size:12px;color:#6b7280;font-weight:600;text-decoration:none;">calrs</a>"#,
            &format!(
                r#"<a href="{}" style="font-size:12px;color:#6b7280;font-weight:600;text-decoration:none;">{}</a>"#,
                html_escape(&site_link),
                html_escape(&site.title)
            ),
        )
        .replace("\u{2014} calrs", &format!("\u{2014} {}", site.title));
    if let Some(reply_to) = &site.reply_to {
        let host_email = host_email.trim();
        if !host_email.is_empty() && !host_email.eq_ignore_ascii_case(reply_to) {
            text = text.replace(host_email, reply_to);
        }
    }
    text
}

/// The organizer address of a booking's calendar files: the `reply_to` of the
/// site it was made on, else the host's address.
pub fn organizer_email(uid: &str, host_email: &str) -> String {
    for_booking(uid)
        .and_then(|site| site.reply_to.clone())
        .unwrap_or_else(|| host_email.to_string())
}

/// Keeps the rows of a public list (by event type id) that the current site
/// offers; all rows outside a site host.
pub async fn retain_offered<T>(
    pool: &sqlx::SqlitePool,
    rows: Vec<T>,
    event_type_id: impl Fn(&T) -> &str,
) -> Vec<T> {
    let Some(site) = current() else { return rows };
    let offered: std::collections::HashSet<String> =
        sqlx::query_scalar("SELECT event_type_id FROM event_type_sites WHERE site_host = ?")
            .bind(&site.host)
            .fetch_all(pool)
            .await
            .unwrap_or_default()
            .into_iter()
            .collect();
    rows.into_iter()
        .filter(|row| offered.contains(event_type_id(row)))
        .collect()
}

// --- Event type form: the "Booking sites" field ---

/// The form's site choices: `(sites, selected)`, each site as
/// `{host, domain, checked}`, and `selected` the checked hosts joined with
/// commas (the hidden field's value). Empty when no site is configured.
pub async fn form_choices(
    sites: &Sites,
    pool: &sqlx::SqlitePool,
    event_type_id: Option<&str>,
) -> (Vec<minijinja::Value>, String) {
    let stored: Vec<String> = match event_type_id {
        Some(id) => {
            sqlx::query_scalar("SELECT site_host FROM event_type_sites WHERE event_type_id = ?")
                .bind(id)
                .fetch_all(pool)
                .await
                .unwrap_or_default()
        }
        None => Vec::new(),
    };
    form_choices_from(sites, &stored)
}

/// [`form_choices`] for a list of hosts already known (a form posted back
/// with an error).
pub fn form_choices_from(sites: &Sites, selected: &[String]) -> (Vec<minijinja::Value>, String) {
    let all = sites.all();
    let choices = all
        .iter()
        .map(|site| {
            minijinja::context! {
                host => &site.host,
                domain => site.domain(),
                checked => selected.contains(&site.host),
            }
        })
        .collect();
    let checked: Vec<&str> = all
        .iter()
        .filter(|site| selected.contains(&site.host))
        .map(|site| site.host.as_str())
        .collect();
    (choices, checked.join(","))
}

/// The configured hosts in a posted `site_hosts` value; unknown ones dropped.
pub fn posted_hosts(sites: &Sites, posted: &str) -> Vec<String> {
    let mut hosts: Vec<String> = posted
        .split(',')
        .map(str::trim)
        .filter(|h| sites.get(h).is_some())
        .map(str::to_string)
        .collect();
    hosts.sort();
    hosts.dedup();
    hosts
}

/// Stores the posted site choice of an event type. `None` (the form did not
/// show the field) leaves the stored choice alone. Rows for hosts no longer
/// configured are kept, so renaming a host in sites.toml loses nothing.
pub async fn save_choice(
    sites: &Sites,
    pool: &sqlx::SqlitePool,
    event_type_id: &str,
    posted: Option<&str>,
) {
    let Some(posted) = posted else { return };
    if sites.is_empty() {
        return;
    }
    let hosts = posted_hosts(sites, posted);
    let result = async {
        let mut tx = pool.begin().await?;
        for site in sites.all() {
            sqlx::query("DELETE FROM event_type_sites WHERE event_type_id = ? AND site_host = ?")
                .bind(event_type_id)
                .bind(&site.host)
                .execute(&mut *tx)
                .await?;
        }
        for host in &hosts {
            sqlx::query("INSERT INTO event_type_sites (event_type_id, site_host) VALUES (?, ?)")
                .bind(event_type_id)
                .bind(host)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await
    }
    .await;
    if let Err(e) = result {
        tracing::error!(error = %e, event_type = event_type_id, "cannot save the event type's booking sites");
    }
}

/// Domains each event type is offered on, for the event types list.
pub async fn domains_by_event_type(
    sites: &Sites,
    pool: &sqlx::SqlitePool,
) -> HashMap<String, Vec<String>> {
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    if sites.is_empty() {
        return map;
    }
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT event_type_id, site_host FROM event_type_sites ORDER BY site_host")
            .fetch_all(pool)
            .await
            .unwrap_or_default();
    for (id, host) in rows {
        if let Some(site) = sites.get(&host) {
            map.entry(id).or_default().push(site.domain().to_string());
        }
    }
    map
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
from_email = "tom@a.example"

[sites."book.b.example"]
skin = "b"
title = "B"
user = "tom"
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
        assert!(a
            .file_url("skin.css")
            .unwrap()
            .starts_with("/skin/a/skin.css?v="));
        assert!(a.logo_url().is_some());
        assert!(a.file_url("notes.txt").is_none());
        assert_eq!(a.base_url(), "https://book.a.example");
        assert_eq!(a.domain(), "a.example");
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
        let site = |host: &str, extra: &str| {
            format!("{base}[sites.\"{host}\"]\nskin=\"a\"\ntitle=\"A\"\nuser=\"t\"\n{extra}")
        };
        assert!(bad(&site("Book.A", "")).contains("lowercase"));
        assert!(bad(&format!(
            "{base}[sites.\"book.a\"]\nskin=\"zz\"\ntitle=\"A\"\nuser=\"t\""
        ))
        .contains("skin \"zz\""));
        assert!(bad(&site("book.a", "from_email=\"nope\"")).contains("from_email"));
        assert!(bad(&site("book.a", "colour=\"red\"")).contains("colour"));
        assert!(bad("source_url = \"http://x.example\"").contains("https"));
    }

    #[test]
    fn site_host_answers_only_public_booking_paths() {
        let sites = sites();
        let a = sites.get("book.a.example").unwrap();
        for ok in [
            "/",
            "/u/tom",
            "/u/tom/",
            "/booking/cancel/abc",
            "/booking/reschedule/abc",
            "/booking/ics/abc",
            "/accent.css",
            "/fonts/inter-latin.woff2",
            "/skin/a/skin.css",
            "/avatar/123",
        ] {
            assert_eq!(path_access(&a, ok), Access::Allowed, "{ok}");
        }
        for (path, slug) in [
            ("/intro", "intro"),
            ("/intro/book", "intro"),
            ("/u/tom/consulting", "consulting"),
            ("/u/tom/consulting/book", "consulting"),
            // Reserved words are meeting types here; no type has that slug.
            ("/dashboard", "dashboard"),
            ("/mcp", "mcp"),
            ("/embed.js", "embed.js"),
        ] {
            assert_eq!(path_access(&a, path), Access::MeetingType(slug), "{path}");
        }
        for blocked in [
            "/u/alice",
            "/u/alice/intro",
            "/dashboard/bookings",
            "/auth/login",
            "/api/v1/openapi.json",
            "/oauth/authorize",
            "/.well-known/oauth-authorization-server",
            "/booking/approve/abc",
            "/booking/decline/abc",
            "/booking/claim/abc",
            "/team/x",
            "/g/x",
            "/t/x",
            "/intro/book/extra",
        ] {
            assert_eq!(path_access(&a, blocked), Access::Denied, "{blocked}");
        }
    }

    #[test]
    fn posted_hosts_keep_only_configured_sites() {
        let sites = sites();
        assert_eq!(
            posted_hosts(
                &sites,
                "book.b.example, evil.example,book.a.example,book.a.example"
            ),
            vec!["book.a.example", "book.b.example"]
        );
        assert!(posted_hosts(&sites, "").is_empty());
        let (choices, selected) = form_choices_from(&sites, &["book.b.example".to_string()]);
        assert_eq!(choices.len(), 2);
        assert_eq!(selected, "book.b.example");
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
    }
}

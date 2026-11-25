//! Fork: propose times (`/dashboard/propose`). The host picks one of their
//! meetings under the booking site the guest should land on, clicks open
//! times in a week, and copies them as an email snippet whose times link to
//! the booking form with that time chosen. Nothing is written: the snippet is
//! built in the browser, and the booking form checks the time again when the
//! guest submits it.
//!
//! Offered starts come from the slot engine the booking page and the REST API
//! use ([`offered_slots`]); busy blocks (with titles) only explain why a time
//! is refused.

use super::*;
use serde::Serialize;

/// Query of the step 2 page and the week JSON.
#[derive(Deserialize, Default)]
pub(super) struct ProposeQuery {
    site: Option<String>,
    start: Option<String>,
}

/// A meeting that can be proposed: a personal, enabled, public event type
/// of the user.
struct Meeting {
    id: String,
    title: String,
    slug: String,
    duration_min: i32,
    location_type: String,
}

/// The meeting and site a propose page is for, once access is checked.
struct Target {
    meeting: Meeting,
    username: String,
    tz: Tz,
    /// The site host the links use, `None` in stock mode.
    site: Option<Arc<sites::Site>>,
    /// Base of the guest links: the site's `https://host`, else
    /// `CALRS_BASE_URL` (empty when unset: the page uses its own origin).
    base_url: String,
}

/// The booking sites of `user`, in the order [`sites::Sites::all`] gives
/// (by host: the order of `sites.toml` is not kept by the loader).
fn user_sites(state: &AppState, user: &crate::models::User) -> Vec<Arc<sites::Site>> {
    let Some(username) = user.username.as_deref() else {
        return Vec::new();
    };
    state
        .sites
        .all()
        .into_iter()
        .filter(|s| s.user == username)
        .collect()
}

const MEETING_COLUMNS: &str =
    "SELECT et.id, et.title, et.slug, et.duration_min, COALESCE(et.location_type, '')
     FROM event_types et
     JOIN accounts a ON a.id = et.account_id
     JOIN users u ON u.id = a.user_id
     WHERE u.id = ? AND et.enabled = 1 AND et.team_id IS NULL AND et.visibility = 'public'";

type MeetingRow = (String, String, String, i32, String);

fn meeting(row: MeetingRow) -> Meeting {
    let (id, title, slug, duration_min, location_type) = row;
    Meeting {
        id,
        title,
        slug,
        duration_min,
        location_type,
    }
}

/// The user's proposable meetings, those offered on `site_host` when given,
/// in landing page order.
async fn meetings(pool: &SqlitePool, user_id: &str, site_host: Option<&str>) -> Vec<Meeting> {
    let site_filter = if site_host.is_some() {
        " AND EXISTS (SELECT 1 FROM event_type_sites s WHERE s.event_type_id = et.id AND s.site_host = ?)"
    } else {
        ""
    };
    let sql = format!("{MEETING_COLUMNS}{site_filter} ORDER BY {PAGE_ORDER}");
    let mut query = sqlx::query_as::<_, MeetingRow>(&sql).bind(user_id);
    if let Some(host) = site_host {
        query = query.bind(host);
    }
    query
        .fetch_all(pool)
        .await
        .unwrap_or_else(|e| {
            tracing::error!(error = %e, "cannot read proposable meetings");
            Vec::new()
        })
        .into_iter()
        .map(meeting)
        .collect()
}

/// The host of `CALRS_BASE_URL`, else the request's `Host`.
fn stock_host(headers: &HeaderMap) -> String {
    crate::settings::base_url()
        .and_then(|url| {
            let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
            let host = rest.split('/').next().unwrap_or_default().to_string();
            (!host.is_empty()).then_some(host)
        })
        .or_else(|| {
            headers
                .get(axum::http::header::HOST)
                .and_then(|h| h.to_str().ok())
                .map(str::to_string)
        })
        .unwrap_or_default()
}

/// The target of steps 2 and 3, or `None` (the not-found page) when the
/// event type would not be listed for the user in step 1 under `site`.
async fn target(
    state: &AppState,
    user: &crate::models::User,
    et_id: &str,
    site: Option<&str>,
) -> Option<Target> {
    let username = user.username.clone().filter(|u| !u.is_empty())?;
    let row: Option<MeetingRow> = sqlx::query_as(&format!("{MEETING_COLUMNS} AND et.id = ?"))
        .bind(&user.id)
        .bind(et_id)
        .fetch_optional(&state.pool)
        .await
        .unwrap_or_else(|e| {
            tracing::error!(error = %e, "cannot read event type to propose");
            None
        });
    let meeting = meeting(row?);
    let mine = user_sites(state, user);
    let (site, base_url) = if mine.is_empty() {
        if site.is_some() {
            return None;
        }
        (
            None,
            crate::settings::base_url()
                .unwrap_or_default()
                .trim_end_matches('/')
                .to_string(),
        )
    } else {
        let site = mine.into_iter().find(|s| Some(s.host.as_str()) == site)?;
        if !sites::offers(&state.pool, &site, &meeting.slug).await {
            return None;
        }
        let base = site.base_url();
        (Some(site), base)
    };
    let tz = get_host_tz(&state.pool, &meeting.id).await;
    Some(Target {
        meeting,
        username,
        tz,
        site,
        base_url,
    })
}

/// The `site` query of a link to step 2.
fn site_query(host: Option<&str>) -> String {
    host.map(|h| format!("?site={h}")).unwrap_or_default()
}

/// `GET /dashboard/propose`: step 1, the meetings under each booking site.
pub(super) async fn propose_pick_meeting(
    State(state): State<Arc<AppState>>,
    auth_user: crate::auth::AuthUser,
    headers: HeaderMap,
) -> Response {
    let user = &auth_user.user;
    let username = user.username.clone().unwrap_or_default();
    let row_ctx = |m: &Meeting, calendars: Vec<String>, host: Option<&str>| {
        context! {
            title => &m.title,
            duration_min => m.duration_min,
            location_type => &m.location_type,
            calendars => calendars.join(", "),
            id => &m.id,
            slug => &m.slug,
            site => host,
        }
    };
    let mut groups = Vec::new();
    let mine = user_sites(&state, user);
    if username.is_empty() {
        // No username, no /u/ link to propose.
    } else if mine.is_empty() {
        let list = meetings(&state.pool, &user.id, None).await;
        let mut rows = Vec::new();
        for m in &list {
            let cals = blocking_calendar_names(&state.pool, &user.id, &m.id).await;
            rows.push(row_ctx(m, cals, None));
        }
        if !rows.is_empty() {
            let host = stock_host(&headers);
            groups.push(context! { domain => &host, title => "", host => &host, rows => rows });
        }
    } else {
        for site in &mine {
            let list = meetings(&state.pool, &user.id, Some(&site.host)).await;
            let mut rows = Vec::new();
            for m in &list {
                let cals = blocking_calendar_names(&state.pool, &user.id, &m.id).await;
                rows.push(row_ctx(m, cals, Some(&site.host)));
            }
            if !rows.is_empty() {
                groups.push(context! {
                    domain => site.domain(),
                    title => &site.title,
                    host => &site.host,
                    rows => rows,
                });
            }
        }
    }
    let tmpl = match state.templates.get_template("propose_meeting.html") {
        Ok(t) => t,
        Err(e) => return internal_error_html("template render", &e).into_response(),
    };
    let (impersonating, impersonating_name, _) = impersonation_ctx(&auth_user);
    Html(
        tmpl.render(context! {
            groups => groups,
            username => &username,
            sidebar => sidebar_context(&auth_user, "propose"),
            lang => auth_user.lang,
            impersonating => impersonating,
            impersonating_name => impersonating_name,
        })
        .unwrap_or_else(|e| internal_error_body("template render", &e)),
    )
    .into_response()
}

/// The city of a zone id, as the snippet's "(1:00 pm Berlin)" names it.
fn zone_city(tz: Tz) -> String {
    tz.name().rsplit('/').next().unwrap_or("").replace('_', " ")
}

/// `GET /dashboard/propose/{event_type_id}`: step 2, the week and the snippet.
pub(super) async fn propose_pick_times(
    State(state): State<Arc<AppState>>,
    auth_user: crate::auth::AuthUser,
    Path(et_id): Path<String>,
    Query(query): Query<ProposeQuery>,
) -> Response {
    let Some(target) = target(&state, &auth_user.user, &et_id, query.site.as_deref()).await else {
        return dashboard_not_found(&state, &auth_user);
    };
    let lang = auth_user.lang;
    let calendars = blocking_calendar_names(&state.pool, &auth_user.user.id, &et_id).await;
    let site_host = target.site.as_ref().map(|s| s.host.clone());
    let shown_host = site_host.clone().unwrap_or_else(|| {
        let base = &target.base_url;
        base.split_once("://")
            .map_or(base.as_str(), |(_, r)| r)
            .to_string()
    });
    let week_start = week_start::event(&state.pool, &et_id).await.effective();
    // The snippet's strings in every language, since its language is chosen
    // apart from the page's.
    let snippet_i18n: serde_json::Map<String, serde_json::Value> =
        crate::i18n::supported_with_labels()
            .map(|(code, _)| {
                let t = |key: &str| crate::i18n::translate(code, key, None);
                (
                    code.to_string(),
                    serde_json::json!({
                        "header": t("propose-snippet-header"),
                        "pick": t("propose-snippet-pick"),
                        "more": t("propose-snippet-more"),
                        "intl": crate::i18n::html_lang(code),
                    }),
                )
            })
            .collect();
    let config = serde_json::json!({
        "weekUrl": format!("/dashboard/propose/{}/week{}", et_id, site_query(site_host.as_deref())),
        "bookBase": format!("{}/u/{}/{}", target.base_url, target.username, target.meeting.slug),
        "hostTz": target.tz.name(),
        "hostCity": zone_city(target.tz),
        "title": &target.meeting.title,
        "duration": target.meeting.duration_min,
        "weekStartDay": week_start.index(),
        "lang": crate::i18n::html_lang(lang),
        // The snippet is written in the page language.
        "snippetLang": lang,
    });
    let tmpl = match state.templates.get_template("propose_times.html") {
        Ok(t) => t,
        Err(e) => return internal_error_html("template render", &e).into_response(),
    };
    let (impersonating, impersonating_name, _) = impersonation_ctx(&auth_user);
    Html(
        tmpl.render(context! {
            meeting_title => &target.meeting.title,
            duration_min => target.meeting.duration_min,
            shown_host => shown_host,
            calendars => calendars.join(", "),
            config => config,
            snippet_i18n => snippet_i18n,
            tz_options => timezone_options(target.tz.name(), lang),
            host_city => zone_city(target.tz),
            host_tz => target.tz.name(),
            sidebar => sidebar_context(&auth_user, "propose"),
            lang => lang,
            impersonating => impersonating,
            impersonating_name => impersonating_name,
        })
        .unwrap_or_else(|e| internal_error_body("template render", &e)),
    )
    .into_response()
}

/// The week answer of `GET /dashboard/propose/{id}/week`. Instants in UTC.
#[derive(Serialize, Debug)]
pub(super) struct ProposeWeek {
    /// The event type's zone, the grid's zone.
    pub(super) timezone: String,
    /// First day of the week shown, in that zone.
    pub(super) start: String,
    /// Slot step the engine used, for snapping.
    pub(super) interval_min: i32,
    pub(super) duration_min: i32,
    /// Every offered start in the week.
    pub(super) offered: Vec<String>,
    /// For display and refusal reasons only.
    pub(super) busy: Vec<BusyBlock>,
    /// Availability windows (rules and overrides) inside the booking window,
    /// from now on: the area not hatched.
    pub(super) open: Vec<[String; 2]>,
    /// Previous week start, `None` for the current week.
    pub(super) prev: Option<String>,
    /// Next week start, `None` past the horizon or `bookable_until`.
    pub(super) next: Option<String>,
}

#[derive(Serialize, Debug)]
pub(super) struct BusyBlock {
    pub(super) start: String,
    pub(super) end: String,
    /// The event's SUMMARY, "Busy" for rows without one, "Booked: {guest}"
    /// for bookings, "Time off" for time off (never its private label).
    pub(super) title: String,
    /// Calendar name, `None` for bookings and time off.
    pub(super) calendar: Option<String>,
}

fn utc_text(t: chrono::DateTime<Utc>) -> String {
    t.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// A wall time of `tz` as an instant: the earlier one when it is ambiguous,
/// the time an hour later when it is skipped by a clock change.
fn instant(tz: Tz, wall: NaiveDateTime) -> Option<chrono::DateTime<Utc>> {
    tz.from_local_datetime(&wall)
        .earliest()
        .or_else(|| {
            tz.from_local_datetime(&(wall + Duration::hours(1)))
                .earliest()
        })
        .map(|t| t.with_timezone(&Utc))
}

/// A `HH:MM` window bound on `date`; `24:00` is the next midnight.
fn wall_on(date: NaiveDate, hhmm: &str) -> Option<NaiveDateTime> {
    if hhmm == "24:00" {
        return Some(date.succ_opt()?.and_time(NaiveTime::MIN));
    }
    Some(date.and_time(NaiveTime::parse_from_str(hhmm, "%H:%M").ok()?))
}

/// `GET /dashboard/propose/{event_type_id}/week?site=&start=YYYY-MM-DD`.
pub(super) async fn propose_week(
    State(state): State<Arc<AppState>>,
    auth_user: crate::auth::AuthUser,
    Path(et_id): Path<String>,
    Query(query): Query<ProposeQuery>,
) -> Response {
    let Some(target) = target(&state, &auth_user.user, &et_id, query.site.as_deref()).await else {
        return dashboard_not_found(&state, &auth_user);
    };
    // The week view syncs the calendars: the REST API's heavy limit, per user.
    if state
        .api_heavy_limiter
        .check_limited(&format!("propose-week:{}", auth_user.user.id))
        .await
    {
        return (
            axum::http::StatusCode::TOO_MANY_REQUESTS,
            axum::Json(serde_json::json!({ "error": "rate_limited" })),
        )
            .into_response();
    }
    let start = query
        .start
        .as_deref()
        .and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok());
    match propose_week_data(&state, &auth_user, &target, start).await {
        Ok(week) => axum::Json(week).into_response(),
        Err(e) => {
            tracing::error!(error = %e, %et_id, "cannot compute the propose week");
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(serde_json::json!({ "error": "internal" })),
            )
                .into_response()
        }
    }
}

async fn propose_week_data(
    state: &AppState,
    auth_user: &crate::auth::AuthUser,
    target: &Target,
    start: Option<NaiveDate>,
) -> Result<ProposeWeek, sqlx::Error> {
    let pool = &state.pool;
    let et_id = target.meeting.id.as_str();
    let user_id = auth_user.user.id.as_str();
    let lang = auth_user.lang;
    let tz = target.tz;
    let now = Utc::now();
    let today = now.with_timezone(&tz).date_naive();
    let week_start = week_start::event(pool, et_id).await.effective();
    let week_of = |d: NaiveDate| d - Duration::days(i64::from(week_start.offset(d)));
    let window = get_booking_window(pool, et_id).await;
    let (first_day, last_day) = window.effective_date_bounds(today);
    let current = week_of(today);
    let last_week = week_of(last_day).max(current);
    let start = start
        .map(week_of)
        .unwrap_or(current)
        .clamp(current, last_week);
    let end = start + Duration::days(7);
    let prev = (start > current).then(|| (start - Duration::days(7)).to_string());
    let next = (end <= last_day).then(|| end.to_string());

    // Offered: the booking page's own slots (`sync_if_stale` included).
    let offered: Vec<String> = offered_slots(state, et_id, tz, tz, start, end - Duration::days(1))
        .await?
        .into_iter()
        .map(|s| utc_text(s.start))
        .collect();

    let (wall_start, wall_end) = (start.and_time(NaiveTime::MIN), end.and_time(NaiveTime::MIN));
    let (utc_start, utc_end) = (
        instant(tz, wall_start).unwrap_or(now),
        instant(tz, wall_end).unwrap_or(now),
    );
    let mut busy: Vec<(chrono::DateTime<Utc>, BusyBlock)> = Vec::new();

    // Calendar events, by the event type's calendar filter.
    let untitled = crate::i18n::translate(lang, "propose-legend-busy", None);
    for ev in titled_calendar_events(pool, user_id, et_id, wall_start, wall_end, tz).await {
        let (Some(s), Some(e)) = (instant(tz, ev.start), instant(tz, ev.end)) else {
            continue;
        };
        let title = ev
            .summary
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| untitled.clone());
        busy.push((
            s,
            BusyBlock {
                start: utc_text(s),
                end: utc_text(e),
                title,
                calendar: ev.calendar,
            },
        ));
    }

    // calrs bookings that make the user busy (the slot engine's rule).
    let start_iso = wall_start.format("%Y-%m-%dT%H:%M:%S").to_string();
    let end_iso = wall_end.format("%Y-%m-%dT%H:%M:%S").to_string();
    let bookings: Vec<(String, String, String, i64, Option<String>)> = sqlx::query_as(&format!(
        "SELECT b.start_at, b.end_at, b.guest_name, b.time_version,
                COALESCE(NULLIF(et.timezone, ''), u.timezone)
         FROM bookings b
         JOIN event_types et ON et.id = b.event_type_id
         JOIN accounts a ON a.id = et.account_id
         LEFT JOIN users u ON u.id = a.user_id
         WHERE {}
           AND {}
         ORDER BY b.start_at",
        crate::booking_time::user_busy_bookings(true),
        crate::booking_time::BUFFERED_BOOKING_WINDOW,
    ))
    .bind(user_id)
    .bind(user_id)
    .bind(user_id)
    .bind(user_id)
    .bind(user_id)
    .bind(&end_iso)
    .bind(&start_iso)
    .fetch_all(pool)
    .await?;
    for (s, e, guest, version, zone) in bookings {
        let legacy_tz = zone.and_then(|z| z.parse().ok()).unwrap_or(Tz::UTC);
        let Some((s, e)) = crate::booking_time::stored_buffered_busy_range(
            &s,
            &e,
            version,
            legacy_tz,
            Tz::UTC,
            (0, 0),
        ) else {
            continue;
        };
        let (s, e) = (s.and_utc(), e.and_utc());
        if e <= utc_start || s >= utc_end {
            continue;
        }
        let mut args = fluent_bundle::FluentArgs::new();
        args.set("name", guest);
        busy.push((
            s,
            BusyBlock {
                start: utc_text(s),
                end: utc_text(e),
                title: crate::i18n::translate(lang, "propose-busy-booked", Some(&args)),
                calendar: None,
            },
        ));
    }

    // Time off, never with its private label.
    let off_title = crate::i18n::translate(lang, "troubleshoot-label-time-off", None);
    for p in crate::time_off::overlapping(pool, user_id, utc_start, utc_end).await? {
        busy.push((
            p.start,
            BusyBlock {
                start: utc_text(p.start),
                end: utc_text(p.end),
                title: off_title.clone(),
                calendar: None,
            },
        ));
    }
    busy.sort_by_key(|a| a.0);

    // Open windows: rules, or that day's overrides, inside the booking window.
    let rules: Vec<(i32, String, String)> = sqlx::query_as(
        "SELECT day_of_week, start_time, end_time FROM availability_rules WHERE event_type_id = ?",
    )
    .bind(et_id)
    .fetch_all(pool)
    .await?;
    let overrides: Vec<(String, Option<String>, Option<String>, i32)> = sqlx::query_as(
        "SELECT date, start_time, end_time, is_blocked FROM availability_overrides
         WHERE event_type_id = ? AND date >= ? AND date < ? ORDER BY date, start_time",
    )
    .bind(et_id)
    .bind(start.to_string())
    .bind(end.to_string())
    .fetch_all(pool)
    .await?;
    let mut open = Vec::new();
    for i in 0..7 {
        let date = start + Duration::days(i);
        if date < first_day || date > last_day {
            continue;
        }
        let weekday = date.weekday().num_days_from_sunday() as i32;
        let weekly: Vec<(String, String)> = rules
            .iter()
            .filter(|(d, _, _)| *d == weekday)
            .map(|(_, s, e)| (s.clone(), e.clone()))
            .collect();
        let day = date.to_string();
        let day_overrides: Vec<crate::availability_rules::DayOverride> = overrides
            .iter()
            .filter(|(d, _, _, _)| *d == day)
            .map(|(_, s, e, b)| (s.clone(), e.clone(), *b))
            .collect();
        for (s, e) in crate::availability_rules::effective_windows_for_day(&weekly, &day_overrides)
        {
            let (Some(s), Some(e)) = (
                wall_on(date, &s).and_then(|w| instant(tz, w)),
                wall_on(date, &e).and_then(|w| instant(tz, w)),
            ) else {
                continue;
            };
            let s = s.max(now);
            if s < e {
                open.push([utc_text(s), utc_text(e)]);
            }
        }
    }

    Ok(ProposeWeek {
        timezone: tz.name().to_string(),
        start: start.to_string(),
        interval_min: slot_interval(pool, et_id, target.meeting.duration_min).await,
        duration_min: target.meeting.duration_min,
        offered,
        busy: busy.into_iter().map(|(_, b)| b).collect(),
        open,
        prev,
        next,
    })
}

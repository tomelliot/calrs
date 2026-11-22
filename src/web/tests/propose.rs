//! Fork: propose times (`/dashboard/propose`). Who may propose which
//! meeting under which site, and what the week JSON holds.

use super::*;
use axum::http::StatusCode;
use serde_json::Value;

const SITE_A: &str = "book.a.test";
const SITE_B: &str = "book.b.test";
/// A site of another user.
const SITE_C: &str = "book.c.test";

async fn offer(pool: &SqlitePool, et_id: &str, host: &str) {
    sqlx::query("INSERT INTO event_type_sites (event_type_id, site_host) VALUES (?, ?)")
        .bind(et_id)
        .bind(host)
        .execute(pool)
        .await
        .unwrap();
}

async fn retitle(pool: &SqlitePool, et_id: &str, title: &str) {
    sqlx::query("UPDATE event_types SET title = ? WHERE id = ?")
        .bind(title)
        .bind(et_id)
        .execute(pool)
        .await
        .unwrap();
}

/// The event types of [`setup_propose_app`].
struct Types {
    user_id: String,
    /// `test-meeting` ("Test Meeting"), offered on A and B.
    both: String,
    /// "Only A", offered on A.
    only_a: String,
    team: String,
    internal: String,
    private: String,
    disabled: String,
    /// Another user's public type, offered on C (that user's site).
    foreign: String,
}

/// A router with `sites.toml` (A and B of `testuser`, C of `otheruser`) and
/// the event types of [`Types`], all ticked for A except where noted.
async fn setup_propose_app() -> (Router, SqlitePool, String, Types) {
    let pool = setup_test_db().await;
    let (user_id, _, both) = seed_test_data(&pool).await;
    retitle(&pool, &both, "Test Meeting").await;
    offer(&pool, &both, SITE_A).await;
    offer(&pool, &both, SITE_B).await;
    let only_a = insert_personal_et(&pool, &user_id, "only-a").await;
    retitle(&pool, &only_a, "Only A").await;
    offer(&pool, &only_a, SITE_A).await;
    let internal = insert_personal_et(&pool, &user_id, "internal").await;
    retitle(&pool, &internal, "Internal Type").await;
    sqlx::query("UPDATE event_types SET visibility = 'internal' WHERE id = ?")
        .bind(&internal)
        .execute(&pool)
        .await
        .unwrap();
    offer(&pool, &internal, SITE_A).await;
    let private = insert_personal_et(&pool, &user_id, "private").await;
    retitle(&pool, &private, "Private Type").await;
    sqlx::query("UPDATE event_types SET visibility = 'private' WHERE id = ?")
        .bind(&private)
        .execute(&pool)
        .await
        .unwrap();
    offer(&pool, &private, SITE_A).await;
    let disabled = insert_personal_et(&pool, &user_id, "disabled").await;
    retitle(&pool, &disabled, "Disabled Type").await;
    sqlx::query("UPDATE event_types SET enabled = 0 WHERE id = ?")
        .bind(&disabled)
        .execute(&pool)
        .await
        .unwrap();
    offer(&pool, &disabled, SITE_A).await;
    let (_, team) = insert_team_with_et(&pool, "crew", "team-type", &[(&user_id, "admin")]).await;
    retitle(&pool, &team, "Team Type").await;
    offer(&pool, &team, SITE_A).await;
    let other = insert_role_user(&pool, "otheruser@example.com", "user").await;
    let foreign = insert_personal_et(&pool, &other, "foreign").await;
    retitle(&pool, &foreign, "Foreign Type").await;
    offer(&pool, &foreign, SITE_C).await;
    offer(&pool, &foreign, SITE_A).await;

    let session = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO sessions (id, user_id, expires_at) VALUES (?, ?, '2999-01-01 00:00:00')",
    )
    .bind(&session)
    .bind(&user_id)
    .execute(&pool)
    .await
    .unwrap();

    let data_dir = std::sync::Arc::new(TestDataDir::new());
    let skin = data_dir.join("skins").join("a");
    std::fs::create_dir_all(&skin).unwrap();
    std::fs::write(skin.join("skin.css"), ":root{--accent:#0a7}").unwrap();
    std::fs::write(
        data_dir.join("sites.toml"),
        format!(
            r#"source_url = "https://github.com/example/calrs"

[sites."{SITE_A}"]
skin = "a"
title = "Site A"
user = "testuser"

[sites."{SITE_B}"]
skin = "a"
title = "Site B"
user = "testuser"

[sites."{SITE_C}"]
skin = "a"
title = "Site C"
user = "otheruser"
"#
        ),
    )
    .unwrap();
    let router = create_router(pool.clone(), data_dir.to_path_buf(), [0u8; 32])
        .await
        .layer(axum::Extension(data_dir))
        .layer(local_proxy_peer());
    let types = Types {
        user_id,
        both,
        only_a,
        team,
        internal,
        private,
        disabled,
        foreign,
    };
    (router, pool, session, types)
}

async fn page(app: &Router, uri: &str, session: &str) -> (StatusCode, String) {
    let response = app.clone().oneshot(get_authed(uri, session)).await.unwrap();
    let status = response.status();
    (status, body_string(response).await)
}

async fn week(app: &Router, uri: &str, session: &str) -> Value {
    let (status, body) = page(app, uri, session).await;
    assert_eq!(status, StatusCode::OK, "{uri}: {body}");
    serde_json::from_str(&body).unwrap()
}

/// The `<section class="card">` of the page holding `domain`, as text.
fn site_card<'a>(page: &'a str, domain: &str) -> &'a str {
    let start = page
        .find(&format!(">{domain}<span class=\"num\">"))
        .unwrap_or_else(|| panic!("no card for {domain}"));
    let rest = &page[start..];
    &rest[..rest.find("</section>").unwrap()]
}

#[tokio::test]
async fn step_one_lists_each_sites_proposable_meetings() {
    let (app, _, session, types) = setup_propose_app().await;
    let (status, body) = page(&app, "/dashboard/propose", &session).await;
    assert_eq!(status, StatusCode::OK);
    // Site::domain() is the host without its first label.
    let a = site_card(&body, "a.test");
    let b = site_card(&body, "b.test");
    assert!(a.contains("Test Meeting") && a.contains("Only A"), "{a}");
    assert!(a.contains(&format!("/dashboard/propose/{}?site={SITE_A}", types.both)));
    assert!(a.contains(&format!(
        "/dashboard/propose/{}?site={SITE_A}",
        types.only_a
    )));
    assert!(b.contains("Test Meeting") && !b.contains("Only A"), "{b}");
    assert!(b.contains(&format!("/dashboard/propose/{}?site={SITE_B}", types.both)));
    assert!(a.contains("/u/testuser/test-meeting"));
    for absent in [
        "Team Type",
        "Internal Type",
        "Private Type",
        "Disabled Type",
        "Foreign Type",
        "c.test",
    ] {
        assert!(!body.contains(absent), "{absent} listed");
    }
}

#[tokio::test]
async fn step_one_without_sites_is_one_group_under_the_base_url() {
    let (app, pool, session, et_id) = setup_test_app().await;
    let user_id: String = sqlx::query_scalar("SELECT id FROM users WHERE username = 'testuser'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let internal = insert_personal_et(&pool, &user_id, "hidden").await;
    sqlx::query("UPDATE event_types SET visibility = 'internal', title = 'Hidden' WHERE id = ?")
        .bind(&internal)
        .execute(&pool)
        .await
        .unwrap();
    let (status, body) = page(&app, "/dashboard/propose", &session).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body.matches("<section class=\"card\" aria-labelledby=\"pp-site-")
            .count(),
        1
    );
    assert!(body.contains(&format!("href=\"/dashboard/propose/{et_id}\"")));
    assert!(!body.contains("Hidden"));
    // Stock mode: step 2 takes no site, and refuses one.
    let (status, _) = page(&app, &format!("/dashboard/propose/{et_id}"), &session).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = page(
        &app,
        &format!("/dashboard/propose/{et_id}?site=x.test"),
        &session,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn steps_two_and_three_are_not_found_outside_step_one() {
    let (app, _, session, types) = setup_propose_app().await;
    let ok = format!("/dashboard/propose/{}?site={SITE_A}", types.both);
    let (status, body) = page(&app, &ok, &session).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Test Meeting"));
    assert!(body.contains(&format!("https://{SITE_A}/u/testuser/test-meeting")));
    let refused = [
        format!("{}?site={SITE_A}", types.foreign),
        format!("{}?site={SITE_C}", types.foreign),
        format!("{}?site={SITE_A}", types.team),
        format!("{}?site={SITE_A}", types.internal),
        format!("{}?site={SITE_A}", types.private),
        format!("{}?site={SITE_A}", types.disabled),
        // Not offered on that site.
        format!("{}?site={SITE_B}", types.only_a),
        // A site of another user.
        format!("{}?site={SITE_C}", types.both),
        // Sites configured: the site is required.
        types.both.clone(),
        "no-such-type".to_string(),
    ];
    for tail in &refused {
        for uri in [
            format!("/dashboard/propose/{tail}"),
            format!(
                "/dashboard/propose/{}/week{}",
                tail.split('?').next().unwrap(),
                tail.find('?')
                    .map_or(String::new(), |i| tail[i..].to_string())
            ),
        ] {
            let (status, _) = page(&app, &uri, &session).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
        }
    }
}

#[tokio::test]
async fn week_offers_exactly_the_rest_api_slots() {
    let (app, pool, session, types) = setup_propose_app().await;
    let next_week = (host_today() + Duration::days(7)).to_string();
    let w = week(
        &app,
        &format!(
            "/dashboard/propose/{}/week?site={SITE_A}&start={next_week}",
            types.both
        ),
        &session,
    )
    .await;
    let start = NaiveDate::parse_from_str(w["start"].as_str().unwrap(), "%Y-%m-%d").unwrap();
    assert_eq!(
        start.weekday(),
        chrono::Weekday::Mon,
        "the site's week start"
    );
    assert_eq!(w["timezone"], "UTC");
    assert_eq!(w["duration_min"], 30);
    assert_eq!(w["interval_min"], 30);
    let offered: Vec<String> = w["offered"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(!offered.is_empty());

    let token = super::api::pat(&pool, &types.user_id, "slots:read").await;
    let (status, slots) = super::api::send(
        &app,
        super::api::api_get(
            &format!(
                "/api/v1/event-types/{}/slots?from={}&to={}",
                types.both,
                start,
                start + Duration::days(6)
            ),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{slots}");
    let api: Vec<String> = slots["slots"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["start"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(offered, api);
    // Mon-Fri 09:00-17:00: the open windows are those days' hours.
    assert_eq!(w["open"].as_array().unwrap().len(), 5);
    assert_eq!(
        w["open"][0],
        serde_json::json!([format!("{start}T09:00:00Z"), format!("{start}T17:00:00Z")])
    );
}

#[tokio::test]
async fn week_busy_follows_the_calendar_selection_and_hides_time_off_labels() {
    let (app, pool, session, types) = setup_propose_app().await;
    let account_id: String = sqlx::query_scalar("SELECT id FROM accounts WHERE user_id = ?")
        .bind(&types.user_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let source = uuid::Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO caldav_sources (id, account_id, name, url, username) VALUES (?, ?, 'Src', 'https://x/dav', 'u')")
        .bind(&source).bind(&account_id).execute(&pool).await.unwrap();
    // Work: selected. Family: busy but not selected. Off: selected, not synced.
    let mut cals = Vec::new();
    for (name, busy, synced) in [("Work", 1, 1), ("Family", 1, 1), ("Off", 1, 0)] {
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO calendars (id, source_id, href, display_name, is_busy, sync_enabled) VALUES (?, ?, ?, ?, ?, ?)")
            .bind(&id).bind(&source).bind(format!("/{name}/")).bind(name).bind(busy).bind(synced)
            .execute(&pool).await.unwrap();
        cals.push(id);
    }
    for cal in [&cals[0], &cals[2]] {
        sqlx::query("INSERT INTO event_type_calendars (event_type_id, calendar_id) VALUES (?, ?)")
            .bind(&types.both)
            .bind(cal)
            .execute(&pool)
            .await
            .unwrap();
    }
    let next_week = host_today() + Duration::days(7);
    let monday = next_week - Duration::days(next_week.weekday().num_days_from_monday() as i64);
    let day = monday + Duration::days(1);
    let compact = |h: u32| format!("{}T{:02}0000", day.format("%Y%m%d"), h);
    for (cal, title, h) in [
        (&cals[0], "Work call", 10),
        (&cals[1], "Family thing", 12),
        (&cals[2], "Unsynced thing", 14),
    ] {
        sqlx::query("INSERT INTO events (id, calendar_id, uid, summary, start_at, end_at, all_day, timezone, status) VALUES (?, ?, ?, ?, ?, ?, 0, 'UTC', 'CONFIRMED')")
            .bind(uuid::Uuid::new_v4().to_string()).bind(cal).bind(uuid::Uuid::new_v4().to_string())
            .bind(title).bind(compact(h)).bind(compact(h + 1))
            .execute(&pool).await.unwrap();
    }
    sqlx::query("INSERT INTO user_time_off (id, user_id, start_at, end_at, timezone, label) VALUES (?, ?, ?, ?, 'UTC', 'Secret dentist')")
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(&types.user_id)
        .bind(format!("{}T15:00:00Z", day))
        .bind(format!("{}T16:00:00Z", day))
        .execute(&pool)
        .await
        .unwrap();

    let w = week(
        &app,
        &format!(
            "/dashboard/propose/{}/week?site={SITE_A}&start={monday}",
            types.both
        ),
        &session,
    )
    .await;
    let busy = w["busy"].as_array().unwrap();
    let titles: Vec<&str> = busy.iter().map(|b| b["title"].as_str().unwrap()).collect();
    assert!(titles.contains(&"Work call"), "{titles:?}");
    assert!(titles.contains(&"Time off"), "{titles:?}");
    assert!(!titles.contains(&"Family thing"), "{titles:?}");
    assert!(!titles.contains(&"Unsynced thing"), "{titles:?}");
    let body = w.to_string();
    assert!(!body.contains("Secret dentist"));
    let work = busy.iter().find(|b| b["title"] == "Work call").unwrap();
    assert_eq!(work["calendar"], "Work");
    assert_eq!(work["start"], format!("{day}T10:00:00Z"));
    // A busy time is not offered.
    let offered = w["offered"].to_string();
    assert!(!offered.contains(&format!("{day}T10:00:00Z")));
    assert!(offered.contains(&format!("{day}T11:00:00Z")));

    // Step 1 names the calendars that block the type.
    let (_, page1) = page(&app, "/dashboard/propose", &session).await;
    assert!(page1.contains("Checks Work"), "selected calendars");
    assert!(
        page1.contains("Checks Family"),
        "the others' default: busy synced ones"
    );
}

#[tokio::test]
async fn week_navigation_stops_at_this_week_and_the_booking_window() {
    let (app, pool, session, types) = setup_propose_app().await;
    let w = week(
        &app,
        &format!("/dashboard/propose/{}/week?site={SITE_A}", types.both),
        &session,
    )
    .await;
    assert!(w["prev"].is_null(), "{w}");
    assert!(w["next"].is_string(), "{w}");
    // A week in the past is the current one.
    let past = week(
        &app,
        &format!(
            "/dashboard/propose/{}/week?site={SITE_A}&start=2020-01-06",
            types.both
        ),
        &session,
    )
    .await;
    assert_eq!(past["start"], w["start"]);

    sqlx::query("UPDATE event_types SET bookable_until = ? WHERE id = ?")
        .bind(host_today().to_string())
        .bind(&types.both)
        .execute(&pool)
        .await
        .unwrap();
    let w = week(
        &app,
        &format!("/dashboard/propose/{}/week?site={SITE_A}", types.both),
        &session,
    )
    .await;
    assert!(w["next"].is_null(), "{w}");
}

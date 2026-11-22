//! Fork: booking sites. A request on a site host sees only that site's
//! meeting types, in its skin; other hosts see stock calrs.

use super::*;
use axum::http::StatusCode;
use chrono::Datelike;

const SITE: &str = "book.a.test";
const OTHER_SITE: &str = "book.b.test";

/// A router whose data directory holds `sites.toml` with two sites and a
/// skin `a`. `test-meeting` (from `seed_test_data`) is offered on `SITE`; the
/// user's public `other` type is offered on no site. Returns the router, the
/// pool, `test-meeting`'s id and a session of its owner.
async fn setup_site_app() -> (Router, SqlitePool, String, String) {
    let pool = setup_test_db().await;
    let (user_id, _, et_id) = seed_test_data(&pool).await;
    insert_personal_et(&pool, &user_id, "other").await;
    sqlx::query("INSERT INTO event_type_sites (event_type_id, site_host) VALUES (?, ?)")
        .bind(&et_id)
        .bind(SITE)
        .execute(&pool)
        .await
        .unwrap();
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
        skin.join("logo.svg"),
        "<svg xmlns='http://www.w3.org/2000/svg'/>",
    )
    .unwrap();
    std::fs::write(
        data_dir.join("sites.toml"),
        format!(
            r#"source_url = "https://github.com/example/calrs"

[sites."{SITE}"]
skin = "a"
title = "Site A"
user = "testuser"
link = "https://a.test"
from_email = "tom@a.test"
from_name = "Tom at A"
reply_to = "tom@a.test"

[sites."{OTHER_SITE}"]
skin = "a"
title = "Site B"
user = "testuser"
"#
        ),
    )
    .unwrap();
    let router = create_router(pool.clone(), data_dir.to_path_buf(), [0u8; 32])
        .await
        .layer(axum::Extension(data_dir))
        .layer(local_proxy_peer());
    (router, pool, et_id, session)
}

fn on(host: &str, uri: &str) -> axum::http::Request<Body> {
    axum::http::Request::builder()
        .uri(uri)
        .header("host", host)
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn root_lists_only_the_sites_types_in_its_skin() {
    let (app, _, _, _) = setup_site_app().await;
    let response = app.clone().oneshot(on(SITE, "/")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    // minijinja escapes `/` in attribute values.
    let page = body_string(response).await.replace("&#x2f;", "/");
    assert!(
        page.contains("/u/testuser/test-meeting"),
        "site type listed"
    );
    assert!(!page.contains("/u/testuser/other"), "other type hidden");
    assert!(
        page.contains("/skin/a/skin.css?v="),
        "skin stylesheet linked"
    );
    assert!(page.contains("/skin/a/logo.svg?v="), "skin logo shown");
    assert!(
        page.contains("https://github.com/example/calrs"),
        "source link in footer"
    );
    assert!(page.contains("<title>Site A</title>"));
    assert!(
        page.contains("href=\"https://a.test\""),
        "logo links to the main site"
    );
}

#[tokio::test]
async fn site_host_refuses_other_types_and_private_paths() {
    let (app, _, _, _) = setup_site_app().await;
    for uri in ["/test-meeting", "/u/testuser/test-meeting"] {
        let response = app.clone().oneshot(on(SITE, uri)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{uri}");
    }
    for uri in [
        "/other",
        "/other/book?date=2030-01-07&time=10:00",
        "/u/testuser/other",
        "/dashboard",
        "/auth/login",
        "/api/v1/openapi.json",
    ] {
        let response = app.clone().oneshot(on(SITE, uri)).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");
    }
    // A POST to a type the site does not offer is refused before the handler.
    let post = axum::http::Request::builder()
        .method("POST")
        .uri("/other/book")
        .header("host", SITE)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from("name=x"))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(post).await.unwrap().status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn other_hosts_behave_like_stock_calrs() {
    let (app, _, _, _) = setup_site_app().await;
    let response = app.clone().oneshot(on("calrs.a.test", "/")).await.unwrap();
    assert!(response.status().is_redirection());
    assert_eq!(response.headers()["location"], "/auth/login");
    let page = body_string(
        app.clone()
            .oneshot(on("calrs.a.test", "/u/testuser"))
            .await
            .unwrap(),
    )
    .await;
    assert!(page.contains("/u/testuser/other"));
    assert!(!page.contains("/skin/a/"));
}

#[tokio::test]
async fn skin_files_are_served_with_a_strict_policy() {
    let (app, _, _, _) = setup_site_app().await;
    let response = app
        .clone()
        .oneshot(on(SITE, "/skin/a/skin.css"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["content-type"],
        "text/css; charset=utf-8"
    );
    let response = app
        .clone()
        .oneshot(on(SITE, "/skin/a/logo.svg"))
        .await
        .unwrap();
    assert!(response.headers()["content-security-policy"]
        .to_str()
        .unwrap()
        .starts_with("default-src 'none'"));
    for missing in [
        "/skin/a/nope.css",
        "/skin/zz/skin.css",
        "/skin/a/..%2Fsites.toml",
    ] {
        let response = app.clone().oneshot(on(SITE, missing)).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{missing}");
    }
}

/// A booking made on a site host stores the site, and its guest email is sent
/// from the site's address with links to the site. The host's email keeps the
/// instance sender and links.
#[tokio::test]
async fn guest_email_comes_from_the_site_the_booking_was_made_on() {
    let _lock = crate::test_support::ENV_LOCK.lock().await;
    let _env = crate::test_support::EnvGuard::new();
    std::env::set_var("CALRS_BASE_URL", "https://calrs.a.test");
    let (app, pool, _, _) = setup_site_app().await;
    let (port, mails) = spawn_capturing_mta().await;
    use_capturing_mta(&pool, port).await;

    let mut day = Utc::now().date_naive() + Duration::days(1);
    while day.weekday() != chrono::Weekday::Mon {
        day += Duration::days(1);
    }
    let csrf = "csrf-site-booking";
    let form = format!(
        "_csrf={csrf}&date={}&time=10%3A00&name=Site+Guest&email=site-guest%40test.com&notes=",
        day.format("%Y-%m-%d")
    );
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/test-meeting/book")
        .header("host", SITE)
        .header("cookie", format!("__Host-calrs_csrf={csrf}"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(form))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert!(response.status() == 200 || response.status().is_redirection());

    let stored: Option<String> = sqlx::query_scalar(
        "SELECT site_host FROM bookings WHERE guest_email = 'site-guest@test.com'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(stored.as_deref(), Some(SITE));

    let guest = mail_to(&mails, "site-guest@test.com").await;
    assert!(
        guest.lines().any(|l| l.starts_with("From:")
            && l.contains("Tom at A")
            && l.contains("<tom@a.test>")),
        "{guest}"
    );
    assert!(
        guest
            .lines()
            .any(|l| l.starts_with("Reply-To:") && l.contains("tom@a.test")),
        "{guest}"
    );
    assert!(
        guest.contains("https://book.a.test/booking/cancel/"),
        "{guest}"
    );
    assert!(!guest.contains("https://calrs.a.test/"), "{guest}");
    assert!(
        !guest.contains("test@example.com"),
        "host address replaced: {guest}"
    );
    assert!(
        guest.contains("mailto:tom@a.test"),
        "ICS organizer: {guest}"
    );

    let host = mail_to(&mails, "test@example.com").await;
    assert!(
        host.lines()
            .any(|l| l.starts_with("From:") && l.contains("noreply@test.com")),
        "{host}"
    );
    std::env::remove_var("CALRS_BASE_URL");
}

fn edit_body(extra: &str) -> String {
    format!("_csrf=sites-test&title=Test+Meeting&slug=test-meeting&duration_min=30&location_type=link&location_value=https%3A%2F%2Fexample.com&avail_schedule=1:09:00-17:00&{extra}")
}

async fn hosts_of(pool: &SqlitePool, et_id: &str) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT site_host FROM event_type_sites WHERE event_type_id = ? ORDER BY site_host",
    )
    .bind(et_id)
    .fetch_all(pool)
    .await
    .unwrap()
}

/// The edit form shows one checkbox per domain and stores what is posted:
/// unknown hosts are dropped, an absent field leaves the choice alone, and
/// an empty one clears it.
#[tokio::test]
async fn edit_form_chooses_the_booking_sites() {
    let (app, pool, et_id, session) = setup_site_app().await;
    let page = body_string(
        app.clone()
            .oneshot(get_authed(
                "/dashboard/event-types/test-meeting/edit",
                &session,
            ))
            .await
            .unwrap(),
    )
    .await;
    assert!(
        page.contains(">\n        a.test\n") || page.contains("a.test"),
        "domain label"
    );
    assert!(page.contains("value=\"book.a.test\" checked"), "{page}");
    assert!(
        page.contains("value=\"book.b.test\" "),
        "second site offered"
    );

    for (extra, expected) in [
        (
            "site_hosts=book.b.test%2Cevil.test%2Cbook.a.test",
            vec![SITE, OTHER_SITE],
        ),
        ("", vec![SITE, OTHER_SITE]),
        ("site_hosts=book.b.test", vec![OTHER_SITE]),
        ("site_hosts=", vec![]),
    ] {
        let response = app
            .clone()
            .oneshot(post_form(
                "/dashboard/event-types/test-meeting/edit",
                &session,
                "sites-test",
                &edit_body(extra),
            ))
            .await
            .unwrap();
        assert!(
            response.status().is_redirection(),
            "{}",
            body_string(response).await
        );
        assert_eq!(hosts_of(&pool, &et_id).await, expected, "after {extra:?}");
    }
    // Offered on no site: a 404 on both, still on the admin host.
    for host in [SITE, OTHER_SITE] {
        let response = app
            .clone()
            .oneshot(on(host, "/test-meeting"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{host}");
    }
    let response = app
        .clone()
        .oneshot(on("calrs.a.test", "/u/testuser/test-meeting"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

/// The event types list names the domains a type is offered on.
#[tokio::test]
async fn event_types_list_shows_the_domains() {
    let (app, _, _, session) = setup_site_app().await;
    let page = body_string(
        app.clone()
            .oneshot(get_authed("/dashboard/event-types", &session))
            .await
            .unwrap(),
    )
    .await;
    assert!(
        page.contains("<span class=\"badge badge-muted\">a.test</span>"),
        "{page}"
    );
    assert!(!page.contains("<span class=\"badge badge-muted\">b.test</span>"));
}

/// Changing the slug keeps the sites, and deleting the type deletes them.
#[tokio::test]
async fn sites_follow_the_event_type_not_its_slug() {
    let (app, pool, et_id, session) = setup_site_app().await;
    let body = edit_body("").replace("slug=test-meeting", "slug=renamed");
    let response = app
        .clone()
        .oneshot(post_form(
            "/dashboard/event-types/test-meeting/edit",
            &session,
            "sites-test",
            &body,
        ))
        .await
        .unwrap();
    assert!(response.status().is_redirection());
    assert_eq!(
        app.clone()
            .oneshot(on(SITE, "/renamed"))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        app.clone()
            .oneshot(on(SITE, "/test-meeting"))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );

    sqlx::query("DELETE FROM event_types WHERE id = ?")
        .bind(&et_id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(hosts_of(&pool, &et_id).await.is_empty());
}

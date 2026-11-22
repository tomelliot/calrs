//! Fork: booking sites. A request on a site host sees only that site's
//! meeting types, in its skin; other hosts see stock calrs.

use super::*;
use axum::http::StatusCode;
use chrono::Datelike;

const SITE: &str = "book.a.test";

/// A router whose data directory holds `sites.toml` with one site offering
/// `test-meeting` (from `seed_test_data`) and a skin `a`. The user also owns
/// a public `other` type that the site must not show.
async fn setup_site_app() -> (Router, SqlitePool, String) {
    let pool = setup_test_db().await;
    let (user_id, _, _) = seed_test_data(&pool).await;
    insert_personal_et(&pool, &user_id, "other").await;

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
types = ["test-meeting"]
link = "https://a.test"
from_email = "tom@a.test"
from_name = "Tom at A"
reply_to = "tom@a.test"
"#
        ),
    )
    .unwrap();
    let router = create_router(pool.clone(), data_dir.to_path_buf(), [0u8; 32])
        .await
        .layer(axum::Extension(data_dir))
        .layer(local_proxy_peer());
    (router, pool, user_id)
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
    let (app, _, _) = setup_site_app().await;
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
    let (app, _, _) = setup_site_app().await;
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
    let (app, _, _) = setup_site_app().await;
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
    let (app, _, _) = setup_site_app().await;
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
    let (app, pool, _) = setup_site_app().await;
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

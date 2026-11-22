//! Fork: the slot page's week timeline and booking dialog. The week view is
//! the default, the slot page carries what the timeline draws, and
//! `…/book?partial=1` renders the booking form alone.

use super::*;
use axum::http::StatusCode;

const WEEK_VIEW_MIGRATION: &str = "20261122154523_fork_default_week_view";

async fn calendar_view(pool: &SqlitePool, slug: &str) -> String {
    sqlx::query_scalar("SELECT default_calendar_view FROM event_types WHERE slug = ?")
        .bind(slug)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn migration_moves_month_types_to_week() {
    let pool = setup_test_db().await;
    let (user_id, _, _) = seed_test_data(&pool).await;
    insert_personal_et(&pool, &user_id, "listed").await;
    sqlx::query(
        "UPDATE event_types SET default_calendar_view = 'month' WHERE slug = 'test-meeting'",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE event_types SET default_calendar_view = 'column' WHERE slug = 'listed'")
        .execute(&pool)
        .await
        .unwrap();
    // Run the migration again, as on a database that predates it.
    sqlx::query("DELETE FROM _migrations WHERE name = ?")
        .bind(WEEK_VIEW_MIGRATION)
        .execute(&pool)
        .await
        .unwrap();
    crate::db::migrate(&pool).await.unwrap();

    assert_eq!(calendar_view(&pool, "test-meeting").await, "week");
    assert_eq!(
        calendar_view(&pool, "listed").await,
        "column",
        "a column choice stays"
    );
}

#[tokio::test]
async fn new_event_type_defaults_to_week() {
    let (app, pool, session, _) = setup_test_app().await;
    // The form offers week first.
    let response = app
        .clone()
        .oneshot(get_authed("/dashboard/event-types/new", &session))
        .await
        .unwrap();
    let page = body_string(response).await;
    assert_eq!(selected_options(&page, "default_calendar_view"), ["week"]);

    // A form that posts no view stores week.
    let csrf = "csrf-timeline";
    let body = [
        ("_csrf", csrf),
        ("title", "Fresh"),
        ("slug", "fresh"),
        ("duration_min", "30"),
        ("location_type", "link"),
        ("location_value", "https://meet.example.com"),
        ("avail_days", "1,2,3,4,5"),
        ("avail_start", "09:00"),
        ("avail_end", "17:00"),
    ]
    .iter()
    .map(|(k, v)| format!("{k}={}", urlencoding::encode(v)))
    .collect::<Vec<_>>()
    .join("&");
    let response = app
        .clone()
        .oneshot(post_form(
            "/dashboard/event-types/new",
            &session,
            csrf,
            &body,
        ))
        .await
        .unwrap();
    assert!(response.status().is_redirection(), "{}", response.status());
    assert_eq!(calendar_view(&pool, "fresh").await, "week");
}

#[tokio::test]
async fn slot_page_renders_the_timeline_and_the_duration() {
    let (app, _, _, _) = setup_test_app().await;
    let response = app.oneshot(get("/u/testuser/test-meeting")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let page = body_string(response).await;
    assert!(page.contains(r#"<div class="week-view" id="week-view">"#));
    assert!(page.contains("window.calrsTimeline = function"));
    assert!(page.contains(r#"<dialog class="tl-dlg" id="tl-dlg""#));
    assert!(page.contains(r#"id="tl-mini""#));
    assert!(
        page.contains(r#""durationMin": 30,"#),
        "duration in the JSON"
    );
    assert!(page.contains(r#"id="tl-view-select""#));
    assert!(page.contains(r#"id="tz-select""#));
}

#[tokio::test]
async fn partial_booking_form_has_the_form_and_csrf_but_no_chrome() {
    let (app, _, _, _) = setup_test_app().await;
    let uri = format!("{}&partial=1", booking_form_uri(None));
    let request = axum::http::Request::builder()
        .uri(&uri)
        .header("cookie", "__Host-calrs_csrf=tok-123")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let page = body_string(response).await;
    assert!(page.contains("data-booking-form"));
    assert!(page.contains(r#"<form method="POST""#));
    assert!(page.contains(r#"name="_csrf" value="tok-123""#));
    assert!(page.contains(r#"name="email""#));
    assert!(!page.contains("<html"), "no page chrome");
    assert!(!page.contains("<body"), "no page chrome");
    assert!(!page.contains("back-link"), "no back link");

    // Without a CSRF cookie, the fragment brings one, matching its field.
    let response = app.clone().oneshot(get(&uri)).await.unwrap();
    let cookies: Vec<String> = response
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok().map(str::to_string))
        .filter(|c| c.starts_with("__Host-calrs_csrf="))
        .collect();
    assert_eq!(cookies.len(), 1, "{cookies:?}");
    let token = cookies[0]
        .trim_start_matches("__Host-calrs_csrf=")
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let page = body_string(response).await;
    assert!(page.contains(&format!(r#"name="_csrf" value="{token}""#)));

    // The page itself is unchanged.
    let page = body_string(app.oneshot(get(&booking_form_uri(None))).await.unwrap()).await;
    assert!(page.contains("<html") && page.contains("back-link"));
    assert!(
        !page.contains(r#"<input type="hidden" name="_csrf""#),
        "base.html injects it"
    );
}

#[tokio::test]
async fn language_cookie_picks_the_page_language() {
    let (app, _, _, _) = setup_test_app().await;
    let request = axum::http::Request::builder()
        .uri("/u/testuser/test-meeting")
        .header("accept-language", "en")
        .header("cookie", "calrs_lang=fr")
        .body(Body::empty())
        .unwrap();
    let page = body_string(app.clone().oneshot(request).await.unwrap()).await;
    assert!(page.contains(r#"<html lang="fr">"#));

    // An unknown language is ignored.
    let request = axum::http::Request::builder()
        .uri("/u/testuser/test-meeting")
        .header("accept-language", "en")
        .header("cookie", "calrs_lang=xx")
        .body(Body::empty())
        .unwrap();
    let page = body_string(app.oneshot(request).await.unwrap()).await;
    assert!(page.contains(r#"<html lang="en">"#));
}

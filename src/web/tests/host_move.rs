//! The host moves a confirmed booking to a time they pick
//! (`host_move_booking`, the reschedule page's week).

use super::*;

/// A confirmed booking at 09:00 UTC on a Monday about a month out, booked
/// by a guest in New York, and its id.
async fn booked_monday(pool: &SqlitePool, et_id: &str) -> (String, String) {
    let day = far_future_monday();
    let id = insert_v1_booking(
        pool,
        et_id,
        "guest@example.com",
        day_at(&day, 9),
        30,
        "confirmed",
        &[("guest_timezone", Some("America/New_York".into()))],
    )
    .await;
    (id, day)
}

fn day_at(day: &str, hour: u32) -> chrono::DateTime<Utc> {
    NaiveDate::parse_from_str(day, "%Y-%m-%d")
        .unwrap()
        .and_hms_opt(hour, 0, 0)
        .unwrap()
        .and_utc()
}

async fn move_to(
    app: &Router,
    session: &str,
    id: &str,
    start: chrono::DateTime<Utc>,
    force: bool,
) -> String {
    let csrf = "test-csrf-move";
    let mut body = format!(
        "_csrf={csrf}&start={}",
        utc_stamp(start).replace(':', "%3A")
    );
    if force {
        body.push_str("&force=1");
    }
    let response = app
        .clone()
        .oneshot(post_form(
            &format!("/dashboard/bookings/{id}/move"),
            session,
            csrf,
            &body,
        ))
        .await
        .unwrap();
    assert!(response.status().is_redirection(), "{}", response.status());
    response
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

async fn stored(pool: &SqlitePool, id: &str) -> (String, String, String, i32) {
    sqlx::query_as(
        "SELECT start_at, status, guest_timezone, reschedule_by_host FROM bookings WHERE id = ?",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn the_host_moves_a_booking_to_an_open_time() {
    let (app, pool, session, et_id) = setup_test_app().await;
    let (id, day) = booked_monday(&pool, &et_id).await;
    let token_before: String =
        sqlx::query_scalar("SELECT reschedule_token FROM bookings WHERE id = ?")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();

    let to = move_to(&app, &session, &id, day_at(&day, 14), false).await;
    assert_eq!(to, format!("/dashboard/bookings?moved={id}#b-{id}"));

    let (start, status, guest_tz, by_host) = stored(&pool, &id).await;
    assert_eq!(start, utc_stamp(day_at(&day, 14)));
    assert_eq!(status, "confirmed");
    // The guest's zone is theirs, not the host's: their emails stay in it.
    assert_eq!(guest_tz, "America/New_York");
    assert_eq!(by_host, 0);
    let token_after: String =
        sqlx::query_scalar("SELECT reschedule_token FROM bookings WHERE id = ?")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_ne!(
        token_before, token_after,
        "the old guest links stop working"
    );

    // The bookings page says so.
    let page = body_string(
        app.oneshot(get_authed(
            &format!("/dashboard/bookings?moved={id}"),
            &session,
        ))
        .await
        .unwrap(),
    )
    .await;
    assert!(
        page.contains("notice--success"),
        "the moved notice is shown"
    );
    assert!(page.contains("Moved Guest to"));
}

#[tokio::test]
async fn a_busy_time_needs_the_hosts_override() {
    let (app, pool, session, et_id) = setup_test_app().await;
    let (id, day) = booked_monday(&pool, &et_id).await;
    let account_id: String = sqlx::query_scalar("SELECT account_id FROM event_types WHERE id = ?")
        .bind(&et_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let compact = day.replace('-', "");
    seed_synced_event(
        &pool,
        &account_id,
        &format!("{compact}T140000Z"),
        &format!("{compact}T150000Z"),
        "UTC",
        "OPAQUE",
    )
    .await;

    let to = move_to(&app, &session, &id, day_at(&day, 14), false).await;
    assert_eq!(
        to,
        format!("/dashboard/bookings/{id}/reschedule?error=slot-unavailable")
    );
    assert_eq!(stored(&pool, &id).await.0, utc_stamp(day_at(&day, 9)));

    let to = move_to(&app, &session, &id, day_at(&day, 14), true).await;
    assert!(to.starts_with("/dashboard/bookings?moved="), "{to}");
    assert_eq!(stored(&pool, &id).await.0, utc_stamp(day_at(&day, 14)));
}

#[tokio::test]
async fn outside_the_hours_needs_the_override_and_the_past_never_moves() {
    let (app, pool, session, et_id) = setup_test_app().await;
    let (id, day) = booked_monday(&pool, &et_id).await;

    // 20:00 UTC is past the seeded 09:00-17:00.
    let to = move_to(&app, &session, &id, day_at(&day, 20), false).await;
    assert!(to.ends_with("error=slot-unavailable"), "{to}");
    let to = move_to(&app, &session, &id, day_at(&day, 20), true).await;
    assert!(to.starts_with("/dashboard/bookings?moved="), "{to}");
    assert_eq!(stored(&pool, &id).await.0, utc_stamp(day_at(&day, 20)));

    let to = move_to(&app, &session, &id, Utc::now() - Duration::hours(2), true).await;
    assert!(to.ends_with("error=slot-too-soon"), "{to}");
    assert_eq!(stored(&pool, &id).await.0, utc_stamp(day_at(&day, 20)));
}

#[tokio::test]
async fn only_the_owner_moves_a_booking() {
    let (app, pool, _, et_id) = setup_test_app().await;
    let (id, day) = booked_monday(&pool, &et_id).await;
    let (_, other) =
        insert_test_user_with_session(&pool, "Other", "other@example.com", "other").await;
    let to = move_to(&app, &other, &id, day_at(&day, 14), true).await;
    assert!(to.starts_with("/dashboard/bookings?error="), "{to}");
    assert_eq!(stored(&pool, &id).await.0, utc_stamp(day_at(&day, 9)));
}

#[tokio::test]
async fn a_request_waiting_for_approval_is_not_moved() {
    let (app, pool, session, et_id) = setup_test_app().await;
    let day = far_future_monday();
    let id = insert_v1_booking(
        &pool,
        &et_id,
        "guest@example.com",
        day_at(&day, 9),
        30,
        "pending",
        &[],
    )
    .await;
    move_to(&app, &session, &id, day_at(&day, 14), true).await;
    let (start, status, _, _) = stored(&pool, &id).await;
    assert_eq!(start, utc_stamp(day_at(&day, 9)));
    assert_eq!(status, "pending");

    // Its page is the request to the guest, as before.
    let page = body_string(
        app.oneshot(get_authed(
            &format!("/dashboard/bookings/{id}/reschedule"),
            &session,
        ))
        .await
        .unwrap(),
    )
    .await;
    assert!(!page.contains("mv-form"));
}

#[tokio::test]
async fn the_week_leaves_the_moving_booking_out() {
    let (app, pool, session, et_id) = setup_test_app().await;
    let (id, day) = booked_monday(&pool, &et_id).await;

    let page = body_string(
        app.clone()
            .oneshot(get_authed(
                &format!("/dashboard/bookings/{id}/reschedule"),
                &session,
            ))
            .await
            .unwrap(),
    )
    .await;
    assert!(
        page.contains("mv-form"),
        "a confirmed booking gets the week"
    );

    let response = app
        .oneshot(get_authed(
            &format!("/dashboard/bookings/{id}/reschedule/week?start={day}"),
            &session,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let week: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    let offered: Vec<&str> = week["offered"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    // Its own time is free to move within.
    assert!(offered.contains(&utc_stamp(day_at(&day, 9)).as_str()));
    assert!(week["busy"].as_array().unwrap().is_empty());
}

//! Fork: the booking page's week timeline and booking dialog.
//!
//! - The default slot page view is `week`, which `slots.html` draws as a
//!   timeline (`partials/timeline_*.html`).
//! - `GET …/book?partial=1` renders only the booking form
//!   (`partials/book_dialog.html`), which the slot page shows in a dialog. The
//!   form still posts to the usual booking handler.
//! - A `calrs_lang` cookie, set by the language menu of the slot page, stands
//!   in for the browser's `Accept-Language`.

use axum::extract::Request;
use axum::http::{header, HeaderValue, Method};
use axum::middleware::Next;
use axum::response::Response;

/// The slot page view of an event type that has none chosen.
pub const DEFAULT_VIEW: &str = "week";

/// The view a form posted, or [`DEFAULT_VIEW`] when it posted none or an
/// unknown one.
pub fn calendar_view(posted: Option<&str>) -> String {
    match posted {
        Some(v @ ("month" | "week" | "column")) => v.to_string(),
        _ => DEFAULT_VIEW.to_string(),
    }
}

/// Query parameter that asks a booking form for the dialog fragment.
const PARTIAL_PARAM: &str = "partial";
/// Cookie holding the language a guest picked on the slot page.
pub const LANG_COOKIE: &str = "calrs_lang";

tokio::task_local! {
    /// The CSRF token of a booking form rendered for the dialog.
    static DIALOG_CSRF: Option<String>;
}

/// The booking form template: the dialog fragment inside a `partial=1`
/// request, else the full page.
pub fn book_template() -> &'static str {
    if dialog_csrf().is_some() {
        "partials/book_dialog.html"
    } else {
        "book.html"
    }
}

fn dialog_csrf() -> Option<String> {
    DIALOG_CSRF.try_with(|t| t.clone()).ok().flatten()
}

fn wants_partial(req: &Request) -> bool {
    req.method() == Method::GET
        && req.uri().path().ends_with("/book")
        && req.uri().query().is_some_and(|q| {
            q.split('&').any(|pair| {
                let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
                k == PARTIAL_PARAM && (v == "1" || v == "true")
            })
        })
}

/// The language named by the `calrs_lang` cookie, when it is a supported one.
fn cookie_lang(req: &Request) -> Option<&'static str> {
    let prefix = format!("{LANG_COOKIE}=");
    let value = req
        .headers()
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|s| s.split(';'))
        .find_map(|part| part.trim().strip_prefix(&prefix).map(str::to_string))?;
    crate::i18n::supported_with_labels()
        .map(|(code, _)| code)
        // The formal register is the admin's choice, never a guest's.
        .find(|code| *code == value && *code != "de-formal")
}

/// Applies the language cookie, and renders `partial=1` booking forms with
/// their own CSRF field (and cookie, on a first visit).
pub async fn timeline_middleware(mut req: Request, next: Next) -> Response {
    if let Some(lang) = cookie_lang(&req) {
        req.headers_mut()
            .insert(header::ACCEPT_LANGUAGE, HeaderValue::from_static(lang));
    }
    if !wants_partial(&req) {
        return next.run(req).await;
    }
    let existing = super::csrf_token_from_headers(req.headers());
    let token = existing.clone().unwrap_or_else(super::generate_csrf_token);
    let mut response = DIALOG_CSRF.scope(Some(token.clone()), next.run(req)).await;
    if existing.is_none() {
        if let Ok(cookie) = super::csrf_cookie_value(&token).parse() {
            response.headers_mut().append(header::SET_COOKIE, cookie);
        }
    }
    response
}

/// Registers `booking_dialog_csrf()` (the dialog form's CSRF token, or none)
/// and `page_languages()` (the languages of the slot page's language menu).
pub fn register(env: &mut minijinja::Environment<'static>) {
    env.add_function("booking_dialog_csrf", || match dialog_csrf() {
        Some(token) => minijinja::Value::from(token),
        None => minijinja::Value::from(()),
    });
    env.add_function("page_languages", || {
        crate::i18n::supported_with_labels()
            .filter(|(code, _)| *code != "de-formal")
            .map(|(code, label)| minijinja::context! { code => code, label => label })
            .collect::<Vec<_>>()
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calendar_view_defaults_to_week() {
        assert_eq!(calendar_view(None), "week");
        assert_eq!(calendar_view(Some("bogus")), "week");
        assert_eq!(calendar_view(Some("month")), "month");
        assert_eq!(calendar_view(Some("column")), "column");
    }

    #[test]
    fn partial_needs_get_on_a_book_path() {
        let req = |method: Method, uri: &str| {
            axum::http::Request::builder()
                .method(method)
                .uri(uri)
                .body(axum::body::Body::empty())
                .unwrap()
        };
        assert!(wants_partial(&req(
            Method::GET,
            "/u/a/b/book?date=x&partial=1"
        )));
        assert!(!wants_partial(&req(Method::POST, "/u/a/b/book?partial=1")));
        assert!(!wants_partial(&req(Method::GET, "/u/a/b?partial=1")));
        assert!(!wants_partial(&req(Method::GET, "/u/a/b/book?partial=0")));
    }
}

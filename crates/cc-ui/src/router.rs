//! Request routing: access checks first, then the embedded single-page app or the API.

use crate::http::{Reply, Req};
use crate::{api, auth, Ctx};

const INDEX_HTML: &str = include_str!("../assets/index.html");
const APP_JS: &str = include_str!("../assets/app.js");
const APP_CSS: &str = include_str!("../assets/app.css");

const NEEDS_TOKEN_HTML: &str = "<!doctype html><meta charset=\"utf-8\"><title>CaseComplete</title>\
<body style=\"font:15px system-ui,sans-serif;max-width:32em;margin:15vh auto;padding:0 1em;color:#333\">\
<h1 style=\"font-size:20px\">Open CaseComplete from its tray icon</h1>\
<p>This page needs the private access link that the app creates each time it starts. \
Choose <b>Dashboard</b> from the CaseComplete tray menu to open it.</p></body>";

pub(crate) fn route(ctx: &Ctx, req: &Req) -> Reply {
    if !auth::host_allowed(req.host.as_deref(), ctx.port) {
        return Reply::error(403, "unexpected Host header");
    }
    if !auth::origin_allowed(req.origin.as_deref(), ctx.port) {
        return Reply::error(403, "cross-origin requests are not allowed");
    }
    if req.too_large {
        return Reply::error(413, "request body too large");
    }
    if let Some(name) = req.path.strip_prefix("/api/") {
        if !auth::token_matches(&ctx.token, req.token.as_deref()) {
            return Reply::error(401, "missing or wrong X-Token");
        }
        return api::handle(ctx, req, name.trim_end_matches('/'));
    }
    if req.method != "GET" {
        return Reply::error(405, "method not allowed");
    }
    match req.path.as_str() {
        "/" | "/index.html" => {
            if auth::token_matches(&ctx.token, req.q("t")) {
                Reply::page(200, INDEX_HTML)
            } else {
                Reply::page(403, NEEDS_TOKEN_HTML)
            }
        }
        // Static code, no user data: no token needed.
        "/app.js" => Reply::asset("application/javascript; charset=utf-8", APP_JS),
        "/app.css" => Reply::asset("text/css; charset=utf-8", APP_CSS),
        "/favicon.ico" => Reply::empty(204),
        _ => Reply::error(404, "not found"),
    }
}

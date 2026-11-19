//! Which handler a request goes to. The paths are the Go engine's, under
//! `/api`, plus a few Moul additions.

use crate::api::Request;

/// A matched route with its path parameters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    /// `GET /api/moul`
    ListCollections,
    /// `POST /api/moul`
    CreateCollection,
    /// `GET /api/moul/:name`
    GetCollection(String),
    /// `PATCH|PUT /api/moul/:name`
    UpdateCollection(String),
    /// `DELETE /api/moul/:name`
    DeleteCollection(String),
    /// `GET /api/moul/:name/email-templates`
    GetEmailTemplates(String),
    /// `PUT /api/moul/:name/email-templates`
    PutEmailTemplates(String),
    /// `GET /api/moul/:name/export`
    Export(String),
    /// `POST /api/moul/:name/import`
    Import(String),
    /// `GET /api/moul/:name/records`
    ListRecords(String),
    /// `POST /api/moul/:name/records`
    CreateRecord(String),
    /// `GET /api/moul/:name/records/:id`
    GetRecord(String, String),
    /// `PATCH /api/moul/:name/records/:id`
    UpdateRecord(String, String),
    /// `DELETE /api/moul/:name/records/:id`
    DeleteRecord(String, String),
    /// `POST /api/moul/:name/auth-with-password`
    AuthWithPassword(String),
    /// `POST /api/moul/:name/request-password-reset`
    RequestPasswordReset(String),
    /// `POST /api/moul/:name/confirm-password-reset`
    ConfirmPasswordReset(String),
    /// `POST /api/moul/:name/refresh` and `/auth-refresh`
    Refresh(String),
    /// `POST /api/moul/:name/logout`
    Logout(String),
    /// `POST /api/moul/:name/otp/request`
    RequestOtp(String),
    /// `POST /api/moul/:name/auth-with-otp`
    AuthWithOtp(String),
    /// `GET /api/moul/:name/auth-methods`
    AuthMethods(String),
    /// `POST /api/upload`
    Upload,
    /// `GET /api/upload` and `/api/files`
    ListFiles,
    /// `DELETE /api/upload/*` and `/api/files/*`
    DeleteFile(String),
    /// `GET /storage/*`
    Storage(String),
    /// `POST /api/rules/test`
    RulesTest,
    /// `GET /api/changes`: the change log (Moul).
    Changes,
    /// `GET /api/inbox`: mail kept locally (Moul).
    Inbox,
    /// `GET /api/schema`: the app definition as a schema file (Moul).
    GetSchema,
    /// `POST /api/schema`: apply a schema file (Moul).
    ApplySchema,
    /// `GET /api/types`: TypeScript types for the schema (Moul).
    Types,
    /// `GET /api/setup`
    SetupStatus,
    /// `POST /api/setup`
    Setup,
    /// `POST /api/admin/login`
    AdminLogin,
    /// `GET /api/admin/account` (and under `/api/setup`, `/api/settings`)
    RootAccount,
    /// `POST|PATCH /api/admin/account`
    UpdateRootAccount,
    /// `POST|PATCH /api/admin/password`
    UpdateRootPassword,
    /// `POST /api/admin/reload` and `/api/settings/reload`
    ReloadSettings,
    /// `GET /api/settings`
    GetSettings,
    /// `PATCH /api/settings`
    UpdateSettings,
    /// `GET /api/feature-flags`
    ListFlags,
    /// `POST /api/feature-flags`
    CreateFlag,
    /// `GET /api/feature-flags/:key`
    GetFlag(String),
    /// `PATCH /api/feature-flags/:key`
    UpdateFlag(String),
    /// `DELETE /api/feature-flags/:key`
    DeleteFlag(String),
    /// `POST /api/feature-flags/:key/eval`
    EvalFlag(String),
    /// `GET /api/workers`
    ListJobs,
    /// `POST /api/workers`
    CreateJob,
    /// `GET /api/workers/:id`
    GetJob(String),
    /// `PATCH /api/workers/:id`
    UpdateJob(String),
    /// `DELETE /api/workers/:id`
    DeleteJob(String),
    /// `POST /api/workers/retry`
    RetryJobs,
    /// `GET /api/moul/:name/webhooks`
    ListWebhooks(String),
    /// `POST /api/moul/:name/webhooks`
    CreateWebhook(String),
    /// `GET /api/moul/:name/webhooks/:id`
    GetWebhook(String, String),
    /// `PATCH|PUT /api/moul/:name/webhooks/:id`
    UpdateWebhook(String, String),
    /// `DELETE /api/moul/:name/webhooks/:id`
    DeleteWebhook(String, String),
    /// `POST /api/moul/:name/webhooks/test` and `/webhooks/:id/test`
    TestWebhook(String),
    /// `POST /api/moul/:name/email-templates/test`
    TestEmailTemplate(String),
    /// `GET /api/moul/:name/oauth2/:provider`
    OAuthAuthorize(String, String),
    /// A sign-in the host vouches for after an OAuth provider answered;
    /// never matched from a path. See `Engine::sign_in_external`.
    ExternalSignIn(String, crate::oauth::ExternalProfile),
    /// A known path with another method.
    MethodNotAllowed,
    /// No such path.
    NotFound,
}

impl Route {
    /// Whether the request is a named write: it changes the shared store,
    /// gets a request id, and is recorded in the change log whether it is
    /// applied or refused.
    pub fn is_logged_write(&self) -> bool {
        matches!(
            self,
            Route::CreateCollection
                | Route::UpdateCollection(_)
                | Route::DeleteCollection(_)
                | Route::PutEmailTemplates(_)
                | Route::Import(_)
                | Route::CreateRecord(_)
                | Route::UpdateRecord(..)
                | Route::DeleteRecord(..)
                | Route::Upload
                | Route::DeleteFile(_)
                | Route::ApplySchema
                // An unknown email signs up: a record is created.
                | Route::RequestOtp(_)
                | Route::CreateFlag
                | Route::UpdateFlag(_)
                | Route::DeleteFlag(_)
                | Route::CreateWebhook(_)
                | Route::UpdateWebhook(..)
                | Route::DeleteWebhook(..)
                // A new account may be made.
                | Route::ExternalSignIn(..)
        )
    }

    /// The operation name recorded in the change log.
    pub fn op(&self) -> &'static str {
        match self {
            Route::CreateCollection
            | Route::UpdateCollection(_)
            | Route::PutEmailTemplates(_)
            | Route::CreateWebhook(_)
            | Route::UpdateWebhook(..)
            | Route::DeleteWebhook(..) => "define",
            Route::CreateFlag | Route::UpdateFlag(_) | Route::DeleteFlag(_) => "flag",
            Route::DeleteCollection(_) => "undefine",
            Route::ApplySchema => "schema",
            Route::Import(_) => "import",
            Route::CreateRecord(_) | Route::RequestOtp(_) | Route::ExternalSignIn(..) => "create",
            Route::UpdateRecord(..) => "update",
            Route::DeleteRecord(..) => "delete",
            Route::Upload => "upload",
            Route::DeleteFile(_) => "unupload",
            _ => "",
        }
    }

    /// The collection name in the path, if any.
    pub fn collection(&self) -> Option<&str> {
        match self {
            Route::GetCollection(n)
            | Route::UpdateCollection(n)
            | Route::DeleteCollection(n)
            | Route::GetEmailTemplates(n)
            | Route::PutEmailTemplates(n)
            | Route::Export(n)
            | Route::Import(n)
            | Route::ListRecords(n)
            | Route::CreateRecord(n)
            | Route::GetRecord(n, _)
            | Route::UpdateRecord(n, _)
            | Route::DeleteRecord(n, _)
            | Route::RequestOtp(n)
            | Route::ListWebhooks(n)
            | Route::CreateWebhook(n)
            | Route::GetWebhook(n, _)
            | Route::UpdateWebhook(n, _)
            | Route::DeleteWebhook(n, _)
            | Route::ExternalSignIn(n, _) => Some(n),
            _ => None,
        }
    }

    /// The record id in the path, if any.
    pub fn record(&self) -> Option<&str> {
        match self {
            Route::GetRecord(_, id) | Route::UpdateRecord(_, id) | Route::DeleteRecord(_, id) => {
                Some(id)
            }
            _ => None,
        }
    }
}

fn decode_segment(s: &str) -> String {
    crate::api::parse_query(&format!("x={}", s.replace('+', "%2B")))
        .pop()
        .map(|(_, v)| v)
        .unwrap_or_default()
}

/// Matches a request to a route.
pub fn route(req: &Request) -> Route {
    let m = req.method.as_str();
    let path = req.path.trim_end_matches('/');
    if let Some(rest) = req.path.strip_prefix("/storage/") {
        return match m {
            "GET" | "HEAD" => Route::Storage(rest.to_owned()),
            _ => Route::MethodNotAllowed,
        };
    }
    let Some(api) = path.strip_prefix("/api") else {
        return Route::NotFound;
    };
    let segs: Vec<String> = api
        .split('/')
        .filter(|s| !s.is_empty())
        .map(decode_segment)
        .collect();
    let s: Vec<&str> = segs.iter().map(String::as_str).collect();
    let allow = |ok: bool, r: Route| if ok { r } else { Route::MethodNotAllowed };
    match s.as_slice() {
        ["moul"] => match m {
            "GET" => Route::ListCollections,
            "POST" => Route::CreateCollection,
            _ => Route::MethodNotAllowed,
        },
        ["moul", name] => match m {
            "GET" => Route::GetCollection(name.to_string()),
            "PATCH" | "PUT" => Route::UpdateCollection(name.to_string()),
            "DELETE" => Route::DeleteCollection(name.to_string()),
            _ => Route::MethodNotAllowed,
        },
        ["moul", name, "email-templates"] => match m {
            "GET" => Route::GetEmailTemplates(name.to_string()),
            "PUT" => Route::PutEmailTemplates(name.to_string()),
            _ => Route::MethodNotAllowed,
        },
        ["moul", name, "export"] => allow(m == "GET", Route::Export(name.to_string())),
        ["moul", name, "import"] => allow(m == "POST", Route::Import(name.to_string())),
        ["moul", name, "records"] => match m {
            "GET" => Route::ListRecords(name.to_string()),
            "POST" => Route::CreateRecord(name.to_string()),
            _ => Route::MethodNotAllowed,
        },
        ["moul", name, "email-templates", "test"] => {
            allow(m == "POST", Route::TestEmailTemplate(name.to_string()))
        }
        ["moul", name, "webhooks"] => match m {
            "GET" => Route::ListWebhooks(name.to_string()),
            "POST" => Route::CreateWebhook(name.to_string()),
            _ => Route::MethodNotAllowed,
        },
        ["moul", name, "webhooks", "test"] => {
            allow(m == "POST", Route::TestWebhook(name.to_string()))
        }
        ["moul", name, "webhooks", id] => match m {
            "GET" => Route::GetWebhook(name.to_string(), id.to_string()),
            "PATCH" | "PUT" => Route::UpdateWebhook(name.to_string(), id.to_string()),
            "DELETE" => Route::DeleteWebhook(name.to_string(), id.to_string()),
            _ => Route::MethodNotAllowed,
        },
        ["moul", name, "webhooks", _, "test"] => {
            allow(m == "POST", Route::TestWebhook(name.to_string()))
        }
        ["moul", name, "oauth2", provider] => allow(
            m == "GET",
            Route::OAuthAuthorize(name.to_string(), provider.to_string()),
        ),
        ["moul", name, "records", id] => match m {
            "GET" => Route::GetRecord(name.to_string(), id.to_string()),
            "PATCH" => Route::UpdateRecord(name.to_string(), id.to_string()),
            "DELETE" => Route::DeleteRecord(name.to_string(), id.to_string()),
            _ => Route::MethodNotAllowed,
        },
        ["moul", name, action] => {
            let n = name.to_string();
            match (m, *action) {
                ("POST", "auth-with-password") => Route::AuthWithPassword(n),
                ("POST", "request-password-reset") => Route::RequestPasswordReset(n),
                ("POST", "confirm-password-reset") => Route::ConfirmPasswordReset(n),
                ("POST", "refresh" | "auth-refresh") => Route::Refresh(n),
                ("POST", "logout") => Route::Logout(n),
                ("POST", "auth-with-otp") => Route::AuthWithOtp(n),
                ("GET", "auth-methods") => Route::AuthMethods(n),
                (
                    _,
                    "auth-with-password"
                    | "request-password-reset"
                    | "confirm-password-reset"
                    | "refresh"
                    | "auth-refresh"
                    | "logout"
                    | "auth-with-otp"
                    | "auth-methods",
                ) => Route::MethodNotAllowed,
                _ => Route::NotFound,
            }
        }
        ["moul", name, "otp", "request"] => allow(m == "POST", Route::RequestOtp(name.to_string())),
        ["upload"] | ["files"] => match (m, s[0]) {
            ("POST", "upload") => Route::Upload,
            ("GET", _) => Route::ListFiles,
            _ => Route::MethodNotAllowed,
        },
        ["upload" | "files", rest @ ..] if !rest.is_empty() => {
            allow(m == "DELETE", Route::DeleteFile(rest.join("/")))
        }
        ["rules", "test"] => allow(m == "POST", Route::RulesTest),
        ["changes"] => allow(m == "GET", Route::Changes),
        ["inbox"] => allow(m == "GET", Route::Inbox),
        ["schema"] => match m {
            "GET" => Route::GetSchema,
            "POST" | "PUT" => Route::ApplySchema,
            _ => Route::MethodNotAllowed,
        },
        ["types"] => allow(m == "GET", Route::Types),
        ["setup"] => match m {
            "GET" => Route::SetupStatus,
            "POST" => Route::Setup,
            _ => Route::MethodNotAllowed,
        },
        ["admin", "login"] => allow(m == "POST", Route::AdminLogin),
        ["setup" | "admin" | "settings", "account"] => match m {
            "GET" => Route::RootAccount,
            "POST" | "PATCH" => Route::UpdateRootAccount,
            _ => Route::MethodNotAllowed,
        },
        ["setup" | "admin" | "settings", "password"] => {
            allow(m == "POST" || m == "PATCH", Route::UpdateRootPassword)
        }
        ["admin" | "settings", "reload"] => allow(m == "POST", Route::ReloadSettings),
        ["settings"] => match m {
            "GET" => Route::GetSettings,
            "PATCH" => Route::UpdateSettings,
            _ => Route::MethodNotAllowed,
        },
        ["feature-flags"] => match m {
            "GET" => Route::ListFlags,
            "POST" => Route::CreateFlag,
            _ => Route::MethodNotAllowed,
        },
        ["feature-flags", key] => match m {
            "GET" => Route::GetFlag(key.to_string()),
            "PATCH" => Route::UpdateFlag(key.to_string()),
            "DELETE" => Route::DeleteFlag(key.to_string()),
            _ => Route::MethodNotAllowed,
        },
        ["feature-flags", key, "eval"] => allow(m == "POST", Route::EvalFlag(key.to_string())),
        ["workers"] => match m {
            "GET" => Route::ListJobs,
            "POST" => Route::CreateJob,
            _ => Route::MethodNotAllowed,
        },
        ["workers", "retry"] => allow(m == "POST", Route::RetryJobs),
        ["workers", id] => match m {
            "GET" => Route::GetJob(id.to_string()),
            "PATCH" => Route::UpdateJob(id.to_string()),
            "DELETE" => Route::DeleteJob(id.to_string()),
            _ => Route::MethodNotAllowed,
        },
        _ => Route::NotFound,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(method: &str, path: &str) -> Route {
        route(&Request::new(method, path))
    }

    #[test]
    fn routes() {
        assert_eq!(r("GET", "/api/moul"), Route::ListCollections);
        assert_eq!(
            r("PATCH", "/api/moul/posts"),
            Route::UpdateCollection("posts".into())
        );
        assert_eq!(
            r("DELETE", "/api/moul/posts/records/post-1"),
            Route::DeleteRecord("posts".into(), "post-1".into())
        );
        assert_eq!(
            r("POST", "/api/moul/users/otp/request"),
            Route::RequestOtp("users".into())
        );
        assert_eq!(
            r("POST", "/api/moul/users/auth-refresh"),
            Route::Refresh("users".into())
        );
        assert_eq!(
            r("GET", "/storage/abc/original.png"),
            Route::Storage("abc/original.png".into())
        );
        assert_eq!(
            r("DELETE", "/api/files/abc/original.png"),
            Route::DeleteFile("abc/original.png".into())
        );
        assert_eq!(r("PUT", "/api/moul/posts/records"), Route::MethodNotAllowed);
        assert_eq!(r("GET", "/nope"), Route::NotFound);
        assert!(r("POST", "/api/moul/posts/records").is_logged_write());
        assert!(!r("POST", "/api/moul/users/auth-with-otp").is_logged_write());
        assert!(r("POST", "/api/moul/users/otp/request").is_logged_write());
    }
}

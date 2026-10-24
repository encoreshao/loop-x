//! Shared plumbing for the mail providers (`bin/mail_providers/base.py`): the
//! bearer-token call with one refresh-and-retry on 401, and plain-address
//! filtering for search queries.

use crate::mail_http::{self, MailError, Request};
use crate::mail_provider::RefreshFn;
use serde_json::Value;

pub const MAX_SEARCH_ADDRESSES: usize = 10;

/// `^[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}$`
pub fn is_plain_address(s: &str) -> bool {
    let Some((local, domain)) = s.split_once('@') else {
        return false;
    };
    let local_ok = !local.is_empty()
        && local
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._%+-".contains(c));
    let Some((host, tld)) = domain.rsplit_once('.') else {
        return false;
    };
    local_ok
        && !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        && tld.len() >= 2
        && tld.chars().all(|c| c.is_ascii_alphabetic())
}

/// Lower-cased, de-duplicated plain email addresses, at most
/// [`MAX_SEARCH_ADDRESSES`]. Anything else is dropped: these come from calendar
/// invites (any outsider can create one) and are pasted into a search query.
pub fn plain_addresses(addresses: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for a in addresses {
        let a = a.trim().to_lowercase();
        if is_plain_address(&a) && !out.contains(&a) {
            out.push(a);
        }
    }
    out.truncate(MAX_SEARCH_ADDRESSES);
    out
}

pub struct Base {
    token: String,
    refresh: Option<RefreshFn>,
    base_url: String,
    pub account: String,
}

impl Base {
    /// `base_url` of `None` or empty means "the provider's own default".
    pub fn new(
        access_token: &str,
        refresh: Option<RefreshFn>,
        base_url: Option<&str>,
        account: &str,
    ) -> Self {
        Self {
            token: access_token.to_string(),
            refresh,
            base_url: base_url
                .unwrap_or_default()
                .trim_end_matches('/')
                .to_string(),
            account: account.to_string(),
        }
    }

    pub fn with_default_url(mut self, default: &str) -> Self {
        if self.base_url.is_empty() {
            self.base_url = default.trim_end_matches('/').to_string();
        }
        self
    }

    /// One API call. A 401 triggers a single token refresh and one retry; the
    /// refresh's own failure (re-auth, Keychain) propagates as-is.
    pub fn call(
        &mut self,
        method: &str,
        path: &str,
        json_body: Option<Value>,
        headers: &[(&str, &str)],
    ) -> Result<Value, MailError> {
        let url = if path.starts_with("http") {
            path.to_string()
        } else {
            format!("{}{path}", self.base_url)
        };
        let build = |token: &str| {
            let mut req = Request::new(method, &url).token(token).headers(headers);
            if let Some(b) = &json_body {
                req = req.json(b.clone());
            }
            req
        };
        match mail_http::request_json(&build(&self.token)) {
            Err(MailError::AuthExpired(e)) => match self.refresh.as_mut() {
                None => Err(MailError::AuthExpired(e)),
                Some(refresh) => {
                    self.token = refresh()?;
                    mail_http::request_json(&build(&self.token))
                }
            },
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mail_http::testserver::Stub;
    use serde_json::json;
    use std::cell::Cell;
    use std::rc::Rc;

    #[test]
    fn plain_addresses_filter_dedupe_and_cap() {
        let v = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            plain_addresses(&v(&[
                "A@X.com",
                "a@x.com",
                "bad",
                "x y@z.com",
                "o'neil@x.com",
                "ok@sub.x.org"
            ])),
            vec!["a@x.com", "ok@sub.x.org"]
        );
        let many: Vec<String> = (0..20).map(|i| format!("u{i}@x.com")).collect();
        assert_eq!(plain_addresses(&many).len(), MAX_SEARCH_ADDRESSES);
        assert!(plain_addresses(&v(&["a@x.c", "a@@x.com", "@x.com", "a@.com"])).is_empty());
    }

    fn counting_refresh(calls: &Rc<Cell<u32>>) -> RefreshFn {
        let c = calls.clone();
        Box::new(move || {
            c.set(c.get() + 1);
            Ok("fresh".into())
        })
    }

    #[test]
    fn a_401_refreshes_once_and_retries() {
        let stub = Stub::new();
        stub.add("GET", "/p", 401, Some(json!({}))).add(
            "GET",
            "/p",
            200,
            Some(json!({"ok": true})),
        );
        let calls = Rc::new(Cell::new(0));
        let mut base = Base::new(
            "old",
            Some(counting_refresh(&calls)),
            Some(&stub.base_url),
            "a@b.co",
        );
        assert_eq!(
            base.call("GET", "/p", None, &[]).unwrap(),
            json!({"ok": true})
        );
        assert_eq!(calls.get(), 1);
        let auth: Vec<_> = stub
            .requests()
            .iter()
            .map(|r| r.headers["authorization"].clone())
            .collect();
        assert_eq!(auth, vec!["Bearer old", "Bearer fresh"]);
    }

    #[test]
    fn a_second_401_propagates_without_a_second_refresh() {
        let stub = Stub::new();
        stub.add("GET", "/p", 401, Some(json!({})));
        let calls = Rc::new(Cell::new(0));
        let mut base = Base::new(
            "old",
            Some(counting_refresh(&calls)),
            Some(&stub.base_url),
            "",
        );
        assert!(base
            .call("GET", "/p", None, &[])
            .unwrap_err()
            .is_auth_expired());
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn without_a_refresh_a_401_is_raised() {
        let stub = Stub::new();
        stub.add("GET", "/p", 401, Some(json!({})));
        let mut base = Base::new("old", None, Some(&stub.base_url), "");
        assert!(base
            .call("GET", "/p", None, &[])
            .unwrap_err()
            .is_auth_expired());
    }

    #[test]
    fn a_failing_refresh_propagates_its_own_error() {
        let stub = Stub::new();
        stub.add("GET", "/p", 401, Some(json!({})));
        let refresh: RefreshFn = Box::new(|| Err(MailError::Reauth("gone".into())));
        let mut base = Base::new("old", Some(refresh), Some(&stub.base_url), "");
        assert_eq!(
            base.call("GET", "/p", None, &[]).unwrap_err(),
            MailError::Reauth("gone".into())
        );
    }

    #[test]
    fn absolute_urls_bypass_the_base_url() {
        let stub = Stub::new();
        stub.add("GET", "/next", 200, Some(json!({"n": 1})));
        let mut base = Base::new("t", None, Some("http://127.0.0.1:1"), "");
        assert_eq!(
            base.call("GET", &format!("{}/next", stub.base_url), None, &[])
                .unwrap(),
            json!({"n": 1})
        );
    }

    #[test]
    fn default_url_applies_only_when_no_override() {
        let b = Base::new("t", None, None, "").with_default_url("https://x.io/v1/");
        assert_eq!(b.base_url, "https://x.io/v1");
        let b = Base::new("t", None, Some("http://h/"), "").with_default_url("https://x.io/v1");
        assert_eq!(b.base_url, "http://h");
    }
}

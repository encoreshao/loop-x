//! Who to tag: only a person who clearly matches exactly one project member.

use super::api::{Member, ReleaseApi};
use super::collect::Change;

/// At most this many people are tagged; the rest are named in plain text.
pub const MAX_TAGS: usize = 40;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Resolved {
    /// GitLab usernames to `@mention`, in first-seen order.
    pub tags: Vec<String>,
    /// People in the change who could not be matched to one user (or are past the cap).
    pub plain: Vec<String>,
    /// `(lowercase author name, username)` for every tagged person, so a name in a
    /// feature can be written as a clickable mention.
    pub mentions: Vec<(String, String)>,
}

fn is_bot(name: &str) -> bool {
    name.to_ascii_lowercase().ends_with("[bot]")
}

/// The one member matching `pred`, or `None` when none or several match.
fn only(members: &[Member], pred: impl Fn(&Member) -> bool) -> Option<&Member> {
    let mut found = members.iter().filter(|m| pred(m));
    match (found.next(), found.next()) {
        (Some(m), None) => Some(m),
        _ => None,
    }
}

pub fn resolve(api: &dyn ReleaseApi, project: &str, changes: &[Change]) -> Resolved {
    let members = api.members(project).unwrap_or_default();
    let me = api
        .current_username()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let mut out = Resolved::default();
    let mut seen: Vec<String> = Vec::new();
    for c in changes {
        let key = format!(
            "{}|{}",
            c.author_name.to_ascii_lowercase(),
            c.author_email.to_ascii_lowercase()
        );
        if seen.contains(&key) || is_bot(&c.author_name) {
            continue;
        }
        seen.push(key);
        let by_mr = c.mr.as_ref().and_then(|m| {
            only(&members, |x| {
                x.username.eq_ignore_ascii_case(&m.author_username) && !m.author_username.is_empty()
            })
        });
        let user = by_mr
            .or_else(|| {
                only(&members, |x| {
                    !c.author_email.is_empty()
                        && x.email
                            .as_deref()
                            .is_some_and(|e| e.eq_ignore_ascii_case(&c.author_email))
                })
            })
            .or_else(|| {
                only(&members, |x| {
                    !c.author_name.is_empty() && x.name.eq_ignore_ascii_case(&c.author_name)
                })
            });
        match user {
            Some(u) if is_bot(&u.username) || u.username.to_ascii_lowercase() == me => {}
            Some(u) if out.tags.len() < MAX_TAGS => {
                if !out.tags.contains(&u.username) {
                    out.tags.push(u.username.clone());
                }
                out.mentions
                    .push((c.author_name.to_ascii_lowercase(), u.username.clone()));
            }
            _ if !c.author_name.is_empty() && c.author_name.to_ascii_lowercase() != me => {
                if !out.plain.contains(&c.author_name) {
                    out.plain.push(c.author_name.clone());
                }
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::release_mr::api::{FakeApi, MrInfo};

    fn member(username: &str, name: &str, email: Option<&str>) -> Member {
        Member {
            username: username.into(),
            name: name.into(),
            email: email.map(str::to_string),
        }
    }
    fn change(name: &str, email: &str, mr_author: Option<&str>) -> Change {
        Change {
            title: "t".into(),
            author_name: name.into(),
            author_email: email.into(),
            mr: mr_author.map(|a| MrInfo {
                iid: 0,
                title: "t".into(),
                author_username: a.into(),
            }),
        }
    }

    #[test]
    fn resolve_tags_only_on_a_single_clear_member_match() {
        let api = FakeApi {
            members: vec![
                member("ann", "Ann Lee", Some("ann@x.io")),
                member("bo", "Bo Chen", None),
                member("cy", "Cy", None),
            ],
            ..FakeApi::default()
        };
        let got = resolve(
            &api,
            "g/app",
            &[
                change("A. Lee", "ann@x.io", None),
                change("Bo Chen", "bo@home.net", None),
                change("Zed", "z@x.io", None),
                change("Cy", "c@x.io", Some("cy")),
            ],
        );
        assert_eq!(got.tags, vec!["ann", "bo", "cy"]);
        assert_eq!(got.plain, vec!["Zed"]);
    }

    #[test]
    fn two_members_with_the_same_name_are_plain_text_never_a_guessed_mention() {
        let api = FakeApi {
            members: vec![
                member("sam1", "Sam Ray", None),
                member("sam2", "Sam Ray", None),
            ],
            ..FakeApi::default()
        };
        let got = resolve(&api, "g/app", &[change("Sam Ray", "sam@x.io", None)]);
        assert!(got.tags.is_empty());
        assert_eq!(got.plain, vec!["Sam Ray"]);
    }

    #[test]
    fn bots_and_the_token_user_are_never_tagged_or_named() {
        let api = FakeApi {
            members: vec![
                member("renovate[bot]", "Renovate", None),
                member("loopx", "Loop X", None),
                member("ann", "Ann", None),
            ],
            me: "loopx".into(),
            ..FakeApi::default()
        };
        let got = resolve(
            &api,
            "g/app",
            &[
                change("renovate[bot]", "r@x.io", None),
                change("Loop X", "l@x.io", None),
                change("Ann", "a@x.io", None),
            ],
        );
        assert_eq!(got.tags, vec!["ann"]);
        assert!(got.plain.is_empty(), "{got:?}");
    }

    #[test]
    fn tags_are_capped_at_40_and_the_rest_are_named_in_plain_text() {
        let members: Vec<Member> = (0..45)
            .map(|i| member(&format!("u{i}"), &format!("User {i}"), None))
            .collect();
        let changes: Vec<Change> = (0..45)
            .map(|i| change(&format!("User {i}"), "", None))
            .collect();
        let got = resolve(
            &FakeApi {
                members,
                ..FakeApi::default()
            },
            "g/app",
            &changes,
        );
        assert_eq!(got.tags.len(), MAX_TAGS);
        assert_eq!(got.plain.len(), 5);
    }
}

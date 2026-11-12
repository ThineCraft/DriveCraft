//! Whether the signed-in person may open an item, judged from the permissions
//! recorded for it.
//!
//! The recorded permissions settle it only where they name the person, or a directory
//! group the server says they belong to. SharePoint's own groups (a site's Members,
//! Visitors and Owners), sharing links, and names known only to SharePoint (such as
//! "Everyone except external users") cannot be settled here: who is in them is kept
//! by SharePoint and not shown to DriveCraft. Then the server is asked instead, with
//! the person's own sign-in. Nothing is ever kept on a guess.

use crate::remote::{Identity, Permission, PrincipalKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// A permission names the person, or a group they are in.
    Allowed,
    /// Every permission names someone else, and none of them could include the person.
    Refused,
    /// Only the server can say.
    Ask,
}

/// Judge one item from its recorded permissions.
pub fn decide(perms: &[Permission], who: &Identity) -> Decision {
    let mut unsure = perms.is_empty();
    for p in perms {
        if p.roles.is_empty() {
            continue;
        }
        let id = p.principal_id.as_deref();
        match p.kind {
            PrincipalKind::User => {
                if id.is_some_and(|i| i.eq_ignore_ascii_case(&who.user_id)) && !who.user_id.is_empty() {
                    return Decision::Allowed;
                }
            }
            PrincipalKind::Group => match (&who.groups, id) {
                (Some(groups), Some(i)) => {
                    if groups.iter().any(|g| g.eq_ignore_ascii_case(i)) {
                        return Decision::Allowed;
                    }
                }
                _ => unsure = true,
            },
            // A program's access is not the person's.
            PrincipalKind::Application => {}
            // A link that adds nothing to what people already have.
            PrincipalKind::Link if p.link_scope.as_deref() == Some("existingAccess") => {}
            // A link for named people: the people are recorded as entries of their own.
            PrincipalKind::Link if p.link_scope.as_deref() == Some("users") => {}
            PrincipalKind::Link | PrincipalKind::SiteGroup | PrincipalKind::SiteUser | PrincipalKind::Unknown => {
                unsure = true
            }
        }
    }
    if unsure { Decision::Ask } else { Decision::Refused }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(kind: PrincipalKind, id: Option<&str>) -> Permission {
        Permission {
            perm_id: "1".into(),
            kind,
            principal_id: id.map(String::from),
            name: "x".into(),
            email: None,
            login: None,
            roles: vec!["read".into()],
            link_scope: None,
            via_link: false,
        }
    }

    fn alex(groups: Option<Vec<&str>>) -> Identity {
        Identity {
            user_id: "alex-id".into(),
            name: "Alex Morgan".into(),
            groups: groups.map(|g| g.into_iter().map(String::from).collect()),
        }
    }

    #[test]
    fn the_person_or_one_of_their_groups_settles_it() {
        let hr = vec![p(PrincipalKind::User, Some("dana-id")), p(PrincipalKind::Group, Some("hr-team"))];
        assert_eq!(decide(&hr, &alex(Some(vec!["sales"]))), Decision::Refused);
        assert_eq!(decide(&hr, &alex(Some(vec!["sales", "HR-TEAM"]))), Decision::Allowed);
        let named = vec![p(PrincipalKind::User, Some("ALEX-ID"))];
        assert_eq!(decide(&named, &alex(None)), Decision::Allowed);
    }

    #[test]
    fn what_cannot_be_settled_here_is_asked_of_the_server() {
        // Groups unknown: the group might include the person.
        let hr = vec![p(PrincipalKind::User, Some("dana-id")), p(PrincipalKind::Group, Some("hr-team"))];
        assert_eq!(decide(&hr, &alex(None)), Decision::Ask);
        // A SharePoint group, a link for the whole organization, or nothing recorded.
        for kind in [PrincipalKind::SiteGroup, PrincipalKind::SiteUser, PrincipalKind::Unknown] {
            assert_eq!(decide(&[p(kind, Some("5"))], &alex(Some(vec![]))), Decision::Ask, "{kind:?}");
        }
        let mut link = p(PrincipalKind::Link, None);
        link.link_scope = Some("organization".into());
        assert_eq!(decide(&[link.clone()], &alex(Some(vec![]))), Decision::Ask);
        link.link_scope = Some("existingAccess".into());
        assert_eq!(decide(&[link], &alex(Some(vec![]))), Decision::Refused);
        assert_eq!(decide(&[], &alex(Some(vec![]))), Decision::Ask);
        // A program's access is not the person's.
        assert_eq!(decide(&[p(PrincipalKind::Application, Some("app"))], &alex(Some(vec![]))), Decision::Refused);
    }
}

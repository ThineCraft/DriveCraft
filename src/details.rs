//! A file's library details: the values it has in the columns its library defines.
//!
//! SharePoint calls this metadata. It is normally reachable only in the SharePoint
//! web pages; DriveCraft shows it beside the file and lets the simple kinds be
//! changed. This module decides which columns are worth showing and turns what the
//! user types into what the server expects.

use crate::graph::{Column, ColumnKind};
use serde_json::Value;

/// Columns every library has that say nothing a person would look for here: they
/// are plumbing, or repeat what the file manager already shows.
const NOISE: &[&str] = &[
    "Attachments", "Edit", "DocIcon", "LinkFilename", "LinkFilenameNoMenu", "LinkTitle", "LinkTitleNoMenu",
    "FileSizeDisplay", "ItemChildCount", "FolderChildCount", "AppAuthor", "AppEditor", "ContentType",
    "SelectTitle", "SelectFilename", "FileLeafRef", "ParentVersionString", "ParentLeafName", "ID",
    "ComplianceAssetId", "SharedWithUsers", "SharedWithDetails", "CheckoutUser", "Modified", "Created",
    "Author", "Editor",
];

/// Columns named like plumbing that people do care about: the description, the
/// version, and the last check-in comment.
const KEPT: &[&str] = &["_ExtendedDescription", "_UIVersionString", "_CheckinComment"];

/// Whether a column belongs in the details shown to the user.
pub fn worth_showing(c: &Column) -> bool {
    if KEPT.contains(&c.name.as_str()) {
        return !c.hidden;
    }
    if c.hidden
        || c.name.starts_with('_')
        || c.name.starts_with("MediaService")
        || NOISE.iter().any(|n| n.eq_ignore_ascii_case(&c.name))
    {
        return false;
    }
    // What cannot be changed is shown only when DriveCraft can at least read it.
    !(c.read_only && c.kind == ColumnKind::Other)
}

/// Whether DriveCraft offers to change a column's value.
pub fn editable(c: &Column) -> bool {
    !c.read_only && c.kind != ColumnKind::Other
}

/// A value as plain words, for a list.
pub fn as_text(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(b)) => if *b { "Yes" } else { "No" }.to_string(),
        Some(Value::Array(a)) => a.iter().map(|x| as_text(Some(x))).collect::<Vec<_>>().join(", "),
        Some(other) => other.to_string(),
    }
}

/// What the user typed for a column, as the value the server wants. Empty clears it.
pub fn to_server(c: &Column, typed: &str) -> Result<Value, String> {
    if !editable(c) {
        return Err(format!("{} cannot be changed from DriveCraft", c.title));
    }
    let typed = typed.trim();
    if typed.is_empty() {
        if c.required {
            return Err(format!("{} must have a value", c.title));
        }
        return Ok(Value::Null);
    }
    match c.kind {
        ColumnKind::Text | ColumnKind::Note => Ok(Value::String(typed.to_string())),
        ColumnKind::Number => typed
            .replace(',', "")
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map(Value::Number)
            .ok_or_else(|| format!("{} needs a number", c.title)),
        ColumnKind::YesNo => match typed.to_lowercase().as_str() {
            "yes" | "true" | "1" | "y" => Ok(Value::Bool(true)),
            "no" | "false" | "0" | "n" => Ok(Value::Bool(false)),
            _ => Err(format!("{} needs yes or no", c.title)),
        },
        ColumnKind::Date => {
            // A day alone is taken as that day; anything longer must already be a full time.
            let full = if typed.len() == 10 { format!("{typed}T00:00:00Z") } else { typed.to_string() };
            time::OffsetDateTime::parse(&full, &time::format_description::well_known::Rfc3339)
                .map(|_| Value::String(full))
                .map_err(|_| format!("{} needs a date written like 2026-11-30", c.title))
        }
        ColumnKind::Choice => {
            let pick = |one: &str| {
                c.choices
                    .iter()
                    .find(|x| x.eq_ignore_ascii_case(one.trim()))
                    .cloned()
                    .ok_or_else(|| format!("{} must be one of: {}", c.title, c.choices.join(", ")))
            };
            if c.multi {
                let picked: Result<Vec<String>, String> = typed.split(';').filter(|p| !p.trim().is_empty()).map(pick).collect();
                Ok(Value::Array(picked?.into_iter().map(Value::String).collect()))
            } else {
                pick(typed).map(Value::String)
            }
        }
        ColumnKind::Other => unreachable!("not editable"),
    }
}

/// The changes to send: each value under its column's name. A column that takes
/// several choices also needs a note saying its value is a list.
pub fn changes(pairs: Vec<(&Column, Value)>) -> Value {
    let mut out = serde_json::Map::new();
    for (c, v) in pairs {
        if c.multi && v.is_array() {
            out.insert(format!("{}@odata.type", c.name), Value::String("Collection(Edm.String)".into()));
        }
        out.insert(c.name.clone(), v);
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, kind: ColumnKind) -> Column {
        Column {
            name: name.into(),
            title: name.into(),
            kind,
            read_only: false,
            required: false,
            hidden: false,
            choices: vec![],
            multi: false,
            date_only: false,
            description: String::new(),
        }
    }

    #[test]
    fn typed_values_become_what_the_server_expects() {
        assert_eq!(to_server(&col("Dept", ColumnKind::Text), " Finance ").unwrap(), Value::String("Finance".into()));
        assert_eq!(to_server(&col("Dept", ColumnKind::Text), "").unwrap(), Value::Null);
        assert_eq!(to_server(&col("Pages", ColumnKind::Number), "1,250").unwrap(), serde_json::json!(1250.0));
        assert!(to_server(&col("Pages", ColumnKind::Number), "many").is_err());
        assert_eq!(to_server(&col("Final", ColumnKind::YesNo), "Yes").unwrap(), Value::Bool(true));
        assert_eq!(to_server(&col("Due", ColumnKind::Date), "2026-11-30").unwrap(), Value::String("2026-11-30T00:00:00Z".into()));
        assert!(to_server(&col("Due", ColumnKind::Date), "next week").is_err());

        let mut status = col("Status", ColumnKind::Choice);
        status.choices = vec!["Draft".into(), "Approved".into()];
        assert_eq!(to_server(&status, "approved").unwrap(), Value::String("Approved".into()));
        assert!(to_server(&status, "Maybe").is_err());
        status.multi = true;
        let both = to_server(&status, "Draft; Approved").unwrap();
        assert_eq!(both, serde_json::json!(["Draft", "Approved"]));
        let sent = changes(vec![(&status, both)]);
        assert_eq!(sent["Status@odata.type"], "Collection(Edm.String)");

        let mut needed = col("Owner", ColumnKind::Text);
        needed.required = true;
        assert!(to_server(&needed, "  ").is_err(), "a required column cannot be cleared");
        let mut fixed = col("Version", ColumnKind::Text);
        fixed.read_only = true;
        assert!(to_server(&fixed, "2").is_err());
    }

    #[test]
    fn plumbing_columns_are_left_out() {
        assert!(worth_showing(&col("Title", ColumnKind::Text)));
        assert!(worth_showing(&col("Department", ColumnKind::Choice)));
        assert!(!worth_showing(&col("DocIcon", ColumnKind::Other)));
        assert!(!worth_showing(&col("_ComplianceTag", ColumnKind::Text)));
        assert!(worth_showing(&col("_ExtendedDescription", ColumnKind::Note)));
        assert!(!worth_showing(&col("MediaServiceImageTags", ColumnKind::Other)));
        let mut hidden = col("Secret", ColumnKind::Text);
        hidden.hidden = true;
        assert!(!worth_showing(&hidden));
        let mut person = col("Reviewer", ColumnKind::Other);
        assert!(worth_showing(&person), "shown, though not editable");
        assert!(!editable(&person));
        person.read_only = true;
        assert!(!worth_showing(&person));
    }
}

//! Folder-layout templates such as `{year}/{year}-{month}/{name}`. Same fields and the same plain-language
//! errors as the Python engine, so the UI's live preview behaves identically.

pub const LAYOUT_FIELDS: [&str; 6] = ["day", "ext", "month", "name", "stem", "year"];
pub const TRIP_FIELDS: [&str; 9] = ["day", "ext", "month", "name", "stem", "trip", "trip_dated", "trip_year", "year"];

/// Fill `{field}` placeholders. `Err` carries the unknown field (or a malformed-brace note).
pub fn format(template: &str, vars: &[(&str, String)]) -> Result<String, String> {
    let mut out = String::new();
    let chars: Vec<char> = template.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '{' if chars.get(i + 1) == Some(&'{') => {
                out.push('{');
                i += 2;
            }
            '}' if chars.get(i + 1) == Some(&'}') => {
                out.push('}');
                i += 2;
            }
            '{' => {
                let end = chars[i..].iter().position(|c| *c == '}').ok_or("Single '{' encountered in format string")?;
                let name: String = chars[i + 1..i + end].iter().collect();
                match vars.iter().find(|(k, _)| *k == name) {
                    Some((_, v)) => out.push_str(v),
                    None => return Err(format!("'{name}'")),
                }
                i += end + 1;
            }
            '}' => return Err("Single '}' encountered in format string".into()),
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    Ok(out)
}

fn field_list(fields: &[&str]) -> String {
    fields.iter().map(|f| format!("{{{f}}}")).collect::<Vec<_>>().join(", ")
}

fn sample(trip: bool) -> Vec<(&'static str, String)> {
    let mut v = vec![
        ("year", "2025".to_string()),
        ("month", "12".into()),
        ("day", "27".into()),
        ("name", "IMG_0001.HEIC".into()),
        ("stem", "IMG_0001".into()),
        ("ext", "HEIC".into()),
    ];
    if trip {
        v.push(("trip", "Udaipur".into()));
        v.push(("trip_dated", "Udaipur (Dec 2025)".into()));
        v.push(("trip_year", "2025".into()));
    }
    v
}

fn relative_problem(out: &str) -> bool {
    let norm = out.replace('\\', "/");
    let parts: Vec<&str> = norm.split('/').filter(|p| !p.is_empty()).collect();
    norm.starts_with('/') || out.starts_with('\\') || parts.contains(&"..") || parts.first().is_some_and(|p| p.contains(':'))
}

/// A readable problem with a layout pattern, or `None` if it is fine.
pub fn check_layout(template: &str) -> Option<String> {
    let out = match format(template, &sample(false)) {
        Ok(o) => o,
        Err(e) => return Some(format!("Unknown or malformed field {e}. Use {}.", field_list(&LAYOUT_FIELDS))),
    };
    if relative_problem(&out) {
        return Some("The layout must be a relative folder pattern (no leading slash, drive or '..').".into());
    }
    if !template.contains("{name}") && !template.contains("{stem}") {
        return Some("The layout must include {name} (or {stem} and {ext}) so files keep their names.".into());
    }
    None
}

pub fn check_trip_layout(template: &str) -> Option<String> {
    let out = match format(template, &sample(true)) {
        Ok(o) => o,
        Err(e) => return Some(format!("Unknown or malformed field {e}. Use {}.", field_list(&TRIP_FIELDS))),
    };
    if relative_problem(&out) {
        return Some("The layout must be a relative folder pattern (no leading slash, drive or '..').".into());
    }
    if !template.contains("{name}") && !template.contains("{stem}") {
        return Some("The layout must include {name} (or {stem} and {ext}) so files keep their names.".into());
    }
    if !template.contains("{trip}") && !template.contains("{trip_dated}") {
        return Some("The layout must include {trip_dated} or {trip} so each trip gets its own folder.".into());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_and_escapes() {
        let v = vec![("year", "2025".to_string())];
        assert_eq!(format("{year}/{{x}}", &v).unwrap(), "2025/{x}");
        assert_eq!(format("{nope}", &v).unwrap_err(), "'nope'");
    }

    #[test]
    fn same_messages_as_the_python_engine() {
        assert_eq!(
            check_layout("{year}/{oops}/{name}").unwrap(),
            "Unknown or malformed field 'oops'. Use {day}, {ext}, {month}, {name}, {stem}, {year}."
        );
        assert!(check_layout("../{name}").unwrap().contains("relative"));
        assert!(check_layout("/abs/{name}").unwrap().contains("relative"));
        assert!(check_layout("C:/x/{name}").unwrap().contains("relative"));
        assert!(check_layout("{year}/folder").unwrap().contains("must include {name}"));
        assert!(check_layout("{year}/{year}-{month}/{name}").is_none());
        assert!(check_trip_layout("Trips/{name}").unwrap().contains("{trip_dated}"));
        assert!(check_trip_layout("Trips/{trip_year}/{trip_dated}/{name}").is_none());
    }
}

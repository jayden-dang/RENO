//! The new-thread state's words: the heading and the chip strip above the composer.

/// The heading, in the parts the screen draws: the project is underlined.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Heading {
    pub(crate) before: String,
    pub(crate) project: Option<String>,
    pub(crate) after: String,
}

impl Heading {
    #[cfg(test)]
    pub(crate) fn text(&self) -> String {
        format!(
            "{}{}{}",
            self.before,
            self.project.as_deref().unwrap_or_default(),
            self.after
        )
    }
}

pub(crate) fn heading(project: Option<&str>) -> Heading {
    match project {
        Some(name) => Heading {
            before: "What should we build in ".to_owned(),
            project: Some(name.to_owned()),
            after: "?".to_owned(),
        },
        None => Heading {
            before: "What should we build?".to_owned(),
            project: None,
            after: String::new(),
        },
    }
}

/// One chip above the composer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Chip {
    pub(crate) id: &'static str,
    pub(crate) label: String,
}

/// "This Mac" · the project (when there is one) · "…".
pub(crate) fn chips(project: Option<&str>) -> Vec<Chip> {
    let chip = |id, label: &str| Chip {
        id,
        label: label.to_owned(),
    };
    let mut chips = vec![chip("this-mac", "This Mac")];
    chips.extend(project.map(|name| chip("project", name)));
    chips.push(chip("more", "\u{2026}"));
    chips
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_heading_names_the_project_or_asks_plainly() {
        let with = heading(Some("atlas-api"));
        assert_eq!(with.text(), "What should we build in atlas-api?");
        assert_eq!(with.project.as_deref(), Some("atlas-api"));
        let without = heading(None);
        assert_eq!(without.text(), "What should we build?");
        assert_eq!(without.project, None);
    }

    #[test]
    fn the_chip_strip_is_this_mac_the_project_and_more() {
        let labels = |p| chips(p).into_iter().map(|c| c.label).collect::<Vec<_>>();
        assert_eq!(
            labels(Some("atlas-api")),
            ["This Mac", "atlas-api", "\u{2026}"]
        );
        assert_eq!(labels(None), ["This Mac", "\u{2026}"]);
    }
}

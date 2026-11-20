//! Where a new task runs, in words: the heading sentence ("What should we build in reno on main
//! in a new worktree?"), what each checkout is called, and the branch list's foot. Pure; the
//! screen draws it (`screens/home.rs`) and the composer's pickers change it.

use reno_client::places::Checkout;

/// "New worktree", "This checkout", or the worktree's own name.
pub(crate) fn checkout_label(checkout: &Checkout) -> String {
    match checkout {
        Checkout::NewWorktree => "New worktree".to_owned(),
        Checkout::ThisCheckout => "This checkout".to_owned(),
        Checkout::Worktree(name) => name.clone(),
    }
}

/// "in a new worktree", "in this checkout", "in the worktree reno-model-chip".
pub(crate) fn checkout_phrase(checkout: &Checkout) -> String {
    match checkout {
        Checkout::NewWorktree => "in a new worktree".to_owned(),
        Checkout::ThisCheckout => "in this checkout".to_owned(),
        Checkout::Worktree(name) => format!("in the worktree {name}"),
    }
}

/// The branch list's foot: "Showing 20 of 34", or the count when all are shown.
pub(crate) fn branches_foot(shown: usize, total: usize) -> String {
    if shown >= total {
        match total {
            1 => "1 branch".to_owned(),
            n => format!("{n} branches"),
        }
    } else {
        format!("Showing {shown} of {total}")
    }
}

/// The new-thread heading as the parts the screen draws: the underlined words open the
/// pickers. "What should we build in [reno] on [main] in [a new worktree]?"
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Sentence {
    pub(crate) project: String,
    pub(crate) branch: String,
    /// "a new worktree", "this checkout", "the worktree reno-model-chip".
    pub(crate) checkout: String,
}

impl Sentence {
    #[cfg(test)]
    pub(crate) fn text(&self) -> String {
        format!(
            "What should we build in {} on {} in {}?",
            self.project, self.branch, self.checkout
        )
    }
}

pub(crate) fn sentence(project: &str, branch: &str, checkout: &Checkout) -> Sentence {
    Sentence {
        project: project.to_owned(),
        branch: branch.to_owned(),
        checkout: checkout_noun(checkout),
    }
}

/// "a new worktree", "this checkout", "the worktree reno-model-chip".
fn checkout_noun(checkout: &Checkout) -> String {
    match checkout {
        Checkout::NewWorktree => "a new worktree".to_owned(),
        Checkout::ThisCheckout => "this checkout".to_owned(),
        Checkout::Worktree(name) => format!("the worktree {name}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sentence_names_the_project_the_branch_and_the_checkout() {
        let s = sentence("reno", "main", &Checkout::NewWorktree);
        assert_eq!(
            s.text(),
            "What should we build in reno on main in a new worktree?"
        );
        assert_eq!(
            sentence(
                "reno",
                "fix/model-chip",
                &Checkout::Worktree("reno-model-chip".into())
            )
            .text(),
            "What should we build in reno on fix/model-chip in the worktree reno-model-chip?"
        );
        assert_eq!(
            sentence("atlas-api", "main", &Checkout::ThisCheckout).text(),
            "What should we build in atlas-api on main in this checkout?"
        );
    }

    #[test]
    fn the_branch_foot_counts_what_is_shown_and_what_there_is() {
        assert_eq!(branches_foot(20, 34), "Showing 20 of 34");
        assert_eq!(branches_foot(7, 7), "7 branches");
        assert_eq!(branches_foot(1, 1), "1 branch");
    }
}

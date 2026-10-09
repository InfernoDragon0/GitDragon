//! Settings structure and the "Find a setting" filter: the sections in page order, the rows
//! each one holds (label + search keywords), and which of them a query leaves visible.
//! Pure logic; the page in `settings.rs` / `settings_cards.rs` asks it what to draw.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Section {
    Appearance,
    Accounts,
    Git,
    Editor,
    Diff,
    PullPush,
    Refresh,
    Ai,
    Shortcuts,
    Privacy,
}

impl Section {
    /// Page order.
    pub const ALL: [Section; 10] = [
        Section::Appearance,
        Section::Accounts,
        Section::Git,
        Section::Editor,
        Section::Diff,
        Section::PullPush,
        Section::Refresh,
        Section::Ai,
        Section::Shortcuts,
        Section::Privacy,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Section::Appearance => "Appearance",
            Section::Accounts => "Accounts",
            Section::Git => "Git",
            Section::Editor => "External editor",
            Section::Diff => "Diff & graph",
            Section::PullPush => "Pull & push",
            Section::Refresh => "Refresh",
            Section::Ai => "AI & MCP",
            Section::Shortcuts => "Shortcuts",
            Section::Privacy => "Privacy",
        }
    }

    /// Extra words the section title answers to.
    fn keywords(self) -> &'static str {
        match self {
            Section::Appearance => "look theme colour color dark light zoom size font",
            Section::Accounts => "sign in login github gitlab azure token host",
            Section::Git => "git program binary version",
            Section::Editor => "vs code vscode zed merge conflicts editor",
            Section::Diff => "diff context whitespace hunk graph",
            Section::PullPush => "pull push fetch merge rebase prune force lease",
            Section::Refresh => "poll interval timer background fetch auto sync",
            Section::Ai => "claude agent chat assistant api key model adapter mcp",
            Section::Shortcuts => "keyboard keys bindings hotkeys",
            Section::Privacy => "telemetry analytics crash reports tracking gravatar data",
        }
    }
}

/// A searchable setting.
pub struct Row {
    pub section: Section,
    pub label: &'static str,
    pub keywords: &'static str,
}

const fn row(section: Section, label: &'static str, keywords: &'static str) -> Row {
    Row {
        section,
        label,
        keywords,
    }
}

/// Every row of the page except the dynamic lists (accounts, AI fields, shortcuts), whose
/// sections answer to their title keywords and, for shortcuts, to each action's name.
pub const ROWS: &[Row] = &[
    row(Section::Appearance, "Theme", "aurora onyx skin colour"),
    row(Section::Appearance, "Mode", "dark light night"),
    row(Section::Appearance, "Zoom", "scale size bigger smaller"),
    row(Section::Git, "Git program", "path version detected binary"),
    row(
        Section::Git,
        "Default branch",
        "init main master new repository",
    ),
    row(
        Section::Git,
        "Show git commands",
        "operation strip status bar command text",
    ),
    row(Section::Git, "Commit signing", "gpg ssh sign key"),
    row(
        Section::Editor,
        "Detected editors",
        "vs code visual studio zed",
    ),
    row(
        Section::Editor,
        "Custom editor",
        "path override program executable",
    ),
    row(Section::Diff, "Context lines", "hunk surrounding unified"),
    row(
        Section::Diff,
        "Ignore whitespace",
        "spaces tabs indentation -w",
    ),
    row(
        Section::PullPush,
        "Pull strategy",
        "merge rebase fast-forward ff-only toolbar",
    ),
    row(
        Section::PullPush,
        "Prune on fetch",
        "remove deleted branches",
    ),
    row(
        Section::PullPush,
        "Force push",
        "force-with-lease lease overwrite",
    ),
    row(
        Section::Refresh,
        "Host refresh",
        "pull requests issues merge requests checks interval",
    ),
    row(
        Section::Refresh,
        "Background fetch",
        "auto fetch interval remote",
    ),
    row(
        Section::Privacy,
        "Nothing is sent",
        "telemetry analytics crash upload tracking",
    ),
    row(
        Section::Privacy,
        "Gravatar",
        "avatars pictures email hash third party",
    ),
];

/// A parsed "Find a setting" query: every word must appear (any case) in a row's label or
/// keywords, or in its section's title or keywords.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filter {
    terms: Vec<String>,
}

impl Filter {
    pub fn new(query: &str) -> Self {
        Self {
            terms: query.split_whitespace().map(str::to_lowercase).collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    fn all_in(&self, parts: &[&str]) -> bool {
        let hay: Vec<String> = parts.iter().map(|p| p.to_lowercase()).collect();
        self.terms.iter().all(|t| hay.iter().any(|h| h.contains(t)))
    }

    fn section_matches(&self, section: Section) -> bool {
        self.all_in(&[section.title(), section.keywords()])
    }

    /// Whether `label` (a row of `section`) stays: no query, a query the section title answers
    /// (the whole card shows), or one the row's own label / keywords answer.
    pub fn row_visible(&self, section: Section, label: &str) -> bool {
        if self.is_empty() || self.section_matches(section) {
            return true;
        }
        let keywords = ROWS
            .iter()
            .find(|r| r.section == section && r.label == label)
            .map_or("", |r| r.keywords);
        self.all_in(&[label, keywords])
    }

    /// Whether the card shows at all. `extra_labels` are the labels of rows the table does not
    /// know (the shortcut list).
    pub fn section_visible(&self, section: Section, extra_labels: &[&str]) -> bool {
        self.is_empty()
            || self.section_matches(section)
            || ROWS
                .iter()
                .filter(|r| r.section == section)
                .any(|r| self.all_in(&[r.label, r.keywords]))
            || extra_labels.iter().any(|label| self.all_in(&[label]))
    }

    /// The sections left after the query, in page order.
    pub fn visible_sections(&self, extra: impl Fn(Section) -> Vec<&'static str>) -> Vec<Section> {
        Section::ALL
            .into_iter()
            .filter(|s| self.section_visible(*s, &extra(*s)))
            .collect()
    }
}

/// The section the scroll position is in: `top` is the index (in `visible`) of the card at the
/// top of the view; at the very bottom the last card wins (a short last card never reaches the
/// top).
pub fn active_section(visible: &[Section], top: usize, at_bottom: bool) -> Option<Section> {
    if at_bottom {
        return visible.last().copied();
    }
    visible
        .get(top.min(visible.len().saturating_sub(1)))
        .copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn visible(query: &str) -> Vec<Section> {
        Filter::new(query).visible_sections(|_| Vec::new())
    }

    #[test]
    fn empty_query_shows_everything() {
        assert_eq!(visible(""), Section::ALL.to_vec());
        assert_eq!(visible("   "), Section::ALL.to_vec());
        assert!(Filter::new("  ").is_empty());
    }

    #[test]
    fn rows_match_label_keywords_and_any_case() {
        assert_eq!(visible("whitespace"), vec![Section::Diff]);
        assert_eq!(visible("WHITESPACE"), vec![Section::Diff]);
        // Keywords the label does not contain.
        assert_eq!(visible("gpg"), vec![Section::Git]);
        assert!(visible("prune").contains(&Section::PullPush));
        assert_eq!(visible("nothing matches this"), Vec::<Section>::new());
    }

    #[test]
    fn every_word_must_match() {
        assert_eq!(visible("background fetch"), vec![Section::Refresh]);
        // "fetch" alone also hits Pull & push (prune on fetch) and the section keywords.
        assert!(visible("fetch").len() >= 2);
        assert!(visible("zoom telemetry").is_empty());
    }

    #[test]
    fn a_section_title_shows_the_whole_card() {
        let filter = Filter::new("git");
        assert!(filter.section_visible(Section::Git, &[]));
        for row in ROWS.iter().filter(|r| r.section == Section::Git) {
            assert!(filter.row_visible(Section::Git, row.label), "{}", row.label);
        }
        // But a row-level query hides the other rows of the card.
        let filter = Filter::new("signing");
        assert!(filter.row_visible(Section::Git, "Commit signing"));
        assert!(!filter.row_visible(Section::Git, "Default branch"));
        assert!(!filter.row_visible(Section::Git, "Git program"));
    }

    #[test]
    fn shortcut_labels_are_searchable() {
        let filter = Filter::new("settings");
        assert!(filter.section_visible(Section::Shortcuts, &["Open settings"]));
        let filter = Filter::new("close tab");
        assert!(!filter.section_visible(Section::Shortcuts, &[]));
        assert!(filter.section_visible(Section::Shortcuts, &["Close tab"]));
        assert!(filter.row_visible(Section::Shortcuts, "Close tab"));
        assert!(!filter.row_visible(Section::Shortcuts, "Next tab"));
    }

    #[test]
    fn every_row_belongs_to_a_known_section_and_is_findable() {
        for row in ROWS {
            assert!(Section::ALL.contains(&row.section));
            assert!(
                visible(row.label).contains(&row.section),
                "{} not found",
                row.label
            );
        }
    }

    #[test]
    fn scroll_spy_picks_the_top_card_or_the_last_at_the_bottom() {
        let sections = [Section::Appearance, Section::Git, Section::Privacy];
        assert_eq!(
            active_section(&sections, 0, false),
            Some(Section::Appearance)
        );
        assert_eq!(active_section(&sections, 1, false), Some(Section::Git));
        assert_eq!(active_section(&sections, 9, false), Some(Section::Privacy));
        assert_eq!(active_section(&sections, 0, true), Some(Section::Privacy));
        assert_eq!(active_section(&[], 0, false), None);
    }
}

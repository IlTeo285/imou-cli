use crate::types::Category;

/// Whether a category should be treated as a real, actionable event.
/// `Empty` (nothing of interest in frame — the usual false-positive case:
/// wind, shadows, lighting changes) and `Unknown` (the model's output
/// couldn't be classified at all) are the only non-relevant categories —
/// fail toward "relevant" for anything the model actually recognized, since
/// a missed real event is worse than an unnecessary upload/notification.
pub fn is_relevant(category: Category) -> bool {
    !matches!(category, Category::Empty | Category::Unknown)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognized_categories_are_relevant() {
        assert!(is_relevant(Category::Human));
        assert!(is_relevant(Category::Vehicle));
        assert!(is_relevant(Category::Animal));
        assert!(is_relevant(Category::Package));
    }

    #[test]
    fn empty_and_unknown_are_not_relevant() {
        assert!(!is_relevant(Category::Empty));
        assert!(!is_relevant(Category::Unknown));
    }
}

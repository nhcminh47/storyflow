pub fn slugify(title: &str) -> String {
    let mut slug = String::new();
    let mut separator_pending = false;

    for character in title.chars() {
        if character.is_ascii_alphanumeric() {
            if separator_pending {
                slug.push('-');
            }
            slug.push(character.to_ascii_lowercase());
            separator_pending = false;
        } else if !slug.is_empty() {
            separator_pending = true;
        }
    }

    slug
}

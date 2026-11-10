pub fn slugify(title: &str) -> String {
    title.to_ascii_lowercase().replace(' ', "-")
}

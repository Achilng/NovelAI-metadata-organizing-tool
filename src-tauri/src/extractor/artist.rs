use std::collections::HashSet;

pub fn extract_artist_tags(positive_prompt: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut artists = Vec::new();

    for part in positive_prompt.split([',', '\n', '\r']) {
        let trimmed = part.trim();
        if trimmed.is_empty() || !trimmed.to_lowercase().contains("artist") {
            continue;
        }

        if seen.insert(trimmed.to_string()) {
            artists.push(trimmed.to_string());
        }
    }

    artists
}

#[cfg(test)]
mod tests {
    use super::extract_artist_tags;

    #[test]
    fn keeps_artist_fragments_and_deduplicates_exact_matches() {
        let tags = extract_artist_tags(
            "best quality, artist:maidcode1023, girl\n0.5::artist:xxx::, artist:maidcode1023, -3::artist collaboration::",
        );

        assert_eq!(
            tags,
            vec![
                "artist:maidcode1023",
                "0.5::artist:xxx::",
                "-3::artist collaboration::"
            ]
        );
    }

    #[test]
    fn ignores_non_artist_fragments() {
        let tags = extract_artist_tags("best quality, character:name, background");

        assert!(tags.is_empty());
    }
}

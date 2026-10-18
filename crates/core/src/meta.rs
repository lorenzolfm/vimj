//! Song names from video pages and file names: "Artist - Title (Official
//! Video)" becomes artist "Artist", title "Title".

/// What a download or a file offers to name a song with.
#[derive(Clone, Copy, Debug, Default)]
pub struct Raw<'a> {
    pub title: &'a str,
    /// Music metadata, when the page has it (YouTube Music does).
    pub track: Option<&'a str>,
    pub artist: Option<&'a str>,
    /// Channel name.
    pub uploader: Option<&'a str>,
}

/// Returns `(artist, title)`.
pub fn song_name(raw: Raw) -> (String, String) {
    if let (Some(track), Some(artist)) = (nonempty(raw.track), nonempty(raw.artist)) {
        return (tidy(artist), tidy(&strip_noise(track)));
    }
    let title = strip_noise(raw.title);
    for separator in [" - ", " – ", " — ", " | "] {
        if let Some((artist, rest)) = title.split_once(separator)
            && !artist.trim().is_empty()
            && !rest.trim().is_empty()
        {
            return (tidy(artist), tidy(rest));
        }
    }
    let artist = raw.uploader.map(channel_artist).unwrap_or_default();
    (artist, tidy(&title))
}

/// Name from a file name like `Artist - Title [videoid].opus`.
pub fn from_file_name(name: &str) -> (String, String) {
    let stem = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
    let stem = match stem.rsplit_once(" [") {
        Some((head, tail)) if tail.ends_with(']') && !tail.contains(' ') => head,
        _ => stem,
    };
    song_name(Raw { title: stem, ..Raw::default() })
}

fn nonempty(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|s| !s.is_empty())
}

/// Words that mark a bracketed part as video packaging rather than part of the name.
const NOISE: [&str; 14] = [
    "official", "video", "audio", "lyric", "visuali", "hd", "hq", "4k", "mv", "m/v", "clip", "explicit", "full song",
    "out now",
];

fn strip_noise(title: &str) -> String {
    let mut out = String::new();
    let mut rest = title;
    while let Some(open) = rest.find(['(', '[']) {
        let close_char = if rest[open..].starts_with('(') { ')' } else { ']' };
        let Some(len) = rest[open..].find(close_char) else { break };
        let inner = rest[open + 1..open + len].to_lowercase();
        out.push_str(&rest[..open]);
        let noisy = NOISE.iter().any(|w| inner.split(|c: char| !c.is_alphanumeric() && c != '/').any(|word| word.starts_with(w)))
            || NOISE.iter().any(|w| w.contains(' ') && inner.contains(w));
        if !noisy {
            out.push_str(&rest[open..=open + len]);
        }
        rest = &rest[open + len + 1..];
    }
    out.push_str(rest);
    out
}

fn channel_artist(uploader: &str) -> String {
    let name = uploader.trim();
    let name = name.strip_suffix(" - Topic").unwrap_or(name);
    let name = name.strip_suffix("VEVO").unwrap_or(name);
    let name = name.strip_suffix(" Official").unwrap_or(name);
    tidy(name)
}

fn tidy(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn title(t: &str) -> (String, String) {
        song_name(Raw { title: t, ..Raw::default() })
    }

    fn pair(a: &str, t: &str) -> (String, String) {
        (a.to_owned(), t.to_owned())
    }

    #[test]
    fn prefers_music_metadata() {
        let raw = Raw { title: "whatever", track: Some("Strobe"), artist: Some("deadmau5"), uploader: Some("x") };
        assert_eq!(song_name(raw), pair("deadmau5", "Strobe"));
    }

    #[test]
    fn splits_and_strips_video_packaging() {
        assert_eq!(title("Daft Punk - One More Time (Official Video)"), pair("Daft Punk", "One More Time"));
        assert_eq!(title("Fred again.. – Delilah (pull me out of this) [Official Audio]"), pair("Fred again..", "Delilah (pull me out of this)"));
        assert_eq!(title("Artist - Song (Extended Mix) [HD]"), pair("Artist", "Song (Extended Mix)"));
        assert_eq!(title("Artist - Song (feat. Other) (Lyrics)"), pair("Artist", "Song (feat. Other)"));
        assert_eq!(title("Artist - Song (Visualizer)"), pair("Artist", "Song"));
    }

    #[test]
    fn falls_back_to_the_channel() {
        let raw = Raw { title: "Midnight City", uploader: Some("M83 - Topic"), ..Raw::default() };
        assert_eq!(song_name(raw), pair("M83", "Midnight City"));
        let raw = Raw { title: "Song", uploader: Some("SomeoneVEVO"), ..Raw::default() };
        assert_eq!(song_name(raw), pair("Someone", "Song"));
    }

    #[test]
    fn reads_file_names() {
        assert_eq!(from_file_name("Artist - Title [dQw4w9WgXcQ].opus"), pair("Artist", "Title"));
        assert_eq!(from_file_name("Just a title.mp3"), pair("", "Just a title"));
        assert_eq!(from_file_name("A - B [Remix Edit].flac"), pair("A", "B [Remix Edit]"));
    }
}

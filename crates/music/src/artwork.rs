use anyhow::{Context as _, Result};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::Track;

const ALBUMS: &str = "https://api.deezer.com/search/album";
const TRACKS: &str = "https://api.deezer.com/search/track";
const RESULTS: &str = "10";

/// The same YouTube picture at `edge` pixels or more on its longer side, as Google's servers
/// scale it. Album art and avatars on Google's image servers take their size in the url, as
/// `=w<width>-h<height>` or `=s<side>`; a video still on `i.ytimg.com` is asked for as its
/// 1280-wide still, without the query that crops it smaller. Any other url, or one already that
/// large, comes back unchanged.
pub fn larger(url: &str, edge: u32) -> String {
    if let Some(still) = video_still(url) {
        return still;
    }
    let Some((base, options)) = url.rsplit_once('=') else {
        return url.to_owned();
    };
    let google = [".googleusercontent.com/", ".ggpht.com/"]
        .iter()
        .any(|host| base.contains(host));
    if !google {
        return url.to_owned();
    }
    let size = |prefix: char| {
        options
            .split('-')
            .find_map(|option| option.strip_prefix(prefix)?.parse::<u32>().ok())
    };
    let numeric = |option: &str, prefix: char| {
        option
            .strip_prefix(prefix)
            .is_some_and(|value| value.parse::<u32>().is_ok())
    };
    if let Some(side) = size('s') {
        if side >= edge {
            return url.to_owned();
        }
        let options = options
            .split('-')
            .map(|option| match numeric(option, 's') {
                true => format!("s{edge}"),
                false => option.to_owned(),
            })
            .collect::<Vec<_>>()
            .join("-");
        return format!("{base}={options}");
    }
    let (Some(width), Some(height)) = (size('w'), size('h')) else {
        return url.to_owned();
    };
    let longer = width.max(height);
    if longer == 0 || longer >= edge {
        return url.to_owned();
    }
    let scale = |side: u32| u64::from(side) * u64::from(edge) / u64::from(longer);
    let options = options
        .split('-')
        .map(|option| {
            if numeric(option, 'w') {
                format!("w{}", scale(width))
            } else if numeric(option, 'h') {
                format!("h{}", scale(height))
            } else {
                option.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("-");
    format!("{base}={options}")
}

/// The same picture on Google's image servers cut by the server to `width` by `height`, which
/// keeps the subject in frame better than cropping a wide banner on our side, since a banner
/// is already a strip cut from a taller photo. Any other url comes back unchanged.
pub fn framed(url: &str, width: u32, height: u32) -> String {
    let Some((base, _)) = url.rsplit_once('=') else {
        return url.to_owned();
    };
    let google = [".googleusercontent.com/", ".ggpht.com/"]
        .iter()
        .any(|host| base.contains(host));
    match google {
        true => format!("{base}=w{width}-h{height}-p-l90-rj"),
        false => url.to_owned(),
    }
}

/// The 1280-wide still of a video on `i.ytimg.com`, from any of the smaller ones YouTube Music
/// hands out. The smaller ones are 4:3 with black bars, and their query crops them further.
fn video_still(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://i.ytimg.com/vi/")?;
    let (id, file) = rest.split_once('/')?;
    let file = file.split('?').next()?;
    let small = [
        "default.jpg",
        "mqdefault.jpg",
        "hqdefault.jpg",
        "sddefault.jpg",
        "hq720.jpg",
    ];
    small
        .contains(&file)
        .then(|| format!("https://i.ytimg.com/vi/{id}/hq720.jpg"))
}

/// Finds cover art by name in Deezer's public catalogue, which needs no account and serves every
/// image from a public https url. It is for a track whose own art cannot be handed to someone
/// else, so a miss is `None` rather than an error.
#[derive(Clone, Default)]
pub struct ArtworkSearch {
    http: reqwest::Client,
}

/// What a lookup searches for. A track with an album is looked up by the album, so every track
/// of one album shares a query and a result. A track without one is looked up by its title.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ArtworkQuery {
    artist: String,
    album: String,
    title: String,
}

#[derive(Deserialize)]
struct Page<T> {
    #[serde(default = "Vec::new")]
    data: Vec<T>,
    error: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct FoundAlbum {
    title: String,
    artist: Named,
    cover_xl: Option<String>,
}

#[derive(Deserialize)]
struct FoundTrack {
    title: String,
    artist: Named,
    album: Covered,
}

#[derive(Deserialize)]
struct Named {
    name: String,
}

#[derive(Deserialize)]
struct Covered {
    cover_xl: Option<String>,
}

impl ArtworkQuery {
    /// The query for `track`, or `None` when it has no artist or nothing else to search by.
    pub fn for_track(track: &Track) -> Option<Self> {
        let artist = track
            .artist_refs
            .first()
            .map(|artist| artist.name.as_str())
            .or_else(|| track.artists.split(", ").next())
            .map(searchable)
            .unwrap_or_default();
        let album = searchable(&track.album);
        let title = match album.is_empty() {
            true => searchable(&track.name),
            false => String::new(),
        };
        if artist.is_empty() || album.is_empty() && title.is_empty() {
            return None;
        }
        Some(Self {
            artist,
            album,
            title,
        })
    }
}

impl ArtworkSearch {
    pub fn new() -> Self {
        Self::default()
    }

    /// The url of a cover for `query`, or `None` when no result names the same artist and a
    /// close enough album or title to trust. A wrong cover is worse than none.
    pub async fn find(&self, query: &ArtworkQuery) -> Result<Option<String>> {
        match query.album.is_empty() {
            false => {
                let found: Vec<FoundAlbum> = self.search(ALBUMS, &query.album, query).await?;
                let candidates = found
                    .into_iter()
                    .filter(|album| same_artist(&album.artist.name, &query.artist))
                    .map(|album| (album.title, album.cover_xl));
                Ok(closest(candidates, &query.album))
            }
            true => {
                let found: Vec<FoundTrack> = self.search(TRACKS, &query.title, query).await?;
                let candidates = found
                    .into_iter()
                    .filter(|track| same_artist(&track.artist.name, &query.artist))
                    .map(|track| (track.title, track.album.cover_xl));
                Ok(closest(candidates, &query.title))
            }
        }
    }

    /// One page of results for the artist and `name`. Deezer reports a refused search, such as
    /// one over its rate limit, in the body of a successful response.
    async fn search<T: DeserializeOwned>(
        &self,
        endpoint: &str,
        name: &str,
        query: &ArtworkQuery,
    ) -> Result<Vec<T>> {
        let terms = format!("{} {name}", query.artist);
        let response = self
            .http
            .get(endpoint)
            .query(&[("q", terms.as_str()), ("limit", RESULTS)])
            .send()
            .await
            .context("cannot reach deezer")?;
        let status = response.status();
        if !status.is_success() {
            anyhow::bail!("deezer answered with status {status}");
        }
        let page: Page<T> = response
            .json()
            .await
            .context("cannot read the deezer search")?;
        if let Some(error) = page.error {
            anyhow::bail!("deezer refused the search: {error}");
        }
        Ok(page.data)
    }
}

/// The cover of the candidate whose name matches `wanted` exactly, or failing that the first one
/// that only adds words to it, as an edition or a feature credit does.
fn closest(
    candidates: impl Iterator<Item = (String, Option<String>)>,
    wanted: &str,
) -> Option<String> {
    let wanted = compared(wanted);
    let mut near = None;
    for (name, cover) in candidates {
        let Some(cover) = cover.filter(|cover| cover.starts_with("https://")) else {
            continue;
        };
        let name = compared(&searchable(&name));
        if name == wanted {
            return Some(cover);
        }
        if near.is_none() && extends(&name, &wanted) {
            near = Some(cover);
        }
    }
    near
}

fn same_artist(found: &str, wanted: &str) -> bool {
    let found = compared(found);
    let wanted = compared(wanted);
    found == wanted || extends(&found, &wanted)
}

/// Whether one of two compared names is the other with whole words added after it.
fn extends(one: &str, other: &str) -> bool {
    let starts = |long: &str, short: &str| {
        !short.is_empty()
            && long
                .strip_prefix(short)
                .is_some_and(|rest| rest.starts_with(' '))
    };
    starts(one, other) || starts(other, one)
}

/// `text` without anything in brackets, since Deezer wants every word of a search to match and
/// "(Official Video)" or "[Remastered]" would rule out the release itself.
fn searchable(text: &str) -> String {
    let mut kept = String::with_capacity(text.len());
    let mut depth = 0usize;
    for character in text.chars() {
        match character {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            _ if depth == 0 => kept.push(character),
            _ => {}
        }
    }
    kept.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `text` lowercased, with every run of punctuation and spaces turned into one space.
fn compared(text: &str) -> String {
    text.split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::larger;

    #[test]
    fn youtube_pictures_come_back_larger() {
        assert_eq!(
            larger("https://yt3.ggpht.com/abc=s88-c-k-c0x00ffffff-no-rj", 1200),
            "https://yt3.ggpht.com/abc=s1200-c-k-c0x00ffffff-no-rj"
        );
        assert_eq!(
            larger("https://lh3.googleusercontent.com/x=w60-h60-l90-rj", 1200),
            "https://lh3.googleusercontent.com/x=w1200-h1200-l90-rj"
        );
        assert_eq!(
            larger(
                "https://i.ytimg.com/vi/abc/hqdefault.jpg?sqp=xyz&rs=1",
                1200
            ),
            "https://i.ytimg.com/vi/abc/hq720.jpg"
        );
    }
}

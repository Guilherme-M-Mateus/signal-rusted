//! GIFs: search through Giphy (Signal's provider) or Tenor, and animated playback of GIF files.

use std::path::{Path, PathBuf};
use std::time::Duration;

use iced::widget::image::Handle;
use serde::{Deserialize, Serialize};

/// Results per request, like Signal's first page.
const LIMIT: u32 = 30;

/// Decoded frames beyond this many bytes stay a still image (a long, large GIF is hundreds of MB raw).
const MAX_FRAME_BYTES: usize = 64 << 20;

#[derive(Clone, Copy, Default, Serialize, Deserialize, Debug, PartialEq)]
pub enum Provider {
    #[default]
    Giphy,
    Tenor,
}

/// One search result: a still preview for the picker and the GIF that gets sent.
#[derive(Clone, Debug, PartialEq)]
pub struct Gif {
    pub title: String,
    pub preview: String,
    pub url: String,
}

/// Trending (empty query) or search request URL.
pub fn request_url(provider: Provider, key: &str, query: &str) -> String {
    let q = query.trim();
    let limit = LIMIT.to_string();
    let (base, mut params) = match provider {
        Provider::Giphy => (
            if q.is_empty() { "https://api.giphy.com/v1/gifs/trending" } else { "https://api.giphy.com/v1/gifs/search" },
            vec![("api_key", key), ("limit", &limit), ("rating", "pg-13")],
        ),
        Provider::Tenor => (
            if q.is_empty() { "https://tenor.googleapis.com/v2/featured" } else { "https://tenor.googleapis.com/v2/search" },
            vec![("key", key), ("client_key", "signal-rusted"), ("limit", &limit), ("media_filter", "gif,tinygif"), ("contentfilter", "medium")],
        ),
    };
    if !q.is_empty() {
        params.push(("q", q));
    }
    url::Url::parse_with_params(base, params).expect("static base URL").into()
}

/// The results in a provider's JSON response; entries without https media are skipped.
pub fn parse(provider: Provider, v: &serde_json::Value) -> Vec<Gif> {
    let s = |v: &serde_json::Value| v.as_str().map(str::to_owned);
    let items = match provider {
        Provider::Giphy => v["data"].as_array(),
        Provider::Tenor => v["results"].as_array(),
    };
    items
        .into_iter()
        .flatten()
        .filter_map(|g| {
            let (title, preview, url) = match provider {
                Provider::Giphy => {
                    let i = &g["images"];
                    (s(&g["title"]), s(&i["fixed_width_still"]["url"]), s(&i["downsized"]["url"]).or_else(|| s(&i["original"]["url"])))
                }
                Provider::Tenor => {
                    let m = &g["media_formats"];
                    (s(&g["content_description"]), s(&m["tinygif"]["url"]), s(&m["gif"]["url"]))
                }
            };
            let (preview, url) = (preview?, url?);
            (is_https(&preview) && is_https(&url)).then(|| Gif { title: title.unwrap_or_default(), preview, url })
        })
        .collect()
}

fn is_https(u: &str) -> bool {
    url::Url::parse(u).is_ok_and(|u| u.scheme() == "https")
}

async fn get(url: &str) -> Result<reqwest::Response, String> {
    let r = reqwest::get(url).await.map_err(|e| e.without_url().to_string())?;
    r.error_for_status().map_err(|e| e.without_url().to_string())
}

/// Searches (or lists trending) and downloads each result's preview.
pub async fn search(provider: Provider, key: String, query: String) -> Result<Vec<(Gif, Handle)>, String> {
    if key.trim().is_empty() {
        return Err(format!("Add a {provider:?} API key in Settings → Chats"));
    }
    let v: serde_json::Value = get(&request_url(provider, key.trim(), &query)).await?.json().await.map_err(|e| e.to_string())?;
    let previews = parse(provider, &v).into_iter().map(|g| async move {
        let bytes = get(&g.preview).await.ok()?.bytes().await.ok()?;
        Some((g, Handle::from_bytes(bytes)))
    });
    Ok(futures::future::join_all(previews).await.into_iter().flatten().collect())
}

/// Downloads a picked GIF into `dir` for sending as an attachment.
pub async fn download(url: String, dir: PathBuf) -> Result<PathBuf, String> {
    if !is_https(&url) {
        return Err("not an https URL".into());
    }
    let bytes = get(&url).await?.bytes().await.map_err(|e| e.to_string())?;
    if !bytes.starts_with(b"GIF8") {
        return Err("the provider didn't return a GIF".into());
    }
    tokio::fs::create_dir_all(&dir).await.map_err(|e| e.to_string())?;
    let path = dir.join(format!("gif-{}.gif", crate::backend::now_ms()));
    tokio::fs::write(&path, &bytes).await.map_err(|e| e.to_string())?;
    Ok(path)
}

/// A GIF's frames and how long each shows.
#[derive(Clone, Debug)]
pub struct Animation {
    frames: Vec<(Handle, Duration)>,
    total: Duration,
}

impl Animation {
    /// The frame showing `elapsed` into the (looping) animation.
    pub fn frame(&self, elapsed: Duration) -> &Handle {
        let mut t = Duration::from_nanos((elapsed.as_nanos() % self.total.as_nanos().max(1)) as u64);
        for (h, d) in &self.frames {
            if t < *d {
                return h;
            }
            t -= *d;
        }
        &self.frames[0].0
    }
}

/// Decodes an animated GIF; `None` for stills, non-GIFs and ones too big to hold decoded.
pub fn decode(path: &Path) -> Option<Animation> {
    use image::AnimationDecoder;
    let file = std::io::BufReader::new(std::fs::File::open(path).ok()?);
    let mut frames = Vec::new();
    let mut bytes = 0;
    for f in image::codecs::gif::GifDecoder::new(file).ok()?.into_frames() {
        let f = f.ok()?;
        // Browsers treat delays under 20ms as 100ms; so do we.
        let d = Duration::from(f.delay());
        let d = if d < Duration::from_millis(20) { Duration::from_millis(100) } else { d };
        let buf = f.into_buffer();
        bytes += buf.len();
        if bytes > MAX_FRAME_BYTES {
            return None;
        }
        frames.push((Handle::from_rgba(buf.width(), buf.height(), buf.into_raw()), d));
    }
    let total = frames.iter().map(|(_, d)| *d).sum();
    (frames.len() > 1).then_some(Animation { frames, total })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_urls() {
        let u = request_url(Provider::Giphy, "K", "");
        assert!(u.starts_with("https://api.giphy.com/v1/gifs/trending?api_key=K&limit=30"), "{u}");
        let u = request_url(Provider::Giphy, "K", " cats & dogs ");
        assert!(u.starts_with("https://api.giphy.com/v1/gifs/search?") && u.ends_with("&q=cats+%26+dogs"), "{u}");
        let u = request_url(Provider::Tenor, "K", "");
        assert!(u.starts_with("https://tenor.googleapis.com/v2/featured?key=K") && !u.contains("&q="), "{u}");
        assert!(request_url(Provider::Tenor, "K", "hi").ends_with("&q=hi"));
    }

    #[test]
    fn parses_giphy_and_tenor() {
        let giphy = serde_json::json!({"data": [
            {"title": "Cat", "images": {"fixed_width_still": {"url": "https://media0.giphy.com/s.gif"}, "downsized": {"url": "https://media0.giphy.com/d.gif"}}},
            {"title": "No media", "images": {}},
            {"title": "Plain http", "images": {"fixed_width_still": {"url": "http://x/s.gif"}, "original": {"url": "http://x/o.gif"}}},
        ]});
        assert_eq!(
            parse(Provider::Giphy, &giphy),
            [Gif { title: "Cat".into(), preview: "https://media0.giphy.com/s.gif".into(), url: "https://media0.giphy.com/d.gif".into() }]
        );
        let tenor = serde_json::json!({"results": [
            {"content_description": "Dog", "media_formats": {"tinygif": {"url": "https://media.tenor.com/t.gif"}, "gif": {"url": "https://media.tenor.com/g.gif"}}},
        ]});
        assert_eq!(parse(Provider::Tenor, &tenor)[0].url, "https://media.tenor.com/g.gif");
        assert!(parse(Provider::Tenor, &serde_json::json!({"error": "bad key"})).is_empty());
    }

    #[test]
    fn decodes_frames_and_loops() {
        use image::codecs::gif::{GifEncoder, Repeat};
        use image::{Delay, Frame, RgbaImage};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.gif");
        let mut enc = GifEncoder::new(std::fs::File::create(&path).unwrap());
        enc.set_repeat(Repeat::Infinite).unwrap();
        for (px, ms) in [([255, 0, 0, 255], 100), ([0, 0, 255, 255], 300)] {
            let img = RgbaImage::from_pixel(2, 2, image::Rgba(px));
            enc.encode_frame(Frame::from_parts(img, 0, 0, Delay::from_numer_denom_ms(ms, 1))).unwrap();
        }
        drop(enc);
        let a = decode(&path).expect("two frames");
        let ms = |n| Duration::from_millis(n);
        assert_eq!(a.frame(ms(50)), &a.frames[0].0);
        assert_eq!(a.frame(ms(150)), &a.frames[1].0);
        assert_eq!(a.frame(ms(450)), &a.frames[0].0, "loops after 400ms");
    }
}

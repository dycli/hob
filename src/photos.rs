//! The house's photos: one Immich album, listed and served at the size of
//! the house's screens. The Immich key stays here; a client gets only photo
//! ids and pictures.
//!
//! Immich's previews run to 1440 pixels on the short side, more than a
//! 1920×1080 screen can show in any orientation, and decoding the excess
//! stalls the frame's slideshow. Each is shrunk once to fit 1080×1920
//! either way round and kept on disk.

use std::io::Cursor;
use std::path::PathBuf;
use std::time::Duration;

use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, ImageDecoder, ImageEncoder, ImageFormat, ImageReader};
use serde::Deserialize;
use serde_json::json;

use crate::core::{Cached, Fault, Health};

/// The screen's sides; a photo shown either way round never needs more.
const SHORT: u32 = 1080;
const LONG: u32 = 1920;
const QUALITY: u8 = 88;
/// How long an album listing is reused before asking Immich again.
const FRESH: Duration = Duration::from_secs(600);
/// Immich's largest search page.
const PAGE: u32 = 1000;

pub struct Photos {
    base: String,
    key: String,
    album: String,
    agent: ureq::Agent,
    listing: Cached<Vec<String>>,
    store: Option<Store>,
}

#[derive(Deserialize)]
struct Album {
    id: String,
    #[serde(rename = "albumName")]
    name: String,
}

#[derive(Deserialize)]
struct Search {
    assets: Page,
}

#[derive(Deserialize)]
struct Page {
    items: Vec<Item>,
    #[serde(rename = "nextCursor")]
    next: Option<String>,
}

#[derive(Deserialize)]
struct Item {
    id: String,
}

impl Photos {
    /// Immich at `base` with `key`, showing the album named `album`; fitted
    /// photos kept in `store` when given.
    pub fn new(base: &str, key: String, album: String, store: Option<&str>) -> Self {
        Photos {
            base: base.trim_end_matches('/').to_owned(),
            key,
            album,
            agent: ureq::Agent::config_builder()
                .timeout_global(Some(Duration::from_secs(30)))
                .build()
                .into(),
            listing: Cached::new(FRESH),
            store: store.map(|d| Store { dir: d.into() }),
        }
    }

    /// The album's image ids, oldest page first as Immich returns them.
    pub fn ids(&self) -> Result<Vec<String>, Fault> {
        self.listing.get(|| {
            let ids = self.list()?;
            if let Some(store) = &self.store {
                store.keep(&ids);
            }
            Ok(ids)
        })
    }

    /// One photo as JPEG no larger than the screen.
    pub fn picture(&self, id: &str) -> Result<Vec<u8>, Fault> {
        if !is_id(id) {
            return Err(Fault::Unknown(format!("no photo {id:?}")));
        }
        if let Some(bytes) = self.store.as_ref().and_then(|s| s.get(id)) {
            return Ok(bytes);
        }
        let bytes = self
            .listing
            .tracker
            .record(self.preview(id))
            .map_err(Fault::Unavailable)?;
        let bytes = fit(&bytes).map_err(Fault::Unavailable)?;
        if let Some(store) = &self.store {
            store.put(id, &bytes);
        }
        Ok(bytes)
    }

    pub fn health(&self) -> Health {
        self.listing.tracker.health("photos")
    }

    fn list(&self) -> Result<Vec<String>, String> {
        let albums: Vec<Album> = self
            .agent
            .get(format!("{}/api/albums", self.base))
            .header("x-api-key", &self.key)
            .call()
            .and_then(|mut r| r.body_mut().read_json())
            .map_err(|e| format!("albums: {e}"))?;
        let album = albums
            .into_iter()
            .find(|a| a.name == self.album)
            .ok_or_else(|| format!("no album named {:?}", self.album))?;

        let mut ids = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut body = json!({
                "filter": {
                    "albumIds": { "any": [album.id] },
                    "type": { "eq": "IMAGE" },
                },
                "size": PAGE,
            });
            if let Some(c) = &cursor {
                body["cursor"] = json!(c);
            }
            let search: Search = self
                .agent
                .post(format!("{}/api/search/metadata", self.base))
                .header("x-api-key", &self.key)
                .send_json(&body)
                .and_then(|mut r| r.body_mut().read_json())
                .map_err(|e| format!("search: {e}"))?;
            ids.extend(search.assets.items.into_iter().map(|i| i.id));
            match search.assets.next {
                Some(c) => cursor = Some(c),
                None => return Ok(ids),
            }
        }
    }

    /// One photo at Immich's preview size.
    fn preview(&self, id: &str) -> Result<Vec<u8>, String> {
        self.agent
            .get(format!("{}/api/assets/{id}/thumbnail", self.base))
            .query("size", "preview")
            .header("x-api-key", &self.key)
            .call()
            .and_then(|mut r| r.body_mut().with_config().limit(32 << 20).read_to_vec())
            .map_err(|e| format!("preview: {e}"))
    }
}

/// Immich ids are UUIDs; anything else never reaches its URL.
pub fn is_id(s: &str) -> bool {
    s.len() == 36
        && s.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// The photo as JPEG no larger than the screen, its colour profile kept:
/// iPhone previews carry Display P3. Immich bakes orientation into
/// previews, so dropping the rest of the metadata turns nothing.
fn fit(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| format!("photo: {e}"))?;
    let jpeg = reader.format() == Some(ImageFormat::Jpeg);
    let mut decoder = reader.into_decoder().map_err(|e| format!("photo: {e}"))?;
    let (w, h) = decoder.dimensions();
    let (short, long) = (w.min(h), w.max(h));
    if jpeg && short <= SHORT && long <= LONG {
        return Ok(bytes.to_vec());
    }
    let icc = decoder.icc_profile().ok().flatten();
    let image = DynamicImage::from_decoder(decoder).map_err(|e| format!("photo: {e}"))?;
    let scale = (SHORT as f64 / short as f64)
        .min(LONG as f64 / long as f64)
        .min(1.0);
    let size = |n: u32| ((n as f64 * scale).round() as u32).max(1);
    let small = image
        .resize_exact(size(w), size(h), FilterType::Lanczos3)
        .into_rgb8();

    let mut out = Vec::new();
    let mut encoder = JpegEncoder::new_with_quality(&mut out, QUALITY);
    if let Some(icc) = icc {
        encoder
            .set_icc_profile(icc)
            .map_err(|e| format!("photo: {e}"))?;
    }
    small
        .write_with_encoder(encoder)
        .map_err(|e| format!("photo: {e}"))?;
    Ok(out)
}

/// Fitted photos on disk, one file per Immich id.
struct Store {
    dir: PathBuf,
}

impl Store {
    fn path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.jpg"))
    }

    fn get(&self, id: &str) -> Option<Vec<u8>> {
        std::fs::read(self.path(id)).ok()
    }

    /// Written aside and renamed, so a reader never sees half a photo.
    fn put(&self, id: &str, bytes: &[u8]) {
        let part = self.dir.join(format!("{id}.part"));
        if let Err(e) =
            std::fs::write(&part, bytes).and_then(|()| std::fs::rename(&part, self.path(id)))
        {
            eprintln!("photo store {id}: {e}");
        }
    }

    /// Drops photos no longer in the album.
    fn keep(&self, ids: &[String]) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let id = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            if !ids.iter().any(|k| k == id)
                && let Err(e) = std::fs::remove_file(&path)
            {
                eprintln!("photo store {}: {e}", path.display());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jpeg(w: u32, h: u32) -> Vec<u8> {
        let mut out = Vec::new();
        image::RgbImage::new(w, h)
            .write_with_encoder(JpegEncoder::new(&mut out))
            .unwrap();
        out
    }

    fn size(bytes: &[u8]) -> (u32, u32) {
        image::load_from_memory(bytes)
            .unwrap()
            .into_rgb8()
            .dimensions()
    }

    #[test]
    fn portrait_fits_the_short_side() {
        assert_eq!(size(&fit(&jpeg(1440, 1913)).unwrap()), (1080, 1435));
    }

    #[test]
    fn wide_panorama_fits_the_long_side() {
        assert_eq!(size(&fit(&jpeg(4000, 1000)).unwrap()), (1920, 480));
    }

    #[test]
    fn small_photo_passes_untouched() {
        let small = jpeg(800, 600);
        assert_eq!(fit(&small).unwrap(), small);
    }

    #[test]
    fn only_uuids_are_ids() {
        assert!(is_id("0b6e5f0e-3c1a-4d2b-9f8e-7a6b5c4d3e2f"));
        assert!(!is_id("../../api/users"));
        assert!(!is_id("0b6e5f0e-3c1a-4d2b-9f8e-7a6b5c4d3e2"));
        assert!(!is_id("0b6e5f0e/3c1a-4d2b-9f8e-7a6b5c4d3e2f"));
    }

    #[test]
    fn a_search_page_reads_ids_and_cursor() {
        let s: Search = serde_json::from_str(
            r#"{"albums":{},"assets":{"total":2,"count":2,"items":[{"id":"a","type":"IMAGE"},{"id":"b"}],"nextCursor":"c2"}}"#,
        )
        .unwrap();
        let ids: Vec<_> = s.assets.items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, ["a", "b"]);
        assert_eq!(s.assets.next.as_deref(), Some("c2"));
    }
}

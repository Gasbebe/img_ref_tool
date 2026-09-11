use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::HashSet, fs, io::Write, path::Path};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

const PREVIEW_MAGIC: &[u8; 8] = b"IRPV0001";
const MAX_PREVIEW_SIDE: u32 = 4096;

#[derive(Clone)]
pub struct Preview {
    pub width: u32,
    pub height: u32,
    pub rgba: std::sync::Arc<Vec<u8>>,
}

pub struct AssetData {
    pub id: String,
    pub bytes: std::sync::Arc<Vec<u8>>,
    pub preview: Preview,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Project {
    pub version: u32,
    pub camera_center: [f32; 2],
    pub zoom: f32,
    pub items: Vec<Item>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Item {
    pub id: String,
    pub center: [f32; 2],
    pub size: [f32; 2],
    pub rotation: f32,
    #[serde(flatten)]
    pub content: Content,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Content {
    Image {
        asset: String,
        original_name: String,
    },
    Note {
        text: String,
        font_size: f32,
        text_color: [u8; 4],
        background: [u8; 4],
    },
}

pub fn asset_id(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn validate(project: &Project) -> Result<()> {
    if project.version != 1 {
        return Err("Unsupported project version".into());
    }
    if !project.zoom.is_finite()
        || !(0.05..=16.0).contains(&project.zoom)
        || !project.camera_center.iter().all(|v| v.is_finite())
    {
        return Err("Invalid camera".into());
    }
    let mut ids = HashSet::new();
    for item in &project.items {
        if item.id.is_empty()
            || !ids.insert(&item.id)
            || !item.rotation.is_finite()
            || !item.center.iter().all(|v| v.is_finite())
            || !item
                .size
                .iter()
                .all(|v| v.is_finite() && *v > 0.0 && *v <= 1_000_000.0)
        {
            return Err("Invalid item geometry or duplicate ID".into());
        }
        match &item.content {
            Content::Image { asset, .. }
                if asset.len() != 64 || !asset.bytes().all(|c| c.is_ascii_hexdigit()) =>
            {
                return Err("Invalid asset ID".into());
            }
            Content::Note { font_size, .. }
                if !font_size.is_finite() || !(1.0..=10000.0).contains(font_size) =>
            {
                return Err("Invalid font size".into());
            }
            _ => {}
        }
    }
    Ok(())
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut temp = tempfile::NamedTempFile::new_in(path.parent().ok_or("Missing parent")?)?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    temp.persist(path)?;
    Ok(())
}

fn preview_path(root: &Path, id: &str) -> std::path::PathBuf {
    root.join("cache").join("previews").join(id)
}

pub fn save_preview(root: &Path, id: &str, preview: &Preview) -> Result<()> {
    let expected = preview.width as usize * preview.height as usize * 4;
    if preview.width == 0
        || preview.height == 0
        || preview.width > MAX_PREVIEW_SIDE
        || preview.height > MAX_PREVIEW_SIDE
        || preview.rgba.len() != expected
    {
        return Err("Invalid preview".into());
    }
    fs::create_dir_all(root.join("cache").join("previews"))?;
    let mut encoded = Vec::with_capacity(16 + expected);
    encoded.extend_from_slice(PREVIEW_MAGIC);
    encoded.extend_from_slice(&preview.width.to_le_bytes());
    encoded.extend_from_slice(&preview.height.to_le_bytes());
    encoded.extend_from_slice(&preview.rgba);
    atomic_write(&preview_path(root, id), &encoded)
}

pub fn load_preview(root: &Path, id: &str) -> Result<Preview> {
    let encoded = fs::read(preview_path(root, id))?;
    if encoded.len() < 16 || &encoded[..8] != PREVIEW_MAGIC {
        return Err("Invalid preview header".into());
    }
    let width = u32::from_le_bytes(encoded[8..12].try_into()?);
    let height = u32::from_le_bytes(encoded[12..16].try_into()?);
    let expected = width as usize * height as usize * 4;
    if width == 0
        || height == 0
        || width > MAX_PREVIEW_SIDE
        || height > MAX_PREVIEW_SIDE
        || encoded.len() != 16 + expected
    {
        return Err("Invalid preview dimensions".into());
    }
    Ok(Preview {
        width,
        height,
        rgba: std::sync::Arc::new(encoded[16..].to_vec()),
    })
}

pub fn save(root: &Path, project: &Project, assets: &[AssetData]) -> Result<()> {
    validate(project)?;
    fs::create_dir_all(root.join("assets"))?;
    fs::create_dir_all(root.join("cache"))?;
    for asset in assets {
        if asset_id(&asset.bytes) != asset.id {
            return Err("Asset hash mismatch".into());
        }
        let path = root.join("assets").join(&asset.id);
        if !path.exists() {
            atomic_write(&path, &asset.bytes)?;
        } else if asset_id(&fs::read(&path)?) != asset.id {
            return Err("Existing asset is corrupted".into());
        }
        let preview = preview_path(root, &asset.id);
        if !preview.is_file() || load_preview(root, &asset.id).is_err() {
            save_preview(root, &asset.id, &asset.preview)?;
        }
    }
    for item in &project.items {
        if let Content::Image { asset, .. } = &item.content
            && !root.join("assets").join(asset).is_file()
        {
            return Err("Missing asset".into());
        }
    }
    let path = root.join("project.json");
    if path.exists() {
        let previous = fs::read(&path)?;
        let parsed: Project = serde_json::from_slice(&previous)?;
        validate(&parsed)?;
        atomic_write(&root.join("project.backup.json"), &previous)?;
    }
    atomic_write(&path, &serde_json::to_vec_pretty(project)?)
}

pub fn load(path: &Path) -> Result<Project> {
    let project = serde_json::from_slice(&fs::read(path)?)?;
    validate(&project)?;
    Ok(project)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn project(asset: String) -> Project {
        Project {
            version: 1,
            camera_center: [12.0, -20.0],
            zoom: 2.0,
            items: vec![
                Item {
                    id: "image1".into(),
                    center: [100.0, 200.0],
                    size: [640.0, 320.0],
                    rotation: 0.4,
                    content: Content::Image {
                        asset,
                        original_name: "참고.png".into(),
                    },
                },
                Item {
                    id: "note1".into(),
                    center: [-10.0, 30.0],
                    size: [300.0, 160.0],
                    rotation: -0.2,
                    content: Content::Note {
                        text: "재질 참고\nQWE 메모".into(),
                        font_size: 18.0,
                        text_color: [238, 238, 238, 255],
                        background: [48, 48, 48, 255],
                    },
                },
            ],
        }
    }

    fn asset(id: String, bytes: Arc<Vec<u8>>) -> AssetData {
        AssetData {
            id,
            bytes,
            preview: Preview {
                width: 1,
                height: 1,
                rgba: Arc::new(vec![10, 20, 30, 255]),
            },
        }
    }

    #[test]
    fn saves_portable_assets_notes_order_and_previous_version() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = Arc::new(b"original file contents".to_vec());
        let id = asset_id(&bytes);
        let first = project(id.clone());
        let assets = vec![asset(id.clone(), bytes.clone()), asset(id.clone(), bytes)];
        save(dir.path(), &first, &assets).unwrap();
        assert_eq!(fs::read_dir(dir.path().join("assets")).unwrap().count(), 1);
        let reopened = load(&dir.path().join("project.json")).unwrap();
        assert_eq!(
            serde_json::to_value(&first).unwrap(),
            serde_json::to_value(&reopened).unwrap()
        );
        let mut second = first.clone();
        second.items[0].center = [500.0, 600.0];
        save(dir.path(), &second, &[]).unwrap();
        let backup = load(&dir.path().join("project.backup.json")).unwrap();
        assert_eq!(backup.items[0].center, first.items[0].center);
        assert_eq!(
            load(&dir.path().join("project.json")).unwrap().items[0].center,
            [500.0, 600.0]
        );
    }

    #[test]
    fn rejects_traversal_invalid_geometry_and_versions() {
        let mut data = project("../../outside".into());
        assert!(validate(&data).is_err());
        data = project("a".repeat(64));
        data.items[0].size[0] = -1.0;
        assert!(validate(&data).is_err());
        data = project("a".repeat(64));
        data.version = 99;
        assert!(validate(&data).is_err());
    }

    #[test]
    fn failed_save_keeps_previous_project() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = Arc::new(vec![1, 2, 3]);
        let id = asset_id(&bytes);
        let data = project(id.clone());
        save(dir.path(), &data, &[asset(id, bytes)]).unwrap();
        let previous = fs::read(dir.path().join("project.json")).unwrap();
        let missing = project("b".repeat(64));
        assert!(save(dir.path(), &missing, &[]).is_err());
        assert_eq!(fs::read(dir.path().join("project.json")).unwrap(), previous);
    }

    #[test]
    fn saves_image_added_after_reopening_note_only_project() {
        let dir = tempfile::tempdir().unwrap();
        let mut note_only = project("a".repeat(64));
        note_only.items.remove(0);

        save(dir.path(), &note_only, &[]).unwrap();
        let mut reopened = load(&dir.path().join("project.json")).unwrap();

        let bytes = Arc::new(b"new image contents".to_vec());
        let id = asset_id(&bytes);
        reopened.items.push(Item {
            id: "image-after-open".into(),
            center: [25.0, 50.0],
            size: [640.0, 480.0],
            rotation: 0.0,
            content: Content::Image {
                asset: id.clone(),
                original_name: "new-image.png".into(),
            },
        });

        save(dir.path(), &reopened, &[asset(id.clone(), bytes)]).unwrap();

        let saved = load(&dir.path().join("project.json")).unwrap();
        assert_eq!(saved.items.len(), 2);
        assert!(dir.path().join("assets").join(id).is_file());
        assert!(matches!(saved.items[0].content, Content::Note { .. }));
        assert!(matches!(saved.items[1].content, Content::Image { .. }));
    }

    #[test]
    fn preview_cache_round_trips_raw_pixels() {
        let dir = tempfile::tempdir().unwrap();
        let preview = Preview {
            width: 2,
            height: 1,
            rgba: Arc::new(vec![1, 2, 3, 255, 4, 5, 6, 128]),
        };

        save_preview(dir.path(), "asset", &preview).unwrap();
        let loaded = load_preview(dir.path(), "asset").unwrap();

        assert_eq!((loaded.width, loaded.height), (2, 1));
        assert_eq!(*loaded.rgba, *preview.rgba);
    }
}

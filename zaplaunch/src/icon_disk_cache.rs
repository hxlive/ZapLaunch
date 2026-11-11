use std::{collections::hash_map::DefaultHasher, fs, hash::{Hash, Hasher}, io::Read, path::Path};

const MAGIC: &[u8; 8] = b"ZLIC0001";
const MAX_BYTES: u64 = 512 * 512 * 4 + 32768 + 40;
static WRITES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn hash(value: impl Hash) -> u64 {
    let mut h = DefaultHasher::new();
    value.hash(&mut h);
    h.finish()
}

fn stamp(path: &str) -> Option<u64> {
    let meta = fs::metadata(path).ok()?;
    Some(hash((path, meta.len(), meta.modified().ok()?)))
}

pub fn cache_file_name(path: &str) -> String { format!("{:016x}.rgba",hash(path)) }

pub fn matches_pixels(path: &str, cache: &Path, pixels: &(u32,u32,Vec<u8>)) -> bool {
    stamp(path).and_then(|source| read(&cache.join(cache_file_name(path)),source)).as_ref() == Some(pixels)
}

fn read(file: &Path, source: u64) -> super::IconPixels {
    let file = fs::File::open(file).ok()?;
    let meta = file.metadata().ok()?;
    // shortcut: Shell 关联或快捷方式外部图标资源变化需手动刷新；自动跟随后再补资源指纹。
    if meta.len() > MAX_BYTES { return None; }
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() < 40 || bytes.len() as u64 > MAX_BYTES || &bytes[..8] != MAGIC { return None; }
    let number = |start| u64::from_le_bytes(bytes[start..start + 8].try_into().unwrap());
    if number(8) != source { return None; }
    let word = |start| u32::from_le_bytes(bytes[start..start + 4].try_into().unwrap());
    let (w,h,len) = (word(24),word(28),word(32) as usize);
    if w == 0 || h == 0 || w > 512 || h > 512 || len > 32768 || bytes.len() != 40 + len + w as usize * h as usize * 4 { return None; }
    let dependency = std::str::from_utf8(&bytes[40..40 + len]).ok()?;
    if !dependency.is_empty() && stamp(dependency)? != number(16) { return None; }
    Some((w,h,bytes[40 + len..].to_vec()))
}

pub fn load(path: &str, cache: &Path, force: bool, extract: impl FnOnce() -> super::IconPixels) -> super::IconPixels {
    let source = stamp(path);
    let file = cache.join(cache_file_name(path));
    if !force {
        if let Some(pixels) = source.and_then(|s| read(&file,s)) { return Some(pixels); }
    }
    let pixels = extract()?;
    if let Some(source) = source {
        let dependency = if path.to_ascii_lowercase().ends_with(".lnk") { zaplaunch_sys::resolve_shortcut_target(path).unwrap_or_default() } else { String::new() };
        let dep_stamp = if dependency.is_empty() { Some(0) } else { stamp(&dependency) };
        if let Some(dep_stamp) = dep_stamp {
            let (w,h,rgba) = &pixels;
            if *w > 0 && *h > 0 && *w <= 512 && *h <= 512 && rgba.len() == *w as usize * *h as usize * 4 && dependency.len() <= 32768 {
                let mut bytes = Vec::with_capacity(40 + dependency.len() + rgba.len());
                bytes.extend_from_slice(MAGIC);
                bytes.extend_from_slice(&source.to_le_bytes());
                bytes.extend_from_slice(&dep_stamp.to_le_bytes());
                bytes.extend_from_slice(&w.to_le_bytes()); bytes.extend_from_slice(&h.to_le_bytes());
                bytes.extend_from_slice(&(dependency.len() as u32).to_le_bytes());
                bytes.extend_from_slice(&[0;4]);
                bytes.extend_from_slice(dependency.as_bytes()); bytes.extend_from_slice(rgba);
                if fs::create_dir_all(cache).is_ok() {
                    let temp = file.with_extension("tmp");
                    if fs::write(&temp,bytes).is_ok() { let _ = fs::rename(&temp,&file); }
                    if temp.exists() { let _ = fs::remove_file(temp); }
                    if WRITES.fetch_add(1,std::sync::atomic::Ordering::Relaxed) % 64 == 0 { prune(cache); }
                }
            }
        }
    }
    Some(pixels)
}

fn prune(cache: &Path) {
    let Ok(files) = fs::read_dir(cache) else { return; };
    let mut files: Vec<_> = files.flatten().filter_map(|entry| {
        let path = entry.path();
        if path.extension()?.to_str()? != "rgba" { return None; }
        Some((entry.metadata().ok()?.modified().ok()?,path))
    }).collect();
    files.sort_unstable_by_key(|(modified,_)| *modified);
    for (_,path) in files.iter().take(files.len().saturating_sub(super::PATH_CACHE_LIMIT)) { let _ = fs::remove_file(path); }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_changed_dependencies_and_invalid_buffers() {
        let root = std::env::temp_dir().join(format!("native-icon-validation-{}",std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let source = root.join("source.exe");
        let dependency = root.join("target.exe");
        fs::write(&source,b"source").unwrap(); fs::write(&dependency,b"target").unwrap();
        let source = source.to_str().unwrap();
        let pixels = || Some((1,1,vec![1,2,3,255]));
        load(source,&root,false,pixels).unwrap();
        let file = root.join(format!("{:016x}.rgba",hash(source)));
        let original = fs::read(&file).unwrap();
        let name = dependency.to_str().unwrap().as_bytes();
        let mut bytes = original[..40].to_vec();
        bytes[16..24].copy_from_slice(&stamp(dependency.to_str().unwrap()).unwrap().to_le_bytes());
        bytes[32..36].copy_from_slice(&(name.len() as u32).to_le_bytes());
        bytes.extend_from_slice(name); bytes.extend_from_slice(&original[40..]);
        fs::write(&file,&bytes).unwrap();
        assert_eq!(read(&file,stamp(source).unwrap()),pixels());
        fs::write(&dependency,b"modified target length").unwrap();
        assert!(read(&file,stamp(source).unwrap()).is_none());
        for offset in [24,28,32] {
            let mut invalid = original.clone();
            invalid[offset..offset+4].copy_from_slice(&u32::MAX.to_le_bytes());
            fs::write(&file,invalid).unwrap();
            assert!(read(&file,stamp(source).unwrap()).is_none());
        }
        fs::write(&file,&original).unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(30 * 86400);
        fs::File::options().write(true).open(&file).unwrap().set_times(fs::FileTimes::new().set_modified(old)).unwrap();
        assert_eq!(read(&file,stamp(source).unwrap()),pixels(),"unchanged icons must remain cached across days");
        for index in 0..600 { fs::write(root.join(format!("unused-{index}.rgba")),b"x").unwrap(); }
        prune(&root);
        assert_eq!(fs::read_dir(&root).unwrap().flatten().filter(|entry| entry.path().extension().and_then(|s|s.to_str()) == Some("rgba")).count(),super::super::PATH_CACHE_LIMIT);
        assert!(Path::new(source).exists() && dependency.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn five_hundred_cached_icons_need_no_extraction_after_restart() {
        let root = std::env::temp_dir().join(format!("native-icon-500-{}",std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let cache = root.join("cache");
        let started = std::time::Instant::now();
        for index in 0..500 {
            let source = root.join(format!("source-{index}.exe"));
            fs::write(&source,index.to_string()).unwrap();
            load(source.to_str().unwrap(),&cache,false,|| Some((48,48,[index as u8,20,30,255].repeat(48*48)))).unwrap();
        }
        let cold_ms = started.elapsed().as_secs_f64() * 1000.0;
        let started = std::time::Instant::now();
        for index in 0..500 {
            let source = root.join(format!("source-{index}.exe"));
            let (w,h,rgba) = load(source.to_str().unwrap(),&cache,false,||panic!("cached source {index} must not be extracted again")).unwrap();
            assert_eq!((w,h),(48,48));
            assert_eq!(rgba,[index as u8,20,30,255].repeat(48*48));
        }
        println!("500 disk icons: fixture/cache creation {cold_ms:.1}ms; warm reads {:.1}ms; zero extractions",started.elapsed().as_secs_f64()*1000.0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn native_shell_pixels_reuse_disk_cache_without_extraction() {
        let root = std::env::temp_dir().join(format!("native-shell-cache-{}",std::process::id()));
        let source = Path::new(concat!(env!("CARGO_MANIFEST_DIR"),"/../../ZapLaunch.ico")).canonicalize().unwrap();
        let source = source.to_string_lossy();
        let source = source.trim_start_matches(r"\\?\");
        let started = std::time::Instant::now();
        let original = load(source,&root,true,||zaplaunch_sys::extract_file_icon_rgba(source)).expect("project ICO must have a Shell icon");
        let extracted_ms = started.elapsed().as_secs_f64()*1000.0;
        let started = std::time::Instant::now();
        for _ in 0..30 {
            assert_eq!(load(source,&root,false,||panic!("warm native icon must bypass Shell")),Some(original.clone()));
        }
        println!("native ICO: Shell extraction/write {extracted_ms:.3}ms; cached read average {:.3}ms; RGBA identical",started.elapsed().as_secs_f64()*1000.0/30.0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cached_pixels_survive_restart_and_invalidate_safely() {
        let root = std::env::temp_dir().join(format!("native-icon-cache-{}",std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let source = root.join("source.exe");
        fs::write(&source,b"original").unwrap();
        let source = source.to_str().unwrap();
        let cache = root.join("cache");
        let pixels = || Some((2,1,vec![10,20,30,255,40,50,60,128]));
        assert_eq!(load(source,&cache,false,pixels),pixels());
        assert!(matches_pixels(source,&cache,&pixels().unwrap()));
        assert_eq!(load(source,&cache,false,||panic!("warm cache must avoid extraction")),pixels());
        let replacement = || Some((1,1,vec![90,80,70,255]));
        assert_eq!(load(source,&cache,true,replacement),replacement());
        assert_eq!(load(source,&cache,false,||panic!("forced refresh must update disk cache")),replacement());
        fs::write(source,b"changed source length").unwrap();
        assert_eq!(load(source,&cache,false,pixels),pixels());
        let file = cache.join(format!("{:016x}.rgba",hash(source)));
        fs::write(&file,b"truncated").unwrap();
        assert_eq!(load(source,&cache,false,pixels),pixels());
        assert!(load(source,&cache,true,||None).is_none());
        assert_eq!(load(source,&cache,false,||panic!("failed refresh must retain good disk cache")),pixels());
        fs::remove_file(source).unwrap();
        assert!(load(source,&cache,false,||None).is_none());
        fs::write(&source,b"restored").unwrap();
        let blocked = root.join("not-a-directory");
        fs::write(&blocked,b"x").unwrap();
        assert_eq!(load(source,&blocked,false,pixels),pixels());
        assert!(!matches_pixels(source,&blocked,&pixels().unwrap()));
        fs::remove_dir_all(root).unwrap();
    }
}

use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use daw_core::SampleData;
use daw_dsp::WaveformPyramid;

/// `audio`'s waveform pyramid, read from a `.pek` file in `cache_dir` when one
/// matches `source` (same path, size, modification time and sample rate), else
/// built and saved there. Blocking: call it off the audio thread. A missing,
/// stale or unwritable cache only costs a rebuild.
pub fn cached_waveform(source: &Path, audio: Arc<SampleData>, cache_dir: &Path) -> WaveformPyramid {
    let Some(file) = cache_file(source, &audio, cache_dir) else {
        return WaveformPyramid::new(audio);
    };
    if let Some(p) = fs::read(&file)
        .ok()
        .and_then(|bytes| WaveformPyramid::from_bytes(audio.clone(), &bytes))
    {
        return p;
    }
    let pyramid = WaveformPyramid::new(audio);
    // Write then rename, so a crash never leaves a half-written cache behind.
    let tmp = file.with_extension("pek.tmp");
    let _ = fs::create_dir_all(cache_dir)
        .and_then(|()| fs::write(&tmp, pyramid.to_bytes()))
        .and_then(|()| fs::rename(&tmp, &file));
    pyramid
}

fn cache_file(source: &Path, audio: &SampleData, cache_dir: &Path) -> Option<PathBuf> {
    let meta = fs::metadata(source).ok()?;
    let mut h = DefaultHasher::new();
    fs::canonicalize(source).ok()?.hash(&mut h);
    meta.len().hash(&mut h);
    meta.modified().ok()?.hash(&mut h);
    audio.sample_rate().hash(&mut h);
    Some(cache_dir.join(format!("{:016x}.pek", h.finish())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_call_reads_the_cache_and_a_changed_source_misses() {
        let dir = std::env::temp_dir().join(format!("canary-peaks-{}", std::process::id()));
        let source = dir.join("a.raw");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&source, [1, 2, 3]).unwrap();
        let audio = Arc::new(SampleData::new(48_000, vec![vec![0.5; 5_000]]).unwrap());
        let cache = dir.join("cache");

        let built = cached_waveform(&source, audio.clone(), &cache);
        let file = cache_file(&source, &audio, &cache).unwrap();
        assert!(file.exists(), "cache written");
        // Poison the cache with another pyramid's bytes of the same shape: a hit
        // must come back from disk, not be rebuilt.
        let other = Arc::new(SampleData::new(48_000, vec![vec![-1.0; 5_000]]).unwrap());
        fs::write(&file, WaveformPyramid::new(other).to_bytes()).unwrap();
        let hit = cached_waveform(&source, audio.clone(), &cache);
        assert_ne!(hit.to_bytes(), built.to_bytes());

        fs::write(&source, [1, 2, 3, 4]).unwrap(); // new size: new key
        let miss = cached_waveform(&source, audio, &cache);
        assert_eq!(miss.to_bytes(), built.to_bytes());
        fs::remove_dir_all(&dir).unwrap();
    }
}

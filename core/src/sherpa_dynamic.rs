//! Runtime loading of the sherpa-onnx C API on Windows.
//!
//! Linking sherpa-onnx and ONNX Runtime statically is fragile on Windows:
//! their static libraries pull in CRT and system-library requirements that the
//! Rust linker does not inherit. Instead, the app ships
//! `sherpa-onnx-c-api.dll` and `onnxruntime.dll` beside the executable and
//! resolves the handful of C functions it uses at runtime.

#![allow(non_snake_case)]
#![allow(clippy::missing_safety_doc)]

use std::ffi::{c_char, c_void};
use std::path::PathBuf;
use std::sync::OnceLock;

use libloading::os::windows::{
    Library, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR,
};

pub type ProgressCallback = Option<unsafe extern "C" fn(i32, i32, *mut c_void) -> i32>;

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct SherpaOnnxSpeakerEmbeddingExtractorConfig {
    pub model: *const c_char,
    pub num_threads: i32,
    pub debug: i32,
    pub provider: *const c_char,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct SherpaOnnxOfflineSpeakerSegmentationPyannoteModelConfig {
    pub model: *const c_char,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct SherpaOnnxOfflineSpeakerSegmentationModelConfig {
    pub pyannote: SherpaOnnxOfflineSpeakerSegmentationPyannoteModelConfig,
    pub num_threads: i32,
    pub debug: i32,
    pub provider: *const c_char,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct SherpaOnnxFastClusteringConfig {
    pub num_clusters: i32,
    pub threshold: f32,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct SherpaOnnxOfflineSpeakerDiarizationConfig {
    pub segmentation: SherpaOnnxOfflineSpeakerSegmentationModelConfig,
    pub embedding: SherpaOnnxSpeakerEmbeddingExtractorConfig,
    pub clustering: SherpaOnnxFastClusteringConfig,
    pub min_duration_on: f32,
    pub min_duration_off: f32,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct SherpaOnnxOfflineSpeakerDiarizationSegment {
    pub start: f32,
    pub end: f32,
    pub speaker: i32,
}

type CreateFn =
    unsafe extern "C" fn(*const SherpaOnnxOfflineSpeakerDiarizationConfig) -> *const c_void;
type DestroyFn = unsafe extern "C" fn(*const c_void);
type ProcessFn = unsafe extern "C" fn(
    *const c_void,
    *const f32,
    i32,
    ProgressCallback,
    *mut c_void,
) -> *const c_void;
type NumSegmentsFn = unsafe extern "C" fn(*const c_void) -> i32;
type SortFn =
    unsafe extern "C" fn(*const c_void) -> *const SherpaOnnxOfflineSpeakerDiarizationSegment;
type DestroySegmentFn = unsafe extern "C" fn(*const SherpaOnnxOfflineSpeakerDiarizationSegment);
type DestroyResultFn = unsafe extern "C" fn(*const c_void);

struct Api {
    /// Keeps the DLL loaded for the lifetime of the process.
    _library: Library,
    create: CreateFn,
    destroy: DestroyFn,
    process: ProcessFn,
    num_segments: NumSegmentsFn,
    sort: SortFn,
    destroy_segment: DestroySegmentFn,
    destroy_result: DestroyResultFn,
}

static API: OnceLock<Result<Api, String>> = OnceLock::new();

fn candidates() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(path) = std::env::var_os("TRANSIEL_SHERPA_DLL") {
        paths.push(PathBuf::from(path));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            paths.push(dir.join("sherpa-onnx-c-api.dll"));
            paths.push(dir.join("lib").join("sherpa-onnx-c-api.dll"));
        }
    }
    paths.push(PathBuf::from("sherpa-onnx-c-api.dll"));
    paths
}

unsafe fn resolve<T: Copy>(library: &Library, name: &[u8]) -> Result<T, String> {
    let symbol = library
        .get::<T>(name)
        .map_err(|error| format!("{}: {error}", String::from_utf8_lossy(name)))?;
    Ok(*symbol)
}

fn load() -> Result<Api, String> {
    let path = candidates()
        .into_iter()
        .find(|path| path.is_file())
        .ok_or_else(|| {
            "sherpa-onnx-c-api.dll was not found next to the executable; reinstall transiel"
                .to_string()
        })?;
    let path = path.canonicalize().unwrap_or(path);

    unsafe {
        let library = Library::load_with_flags(
            &path,
            LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
        )
        .map_err(|error| format!("could not load {}: {error}", path.display()))?;
        Ok(Api {
            create: resolve(&library, b"SherpaOnnxCreateOfflineSpeakerDiarization\0")?,
            destroy: resolve(&library, b"SherpaOnnxDestroyOfflineSpeakerDiarization\0")?,
            process: resolve(
                &library,
                b"SherpaOnnxOfflineSpeakerDiarizationProcessWithCallback\0",
            )?,
            num_segments: resolve(
                &library,
                b"SherpaOnnxOfflineSpeakerDiarizationResultGetNumSegments\0",
            )?,
            sort: resolve(
                &library,
                b"SherpaOnnxOfflineSpeakerDiarizationResultSortByStartTime\0",
            )?,
            destroy_segment: resolve(
                &library,
                b"SherpaOnnxOfflineSpeakerDiarizationDestroySegment\0",
            )?,
            destroy_result: resolve(
                &library,
                b"SherpaOnnxOfflineSpeakerDiarizationDestroyResult\0",
            )?,
            _library: library,
        })
    }
}

/// Resolve the sherpa-onnx DLL and all required symbols.
pub fn ensure_loaded() -> Result<(), String> {
    API.get_or_init(load)
        .as_ref()
        .map(|_| ())
        .map_err(|error| error.clone())
}

fn api() -> &'static Api {
    API.get_or_init(load)
        .as_ref()
        .unwrap_or_else(|error| panic!("sherpa-onnx is unavailable: {error}"))
}

pub unsafe fn SherpaOnnxCreateOfflineSpeakerDiarization(
    config: *const SherpaOnnxOfflineSpeakerDiarizationConfig,
) -> *const c_void {
    (api().create)(config)
}

pub unsafe fn SherpaOnnxDestroyOfflineSpeakerDiarization(sd: *const c_void) {
    (api().destroy)(sd)
}

pub unsafe fn SherpaOnnxOfflineSpeakerDiarizationProcessWithCallback(
    sd: *const c_void,
    samples: *const f32,
    n: i32,
    callback: ProgressCallback,
    arg: *mut c_void,
) -> *const c_void {
    (api().process)(sd, samples, n, callback, arg)
}

pub unsafe fn SherpaOnnxOfflineSpeakerDiarizationResultGetNumSegments(
    result: *const c_void,
) -> i32 {
    (api().num_segments)(result)
}

pub unsafe fn SherpaOnnxOfflineSpeakerDiarizationResultSortByStartTime(
    result: *const c_void,
) -> *const SherpaOnnxOfflineSpeakerDiarizationSegment {
    (api().sort)(result)
}

pub unsafe fn SherpaOnnxOfflineSpeakerDiarizationDestroySegment(
    segment: *const SherpaOnnxOfflineSpeakerDiarizationSegment,
) {
    (api().destroy_segment)(segment)
}

pub unsafe fn SherpaOnnxOfflineSpeakerDiarizationDestroyResult(result: *const c_void) {
    (api().destroy_result)(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_matches_the_c_header() {
        assert_eq!(
            std::mem::size_of::<SherpaOnnxSpeakerEmbeddingExtractorConfig>(),
            24
        );
        assert_eq!(
            std::mem::size_of::<SherpaOnnxOfflineSpeakerSegmentationPyannoteModelConfig>(),
            8
        );
        assert_eq!(
            std::mem::size_of::<SherpaOnnxOfflineSpeakerSegmentationModelConfig>(),
            24
        );
        assert_eq!(std::mem::size_of::<SherpaOnnxFastClusteringConfig>(), 8);
        assert_eq!(
            std::mem::size_of::<SherpaOnnxOfflineSpeakerDiarizationConfig>(),
            64
        );
        assert_eq!(
            std::mem::size_of::<SherpaOnnxOfflineSpeakerDiarizationSegment>(),
            12
        );
    }
}
